use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::consts::TEMP_KEY_PREFIX;
use crate::db;
use crate::jobs::video_id_from_key;
use crate::s3_client::S3Client;

pub async fn run(
    s3: S3Client,
    db_url: String,
    cancel: CancellationToken,
    limit: Option<usize>,
    prefix: Option<String>,
    full_scan: bool,
) -> Result<()> {
    tracing::info!("Job 1 (scan-hetzner): starting");
    let client = db::connect(&db_url).await?;
    let mut total = 0usize;

    if cancel.is_cancelled() {
        return Ok(());
    }

    let start_after = if !full_scan {
        db::get_max_hetzner_key(&client, prefix.as_deref()).await?
    } else {
        None
    };

    if let Some(after) = &start_after {
        tracing::info!("Job 1 (scan-hetzner): resuming from after key: {}", after);
    } else {
        tracing::info!("Job 1 (scan-hetzner): starting full scan from the beginning");
    }

    let objects = s3
        .list_objects(prefix.as_deref(), start_after.as_deref())
        .await
        .map_err(|e| anyhow::anyhow!("Hetzner list failed: {e}"))?;

    for obj in &objects {
        // Skip thumbnails (both variants) — only index .mp4 files
        if obj.key.ends_with("_thumbnail.png") || obj.key.ends_with("-thumbnail.png") {
            continue;
        }
        let Some(video_id) = video_id_from_key(&obj.key) else {
            continue;
        };

        let is_temp = obj.key.contains(TEMP_KEY_PREFIX.as_str());

        if full_scan {
            db::upsert_hetzner_key_with_reset(&client, &video_id, &obj.key, is_temp)
                .await
                .map_err(|e| anyhow::anyhow!("DB upsert with reset failed for {}: {e}", obj.key))?;
        } else {
            db::upsert_hetzner_key(&client, &video_id, &obj.key, is_temp)
                .await
                .map_err(|e| anyhow::anyhow!("DB upsert failed for {}: {e}", obj.key))?;
        }

        total += 1;
        crate::jobs::log_progress(total, "scan-hetzner");
        if limit.is_some_and(|n| total >= n) {
            tracing::info!(total, "Job 1 (scan-hetzner): limit reached");
            break;
        }
    }

    tracing::info!(total, "Job 1 (scan-hetzner): complete");
    Ok(())
}
