//! In-process `DraftServiceClient` — calls the merged `update_metadata_impl`
//! directly instead of POSTing to upload.yral.com (Phase 2). Lives in the bin crate
//! because it needs `AppState`; the `DraftServiceClient` trait it implements is
//! defined in the lib (`crate::videogen::draft`).

use yral_canisters_client::user_post_service::{PostDetailsFromFrontendV1, PostStatusFromFrontend};

use crate::routes::upload::{
    events::EventService,
    notification::NotificationClient,
    update_video_metadata::{update_metadata_impl, UpdateMetadataRequest},
};
use crate::videogen::draft::{DraftCreationRequest, DraftServiceClient, DraftServiceError};
use crate::AppState;

// allow(dead_code): constructed by RuntimeCompletionDeps in Task 2.2b.
#[allow(dead_code)]
pub struct InProcessDraftServiceClient {
    state: AppState,
    events: EventService,
    notif: NotificationClient,
}

#[allow(dead_code)]
impl InProcessDraftServiceClient {
    pub fn new(state: AppState, events: EventService, notif: NotificationClient) -> Self {
        Self {
            state,
            events,
            notif,
        }
    }
}

/// Pure mapping: a draft registers a `Draft`-status post whose id and video_uid are
/// the generated `video_id`, owned by `user_principal`. No hashtags/description.
fn draft_post_details(
    video_id: &str,
    user_principal: &str,
) -> Result<PostDetailsFromFrontendV1, DraftServiceError> {
    let creator_principal = candid::Principal::from_text(user_principal)
        .map_err(|e| DraftServiceError::Rejected(format!("invalid user principal: {e}")))?;
    Ok(PostDetailsFromFrontendV1 {
        id: video_id.to_string(),
        video_uid: video_id.to_string(),
        creator_principal,
        status: PostStatusFromFrontend::Draft,
        hashtags: vec![],
        description: String::new(),
    })
}

#[async_trait::async_trait]
impl DraftServiceClient for InProcessDraftServiceClient {
    async fn create_draft(&self, request: DraftCreationRequest) -> Result<(), DraftServiceError> {
        let Some(encrypted_identity) = request.encrypted_identity.as_ref() else {
            tracing::warn!(
                request_id = %request.request_id,
                video_id = %request.video_id,
                "no encrypted_identity — skipping draft registration"
            );
            return Ok(());
        };

        let identity_wire = crate::videogen::identity_crypto::IdentityCrypto::from_env()
            .and_then(|c| c.decrypt(encrypted_identity))
            .map_err(|e| DraftServiceError::Unavailable(format!("identity decrypt failed: {e}")))?;

        let post_details = draft_post_details(&request.video_id, &request.user_principal)?;

        let req = UpdateMetadataRequest {
            delegated_identity_wire: identity_wire,
            meta: std::collections::HashMap::new(),
            post_details,
        };

        tracing::info!(
            request_id = %request.request_id,
            video_id = %request.video_id,
            "registering draft in-process via update_metadata_impl"
        );

        update_metadata_impl(&self.state, &self.events, &self.notif, req)
            .await
            .map_err(|e| DraftServiceError::Unavailable(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draft_post_details_maps_to_draft_status() {
        let pd = draft_post_details("vid-1", "aaaaa-aa").expect("valid principal");
        assert_eq!(pd.id, "vid-1");
        assert_eq!(pd.video_uid, "vid-1");
        assert!(matches!(pd.status, PostStatusFromFrontend::Draft));
        assert!(pd.hashtags.is_empty());
        assert!(pd.description.is_empty());
    }

    #[test]
    fn draft_post_details_rejects_bad_principal() {
        assert!(draft_post_details("vid", "not a principal").is_err());
    }
}
