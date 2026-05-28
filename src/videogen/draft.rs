use crate::videogen::rate_limiter::RateLimiterRequestKey;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftCreationRequest {
    pub request_id: String,
    pub request_key: RateLimiterRequestKey,
    pub user_principal: String,
    pub video_id: String,
    pub object_key: String,
}
