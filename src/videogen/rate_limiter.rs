/// Correlation ID for a video-generation request, threaded through Prakash ↔
/// Vast ↔ draft service. Previously assigned by the ICP rate_limits canister;
/// now generated locally by Prakash (counter = timestamp-based unique value).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
pub struct RateLimiterRequestKey {
    pub principal: String,
    pub counter: u64,
}
