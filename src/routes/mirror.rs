use axum::{extract::State, http::StatusCode, Json};
use serde::Serialize;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;
use utoipa::{IntoParams, ToSchema};

use crate::db;
use crate::jobs;
use crate::jobs::JobGuard;
use crate::AppState;

#[derive(serde::Deserialize, Default, IntoParams)]
pub struct JobParams {
    /// Max number of items to process
    pub limit: Option<usize>,
    /// Key prefix filter
    pub prefix: Option<String>,
    /// If true, re-scans all items instead of incremental
    pub full_scan: Option<bool>,
}

#[derive(Serialize, ToSchema)]
pub struct VideoEntry {
    pub video_id: String,
    pub storj_key: Option<String>,
    pub hetzner_key: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct AuditResponse {
    pub total: i64,
    pub phash_computed: i64,
    pub mirrored: i64,
    pub missing_storj: i64,
    pub missing_hetzner: i64,
    pub cleanup_pending: i64,
    pub failed: i64,
    pub error_count: i64,
    pub status_breakdown: std::collections::HashMap<String, i64>,
    pub duplicate_phashes: Vec<DuplicateEntry>,
}

#[derive(Serialize, ToSchema)]
pub struct DuplicateEntry {
    pub phash: String,
    pub hash_kind: String,
    pub hash_version: String,
    pub videos: Vec<VideoEntry>,
}

#[derive(Serialize, ToSchema)]
pub struct DuplicatesResponse {
    pub total_groups: usize,
    pub total_duplicate_videos: usize,
    pub groups: Vec<DuplicateGroup>,
}

#[derive(Serialize, ToSchema)]
pub struct DuplicateGroup {
    pub phash: String,
    pub hash_kind: String,
    pub hash_version: String,
    pub count: usize,
    pub videos: Vec<VideoEntry>,
}

#[utoipa::path(
    post,
    path = "/mirror/jobs/scan-storj",
    tag = "mirror",
    params(JobParams),
    responses(
        (status = 202, description = "Job started"),
        (status = 409, description = "Job already running"),
        (status = 401, description = "Unauthorized"),
    )
)]
pub async fn scan_storj(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<JobParams>,
) -> StatusCode {
    if state
        .job_scan_storj_running
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return StatusCode::CONFLICT;
    }
    let storj = state.storj_client.clone();
    let db_url = state.db_url.clone();
    let cancel = state
        .job_cancel
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let guard = JobGuard(state.job_scan_storj_running.clone());
    tokio::spawn(async move {
        let _guard = guard;
        if let Err(e) = jobs::scan_storj::run(
            storj,
            db_url,
            cancel,
            params.limit,
            params.prefix,
            params.full_scan.unwrap_or(false),
        )
        .await
        {
            tracing::error!(error = %e, "Job 0 (scan-storj) error");
            sentry::capture_message(&format!("scan-storj job failed: {e}"), sentry::Level::Error);
        }
    });
    StatusCode::ACCEPTED
}

#[utoipa::path(
    post,
    path = "/mirror/jobs/scan-hetzner",
    tag = "mirror",
    params(JobParams),
    responses(
        (status = 202, description = "Job started"),
        (status = 409, description = "Job already running"),
        (status = 401, description = "Unauthorized"),
    )
)]
pub async fn scan_hetzner(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<JobParams>,
) -> StatusCode {
    if state
        .job_scan_hetzner_running
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return StatusCode::CONFLICT;
    }
    let s3 = state.s3_client.clone();
    let db_url = state.db_url.clone();
    let cancel = state
        .job_cancel
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let guard = JobGuard(state.job_scan_hetzner_running.clone());
    tokio::spawn(async move {
        let _guard = guard;
        if let Err(e) = jobs::scan_hetzner::run(
            s3,
            db_url,
            cancel,
            params.limit,
            params.prefix,
            params.full_scan.unwrap_or(false),
        )
        .await
        {
            tracing::error!(error = %e, "Job 1 (scan-hetzner) error");
            sentry::capture_message(
                &format!("scan-hetzner job failed: {e}"),
                sentry::Level::Error,
            );
        }
    });
    StatusCode::ACCEPTED
}

#[utoipa::path(
    post,
    path = "/mirror/jobs/phash",
    tag = "mirror",
    params(JobParams),
    responses(
        (status = 202, description = "Job started"),
        (status = 409, description = "Job already running"),
        (status = 401, description = "Unauthorized"),
    )
)]
pub async fn phash_backfill(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<JobParams>,
) -> StatusCode {
    if state
        .job_phash_running
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return StatusCode::CONFLICT;
    }
    let s3 = state.s3_client.clone();
    let db_url = state.db_url.clone();
    let cancel = state
        .job_cancel
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let guard = JobGuard(state.job_phash_running.clone());
    tokio::spawn(async move {
        let _guard = guard;
        if let Err(e) = jobs::phash_backfill::run(s3, db_url, cancel, params.limit).await {
            tracing::error!(error = %e, "Job 2 (phash-backfill) error");
            sentry::capture_message(&format!("phash job failed: {e}"), sentry::Level::Error);
        }
    });
    StatusCode::ACCEPTED
}

#[utoipa::path(
    post,
    path = "/mirror/jobs/mirror",
    tag = "mirror",
    params(JobParams),
    responses(
        (status = 202, description = "Job started"),
        (status = 409, description = "Job already running"),
        (status = 401, description = "Unauthorized"),
    )
)]
pub async fn mirror(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<JobParams>,
) -> StatusCode {
    if state
        .job_mirror_running
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return StatusCode::CONFLICT;
    }
    let s3 = state.s3_client.clone();
    let db_url = state.db_url.clone();
    let cancel = state
        .job_cancel
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let guard = JobGuard(state.job_mirror_running.clone());
    tokio::spawn(async move {
        let _guard = guard;
        if let Err(e) = jobs::mirror::run(s3, db_url, cancel, params.limit).await {
            tracing::error!(error = %e, "Job 3 (mirror) error");
            sentry::capture_message(&format!("mirror job failed: {e}"), sentry::Level::Error);
        }
    });
    StatusCode::ACCEPTED
}

#[utoipa::path(
    post,
    path = "/mirror/jobs/run-pipeline",
    tag = "mirror",
    params(JobParams),
    responses(
        (status = 202, description = "Pipeline started"),
        (status = 409, description = "Pipeline already running"),
        (status = 401, description = "Unauthorized"),
    )
)]
pub async fn run_pipeline(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<JobParams>,
) -> StatusCode {
    if state
        .job_pipeline_running
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return StatusCode::CONFLICT;
    }
    let pipeline_guard = JobGuard(state.job_pipeline_running.clone());

    let s3 = state.s3_client.clone();
    let db_url = state.db_url.clone();
    let cancel = state
        .job_cancel
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();

    if cancel.is_cancelled() {
        drop(pipeline_guard);
        return StatusCode::CONFLICT;
    }

    tokio::spawn(async move {
        let _pipeline_guard = pipeline_guard;

        let db_client = match db::connect(&db_url).await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "Pipeline DB connect failed");
                return;
            }
        };

        let mut continuation_token: Option<String> = None;
        let mut grand_total = 0usize;
        let full_scan = params.full_scan.unwrap_or(false);
        let mut current_start_after = if !full_scan {
            match db::get_max_hetzner_key(&db_client, params.prefix.as_deref()).await {
                Ok(k) => k,
                Err(e) => {
                    tracing::error!(error = %e, "Failed to get max hetzner key");
                    None
                }
            }
        } else {
            None
        };

        loop {
            if cancel.is_cancelled() {
                return;
            }

            // Step 1: fetch one S3 page from Hetzner
            let (objects, next_token) = match s3
                .list_objects_page(
                    params.prefix.as_deref(),
                    continuation_token.as_deref(),
                    current_start_after.as_deref(),
                )
                .await
            {
                Ok(result) => result,
                Err(e) => {
                    tracing::error!(error = %e, "Pipeline scan-hetzner page error");
                    sentry::capture_message(
                        &format!("Pipeline scan-hetzner page failed: {e}"),
                        sentry::Level::Error,
                    );
                    return;
                }
            };

            current_start_after = None; // Only use start_after for the first page

            // Step 2: upsert this page to DB
            for obj in &objects {
                if obj.key.ends_with("_thumbnail.png") || obj.key.ends_with("-thumbnail.png") {
                    continue;
                }
                let Some(video_id) = jobs::video_id_from_key(&obj.key) else {
                    continue;
                };
                let is_temp = obj.key.contains(crate::consts::TEMP_KEY_PREFIX.as_str());

                let upsert_result = if full_scan {
                    db::upsert_hetzner_key_with_reset(&db_client, &video_id, &obj.key, is_temp)
                        .await
                } else {
                    db::upsert_hetzner_key(&db_client, &video_id, &obj.key, is_temp).await
                };

                if let Err(e) = upsert_result {
                    tracing::error!(error = %e, key = %obj.key, "Pipeline upsert error");
                }
                grand_total += 1;
                jobs::log_progress(grand_total, "pipeline/scan");
            }

            if cancel.is_cancelled() {
                return;
            }

            // Step 3: phash all pending rows (operates on full DB state, not just this page)
            if let Err(e) =
                crate::jobs::phash_backfill::run(s3.clone(), db_url.clone(), cancel.clone(), None)
                    .await
            {
                tracing::error!(error = %e, "Pipeline phash error");
            }

            if cancel.is_cancelled() {
                return;
            }

            // Step 4: mirror all pending rows (operates on full DB state, not just this page)
            if let Err(e) =
                crate::jobs::mirror::run(s3.clone(), db_url.clone(), cancel.clone(), None).await
            {
                tracing::error!(error = %e, "Pipeline mirror error");
            }

            if params.limit.is_some_and(|n| grand_total >= n) {
                tracing::info!(grand_total, "Pipeline: scan limit reached");
                return;
            }

            continuation_token = next_token;
            if continuation_token.is_none() {
                break;
            }
        }

        tracing::info!(grand_total, "Pipeline: complete");
    });

    StatusCode::ACCEPTED
}

#[utoipa::path(
    get,
    path = "/mirror/audit",
    tag = "mirror",
    responses(
        (status = 200, description = "Audit stats", body = AuditResponse),
        (status = 401, description = "Unauthorized"),
        (status = 500, description = "Internal server error"),
    )
)]
pub async fn audit(State(state): State<AppState>) -> Result<Json<AuditResponse>, StatusCode> {
    let client = db::connect(&state.db_url)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let stats = db::get_audit_stats(&client)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let dups = db::get_duplicate_phashes(&client)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(AuditResponse {
        total: stats.total,
        phash_computed: stats.phash_computed,
        mirrored: stats.mirrored,
        missing_storj: stats.missing_storj,
        missing_hetzner: stats.missing_hetzner,
        cleanup_pending: stats.cleanup_pending,
        failed: stats.failed,
        error_count: stats.error_count,
        status_breakdown: stats.status_breakdown,
        duplicate_phashes: dups
            .into_iter()
            .map(|d| DuplicateEntry {
                phash: d.phash,
                hash_kind: d.hash_kind,
                hash_version: d.hash_version,
                videos: d
                    .videos
                    .into_iter()
                    .map(|v| VideoEntry {
                        video_id: v.video_id,
                        storj_key: v.storj_key,
                        hetzner_key: v.hetzner_key,
                    })
                    .collect(),
            })
            .collect(),
    }))
}

#[derive(Serialize, ToSchema)]
pub struct FailedJobEntry {
    pub video_id: String,
    pub error_message: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct FailedJobsResponse {
    pub count: usize,
    pub jobs: Vec<FailedJobEntry>,
}

#[utoipa::path(
    get,
    path = "/mirror/jobs/failed",
    tag = "mirror",
    responses(
        (status = 200, description = "Failed jobs list", body = FailedJobsResponse),
        (status = 401, description = "Unauthorized"),
        (status = 500, description = "Internal server error"),
    )
)]
pub async fn failed_jobs(
    State(state): State<AppState>,
) -> Result<Json<FailedJobsResponse>, StatusCode> {
    let client = db::connect(&state.db_url)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let jobs = db::get_failed_jobs(&client)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let count = jobs.len();
    Ok(Json(FailedJobsResponse {
        count,
        jobs: jobs
            .into_iter()
            .map(|j| FailedJobEntry {
                video_id: j.video_id,
                error_message: j.error_message,
            })
            .collect(),
    }))
}

#[derive(Serialize, ToSchema)]
pub struct RetryResponse {
    pub reset_count: i64,
}

#[utoipa::path(
    post,
    path = "/mirror/jobs/retry-failed",
    tag = "mirror",
    responses(
        (status = 200, description = "Failed jobs reset for retry", body = RetryResponse),
        (status = 401, description = "Unauthorized"),
        (status = 500, description = "Internal server error"),
    )
)]
pub async fn retry_failed(
    State(state): State<AppState>,
) -> Result<Json<RetryResponse>, StatusCode> {
    let client = db::connect(&state.db_url)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let reset_count = db::reset_failed_to_retry(&client)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    tracing::info!(reset_count, "retry-failed: reset jobs to phash_computed");
    Ok(Json(RetryResponse { reset_count }))
}

#[utoipa::path(
    get,
    path = "/mirror/duplicates/{video_id}",
    tag = "mirror",
    params(
        ("video_id" = String, Path, description = "Video ID to look up duplicates for")
    ),
    responses(
        (status = 200, description = "Duplicate group for video, or null", body = Option<DuplicateGroup>),
        (status = 401, description = "Unauthorized"),
        (status = 500, description = "Internal server error"),
    )
)]
pub async fn video_duplicates(
    State(state): State<AppState>,
    axum::extract::Path(video_id): axum::extract::Path<String>,
) -> Result<Json<Option<DuplicateGroup>>, StatusCode> {
    let client = db::connect(&state.db_url)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let result = db::get_duplicates_for_video(&client, &video_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(result.map(|d| {
        let count = d.videos.len();
        DuplicateGroup {
            phash: d.phash,
            hash_kind: d.hash_kind,
            hash_version: d.hash_version,
            count,
            videos: d
                .videos
                .into_iter()
                .map(|v| VideoEntry {
                    video_id: v.video_id,
                    storj_key: v.storj_key,
                    hetzner_key: v.hetzner_key,
                })
                .collect(),
        }
    })))
}

#[utoipa::path(
    get,
    path = "/mirror/duplicates",
    tag = "mirror",
    responses(
        (status = 200, description = "All duplicate phash groups", body = DuplicatesResponse),
        (status = 401, description = "Unauthorized"),
        (status = 500, description = "Internal server error"),
    )
)]
pub async fn duplicates(
    State(state): State<AppState>,
) -> Result<Json<DuplicatesResponse>, StatusCode> {
    let client = db::connect(&state.db_url)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let dups = db::get_duplicate_phashes(&client)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let groups: Vec<DuplicateGroup> = dups
        .into_iter()
        .map(|d| {
            let count = d.videos.len();
            DuplicateGroup {
                phash: d.phash,
                hash_kind: d.hash_kind,
                hash_version: d.hash_version,
                count,
                videos: d
                    .videos
                    .into_iter()
                    .map(|v| VideoEntry {
                        video_id: v.video_id,
                        storj_key: v.storj_key,
                        hetzner_key: v.hetzner_key,
                    })
                    .collect(),
            }
        })
        .collect();

    let total_duplicate_videos = groups.iter().map(|g| g.count).sum();

    Ok(Json(DuplicatesResponse {
        total_groups: groups.len(),
        total_duplicate_videos,
        groups,
    }))
}

/// Cancel all running background jobs.
/// The token is replaced so subsequently started jobs get a fresh token.
#[utoipa::path(
    post,
    path = "/mirror/jobs/cancel",
    tag = "mirror",
    responses(
        (status = 200, description = "Cancellation signal sent"),
        (status = 401, description = "Unauthorized"),
    )
)]
pub async fn cancel_all(State(state): State<AppState>) -> Json<serde_json::Value> {
    // Cancel the current token and replace with a fresh one
    let old_token = {
        let mut token = state.job_cancel.lock().unwrap_or_else(|e| e.into_inner());
        let old = token.clone();
        *token = CancellationToken::new();
        old
    };
    old_token.cancel();

    let cancelled: Vec<&str> = [
        ("scan-storj", &state.job_scan_storj_running),
        ("scan-hetzner", &state.job_scan_hetzner_running),
        ("phash", &state.job_phash_running),
        ("mirror", &state.job_mirror_running),
    ]
    .iter()
    .filter(|(_, flag)| flag.load(Ordering::Acquire))
    .map(|(name, _)| *name)
    .collect();

    tracing::info!("Cancellation requested for jobs: {:?}", cancelled);
    Json(serde_json::json!({
        "message": "cancellation signal sent",
        "jobs_running_at_cancel": cancelled,
    }))
}

/// Report the current status of all background jobs.
#[derive(Serialize, ToSchema)]
pub struct JobStatus {
    pub scan_storj: bool,
    pub scan_hetzner: bool,
    pub phash: bool,
    pub mirror: bool,
    pub pipeline: bool,
}

#[utoipa::path(
    get,
    path = "/mirror/jobs/status",
    tag = "mirror",
    responses(
        (status = 200, description = "Current job running status", body = JobStatus),
        (status = 401, description = "Unauthorized"),
    )
)]
pub async fn status(State(state): State<AppState>) -> Json<JobStatus> {
    Json(JobStatus {
        scan_storj: state.job_scan_storj_running.load(Ordering::Acquire),
        scan_hetzner: state.job_scan_hetzner_running.load(Ordering::Acquire),
        phash: state.job_phash_running.load(Ordering::Acquire),
        mirror: state.job_mirror_running.load(Ordering::Acquire),
        pipeline: state.job_pipeline_running.load(Ordering::Acquire),
    })
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
pub struct ConfigResponse {
    pub phash_concurrency: usize,
    pub mirror_concurrency: usize,
    pub scan_page_size: i64,
}

#[derive(serde::Deserialize, ToSchema)]
pub struct ConfigUpdate {
    pub phash_concurrency: Option<usize>,
    pub mirror_concurrency: Option<usize>,
    pub scan_page_size: Option<i64>,
}

#[utoipa::path(
    get,
    path = "/mirror/config",
    tag = "mirror",
    responses(
        (status = 200, description = "Current job concurrency config", body = ConfigResponse),
        (status = 401, description = "Unauthorized"),
    )
)]
pub async fn get_config() -> Json<ConfigResponse> {
    Json(ConfigResponse {
        phash_concurrency: crate::consts::PHASH_CONCURRENCY
            .load(std::sync::atomic::Ordering::Relaxed),
        mirror_concurrency: crate::consts::MIRROR_CONCURRENCY
            .load(std::sync::atomic::Ordering::Relaxed),
        scan_page_size: crate::consts::SCAN_PAGE_SIZE.load(std::sync::atomic::Ordering::Relaxed),
    })
}

#[utoipa::path(
    post,
    path = "/mirror/config",
    tag = "mirror",
    request_body = ConfigUpdate,
    responses(
        (status = 200, description = "Updated config", body = ConfigResponse),
        (status = 401, description = "Unauthorized"),
    )
)]
pub async fn update_config(Json(payload): Json<ConfigUpdate>) -> Json<ConfigResponse> {
    if let Some(v) = payload.phash_concurrency {
        crate::consts::PHASH_CONCURRENCY.store(v, std::sync::atomic::Ordering::Relaxed);
    }
    if let Some(v) = payload.mirror_concurrency {
        crate::consts::MIRROR_CONCURRENCY.store(v, std::sync::atomic::Ordering::Relaxed);
    }
    if let Some(v) = payload.scan_page_size {
        crate::consts::SCAN_PAGE_SIZE.store(v, std::sync::atomic::Ordering::Relaxed);
    }
    get_config().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_params_limit_parses() {
        let p: JobParams = serde_json::from_value(serde_json::json!({"limit": 10})).unwrap();
        assert_eq!(p.limit, Some(10));
    }

    #[test]
    fn job_params_no_limit_defaults_none() {
        let p: JobParams = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(p.limit, None);
    }

    #[test]
    fn job_params_invalid_limit_fails() {
        let result: Result<JobParams, _> =
            serde_json::from_value(serde_json::json!({"limit": "abc"}));
        assert!(result.is_err());
    }

    #[test]
    fn job_params_full_scan_parses() {
        let p: JobParams = serde_json::from_value(serde_json::json!({"full_scan": true})).unwrap();
        assert_eq!(p.full_scan, Some(true));

        let p: JobParams = serde_json::from_value(serde_json::json!({"full_scan": false})).unwrap();
        assert_eq!(p.full_scan, Some(false));
    }

    #[test]
    fn job_params_full_scan_defaults_none() {
        let p: JobParams = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(p.full_scan, None);
    }
}
