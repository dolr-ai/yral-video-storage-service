use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::consts::SCAN_PAGE_SIZE;
use crate::db;
use crate::s3_client::S3Client;
use crate::storj_s3_client::StorjS3Client;

pub async fn run(
    s3: S3Client,
    storj: StorjS3Client,
    db_url: String,
    cancel: CancellationToken,
) -> Result<()> {
    tracing::info!("Job 4 (cleanup): starting");
    let client = db::connect(&db_url).await?;
    let mut total = 0usize;

    loop {
        if cancel.is_cancelled() {
            tracing::info!("Job 4 (cleanup): cancelled at {total} rows");
            return Ok(());
        }

        let rows = db::fetch_pending_cleanup_batch(&client, *SCAN_PAGE_SIZE).await?;
        if rows.is_empty() {
            break;
        }

        // Intentionally serial — deletion is irreversible
        for row in rows {
            match storj.object_exists(&row.storj_key).await {
                Err(e) => {
                    tracing::error!(
                        video_id = %row.video_id,
                        storj_key = %row.storj_key,
                        error = %e,
                        "transient error checking Storj — skipping delete"
                    );
                    sentry::capture_message(
                        &format!("cleanup storj check failed for {}: {e}", row.video_id),
                        sentry::Level::Error,
                    );
                    continue;
                }
                Ok(false) => {
                    tracing::error!(
                        video_id = %row.video_id,
                        storj_key = %row.storj_key,
                        "CRITICAL: Storj copy missing before cleanup — data loss risk, skipping"
                    );
                    sentry::capture_message(
                        &format!(
                            "CRITICAL: storj copy missing for {} before delete",
                            row.video_id
                        ),
                        sentry::Level::Fatal,
                    );
                    continue;
                }
                Ok(true) => {}
            }

            if let Err(e) = s3.delete_video(&row.hetzner_key).await {
                tracing::error!(
                    video_id = %row.video_id,
                    hetzner_key = %row.hetzner_key,
                    error = %e,
                    "failed to delete from Hetzner"
                );
                sentry::capture_message(
                    &format!("cleanup delete failed for {}: {e:?}", row.video_id),
                    sentry::Level::Error,
                );
                continue;
            }

            db::update_cleanup_done(&client, &row.video_id).await?;
            total += 1;
            crate::jobs::log_progress(total, "cleanup");
        }
    }

    tracing::info!(total, "Job 4 (cleanup): complete");
    Ok(())
}
