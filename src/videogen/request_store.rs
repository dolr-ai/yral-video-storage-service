//! Local store for video-generation requests.
//!
//! Replaces the ICP `rate_limits` canister as the record of an in-flight generation.
//! The canister was the only place a request existed between `/generate` (submit to
//! Vast) and `/complete` (the callback that registers the draft), which is why
//! `/drafts/in-progress` had no data source once it was removed.
//!
//! Lifecycle: `insert_pending` at submit → `mark_complete` / `mark_failed` from the
//! completion callback or a failed submit → `expire_stale_for_principal` closes rows
//! whose callback never arrived → `purge_for_principal` deletes rows past retention.
//! The last two run on the `/drafts/in-progress` read path and are scoped to the
//! caller, so a polled endpoint never sweeps the whole table.
//!
//! `counter` is a global `BIGSERIAL`: unique and monotonic, so it replaces the
//! canister-issued per-principal counter in `operation_id` (`<principal>_<counter>`)
//! without collisions. Rows are always addressed by `(principal, counter)` so a
//! guessed counter cannot touch another user's request.
//!
//! Every write helper here is best-effort by design (`*_best_effort`): generation
//! must not fail because Postgres is unreachable. A dropped write degrades the
//! in-progress list, nothing else, and is logged at ERROR.

use tokio_postgres::Client;

/// Grace added to the LTX generation timeout before a pending row is called
/// abandoned. Without it the sweep fires at the same moment Vast gives up, races the
/// timeout-failure callback, and can clear the row while a late completion is still
/// in flight — mobile stops polling as soon as the list is empty, so a premature
/// sweep makes the user's in-progress video disappear.
const STALE_MARGIN_SECS: i64 = 600;
/// Fallback when the generation timeout is unset/unparseable, matching
/// `VideogenConfig::ltx_generation_timeout_secs`.
const DEFAULT_GENERATION_TIMEOUT_SECS: i64 = 1800;
const STALE_AFTER_SECS_ENV: &str = "VIDEOGEN_REQUEST_STALE_SECS";

/// Cap on rows returned by the in-progress list. A user cannot legitimately have
/// this many generations in flight; the bound keeps a runaway client from producing
/// an unbounded response.
const IN_PROGRESS_LIMIT: i64 = 50;

/// Terminal rows older than this are deleted. Prompts are user content that is
/// redacted from every log in this service, so they are not kept indefinitely just
/// because a request happened to complete. Override with
/// `VIDEOGEN_REQUEST_RETENTION_DAYS`.
const DEFAULT_RETENTION_DAYS: i64 = 30;
const RETENTION_DAYS_ENV: &str = "VIDEOGEN_REQUEST_RETENTION_DAYS";

pub const SCHEMA_SQL: &str = r#"
-- Two instances can boot concurrently; CREATE ... IF NOT EXISTS is not atomic
-- against a concurrent creator (it raises a duplicate pg_type key). Serialize the
-- whole schema application, same as media_index::schema.
SELECT pg_advisory_xact_lock(904648332137142901);

CREATE TABLE IF NOT EXISTS videogen_requests (
    counter        BIGSERIAL PRIMARY KEY,
    principal      TEXT NOT NULL,
    request_id     TEXT NOT NULL,
    model_id       TEXT NOT NULL,
    prompt         TEXT NOT NULL,
    status         TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending','complete','failed')),
    video_id       TEXT,
    bucket_url     TEXT,
    failure_reason TEXT,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Serves the in-progress lookup and the per-principal staleness sweep, both of
-- which filter (principal, status) and order by created_at.
CREATE INDEX IF NOT EXISTS idx_videogen_requests_principal_status
    ON videogen_requests (principal, status, created_at DESC);

-- Correlating a Vast request id back to its row (debugging) must not seq scan.
CREATE INDEX IF NOT EXISTS idx_videogen_requests_request_id
    ON videogen_requests (request_id);

-- updated_at is maintained by trigger, not by each UPDATE, so a future write
-- cannot forget it.
CREATE OR REPLACE FUNCTION videogen_requests_touch_updated_at()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    NEW.updated_at = NOW();
    RETURN NEW;
END;
$$;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM pg_trigger
        WHERE tgname = 'videogen_requests_touch_updated_at'
          AND tgrelid = 'videogen_requests'::regclass
    ) THEN
        CREATE TRIGGER videogen_requests_touch_updated_at
            BEFORE UPDATE ON videogen_requests
            FOR EACH ROW EXECUTE FUNCTION videogen_requests_touch_updated_at();
    END IF;
END;
$$;
"#;

pub async fn init_schema(client: &Client) -> Result<(), tokio_postgres::Error> {
    client.batch_execute(SCHEMA_SQL).await
}

/// Seconds after which a still-`pending` row is considered abandoned. Derived from the
/// generation timeout so raising `VIDEOGEN_LTX_GENERATION_TIMEOUT_SECS` moves the sweep
/// with it; `VIDEOGEN_REQUEST_STALE_SECS` overrides outright.
pub fn stale_after_secs() -> i64 {
    stale_after_secs_from(
        read_positive_env(STALE_AFTER_SECS_ENV),
        read_positive_env(crate::consts::VIDEOGEN_LTX_GENERATION_TIMEOUT_SECS),
    )
}

fn stale_after_secs_from(explicit: Option<i64>, generation_timeout: Option<i64>) -> i64 {
    explicit.unwrap_or_else(|| {
        generation_timeout.unwrap_or(DEFAULT_GENERATION_TIMEOUT_SECS) + STALE_MARGIN_SECS
    })
}

fn read_positive_env(name: &str) -> Option<i64> {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
}

/// Days a row is kept before deletion.
pub fn retention_days() -> i64 {
    read_positive_env(RETENTION_DAYS_ENV).unwrap_or(DEFAULT_RETENTION_DAYS)
}

#[derive(Debug, Clone)]
pub struct NewRequest<'a> {
    pub principal: &'a str,
    pub request_id: &'a str,
    pub model_id: &'a str,
    pub prompt: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InProgressRow {
    pub counter: i64,
    pub model_id: String,
    pub prompt: String,
    /// RFC 3339 UTC, seconds precision.
    pub created_at: String,
}

/// Insert a `pending` row and return its DB-assigned `counter`.
pub async fn insert_pending(
    client: &Client,
    req: NewRequest<'_>,
) -> Result<i64, tokio_postgres::Error> {
    let row = client
        .query_one(
            "INSERT INTO videogen_requests (principal, request_id, model_id, prompt)
             VALUES ($1, $2, $3, $4)
             RETURNING counter",
            &[&req.principal, &req.request_id, &req.model_id, &req.prompt],
        )
        .await?;
    Ok(row.get(0))
}

/// Close a request as `complete`. Returns rows affected: 0 means no such request for
/// this principal (e.g. the pending insert was dropped because the DB was down).
pub async fn mark_complete(
    client: &Client,
    principal: &str,
    counter: i64,
    video_id: &str,
    bucket_url: &str,
) -> Result<u64, tokio_postgres::Error> {
    client
        .execute(
            "UPDATE videogen_requests
             SET status = 'complete',
                 video_id = $3,
                 bucket_url = $4
             WHERE principal = $1 AND counter = $2 AND status = 'pending'",
            &[&principal, &counter, &video_id, &bucket_url],
        )
        .await
}

/// Close a request as `failed`. Returns rows affected (see [`mark_complete`]).
pub async fn mark_failed(
    client: &Client,
    principal: &str,
    counter: i64,
    reason: &str,
) -> Result<u64, tokio_postgres::Error> {
    client
        .execute(
            "UPDATE videogen_requests
             SET status = 'failed',
                 failure_reason = $3
             WHERE principal = $1 AND counter = $2 AND status = 'pending'",
            &[&principal, &counter, &reason],
        )
        .await
}

/// Close one principal's `pending` rows whose completion callback never arrived.
/// Returns rows swept.
///
/// Scoped to a principal on purpose: this runs on the `/drafts/in-progress` read
/// path, which clients poll. An unscoped sweep could not use
/// `idx_videogen_requests_principal_status` (its leading column is `principal`), so it
/// would seq scan and take row locks across other users on every poll.
pub async fn expire_stale_for_principal(
    client: &Client,
    principal: &str,
    stale_after_secs: i64,
) -> Result<u64, tokio_postgres::Error> {
    client
        .execute(
            "UPDATE videogen_requests
             SET status = 'failed',
                 failure_reason = 'no completion callback within ' || $2::bigint || 's'
             WHERE principal = $1
               AND status = 'pending'
               AND created_at < NOW() - ($2::bigint * INTERVAL '1 second')",
            &[&principal, &stale_after_secs],
        )
        .await
}

/// Delete one principal's rows past the retention window. Returns rows deleted.
/// Scoped for the same reason as [`expire_stale_for_principal`].
///
/// Deletes `pending` rows too, not just terminal ones: a request still pending days
/// after retention is dead whatever its status says, and the staleness sweep only runs
/// when that user polls — so without this a user who generates once and never opens
/// the drafts screen would leave a row behind permanently.
pub async fn purge_for_principal(
    client: &Client,
    principal: &str,
    retention_days: i64,
) -> Result<u64, tokio_postgres::Error> {
    client
        .execute(
            "DELETE FROM videogen_requests
             WHERE principal = $1
               AND created_at < NOW() - ($2::bigint * INTERVAL '1 day')",
            &[&principal, &retention_days],
        )
        .await
}

/// In-progress requests for one principal, newest first.
pub async fn list_pending(
    client: &Client,
    principal: &str,
) -> Result<Vec<InProgressRow>, tokio_postgres::Error> {
    let rows = client
        .query(
            "SELECT counter, model_id, prompt, created_at
             FROM videogen_requests
             WHERE principal = $1 AND status = 'pending'
             ORDER BY created_at DESC
             LIMIT $2",
            &[&principal, &IN_PROGRESS_LIMIT],
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let created_at: chrono::DateTime<chrono::Utc> = row.get("created_at");
            InProgressRow {
                counter: row.get("counter"),
                model_id: row.get("model_id"),
                prompt: row.get("prompt"),
                created_at: created_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            }
        })
        .collect())
}

// ─── Best-effort wrappers (connect + log, never propagate) ───────────────────

/// Record a submitted request. `None` means it was not persisted — the caller must
/// still be able to generate, so it falls back to a locally minted counter and the
/// request simply will not appear in the in-progress list.
pub async fn record_pending_best_effort(db_url: &str, req: NewRequest<'_>) -> Option<i64> {
    let client = match crate::db::connect(db_url).await {
        Ok(client) => client,
        Err(error) => {
            tracing::error!(
                principal = req.principal,
                request_id = req.request_id,
                error = %error,
                "videogen request store: db connect failed, request will not appear in-progress"
            );
            return None;
        }
    };
    match insert_pending(&client, req.clone()).await {
        Ok(counter) => Some(counter),
        Err(error) => {
            tracing::error!(
                principal = req.principal,
                request_id = req.request_id,
                error = %error,
                "videogen request store: insert_pending failed, request will not appear in-progress"
            );
            None
        }
    }
}

/// Terminal state for a request, from the completion callback or a failed submit.
#[derive(Debug, Clone)]
pub enum Terminal<'a> {
    Complete {
        video_id: &'a str,
        bucket_url: &'a str,
    },
    Failed {
        reason: &'a str,
    },
}

pub async fn record_terminal_best_effort(
    db_url: &str,
    principal: &str,
    counter: u64,
    terminal: Terminal<'_>,
) {
    // Counters from the store are always in i64 range; a `fallback_counter()` value
    // is not necessarily, and wrapping it negative would put a nonsense counter in
    // the log line below. Saturate instead — either way no row matches.
    let counter = i64::try_from(counter).unwrap_or(i64::MAX);
    let client = match crate::db::connect(db_url).await {
        Ok(client) => client,
        Err(error) => {
            tracing::error!(
                principal,
                counter,
                error = %error,
                "videogen request store: db connect failed, request left pending until swept"
            );
            return;
        }
    };
    let result = match terminal {
        Terminal::Complete {
            video_id,
            bucket_url,
        } => mark_complete(&client, principal, counter, video_id, bucket_url).await,
        Terminal::Failed { reason } => mark_failed(&client, principal, counter, reason).await,
    };
    match result {
        // A pending row is missing whenever the insert was dropped (DB was down at
        // submit) or the sweep already closed it. Not an error for the caller, but
        // worth seeing: it means the user's in-progress entry was never accurate.
        Ok(0) => tracing::warn!(
            principal,
            counter,
            "videogen request store: no pending row to close"
        ),
        Ok(_) => {}
        Err(error) => tracing::error!(
            principal,
            counter,
            error = %error,
            "videogen request store: terminal update failed, request left pending until swept"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_index::test_support::test_client;

    async fn store_client() -> (crate::media_index::test_support::PgContainer, Client) {
        let (pg, client) = test_client().await;
        init_schema(&client).await.expect("schema");
        (pg, client)
    }

    fn new_request<'a>(principal: &'a str, request_id: &'a str) -> NewRequest<'a> {
        NewRequest {
            principal,
            request_id,
            model_id: "ltx2",
            prompt: "a sunrise over mountains",
        }
    }

    #[tokio::test]
    async fn pending_request_is_listed_then_disappears_once_complete() {
        let (_pg, client) = store_client().await;

        let counter = insert_pending(&client, new_request("alice", "req-1"))
            .await
            .expect("insert");

        let listed = list_pending(&client, "alice").await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].counter, counter);
        assert_eq!(listed[0].model_id, "ltx2");
        assert_eq!(listed[0].prompt, "a sunrise over mountains");

        let updated = mark_complete(
            &client,
            "alice",
            counter,
            "vid-1",
            "https://bucket/vid-1.mp4",
        )
        .await
        .expect("complete");
        assert_eq!(updated, 1);
        assert!(list_pending(&client, "alice")
            .await
            .expect("list")
            .is_empty());
    }

    #[tokio::test]
    async fn failed_request_disappears_and_keeps_its_reason() {
        let (_pg, client) = store_client().await;
        let counter = insert_pending(&client, new_request("alice", "req-1"))
            .await
            .expect("insert");

        assert_eq!(
            mark_failed(&client, "alice", counter, "Vast submit failed: timeout")
                .await
                .expect("fail"),
            1
        );

        assert!(list_pending(&client, "alice")
            .await
            .expect("list")
            .is_empty());
        let row = client
            .query_one(
                "SELECT status, failure_reason FROM videogen_requests WHERE counter = $1",
                &[&counter],
            )
            .await
            .expect("row");
        assert_eq!(row.get::<_, String>("status"), "failed");
        assert_eq!(
            row.get::<_, Option<String>>("failure_reason").as_deref(),
            Some("Vast submit failed: timeout")
        );
    }

    #[tokio::test]
    async fn one_users_requests_are_not_visible_to_another() {
        let (_pg, client) = store_client().await;
        let alice = insert_pending(&client, new_request("alice", "req-1"))
            .await
            .expect("insert");
        insert_pending(&client, new_request("bob", "req-2"))
            .await
            .expect("insert");

        let bobs = list_pending(&client, "bob").await.expect("list");
        assert_eq!(bobs.len(), 1);
        assert_ne!(bobs[0].counter, alice);

        // A guessed counter belonging to another principal must not be closable.
        assert_eq!(
            mark_complete(&client, "bob", alice, "vid-x", "https://bucket/vid-x.mp4")
                .await
                .expect("complete"),
            0
        );
        assert_eq!(list_pending(&client, "alice").await.expect("list").len(), 1);
    }

    #[tokio::test]
    async fn stale_pending_requests_are_swept_but_fresh_ones_survive() {
        let (_pg, client) = store_client().await;
        let stale = insert_pending(&client, new_request("alice", "old"))
            .await
            .expect("insert");
        let fresh = insert_pending(&client, new_request("alice", "new"))
            .await
            .expect("insert");
        client
            .execute(
                "UPDATE videogen_requests SET created_at = NOW() - INTERVAL '2 hours' WHERE counter = $1",
                &[&stale],
            )
            .await
            .expect("age the row");

        assert_eq!(
            expire_stale_for_principal(&client, "alice", 1800)
                .await
                .expect("sweep"),
            1
        );

        let listed = list_pending(&client, "alice").await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].counter, fresh);
    }

    #[tokio::test]
    async fn sweep_does_not_touch_another_principals_rows() {
        // The sweep runs on a polled read path, so it must only ever touch the
        // caller's rows — no cross-user writes or locks.
        let (_pg, client) = store_client().await;
        let bobs = insert_pending(&client, new_request("bob", "old"))
            .await
            .expect("insert");
        client
            .execute(
                "UPDATE videogen_requests SET created_at = NOW() - INTERVAL '2 hours' WHERE counter = $1",
                &[&bobs],
            )
            .await
            .expect("age the row");

        assert_eq!(
            expire_stale_for_principal(&client, "alice", 1800)
                .await
                .expect("sweep"),
            0
        );
        assert_eq!(list_pending(&client, "bob").await.expect("list").len(), 1);
    }

    #[tokio::test]
    async fn retention_purge_drops_rows_past_retention_and_keeps_fresh_ones() {
        let (_pg, client) = store_client().await;
        let old_done = insert_pending(&client, new_request("alice", "old-done"))
            .await
            .expect("insert");
        let old_pending = insert_pending(&client, new_request("alice", "old-pending"))
            .await
            .expect("insert");
        let fresh_done = insert_pending(&client, new_request("alice", "fresh-done"))
            .await
            .expect("insert");
        for counter in [old_done, old_pending] {
            client
                .execute(
                    "UPDATE videogen_requests SET created_at = NOW() - INTERVAL '90 days' WHERE counter = $1",
                    &[&counter],
                )
                .await
                .expect("age the row");
        }
        mark_failed(&client, "alice", old_done, "old failure")
            .await
            .expect("fail");
        mark_complete(
            &client,
            "alice",
            fresh_done,
            "vid",
            "https://bucket/vid.mp4",
        )
        .await
        .expect("complete");

        // Both aged rows go, including the one still marked pending — the sweep only
        // runs when this user polls, so pending rows must not be exempt from retention.
        assert_eq!(
            purge_for_principal(&client, "alice", 30)
                .await
                .expect("purge"),
            2
        );

        let remaining: Vec<i64> = client
            .query(
                "SELECT counter FROM videogen_requests ORDER BY counter",
                &[],
            )
            .await
            .expect("rows")
            .into_iter()
            .map(|r| r.get(0))
            .collect();
        assert_eq!(remaining, vec![fresh_done]);
        assert!(!remaining.contains(&old_pending));
    }

    #[tokio::test]
    async fn retention_purge_leaves_other_principals_alone() {
        let (_pg, client) = store_client().await;
        let bobs = insert_pending(&client, new_request("bob", "old"))
            .await
            .expect("insert");
        client
            .execute(
                "UPDATE videogen_requests SET created_at = NOW() - INTERVAL '90 days' WHERE counter = $1",
                &[&bobs],
            )
            .await
            .expect("age the row");

        assert_eq!(
            purge_for_principal(&client, "alice", 30)
                .await
                .expect("purge"),
            0
        );
        assert_eq!(
            client
                .query_one("SELECT COUNT(*) FROM videogen_requests", &[])
                .await
                .expect("count")
                .get::<_, i64>(0),
            1
        );
    }

    #[tokio::test]
    async fn schema_initialization_can_run_concurrently() {
        // Two instances booting at once must not race on CREATE ... IF NOT EXISTS.
        let (pg, url) = crate::media_index::test_support::PgContainer::spawn().await;
        let client_a = crate::media_index::test_support::connect_test_client(&url).await;
        let client_b = crate::media_index::test_support::connect_test_client(&url).await;

        let (result_a, result_b) = tokio::join!(init_schema(&client_a), init_schema(&client_b));

        result_a.unwrap();
        result_b.unwrap();
        drop(pg);
    }

    #[tokio::test]
    async fn updated_at_is_touched_by_trigger_not_by_each_update() {
        let (_pg, client) = store_client().await;
        let counter = insert_pending(&client, new_request("alice", "req-1"))
            .await
            .expect("insert");
        client
            .execute(
                "UPDATE videogen_requests SET updated_at = NOW() - INTERVAL '1 day' WHERE counter = $1",
                &[&counter],
            )
            .await
            .expect("age updated_at");

        mark_failed(&client, "alice", counter, "boom")
            .await
            .expect("fail");

        // `mark_failed` never assigns updated_at; the trigger must have.
        let stale: bool = client
            .query_one(
                "SELECT updated_at < NOW() - INTERVAL '1 hour' FROM videogen_requests WHERE counter = $1",
                &[&counter],
            )
            .await
            .expect("row")
            .get(0);
        assert!(!stale, "trigger did not touch updated_at");
    }

    #[test]
    fn schema_sql_is_concurrency_safe_and_indexed() {
        assert!(SCHEMA_SQL.contains("pg_advisory_xact_lock"));
        assert!(SCHEMA_SQL.contains("CREATE TABLE IF NOT EXISTS videogen_requests"));
        assert!(SCHEMA_SQL.contains("CHECK (status IN ('pending','complete','failed'))"));
        assert!(SCHEMA_SQL.contains("idx_videogen_requests_principal_status"));
        assert!(SCHEMA_SQL.contains("idx_videogen_requests_request_id"));
        assert!(SCHEMA_SQL.contains("videogen_requests_touch_updated_at"));
        assert!(SCHEMA_SQL.contains("NEW.updated_at = NOW()"));
        assert!(!SCHEMA_SQL.contains("DROP TRIGGER"));
    }

    #[tokio::test]
    async fn terminal_update_on_an_already_closed_request_is_a_no_op() {
        // The sweep and a late callback race; whichever lands second must not reopen
        // or overwrite the request.
        let (_pg, client) = store_client().await;
        let counter = insert_pending(&client, new_request("alice", "req-1"))
            .await
            .expect("insert");
        mark_failed(&client, "alice", counter, "swept")
            .await
            .expect("fail");

        assert_eq!(
            mark_complete(
                &client,
                "alice",
                counter,
                "vid-1",
                "https://bucket/vid-1.mp4"
            )
            .await
            .expect("complete"),
            0
        );
        let status: String = client
            .query_one(
                "SELECT status FROM videogen_requests WHERE counter = $1",
                &[&counter],
            )
            .await
            .expect("row")
            .get("status");
        assert_eq!(status, "failed");
    }

    #[tokio::test]
    async fn counters_are_unique_across_principals() {
        let (_pg, client) = store_client().await;
        let a = insert_pending(&client, new_request("alice", "req-1"))
            .await
            .expect("insert");
        let b = insert_pending(&client, new_request("bob", "req-2"))
            .await
            .expect("insert");
        // operation_id is `<principal>_<counter>`; a shared sequence keeps it unique
        // even though the old canister counter was per-principal.
        assert_ne!(a, b);
        assert!(b > a);
    }

    #[test]
    fn stale_window_outlives_the_generation_timeout() {
        // Must never be <= the generation timeout: the sweep would then race Vast's
        // own timeout-failure callback, and mobile stops polling the instant the
        // in-progress list empties.
        assert_eq!(stale_after_secs_from(None, Some(1800)), 2400);
        assert_eq!(stale_after_secs_from(None, Some(3600)), 4200);
        // Unset generation timeout falls back to the same default config uses.
        assert_eq!(stale_after_secs_from(None, None), 2400);
        // Explicit override wins outright, even if shorter.
        assert_eq!(stale_after_secs_from(Some(300), Some(1800)), 300);
    }

    #[test]
    fn env_overrides_are_ignored_unless_a_positive_integer() {
        // Env is process-global, so drive the parse/filter logic through a var this
        // test owns rather than mutating the real ones.
        const VAR: &str = "VIDEOGEN_REQUEST_STORE_ENV_PARSE_TEST";
        for raw in ["0", "-5", "abc", ""] {
            std::env::set_var(VAR, raw);
            assert_eq!(read_positive_env(VAR), None, "{raw} must not override");
        }
        std::env::set_var(VAR, "600");
        assert_eq!(read_positive_env(VAR), Some(600));
        std::env::remove_var(VAR);
        assert_eq!(read_positive_env(VAR), None);
    }
}
