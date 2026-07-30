//! Post storage — the system of record for posts this service creates.
//!
//! See `docs/superpowers/specs/2026-07-29-canister-data-migration-design.md`.
//! The schema itself lives in `migrations/V2__posts.sql`; this module owns the
//! Rust side (types, repository, outbox, reconcile, API) in later tasks.
//!
//! Isolation rule: only `ic_sync.rs` may import `yral_canisters_client`, so
//! retiring the canister is a file deletion rather than a refactor.

#[cfg(test)]
mod schema_tests {
    use crate::media_index::test_support::test_client;
    use tokio_postgres::Client;

    async fn migrated() -> (crate::media_index::test_support::PgContainer, Client) {
        let (pg, mut c) = test_client().await;
        crate::migrations::run_migrations(&mut c).await.unwrap();
        (pg, c)
    }

    /// Minimal valid insert; `status`/`deleted_at` vary per test.
    async fn insert_post(
        c: &Client,
        post_id: &str,
        status: &str,
        deleted_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<u64, tokio_postgres::Error> {
        c.execute(
            "INSERT INTO posts (post_id, video_uid, creator_principal, status, origin, created_at, deleted_at)
             VALUES ($1, $1, 'aaaaa-aa', $2, 'upload', NOW(), $3)",
            &[&post_id, &status, &deleted_at],
        )
        .await
    }

    async fn table_exists(c: &Client, name: &str) -> bool {
        c.query_one(
            "SELECT to_regclass('public.' || $1) IS NOT NULL AS ok",
            &[&name],
        )
        .await
        .unwrap()
        .get("ok")
    }

    async fn count(c: &Client, sql: &str) -> i64 {
        c.query_one(sql, &[]).await.unwrap().get(0)
    }

    /// Assert a statement failed for the *stated* reason.
    ///
    /// Plain `is_err()` is not good enough here: when these tests were first run
    /// against a database where `posts` did not exist yet, every `is_err()`
    /// assertion passed on `undefined_table` — the tests were green while
    /// testing nothing. Pinning the SQLSTATE makes a missing table a failure
    /// instead of a false pass.
    fn assert_sqlstate<T>(
        result: Result<T, tokio_postgres::Error>,
        expected: &tokio_postgres::error::SqlState,
        what: &str,
    ) {
        let err = match result {
            Ok(_) => panic!("{what}: expected rejection, statement succeeded"),
            Err(e) => e,
        };
        assert_eq!(
            err.code(),
            Some(expected),
            "{what}: wrong failure reason — {err}"
        );
    }

    fn assert_check_violation<T>(result: Result<T, tokio_postgres::Error>, what: &str) {
        assert_sqlstate(
            result,
            &tokio_postgres::error::SqlState::CHECK_VIOLATION,
            what,
        );
    }

    #[tokio::test]
    async fn v2_creates_all_five_tables() {
        let (_pg, c) = migrated().await;
        for t in ["posts", "post_likes", "post_outbox", "users", "post_events"] {
            assert!(table_exists(&c, t).await, "{t} must exist");
        }
    }

    // --- posts constraints -------------------------------------------------

    /// Spec § Deletion modelling: the constraint is ONE-directional. A deleted
    /// post with an unknown timestamp is legal (the chain records no deletion
    /// time, so every backfilled `Deleted` row has one); a `deleted_at` on a
    /// post that is not deleted is not.
    #[tokio::test]
    async fn deleted_consistency_is_one_directional() {
        let (_pg, c) = migrated().await;
        assert_check_violation(
            insert_post(&c, "p-live-ts", "Uploaded", Some(chrono::Utc::now())).await,
            "deleted_at on a non-Deleted post",
        );
        assert!(
            insert_post(&c, "p-del-nots", "Deleted", None).await.is_ok(),
            "Deleted with an unknown timestamp must be ACCEPTED (backfill case)"
        );
        assert!(
            insert_post(&c, "p-del-ts", "Deleted", Some(chrono::Utc::now()))
                .await
                .is_ok(),
            "Deleted with a known timestamp must be accepted"
        );
    }

    #[tokio::test]
    async fn status_check_rejects_unknown_variants() {
        let (_pg, c) = migrated().await;
        assert_check_violation(
            insert_post(&c, "p-bad", "NotAStatus", None).await,
            "unknown status variant",
        );
    }

    /// All eight candid variants must be storable — including the four with no
    /// live writer, which exist in data via sync_post_from_individual_canister.
    #[tokio::test]
    async fn status_check_accepts_all_eight_candid_variants() {
        let (_pg, c) = migrated().await;
        for (i, s) in [
            "Draft",
            "Uploaded",
            "Transcoding",
            "CheckingExplicitness",
            "ReadyToView",
            "Deleted",
            "BannedForExplicitness",
            "BannedDueToUserReporting",
        ]
        .iter()
        .enumerate()
        {
            insert_post(&c, &format!("p{i}"), s, None)
                .await
                .unwrap_or_else(|e| panic!("status {s} must be storable: {e}"));
        }
    }

    #[tokio::test]
    async fn origin_check_rejects_unknown_values() {
        let (_pg, c) = migrated().await;
        let r = c
            .execute(
                "INSERT INTO posts (post_id, video_uid, creator_principal, status, origin, created_at)
                 VALUES ('p-o', 'p-o', 'aaaaa-aa', 'Draft', 'nonsense', NOW())",
                &[],
            )
            .await;
        assert_check_violation(r, "unknown origin value");
    }

    #[tokio::test]
    async fn watch_percentage_is_range_checked() {
        let (_pg, c) = migrated().await;
        insert_post(&c, "p-w", "Uploaded", None).await.unwrap();
        assert_check_violation(
            c.execute(
                "UPDATE posts SET average_watch_percentage = 101 WHERE post_id = 'p-w'",
                &[],
            )
            .await,
            "watch percentage above 100",
        );
        assert!(c
            .execute(
                "UPDATE posts SET average_watch_percentage = 100 WHERE post_id = 'p-w'",
                &[]
            )
            .await
            .is_ok());
    }

    /// A column that defaults on insert and is never touched reads like a
    /// freshness signal and silently is not one. Spec § `updated_at` maintenance.
    #[tokio::test]
    async fn updated_at_advances_on_update_for_posts_and_users() {
        let (_pg, c) = migrated().await;
        insert_post(&c, "p-u", "Draft", None).await.unwrap();
        c.execute("INSERT INTO users (principal) VALUES ('aaaaa-aa')", &[])
            .await
            .unwrap();

        // Force a measurable gap; the trigger uses NOW(), which is transaction time.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let bumped_post: bool = c
            .query_one(
                "WITH before AS (SELECT updated_at FROM posts WHERE post_id='p-u'),
                      upd AS (UPDATE posts SET description='x' WHERE post_id='p-u'
                              RETURNING updated_at)
                 SELECT upd.updated_at > before.updated_at FROM upd, before",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert!(bumped_post, "posts.updated_at must advance on UPDATE");

        let bumped_user: bool = c
            .query_one(
                "WITH before AS (SELECT updated_at FROM users WHERE principal='aaaaa-aa'),
                      upd AS (UPDATE users SET profile_picture_url='u' WHERE principal='aaaaa-aa'
                              RETURNING updated_at)
                 SELECT upd.updated_at > before.updated_at FROM upd, before",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert!(bumped_user, "users.updated_at must advance on UPDATE");
    }

    // --- post_likes / like_count trigger ------------------------------------

    #[tokio::test]
    async fn like_count_trigger_tracks_inserts_and_deletes() {
        let (_pg, c) = migrated().await;
        insert_post(&c, "p-l", "Uploaded", None).await.unwrap();

        for p in ["alice", "bob"] {
            c.execute(
                "INSERT INTO post_likes (post_id, principal) VALUES ('p-l', $1)",
                &[&p],
            )
            .await
            .unwrap();
        }
        assert_eq!(
            count(&c, "SELECT like_count FROM posts WHERE post_id='p-l'").await,
            2
        );

        c.execute(
            "DELETE FROM post_likes WHERE post_id='p-l' AND principal='alice'",
            &[],
        )
        .await
        .unwrap();
        assert_eq!(
            count(&c, "SELECT like_count FROM posts WHERE post_id='p-l'").await,
            1
        );
    }

    #[tokio::test]
    async fn post_likes_requires_an_existing_post() {
        let (_pg, c) = migrated().await;
        assert_sqlstate(
            c.execute(
                "INSERT INTO post_likes (post_id, principal) VALUES ('ghost', 'alice')",
                &[],
            )
            .await,
            &tokio_postgres::error::SqlState::FOREIGN_KEY_VIOLATION,
            "like on a nonexistent post",
        );
    }

    // --- post_outbox --------------------------------------------------------

    #[tokio::test]
    async fn outbox_status_check_allows_in_flight() {
        let (_pg, c) = migrated().await;
        for s in ["pending", "in_flight", "sent", "dead"] {
            c.execute(
                "INSERT INTO post_outbox (post_id, op, payload, status)
                 VALUES ('p', 'add_post', '{}'::jsonb, $1)",
                &[&s],
            )
            .await
            .unwrap_or_else(|e| panic!("outbox status {s} must be valid: {e}"));
        }
        assert_check_violation(
            c.execute(
                "INSERT INTO post_outbox (post_id, op, payload, status)
                 VALUES ('p', 'add_post', '{}'::jsonb, 'bogus')",
                &[],
            )
            .await,
            "unknown outbox status",
        );
    }

    #[tokio::test]
    async fn outbox_op_check_rejects_unknown_ops() {
        let (_pg, c) = migrated().await;
        assert_check_violation(
            c.execute(
                "INSERT INTO post_outbox (post_id, op, payload)
                 VALUES ('p', 'delete_everything', '{}'::jsonb)",
                &[],
            )
            .await,
            "unknown outbox op",
        );
    }

    /// Log-shaped tables must outlive the rows they describe, so neither carries
    /// an FK to `posts`. Spec § "Why post_events and post_outbox carry no FK".
    #[tokio::test]
    async fn outbox_and_events_have_no_post_fk() {
        let (_pg, c) = migrated().await;
        c.execute(
            "INSERT INTO post_outbox (post_id, op, payload)
             VALUES ('never-existed', 'add_post', '{}'::jsonb)",
            &[],
        )
        .await
        .expect("outbox row must survive without a posts row");
        c.execute(
            "INSERT INTO post_events (post_id, event_kind, payload)
             VALUES ('never-existed', 'created', '{}'::jsonb)",
            &[],
        )
        .await
        .expect("event row must survive without a posts row");
    }

    // --- indexes ------------------------------------------------------------

    /// The ordering guard depends on this predicate being `<> 'sent'` rather than
    /// `IN ('pending','in_flight')`; a dead op must keep blocking its successor.
    #[tokio::test]
    async fn outbox_unsent_index_predicate_excludes_only_sent() {
        let (_pg, c) = migrated().await;
        let def: String = c
            .query_one(
                "SELECT indexdef FROM pg_indexes WHERE indexname = 'idx_post_outbox_unsent'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert!(
            def.contains("<> 'sent'") || def.contains("''sent''"),
            "predicate must exclude only 'sent', got: {def}"
        );
    }

    #[tokio::test]
    async fn creator_partial_indexes_exist_with_expected_predicates() {
        let (_pg, c) = migrated().await;
        let visible: String = c
            .query_one(
                "SELECT indexdef FROM pg_indexes WHERE indexname='idx_posts_creator_visible'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        // BannedForExplicitness is deliberately NOT excluded — canister parity.
        assert!(!visible.contains("BannedForExplicitness"), "got: {visible}");
        for s in ["Draft", "Deleted", "BannedDueToUserReporting"] {
            assert!(visible.contains(s), "{s} must be excluded: {visible}");
        }

        assert!(c
            .query_opt(
                "SELECT 1 FROM pg_indexes WHERE indexname='idx_posts_creator_drafts'",
                &[]
            )
            .await
            .unwrap()
            .is_some());
    }
}
