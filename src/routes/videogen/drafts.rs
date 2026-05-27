use axum::{extract::State, http::StatusCode, Json};
use chrono::{DateTime, Utc};
use ic_agent::identity::{DelegatedIdentity, Identity};
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use utoipa::ToSchema;
use yral_canisters_client::rate_limits::{RateLimits, VideoGenRequestStatus};
use yral_types::delegated_identity::DelegatedIdentityWire;

use crate::consts::RATE_LIMITS_CANISTER_ID;
use crate::AppState;

#[derive(Debug, Deserialize, ToSchema)]
pub struct InProgressDraftsRequest {
    #[schema(value_type = Object)]
    pub delegated_identity: DelegatedIdentityWire,
    pub user_id: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct InProgressDraftItem {
    /// Same operation_id returned by /generate
    pub operation_id: String,
    /// Always "in_progress" for this endpoint
    pub status: String,
    /// ISO 8601 UTC timestamp
    pub created_at: String,
    /// Compute provider (e.g. "LumaLabs", "Ltx2")
    pub provider: Option<String>,
    /// Model identifier (e.g. "ltx2", "lumalabs")
    pub model_id: String,
    pub prompt: String,
    pub thumbnail_url: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct InProgressDraftsResponse {
    pub items: Vec<InProgressDraftItem>,
}

/// Returns "in_progress" drafts for the authenticated user.
/// Queries the IC rate_limits canister for Pending/Processing video generation requests.
#[utoipa::path(
    post,
    path = "/api/v2/videogen/drafts/in-progress",
    tag = "videogen",
    request_body = InProgressDraftsRequest,
    responses(
        (status = 200, description = "In-progress drafts", body = InProgressDraftsResponse),
        (status = 401, description = "Invalid or mismatched identity"),
        (status = 400, description = "Invalid user_id principal"),
        (status = 502, description = "Canister query failed"),
    )
)]
pub async fn get_in_progress_drafts(
    State(state): State<AppState>,
    Json(request): Json<InProgressDraftsRequest>,
) -> Result<Json<InProgressDraftsResponse>, (StatusCode, String)> {
    // Validate delegated identity and extract principal
    let identity: DelegatedIdentity =
        request
            .delegated_identity
            .try_into()
            .map_err(|e: k256::elliptic_curve::Error| {
                tracing::warn!("Invalid delegated identity: {e}");
                (
                    StatusCode::UNAUTHORIZED,
                    "Invalid delegated identity".into(),
                )
            })?;

    let identity_principal = identity.sender().map_err(|_| {
        (
            StatusCode::UNAUTHORIZED,
            "Failed to derive principal".into(),
        )
    })?;

    // Verify user_id matches the identity
    let claimed_principal = candid::Principal::from_str(&request.user_id)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("Invalid user_id: {e}")))?;

    if identity_principal != claimed_principal {
        return Err((
            StatusCode::UNAUTHORIZED,
            "user_id does not match delegated identity".into(),
        ));
    }

    let rate_limits = RateLimits(*RATE_LIMITS_CANISTER_ID, &state.ic_agent);

    let requests = rate_limits
        .get_user_video_generation_requests(claimed_principal, None, None)
        .await
        .map_err(|e| {
            tracing::error!("Canister query failed: {e}");
            (
                StatusCode::BAD_GATEWAY,
                format!("Failed to fetch generation requests: {e}"),
            )
        })?;

    let items = requests
        .into_iter()
        .filter(|(_, req)| {
            matches!(
                req.status,
                VideoGenRequestStatus::Pending | VideoGenRequestStatus::Processing
            )
        })
        .map(|(key, req)| InProgressDraftItem {
            operation_id: format!("{}_{}", key.principal, key.counter),
            status: "in_progress".to_string(),
            created_at: ns_to_iso(req.created_at),
            provider: provider_from_model_id(&req.model_name),
            model_id: req.model_name,
            prompt: req.prompt,
            thumbnail_url: None,
        })
        .collect();

    Ok(Json(InProgressDraftsResponse { items }))
}

// #[derive(Debug, Serialize, ToSchema)]
// pub struct AllStatusItem {
//     pub operation_id: String,
//     /// "in_progress" | "complete: <url>" | "failed: <reason>"
//     pub status: String,
//     pub created_at: String,
//     pub provider: Option<String>,
//     pub model_id: String,
//     pub prompt: String,
//     pub thumbnail_url: Option<String>,
// }

// #[derive(Debug, Serialize, ToSchema)]
// pub struct AllStatusResponse {
//     pub items: Vec<AllStatusItem>,
// }

// Returns in-progress video generations for a principal (no auth required).
// #[utoipa::path(
//     get,
//     path = "/api/v2/videogen/in-progress/{principal}",
//     tag = "videogen",
//     params(("principal" = String, Path, description = "User principal ID")),
//     responses(
//         (status = 200, description = "In-progress items", body = InProgressDraftsResponse),
//         (status = 400, description = "Invalid principal"),
//         (status = 502, description = "Canister query failed"),
//     )
// )]
// pub async fn get_in_progress_by_principal(
//     State(state): State<AppState>,
//     Path(principal): Path<String>,
// ) -> Result<Json<InProgressDraftsResponse>, (StatusCode, String)> {
//     let user_principal = candid::Principal::from_str(&principal)
//         .map_err(|e| (StatusCode::BAD_REQUEST, format!("Invalid principal: {e}")))?;

//     let rate_limits = RateLimits(*RATE_LIMITS_CANISTER_ID, &state.ic_agent);

//     let requests = rate_limits
//         .get_user_video_generation_requests(user_principal, None, None)
//         .await
//         .map_err(|e| {
//             tracing::error!("Canister query failed: {e}");
//             (
//                 StatusCode::BAD_GATEWAY,
//                 format!("Failed to fetch generation requests: {e}"),
//             )
//         })?;

//     let items = requests
//         .into_iter()
//         .filter(|(_, req)| {
//             matches!(
//                 req.status,
//                 VideoGenRequestStatus::Pending | VideoGenRequestStatus::Processing
//             )
//         })
//         .map(|(key, req)| InProgressDraftItem {
//             operation_id: format!("{}_{}", key.principal, key.counter),
//             status: "in_progress".to_string(),
//             created_at: ns_to_iso(req.created_at),
//             provider: provider_from_model_id(&req.model_name),
//             model_id: req.model_name,
//             prompt: req.prompt,
//             thumbnail_url: None,
//         })
//         .collect();

//     Ok(Json(InProgressDraftsResponse { items }))
// }

// Returns all video generation statuses for a principal (no auth required).
// #[utoipa::path(
//     get,
//     path = "/api/v2/videogen/status/{principal}/all",
//     tag = "videogen",
//     params(("principal" = String, Path, description = "User principal ID")),
//     responses(
//         (status = 200, description = "All statuses", body = AllStatusResponse),
//         (status = 400, description = "Invalid principal"),
//         (status = 502, description = "Canister query failed"),
//     )
// )]
// pub async fn get_all_status_by_principal(
//     State(state): State<AppState>,
//     Path(principal): Path<String>,
// ) -> Result<Json<AllStatusResponse>, (StatusCode, String)> {
//     let user_principal = candid::Principal::from_str(&principal)
//         .map_err(|e| (StatusCode::BAD_REQUEST, format!("Invalid principal: {e}")))?;

//     let rate_limits = RateLimits(*RATE_LIMITS_CANISTER_ID, &state.ic_agent);

//     let requests = rate_limits
//         .get_user_video_generation_requests(user_principal, None, None)
//         .await
//         .map_err(|e| {
//             tracing::error!("Canister query failed: {e}");
//             (
//                 StatusCode::BAD_GATEWAY,
//                 format!("Failed to fetch generation requests: {e}"),
//             )
//         })?;

//     let items = requests
//         .into_iter()
//         .map(|(key, req)| {
//             let status = match req.status {
//                 VideoGenRequestStatus::Pending => "in_progress".to_string(),
//                 VideoGenRequestStatus::Processing => "in_progress".to_string(),
//                 VideoGenRequestStatus::Complete(url) => format!("complete: {url}"),
//                 VideoGenRequestStatus::Failed(reason) => format!("failed: {reason}"),
//             };
//             AllStatusItem {
//                 operation_id: format!("{}_{}", key.principal, key.counter),
//                 status,
//                 created_at: ns_to_iso(req.created_at),
//                 provider: provider_from_model_id(&req.model_name),
//                 model_id: req.model_name,
//                 prompt: req.prompt,
//                 thumbnail_url: None,
//             }
//         })
//         .collect();

//     Ok(Json(AllStatusResponse { items }))
// }

/// Maps the model_id stored in the canister to its compute provider string.
fn provider_from_model_id(model_id: &str) -> Option<String> {
    let provider = match model_id {
        "lumalabs" => "LumaLabs",
        "ltx2" => "Ltx2",
        "wan2_5" => "Wan25",
        "wan2_5_fast" => "Wan25Fast",
        "talkinghead" => "TalkingHead",
        "speech_to_video" => "SpeechToVideo",
        "inttest" => "IntTest",
        _ => return None,
    };
    Some(provider.to_string())
}

/// Converts an IC nanosecond timestamp to an ISO 8601 string.
fn ns_to_iso(ns: u64) -> String {
    let secs = (ns / 1_000_000_000) as i64;
    DateTime::<Utc>::from_timestamp(secs, 0)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
