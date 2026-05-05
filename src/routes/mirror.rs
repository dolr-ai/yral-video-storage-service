use axum::{extract::State, http::StatusCode, Json};
use serde::Serialize;
use std::sync::atomic::Ordering;

use crate::db;
use crate::jobs;
use crate::jobs::JobGuard;
use crate::AppState;

#[derive(serde::Deserialize, Default)]
pub struct JobParams {
    pub limit: Option<usize>,
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
    pub video_ids: Vec<String>,
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
    let cancel = state.cancel.clone();
    let guard = JobGuard(state.job_scan_storj_running.clone());
    tokio::spawn(async move {
        let _guard = guard;
        if let Err(e) = jobs::scan_storj::run(storj, db_url, cancel, params.limit).await {
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
    let cancel = state.cancel.clone();
    let guard = JobGuard(state.job_scan_hetzner_running.clone());
    tokio::spawn(async move {
        let _guard = guard;
        if let Err(e) = jobs::scan_hetzner::run(s3, db_url, cancel, params.limit).await {
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
    let cancel = state.cancel.clone();
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
    let cancel = state.cancel.clone();
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
                video_ids: d.video_ids,
            })
            .collect(),
    }))
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
