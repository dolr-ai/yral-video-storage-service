use axum::{extract::State, http::StatusCode, Json};
use serde::Serialize;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;

use crate::db;
use crate::jobs;
use crate::jobs::JobGuard;
use crate::AppState;

#[derive(serde::Deserialize, Default)]
pub struct JobParams {
    pub limit: Option<usize>,
    pub prefix: Option<String>,
}

#[derive(Serialize)]
pub struct VideoEntry {
    pub video_id: String,
    pub storj_key: Option<String>,
    pub hetzner_key: Option<String>,
}

#[derive(Serialize)]
pub struct AuditResponse {
    pub total: i64,
    pub phash_computed: i64,
    pub mirrored: i64,
    pub missing_storj: i64,
    pub missing_hetzner: i64,
    pub cleanup_pending: i64,
    pub failed: i64,
    pub error_count: i64,
    pub duplicate_phashes: Vec<DuplicateEntry>,
}

#[derive(Serialize)]
pub struct DuplicateEntry {
    pub phash: String,
    pub videos: Vec<VideoEntry>,
}

#[derive(Serialize)]
pub struct DuplicatesResponse {
    pub total_groups: usize,
    pub total_duplicate_videos: usize,
    pub groups: Vec<DuplicateGroup>,
}

#[derive(Serialize)]
pub struct DuplicateGroup {
    pub phash: String,
    pub count: usize,
    pub videos: Vec<VideoEntry>,
}

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
        if let Err(e) =
            jobs::scan_storj::run(storj, db_url, cancel, params.limit, params.prefix).await
        {
            tracing::error!(error = %e, "Job 0 (scan-storj) error");
            sentry::capture_message(&format!("scan-storj job failed: {e}"), sentry::Level::Error);
        }
    });
    StatusCode::ACCEPTED
}

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
        if let Err(e) =
            jobs::scan_hetzner::run(s3, db_url, cancel, params.limit, params.prefix).await
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
        duplicate_phashes: dups
            .into_iter()
            .map(|d| DuplicateEntry {
                phash: d.phash,
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
#[derive(Serialize)]
pub struct JobStatus {
    pub scan_storj: bool,
    pub scan_hetzner: bool,
    pub phash: bool,
    pub mirror: bool,
}

pub async fn status(State(state): State<AppState>) -> Json<JobStatus> {
    Json(JobStatus {
        scan_storj: state.job_scan_storj_running.load(Ordering::Acquire),
        scan_hetzner: state.job_scan_hetzner_running.load(Ordering::Acquire),
        phash: state.job_phash_running.load(Ordering::Acquire),
        mirror: state.job_mirror_running.load(Ordering::Acquire),
    })
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
}
