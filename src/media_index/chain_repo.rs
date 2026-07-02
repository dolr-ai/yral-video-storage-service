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

/// Read-only reconciliation of coverage-expected chain `video_uid`s against
/// our master + hashes + video_index tables. See module docs in the task
/// spec for the A-E category definitions.
#[derive(Debug, Clone, Default)]
pub struct ChainAuditReport {
    pub total_expected: i64,
    pub category_a: i64,
    pub category_b: i64,
    pub category_c: i64,
    pub category_d: i64,
    pub category_e: i64,
    pub excluded_by_status: i64,
    pub b_backing_off: i64,
}

const EXPECTED_STATUSES: &str = "('Uploaded','ReadyToView','Transcoding','CheckingExplicitness')";

/// Categorize every coverage-expected, non-stale distinct `video_uid` into
/// A (clean) / B (missing canonical pHash) / C (video_index only) /
/// D (nowhere) / E (in master but not servable), plus an `excluded_by_status`
/// count of video_uids whose posts are all Draft/Deleted/Banned*.
pub async fn chain_audit(client: &Client) -> Result<ChainAuditReport, tokio_postgres::Error> {
    let sql = format!(
        r#"
WITH expected AS (
    SELECT video_uid
    FROM yral_posts
    WHERE NOT stale
    GROUP BY video_uid
    HAVING bool_or(status IN {expected})
),
excluded AS (
    SELECT count(*) AS n FROM (
        SELECT video_uid FROM yral_posts WHERE NOT stale
        GROUP BY video_uid HAVING NOT bool_or(status IN {expected})
    ) t
),
cat AS (
    SELECT e.video_uid,
        m.video_id IS NOT NULL AS in_master,
        (m.servable_status = 'servable') AS servable,
        h.video_id IS NOT NULL AS has_hash,
        vi.video_id IS NOT NULL AS in_index,
        EXISTS (SELECT 1 FROM media_job_failures f
                WHERE f.video_id = e.video_uid
                  AND f.job_kind = 'media_phash'
                  AND f.next_retry_at > now()) AS backing_off
    FROM expected e
    LEFT JOIN all_servable_videos_on_yral m ON m.video_id = e.video_uid
    LEFT JOIN servable_video_hashes h
        ON h.video_id = e.video_uid
       AND h.hash_kind = 'phash'
       AND h.hash_version = 'offchain_binary_10x8_v1'
       AND h.input_media_version = 'current_stored_object_v1'
    LEFT JOIN video_index vi ON vi.video_id = e.video_uid
)
SELECT
  (SELECT count(*) FROM cat) AS total_expected,
  count(*) FILTER (WHERE in_master AND servable AND has_hash)          AS a,
  count(*) FILTER (WHERE in_master AND servable AND NOT has_hash)      AS b,
  count(*) FILTER (WHERE NOT in_master AND in_index)                   AS c,
  count(*) FILTER (WHERE NOT in_master AND NOT in_index)               AS d,
  count(*) FILTER (WHERE in_master AND NOT servable)                   AS e,
  (SELECT n FROM excluded)                                             AS excluded,
  count(*) FILTER (WHERE in_master AND servable AND NOT has_hash AND backing_off) AS b_backing_off
FROM cat
    "#,
        expected = EXPECTED_STATUSES
    );
    let row = client.query_one(&sql, &[]).await?;
    Ok(ChainAuditReport {
        total_expected: row.get("total_expected"),
        category_a: row.get("a"),
        category_b: row.get("b"),
        category_c: row.get("c"),
        category_d: row.get("d"),
        category_e: row.get("e"),
        excluded_by_status: row.get("excluded"),
        b_backing_off: row.get("b_backing_off"),
    })
}

/// Fraction (0.0-1.0) of a sample of expected video_uids that match a video_id
/// in master OR video_index. Low value ⇒ join-key skew ⇒ audit is meaningless.
pub async fn join_key_match_rate(client: &Client, sample: i64) -> Result<f64, tokio_postgres::Error> {
    let row = client
        .query_one(
            &format!(
                r#"
        WITH s AS (
            SELECT DISTINCT video_uid FROM yral_posts WHERE NOT stale
              AND status IN {expected} LIMIT $1
        )
        SELECT count(*) AS total,
               count(*) FILTER (WHERE
                   EXISTS (SELECT 1 FROM all_servable_videos_on_yral m WHERE m.video_id = s.video_uid)
                OR EXISTS (SELECT 1 FROM video_index vi WHERE vi.video_id = s.video_uid)) AS matched
        FROM s"#,
                expected = EXPECTED_STATUSES
            ),
            &[&sample],
        )
        .await?;
    let total: i64 = row.get("total");
    let matched: i64 = row.get("matched");
    Ok(if total == 0 { 1.0 } else { matched as f64 / total as f64 })
}

/// Up to `limit` category-D video_uids (expected, non-stale, not in master, not
/// in video_index) with one representative creator, for manual probing.
pub async fn category_d_sample(
    client: &Client,
    limit: i64,
) -> Result<Vec<(String, String)>, tokio_postgres::Error> {
    let sql = format!(
        r#"
        WITH expected AS (
            SELECT video_uid, min(creator_principal) AS creator
            FROM yral_posts WHERE NOT stale
            GROUP BY video_uid HAVING bool_or(status IN {expected})
        )
        SELECT e.video_uid, e.creator
        FROM expected e
        LEFT JOIN all_servable_videos_on_yral m ON m.video_id = e.video_uid
        LEFT JOIN video_index vi ON vi.video_id = e.video_uid
        WHERE m.video_id IS NULL AND vi.video_id IS NULL
        ORDER BY e.video_uid
        LIMIT $1"#,
        expected = EXPECTED_STATUSES
    );
    let rows = client.query(&sql, &[&limit]).await?;
    Ok(rows
        .iter()
        .map(|r| (r.get::<_, String>(0), r.get::<_, String>(1)))
        .collect())
}

/// Creators ranked by count of DISTINCT non-clean (B/C/D/E) expected videos they
/// authored. A mixed-creator video is charged to EVERY creator that authored an
/// expected post for it.
pub async fn worst_creators(
    client: &Client,
    limit: i64,
) -> Result<Vec<(String, i64)>, tokio_postgres::Error> {
    let sql = format!(
        r#"
        WITH expected AS (
            SELECT video_uid FROM yral_posts WHERE NOT stale
            GROUP BY video_uid HAVING bool_or(status IN {expected})
        ),
        non_clean AS (
            SELECT e.video_uid
            FROM expected e
            LEFT JOIN all_servable_videos_on_yral m ON m.video_id = e.video_uid
            LEFT JOIN servable_video_hashes h
                ON h.video_id = e.video_uid
               AND h.hash_kind='phash' AND h.hash_version='offchain_binary_10x8_v1'
               AND h.input_media_version='current_stored_object_v1'
            LEFT JOIN video_index vi ON vi.video_id = e.video_uid
            WHERE NOT (m.video_id IS NOT NULL AND m.servable_status='servable' AND h.video_id IS NOT NULL)
        )
        SELECT p.creator_principal, count(DISTINCT p.video_uid) AS n
        FROM yral_posts p
        JOIN non_clean nc ON nc.video_uid = p.video_uid
        WHERE NOT p.stale AND p.status IN {expected}
        GROUP BY p.creator_principal
        ORDER BY n DESC
        LIMIT $1"#,
        expected = EXPECTED_STATUSES
    );
    let rows = client.query(&sql, &[&limit]).await?;
    Ok(rows
        .iter()
        .map(|r| (r.get::<_, String>(0), r.get::<_, i64>(1)))
        .collect())
}

/// Clear ONLY the phash failure rows for the given video_ids so the media-phash
/// worker retries. Scoped to `job_kind='media_phash'` so unrelated import/other
/// failure rows are left intact. Returns rows deleted. (Category-B remediation.)
pub async fn clear_phash_failures(
    client: &Client,
    video_ids: &[String],
) -> Result<u64, tokio_postgres::Error> {
    if video_ids.is_empty() {
        return Ok(0);
    }
    client
        .execute(
            "DELETE FROM media_job_failures WHERE job_kind = 'media_phash' AND video_id = ANY($1)",
            &[&video_ids],
        )
        .await
}

/// The category-B video_ids (coverage-expected, non-stale, in master, servable,
/// missing the canonical hash tuple).
pub async fn category_b_video_ids(
    client: &Client,
    limit: i64,
) -> Result<Vec<String>, tokio_postgres::Error> {
    let sql = format!(
        r#"
        WITH expected AS (
            SELECT video_uid FROM yral_posts WHERE NOT stale
            GROUP BY video_uid HAVING bool_or(status IN {expected})
        )
        SELECT e.video_uid
        FROM expected e
        JOIN all_servable_videos_on_yral m ON m.video_id = e.video_uid AND m.servable_status = 'servable'
        LEFT JOIN servable_video_hashes h
            ON h.video_id = e.video_uid
           AND h.hash_kind='phash' AND h.hash_version='offchain_binary_10x8_v1'
           AND h.input_media_version='current_stored_object_v1'
        WHERE h.video_id IS NULL
        LIMIT $1"#,
        expected = EXPECTED_STATUSES
    );
    let rows = client.query(&sql, &[&limit]).await?;
    Ok(rows.iter().map(|r| r.get::<_, String>(0)).collect())
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

    async fn seed_master(c: &tokio_postgres::Client, video_id: &str, servable_status: &str) {
        c.execute(
            "INSERT INTO all_servable_videos_on_yral
                (video_id, source_kind, servable_status, storage_provider, object_key, discovered_from)
             VALUES ($1, 'test', $2, 'hetzner', $3, 'test')
             ON CONFLICT (video_id) DO UPDATE SET servable_status = EXCLUDED.servable_status",
            &[&video_id, &servable_status, &format!("videos/{video_id}.mp4")],
        )
        .await
        .unwrap();
    }

    async fn seed_canonical_hash(c: &tokio_postgres::Client, video_id: &str) {
        c.execute(
            "INSERT INTO servable_video_hashes
                (video_id, hash_kind, hash_version, input_media_version,
                 hash_value, hash_bit_length, num_frames, hash_size)
             VALUES ($1, 'phash', 'offchain_binary_10x8_v1', 'current_stored_object_v1', 'ff', 64, 1, 64)
             ON CONFLICT DO NOTHING",
            &[&video_id],
        )
        .await
        .unwrap();
    }

    async fn seed_video_index(c: &tokio_postgres::Client, video_id: &str) {
        c.execute(
            "INSERT INTO video_index (video_id, storj_key) VALUES ($1, $2) ON CONFLICT (video_id) DO NOTHING",
            &[&video_id, &format!("creator/{video_id}.mp4")],
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn audit_categorizes_all_five() {
        let (_pg, c) = setup().await;
        let run = uuid::Uuid::new_v4();
        seed_master(&c, "A", "servable").await;
        seed_canonical_hash(&c, "A").await;
        seed_master(&c, "B", "servable").await;
        seed_video_index(&c, "C").await;
        // D: nowhere
        seed_master(&c, "E", "unservable").await;
        seed_canonical_hash(&c, "E").await;
        for v in ["A", "B", "C", "D", "E"] {
            upsert_chain_post(&c, &post(&format!("p{v}"), v, "cc", "Uploaded"), run)
                .await
                .unwrap();
        }
        upsert_chain_post(&c, &post("pX", "X", "cc", "Deleted"), run)
            .await
            .unwrap(); // excluded
        let rep = chain_audit(&c).await.unwrap();
        assert_eq!(rep.category_a, 1);
        assert_eq!(rep.category_b, 1);
        assert_eq!(rep.category_c, 1);
        assert_eq!(rep.category_d, 1);
        assert_eq!(rep.category_e, 1);
        assert_eq!(rep.excluded_by_status, 1);
    }

    #[tokio::test]
    async fn mixed_status_video_is_expected_if_any_expected() {
        let (_pg, c) = setup().await;
        let run = uuid::Uuid::new_v4();
        seed_master(&c, "vm", "servable").await; // in master, no hash → B if expected
        upsert_chain_post(&c, &post("pm1", "vm", "cc", "Deleted"), run)
            .await
            .unwrap();
        upsert_chain_post(&c, &post("pm2", "vm", "cc", "ReadyToView"), run)
            .await
            .unwrap();
        let rep = chain_audit(&c).await.unwrap();
        assert_eq!(rep.category_b, 1);
        assert_eq!(rep.excluded_by_status, 0);
    }

    #[tokio::test]
    async fn non_canonical_hash_does_not_satisfy_a() {
        let (_pg, c) = setup().await;
        let run = uuid::Uuid::new_v4();
        seed_master(&c, "vh", "servable").await;
        c.execute(
            "INSERT INTO servable_video_hashes \
            (video_id, hash_kind, hash_version, input_media_version, hash_value, hash_bit_length, num_frames, hash_size) \
            VALUES ('vh','phash','SOME_OTHER_VERSION','current_stored_object_v1','ff',64,1,64)",
            &[],
        )
        .await
        .unwrap();
        upsert_chain_post(&c, &post("ph", "vh", "cc", "Uploaded"), run)
            .await
            .unwrap();
        let rep = chain_audit(&c).await.unwrap();
        assert_eq!(rep.category_a, 0);
        assert_eq!(rep.category_b, 1);
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

    #[tokio::test]
    async fn join_key_gate_passes_when_matches_high() {
        let (_pg, c) = setup().await;
        let run = uuid::Uuid::new_v4();
        for v in ["m1", "m2", "m3"] {
            seed_master(&c, v, "servable").await;
            upsert_chain_post(&c, &post(&format!("p{v}"), v, "cc", "Uploaded"), run)
                .await
                .unwrap();
        }
        assert!(join_key_match_rate(&c, 200).await.unwrap() >= 0.99);
    }

    #[tokio::test]
    async fn join_key_gate_flags_when_matches_low() {
        let (_pg, c) = setup().await;
        let run = uuid::Uuid::new_v4();
        for v in ["x1", "x2"] {
            upsert_chain_post(&c, &post(&format!("p{v}"), v, "cc", "Uploaded"), run)
                .await
                .unwrap();
        }
        assert_eq!(join_key_match_rate(&c, 200).await.unwrap(), 0.0);
    }

    #[tokio::test]
    async fn remediate_b_clears_only_phash_failures() {
        let (_pg, c) = setup().await;
        // a phash failure row AND an unrelated import failure row for the same video
        c.execute("INSERT INTO media_job_failures (job_kind, item_key, phase, last_error, next_retry_at) \
                   VALUES ('media_phash','v1','download','x', now()+interval '1 day')", &[]).await.unwrap();
        c.execute("UPDATE media_job_failures SET video_id='v1' WHERE item_key='v1'", &[]).await.unwrap();
        c.execute("INSERT INTO media_job_failures (job_kind, item_key, phase, last_error, video_id) \
                   VALUES ('legacy_video_index_import','v1','import','y','v1')", &[]).await.unwrap();
        let n = clear_phash_failures(&c, &["v1".to_string()]).await.unwrap();
        assert_eq!(n, 1); // only the media_phash row
        let import_left: i64 = c.query_one("SELECT count(*) FROM media_job_failures WHERE job_kind='legacy_video_index_import'", &[]).await.unwrap().get(0);
        assert_eq!(import_left, 1); // untouched
    }

    #[tokio::test]
    async fn d_sample_and_worst_creators_returned() {
        let (_pg, c) = setup().await;
        let run = uuid::Uuid::new_v4();
        upsert_chain_post(&c, &post("pD", "vD", "cD", "Uploaded"), run)
            .await
            .unwrap(); // D: nowhere
        let d = category_d_sample(&c, 100).await.unwrap();
        assert!(d.iter().any(|(v, cr)| v == "vD" && cr == "cD"));
        let worst = worst_creators(&c, 50).await.unwrap();
        assert!(worst.iter().any(|(cr, n)| cr == "cD" && *n >= 1));
    }
}
