use chrono::{DateTime, Utc};
use tokio_postgres::Client;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct ChainPost {
    pub post_id: String,
    pub video_uid: String,
    pub creator_principal: String,
    pub created_at: DateTime<Utc>,
    pub status: String,
}

/// Idempotent upsert of one chain post. Re-writing the same post updates its
/// mutable `status`, stamps the current `snapshot_run_id`/`fetched_at`, and
/// clears `stale` (the post was seen again this run).
pub async fn upsert_chain_post(
    client: &Client,
    post: &ChainPost,
    run_id: Uuid,
) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            "INSERT INTO yral_posts
                 (post_id, video_uid, creator_principal, created_at, status, snapshot_run_id, fetched_at, stale)
             VALUES ($1, $2, $3, $4, $5, $6::TEXT::UUID, NOW(), FALSE)
             ON CONFLICT (post_id) DO UPDATE SET
                 video_uid = EXCLUDED.video_uid,
                 creator_principal = EXCLUDED.creator_principal,
                 created_at = EXCLUDED.created_at,
                 status = EXCLUDED.status,
                 snapshot_run_id = EXCLUDED.snapshot_run_id,
                 fetched_at = NOW(),
                 stale = FALSE",
            &[
                &post.post_id,
                &post.video_uid,
                &post.creator_principal,
                &post.created_at,
                &post.status,
                &run_id.to_string(),
            ],
        )
        .await?;
    Ok(())
}

/// After a COMPLETE walk: rows not touched by `run_id` were not seen this pass →
/// hard-deleted on chain. Flag them stale. Returns count flagged. NEVER call
/// after a partial/aborted walk (would tombstone live rows).
pub async fn mark_stale_posts(client: &Client, run_id: Uuid) -> Result<u64, tokio_postgres::Error> {
    client
        .execute(
            "UPDATE yral_posts SET stale = TRUE
             WHERE stale = FALSE AND snapshot_run_id IS DISTINCT FROM $1::TEXT::UUID",
            &[&run_id.to_string()],
        )
        .await
}

/// Recompute the derived rollup from non-stale posts.
pub async fn rebuild_yral_users(client: &Client) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            "INSERT INTO yral_users (creator_principal, post_count, first_seen, last_seen)
             SELECT creator_principal, count(*), min(created_at), max(created_at)
             FROM yral_posts WHERE NOT stale
             GROUP BY creator_principal
             ON CONFLICT (creator_principal) DO UPDATE SET
                 post_count = EXCLUDED.post_count,
                 first_seen = EXCLUDED.first_seen,
                 last_seen  = EXCLUDED.last_seen",
            &[],
        )
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Full DB setup. Bind the returned PgContainer for the whole test (do NOT `_`-drop the client's container).
    async fn setup() -> (
        crate::media_index::test_support::PgContainer,
        tokio_postgres::Client,
    ) {
        let (pg, client) = crate::media_index::test_support::test_client().await;
        crate::db::init_schema(&client).await.unwrap();
        crate::media_index::init_schema(&client).await.unwrap();
        (pg, client)
    }

    // Test helper: build a ChainPost quickly.
    fn post(id: &str, vid: &str, creator: &str, status: &str) -> ChainPost {
        ChainPost {
            post_id: id.into(),
            video_uid: vid.into(),
            creator_principal: creator.into(),
            created_at: chrono::Utc::now(),
            status: status.into(),
        }
    }

    #[tokio::test]
    async fn mark_stale_flags_posts_from_older_runs_only() {
        let (_pg, c) = setup().await;
        let old = uuid::Uuid::new_v4();
        let cur = uuid::Uuid::new_v4();
        upsert_chain_post(&c, &post("s-old", "vo", "ca", "Uploaded"), old)
            .await
            .unwrap();
        upsert_chain_post(&c, &post("s-cur", "vc", "ca", "Uploaded"), cur)
            .await
            .unwrap();
        let n = mark_stale_posts(&c, cur).await.unwrap();
        assert_eq!(n, 1);
        let stale_old: bool = c
            .query_one("SELECT stale FROM yral_posts WHERE post_id='s-old'", &[])
            .await
            .unwrap()
            .get(0);
        let stale_cur: bool = c
            .query_one("SELECT stale FROM yral_posts WHERE post_id='s-cur'", &[])
            .await
            .unwrap()
            .get(0);
        assert!(stale_old);
        assert!(!stale_cur);
    }

    #[tokio::test]
    async fn rebuild_users_excludes_stale_and_aggregates() {
        let (_pg, c) = setup().await;
        let run = uuid::Uuid::new_v4();
        upsert_chain_post(&c, &post("u1", "v1", "creatorX", "Uploaded"), run)
            .await
            .unwrap();
        upsert_chain_post(&c, &post("u2", "v2", "creatorX", "ReadyToView"), run)
            .await
            .unwrap();
        // a stale row for creatorX must NOT inflate the count
        upsert_chain_post(
            &c,
            &post("u3", "v3", "creatorX", "Deleted"),
            uuid::Uuid::new_v4(),
        )
        .await
        .unwrap();
        mark_stale_posts(&c, run).await.unwrap(); // flags u3 (older run)
        rebuild_yral_users(&c).await.unwrap();
        let cnt: i64 = c
            .query_one(
                "SELECT post_count FROM yral_users WHERE creator_principal='creatorX'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(cnt, 2);
    }

    #[tokio::test]
    async fn upsert_is_idempotent_and_updates_status_and_run() {
        let (_pg, c) = setup().await;
        let run1 = uuid::Uuid::new_v4();
        let p = ChainPost {
            post_id: "p1".into(),
            video_uid: "v1".into(),
            creator_principal: "creator-a".into(),
            created_at: chrono::Utc::now(),
            status: "Uploaded".into(),
        };
        upsert_chain_post(&c, &p, run1).await.unwrap();
        let run2 = uuid::Uuid::new_v4();
        let mut p2 = p.clone();
        p2.status = "ReadyToView".into();
        upsert_chain_post(&c, &p2, run2).await.unwrap();

        // tokio-postgres in this crate is not built with the `with-uuid-1`
        // feature (only `with-chrono-0_4` and `with-serde_json-1` are
        // enabled), so `Uuid` does not implement FromSql/ToSql here. Fetch
        // snapshot_run_id as TEXT and compare against the stringified UUID
        // instead of binding it as `uuid::Uuid` directly.
        let row = c
            .query_one(
                "SELECT status, snapshot_run_id::TEXT, stale FROM yral_posts WHERE post_id='p1'",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(row.get::<_, String>(0), "ReadyToView");
        assert_eq!(row.get::<_, String>(1), run2.to_string());
        assert_eq!(row.get::<_, bool>(2), false);
        let count: i64 = c
            .query_one("SELECT count(*) FROM yral_posts WHERE post_id='p1'", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(count, 1);
    }
}
