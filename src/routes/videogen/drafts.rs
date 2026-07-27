use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use utoipa::ToSchema;
use yral_types::delegated_identity::DelegatedIdentityWire;

use crate::routes::identity_auth::verify_delegated_identity;
use crate::videogen::request_store;
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

/// Error bodies here stay plain strings, matching what this endpoint returned before
/// the store was wired in. Its sibling videogen endpoints use a typed `{code, message}`
/// envelope, but this one is polled by mobile and the migration contract is explicit
/// that error bodies must not change shape (see
/// `docs/superpowers/specs/2026-05-27-lean-videogen-migration-design.md`).
fn err(status: StatusCode, message: impl Into<String>) -> (StatusCode, String) {
    (status, message.into())
}

/// Returns "in_progress" video generations for the authenticated user.
///
/// Source of truth is the local `videogen_requests` store, written at `/generate`
/// and closed by the `/complete` callback — it replaced the IC rate_limits canister,
/// which used to be the only record of an in-flight generation.
///
/// Requests whose completion callback never arrived are swept to `failed` on read
/// (`VIDEOGEN_REQUEST_STALE_SECS`, default 30 min) so an abandoned generation cannot
/// spin in the UI forever.
#[utoipa::path(
    post,
    path = "/api/v2/videogen/drafts/in-progress",
    tag = "videogen",
    request_body = InProgressDraftsRequest,
    responses(
        (status = 200, description = "In-progress drafts", body = InProgressDraftsResponse),
        (status = 401, description = "Unverified, anonymous, or mismatched delegated identity"),
        (status = 400, description = "Invalid user_id principal"),
        (status = 503, description = "Request store unavailable"),
    )
)]
pub async fn get_in_progress_drafts(
    State(state): State<AppState>,
    Json(request): Json<InProgressDraftsRequest>,
) -> Result<Json<InProgressDraftsResponse>, (StatusCode, String)> {
    // Chain-verified: this endpoint returns another user's prompts if the identity can
    // be spoofed, so `try_into` (new_unchecked) is not acceptable here.
    let (_identity, identity_principal) = verify_delegated_identity(&request.delegated_identity)
        .map_err(|e| {
            tracing::warn!(error = %e, "rejected unverified delegated identity");
            err(StatusCode::UNAUTHORIZED, "Invalid delegated identity")
        })?;

    let claimed_principal = candid::Principal::from_str(&request.user_id)
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("Invalid user_id: {e}")))?;

    crate::sentry_utils::set_sentry_user(&request.user_id, None);

    if identity_principal != claimed_principal {
        return Err(err(
            StatusCode::UNAUTHORIZED,
            "user_id does not match delegated identity",
        ));
    }
    if identity_principal == candid::Principal::anonymous() {
        return Err(err(
            StatusCode::UNAUTHORIZED,
            "anonymous identity cannot list drafts",
        ));
    }

    // Key on the canonical principal text, never the raw `user_id`: `from_text` parses
    // case-insensitively, so "AAAAA-AA" and "aaaaa-aa" are the same principal but
    // would be two different store keys. `/generate` records rows under the same
    // canonical form.
    let principal = claimed_principal.to_string();

    let client = crate::db::connect(&state.db_url).await.map_err(|e| {
        tracing::error!(error = %e, "in-progress drafts: db connect failed");
        err(StatusCode::SERVICE_UNAVAILABLE, "Request store unavailable")
    })?;

    // Sweep + purge this caller's rows before reading: abandoned requests must not be
    // reported as in progress, and terminal rows must not be kept past retention.
    // Both are best-effort — a failure only risks a stale row or a late delete.
    if let Err(e) = request_store::expire_stale_for_principal(
        &client,
        &principal,
        request_store::stale_after_secs(),
    )
    .await
    {
        tracing::warn!(error = %e, "in-progress drafts: stale sweep failed (best-effort)");
    }
    if let Err(e) =
        request_store::purge_for_principal(&client, &principal, request_store::retention_days())
            .await
    {
        tracing::warn!(error = %e, "in-progress drafts: retention purge failed (best-effort)");
    }

    let rows = request_store::list_pending(&client, &principal)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "in-progress drafts: query failed");
            err(StatusCode::SERVICE_UNAVAILABLE, "Request store unavailable")
        })?;

    Ok(Json(to_response(&principal, rows)))
}

/// Shape store rows into the API response. Split out from the handler so it is
/// testable without an `AppState`/live DB.
fn to_response(
    principal: &str,
    rows: Vec<request_store::InProgressRow>,
) -> InProgressDraftsResponse {
    let items = rows
        .into_iter()
        .map(|row| InProgressDraftItem {
            operation_id: format!("{principal}_{}", row.counter),
            status: "in_progress".to_string(),
            created_at: row.created_at,
            provider: provider_from_model_id(&row.model_id),
            model_id: row.model_id,
            prompt: row.prompt,
            thumbnail_url: None,
        })
        .collect();
    InProgressDraftsResponse { items }
}

/// Maps a stored `model_id` to its compute provider string. Unknown ids yield `None`
/// rather than a guess — the client treats provider as optional display metadata.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_is_mapped_for_known_models_and_none_otherwise() {
        assert_eq!(provider_from_model_id("ltx2").as_deref(), Some("Ltx2"));
        assert_eq!(
            provider_from_model_id("speech_to_video").as_deref(),
            Some("SpeechToVideo")
        );
        assert_eq!(provider_from_model_id("unknown-model"), None);
    }

    #[test]
    fn rows_map_to_items_with_generate_compatible_operation_id() {
        let response = to_response(
            "aaaaa-aa",
            vec![request_store::InProgressRow {
                counter: 17,
                model_id: "ltx2".to_string(),
                prompt: "a sunrise".to_string(),
                created_at: "2026-07-27T10:00:00Z".to_string(),
            }],
        );

        let item = &response.items[0];
        // Must match what /generate returned for the same request.
        assert_eq!(item.operation_id, "aaaaa-aa_17");
        assert_eq!(item.status, "in_progress");
        assert_eq!(item.created_at, "2026-07-27T10:00:00Z");
        assert_eq!(item.provider.as_deref(), Some("Ltx2"));
        assert_eq!(item.prompt, "a sunrise");
        assert!(item.thumbnail_url.is_none());
    }

    #[test]
    fn empty_store_yields_empty_items() {
        assert!(to_response("aaaaa-aa", vec![]).items.is_empty());
    }

    #[test]
    fn error_bodies_stay_plain_strings_for_the_mobile_contract() {
        // Mobile polls this endpoint; the body shape must not become JSON.
        let (status, body) = err(StatusCode::UNAUTHORIZED, "Invalid delegated identity");
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, "Invalid delegated identity");
    }
}
