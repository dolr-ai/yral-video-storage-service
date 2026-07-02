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

/// Pure per-page decision. Returns `(stop, next_cursor)`. Stop on ANY of:
/// empty page, null/empty next cursor, non-advancing cursor (echoed prev), or
/// short page (`len < page`). The MAX_ITERS backstop lives in the caller.
pub fn walk_step(
    prev_cursor: &Option<String>,
    res: &FetchPostsResult,
    page: u64,
) -> (bool, Option<String>) {
    let next = res.last_post_id_fetched.clone().filter(|s| !s.is_empty());
    let advanced = next.is_some() && &next != prev_cursor;
    let stop = res.posts.is_empty() || next.is_none() || !advanced || (res.posts.len() as u64) < page;
    (stop, next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::Principal;
    use yral_canisters_client::user_post_service::{Post, PostStatus, PostViewStatistics};

    #[test]
    fn converts_system_time_to_utc() {
        let st = SystemTime { secs_since_epoch: 1_700_000_000, nanos_since_epoch: 500_000_000 };
        let dt = system_time_to_utc(&st);
        assert_eq!(dt.timestamp(), 1_700_000_000);
        assert_eq!(dt.timestamp_subsec_millis(), 500);
    }

    fn mkpost(id: &str, video_uid: &str, creator: &str, status: PostStatus) -> Post {
        Post {
            id: id.into(),
            status,
            share_count: 0,
            hashtags: vec![],
            description: String::new(),
            created_at: SystemTime { secs_since_epoch: 0, nanos_since_epoch: 0 },
            likes: vec![],
            video_uid: video_uid.into(),
            view_stats: PostViewStatistics {
                total_view_count: 0,
                average_watch_percentage: 0,
                threshold_view_count: 0,
            },
            creator_principal: Principal::from_text(creator).unwrap_or_else(|_| Principal::anonymous()),
        }
    }

    fn res(ids: &[&str], last: Option<&str>) -> FetchPostsResult {
        FetchPostsResult {
            posts: ids.iter().map(|i| mkpost(i, i, "aaaaa-aa", PostStatus::Uploaded)).collect(),
            last_post_id_fetched: last.map(|s| s.to_string()),
        }
    }

    #[test]
    fn stops_on_null_cursor() {
        let (stop, _) = walk_step(&Some("a".into()), &res(&["b"], None), 10);
        assert!(stop);
    }

    #[test]
    fn stops_on_empty_cursor() {
        let (stop, _) = walk_step(&Some("a".into()), &res(&["b"], Some("")), 10);
        assert!(stop);
    }

    #[test]
    fn stops_on_empty_page() {
        let (stop, _) = walk_step(&None, &res(&[], Some("z")), 10);
        assert!(stop);
    }

    #[test]
    fn stops_on_short_page() {
        let (stop, _) = walk_step(&None, &res(&["a"], Some("a")), 10);
        assert!(stop);
    }

    #[test]
    fn stops_on_non_advancing() {
        let (stop, _) = walk_step(&Some("a".into()), &res(&["a"], Some("a")), 1);
        assert!(stop);
    }

    #[test]
    fn continues_on_full_advance() {
        let (stop, next) = walk_step(&Some("a".into()), &res(&["b", "c"], Some("c")), 2);
        assert!(!stop);
        assert_eq!(next, Some("c".into()));
    }
}
