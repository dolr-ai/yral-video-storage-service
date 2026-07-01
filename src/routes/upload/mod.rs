pub mod auth;
pub mod draft_client;
pub mod events;
pub mod get_upload_url;
pub mod mark_post_as_published;
pub mod notification;
pub mod storj_finalize;
pub mod types;
pub mod update_video_metadata;

#[cfg(test)]
pub mod test_support;

use crate::routes::upload::{events::EventService, notification::NotificationClient};

/// Optional, best-effort side-effect clients for the upload routes. Each is built
/// independently from its token; an absent token disables ONLY that side-effect
/// (analytics event / push notification) — it never blocks the core publish flow
/// (Storj finalize + canister `add_post_v_1`), which needs no token. Missing tokens
/// never panic the binary (spec D9).
#[derive(Clone, Default)]
pub struct UploadState {
    pub events_service: Option<EventService>,
    pub notification_client: Option<NotificationClient>,
}

impl UploadState {
    /// Build each client from its env token; absent token → that client is `None`.
    pub fn from_env() -> Self {
        Self {
            events_service: std::env::var(crate::consts::OFFCHAIN_EVENTS_API_TOKEN)
                .ok()
                .map(EventService::with_auth_token),
            notification_client: std::env::var(
                crate::consts::YRAL_METADATA_NOTIFICATION_SERVICE_API_TOKEN,
            )
            .ok()
            .map(NotificationClient::new),
        }
    }
}
