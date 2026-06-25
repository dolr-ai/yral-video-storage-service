use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;
use utoipa::{IntoParams, ToSchema};

use crate::db;
use crate::jobs::JobGuard;
use crate::media_index;
use crate::AppState;

// ─── Request / response types ────────────────────────────────────────────────

#[derive(Deserialize, IntoParams)]
pub struct ImportParams {
    /// Optional label for who triggered the import (default: "media_import_api")
    pub requested_by: Option<String>,
    /// Optional row limit passed to the import job
    pub limit: Option<i64>,
}

#[derive(Serialize, ToSchema)]
pub struct CoverageStatsResponse {
    pub total_servable: i64,
    pub with_canonical_phash: i64,
    pub missing_canonical_phash: i64,
}

#[derive(Deserialize, IntoParams)]
pub struct FeedParams {
    /// Return only events with cursor > after (default: 0)
    pub after: Option<i64>,
    /// Number of events to return (default: 100, max: 500)
    pub limit: Option<i64>,
}

#[derive(Serialize, ToSchema)]
pub struct FeedEvent {
    pub cursor: i64,
    pub event_kind: String,
    pub video_id: String,
    pub hash_kind: Option<String>,
    pub hash_version: Option<String>,
    pub input_media_version: Option<String>,
    pub payload: serde_json::Value,
    /// ISO-8601 UTC timestamp
    pub created_at: String,
}

#[derive(Serialize, ToSchema)]
pub struct FeedResponse {
    pub events: Vec<FeedEvent>,
}

// ─── Media job response types ─────────────────────────────────────────────────

#[derive(Serialize, ToSchema)]
pub struct MediaJobsStatus {
    pub import_running: bool,
    pub phash_running: bool,
}

#[derive(Serialize, ToSchema)]
pub struct MediaCancelResponse {
    pub message: String,
    pub cancelled: Vec<String>,
}

/// Pure, unit-testable status body. Reads both flags atomically and returns the
/// current snapshot. Decomposed from the handler so it can be tested without an
/// HTTP server (mirrors the `run_status`/`persist_one` decomposition pattern).
pub fn media_jobs_status_body(
    import: &std::sync::atomic::AtomicBool,
    phash: &std::sync::atomic::AtomicBool,
) -> MediaJobsStatus {
    MediaJobsStatus {
        import_running: import.load(Ordering::Acquire),
        phash_running: phash.load(Ordering::Acquire),
    }
}

/// Query params for the pHash run handler.
#[derive(Deserialize, IntoParams)]
pub struct PhashParams {
    /// Optional row limit passed to the pHash job
    pub limit: Option<i64>,
    /// Optional label for who triggered the run (default: "media_phash_api")
    pub requested_by: Option<String>,
}

// ─── Handlers ────────────────────────────────────────────────────────────────

/// Start the legacy video-index import job in the background.
///
/// Returns 202 Accepted immediately; the job runs asynchronously.
/// Returns 409 Conflict if the job is already running.
#[utoipa::path(
    post,
    path = "/media/import/video-index",
    tag = "media",
    params(ImportParams),
    responses(
        (status = 202, description = "Import job started"),
        (status = 409, description = "Import job already running"),
        (status = 401, description = "Unauthorized"),
    )
)]
pub async fn import_video_index(
    State(state): State<AppState>,
    Query(params): Query<ImportParams>,
) -> StatusCode {
    if state
        .job_media_import_running
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return StatusCode::CONFLICT;
    }

    let db_url = state.db_url.clone();
    // Clamp the caller-supplied label so an arbitrarily large string can't be
    // persisted to media_job_runs.requested_by.
    let requested_by = params
        .requested_by
        .map(|s| s.chars().take(256).collect::<String>())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "media_import_api".to_string());
    let limit = params.limit;
    let guard = JobGuard(state.job_media_import_running.clone());
    let cancel = state
        .media_job_cancel
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();

    tokio::spawn(async move {
        let _guard = guard;
        let mut client = match db::connect(&db_url).await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "media_import: DB connect failed");
                sentry::capture_message(
                    &format!("media_import: DB connect failed: {e}"),
                    sentry::Level::Error,
                );
                return;
            }
        };
        match crate::jobs::media_imports::import_current_video_index(
            &mut client,
            &requested_by,
            limit,
            &cancel,
        )
        .await
        {
            Ok(summary) => {
                tracing::info!(
                    job_run_id = %summary.job_run_id,
                    scanned_rows = summary.scanned_rows,
                    imported_media_rows = summary.imported_media_rows,
                    hash_rows_upserted = summary.hash_rows_upserted,
                    hash_feed_events_appended = summary.hash_feed_events_appended,
                    row_failures = summary.row_failures,
                    "media_import: completed"
                );
            }
            Err(e) => {
                tracing::error!(error = %e, "media_import: job failed");
                sentry::capture_message(
                    &format!("media_import job failed: {e}"),
                    sentry::Level::Error,
                );
            }
        }
    });

    StatusCode::ACCEPTED
}

/// Return deterministic pHash coverage stats for the media master table.
///
/// Reports how many rows in `all_servable_videos_on_yral` have (or are
/// missing) the canonical hash tuple
/// `(phash, offchain_binary_10x8_v1, current_stored_object_v1)`.
#[utoipa::path(
    get,
    path = "/media/audit/missing-phash",
    tag = "media",
    responses(
        (status = 200, description = "Coverage stats", body = CoverageStatsResponse),
        (status = 401, description = "Unauthorized"),
        (status = 500, description = "Internal server error"),
    )
)]
pub async fn missing_phash_audit(
    State(state): State<AppState>,
) -> Result<Json<CoverageStatsResponse>, StatusCode> {
    let client = db::connect(&state.db_url)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let stats = media_index::canonical_phash_coverage(&client)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(CoverageStatsResponse {
        total_servable: stats.total_servable,
        with_canonical_phash: stats.with_canonical_phash,
        missing_canonical_phash: stats.missing_canonical_phash,
    }))
}

/// Page the media outbox feed.
///
/// Returns events ordered by cursor (ascending) with `cursor > after`.
/// Defaults: `after=0`, `limit=100`.
#[utoipa::path(
    get,
    path = "/media/feed/events",
    tag = "media",
    params(FeedParams),
    responses(
        (status = 200, description = "Paginated feed events", body = FeedResponse),
        (status = 400, description = "Bad request (e.g. limit out of range)"),
        (status = 401, description = "Unauthorized"),
        (status = 500, description = "Internal server error"),
    )
)]
pub async fn feed_events(
    State(state): State<AppState>,
    Query(params): Query<FeedParams>,
) -> Result<Json<FeedResponse>, StatusCode> {
    let after = params.after.unwrap_or(0);
    let limit = params.limit.unwrap_or(100);

    let client = db::connect(&state.db_url)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let events = media_index::list_feed_events_after(&client, after, limit)
        .await
        .map_err(|e| match e {
            media_index::FeedReadError::InvalidLimit { .. } => StatusCode::BAD_REQUEST,
            media_index::FeedReadError::Postgres(_)
            | media_index::FeedReadError::UnknownEventKind(_) => StatusCode::INTERNAL_SERVER_ERROR,
        })?;

    Ok(Json(FeedResponse {
        events: events
            .into_iter()
            .map(|e| FeedEvent {
                cursor: e.cursor,
                event_kind: e.event_kind.as_str().to_string(),
                video_id: e.video_id,
                hash_kind: e.hash_kind,
                hash_version: e.hash_version,
                input_media_version: e.input_media_version,
                payload: e.payload,
                created_at: e.created_at.to_rfc3339(),
            })
            .collect(),
    }))
}

/// Start the media pHash compute job in the background.
///
/// Returns 202 Accepted immediately; the job runs asynchronously.
/// Returns 409 Conflict if the job is already running.
#[utoipa::path(
    post,
    path = "/media/phash/run",
    tag = "media",
    params(PhashParams),
    responses(
        (status = 202, description = "pHash job started"),
        (status = 409, description = "pHash job already running"),
        (status = 401, description = "Unauthorized"),
    )
)]
pub async fn run_phash(
    State(state): State<AppState>,
    Query(params): Query<PhashParams>,
) -> StatusCode {
    if state
        .job_media_phash_running
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return StatusCode::CONFLICT;
    }

    let s3 = state.s3_client.clone();
    let storj = state.storj_client.clone();
    let db_url = state.db_url.clone();
    let limit = params.limit;
    let requested_by = params
        .requested_by
        .map(|s| s.chars().take(256).collect::<String>())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "media_phash_api".to_string());
    let guard = JobGuard(state.job_media_phash_running.clone());
    let cancel = state
        .media_job_cancel
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();

    tokio::spawn(async move {
        let _guard = guard;
        if let Err(e) =
            crate::jobs::media_phash::run(s3, storj, db_url, cancel, limit, &requested_by).await
        {
            tracing::error!(error = %e, "media_phash: job failed");
            sentry::capture_message(
                &format!("media_phash job failed: {e}"),
                sentry::Level::Error,
            );
        }
    });

    StatusCode::ACCEPTED
}

/// Cancel all running media jobs (import and pHash).
///
/// The cancellation token is replaced so subsequently started jobs get a fresh
/// token. Both media_import and media_phash listen on this shared token.
#[utoipa::path(
    post,
    path = "/media/jobs/cancel",
    tag = "media",
    responses(
        (status = 200, description = "Cancellation signal sent", body = MediaCancelResponse),
        (status = 401, description = "Unauthorized"),
    )
)]
pub async fn cancel_media_jobs(State(state): State<AppState>) -> Json<MediaCancelResponse> {
    use tokio_util::sync::CancellationToken;

    // Swap in a fresh token and cancel the old one OUTSIDE the lock to avoid
    // holding the lock while cancellation propagates (mirrors mirror::cancel_all).
    let old_token = {
        let mut token = state
            .media_job_cancel
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let old = token.clone();
        *token = CancellationToken::new();
        old
    };
    old_token.cancel();

    Json(MediaCancelResponse {
        message: "media jobs cancellation requested".into(),
        cancelled: vec!["media_import".into(), "media_phash".into()],
    })
}

/// Report the current running status of all media jobs.
#[utoipa::path(
    get,
    path = "/media/jobs/status",
    tag = "media",
    responses(
        (status = 200, description = "Current media job running status", body = MediaJobsStatus),
        (status = 401, description = "Unauthorized"),
    )
)]
pub async fn media_jobs_status(State(state): State<AppState>) -> Json<MediaJobsStatus> {
    Json(media_jobs_status_body(
        &state.job_media_import_running,
        &state.job_media_phash_running,
    ))
}

// ─── Job run + failure response types ────────────────────────────────────────

#[derive(Serialize, ToSchema)]
pub struct JobRunView {
    pub job_kind: String,
    pub status: String,
    pub requested_by: String,
    /// RFC3339 UTC timestamp
    pub started_at: String,
    /// RFC3339 UTC timestamp, absent while the job is still running
    pub finished_at: Option<String>,
    pub totals: Option<serde_json::Value>,
    pub error_message: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct JobRunsResponse {
    pub runs: Vec<JobRunView>,
}

#[derive(Serialize, ToSchema)]
pub struct FailureGroupView {
    pub phase: String,
    pub count: i64,
    pub samples: Vec<String>,
}

#[derive(Serialize, ToSchema)]
pub struct FailuresResponse {
    pub failures: Vec<FailureGroupView>,
}

#[derive(Deserialize, IntoParams)]
pub struct JobRunsQuery {
    /// Filter by job kind (e.g. "media_phash")
    pub job_kind: Option<String>,
    /// Number of runs to return (default: 20, max: 100)
    pub limit: Option<i64>,
}

#[derive(Deserialize, IntoParams)]
pub struct FailuresQuery {
    /// Filter by job kind (e.g. "media_phash")
    pub job_kind: Option<String>,
    /// Number of failure groups to return (default: 20, max: 100)
    pub limit: Option<i64>,
}

/// Map a `JobRunRow` to its HTTP view, converting timestamps to RFC3339 strings.
pub fn job_run_view(r: crate::media_index::JobRunRow) -> JobRunView {
    JobRunView {
        job_kind: r.job_kind,
        status: r.status,
        requested_by: r.requested_by,
        started_at: r.started_at.to_rfc3339(),
        finished_at: r.finished_at.map(|t| t.to_rfc3339()),
        totals: r.totals,
        error_message: r.error_message,
    }
}

/// List recent media job runs, newest first.
///
/// Optionally filter by `job_kind`. Returns at most `limit` rows (default 20, max 100).
#[utoipa::path(
    get,
    path = "/media/jobs/runs",
    tag = "media",
    params(JobRunsQuery),
    responses(
        (status = 200, description = "Recent job runs", body = JobRunsResponse),
        (status = 401, description = "Unauthorized"),
        (status = 500, description = "Internal server error"),
    )
)]
pub async fn media_jobs_runs(
    State(state): State<AppState>,
    Query(params): Query<JobRunsQuery>,
) -> Result<Json<JobRunsResponse>, StatusCode> {
    let limit = params.limit.unwrap_or(20).clamp(1, 100);

    let client = db::connect(&state.db_url)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let rows = media_index::recent_job_runs(&client, params.job_kind.as_deref(), limit)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let runs = rows.into_iter().map(job_run_view).collect();
    Ok(Json(JobRunsResponse { runs }))
}

/// Summarise media job failures grouped by phase, with sample error messages.
///
/// Optionally filter by `job_kind`. Returns at most `limit` groups (default 20, max 100).
#[utoipa::path(
    get,
    path = "/media/jobs/failures",
    tag = "media",
    params(FailuresQuery),
    responses(
        (status = 200, description = "Failure groups", body = FailuresResponse),
        (status = 401, description = "Unauthorized"),
        (status = 500, description = "Internal server error"),
    )
)]
pub async fn media_jobs_failures(
    State(state): State<AppState>,
    Query(params): Query<FailuresQuery>,
) -> Result<Json<FailuresResponse>, StatusCode> {
    let limit = params.limit.unwrap_or(20).clamp(1, 100);

    let client = db::connect(&state.db_url)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let groups = media_index::failure_summary(&client, params.job_kind.as_deref(), limit)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let failures = groups
        .into_iter()
        .map(|g| FailureGroupView {
            phase: g.phase,
            count: g.count,
            samples: g.samples,
        })
        .collect();
    Ok(Json(FailuresResponse { failures }))
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::media_index::test_support::test_client;

    /// Seed N servable rows in `all_servable_videos_on_yral`, give M of them
    /// the canonical pHash tuple, and assert coverage counts.
    #[tokio::test]
    async fn audit_coverage_counts_are_deterministic() {
        let (_pg, mut client) = test_client().await;
        crate::db::init_schema(&client).await.unwrap();
        crate::media_index::init_schema(&client).await.unwrap();

        // Insert 5 servable rows
        for i in 0..5u32 {
            let vid = format!("video-{i:03}");
            let tx = client.transaction().await.unwrap();
            crate::media_index::upsert_servable_video_txn(
                &tx,
                crate::media_index::ServableVideoInput {
                    video_id: &vid,
                    publisher_user_id: None,
                    post_id: None,
                    source_kind: "legacy_video_index",
                    source_ref: Some(&vid),
                    servable_status: "servable",
                    nsfw_state: None,
                    storage_provider: Some("hetzner"),
                    bucket: None,
                    object_key: Some(&vid),
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
                },
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
        }

        // Give exactly 2 rows the canonical pHash tuple
        for i in 0..2u32 {
            let vid = format!("video-{i:03}");
            let tx = client.transaction().await.unwrap();
            crate::media_index::upsert_hash_record_txn(
                &tx,
                crate::media_index::HashRecordInput {
                    video_id: &vid,
                    hash_kind: "phash",
                    hash_version: "offchain_binary_10x8_v1",
                    input_media_version: "current_stored_object_v1",
                    hash_value: "01010101",
                    hash_bit_length: 8,
                    num_frames: 1,
                    hash_size: 8,
                    computed_from_provider: Some("hetzner"),
                    computed_from_bucket: None,
                    computed_from_key: Some(&vid),
                    metadata: None,
                },
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
        }

        let stats = crate::media_index::canonical_phash_coverage(&client)
            .await
            .unwrap();

        assert_eq!(stats.total_servable, 5);
        assert_eq!(stats.with_canonical_phash, 2);
        assert_eq!(stats.missing_canonical_phash, 3);
    }

    /// Insert several feed events, page with `after=<mid-cursor>`, and assert
    /// only events with higher cursors are returned in order.
    #[tokio::test]
    async fn feed_events_pages_by_cursor_gt_after() {
        let (_pg, mut client) = test_client().await;
        crate::db::init_schema(&client).await.unwrap();
        crate::media_index::init_schema(&client).await.unwrap();

        // Insert 4 videos and append one feed event per video
        let mut cursors = Vec::new();
        for i in 0..4u32 {
            let vid = format!("feed-video-{i:03}");
            let tx = client.transaction().await.unwrap();
            crate::media_index::upsert_servable_video_txn(
                &tx,
                crate::media_index::ServableVideoInput {
                    video_id: &vid,
                    publisher_user_id: None,
                    post_id: None,
                    source_kind: "legacy_video_index",
                    source_ref: Some(&vid),
                    servable_status: "servable",
                    nsfw_state: None,
                    storage_provider: Some("hetzner"),
                    bucket: None,
                    object_key: Some(&vid),
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
                },
            )
            .await
            .unwrap();
            let cursor = crate::media_index::append_feed_event_txn(
                &tx,
                crate::media_index::FeedEventInput {
                    event_kind: crate::media_index::FeedEventKind::HashUpserted,
                    video_id: &vid,
                    hash_kind: Some("phash"),
                    hash_version: Some("offchain_binary_10x8_v1"),
                    input_media_version: Some("current_stored_object_v1"),
                    payload: json!({"video_id": vid}),
                },
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
            cursors.push(cursor);
        }

        // Fetch events after the second cursor (should return events 3 and 4)
        let split_after = cursors[1];
        let events = crate::media_index::list_feed_events_after(&client, split_after, 100)
            .await
            .unwrap();

        assert_eq!(events.len(), 2);
        assert!(
            events.iter().all(|e| e.cursor > split_after),
            "all returned cursors must be > after"
        );
        // Events must be in ascending cursor order
        assert!(events[0].cursor < events[1].cursor);
        assert_eq!(events[0].cursor, cursors[2]);
        assert_eq!(events[1].cursor, cursors[3]);
    }

    /// Feed read must NOT join `servable_video_sources` at read time —
    /// deleting the sources table must not break feed reads.
    #[tokio::test]
    async fn feed_read_does_not_join_source_tables() {
        let (_pg, mut client) = test_client().await;
        crate::db::init_schema(&client).await.unwrap();
        crate::media_index::init_schema(&client).await.unwrap();

        let vid = "denorm-video-001";
        let tx = client.transaction().await.unwrap();
        crate::media_index::upsert_servable_video_txn(
            &tx,
            crate::media_index::ServableVideoInput {
                video_id: vid,
                publisher_user_id: None,
                post_id: None,
                source_kind: "legacy_video_index",
                source_ref: Some(vid),
                servable_status: "servable",
                nsfw_state: None,
                storage_provider: Some("hetzner"),
                bucket: None,
                object_key: Some("hetzner/denorm-video-001.mp4"),
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
            },
        )
        .await
        .unwrap();
        let cursor = crate::media_index::append_feed_event_txn(
            &tx,
            crate::media_index::FeedEventInput {
                event_kind: crate::media_index::FeedEventKind::HashUpserted,
                video_id: vid,
                hash_kind: Some("phash"),
                hash_version: Some("offchain_binary_10x8_v1"),
                input_media_version: Some("current_stored_object_v1"),
                payload: json!({
                    "video_id": vid,
                    "object_key": "hetzner/denorm-video-001.mp4",
                    "hash_value": "deadbeef"
                }),
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();

        // Drop source table — feed must still work because events are denormalized
        client
            .execute("DROP TABLE servable_video_sources", &[])
            .await
            .unwrap();

        let events = crate::media_index::list_feed_events_after(&client, 0, 10)
            .await
            .unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].cursor, cursor);
        assert_eq!(events[0].video_id, vid);
        assert_eq!(
            events[0].payload["object_key"],
            "hetzner/denorm-video-001.mp4"
        );
    }

    /// `media_jobs_status_body` returns the correct snapshot of both running flags.
    #[test]
    fn media_status_reports_running_flags() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let import = Arc::new(AtomicBool::new(false));
        let phash = Arc::new(AtomicBool::new(true));
        let body = super::media_jobs_status_body(&import, &phash);
        assert!(!body.import_running);
        assert!(body.phash_running);
        phash.store(false, Ordering::Release);
        let body = super::media_jobs_status_body(&import, &phash);
        assert!(!body.phash_running);
    }

    /// `job_run_view` maps `JobRunRow` fields to `JobRunView` with RFC3339 timestamps.
    #[test]
    fn job_run_view_serializes_timestamps_rfc3339() {
        let row = crate::media_index::JobRunRow {
            job_kind: "media_phash".into(),
            status: "running".into(),
            requested_by: "t".into(),
            started_at: chrono::Utc::now(),
            finished_at: None,
            totals: Some(serde_json::json!({"scanned_rows": 9})),
            error_message: None,
        };
        let v = super::job_run_view(row);
        assert_eq!(v.status, "running");
        assert!(v.started_at.contains('T')); // rfc3339
        assert!(v.finished_at.is_none());
    }

    /// The import running flag prevents two concurrent imports and returns 409.
    /// We test the flag logic directly (no full HTTP server needed).
    #[test]
    fn import_running_flag_prevents_concurrent_invocations() {
        use std::sync::atomic::Ordering;
        use std::sync::{atomic::AtomicBool, Arc};

        let flag = Arc::new(AtomicBool::new(false));

        // First acquire succeeds
        assert!(
            flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "first CAS must succeed"
        );

        // Second acquire fails — simulates 409
        assert!(
            flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err(),
            "second CAS must fail while flag is true"
        );

        // After the guard drops and resets the flag, it becomes acquirable again
        flag.store(false, Ordering::Release);
        assert!(
            flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "CAS must succeed again after flag reset"
        );
    }
}
