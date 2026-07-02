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
