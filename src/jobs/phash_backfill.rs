use anyhow::Result;
use futures::StreamExt;
use phash::PHasher;
use tempfile::NamedTempFile;
use tokio_util::sync::CancellationToken;

use crate::consts::{MAX_PHASH_RETRIES, PHASH_CONCURRENCY, SCAN_PAGE_SIZE};
use crate::db;
use crate::s3_client::S3Client;

pub async fn run(s3: S3Client, db_url: String, cancel: CancellationToken, limit: Option<usize>) -> Result<()> {
    tracing::info!("Job 2 (phash-backfill): starting");
    let client = db::connect(&db_url).await?;
    let mut grand_total = 0usize;

    loop {
        if cancel.is_cancelled() {
            tracing::info!("Job 2 (phash-backfill): cancelled at {grand_total} videos");
            return Ok(());
        }

        let rows = db::fetch_pending_phash_batch(&client, *SCAN_PAGE_SIZE).await?;
        if rows.is_empty() {
            break;
        }

        // buffer_unordered bounds concurrent in-flight futures (and their tempfiles).
        // Do NOT use join_all — that would open SCAN_PAGE_SIZE tempfiles simultaneously.
        let results: Vec<(String, Result<String>)> = futures::stream::iter(rows)
            .map(|row| {
                let s3 = s3.clone();
                async move {
                    let mut tmp =
                        NamedTempFile::new().map_err(|e| anyhow::anyhow!("tempfile: {e}"))?;
                    {
                        let mut f = tokio::fs::File::from_std(
                            tmp.as_file()
                                .try_clone()
                                .map_err(|e| anyhow::anyhow!("file clone: {e}"))?,
                        );
                        s3.download_to_file(&row.hetzner_key, &mut f)
                            .await
                            .map_err(|e| anyhow::anyhow!("download {}: {e}", row.hetzner_key))?;
                    }

                    // Pass path only (not file handle) into blocking thread
                    let path = tmp.path().to_owned();
                    let phash_result =
                        tokio::task::spawn_blocking(move || PHasher::new().compute_hash(&path))
                            .await
                            .map_err(|e| anyhow::anyhow!("spawn_blocking panic: {e}"))
                            .and_then(|r| r.map_err(|e| anyhow::anyhow!("phash: {e}")));

                    // Always delete tempfile
                    drop(tmp);

                    (row.video_id, phash_result)
                }
            })
            .buffer_unordered(*PHASH_CONCURRENCY)
            .collect()
            .await;

        // Sequential DB writes after all parallel work
        for (video_id, result) in results {
            match result {
                Ok(phash) => {
                    db::update_phash_success(&client, &video_id, &phash).await?;
                }
                Err(e) => {
                    tracing::error!(video_id = %video_id, error = %e, "phash failed");
                    sentry::capture_message(
                        &format!("phash failed for {video_id}: {e}"),
                        sentry::Level::Error,
                    );
                    let retries = db::update_phash_failure(
                        &client,
                        &video_id,
                        &e.to_string(),
                        *MAX_PHASH_RETRIES,
                    )
                    .await?;
                    tracing::warn!(video_id = %video_id, retries, "phash retry scheduled");
                }
            }
            grand_total += 1;
            crate::jobs::log_progress(grand_total, "phash-backfill");
            if limit.map_or(false, |n| grand_total >= n) {
                tracing::info!(grand_total, "Job 2 (phash-backfill): limit reached");
                return Ok(());
            }
        }
    }

    tracing::info!(grand_total, "Job 2 (phash-backfill): complete");
    Ok(())
}
