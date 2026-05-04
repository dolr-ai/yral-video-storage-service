use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::db;
use crate::jobs::video_id_from_key;
use crate::storj_s3_client::StorjS3Client;

pub async fn run(storj: StorjS3Client, db_url: String, cancel: CancellationToken) -> Result<()> {
    tracing::info!("Job 0 (scan-storj): starting");
    let client = db::connect(&db_url).await?;
    let mut total = 0usize;

    if cancel.is_cancelled() {
        tracing::info!("Job 0 (scan-storj): cancelled before start");
        return Ok(());
    }

    // S3Client::list_objects paginates internally — returns all keys in one Vec
    let objects = storj
        .list_objects(None)
        .await
        .map_err(|e| anyhow::anyhow!("Storj list failed: {e}"))?;

    for obj in &objects {
        let Some(video_id) = video_id_from_key(&obj.key) else {
            continue;
        };

        db::upsert_storj_key(&client, &video_id, &obj.key)
            .await
            .map_err(|e| anyhow::anyhow!("DB upsert failed for {}: {e}", obj.key))?;

        total += 1;
        crate::jobs::log_progress(total, "scan-storj");
    }

    tracing::info!(total, "Job 0 (scan-storj): complete");
    Ok(())
}
