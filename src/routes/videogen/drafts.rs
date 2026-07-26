use axum::{extract::State, http::StatusCode, Json};
use ic_agent::identity::{DelegatedIdentity, Identity};
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use utoipa::ToSchema;
use yral_types::delegated_identity::DelegatedIdentityWire;

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
///
/// Previously queried the IC rate_limits canister for Pending/Processing
/// video generation requests. The canister call has been removed — this
/// endpoint now returns an empty list until a local data source (e.g.
/// querying Prakash's own DB for in-progress requests) is implemented.
#[utoipa::path(
    post,
    path = "/api/v2/videogen/drafts/in-progress",
    tag = "videogen",
    request_body = InProgressDraftsRequest,
    responses(
        (status = 200, description = "In-progress drafts", body = InProgressDraftsResponse),
        (status = 401, description = "Invalid or mismatched identity"),
        (status = 400, description = "Invalid user_id principal"),
    )
)]
pub async fn get_in_progress_drafts(
    _state: State<AppState>,
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

    crate::sentry_utils::set_sentry_user(&request.user_id, None);

    if identity_principal != claimed_principal {
        return Err((
            StatusCode::UNAUTHORIZED,
            "user_id does not match delegated identity".into(),
        ));
    }

    // TODO: query Prakash's own DB/draft service for in-progress video-gen
    // requests by principal. The canister was the previous source of truth;
    // it has been removed. Returns empty until a local data source is wired up.
    Ok(Json(InProgressDraftsResponse { items: vec![] }))
}
