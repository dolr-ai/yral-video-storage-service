//! `POST /get-upload-url` — validate the publisher principal, mint a video id, and
//! return a signed-style upload URL pointing at this service's own
//! `/duplicate_raw/upload`. Public; needs no upload tokens (no events/notifications).
//!
//! Ported from yral-video-upload-service. `get_upload_url_core` is extracted so the
//! videogen `reserve_upload_destination` / `generate_fresh_upload_url` paths can call
//! it in-process (Phase 2.5).

use axum::{extract::State, Json};
use candid::Principal;
use ic_agent::Agent;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use yral_canisters_client::{
    ic::USER_INFO_SERVICE_ID,
    user_info_service::{Result6, UserInfoService},
};

use super::types::{ApiResponse, AppError};
use crate::AppState;

#[derive(Debug, Clone, Deserialize)]
pub struct GetUploadUrlReq {
    pub publisher_user_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GetUploadUrlResp {
    pub upload_url: String,
    pub video_id: String,
}

/// Build `{base}/duplicate_raw/upload?publisher_user_id=&video_id=&is_nsfw=` with
/// URL-encoded params (S16). Pure — unit tested. `pub(crate)` so the videogen
/// upload-refresh path can re-issue a URL for an EXISTING video_id.
pub(crate) fn build_upload_url(
    base: &str,
    publisher_user_id: &str,
    video_id: &str,
    is_nsfw: bool,
) -> String {
    let mut url = Url::parse(&format!(
        "{}/duplicate_raw/upload",
        base.trim_end_matches('/')
    ))
    .expect("PUBLIC_BASE_URL must be a valid base URL");
    url.query_pairs_mut()
        .append_pair("publisher_user_id", publisher_user_id)
        .append_pair("video_id", video_id)
        .append_pair("is_nsfw", if is_nsfw { "true" } else { "false" });
    url.to_string()
}

/// Validate the principal exists (via user-info-service) and return a fresh upload
/// URL + video id. Reusable in-process (Phase 2.5). NOTE: makes a canister call.
// allow(dead_code): consumed by the handler below and by videogen Phase 2.5.
#[allow(dead_code)]
pub async fn get_upload_url_core(
    ic_agent: &Agent,
    base: &str,
    publisher_user_id: &str,
) -> Result<GetUploadUrlResp, AppError> {
    let user_principal = Principal::from_text(publisher_user_id)?;
    let user_info_service = UserInfoService(USER_INFO_SERVICE_ID, ic_agent);

    match user_info_service
        .get_user_profile_details_v_6(user_principal)
        .await?
    {
        Result6::Ok(_) => {}
        Result6::Err(e) => {
            tracing::error!("Failed to fetch user profile details: {e}");
            return Err(AppError::UserProfileFetchError(e));
        }
    }

    let video_id = Uuid::new_v4().to_string();
    Ok(GetUploadUrlResp {
        upload_url: build_upload_url(base, publisher_user_id, &video_id, false),
        video_id,
    })
}

// allow(dead_code): registered on the router in Task 1.10.
#[allow(dead_code)]
pub async fn get_upload_url(
    State(state): State<AppState>,
    Json(req): Json<GetUploadUrlReq>,
) -> ApiResponse<GetUploadUrlResp> {
    let base = match std::env::var(crate::consts::PUBLIC_BASE_URL) {
        Ok(b) => b,
        Err(_) => {
            return AppError::InternalError("PUBLIC_BASE_URL not set".into()).to_api_response()
        }
    };
    get_upload_url_core(&state.ic_agent, &base, &req.publisher_user_id)
        .await
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_upload_url_maps_args_to_query_keys() {
        let u = build_upload_url("https://x.test", "principal abc", "vid-1", false);
        assert!(u.starts_with("https://x.test/duplicate_raw/upload?"));
        // publisher arg -> publisher_user_id key, encoded (no raw space)
        assert!(
            !u.contains("principal abc"),
            "publisher must be encoded: {u}"
        );
        assert!(u.contains("video_id=vid-1"));
        assert!(u.contains("is_nsfw=false"));
        // ensure args weren't transposed into the wrong keys (N4)
        assert!(u.contains("publisher_user_id="));
    }
}
