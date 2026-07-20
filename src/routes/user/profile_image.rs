//! `POST /profile-image` (upload) and `DELETE /profile-image`.
//!
//! Ported from off-chain-agent to keep an identical wire contract (bare JSON body,
//! same status codes) so web/mobile migrate by base URL only. Auth is the in-body
//! chain-verified delegated identity; the canister write runs as the user via a
//! per-request agent. Responses are NOT wrapped in this repo's `ApiResponse` envelope.

use axum::{http::StatusCode, response::IntoResponse, Json};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use ic_agent::{identity::DelegatedIdentity, Agent};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use yral_canisters_client::{
    ic::USER_INFO_SERVICE_ID,
    user_info_service::{ProfileUpdateDetails, Result_, UserInfoService},
};
use yral_types::delegated_identity::DelegatedIdentityWire;

use super::profile_s3::{
    create_client, delete_profile_images, upload_profile_image, ProfileS3Config,
};
use crate::routes::upload::auth::verified_identity;

#[derive(Deserialize, ToSchema)]
pub struct UploadProfileImageRequest {
    // DelegatedIdentityWire has no ToSchema; represent it as a generic object (repo pattern).
    #[schema(value_type = Object)]
    pub delegated_identity_wire: DelegatedIdentityWire,
    /// Base64-encoded image data (optional `data:` URL prefix tolerated).
    pub image_data: String,
}

#[derive(Serialize, ToSchema)]
pub struct UploadProfileImageResponse {
    pub profile_image_url: String,
}

#[derive(Deserialize, ToSchema)]
pub struct DeleteProfileImageRequest {
    #[schema(value_type = Object)]
    pub delegated_identity_wire: DelegatedIdentityWire,
}

/// ~5 MB decoded; base64 is ~4/3 larger.
const MAX_BASE64_SIZE: usize = 7 * 1024 * 1024;

fn strip_data_url(image_data: &str) -> &str {
    match image_data.find(',') {
        Some(comma) => &image_data[comma + 1..],
        None => image_data,
    }
}

fn validate_base64_len(base64_data: &str) -> Result<(), (StatusCode, String)> {
    if base64_data.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "Image data is empty".to_string()));
    }
    if base64_data.len() > MAX_BASE64_SIZE {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "Image too large: {}MB. Maximum allowed size is 5MB",
                base64_data.len() / (1024 * 1024)
            ),
        ));
    }
    if BASE64.decode(base64_data).is_err() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Invalid image data format. Please upload a valid image".to_string(),
        ));
    }
    Ok(())
}

/// Per-request agent signing as the user (mainnet — no root-key fetch).
fn build_user_agent(identity: DelegatedIdentity) -> Result<Agent, String> {
    Agent::builder()
        .with_url(crate::consts::IC_URL.as_str())
        .with_identity(identity)
        .build()
        .map_err(|e| format!("Failed to build user agent: {e}"))
}

/// Write `profile_picture_url` to the user_info_service canister as the user.
/// Maps canister "not authorized" -> 403, everything else -> 500.
async fn set_canister_profile_url(
    identity: DelegatedIdentity,
    profile_picture_url: Option<String>,
) -> Result<(), (StatusCode, String)> {
    let agent = build_user_agent(identity).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let user_info_service = UserInfoService(USER_INFO_SERVICE_ID, &agent);
    let update = ProfileUpdateDetails {
        profile_picture_url,
        bio: None,
        website_url: None,
    };
    match user_info_service.update_profile_details(update).await {
        Ok(Result_::Ok) => Ok(()),
        Ok(Result_::Err(e)) => {
            tracing::error!("Failed to update profile in canister: {e}");
            if e.contains("not authorized") || e.contains("Not authorized") {
                Err((
                    StatusCode::FORBIDDEN,
                    "Not authorized to update profile".to_string(),
                ))
            } else {
                Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to update profile in canister: {e}"),
                ))
            }
        }
        Err(e) => {
            tracing::error!("Failed to update profile in canister: {e}");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to update profile in canister: {e}"),
            ))
        }
    }
}

#[utoipa::path(
    post,
    path = "/api/v1/user/profile-image",
    request_body = UploadProfileImageRequest,
    tag = "user",
    responses(
        (status = 200, description = "Profile image uploaded", body = UploadProfileImageResponse),
        (status = 400, description = "Invalid image data"),
        (status = 401, description = "Invalid delegated identity"),
        (status = 403, description = "Not authorized to update profile"),
        (status = 500, description = "Storage or canister error"),
    )
)]
pub async fn handle_upload_profile_image(
    Json(request): Json<UploadProfileImageRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    // Chain-verified identity → 401 on any forged/invalid wire (matches off-chain-agent).
    let (identity, principal) =
        verified_identity(&request.delegated_identity_wire).map_err(|e| {
            (
                StatusCode::UNAUTHORIZED,
                format!("Failed to get user info: {e}"),
            )
        })?;
    crate::sentry_utils::set_sentry_user(&principal.to_text(), None);

    let base64_data = strip_data_url(&request.image_data);
    validate_base64_len(base64_data)?;

    let cfg = ProfileS3Config::from_env();
    let client = create_client(&cfg)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let profile_image_url = upload_profile_image(&cfg, &client, base64_data, &principal.to_text())
        .await
        .map_err(|e| {
            tracing::error!("Failed to upload profile image: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to upload profile image: {e}"),
            )
        })?;

    set_canister_profile_url(identity, Some(profile_image_url.clone())).await?;

    Ok(Json(UploadProfileImageResponse { profile_image_url }))
}

#[utoipa::path(
    delete,
    path = "/api/v1/user/profile-image",
    request_body = DeleteProfileImageRequest,
    tag = "user",
    responses(
        (status = 200, description = "Profile image deleted"),
        (status = 401, description = "Invalid delegated identity"),
        (status = 403, description = "Not authorized to update profile"),
        (status = 500, description = "Storage or canister error"),
    )
)]
pub async fn handle_delete_profile_image(
    Json(request): Json<DeleteProfileImageRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let (identity, principal) =
        verified_identity(&request.delegated_identity_wire).map_err(|e| {
            (
                StatusCode::UNAUTHORIZED,
                format!("Failed to get user info: {e}"),
            )
        })?;
    crate::sentry_utils::set_sentry_user(&principal.to_text(), None);

    let cfg = ProfileS3Config::from_env();
    let client = create_client(&cfg)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    delete_profile_images(&cfg, &client, &principal.to_text())
        .await
        .map_err(|e| {
            tracing::error!("Failed to delete profile image: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to delete profile image: {e}"),
            )
        })?;

    // F5: clear the canister URL so reads fall back to the GobGob default (empty string
    // is treated as "no propic" by the frontend's profile_pic_or_random).
    set_canister_profile_url(identity, Some(String::new())).await?;

    Ok(StatusCode::OK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::upload::test_support::signed_wire_with_sender;
    use ic_agent::identity::Secp256k1Identity;
    use ic_agent::Identity;
    use k256::{elliptic_curve::rand_core::OsRng, SecretKey};

    #[test]
    fn strips_data_url_prefix() {
        assert_eq!(strip_data_url("data:image/png;base64,QUJD"), "QUJD");
        assert_eq!(strip_data_url("QUJD"), "QUJD");
    }

    #[test]
    fn rejects_empty_and_oversized() {
        assert!(validate_base64_len("").is_err());
        assert!(validate_base64_len(&"QQ".repeat(MAX_BASE64_SIZE)).is_err());
    }

    #[test]
    fn rejects_non_base64() {
        assert!(validate_base64_len("not valid base64!!!").is_err());
    }

    // Contract fidelity: a forged/invalid delegated identity must map to 401 (not 400).
    // Verification happens before any S3/env work, so this needs no credentials.
    #[tokio::test]
    async fn upload_forged_identity_is_401() {
        let (mut wire, _) = signed_wire_with_sender();
        let other = Secp256k1Identity::from_private_key(SecretKey::random(&mut OsRng));
        wire.from_key = other.public_key().expect("pubkey"); // forged chain
        let req = UploadProfileImageRequest {
            delegated_identity_wire: wire,
            image_data: "QUJD".to_string(),
        };
        let err = handle_upload_profile_image(Json(req))
            .await
            .err()
            .expect("forged identity must be rejected");
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);
    }
}
