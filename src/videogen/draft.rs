use crate::videogen::rate_limiter::RateLimiterRequestKey;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftCreationRequest {
    pub request_id: String,
    pub request_key: RateLimiterRequestKey,
    pub user_principal: String,
    pub video_id: String,
    pub object_key: String,
    /// AES-256-GCM encrypted `DelegatedIdentityWire`.
    /// Required by the upload service for auth and Storj finalization.
    pub encrypted_identity: Option<String>,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[allow(dead_code)]
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
