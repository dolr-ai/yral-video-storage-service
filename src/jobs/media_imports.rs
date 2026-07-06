use serde_json::{json, Value};
use tokio_postgres::{Client, Transaction};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const JOB_KIND: &str = "legacy_video_index_import";
const SOURCE_KIND: &str = "legacy_video_index";
const INPUT_MEDIA_VERSION: &str = "legacy_video_index_object_v1";

#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "the import summary reports scanned/imported/failed counts and should be inspected"]
pub struct ImportSummary {
    pub job_run_id: Uuid,
    pub scanned_rows: i64,
    pub imported_media_rows: i64,
    pub hash_rows_upserted: i64,
    pub hash_feed_events_appended: i64,
    pub row_failures: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("invalid legacy video_index import limit {0}; expected a non-negative value")]
    InvalidLimit(i64),
    #[error("postgres legacy video_index import failed: {0}")]
    Postgres(#[from] tokio_postgres::Error),
    #[error("import aborted after {consecutive} consecutive row failures (systemic error); last: {last_error}")]
    TooManyConsecutiveFailures {
        consecutive: i64,
        last_error: String,
    },
}

/// If this many rows fail consecutively in the per-row fallback, treat the
/// error as systemic (not one bad row) and fail the whole job. Reset to 0 on
/// any successful row / committed batch.
const MAX_CONSECUTIVE_IMPORT_FAILURES: i64 = 50;

#[derive(Debug, Clone)]
struct LegacyVideoIndexRow {
    video_id: String,
    storj_key: Option<String>,
    hetzner_key: Option<String>,
    phash: Option<String>,
    phash_kind: Option<String>,
    phash_version: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct StorageRef<'a> {
    provider: &'static str,
    bucket: Option<&'a str>,
    object_key: &'a str,
}

pub async fn import_current_video_index(
    client: &mut Client,
    requested_by: &str,
    limit: Option<i64>,
    cancel: &CancellationToken,
) -> Result<ImportSummary, ImportError> {
    let job_run_id = Uuid::new_v4();
    insert_job_run(client, job_run_id, requested_by).await?;

    if let Some(limit) = limit {
        if limit < 0 {
            let err = ImportError::InvalidLimit(limit);
            // Best-effort: never let a failed status update mask the real error.
            let _ = mark_job_run_failed(client, job_run_id, &err.to_string()).await;
            return Err(err);
        }
    }

    let result = import_current_video_index_inner(client, job_run_id, limit, cancel).await;
    if let Err(err) = &result {
        // Best-effort: preserve the original error even if the status update fails.
        let _ = mark_job_run_failed(client, job_run_id, &err.to_string()).await;
    }
    result
}

#[derive(Debug, Default, Clone, Copy)]
struct RowCounts {
    imported_media_rows: i64,
    hash_rows_upserted: i64,
    hash_feed_events_appended: i64,
    row_failures: i64,
}

/// Import a single legacy row within an existing transaction. Keyless rows are
/// recorded as a failure (NOT an error) and return `row_failures = 1`. Returns
/// the per-row counter deltas. Any returned `Err` is a real SQL error that
/// poisons `tx` and must trigger the caller's rollback + per-row fallback.
async fn import_one_row_txn(
    tx: &Transaction<'_>,
    row: &LegacyVideoIndexRow,
    job_run_id: Uuid,
) -> Result<RowCounts, tokio_postgres::Error> {
    let mut counts = RowCounts::default();

    let Some(storage) = canonical_storage(row) else {
        record_row_failure_txn(
            tx,
            job_run_id,
            &row.video_id,
            "storage_selection",
            "legacy video_index row has neither storj_key nor hetzner_key",
        )
        .await?;
        counts.row_failures = 1;
        return Ok(counts);
    };

    let media_outcome = import_media_row_txn(tx, row, storage).await?;
    if matches!(
        media_outcome.media,
        crate::media_index::UpsertOutcome::Inserted | crate::media_index::UpsertOutcome::Changed
    ) {
        crate::media_index::append_feed_event_txn(
            tx,
            crate::media_index::FeedEventInput {
                event_kind: crate::media_index::FeedEventKind::MediaVisibilityChanged,
                video_id: &row.video_id,
                hash_kind: None,
                hash_version: None,
                input_media_version: None,
                payload: media_feed_payload(row, storage),
            },
        )
        .await?;
        counts.imported_media_rows = 1;
    }

    if let (Some(phash), Some(phash_kind), Some(phash_version)) = (
        row.phash.as_deref(),
        row.phash_kind.as_deref(),
        row.phash_version.as_deref(),
    ) {
        let provenance = hash_provenance(row);
        let outcome = crate::media_index::upsert_hash_record_txn(
            tx,
            crate::media_index::HashRecordInput {
                video_id: &row.video_id,
                hash_kind: phash_kind,
                hash_version: phash_version,
                input_media_version: INPUT_MEDIA_VERSION,
                hash_value: phash,
                hash_bit_length: phash.len() as i32,
                num_frames: 0,
                hash_size: 0,
                computed_from_provider: provenance.map(|s| s.provider),
                computed_from_bucket: provenance.and_then(|s| s.bucket),
                computed_from_key: provenance.map(|s| s.object_key),
                metadata: Some(json!({"source": SOURCE_KIND})),
            },
        )
        .await?;

        if matches!(
            outcome,
            crate::media_index::UpsertOutcome::Inserted
                | crate::media_index::UpsertOutcome::Changed
        ) {
            crate::media_index::append_feed_event_txn(
                tx,
                crate::media_index::FeedEventInput {
                    event_kind: crate::media_index::FeedEventKind::HashUpserted,
                    video_id: &row.video_id,
                    hash_kind: Some(phash_kind),
                    hash_version: Some(phash_version),
                    input_media_version: Some(INPUT_MEDIA_VERSION),
                    payload: json!({
                        "video_id": row.video_id,
                        "hash_kind": phash_kind,
                        "hash_version": phash_version,
                        "input_media_version": INPUT_MEDIA_VERSION,
                        "hash_value": phash,
                        "source": SOURCE_KIND
                    }),
                },
            )
            .await?;
            counts.hash_rows_upserted = 1;
            counts.hash_feed_events_appended = 1;
        }
    }

    Ok(counts)
}

async fn import_current_video_index_inner(
    client: &mut Client,
    job_run_id: Uuid,
    limit: Option<i64>,
    cancel: &CancellationToken,
) -> Result<ImportSummary, ImportError> {
    let mut summary = ImportSummary {
        job_run_id,
        scanned_rows: 0,
        imported_media_rows: 0,
        hash_rows_upserted: 0,
        hash_feed_events_appended: 0,
        row_failures: 0,
    };
    let batch_size =
        crate::consts::MEDIA_IMPORT_BATCH_SIZE.load(std::sync::atomic::Ordering::Relaxed);
    let mut after: Option<String> = None;
    let mut cancelled = false;
    let mut consecutive_failures: i64 = 0; // for the systemic-error circuit breaker

    loop {
        if cancel.is_cancelled() {
            cancelled = true;
            break;
        }

        // Global limit: cap rows fetched across the whole run.
        let remaining = limit.map(|l| l - summary.scanned_rows);
        if remaining == Some(0) {
            break;
        }
        let page_limit = match remaining {
            Some(r) => r.min(batch_size),
            None => batch_size,
        };

        let page = fetch_missing_legacy_rows_after(client, after.as_deref(), page_limit).await?;
        if page.is_empty() {
            break;
        }
        // Advance the cursor on FETCH (not on import success) so an all-failing
        // page still makes forward progress and the loop terminates.
        after = page.last().map(|r| r.video_id.clone());

        // Optimistic batch: import the whole page in one transaction.
        let mut page_counts = RowCounts::default();
        let tx = client.transaction().await?;
        let mut batch_ok = true;
        for row in &page {
            match import_one_row_txn(&tx, row, job_run_id).await {
                Ok(c) => {
                    page_counts.imported_media_rows += c.imported_media_rows;
                    page_counts.hash_rows_upserted += c.hash_rows_upserted;
                    page_counts.hash_feed_events_appended += c.hash_feed_events_appended;
                    page_counts.row_failures += c.row_failures;
                }
                Err(_) => {
                    batch_ok = false;
                    break;
                }
            }
        }

        if batch_ok {
            tx.commit().await?;
            summary.imported_media_rows += page_counts.imported_media_rows;
            summary.hash_rows_upserted += page_counts.hash_rows_upserted;
            summary.hash_feed_events_appended += page_counts.hash_feed_events_appended;
            summary.row_failures += page_counts.row_failures;
            consecutive_failures = 0; // a batch committed cleanly → not systemic
        } else {
            // A real SQL error poisoned the batch tx. Roll it back and reprocess
            // this page row-by-row to isolate the offending row.
            drop(tx);
            for row in &page {
                let row_tx = client.transaction().await?;
                match import_one_row_txn(&row_tx, row, job_run_id).await {
                    Ok(c) => {
                        row_tx.commit().await?;
                        summary.imported_media_rows += c.imported_media_rows;
                        summary.hash_rows_upserted += c.hash_rows_upserted;
                        summary.hash_feed_events_appended += c.hash_feed_events_appended;
                        summary.row_failures += c.row_failures;
                        consecutive_failures = 0; // a row succeeded (incl. handled keyless) → not systemic
                    }
                    Err(e) => {
                        drop(row_tx);
                        let fail_tx = client.transaction().await?;
                        record_row_failure_txn(
                            &fail_tx,
                            job_run_id,
                            &row.video_id,
                            "import_error",
                            &e.to_string(),
                        )
                        .await?;
                        fail_tx.commit().await?;
                        summary.row_failures += 1;
                        consecutive_failures += 1;
                        if consecutive_failures >= MAX_CONSECUTIVE_IMPORT_FAILURES {
                            // Systemic error (every row failing the same way) → fail loud.
                            return Err(ImportError::TooManyConsecutiveFailures {
                                consecutive: consecutive_failures,
                                last_error: e.to_string(),
                            });
                        }
                    }
                }
            }
        }

        summary.scanned_rows += page.len() as i64;
        // Best-effort live progress flush — never abort the import on failure.
        let _ = crate::media_index::update_job_run_totals(
            client,
            job_run_id,
            &summary_totals(&summary),
        )
        .await;
    }

    let status = if cancelled {
        "cancelled"
    } else if summary.row_failures == 0 {
        "succeeded"
    } else {
        "succeeded_with_failures"
    };
    complete_job_run(client, &summary, status).await?;

    Ok(summary)
}

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

/// Paged, skip-existing scan of legacy `video_index`. Returns up to `limit`
/// rows with `video_id > after` (or from the start when `after` is None) that
/// are NOT yet present in `all_servable_videos_on_yral`. Both columns are PKs,
/// so the anti-join + cursor range are index-driven. Mirrors the convention of
/// `media_index::videos_missing_canonical_phash`.
async fn fetch_missing_legacy_rows_after(
    client: &Client,
    after: Option<&str>,
    limit: i64,
) -> Result<Vec<LegacyVideoIndexRow>, tokio_postgres::Error> {
    let rows = client
        .query(
            "SELECT v.video_id, v.storj_key, v.hetzner_key, v.phash, v.phash_kind, v.phash_version
             FROM video_index v
             WHERE ($1::TEXT IS NULL OR v.video_id > $1)
               AND NOT EXISTS (
                 SELECT 1 FROM all_servable_videos_on_yral m WHERE m.video_id = v.video_id
               )
             ORDER BY v.video_id
             LIMIT $2",
            &[&after, &limit],
        )
        .await?;

    Ok(rows
        .into_iter()
        .map(|row| LegacyVideoIndexRow {
            video_id: row.get(0),
            storj_key: row.get(1),
            hetzner_key: row.get(2),
            phash: row.get(3),
            phash_kind: row.get(4),
            phash_version: row.get(5),
        })
        .collect())
}

async fn import_media_row_txn(
    tx: &Transaction<'_>,
    row: &LegacyVideoIndexRow,
    storage: StorageRef<'_>,
) -> Result<crate::media_index::ServableVideoUpsertOutcome, tokio_postgres::Error> {
    let outcome = crate::media_index::upsert_servable_video_txn(
        tx,
        crate::media_index::ServableVideoInput {
            video_id: &row.video_id,
            publisher_user_id: None,
            post_id: None,
            source_kind: SOURCE_KIND,
            source_ref: Some(&row.video_id),
            servable_status: "servable",
            nsfw_state: None,
            storage_provider: Some(storage.provider),
            bucket: storage.bucket,
            object_key: Some(storage.object_key),
            canonical_url: None,
            thumbnail_key: None,
            duration_ms: None,
            width: None,
            height: None,
            fps: None,
            container: None,
            video_codec: None,
            audio_codec: None,
            moov_atom_front: None,
            canonical_encoding_version: None,
            discovered_from: JOB_KIND,
        },
    )
    .await?;

    tx.execute(
        "UPDATE servable_video_sources
         SET raw_payload = $4
         WHERE video_id = $1
           AND source_kind = $2
           AND source_ref = $3",
        &[
            &row.video_id,
            &SOURCE_KIND,
            &row.video_id,
            &raw_payload(row),
        ],
    )
    .await?;

    Ok(outcome)
}

fn canonical_storage(row: &LegacyVideoIndexRow) -> Option<StorageRef<'_>> {
    if let Some(storj_key) = row.storj_key.as_deref() {
        return Some(StorageRef {
            provider: "storj",
            bucket: Some(crate::consts::STORJ_SFW_BUCKET.as_str()),
            object_key: storj_key,
        });
    }

    row.hetzner_key.as_deref().map(|hetzner_key| StorageRef {
        provider: "hetzner",
        bucket: None,
        object_key: hetzner_key,
    })
}

fn hash_provenance(row: &LegacyVideoIndexRow) -> Option<StorageRef<'_>> {
    if let Some(hetzner_key) = row.hetzner_key.as_deref() {
        return Some(StorageRef {
            provider: "hetzner",
            bucket: None,
            object_key: hetzner_key,
        });
    }

    // Legacy pHash normally came from Hetzner. Only Storj-only legacy rows fall
    // back to Storj provenance, because no Hetzner source is available.
    row.storj_key.as_deref().map(|storj_key| StorageRef {
        provider: "storj",
        bucket: Some(crate::consts::STORJ_SFW_BUCKET.as_str()),
        object_key: storj_key,
    })
}

fn raw_payload(row: &LegacyVideoIndexRow) -> Value {
    json!({
        "video_id": row.video_id,
        "storj_key": row.storj_key,
        "hetzner_key": row.hetzner_key,
        "phash": row.phash,
        "phash_kind": row.phash_kind,
        "phash_version": row.phash_version
    })
}

fn media_feed_payload(row: &LegacyVideoIndexRow, storage: StorageRef<'_>) -> Value {
    json!({
        "video_id": row.video_id,
        "source_kind": SOURCE_KIND,
        "source_ref": row.video_id,
        "servable_status": "servable",
        "storage_provider": storage.provider,
        "bucket": storage.bucket,
        "object_key": storage.object_key
    })
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

async fn complete_job_run(
    client: &Client,
    summary: &ImportSummary,
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

fn summary_totals(summary: &ImportSummary) -> Value {
    json!({
        "scanned_rows": summary.scanned_rows,
        "imported_media_rows": summary.imported_media_rows,
        "hash_rows_upserted": summary.hash_rows_upserted,
        "hash_feed_events_appended": summary.hash_feed_events_appended,
        "row_failures": summary.row_failures
    })
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::media_index::test_support::test_client;

    async fn init_test_schema(client: &tokio_postgres::Client) {
        crate::db::init_schema(client).await.unwrap();
        crate::media_index::init_schema(client).await.unwrap();
    }

    fn servable_input(video_id: &str) -> crate::media_index::ServableVideoInput<'_> {
        crate::media_index::ServableVideoInput {
            video_id,
            publisher_user_id: None,
            post_id: None,
            source_kind: "legacy_video_index",
            source_ref: Some(video_id),
            servable_status: "servable",
            nsfw_state: None,
            storage_provider: Some("storj"),
            bucket: None,
            object_key: Some(video_id),
            canonical_url: None,
            thumbnail_key: None,
            duration_ms: None,
            width: None,
            height: None,
            fps: None,
            container: None,
            video_codec: None,
            audio_codec: None,
            moov_atom_front: None,
            canonical_encoding_version: None,
            discovered_from: "test",
        }
    }

    #[tokio::test]
    async fn scan_returns_only_rows_missing_from_master() {
        let (_pg, mut client) = test_client().await;
        init_test_schema(&client).await;
        // three legacy rows
        for vid in ["v-a", "v-b", "v-c"] {
            client
                .execute(
                    "INSERT INTO video_index (video_id, storj_key) VALUES ($1, $2)",
                    &[&vid, &format!("creator/{vid}.mp4")],
                )
                .await
                .unwrap();
        }
        // v-b already in master
        let tx = client.transaction().await.unwrap();
        crate::media_index::upsert_servable_video_txn(&tx, servable_input("v-b"))
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let rows = super::fetch_missing_legacy_rows_after(&client, None, 100)
            .await
            .unwrap();
        let ids: Vec<_> = rows.iter().map(|r| r.video_id.as_str()).collect();
        assert_eq!(ids, vec!["v-a", "v-c"], "v-b is in master, must be skipped");
    }

    #[tokio::test]
    async fn scan_pages_by_video_id_cursor_and_respects_limit() {
        let (_pg, client) = test_client().await;
        init_test_schema(&client).await;
        for vid in ["v-a", "v-b", "v-c"] {
            client
                .execute(
                    "INSERT INTO video_index (video_id, storj_key) VALUES ($1, $2)",
                    &[&vid, &format!("creator/{vid}.mp4")],
                )
                .await
                .unwrap();
        }
        // limit 2, no cursor -> first two by video_id
        let page1 = super::fetch_missing_legacy_rows_after(&client, None, 2)
            .await
            .unwrap();
        assert_eq!(
            page1
                .iter()
                .map(|r| r.video_id.as_str())
                .collect::<Vec<_>>(),
            vec!["v-a", "v-b"]
        );
        // cursor past v-b -> only v-c
        let page2 = super::fetch_missing_legacy_rows_after(&client, Some("v-b"), 2)
            .await
            .unwrap();
        assert_eq!(
            page2
                .iter()
                .map(|r| r.video_id.as_str())
                .collect::<Vec<_>>(),
            vec!["v-c"]
        );
        // cursor past last -> empty
        let page3 = super::fetch_missing_legacy_rows_after(&client, Some("v-c"), 2)
            .await
            .unwrap();
        assert!(page3.is_empty());
    }

    #[tokio::test]
    async fn imports_legacy_video_index_rows_into_media_and_source_tables_preserving_raw_payload() {
        let (_pg, mut client) = test_client().await;
        init_test_schema(&client).await;

        client
            .execute(
                "INSERT INTO video_index (
                    video_id, storj_key, hetzner_key, phash, phash_kind, phash_version
                 )
                 VALUES
                    ('video-storj', 'creator/video-storj.mp4', 'legacy/video-storj.mp4',
                     'abc123', 'phash', 'legacy_hex_8x8_v0'),
                    ('video-hetzner', NULL, 'legacy/video-hetzner.mp4',
                     NULL, NULL, NULL)",
                &[],
            )
            .await
            .unwrap();

        let summary = super::import_current_video_index(
            &mut client,
            "test-runner",
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(summary.scanned_rows, 2);
        assert_eq!(summary.imported_media_rows, 2);
        assert_eq!(summary.row_failures, 0);

        let media_rows = client
            .query(
                "SELECT video_id,
                        storage_provider,
                        bucket,
                        object_key,
                        source_kind,
                        source_ref,
                        servable_status,
                        discovered_from
                 FROM all_servable_videos_on_yral
                 ORDER BY video_id",
                &[],
            )
            .await
            .unwrap();

        assert_eq!(media_rows.len(), 2);
        assert_eq!(media_rows[0].get::<_, String>(0), "video-hetzner");
        assert_eq!(media_rows[0].get::<_, String>(1), "hetzner");
        assert_eq!(media_rows[0].get::<_, Option<String>>(2), None);
        assert_eq!(
            media_rows[0].get::<_, String>(3),
            "legacy/video-hetzner.mp4"
        );
        assert_eq!(media_rows[0].get::<_, String>(4), SOURCE_KIND);
        assert_eq!(media_rows[0].get::<_, String>(5), "video-hetzner");
        assert_eq!(media_rows[0].get::<_, String>(6), "servable");
        assert_eq!(media_rows[0].get::<_, String>(7), JOB_KIND);

        assert_eq!(media_rows[1].get::<_, String>(0), "video-storj");
        assert_eq!(media_rows[1].get::<_, String>(1), "storj");
        assert_eq!(
            media_rows[1].get::<_, String>(2),
            crate::consts::STORJ_SFW_BUCKET.as_str()
        );
        assert_eq!(media_rows[1].get::<_, String>(3), "creator/video-storj.mp4");

        let source_count: i64 = client
            .query_one("SELECT COUNT(*) FROM servable_video_sources", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(source_count, 2);

        let raw_payload: Value = client
            .query_one(
                "SELECT raw_payload
                 FROM servable_video_sources
                 WHERE video_id = 'video-storj'
                   AND source_kind = 'legacy_video_index'
                   AND source_ref = 'video-storj'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(
            raw_payload,
            json!({
                "video_id": "video-storj",
                "storj_key": "creator/video-storj.mp4",
                "hetzner_key": "legacy/video-storj.mp4",
                "phash": "abc123",
                "phash_kind": "phash",
                "phash_version": "legacy_hex_8x8_v0"
            })
        );

        let feed_events = crate::media_index::list_feed_events_after(&client, 0, 10)
            .await
            .unwrap();
        let visibility_events = feed_events
            .iter()
            .filter(|event| {
                event.event_kind == crate::media_index::FeedEventKind::MediaVisibilityChanged
            })
            .count();
        assert_eq!(visibility_events, 2);
    }

    #[tokio::test]
    async fn imports_versioned_mirror_hash_rows_and_emits_exactly_one_hash_upserted_feed_event() {
        let (_pg, mut client) = test_client().await;
        init_test_schema(&client).await;

        client
            .execute(
                "INSERT INTO video_index (
                    video_id, storj_key, hetzner_key, phash, phash_kind, phash_version
                 )
                 VALUES (
                    'video-hash',
                    'creator/video-hash.mp4',
                    'legacy/video-hash.mp4',
                    '01001100',
                    'phash',
                    'legacy_hex_8x8_v0'
                 )",
                &[],
            )
            .await
            .unwrap();

        let summary = super::import_current_video_index(
            &mut client,
            "test-runner",
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(summary.hash_rows_upserted, 1);
        assert_eq!(summary.hash_feed_events_appended, 1);

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
                 WHERE video_id = 'video-hash'",
                &[],
            )
            .await
            .unwrap();

        assert_eq!(hash_row.get::<_, String>(0), "phash");
        assert_eq!(hash_row.get::<_, String>(1), "legacy_hex_8x8_v0");
        assert_eq!(hash_row.get::<_, String>(2), INPUT_MEDIA_VERSION);
        assert_eq!(hash_row.get::<_, String>(3), "01001100");
        assert_eq!(hash_row.get::<_, i32>(4), 8);
        assert_eq!(hash_row.get::<_, i32>(5), 0);
        assert_eq!(hash_row.get::<_, i32>(6), 0);
        assert_eq!(hash_row.get::<_, String>(7), "hetzner");
        assert_eq!(hash_row.get::<_, Option<String>>(8), None);
        assert_eq!(hash_row.get::<_, String>(9), "legacy/video-hash.mp4");
        assert_eq!(hash_row.get::<_, Value>(10), json!({"source": SOURCE_KIND}));

        let events = crate::media_index::list_feed_events_after(&client, 0, 10)
            .await
            .unwrap();
        let hash_events: Vec<_> = events
            .iter()
            .filter(|event| event.event_kind == crate::media_index::FeedEventKind::HashUpserted)
            .collect();
        assert_eq!(hash_events.len(), 1);
        let event = hash_events[0];
        assert_eq!(
            event.event_kind,
            crate::media_index::FeedEventKind::HashUpserted
        );
        assert_eq!(event.video_id, "video-hash");
        assert_eq!(event.hash_kind.as_deref(), Some("phash"));
        assert_eq!(event.hash_version.as_deref(), Some("legacy_hex_8x8_v0"));
        assert_eq!(
            event.input_media_version.as_deref(),
            Some(INPUT_MEDIA_VERSION)
        );
    }

    #[tokio::test]
    async fn second_run_is_idempotent_without_duplicate_source_rows_or_unchanged_hash_feed_events()
    {
        let (_pg, mut client) = test_client().await;
        init_test_schema(&client).await;

        client
            .execute(
                "INSERT INTO video_index (
                    video_id, storj_key, hetzner_key, phash, phash_kind, phash_version
                 )
                 VALUES (
                    'video-repeat',
                    'creator/video-repeat.mp4',
                    'legacy/video-repeat.mp4',
                    '11110000',
                    'phash',
                    'legacy_hex_8x8_v0'
                 )",
                &[],
            )
            .await
            .unwrap();

        let first = super::import_current_video_index(
            &mut client,
            "test-runner",
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let second = super::import_current_video_index(
            &mut client,
            "test-runner",
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(first.hash_feed_events_appended, 1);
        assert_eq!(second.hash_feed_events_appended, 0);

        let source_count: i64 = client
            .query_one(
                "SELECT COUNT(*)
                 FROM servable_video_sources
                 WHERE video_id = 'video-repeat'
                   AND source_kind = 'legacy_video_index'
                   AND source_ref = 'video-repeat'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(source_count, 1);

        let events = crate::media_index::list_feed_events_after(&client, 0, 10)
            .await
            .unwrap();
        let hash_events = events
            .iter()
            .filter(|event| event.event_kind == crate::media_index::FeedEventKind::HashUpserted)
            .count();
        let visibility_events = events
            .iter()
            .filter(|event| {
                event.event_kind == crate::media_index::FeedEventKind::MediaVisibilityChanged
            })
            .count();
        assert_eq!(hash_events, 1);
        assert_eq!(visibility_events, 1);
    }

    #[tokio::test]
    async fn rows_with_no_storage_key_are_recorded_as_failures_and_job_succeeds_with_failures() {
        let (_pg, mut client) = test_client().await;
        init_test_schema(&client).await;

        client
            .execute(
                "INSERT INTO video_index (
                    video_id, storj_key, hetzner_key, phash, phash_kind, phash_version
                 )
                 VALUES (
                    'video-missing-storage',
                    NULL,
                    NULL,
                    '11110000',
                    'phash',
                    'legacy_hex_8x8_v0'
                 )",
                &[],
            )
            .await
            .unwrap();

        let summary = super::import_current_video_index(
            &mut client,
            "test-runner",
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(summary.scanned_rows, 1);
        assert_eq!(summary.imported_media_rows, 0);
        assert_eq!(summary.row_failures, 1);

        let run_row = client
            .query_one(
                "SELECT status, requested_by, totals
                 FROM media_job_runs
                 WHERE id = $1::TEXT::UUID",
                &[&summary.job_run_id.to_string()],
            )
            .await
            .unwrap();
        assert_eq!(run_row.get::<_, String>(0), "succeeded_with_failures");
        assert_eq!(run_row.get::<_, String>(1), "test-runner");
        assert_eq!(run_row.get::<_, Value>(2), summary_totals(&summary));

        let media_count: i64 = client
            .query_one(
                "SELECT COUNT(*)
                 FROM all_servable_videos_on_yral
                 WHERE video_id = 'video-missing-storage'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(media_count, 0);

        let failure_row = client
            .query_one(
                "SELECT job_run_id::text,
                        job_kind,
                        item_key,
                        video_id,
                        phase,
                        source_ref,
                        last_error,
                        status
                 FROM media_job_failures
                 WHERE video_id = 'video-missing-storage'",
                &[],
            )
            .await
            .unwrap();

        assert_eq!(
            failure_row.get::<_, String>(0),
            summary.job_run_id.to_string()
        );
        assert_eq!(failure_row.get::<_, String>(1), JOB_KIND);
        assert_eq!(failure_row.get::<_, String>(2), "video-missing-storage");
        assert_eq!(failure_row.get::<_, String>(3), "video-missing-storage");
        assert_eq!(failure_row.get::<_, String>(4), "storage_selection");
        assert_eq!(failure_row.get::<_, String>(5), "video-missing-storage");
        assert_eq!(
            failure_row.get::<_, String>(6),
            "legacy video_index row has neither storj_key nor hetzner_key"
        );
        assert_eq!(failure_row.get::<_, String>(7), "pending_retry");
    }

    #[tokio::test]
    async fn unexpected_import_errors_mark_the_job_run_failed() {
        let (_pg, mut client) = test_client().await;
        init_test_schema(&client).await;

        // Seed MAX_CONSECUTIVE_IMPORT_FAILURES + 10 (= 60) rows so the circuit breaker trips.
        // With one row the per-row fallback would isolate the failure as succeeded_with_failures;
        // many consecutive failures (all hitting the missing table) must fail the whole job.
        for i in 0..(super::MAX_CONSECUTIVE_IMPORT_FAILURES + 10) {
            client
                .execute(
                    "INSERT INTO video_index (video_id, storj_key) VALUES ($1, $2)",
                    &[
                        &format!("video-error-{i:03}"),
                        &format!("creator/video-error-{i:03}.mp4"),
                    ],
                )
                .await
                .unwrap();
        }
        client
            .execute("DROP TABLE media_feed_events", &[])
            .await
            .unwrap();

        let cancel = CancellationToken::new();
        let err = super::import_current_video_index(&mut client, "test-runner", None, &cancel)
            .await
            .unwrap_err();
        // With the dropped table every row's fallback also fails → consecutive counter hits
        // MAX_CONSECUTIVE_IMPORT_FAILURES → TooManyConsecutiveFailures returned.
        assert!(
            matches!(err, ImportError::TooManyConsecutiveFailures { .. }),
            "expected TooManyConsecutiveFailures, got {err:?}"
        );

        let status: String = client
            .query_one(
                "SELECT status FROM media_job_runs ORDER BY started_at DESC LIMIT 1",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(status, "failed");
    }

    #[tokio::test]
    async fn keyless_row_in_a_batch_is_recorded_and_rest_of_batch_commits() {
        let (_pg, mut client) = test_client().await;
        init_test_schema(&client).await;
        // one good, one keyless (no storj/hetzner), one good
        client
            .execute(
                "INSERT INTO video_index (video_id, storj_key) VALUES ('g1','creator/g1.mp4')",
                &[],
            )
            .await
            .unwrap();
        client
            .execute("INSERT INTO video_index (video_id) VALUES ('bad1')", &[])
            .await
            .unwrap();
        client
            .execute(
                "INSERT INTO video_index (video_id, storj_key) VALUES ('g2','creator/g2.mp4')",
                &[],
            )
            .await
            .unwrap();

        let cancel = tokio_util::sync::CancellationToken::new();
        let summary = super::import_current_video_index(&mut client, "t", None, &cancel)
            .await
            .unwrap();

        assert_eq!(summary.scanned_rows, 3);
        assert_eq!(summary.imported_media_rows, 2);
        assert_eq!(summary.row_failures, 1);
        // good rows in master
        let n: i64 = client
            .query_one("SELECT count(*) FROM all_servable_videos_on_yral", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(n, 2);
        // failure recorded
        let f: i64 = client
            .query_one(
                "SELECT count(*) FROM media_job_failures WHERE video_id='bad1'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(f, 1);
    }

    #[tokio::test]
    async fn import_makes_progress_over_keyless_rows_and_terminates() {
        use std::sync::atomic::Ordering;
        // Force tiny batches so multiple pages of keyless rows are exercised.
        let prev_batch = crate::consts::MEDIA_IMPORT_BATCH_SIZE.swap(2, Ordering::Relaxed);
        let (_pg, mut client) = test_client().await;
        init_test_schema(&client).await;
        for vid in ["k1", "k2", "k3", "k4", "k5"] {
            client
                .execute("INSERT INTO video_index (video_id) VALUES ($1)", &[&vid])
                .await
                .unwrap();
        }
        let cancel = tokio_util::sync::CancellationToken::new();
        let summary = super::import_current_video_index(&mut client, "t", None, &cancel)
            .await
            .unwrap();
        assert_eq!(summary.scanned_rows, 5);
        assert_eq!(summary.row_failures, 5);
        assert_eq!(summary.imported_media_rows, 0);
        let status: String = client
            .query_one(
                "SELECT status FROM media_job_runs WHERE id=$1::TEXT::UUID",
                &[&summary.job_run_id.to_string()],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(status, "succeeded_with_failures");
        crate::consts::MEDIA_IMPORT_BATCH_SIZE.store(prev_batch, Ordering::Relaxed);
    }

    #[tokio::test]
    async fn second_run_imports_only_missing_rows() {
        let (_pg, mut client) = test_client().await;
        init_test_schema(&client).await;
        client
            .execute(
                "INSERT INTO video_index (video_id, storj_key) VALUES ('r1','creator/r1.mp4')",
                &[],
            )
            .await
            .unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let _first = super::import_current_video_index(&mut client, "t", None, &cancel)
            .await
            .unwrap();
        let events_after_first: i64 = client
            .query_one("SELECT count(*) FROM media_feed_events", &[])
            .await
            .unwrap()
            .get(0);
        // add a new legacy row, re-run
        client
            .execute(
                "INSERT INTO video_index (video_id, storj_key) VALUES ('r2','creator/r2.mp4')",
                &[],
            )
            .await
            .unwrap();
        let summary = super::import_current_video_index(&mut client, "t", None, &cancel)
            .await
            .unwrap();
        assert_eq!(summary.scanned_rows, 1, "only the new row r2 is scanned");
        assert_eq!(summary.imported_media_rows, 1);
        let events_after_second: i64 = client
            .query_one("SELECT count(*) FROM media_feed_events", &[])
            .await
            .unwrap()
            .get(0);
        assert!(
            events_after_second > events_after_first,
            "only r2's events added; r1 not re-emitted"
        );
    }

    #[tokio::test]
    async fn import_honors_cancellation_and_finalizes_cancelled() {
        let (_pg, mut client) = test_client().await;
        init_test_schema(&client).await;
        client
            .execute(
                "INSERT INTO video_index (video_id, storj_key, hetzner_key, phash, phash_kind, phash_version)
                 VALUES ('vid-cancel', 'creator/vid-cancel.mp4', 'legacy/vid-cancel.mp4', 'abc', 'phash', 'legacy_hex_8x8_v0')",
                &[],
            )
            .await
            .unwrap();

        let cancel = CancellationToken::new();
        cancel.cancel(); // pre-cancelled

        let summary = super::import_current_video_index(&mut client, "test-runner", None, &cancel)
            .await
            .unwrap();

        assert_eq!(summary.scanned_rows, 0);

        let status: String = client
            .query_one(
                "SELECT status FROM media_job_runs WHERE id = $1::TEXT::UUID",
                &[&summary.job_run_id.to_string()],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(status, "cancelled");
    }
}
