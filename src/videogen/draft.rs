use crate::videogen::rate_limiter::RateLimiterRequestKey;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftCreationRequest {
    pub request_id: String,
    pub request_key: RateLimiterRequestKey,
    pub user_principal: String,
    pub video_id: String,
    pub object_key: String,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum DraftServiceError {
    #[error("draft service unavailable: {0}")]
    Unavailable(String),
    #[error("draft service rejected request: {0}")]
    Rejected(String),
}

#[async_trait::async_trait]
pub trait DraftServiceClient: Send + Sync {
    async fn create_draft(&self, request: DraftCreationRequest) -> Result<(), DraftServiceError>;
}

/// Stub implementation that logs and returns Ok. Used in production until the
/// actual draft endpoint is configured.
pub struct LoggingDraftServiceClient;

#[async_trait::async_trait]
impl DraftServiceClient for LoggingDraftServiceClient {
    async fn create_draft(&self, request: DraftCreationRequest) -> Result<(), DraftServiceError> {
        metrics::counter!(crate::videogen::metrics::DRAFT_CREATION_TOTAL).increment(1);
        tracing::info!(
            request_id = %request.request_id,
            principal = %request.request_key.principal,
            video_id = %request.video_id,
            "draft creation stub: would call upload service"
        );
        Ok(())
    }
}

#[cfg(test)]
pub mod test_helpers {
    use super::{DraftCreationRequest, DraftServiceClient, DraftServiceError};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    pub struct MockDraftServiceClient {
        pub calls: Arc<Mutex<Vec<DraftCreationRequest>>>,
        pub result: Option<Result<(), DraftServiceError>>,
    }

    impl MockDraftServiceClient {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn with_error(error: DraftServiceError) -> Self {
            Self {
                result: Some(Err(error)),
                ..Self::default()
            }
        }

        pub fn calls(&self) -> Vec<DraftCreationRequest> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl DraftServiceClient for MockDraftServiceClient {
        async fn create_draft(
            &self,
            request: DraftCreationRequest,
        ) -> Result<(), DraftServiceError> {
            self.calls.lock().unwrap().push(request);
            self.result.clone().unwrap_or_else(|| Ok(()))
        }
    }
}
