//! Chain snapshot job: walk user_post_service.fetch_posts and stage yral_posts.
use chrono::{DateTime, Utc};
use ic_agent::Agent;
use yral_canisters_client::{
    ic::USER_POST_SERVICE_ID,
    user_post_service::{FetchPostsArgs, FetchPostsResult, SystemTime, UserPostService},
};

/// IC SystemTime (secs + nanos since epoch) → chrono UTC.
pub fn system_time_to_utc(st: &SystemTime) -> DateTime<Utc> {
    DateTime::from_timestamp(st.secs_since_epoch as i64, st.nanos_since_epoch)
        .unwrap_or_else(|| DateTime::from_timestamp(0, 0).unwrap())
}

/// Test seam: one page of posts. Implemented for the real UserPostService and
/// for a mock in tests, so the walk loop can be exercised without a live canister.
#[async_trait::async_trait]
pub trait PostPageSource {
    async fn fetch(&self, limit: u64, cursor: Option<String>) -> anyhow::Result<FetchPostsResult>;
}

/// Real page source backed by the live canister.
pub struct LivePostSource<'a>(pub &'a Agent);

#[async_trait::async_trait]
impl<'a> PostPageSource for LivePostSource<'a> {
    async fn fetch(&self, limit: u64, cursor: Option<String>) -> anyhow::Result<FetchPostsResult> {
        let svc = UserPostService(USER_POST_SERVICE_ID, self.0);
        svc.fetch_posts(FetchPostsArgs { limit, last_uuid_processed: cursor })
            .await
            .map_err(|e| anyhow::anyhow!("fetch_posts failed: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_system_time_to_utc() {
        let st = SystemTime { secs_since_epoch: 1_700_000_000, nanos_since_epoch: 500_000_000 };
        let dt = system_time_to_utc(&st);
        assert_eq!(dt.timestamp(), 1_700_000_000);
        assert_eq!(dt.timestamp_subsec_millis(), 500);
    }
}
