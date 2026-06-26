pub mod auth;
pub mod events;
pub mod notification;
pub mod types;

#[cfg(test)]
pub mod test_support;

use crate::routes::upload::{events::EventService, notification::NotificationClient};

/// Upload-route dependencies that require secrets. Built tolerantly: if either token
/// is absent, `from_env`/`build` returns `None` and the upload routes are disabled
/// (return 503) rather than panicking the whole binary at startup (spec D9).
// allow(dead_code): fields are read by the handlers wired in later tasks.
#[allow(dead_code)]
#[derive(Clone)]
pub struct UploadState {
    pub events_service: EventService,
    pub notification_client: NotificationClient,
}

impl UploadState {
    /// Returns `None` if either token is missing.
    pub fn build(events_token: Option<String>, notif_token: Option<String>) -> Option<Self> {
        let (events_token, notif_token) = (events_token?, notif_token?);
        Some(Self {
            events_service: EventService::with_auth_token(events_token),
            notification_client: NotificationClient::new(notif_token),
        })
    }

    /// Build from the two token env vars; `None` (disabled) if either is unset.
    // allow(dead_code): wired into AppState in the next task.
    #[allow(dead_code)]
    pub fn from_env() -> Option<Self> {
        Self::build(
            std::env::var(crate::consts::OFFCHAIN_EVENTS_API_TOKEN).ok(),
            std::env::var(crate::consts::YRAL_METADATA_NOTIFICATION_SERVICE_API_TOKEN).ok(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_is_none_when_a_token_missing() {
        assert!(UploadState::build(None, None).is_none());
        assert!(UploadState::build(Some("a".into()), None).is_none());
        assert!(UploadState::build(None, Some("b".into())).is_none());
        assert!(UploadState::build(Some("a".into()), Some("b".into())).is_some());
    }
}
