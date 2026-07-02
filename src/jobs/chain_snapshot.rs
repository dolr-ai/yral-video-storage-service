//! Chain snapshot job: walk user_post_service.fetch_posts and stage yral_posts.
use chrono::{DateTime, Utc};
use ic_agent::Agent;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use yral_canisters_client::{
    ic::USER_POST_SERVICE_ID,
    user_post_service::{FetchPostsArgs, FetchPostsResult, SystemTime, UserPostService},
};

use crate::media_index::chain_repo::{self, ChainPost};

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
        svc.fetch_posts(FetchPostsArgs {
            limit,
            last_uuid_processed: cursor,
        })
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
    let stop =
        res.posts.is_empty() || next.is_none() || !advanced || (res.posts.len() as u64) < page;
    (stop, next)
}

const JOB_KIND: &str = "chain_snapshot";
const PAGE: u64 = 100;
const MAX_ITERS: u64 = 100_000;
const PAGE_DELAY_MS: u64 = 50;

#[derive(Debug, Clone)]
pub struct ChainSnapshotSummary {
    pub job_run_id: Uuid,
    pub posts_upserted: u64,
    pub pages: u64,
    pub skipped: u64,
    pub completed: bool,
}

/// Orchestrates a full (or backstopped/cancelled) walk of chain posts: pages
/// via `PostPageSource`, upserts each post into `yral_posts`, and tracks the
/// run in `media_job_runs`. Only a COMPLETE walk marks stale posts and
/// rebuilds the `yral_users` rollup — a partial/cancelled walk must never
/// tombstone rows it hasn't seen.
pub async fn run_chain_snapshot<S: PostPageSource>(
    source: &S,
    client: &mut tokio_postgres::Client,
    requested_by: &str,
    cancel: &CancellationToken,
    limit: Option<u64>,
) -> anyhow::Result<ChainSnapshotSummary> {
    let run_id = Uuid::new_v4();
    let job_kind: &str = JOB_KIND;
    client
        .execute(
            "INSERT INTO media_job_runs (id, job_kind, status, requested_by) VALUES ($1::TEXT::UUID,$2,'running',$3)",
            &[&run_id.to_string(), &job_kind, &requested_by],
        )
        .await?;

    let mut upserted = 0u64;
    let mut skipped = 0u64;

    let result = async {
        let mut cursor: Option<String> = None;
        let mut pages = 0u64;
        loop {
            if cancel.is_cancelled() {
                return Ok::<_, anyhow::Error>((pages, false));
            }
            if pages >= MAX_ITERS {
                return Ok((pages, false));
            }
            let res = source.fetch(PAGE, cursor.clone()).await?;
            pages += 1;
            for p in &res.posts {
                if p.video_uid.is_empty() {
                    skipped += 1;
                    continue;
                }
                let cp = ChainPost {
                    post_id: p.id.clone(),
                    video_uid: p.video_uid.clone(),
                    creator_principal: p.creator_principal.to_text(),
                    created_at: system_time_to_utc(&p.created_at),
                    status: format!("{:?}", p.status),
                };
                chain_repo::upsert_chain_post(client, &cp, run_id).await?;
                upserted += 1;
            }
            let totals = serde_json::json!({ "pages": pages, "posts_upserted": upserted, "skipped": skipped });
            let cursor_json = serde_json::json!({ "last": res.last_post_id_fetched });
            let _ = client
                .execute(
                    "UPDATE media_job_runs SET cursor=$2, totals=$3 WHERE id=$1::TEXT::UUID",
                    &[&run_id.to_string(), &cursor_json, &totals],
                )
                .await;
            // Bounded sample (preview/testing): once `limit` posts are upserted,
            // stop as a partial run — checked BEFORE walk_step so a deliberate cap
            // always yields completed=false (no mark-stale/rebuild; we didn't see
            // the whole corpus), even if this page would otherwise be a natural end.
            // Prod passes None for a real, complete coverage snapshot.
            if let Some(l) = limit {
                if upserted >= l {
                    return Ok((pages, false));
                }
            }
            let (stop, next) = walk_step(&cursor, &res, PAGE);
            if stop {
                return Ok((pages, true));
            }
            tokio::time::sleep(std::time::Duration::from_millis(PAGE_DELAY_MS)).await;
            cursor = next;
        }
    }
    .await;

    match result {
        Ok((pages, completed)) => {
            if completed {
                chain_repo::mark_stale_posts(client, run_id).await?;
                chain_repo::rebuild_yral_users(client).await?;
            }
            let totals = serde_json::json!({
                "pages": pages,
                "posts_upserted": upserted,
                "skipped": skipped,
                "completed": completed
            });
            let status: &str = if completed { "completed" } else { "partial" };
            client
                .execute(
                    "UPDATE media_job_runs SET status=$2, finished_at=NOW(), totals=$3, error_message=NULL WHERE id=$1::TEXT::UUID",
                    &[&run_id.to_string(), &status, &totals],
                )
                .await?;
            Ok(ChainSnapshotSummary {
                job_run_id: run_id,
                posts_upserted: upserted,
                pages,
                skipped,
                completed,
            })
        }
        Err(e) => {
            let _ = client
                .execute(
                    "UPDATE media_job_runs SET status='failed', finished_at=NOW(), error_message=$2 WHERE id=$1::TEXT::UUID",
                    &[&run_id.to_string(), &e.to_string()],
                )
                .await;
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::Principal;
    use yral_canisters_client::user_post_service::{Post, PostStatus, PostViewStatistics};

    #[test]
    fn converts_system_time_to_utc() {
        let st = SystemTime {
            secs_since_epoch: 1_700_000_000,
            nanos_since_epoch: 500_000_000,
        };
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
            created_at: SystemTime {
                secs_since_epoch: 0,
                nanos_since_epoch: 0,
            },
            likes: vec![],
            video_uid: video_uid.into(),
            view_stats: PostViewStatistics {
                total_view_count: 0,
                average_watch_percentage: 0,
                threshold_view_count: 0,
            },
            creator_principal: Principal::from_text(creator)
                .unwrap_or_else(|_| Principal::anonymous()),
        }
    }

    fn res(ids: &[&str], last: Option<&str>) -> FetchPostsResult {
        FetchPostsResult {
            posts: ids
                .iter()
                .map(|i| mkpost(i, i, "aaaaa-aa", PostStatus::Uploaded))
                .collect(),
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

    async fn setup() -> (
        crate::media_index::test_support::PgContainer,
        tokio_postgres::Client,
    ) {
        let (pg, client) = crate::media_index::test_support::test_client().await;
        crate::db::init_schema(&client).await.unwrap();
        crate::media_index::init_schema(&client).await.unwrap();
        (pg, client)
    }

    fn default_cancel() -> tokio_util::sync::CancellationToken {
        tokio_util::sync::CancellationToken::new()
    }

    // Mock page source; `fetch` takes &self so interior mutability (std Mutex) pops pages.
    struct MockSource {
        pages: std::sync::Mutex<Vec<FetchPostsResult>>,
    }

    #[async_trait::async_trait]
    impl PostPageSource for MockSource {
        async fn fetch(
            &self,
            _limit: u64,
            _cursor: Option<String>,
        ) -> anyhow::Result<FetchPostsResult> {
            let mut p = self.pages.lock().unwrap();
            if p.is_empty() {
                return Ok(FetchPostsResult {
                    posts: vec![],
                    last_post_id_fetched: None,
                });
            }
            Ok(p.remove(0))
        }
    }

    #[tokio::test]
    async fn snapshot_populates_posts_and_users_and_completes() {
        let (_pg, mut c) = setup().await;
        let src = MockSource {
            pages: std::sync::Mutex::new(vec![FetchPostsResult {
                posts: vec![mkpost("p1", "v1", "aaaaa-aa", PostStatus::Uploaded)],
                last_post_id_fetched: None,
            }]),
        };
        let summary = run_chain_snapshot(&src, &mut c, "test", &default_cancel(), None)
            .await
            .unwrap();
        assert_eq!(summary.posts_upserted, 1);
        assert!(summary.completed);
        let posts: i64 = c
            .query_one("SELECT count(*) FROM yral_posts WHERE NOT stale", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(posts, 1);
        let users: i64 = c
            .query_one("SELECT count(*) FROM yral_users", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(users, 1);
        let run: String = c
            .query_one(
                "SELECT status FROM media_job_runs WHERE id=$1::TEXT::UUID",
                &[&summary.job_run_id.to_string()],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(run, "completed");
    }

    #[tokio::test]
    async fn limited_snapshot_stops_early_and_is_partial() {
        let (_pg, mut c) = setup().await;
        // Two full pages available, but limit=1 stops after the first page.
        let src = MockSource {
            pages: std::sync::Mutex::new(vec![
                FetchPostsResult {
                    posts: vec![mkpost("p1", "v1", "aaaaa-aa", PostStatus::Uploaded)],
                    last_post_id_fetched: Some("p1".into()),
                },
                FetchPostsResult {
                    posts: vec![mkpost("p2", "v2", "aaaaa-aa", PostStatus::Uploaded)],
                    last_post_id_fetched: Some("p2".into()),
                },
            ]),
        };
        let summary = run_chain_snapshot(&src, &mut c, "test", &default_cancel(), Some(1))
            .await
            .unwrap();
        assert_eq!(summary.posts_upserted, 1);
        assert!(!summary.completed); // limited → partial
        // A partial run must NOT rebuild the users rollup (didn't see whole corpus).
        let users: i64 = c
            .query_one("SELECT count(*) FROM yral_users", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(users, 0);
        let run: String = c
            .query_one(
                "SELECT status FROM media_job_runs WHERE id=$1::TEXT::UUID",
                &[&summary.job_run_id.to_string()],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(run, "partial");
    }
}
