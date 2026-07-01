//! `POST /mark-post-as-published` — flip a post to Uploaded after verifying the
//! caller (chain-verified delegated identity) owns it, then fire analytics +
//! notification. Ported from yral-video-upload-service.

use axum::{extract::State, Json};
use serde::Deserialize;
use yral_canisters_client::{
    ic::{USER_INFO_SERVICE_ID, USER_POST_SERVICE_ID},
    user_post_service::{PostStatus, Result2, UserPostService},
};
use yral_types::delegated_identity::DelegatedIdentityWire;

use super::{
    auth::verified_sender,
    events::EventService,
    notification::{NotificationClient, NotificationType},
    types::{ApiResponse, AppError},
};
use crate::AppState;

#[derive(Debug, Clone, Deserialize)]
pub struct MarkPostAsPublishedRequest {
    pub post_id: String,
    pub delegated_identity_wire: DelegatedIdentityWire,
}

pub async fn mark_post_as_published(
    State(state): State<AppState>,
    Json(payload): Json<MarkPostAsPublishedRequest>,
) -> ApiResponse<()> {
    mark_post_as_published_impl(
        &state,
        state.upload.events_service.as_ref(),
        state.upload.notification_client.as_ref(),
        payload,
    )
    .await
    .into()
}

async fn mark_post_as_published_impl(
    state: &AppState,
    events_service: Option<&EventService>,
    notification_client: Option<&NotificationClient>,
    payload: MarkPostAsPublishedRequest,
) -> Result<(), AppError> {
    // Chain-verified sender (rejects forged delegation chains — see auth::verified_sender).
    let sender = verified_sender(&payload.delegated_identity_wire)?;

    let user_post_service = UserPostService(USER_POST_SERVICE_ID, &state.ic_agent);

    let post_details = match user_post_service
        .get_individual_post_details_by_id(payload.post_id.clone())
        .await?
    {
        Result2::Ok(post) => post,
        Result2::Err(e) => {
            return Err(AppError::PostNotFound(format!(
                "Error from user post service while fetching post details for post id {}: {e:?}",
                payload.post_id
            )));
        }
    };

    if sender != post_details.creator_principal {
        return Err(AppError::Unauthorized(format!(
            "The sender of the delegated identity is not the creator of the post. \
             Sender: {sender:?}, Post Creator: {:?}",
            post_details.creator_principal
        )));
    }

    user_post_service
        .update_post_status(payload.post_id.clone(), PostStatus::Uploaded)
        .await?;

    if let Some(events) = events_service {
        let _ = events
            .send_video_upload_successful_event(
                post_details.video_uid,
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
            .inspect_err(|e| tracing::error!("error sending video upload successful event {e}"));
    }

    if let Some(notif) = notification_client {
        notif
            .send_notification(
                NotificationType::VideoPublished {
                    user_principal: post_details.creator_principal,
                    post_id: payload.post_id.clone(),
                },
                post_details.creator_principal,
            )
            .await;
    }

    Ok(())
}
