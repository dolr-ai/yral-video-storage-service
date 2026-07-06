use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;

use crate::jobs::chain_snapshot;
use crate::jobs::JobGuard;
use crate::media_index::chain_repo;
use crate::{db, AppState};

#[derive(Deserialize)]
pub struct SnapshotParams {
    pub requested_by: Option<String>,
    /// Optional cap on posts upserted — bounded sample for preview/testing. A
    /// limited run is marked `partial` (no stale/rollup). Omit on prod for a full walk.
    pub limit: Option<u64>,
}

/// POST /chain/snapshot — trigger the fetch_posts walk. 202 accepted / 409 running.
pub async fn chain_snapshot_start(
    State(state): State<AppState>,
    Query(params): Query<SnapshotParams>,
) -> StatusCode {
    if state
        .job_chain_snapshot_running
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return StatusCode::CONFLICT;
    }
    let guard = JobGuard(state.job_chain_snapshot_running.clone());
    let db_url = state.db_url.clone();
    let agent = state.ic_agent.clone();
    let cancel = state
        .media_job_cancel
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let requested_by = params
        .requested_by
        .map(|s| s.chars().take(256).collect::<String>())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "chain_snapshot_api".into());
    let limit = params.limit;

    tokio::spawn(async move {
        let _guard = guard;
        let mut client = match db::connect(&db_url).await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error=%e, "chain_snapshot: DB connect failed");
                return;
            }
        };
        let src = chain_snapshot::LivePostSource(&agent);
        match chain_snapshot::run_chain_snapshot(&src, &mut client, &requested_by, &cancel, limit)
            .await
        {
            Ok(s) => {
                tracing::info!(job_run_id=%s.job_run_id, posts=s.posts_upserted, pages=s.pages, skipped=s.skipped, completed=s.completed, "chain_snapshot: done")
            }
            Err(e) => {
                tracing::error!(error=%e, "chain_snapshot: failed");
                sentry::capture_message(
                    &format!("chain_snapshot failed: {e}"),
                    sentry::Level::Error,
                );
            }
        }
    });
    StatusCode::ACCEPTED
}

#[derive(Serialize)]
pub struct ChainAuditResponse {
    pub total_expected: i64,
    pub category_a: i64,
    pub category_b: i64,
    pub category_c: i64,
    pub category_d: i64,
    pub category_e: i64,
    pub excluded_by_status: i64,
    pub b_backing_off: i64,
    pub d_sample: Vec<DVideo>,
    pub worst_creators: Vec<CreatorGap>,
    pub remediated: Option<Remediated>,
    pub snapshot_run_id: Option<String>,
    pub snapshot_status: Option<String>,
    pub snapshot_newest_fetched_at: Option<String>,
}
#[derive(Serialize)]
pub struct DVideo {
    pub video_uid: String,
    pub creator_principal: String,
}
#[derive(Serialize)]
pub struct CreatorGap {
    pub creator_principal: String,
    pub non_clean: i64,
}
// `import_triggered` = false means EITHER no category-C rows OR an import was already running.
#[derive(Serialize)]
pub struct Remediated {
    pub b_failures_cleared: u64,
    pub import_triggered: bool,
}

#[derive(Deserialize)]
pub struct AuditParams {
    #[serde(default)]
    pub remediate: bool,
}

/// GET /chain/audit — read-only reconciliation. `?remediate=true` also clears
/// category-B failure rows and triggers a bulk import run for category C.
pub async fn chain_audit(
    State(state): State<AppState>,
    Query(params): Query<AuditParams>,
) -> Result<Json<ChainAuditResponse>, StatusCode> {
    let client = db::connect(&state.db_url)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let fresh = chain_repo::snapshot_freshness(&client)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let rate = chain_repo::join_key_match_rate(&client, 200)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if rate < 0.5 {
        tracing::error!(
            match_rate = rate,
            "chain_audit: join-key match rate implausibly low — aborting"
        );
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    let rep = chain_repo::chain_audit(&client)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let d_sample = chain_repo::category_d_sample(&client, 100)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let worst = chain_repo::worst_creators(&client, 50)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let remediated = if params.remediate {
        let b_ids = chain_repo::category_b_video_ids(&client, 100_000)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let cleared = chain_repo::clear_phash_failures(&client, &b_ids)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let import_triggered = if rep.category_c > 0
            && state
                .job_media_import_running
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            let guard = JobGuard(state.job_media_import_running.clone());
            let db_url = state.db_url.clone();
            let cancel = state
                .media_job_cancel
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            tokio::spawn(async move {
                let _guard = guard;
                match db::connect(&db_url).await {
                    Ok(mut c) => {
                        if let Err(e) = crate::jobs::media_imports::import_current_video_index(
                            &mut c,
                            "chain_audit_remediate",
                            None,
                            &cancel,
                        )
                        .await
                        {
                            tracing::error!(error=%e, "chain_audit remediate: import failed");
                        }
                    }
                    Err(e) => tracing::error!(error=%e, "chain_audit remediate: DB connect failed"),
                }
            });
            true
        } else {
            false
        };
        Some(Remediated {
            b_failures_cleared: cleared,
            import_triggered,
        })
    } else {
        None
    };

    Ok(Json(ChainAuditResponse {
        total_expected: rep.total_expected,
        category_a: rep.category_a,
        category_b: rep.category_b,
        category_c: rep.category_c,
        category_d: rep.category_d,
        category_e: rep.category_e,
        excluded_by_status: rep.excluded_by_status,
        b_backing_off: rep.b_backing_off,
        d_sample: d_sample
            .into_iter()
            .map(|(v, cr)| DVideo {
                video_uid: v,
                creator_principal: cr,
            })
            .collect(),
        worst_creators: worst
            .into_iter()
            .map(|(cr, n)| CreatorGap {
                creator_principal: cr,
                non_clean: n,
            })
            .collect(),
        remediated,
        snapshot_run_id: fresh.run_id,
        snapshot_status: fresh.status,
        snapshot_newest_fetched_at: fresh.newest_fetched_at,
    }))
}

/// GET /chain/snapshot/status — latest chain_snapshot run row.
pub async fn chain_snapshot_status(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let client = db::connect(&state.db_url)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let row = client
        .query_opt(
            "SELECT id::TEXT, status, started_at, finished_at, totals, cursor
         FROM media_job_runs WHERE job_kind='chain_snapshot' ORDER BY started_at DESC LIMIT 1",
            &[],
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let body = match row {
        Some(r) => {
            let started_at: chrono::DateTime<chrono::Utc> = r.get(2);
            let finished_at: Option<chrono::DateTime<chrono::Utc>> = r.get(3);
            serde_json::json!({
                "id": r.get::<_,String>(0),
                "status": r.get::<_,String>(1),
                "started_at": started_at.to_rfc3339(),
                "finished_at": finished_at.map(|t| t.to_rfc3339()),
                "totals": r.get::<_,Option<serde_json::Value>>(4),
                "cursor": r.get::<_,Option<serde_json::Value>>(5),
            })
        }
        None => serde_json::json!({ "status": "none" }),
    };
    Ok(Json(body))
}

/// GET /chain/diagnose — read-only join-key diagnostic. Samples yral_posts and
/// reports master/video_index membership + a few raw example rows, so an
/// operator can tell an unrepresentative sample apart from a real
/// `video_uid` != `video_id` format skew when the audit gate trips.
pub async fn chain_diagnose(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let client = db::connect(&state.db_url)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let d = chain_repo::join_key_diag(&client, 500)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let examples: Vec<serde_json::Value> = d
        .examples
        .iter()
        .map(|e| {
            serde_json::json!({
                "video_uid": e.video_uid,
                "status": e.status,
                "in_master": e.in_master,
                "in_index": e.in_index,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({
        "total_nonstale": d.total_nonstale,
        "sampled": d.sampled,
        "in_master": d.in_master,
        "in_index": d.in_index,
        "examples": examples,
    })))
}
