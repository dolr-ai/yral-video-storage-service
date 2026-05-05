// NOTE: The spec pseudocode uses join_all — that is an error. buffer_unordered is used here
// to bound the number of concurrent tempfiles (one per in-flight future). join_all would open
// SCAN_PAGE_SIZE tempfiles simultaneously, exhausting disk space at 600K scale.
use anyhow::Result;
use futures::StreamExt;
use tempfile::NamedTempFile;
use tokio_util::sync::CancellationToken;

use crate::consts::{MIRROR_ACCESS_GRANT, MIRROR_CONCURRENCY, SCAN_PAGE_SIZE, STORJ_SFW_BUCKET};
use crate::db;
use crate::jobs::{thumb_key_from_mp4_key, uplink_cp};
use crate::s3_client::S3Client;

pub async fn run(s3: S3Client, db_url: String, cancel: CancellationToken, limit: Option<usize>) -> Result<()> {
    tracing::info!("Job 3 (mirror): starting");
    let client = db::connect(&db_url).await?;
    let mut grand_total = 0usize;

    loop {
        if cancel.is_cancelled() {
            tracing::info!("Job 3 (mirror): cancelled at {grand_total} videos");
            return Ok(());
        }

        let rows = db::fetch_pending_mirror_batch(&client, *SCAN_PAGE_SIZE).await?;
        if rows.is_empty() {
            break;
        }

        // buffer_unordered bounds concurrent in-flight futures (and their tempfiles).
        // Do NOT replace with join_all.
        let results: Vec<(String, String, Result<()>)> = futures::stream::iter(rows)
            .map(|row| {
                let s3 = s3.clone();
                let bucket = STORJ_SFW_BUCKET.clone();
                let grant = MIRROR_ACCESS_GRANT.clone();
                async move {
                    let result = mirror_one(&s3, &row.hetzner_key, &bucket, &grant).await;
                    (row.video_id, row.hetzner_key, result)
                }
            })
            .buffer_unordered(*MIRROR_CONCURRENCY)
            .collect()
            .await;

        // Sequential DB writes
        for (video_id, hetzner_key, result) in results {
            match result {
                Ok(()) => {
                    // storj_key mirrors hetzner_key verbatim (folder structure preserved)
                    db::update_mirror_success(&client, &video_id, &hetzner_key).await?;
                }
                Err(e) => {
                    tracing::error!(video_id = %video_id, error = %e, "mirror failed");
                    sentry::capture_message(
                        &format!("mirror failed for {video_id}: {e}"),
                        sentry::Level::Error,
                    );
                    db::update_error(&client, &video_id, &e.to_string()).await?;
                }
            }
            grand_total += 1;
            crate::jobs::log_progress(grand_total, "mirror");
            if limit.map_or(false, |n| grand_total >= n) {
                tracing::info!(grand_total, "Job 3 (mirror): limit reached");
                return Ok(());
            }
        }
    }

    tracing::info!(grand_total, "Job 3 (mirror): complete");
    Ok(())
}

async fn mirror_one(s3: &S3Client, hetzner_key: &str, bucket: &str, grant: &str) -> Result<()> {
    // 1. Copy MP4
    let mut tmp_mp4 = NamedTempFile::new()?;
    {
        let mut f = tokio::fs::File::from_std(tmp_mp4.as_file().try_clone()?);
        s3.download_to_file(hetzner_key, &mut f)
            .await
            .map_err(|e| anyhow::anyhow!("download mp4 {hetzner_key}: {e}"))?;
    }
    uplink_cp(
        tmp_mp4.path(),
        &format!("sj://{bucket}/{hetzner_key}"),
        grant,
    )
    .await?;
    drop(tmp_mp4);

    // 2. Copy thumbnail (best-effort: warn if absent, hard fail if S3 check errors)
    if let Some(thumb_key) = thumb_key_from_mp4_key(hetzner_key) {
        match s3.object_exists(&thumb_key).await {
            Ok(true) => {
                let mut tmp_thumb = NamedTempFile::new()?;
                {
                    let mut f = tokio::fs::File::from_std(tmp_thumb.as_file().try_clone()?);
                    s3.download_to_file(&thumb_key, &mut f)
                        .await
                        .map_err(|e| anyhow::anyhow!("download thumb {thumb_key}: {e}"))?;
                }
                uplink_cp(
                    tmp_thumb.path(),
                    &format!("sj://{bucket}/{thumb_key}"),
                    grant,
                )
                .await?;
                drop(tmp_thumb);
            }
            Ok(false) => {
                tracing::warn!(
                    hetzner_key,
                    "thumbnail missing on Hetzner — mirroring MP4 only"
                );
            }
            Err(e) => {
                // Transient S3 error checking thumbnail — abort this video, retry next run
                anyhow::bail!("S3 check for thumbnail {thumb_key}: {e}");
            }
        }
    }

    Ok(())
}
