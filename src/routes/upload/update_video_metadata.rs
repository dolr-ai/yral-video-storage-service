//! `POST /update-video-metadata` — finalize a Storj upload + register the post on
//! the user-post-service canister, then fire analytics + notification.
//!
//! Ported from yral-video-upload-service. Adaptations: uses storage's shared
//! `ic_agent` (D2), `auth::verified_sender` for the chain-verified auth check (1.2c),
//! and the Phase-1 `storj_finalize::finalize_via_http` self-hop (Phase 3 internalizes).

use std::collections::HashMap;

use axum::{extract::State, Json};
use candid::Principal;
use serde::Deserialize;
use yral_canisters_client::{
    ic::{USER_INFO_SERVICE_ID, USER_POST_SERVICE_ID},
    user_post_service::{
        PostDetailsFromFrontendV1, PostStatusFromFrontend, Result_, UserPostService,
    },
};
use yral_types::delegated_identity::DelegatedIdentityWire;

use super::{
    auth::verified_sender,
    events::EventService,
    notification::{NotificationClient, NotificationType},
    storj_finalize::finalize_via_http,
    types::{ApiResponse, AppError, RequestPostDetails},
};
use crate::AppState;

static POST_DETAILS_KEY: &str = "post_details";

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateMetadataRequest {
    pub delegated_identity_wire: DelegatedIdentityWire,
    pub meta: HashMap<String, String>,
    pub post_details: PostDetailsFromFrontendV1,
}

/// `POST /update-video-metadata`. Public route — auth is the in-body chain-verified
/// delegated identity. Core flow (finalize + canister add_post) always runs; the
/// analytics event + push notification fire only if their tokens are configured.
pub async fn update_video_metadata(
    State(state): State<AppState>,
    Json(req): Json<UpdateMetadataRequest>,
) -> ApiResponse<()> {
    update_metadata_impl(
        &state,
        state.upload.events_service.as_ref(),
        state.upload.notification_client.as_ref(),
        req,
    )
    .await
    .into()
}

/// Verify the delegation chain and assert the sender owns the post. Pure (no I/O) —
/// unit-tested without network.
fn authorize_publisher(req: &UpdateMetadataRequest) -> Result<Principal, AppError> {
    let publisher = verified_sender(&req.delegated_identity_wire)?;
    if publisher != req.post_details.creator_principal {
        return Err(AppError::Unauthorized(
            "Publisher user id does not match creator principal in post details".to_string(),
        ));
    }
    Ok(publisher)
}

/// Inject the serialized `RequestPostDetails` into `meta["post_details"]`. Pure.
fn inject_post_details(
    meta: &mut HashMap<String, String>,
    post_details: &PostDetailsFromFrontendV1,
) -> Result<(), AppError> {
    let request_post_details: RequestPostDetails = post_details.clone().into();
    meta.insert(
        POST_DETAILS_KEY.to_string(),
        serde_json::to_string(&request_post_details)?,
    );
    Ok(())
}

/// Shared by the HTTP handler and the in-process videogen draft client (Phase 2).
/// `events_service`/`notification_client` are optional best-effort side-effects.
pub(crate) async fn update_metadata_impl(
    state: &AppState,
    events_service: Option<&EventService>,
    notification_client: Option<&NotificationClient>,
    mut req: UpdateMetadataRequest,
) -> Result<(), AppError> {
    let publisher = authorize_publisher(&req)?;
    let publisher_text = publisher.to_text();

    inject_post_details(&mut req.meta, &req.post_details)?;

    // Phase-1: finalize via a self-HTTP hop to /duplicate_raw/finalize.
    // Phase 3 replaces this with an in-process call.
    let base = std::env::var(crate::consts::PUBLIC_BASE_URL)
        .map_err(|_| AppError::InternalError("PUBLIC_BASE_URL not set".into()))?;
    finalize_via_http(
        &base,
        &publisher_text,
        &req.post_details.id,
        false,
        req.meta.clone(),
    )
    .await?;

    upload_video_canister(
        state,
        events_service,
        notification_client,
        req.post_details.clone(),
    )
    .await
}

async fn upload_video_canister(
    state: &AppState,
    events_service: Option<&EventService>,
    notification_client: Option<&NotificationClient>,
    post_details: PostDetailsFromFrontendV1,
) -> Result<(), AppError> {
    let user_post_service = UserPostService(USER_POST_SERVICE_ID, &state.ic_agent);
    let post_is_published = matches!(post_details.status, PostStatusFromFrontend::Published);

    match user_post_service.add_post_v_1(post_details.clone()).await? {
        Result_::Ok => {
            if post_is_published {
                if let Some(events) = events_service {
                    let _ = events
                        .send_video_upload_successful_event(
                            post_details.video_uid.clone(),
                            post_details.hashtags.len(),
                            false,
                            true,
                            post_details.id.clone(),
                            post_details.creator_principal,
                            USER_INFO_SERVICE_ID,
                            String::new(),
                            None,
                        )
                        .await
                        .inspect_err(|e| {
                            tracing::error!("Failed to send video_upload_successful event: {e}")
                        });
                }
            }

            if let Some(notif) = notification_client {
                let payload = if post_is_published {
                    NotificationType::VideoPublished {
                        user_principal: post_details.creator_principal,
                        post_id: post_details.id.clone(),
                    }
                } else {
                    NotificationType::VideoUploadedToDraft {
                        user_principal: post_details.creator_principal,
                        post_id: post_details.id.clone(),
                    }
                };
                notif
                    .send_notification(payload, post_details.creator_principal)
                    .await;
            }

            Ok(())
        }
        Result_::Err(user_post_service_error) => {
            let error = format!("{user_post_service_error:?}");
            if let Some(events) = events_service {
                let _ = events
                    .send_video_event_unsuccessful(
                        error.clone(),
                        post_details.hashtags.len(),
                        false,
                        true,
                        post_details.creator_principal,
                        String::new(),
                        USER_INFO_SERVICE_ID,
                    )
                    .await
                    .inspect_err(|e| {
                        tracing::error!("Failed to send video_event_unsuccessful: {e}")
                    });
            }
            Err(AppError::CanisterError(error))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::upload::test_support::signed_wire_with_sender;

    fn post_details_for(creator: Principal) -> PostDetailsFromFrontendV1 {
        PostDetailsFromFrontendV1 {
            id: "post-1".into(),
            video_uid: "post-1".into(),
            creator_principal: creator,
            status: PostStatusFromFrontend::Draft,
            hashtags: vec![],
            description: String::new(),
        }
    }

    #[test]
    fn authorize_rejects_sender_creator_mismatch() {
        let (wire, _sender) = signed_wire_with_sender();
        // creator is a DIFFERENT principal than the wire's verified sender.
        let req = UpdateMetadataRequest {
            delegated_identity_wire: wire,
            meta: HashMap::new(),
            post_details: post_details_for(Principal::anonymous()),
        };
        assert!(matches!(
            authorize_publisher(&req),
            Err(AppError::Unauthorized(_))
        ));
    }

    #[test]
    fn authorize_accepts_matching_creator() {
        let (wire, sender) = signed_wire_with_sender();
        let req = UpdateMetadataRequest {
            delegated_identity_wire: wire,
            meta: HashMap::new(),
            post_details: post_details_for(sender),
        };
        assert_eq!(authorize_publisher(&req).expect("authorized"), sender);
    }

    #[test]
    fn inject_post_details_writes_request_post_details_json() {
        let mut meta = HashMap::new();
        let pd = post_details_for(Principal::anonymous());
        inject_post_details(&mut meta, &pd).expect("inject");
        let raw = meta
            .get(POST_DETAILS_KEY)
            .expect("post_details key present");
        let parsed: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed["id"], "post-1");
        assert_eq!(parsed["video_uid"], "post-1");
    }
}
