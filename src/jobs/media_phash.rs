use anyhow::Result;
use futures::StreamExt;
use phash::{VideoHashResult, VideoMetadata, HASH_KIND, HASH_VERSION};
use serde_json::{json, Value};
use tempfile::NamedTempFile;
use tokio_postgres::{Client, Transaction};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::media_index::{
    videos_missing_canonical_phash, FeedEventInput, FeedEventKind, HashRecordInput, MissingHashRow,
    UpsertOutcome,
};
use crate::s3_client::S3Client;
use crate::storj_s3_client::StorjS3Client;

const JOB_KIND: &str = "media_phash";
pub const INPUT_MEDIA_VERSION: &str = "current_stored_object_v1";

// Off-chain contract constants: 10 frames at 8x8 pixels each.
// These values are baked into HASH_VERSION ("offchain_binary_10x8_v1") and
// match PHasher::new() defaults.
const OFFCHAIN_NUM_FRAMES: i32 = 10;
const OFFCHAIN_HASH_SIZE: i32 = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "inspect to verify the backfill completed as expected"]
pub struct PHashSummary {
    pub job_run_id: Uuid,
    pub scanned_rows: i64,
    pub hash_rows_upserted: i64,
    pub hash_feed_events_appended: i64,
    pub row_failures: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum PHashJobError {
    #[error("postgres media_phash job failed: {0}")]
    Postgres(#[from] tokio_postgres::Error),
}

/// Orchestration shell: scan -> download -> spawn_blocking -> persist.
/// Not unit-tested (same as phash_backfill); testability lives in
/// `persist_one` which is called from tests with synthetic `VideoHashResult`.
#[allow(dead_code)]
pub async fn run(
    s3: S3Client,
    storj: StorjS3Client,
    db_url: String,
    cancel: CancellationToken,
    limit: Option<i64>,
    requested_by: &str,
    shard: Option<(i64, i64)>,
) -> Result<()> {
    tracing::info!("media_phash: starting");
    let mut client = crate::db::connect(&db_url).await?;

    let job_run_id = Uuid::new_v4();
    insert_job_run(&client, job_run_id, requested_by).await?;

    let result = run_inner(&mut client, job_run_id, &s3, &storj, &cancel, limit, shard).await;
    if let Err(err) = &result {
        let _ = mark_job_run_failed(&client, job_run_id, &err.to_string()).await;
        return Err(anyhow::anyhow!("{err}"));
    }
    Ok(())
}

async fn run_inner(
    client: &mut Client,
    job_run_id: Uuid,
    s3: &S3Client,
    storj: &StorjS3Client,
    cancel: &CancellationToken,
    limit: Option<i64>,
    shard: Option<(i64, i64)>,
) -> Result<(), PHashJobError> {
    let mut summary = PHashSummary {
        job_run_id,
        scanned_rows: 0,
        hash_rows_upserted: 0,
        hash_feed_events_appended: 0,
        row_failures: 0,
    };

    let page_size = crate::consts::SCAN_PAGE_SIZE.load(std::sync::atomic::Ordering::Relaxed);
    let concurrency = crate::consts::PHASH_CONCURRENCY.load(std::sync::atomic::Ordering::Relaxed);

    // Exclusive `video_id` cursor. We advance it past every fetched row so a row
    // that fails to produce a hash (permanent compute failure) is not re-fetched
    // on the next iteration — otherwise the missing-hash scan would loop forever.
    // Such rows stay "missing" and are retried on the next full job run.
    let mut after: Option<String> = None;
    let mut cancelled = false;

    loop {
        if cancel.is_cancelled() {
            tracing::info!(scanned = summary.scanned_rows, "media_phash: cancelled");
            cancelled = true;
            break;
        }

        // Respect limit: never fetch more rows than needed.
        let remaining = limit.map(|l| l - summary.scanned_rows);
        if remaining == Some(0) {
            break;
        }
        let batch_limit = match (remaining, page_size) {
            (Some(r), p) => Some(r.min(p)),
            (None, p) => Some(p),
        };

        let rows = videos_missing_canonical_phash(
            client,
            HASH_KIND,
            HASH_VERSION,
            INPUT_MEDIA_VERSION,
            after.as_deref(),
            batch_limit,
            shard,
        )
        .await?;

        if rows.is_empty() {
            break;
        }

        // Rows are ordered by video_id; advance the cursor past this batch.
        after = rows.last().map(|row| row.video_id.clone());

        // Parallel download + hash; bounded by PHASH_CONCURRENCY to avoid
        // exhausting tempfile descriptors.  NEVER use join_all here.
        // Stream-persist: each row is persisted as soon as it completes rather
        // than collecting the whole page first, enabling per-row cancellation.
        let mut stream = futures::stream::iter(rows)
            .map(|row| {
                let s3 = s3.clone();
                let storj = storj.clone();
                async move {
                    let hash_result: Result<VideoHashResult, (&'static str, String)> = async {
                        let tmp = NamedTempFile::new()
                            .map_err(|e| ("phash_download", format!("tempfile: {e}")))?;
                        {
                            let mut f = tokio::fs::File::from_std(
                                tmp.as_file()
                                    .try_clone()
                                    .map_err(|e| ("phash_download", format!("file clone: {e}")))?,
                            );
                            match row.storage_provider.as_deref() {
                                Some("hetzner") => {
                                    let key = row.object_key.as_deref().ok_or_else(|| {
                                        (
                                            "phash_download",
                                            "hetzner row missing object_key".to_string(),
                                        )
                                    })?;
                                    s3.download_to_file(key, &mut f).await.map_err(|e| {
                                        ("phash_download", format!("hetzner download {key}: {e}"))
                                    })?;
                                }
                                Some("storj") | None => {
                                    let key = row.object_key.as_deref().ok_or_else(|| {
                                        (
                                            "phash_download",
                                            "storj row missing object_key".to_string(),
                                        )
                                    })?;
                                    storj.download_to_file(key, &mut f).await.map_err(|e| {
                                        ("phash_download", format!("storj download {key}: {e}"))
                                    })?;
                                }
                                Some(provider) => {
                                    return Err((
                                        "phash_download",
                                        format!("unknown storage provider: {provider}"),
                                    ));
                                }
                            }
                        }

                        let path = tmp.path().to_owned();
                        let result = tokio::task::spawn_blocking(move || {
                            phash::PHasher::new().hash_video_with_metadata(&path)
                        })
                        .await
                        .map_err(|e| ("phash_decode", format!("spawn_blocking panic: {e}")))
                        .and_then(|r| r.map_err(|e| ("phash_decode", format!("phash: {e}"))))?;

                        drop(tmp);
                        Ok(result)
                    }
                    .await;

                    (row, hash_result)
                }
            })
            .buffer_unordered(concurrency);

        while let Some((row, hash_result)) = stream.next().await {
            if cancel.is_cancelled() {
                cancelled = true;
                break;
            }
            summary.scanned_rows += 1;
            persist_one(client, job_run_id, &row, hash_result, &mut summary).await?;
            crate::jobs::log_progress(summary.scanned_rows as usize, JOB_KIND);
        }
        drop(stream);
        if cancelled {
            break;
        }
        // Best-effort live progress flush — never abort the job on failure.
        let _ = crate::media_index::update_job_run_totals(
            client,
            job_run_id,
            &summary_totals(&summary),
        )
        .await;
    }

    complete_job_run(
        client,
        &summary,
        terminal_status(cancelled, summary.row_failures),
    )
    .await?;

    tracing::info!(
        scanned = summary.scanned_rows,
        upserted = summary.hash_rows_upserted,
        events = summary.hash_feed_events_appended,
        failures = summary.row_failures,
        "media_phash: complete"
    );
    Ok(())
}

/// Maps per-row failure count to the terminal `media_job_runs` status.
/// `failed` is reserved for whole-job aborts and is set elsewhere.
fn run_status(row_failures: i64) -> &'static str {
    if row_failures == 0 {
        "succeeded"
    } else {
        "succeeded_with_failures"
    }
}

/// Maps cancellation flag and per-row failure count to the terminal status.
/// A cancelled run always finalises as `cancelled`, regardless of failures.
fn terminal_status(cancelled: bool, row_failures: i64) -> &'static str {
    if cancelled {
        "cancelled"
    } else {
        run_status(row_failures)
    }
}

/// Pure persistence function — testable with synthetic `VideoHashResult`.
///
/// Accepts a pre-computed hash result (or an error string describing a compute
/// failure) and writes:
/// - on success: upsert `servable_video_hashes` + optional `media_feed_events`
///   (both in the same serialized transaction)
/// - on failure: record in `media_job_failures`
///
/// Updates `summary` in place.
pub async fn persist_one(
    client: &mut Client,
    job_run_id: Uuid,
    row: &MissingHashRow,
    hash_result: Result<VideoHashResult, (&'static str, String)>,
    summary: &mut PHashSummary,
) -> Result<(), PHashJobError> {
    let tx = client.transaction().await?;

    match hash_result {
        Ok(vhr) => {
            let metadata = build_metadata_json(&vhr.metadata);
            let hash_value = &vhr.hash;
            let hash_bit_length = hash_value.len() as i32;

            let outcome = crate::media_index::upsert_hash_record_txn(
                &tx,
                HashRecordInput {
                    video_id: &row.video_id,
                    hash_kind: HASH_KIND,
                    hash_version: HASH_VERSION,
                    input_media_version: INPUT_MEDIA_VERSION,
                    hash_value,
                    hash_bit_length,
                    num_frames: OFFCHAIN_NUM_FRAMES,
                    hash_size: OFFCHAIN_HASH_SIZE,
                    computed_from_provider: row.storage_provider.as_deref(),
                    computed_from_bucket: row.bucket.as_deref(),
                    computed_from_key: row.object_key.as_deref(),
                    metadata: Some(metadata.clone()),
                },
            )
            .await?;

            if matches!(outcome, UpsertOutcome::Inserted | UpsertOutcome::Changed) {
                let feed_payload = build_feed_payload(row, hash_value, &metadata);
                crate::media_index::append_feed_event_txn(
                    &tx,
                    FeedEventInput {
                        event_kind: FeedEventKind::HashUpserted,
                        video_id: &row.video_id,
                        hash_kind: Some(HASH_KIND),
                        hash_version: Some(HASH_VERSION),
                        input_media_version: Some(INPUT_MEDIA_VERSION),
                        payload: feed_payload,
                    },
                )
                .await?;
                summary.hash_rows_upserted += 1;
                summary.hash_feed_events_appended += 1;
            }

            tx.commit().await?;
        }
        Err((phase, err)) => {
            tracing::error!(
                video_id = %row.video_id,
                phase,
                error = %err,
                "media_phash: row failed"
            );
            record_row_failure_txn(&tx, job_run_id, &row.video_id, phase, &err).await?;
            tx.commit().await?;
            summary.row_failures += 1;
        }
    }

    Ok(())
}

fn build_metadata_json(meta: &VideoMetadata) -> Value {
    json!({
        "duration_seconds": meta.duration_seconds,
        "frame_count": meta.frame_count,
        "width": meta.width,
        "height": meta.height,
        "fps": meta.fps,
    })
}

fn build_feed_payload(row: &MissingHashRow, hash_value: &str, metadata: &Value) -> Value {
    json!({
        "video_id": row.video_id,
        "servable_status": row.servable_status,
        "storage_provider": row.storage_provider,
        "bucket": row.bucket,
        "object_key": row.object_key,
        "hash_kind": HASH_KIND,
        "hash_version": HASH_VERSION,
        "input_media_version": INPUT_MEDIA_VERSION,
        "hash_value": hash_value,
        "metadata": metadata,
    })
}

// ── job_runs / failures helpers ──────────────────────────────────────────────

async fn insert_job_run(
    client: &Client,
    job_run_id: Uuid,
    requested_by: &str,
) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            "INSERT INTO media_job_runs (id, job_kind, status, requested_by)
             VALUES ($1::TEXT::UUID, $2, 'running', $3)",
            &[&job_run_id.to_string(), &JOB_KIND, &requested_by],
        )
        .await?;
    Ok(())
}

async fn complete_job_run(
    client: &Client,
    summary: &PHashSummary,
    status: &str,
) -> Result<(), tokio_postgres::Error> {
    let totals = summary_totals(summary);
    client
        .execute(
            "UPDATE media_job_runs
             SET status = $2,
                 finished_at = NOW(),
                 totals = $3,
                 error_message = NULL
             WHERE id = $1::TEXT::UUID",
            &[&summary.job_run_id.to_string(), &status, &totals],
        )
        .await?;
    Ok(())
}

async fn mark_job_run_failed(
    client: &Client,
    job_run_id: Uuid,
    error: &str,
) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            "UPDATE media_job_runs
             SET status = 'failed',
                 finished_at = NOW(),
                 error_message = $2
             WHERE id = $1::TEXT::UUID",
            &[&job_run_id.to_string(), &error],
        )
        .await?;
    Ok(())
}

async fn record_row_failure_txn(
    tx: &Transaction<'_>,
    job_run_id: Uuid,
    video_id: &str,
    phase: &str,
    error: &str,
) -> Result<(), tokio_postgres::Error> {
    tx.execute(
        "INSERT INTO media_job_failures (
            job_run_id,
            job_kind,
            item_key,
            video_id,
            phase,
            source_ref,
            last_error,
            status
         )
         VALUES ($1::TEXT::UUID, $2, $3, $4, $5, $6, $7, 'pending_retry')
         ON CONFLICT (job_kind, item_key, phase) DO UPDATE
         SET job_run_id = EXCLUDED.job_run_id,
             video_id = EXCLUDED.video_id,
             source_ref = EXCLUDED.source_ref,
             last_error = EXCLUDED.last_error,
             status = EXCLUDED.status",
        &[
            &job_run_id.to_string(),
            &JOB_KIND,
            &video_id,
            &video_id,
            &phase,
            &video_id,
            &error,
        ],
    )
    .await?;
    Ok(())
}

fn summary_totals(summary: &PHashSummary) -> Value {
    json!({
        "scanned_rows": summary.scanned_rows,
        "hash_rows_upserted": summary.hash_rows_upserted,
        "hash_feed_events_appended": summary.hash_feed_events_appended,
        "row_failures": summary.row_failures,
    })
}

// ── helpers to build synthetic test rows ─────────────────────────────────────

#[cfg(test)]
pub(crate) fn make_hash_result(hash_value: &str) -> VideoHashResult {
    VideoHashResult {
        hash: hash_value.to_string(),
        metadata: VideoMetadata {
            duration_seconds: 10.0,
            frame_count: 120,
            width: 1280,
            height: 720,
            fps: 30.0,
        },
        hash_kind: HASH_KIND,
        hash_version: HASH_VERSION,
    }
}

#[cfg(test)]
pub(crate) fn make_row(video_id: &str) -> MissingHashRow {
    MissingHashRow {
        video_id: video_id.to_string(),
        storage_provider: Some("hetzner".to_string()),
        bucket: None,
        object_key: Some(format!("videos/{video_id}.mp4")),
        servable_status: "servable".to_string(),
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::media_index::test_support::test_client;

    async fn init_test_schema(client: &tokio_postgres::Client) {
        crate::db::init_schema(client).await.unwrap();
        crate::media_index::init_schema(client).await.unwrap();
    }

    /// Seed a row into `all_servable_videos_on_yral` without going through the
    /// full upsert helper (which requires video_index / feed machinery).
    async fn seed_video(client: &tokio_postgres::Client, video_id: &str) {
        client
            .execute(
                "INSERT INTO all_servable_videos_on_yral (
                    video_id,
                    source_kind,
                    servable_status,
                    storage_provider,
                    object_key,
                    discovered_from
                 )
                 VALUES ($1, 'test', 'servable', 'hetzner', $2, 'test')
                 ON CONFLICT (video_id) DO NOTHING",
                &[&video_id, &format!("videos/{video_id}.mp4")],
            )
            .await
            .unwrap();
    }

    fn make_run_id() -> Uuid {
        Uuid::new_v4()
    }

    // ── test 1: successful compute writes a servable_video_hashes row ─────────

    #[tokio::test]
    async fn successful_compute_writes_hash_row_with_canonical_labels_and_metadata() {
        let (_pg, mut client) = test_client().await;
        init_test_schema(&client).await;
        seed_video(&client, "video-a").await;

        let job_run_id = make_run_id();
        insert_job_run(&client, job_run_id, "test-runner")
            .await
            .unwrap();

        let row = make_row("video-a");
        let vhr = make_hash_result("1010101010");

        let mut summary = PHashSummary {
            job_run_id,
            scanned_rows: 0,
            hash_rows_upserted: 0,
            hash_feed_events_appended: 0,
            row_failures: 0,
        };
        persist_one(&mut client, job_run_id, &row, Ok(vhr), &mut summary)
            .await
            .unwrap();

        let hash_row = client
            .query_one(
                "SELECT hash_kind,
                        hash_version,
                        input_media_version,
                        hash_value,
                        hash_bit_length,
                        num_frames,
                        hash_size,
                        computed_from_provider,
                        computed_from_bucket,
                        computed_from_key,
                        metadata
                 FROM servable_video_hashes
                 WHERE video_id = 'video-a'",
                &[],
            )
            .await
            .unwrap();

        assert_eq!(hash_row.get::<_, String>(0), HASH_KIND);
        assert_eq!(hash_row.get::<_, String>(1), HASH_VERSION);
        assert_eq!(hash_row.get::<_, String>(2), INPUT_MEDIA_VERSION);
        assert_eq!(hash_row.get::<_, String>(3), "1010101010");
        assert_eq!(hash_row.get::<_, i32>(4), 10); // hash.len()
        assert_eq!(hash_row.get::<_, i32>(5), OFFCHAIN_NUM_FRAMES);
        assert_eq!(hash_row.get::<_, i32>(6), OFFCHAIN_HASH_SIZE);
        assert_eq!(
            hash_row.get::<_, Option<String>>(7).as_deref(),
            Some("hetzner")
        );
        assert_eq!(hash_row.get::<_, Option<String>>(8), None); // no bucket for hetzner
        assert_eq!(
            hash_row.get::<_, Option<String>>(9).as_deref(),
            Some("videos/video-a.mp4")
        );

        let meta: Value = hash_row.get(10);
        assert_eq!(meta["duration_seconds"], 10.0);
        assert_eq!(meta["frame_count"], 120);
        assert_eq!(meta["width"], 1280);
        assert_eq!(meta["height"], 720);
        assert_eq!(meta["fps"], 30.0);

        assert_eq!(summary.hash_rows_upserted, 1);
    }

    // ── test 2: successful compute appends exactly ONE feed event ─────────────

    #[tokio::test]
    async fn successful_compute_appends_exactly_one_hash_upserted_feed_event() {
        let (_pg, mut client) = test_client().await;
        init_test_schema(&client).await;
        seed_video(&client, "video-b").await;

        let job_run_id = make_run_id();
        insert_job_run(&client, job_run_id, "test-runner")
            .await
            .unwrap();

        let row = make_row("video-b");
        let vhr = make_hash_result("0011001100");

        let mut summary = PHashSummary {
            job_run_id,
            scanned_rows: 0,
            hash_rows_upserted: 0,
            hash_feed_events_appended: 0,
            row_failures: 0,
        };
        persist_one(&mut client, job_run_id, &row, Ok(vhr), &mut summary)
            .await
            .unwrap();

        let events = crate::media_index::list_feed_events_after(&client, 0, 10)
            .await
            .unwrap();
        let hash_events: Vec<_> = events
            .iter()
            .filter(|e| e.event_kind == FeedEventKind::HashUpserted)
            .collect();

        assert_eq!(hash_events.len(), 1);
        let ev = hash_events[0];
        assert_eq!(ev.video_id, "video-b");
        assert_eq!(ev.hash_kind.as_deref(), Some(HASH_KIND));
        assert_eq!(ev.hash_version.as_deref(), Some(HASH_VERSION));
        assert_eq!(ev.input_media_version.as_deref(), Some(INPUT_MEDIA_VERSION));
        assert_eq!(ev.payload["hash_value"], "0011001100");
        assert_eq!(ev.payload["storage_provider"], "hetzner");
        assert_eq!(summary.hash_feed_events_appended, 1);
    }

    // ── test 3: recompute of same result is idempotent ────────────────────────

    #[tokio::test]
    async fn recompute_of_same_result_is_idempotent_no_new_feed_event_no_duplicate_row() {
        let (_pg, mut client) = test_client().await;
        init_test_schema(&client).await;
        seed_video(&client, "video-c").await;

        let job_run_id = make_run_id();
        insert_job_run(&client, job_run_id, "test-runner")
            .await
            .unwrap();

        let row = make_row("video-c");
        let vhr = make_hash_result("1111000011");

        let mut summary1 = PHashSummary {
            job_run_id,
            scanned_rows: 0,
            hash_rows_upserted: 0,
            hash_feed_events_appended: 0,
            row_failures: 0,
        };
        persist_one(
            &mut client,
            job_run_id,
            &row,
            Ok(vhr.clone()),
            &mut summary1,
        )
        .await
        .unwrap();

        let mut summary2 = PHashSummary {
            job_run_id,
            scanned_rows: 0,
            hash_rows_upserted: 0,
            hash_feed_events_appended: 0,
            row_failures: 0,
        };
        persist_one(&mut client, job_run_id, &row, Ok(vhr), &mut summary2)
            .await
            .unwrap();

        // First call: Inserted -> event emitted
        assert_eq!(summary1.hash_rows_upserted, 1);
        assert_eq!(summary1.hash_feed_events_appended, 1);

        // Second call: Unchanged -> no new event
        assert_eq!(summary2.hash_rows_upserted, 0);
        assert_eq!(summary2.hash_feed_events_appended, 0);

        // Only one row in the table
        let count: i64 = client
            .query_one(
                "SELECT COUNT(*) FROM servable_video_hashes WHERE video_id = 'video-c'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(count, 1);

        // Only one feed event
        let events = crate::media_index::list_feed_events_after(&client, 0, 10)
            .await
            .unwrap();
        let hash_events = events
            .iter()
            .filter(|e| e.event_kind == FeedEventKind::HashUpserted && e.video_id == "video-c")
            .count();
        assert_eq!(hash_events, 1);
    }

    // ── test 4: two videos with the same pHash both persist and are found ─────

    #[tokio::test]
    async fn two_videos_with_same_phash_both_persist_and_are_returned_by_find_exact_duplicates() {
        let (_pg, mut client) = test_client().await;
        init_test_schema(&client).await;
        seed_video(&client, "video-d1").await;
        seed_video(&client, "video-d2").await;

        let job_run_id = make_run_id();
        insert_job_run(&client, job_run_id, "test-runner")
            .await
            .unwrap();

        let shared_hash = "1100110011";

        for video_id in ["video-d1", "video-d2"] {
            let row = make_row(video_id);
            let vhr = make_hash_result(shared_hash);
            let mut summary = PHashSummary {
                job_run_id,
                scanned_rows: 0,
                hash_rows_upserted: 0,
                hash_feed_events_appended: 0,
                row_failures: 0,
            };
            persist_one(&mut client, job_run_id, &row, Ok(vhr), &mut summary)
                .await
                .unwrap();
            assert_eq!(summary.hash_rows_upserted, 1);
        }

        let duplicates = crate::media_index::find_exact_duplicates(
            &client,
            crate::media_index::ExactDuplicateQuery {
                hash_kind: HASH_KIND,
                hash_version: HASH_VERSION,
                hash_value: shared_hash,
            },
        )
        .await
        .unwrap();

        assert_eq!(duplicates.len(), 2);
        assert!(duplicates.iter().any(|r| r.video_id == "video-d1"));
        assert!(duplicates.iter().any(|r| r.video_id == "video-d2"));
    }

    // ── test 5: worker failure is recorded in media_job_failures ─────────────

    #[tokio::test]
    async fn worker_failure_is_recorded_in_job_failures_and_run_completes_succeeded_with_failures()
    {
        let (_pg, mut client) = test_client().await;
        init_test_schema(&client).await;
        seed_video(&client, "video-e").await;

        let job_run_id = make_run_id();
        insert_job_run(&client, job_run_id, "test-runner")
            .await
            .unwrap();

        let row = make_row("video-e");
        let mut summary = PHashSummary {
            job_run_id,
            scanned_rows: 1,
            hash_rows_upserted: 0,
            hash_feed_events_appended: 0,
            row_failures: 0,
        };

        persist_one(
            &mut client,
            job_run_id,
            &row,
            Err(("phash_decode", "phash: no video stream".to_string())),
            &mut summary,
        )
        .await
        .unwrap();

        assert_eq!(summary.row_failures, 1);
        assert_eq!(summary.hash_rows_upserted, 0);

        let failure_row = client
            .query_one(
                "SELECT job_run_id::text,
                        job_kind,
                        item_key,
                        video_id,
                        phase,
                        last_error,
                        status
                 FROM media_job_failures
                 WHERE video_id = 'video-e'",
                &[],
            )
            .await
            .unwrap();

        assert_eq!(failure_row.get::<_, String>(0), job_run_id.to_string());
        assert_eq!(failure_row.get::<_, String>(1), JOB_KIND);
        assert_eq!(failure_row.get::<_, String>(2), "video-e");
        assert_eq!(failure_row.get::<_, String>(3), "video-e");
        assert_eq!(failure_row.get::<_, String>(4), "phash_decode");
        assert_eq!(failure_row.get::<_, String>(5), "phash: no video stream");
        assert_eq!(failure_row.get::<_, String>(6), "pending_retry");

        // Verify job completes with succeeded_with_failures status
        complete_job_run(&client, &summary, run_status(summary.row_failures))
            .await
            .unwrap();

        let run_row = client
            .query_one(
                "SELECT status FROM media_job_runs WHERE id = $1::TEXT::UUID",
                &[&job_run_id.to_string()],
            )
            .await
            .unwrap();
        assert_eq!(run_row.get::<_, String>(0), "succeeded_with_failures");
    }

    // ── test 6: missing-hash scan helper ─────────────────────────────────────

    #[tokio::test]
    async fn missing_hash_scan_returns_only_rows_lacking_canonical_hash_and_respects_limit() {
        let (_pg, mut client) = test_client().await;
        init_test_schema(&client).await;

        // Seed two videos
        seed_video(&client, "video-f1").await;
        seed_video(&client, "video-f2").await;

        // Insert the canonical hash for video-f1 only
        let job_run_id = make_run_id();
        insert_job_run(&client, job_run_id, "test-runner")
            .await
            .unwrap();
        let row_with_hash = make_row("video-f1");
        let mut summary = PHashSummary {
            job_run_id,
            scanned_rows: 0,
            hash_rows_upserted: 0,
            hash_feed_events_appended: 0,
            row_failures: 0,
        };
        persist_one(
            &mut client,
            job_run_id,
            &row_with_hash,
            Ok(make_hash_result("0101010101")),
            &mut summary,
        )
        .await
        .unwrap();

        // Without limit (no cursor): only video-f2 should be returned
        let missing = crate::media_index::videos_missing_canonical_phash(
            &client,
            HASH_KIND,
            HASH_VERSION,
            INPUT_MEDIA_VERSION,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(missing.len(), 1, "expected exactly one missing row");
        assert_eq!(missing[0].video_id, "video-f2");

        // Seed a third video
        seed_video(&client, "video-f3").await;

        // With limit=1: still only one row returned
        let missing_limited = crate::media_index::videos_missing_canonical_phash(
            &client,
            HASH_KIND,
            HASH_VERSION,
            INPUT_MEDIA_VERSION,
            None,
            Some(1),
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            missing_limited.len(),
            1,
            "limit=1 should return exactly 1 row"
        );
    }

    // ── test 8: cancelled run finalizes with cancelled status ─────────────────

    #[test]
    fn cancelled_run_finalizes_cancelled_status() {
        assert_eq!(terminal_status(true, 0), "cancelled");
        assert_eq!(terminal_status(true, 5), "cancelled");
        assert_eq!(terminal_status(false, 0), "succeeded");
        assert_eq!(terminal_status(false, 3), "succeeded_with_failures");
    }

    // ── test 9: decode failures are recorded under phash_decode phase ─────────

    #[tokio::test]
    async fn persist_records_decode_failures_under_phash_decode() {
        let (_pg, mut client) = test_client().await;
        init_test_schema(&client).await;
        let row = make_row("vid-x");
        let mut summary = PHashSummary {
            job_run_id: make_run_id(),
            scanned_rows: 0,
            hash_rows_upserted: 0,
            hash_feed_events_appended: 0,
            row_failures: 0,
        };
        insert_job_run(&client, summary.job_run_id, "t")
            .await
            .unwrap();
        super::persist_one(
            &mut client,
            summary.job_run_id,
            &row,
            Err(("phash_decode", "phash: no video stream".to_string())),
            &mut summary,
        )
        .await
        .unwrap();
        let phase: String = client
            .query_one(
                "SELECT phase FROM media_job_failures WHERE video_id='vid-x'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(phase, "phash_decode");
    }

    // ── test 7: scan cursor advances past fetched rows (forward progress) ─────
    // Guards against the missing-hash loop re-fetching a permanently-failing row
    // forever: paging by an exclusive video_id cursor must skip rows already seen.

    #[tokio::test]
    async fn missing_hash_scan_cursor_excludes_rows_at_or_before_it() {
        let (_pg, client) = test_client().await;
        init_test_schema(&client).await;

        // Three unhashed videos; none get a canonical hash (simulates a batch
        // where every row failed to compute).
        seed_video(&client, "video-g1").await;
        seed_video(&client, "video-g2").await;
        seed_video(&client, "video-g3").await;

        // First page (no cursor, limit 2): the first two by video_id.
        let page1 = crate::media_index::videos_missing_canonical_phash(
            &client,
            HASH_KIND,
            HASH_VERSION,
            INPUT_MEDIA_VERSION,
            None,
            Some(2),
            None,
        )
        .await
        .unwrap();
        let page1_ids: Vec<_> = page1.iter().map(|r| r.video_id.as_str()).collect();
        assert_eq!(page1_ids, vec!["video-g1", "video-g2"]);

        // Next page with cursor = last id of page1: must NOT re-return g1/g2,
        // even though they are still missing a hash.
        let page2 = crate::media_index::videos_missing_canonical_phash(
            &client,
            HASH_KIND,
            HASH_VERSION,
            INPUT_MEDIA_VERSION,
            Some("video-g2"),
            Some(2),
            None,
        )
        .await
        .unwrap();
        let page2_ids: Vec<_> = page2.iter().map(|r| r.video_id.as_str()).collect();
        assert_eq!(page2_ids, vec!["video-g3"]);

        // Cursor past the last row terminates the scan.
        let page3 = crate::media_index::videos_missing_canonical_phash(
            &client,
            HASH_KIND,
            HASH_VERSION,
            INPUT_MEDIA_VERSION,
            Some("video-g3"),
            Some(2),
            None,
        )
        .await
        .unwrap();
        assert!(page3.is_empty(), "cursor past the last row yields no rows");
    }

    // ── test 10: download failures are recorded under phash_download phase ────

    #[tokio::test]
    async fn persist_records_download_failures_under_phash_download() {
        let (_pg, mut client) = test_client().await;
        init_test_schema(&client).await;
        let row = make_row("vid-y");
        let mut summary = PHashSummary {
            job_run_id: make_run_id(),
            scanned_rows: 0,
            hash_rows_upserted: 0,
            hash_feed_events_appended: 0,
            row_failures: 0,
        };
        insert_job_run(&client, summary.job_run_id, "t")
            .await
            .unwrap();
        super::persist_one(
            &mut client,
            summary.job_run_id,
            &row,
            Err((
                "phash_download",
                "storj download vid-y.mp4: 404".to_string(),
            )),
            &mut summary,
        )
        .await
        .unwrap();
        let phase: String = client
            .query_one(
                "SELECT phase FROM media_job_failures WHERE video_id='vid-y'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(phase, "phash_download");
    }
}
