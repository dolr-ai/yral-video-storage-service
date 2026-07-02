# Chain Coverage Audit Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Snapshot the chain's full post record (gxhc3 `fetch_posts`) into two local tables and reconcile it against our master + canonical-pHash tables to prove every video is migrated and hashed, with opt-in remediation.

**Architecture:** The work runs server-side (the service already holds `ic_agent`, the Postgres pool, and the media repos). A new async `chain_snapshot` job walks `fetch_posts` and upserts `yral_posts` (+ derived `yral_users` rollup). A read-only `chain-audit` SQL reconciliation categorizes each chain `video_uid` (A clean / B no-phash / C unimported / D not-in-buckets / E dead-in-master). `mirror-client` gets thin HMAC subcommands to trigger/poll/audit — exactly the existing `media-*` pattern.

**Tech Stack:** Rust, Axum, tokio-postgres, `yral-canisters-client` (candid-generated `UserPostService`), chrono, HMAC-signed CLI (`mirror-client`).

**Spec:** `docs/superpowers/specs/2026-07-02-chain-coverage-audit-design.md`

---

## Key codebase facts (verified — do not re-derive)

- **Canister binding exists** (generated at build time from the `.did`):
  `yral_canisters_client::user_post_service::UserPostService(canister_id, &agent)`
  with `pub async fn fetch_posts(&self, arg0: FetchPostsArgs) -> Result<FetchPostsResult>`.
  - `FetchPostsArgs { limit: u64, last_uuid_processed: Option<String> }`
  - `FetchPostsResult { last_post_id_fetched: Option<String>, posts: Vec<Post> }`
  - `Post { id: String, video_uid: String, creator_principal: Principal, status: PostStatus, created_at: SystemTime, .. }`
  - `PostStatus` variants: `Uploaded, ReadyToView, Transcoding, CheckingExplicitness, Draft, Deleted, BannedForExplicitness, BannedDueToUserReporting`
  - `SystemTime { secs_since_epoch: u64, nanos_since_epoch: u32 }`
  - The generated `fetch_posts` already retries transport errors (200ms base, 5×).
- **Canister id constant:** `yral_canisters_client::ic::USER_POST_SERVICE_ID` (= `gxhc3-pqaaa-aaaas-qbh3q-cai`). Import pattern used at `src/routes/upload/mark_post_as_published.rs:50`.
- **Join key:** chain `Post.video_uid` == internal `video_id` (bare uuid). Direct string equality, no transform.
- **Canonical pHash tuple:** `hash_kind='phash'`, `hash_version='offchain_binary_10x8_v1'`, `input_media_version='current_stored_object_v1'` (see `src/routes/media.rs:204,679-680`). "Has canonical pHash" = a `servable_video_hashes` row exists for `(video_id, phash, offchain_binary_10x8_v1, current_stored_object_v1)`.
- **Master servable gate:** `all_servable_videos_on_yral.servable_status` (TEXT NOT NULL). Read the exact "servable" value from data at impl time (grep `servable_status` writers in `src/media_index/repo.rs`); this plan uses `'servable'` as the placeholder — confirm before final.
- **Run infra:** `media_job_runs (id UUID, job_kind, status, requested_by, started_at, finished_at, cursor JSONB, totals JSONB, error_message)`. Row lifecycle mirrors `src/jobs/media_imports.rs`:
  - insert running: `INSERT INTO media_job_runs (id, job_kind, status, requested_by) VALUES ($1::TEXT::UUID, $2, 'running', $3)` (`media_imports.rs:324-337`)
  - finish: `UPDATE media_job_runs SET status=$2, finished_at=NOW(), totals=$3, error_message=NULL WHERE id=$1::TEXT::UUID` (`media_imports.rs:530-540`)
  - fail: `UPDATE ... SET status='failed', finished_at=NOW(), error_message=$2 ...` (`media_imports.rs:544-559`)
  - progress: `UPDATE media_job_runs SET cursor=$2, totals=$3 WHERE id=$1::TEXT::UUID`
- **Single-flight + spawn pattern:** `src/routes/media.rs:127-198` (`import_video_index`): `AtomicBool::compare_exchange` → 409 on contention → `JobGuard(flag.clone())` → `tokio::spawn` → `db::connect(&db_url)` → job fn → log. `JobGuard` is `src/jobs/mod.rs:23`.
- **AppState** (`src/main.rs:~40-64`): has `db_url`, `ic_agent: Agent`, `media_job_cancel: Arc<Mutex<CancellationToken>>`, `job_media_import_running: Arc<AtomicBool>`, `job_media_phash_running: Arc<AtomicBool>`. We add `job_chain_snapshot_running: Arc<AtomicBool>`.
- **Route registration:** `src/main.rs:335+` — `.route("/path", post(routes::media::handler).with_state(app_state.clone()).layer(middleware::from_fn(authorize)))`.
- **Schema DDL:** all `CREATE TABLE IF NOT EXISTS` live in `SCHEMA_SQL` in `src/media_index/schema.rs` (guarded by `pg_advisory_xact_lock`). Applied on startup.
- **mirror-client:** `crates/mirror-client/src/main.rs` dispatches on a subcommand string → `client.<method>()`. Client methods in `crates/mirror-client/src/lib.rs` use `self.sign(METHOD, path)` → `X-Timestamp` + `Authorization: HMAC-SHA256 <sig>` headers; `self.post_job(path, limit, ...)` for POST triggers; GET returns typed JSON.
- **Tests:** Postgres-container tests must run with `--test-threads=1` (known `PgContainer` parallelism flake). Existing repo tests show the `test_client`/seed helpers in `src/media_index/repo.rs` and `src/jobs/media_phash.rs` test modules — reuse them.

## File structure

**Create:**
- `src/media_index/chain_repo.rs` — chain-table types + all SQL (upsert post, mark stale, rebuild users, audit categorize, join-key sample, remediation-B delete). One responsibility: chain-table persistence + reconciliation SQL.
- `src/jobs/chain_snapshot.rs` — the `fetch_posts` walk job: page loop + termination guards + run tracking + SystemTime conversion + post-walk stale/rollup. Uses a `PostPageSource` trait seam so the loop is unit-testable without a live canister.
- `src/routes/chain.rs` — three handlers: `chain_snapshot` (POST trigger), `chain_snapshot_status` (GET), `chain_audit` (GET, `?remediate=`).

**Modify:**
- `src/media_index/schema.rs` — append `yral_posts` + `yral_users` DDL to `SCHEMA_SQL`.
- `src/media_index/mod.rs` — `pub mod chain_repo;` + re-exports.
- `src/jobs/mod.rs` — `pub mod chain_snapshot;`.
- `src/routes/mod.rs` — `pub mod chain;`.
- `src/main.rs` — add `job_chain_snapshot_running` to `AppState` + its construction; register 3 routes; add handlers to OpenApi `paths(...)` list.
- `crates/mirror-client/src/lib.rs` — `chain_snapshot`, `chain_status`, `chain_audit` client methods + response structs.
- `crates/mirror-client/src/main.rs` — `"chain-snapshot"`, `"chain-status"`, `"chain-audit"` (with `--remediate`) dispatch arms + help text.

---

## Phase 1 — Schema + chain_repo persistence

### Task 1: Create the two tables

**Files:**
- Modify: `src/media_index/schema.rs` (inside the `SCHEMA_SQL` string, after the existing `CREATE TABLE` blocks)

- [ ] **Step 1: Add DDL to `SCHEMA_SQL`**

Append (before the closing `"#;`), keeping the file's existing style:

```sql
CREATE TABLE IF NOT EXISTS yral_posts (
    post_id TEXT PRIMARY KEY,
    video_uid TEXT NOT NULL,
    creator_principal TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    status TEXT NOT NULL,
    snapshot_run_id UUID,
    fetched_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    stale BOOLEAN NOT NULL DEFAULT FALSE
);
CREATE INDEX IF NOT EXISTS idx_yral_posts_video_uid ON yral_posts (video_uid);
CREATE INDEX IF NOT EXISTS idx_yral_posts_creator ON yral_posts (creator_principal);
CREATE INDEX IF NOT EXISTS idx_yral_posts_status ON yral_posts (status);
CREATE INDEX IF NOT EXISTS idx_yral_posts_created_at ON yral_posts (created_at);

CREATE TABLE IF NOT EXISTS yral_users (
    creator_principal TEXT PRIMARY KEY,
    post_count BIGINT NOT NULL,
    first_seen TIMESTAMPTZ,
    last_seen TIMESTAMPTZ
);
```

- [ ] **Step 2: Verify it compiles + schema applies**

Run: `cargo build -p storj-interface`
Expected: PASS (SCHEMA_SQL is a `&str`; a syntax typo here won't fail the build but will fail Task 2's test — that's the real gate).

- [ ] **Step 3: Commit**

```bash
git add src/media_index/schema.rs
git commit -m "feat(chain-audit): add yral_posts + yral_users tables"
```

### Task 2: `chain_repo` module + `upsert_chain_post` (idempotent)

**Files:**
- Create: `src/media_index/chain_repo.rs`
- Modify: `src/media_index/mod.rs`
- Test: inline `#[cfg(test)]` in `chain_repo.rs`

- [ ] **Step 1: Register module**

In `src/media_index/mod.rs` add `pub mod chain_repo;` alongside the existing `pub mod` lines.

- [ ] **Step 2: Write the failing test**

In `src/media_index/chain_repo.rs`, create the test module. Reuse the crate's existing PG test helper (find it in `repo.rs` tests — likely `test_client()` returning a connected `Client` against the test container; match its exact signature/name).

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: match the real helper name/signature used in repo.rs tests.
    async fn client() -> tokio_postgres::Client { crate::media_index::repo::tests::test_client().await }

    #[tokio::test]
    async fn upsert_is_idempotent_and_updates_status_and_run() {
        let c = client().await;
        let run1 = uuid::Uuid::new_v4();
        let p = ChainPost {
            post_id: "p1".into(),
            video_uid: "v1".into(),
            creator_principal: "creator-a".into(),
            created_at: chrono::Utc::now(),
            status: "Uploaded".into(),
        };
        upsert_chain_post(&c, &p, run1).await.unwrap();
        // second write, same post, new run + new status
        let run2 = uuid::Uuid::new_v4();
        let mut p2 = p.clone();
        p2.status = "ReadyToView".into();
        upsert_chain_post(&c, &p2, run2).await.unwrap();

        let row = c.query_one(
            "SELECT status, snapshot_run_id, stale FROM yral_posts WHERE post_id='p1'", &[]
        ).await.unwrap();
        assert_eq!(row.get::<_, String>(0), "ReadyToView");
        assert_eq!(row.get::<_, uuid::Uuid>(1), run2);
        assert_eq!(row.get::<_, bool>(2), false);
        let count: i64 = c.query_one("SELECT count(*) FROM yral_posts WHERE post_id='p1'", &[]).await.unwrap().get(0);
        assert_eq!(count, 1);
    }
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p storj-interface --lib chain_repo -- --test-threads=1`
Expected: FAIL — `ChainPost` / `upsert_chain_post` not defined.

- [ ] **Step 4: Implement `ChainPost` + `upsert_chain_post`**

```rust
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
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p storj-interface --lib chain_repo -- --test-threads=1`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/media_index/chain_repo.rs src/media_index/mod.rs
git commit -m "feat(chain-audit): ChainPost + idempotent upsert_chain_post"
```

### Task 3: `mark_stale_posts` + `rebuild_yral_users`

**Files:**
- Modify: `src/media_index/chain_repo.rs`

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn mark_stale_flags_posts_from_older_runs_only() {
    let c = client().await;
    let old = uuid::Uuid::new_v4();
    let cur = uuid::Uuid::new_v4();
    upsert_chain_post(&c, &post("s-old", "vo", "ca", "Uploaded"), old).await.unwrap();
    upsert_chain_post(&c, &post("s-cur", "vc", "ca", "Uploaded"), cur).await.unwrap();
    let n = mark_stale_posts(&c, cur).await.unwrap();
    assert_eq!(n, 1);
    let stale_old: bool = c.query_one("SELECT stale FROM yral_posts WHERE post_id='s-old'", &[]).await.unwrap().get(0);
    let stale_cur: bool = c.query_one("SELECT stale FROM yral_posts WHERE post_id='s-cur'", &[]).await.unwrap().get(0);
    assert!(stale_old);
    assert!(!stale_cur);
}

#[tokio::test]
async fn rebuild_users_excludes_stale_and_aggregates() {
    let c = client().await;
    let run = uuid::Uuid::new_v4();
    upsert_chain_post(&c, &post("u1", "v1", "creatorX", "Uploaded"), run).await.unwrap();
    upsert_chain_post(&c, &post("u2", "v2", "creatorX", "ReadyToView"), run).await.unwrap();
    // a stale row for creatorX must NOT inflate the count
    upsert_chain_post(&c, &post("u3", "v3", "creatorX", "Deleted"), uuid::Uuid::new_v4()).await.unwrap();
    mark_stale_posts(&c, run).await.unwrap(); // flags u3
    rebuild_yral_users(&c).await.unwrap();
    let cnt: i64 = c.query_one("SELECT post_count FROM yral_users WHERE creator_principal='creatorX'", &[]).await.unwrap().get(0);
    assert_eq!(cnt, 2);
}

// test helper
fn post(id: &str, vid: &str, creator: &str, status: &str) -> ChainPost {
    ChainPost { post_id: id.into(), video_uid: vid.into(), creator_principal: creator.into(),
                created_at: chrono::Utc::now(), status: status.into() }
}
```

- [ ] **Step 2: Run to verify fail**

Run: `cargo test -p storj-interface --lib chain_repo -- --test-threads=1`
Expected: FAIL — functions undefined.

- [ ] **Step 3: Implement**

```rust
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
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p storj-interface --lib chain_repo -- --test-threads=1`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/media_index/chain_repo.rs
git commit -m "feat(chain-audit): mark_stale_posts + rebuild_yral_users"
```

---

## Phase 2 — The fetch_posts walk job

### Task 4: SystemTime → timestamptz + `PostPageSource` seam

**Files:**
- Create: `src/jobs/chain_snapshot.rs`
- Modify: `src/jobs/mod.rs` (`pub mod chain_snapshot;`)

- [ ] **Step 1: Write the failing test (conversion)**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use yral_canisters_client::user_post_service::SystemTime;

    #[test]
    fn converts_system_time_to_utc() {
        let st = SystemTime { secs_since_epoch: 1_700_000_000, nanos_since_epoch: 500_000_000 };
        let dt = system_time_to_utc(&st);
        assert_eq!(dt.timestamp(), 1_700_000_000);
        assert_eq!(dt.timestamp_subsec_millis(), 500);
    }
}
```

- [ ] **Step 2: Run to verify fail**

Run: `cargo test -p storj-interface --lib chain_snapshot -- --test-threads=1`
Expected: FAIL — module/function undefined.

- [ ] **Step 3: Implement conversion + trait seam**

```rust
//! Chain snapshot job: walk user_post_service.fetch_posts and stage yral_posts.
use chrono::{DateTime, Utc};
use yral_canisters_client::user_post_service::{FetchPostsArgs, FetchPostsResult, SystemTime};

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
```

Add the real impl (thin wrapper over the generated client):

```rust
use ic_agent::Agent;
use yral_canisters_client::{ic::USER_POST_SERVICE_ID, user_post_service::UserPostService};

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
```

> Confirm `async-trait` is a workspace dep (it is — see root `Cargo.toml`). Confirm the generated `UserPostService(id, &agent)` tuple-struct constructor form against `target/.../out/did/user_post_service.rs` (matches `mark_post_as_published.rs:50`).

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p storj-interface --lib chain_snapshot -- --test-threads=1`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/jobs/chain_snapshot.rs src/jobs/mod.rs
git commit -m "feat(chain-audit): SystemTime conversion + PostPageSource seam"
```

### Task 5: The walk loop with termination guards (testable, no DB, no canister)

**Files:**
- Modify: `src/jobs/chain_snapshot.rs`

The loop is factored so the *page-driving* logic is tested against a mock source, and the DB upsert is a callback. This isolates the C2 termination risk.

- [ ] **Step 1: Write failing tests for termination**

```rust
#[cfg(test)]
mod walk_tests {
    use super::*;
    use std::sync::Mutex;
    use yral_canisters_client::user_post_service::{Post, PostStatus, SystemTime};

    struct MockSource { pages: Mutex<Vec<FetchPostsResult>> }
    #[async_trait::async_trait]
    impl PostPageSource for MockSource {
        async fn fetch(&self, _l: u64, _c: Option<String>) -> anyhow::Result<FetchPostsResult> {
            Ok(self.pages.lock().unwrap().remove(0))
        }
    }
    fn mkpost(id: &str) -> Post { /* fill required fields; video_uid=id, creator=Principal::anonymous(), status=Uploaded, created_at=SystemTime{0,0}, plus share/likes/view defaults */ unimplemented!() }

    #[tokio::test]
    async fn stops_on_null_cursor() {
        let src = MockSource { pages: Mutex::new(vec![
            FetchPostsResult { posts: vec![mkpost("a")], last_post_id_fetched: Some("a".into()) },
            FetchPostsResult { posts: vec![mkpost("b")], last_post_id_fetched: None },
        ]) };
        let mut seen = vec![];
        let done = walk_pages(&src, 10, 1000, |p| { seen.push(p.video_uid.clone()); }).await.unwrap();
        assert_eq!(seen, vec!["a", "b"]);
        assert!(done.completed);
    }

    #[tokio::test]
    async fn stops_on_non_advancing_cursor() {
        // cursor echoes the same id → must NOT loop forever
        let src = MockSource { pages: Mutex::new(vec![
            FetchPostsResult { posts: vec![mkpost("a")], last_post_id_fetched: Some("a".into()) },
            FetchPostsResult { posts: vec![mkpost("a")], last_post_id_fetched: Some("a".into()) },
        ]) };
        let mut seen = vec![];
        let done = walk_pages(&src, 1, 1000, |p| seen.push(p.video_uid.clone())).await.unwrap();
        assert!(done.completed);
        assert!(seen.len() <= 2); // stopped once cursor failed to advance
    }

    #[tokio::test]
    async fn stops_on_short_page() {
        let src = MockSource { pages: Mutex::new(vec![
            FetchPostsResult { posts: vec![mkpost("a")], last_post_id_fetched: Some("a".into()) }, // len 1 < PAGE 10
        ]) };
        let mut seen = vec![];
        let done = walk_pages(&src, 10, 1000, |p| seen.push(p.video_uid.clone())).await.unwrap();
        assert_eq!(seen, vec!["a"]);
        assert!(done.completed);
    }

    #[tokio::test]
    async fn max_iters_backstop_marks_incomplete() {
        // every page is full and cursor advances forever → backstop hits, completed=false
        // (build a source that always returns a full page with a fresh cursor)
    }
}
```

- [ ] **Step 2: Run to verify fail**

Run: `cargo test -p storj-interface --lib chain_snapshot -- --test-threads=1`
Expected: FAIL — `walk_pages`/`WalkOutcome` undefined.

- [ ] **Step 3: Implement `walk_pages`**

```rust
pub struct WalkOutcome {
    pub pages: u64,
    pub posts_seen: u64,
    pub completed: bool, // true only if a natural terminator hit (not the MAX_ITERS backstop)
}

/// Drive fetch_posts pagination. `on_post` is called for every post in order.
/// Terminates on: empty page, null/empty cursor, non-advancing cursor, short
/// page. `max_iters` is a hard backstop — if it trips, `completed = false`.
pub async fn walk_pages<S: PostPageSource, F: FnMut(&yral_canisters_client::user_post_service::Post)>(
    source: &S,
    page: u64,
    max_iters: u64,
    mut on_post: F,
) -> anyhow::Result<WalkOutcome> {
    let mut cursor: Option<String> = None;
    let mut pages = 0u64;
    let mut posts_seen = 0u64;
    loop {
        if pages >= max_iters {
            return Ok(WalkOutcome { pages, posts_seen, completed: false });
        }
        let res = source.fetch(page, cursor.clone()).await?;
        pages += 1;
        let len = res.posts.len() as u64;
        for p in &res.posts {
            on_post(p);
            posts_seen += 1;
        }
        let next = res.last_post_id_fetched.filter(|s| !s.is_empty());
        let advanced = next.is_some() && next != cursor;
        // terminate on any natural end-condition
        if res.posts.is_empty() || next.is_none() || !advanced || len < page {
            return Ok(WalkOutcome { pages, posts_seen, completed: true });
        }
        cursor = next;
    }
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p storj-interface --lib chain_snapshot -- --test-threads=1`
Expected: PASS (fill in `mkpost` + the max-iters test body first).

- [ ] **Step 5: Commit**

```bash
git add src/jobs/chain_snapshot.rs
git commit -m "feat(chain-audit): fetch_posts walk loop with termination guards"
```

### Task 6: Orchestrator `run_chain_snapshot` (run row + upsert + stale + rollup)

**Files:**
- Modify: `src/jobs/chain_snapshot.rs`

- [ ] **Step 1: Write failing test (DB-backed, mock source)**

```rust
#[tokio::test]
async fn snapshot_populates_posts_and_users_and_completes() {
    let mut c = crate::media_index::repo::tests::test_client().await;
    let src = MockSource { pages: Mutex::new(vec![
        FetchPostsResult { posts: vec![mkpost_full("p1","v1","cA","Uploaded")], last_post_id_fetched: None },
    ]) };
    let summary = run_chain_snapshot(&src, &mut c, "test", &default_cancel()).await.unwrap();
    assert_eq!(summary.posts_upserted, 1);
    assert!(summary.completed);
    let posts: i64 = c.query_one("SELECT count(*) FROM yral_posts WHERE NOT stale", &[]).await.unwrap().get(0);
    assert_eq!(posts, 1);
    let users: i64 = c.query_one("SELECT count(*) FROM yral_users", &[]).await.unwrap().get(0);
    assert_eq!(users, 1);
    let run = c.query_one("SELECT status FROM media_job_runs WHERE id=$1::TEXT::UUID", &[&summary.job_run_id.to_string()]).await.unwrap();
    assert_eq!(run.get::<_, String>(0), "completed"); // match the status literal media_imports uses
}
```

> Confirm the exact success status literal used elsewhere (grep `mark_job_run_finished` call sites in `media_imports.rs` — e.g. `"completed"`). Use the same literal.

- [ ] **Step 2: Run to verify fail**

Run: `cargo test -p storj-interface --lib chain_snapshot -- --test-threads=1`
Expected: FAIL — `run_chain_snapshot`/`ChainSnapshotSummary` undefined.

- [ ] **Step 3: Implement orchestrator**

```rust
use crate::media_index::chain_repo::{self, ChainPost};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const JOB_KIND: &str = "chain_snapshot";
const PAGE: u64 = 100;        // tune; gentle on canister
const MAX_ITERS: u64 = 100_000;

pub struct ChainSnapshotSummary {
    pub job_run_id: Uuid,
    pub posts_upserted: u64,
    pub pages: u64,
    pub skipped: u64,
    pub completed: bool,
}

pub async fn run_chain_snapshot<S: PostPageSource>(
    source: &S,
    client: &mut tokio_postgres::Client,
    requested_by: &str,
    cancel: &CancellationToken,
) -> anyhow::Result<ChainSnapshotSummary> {
    let run_id = Uuid::new_v4();
    // INSERT running (mirror media_imports::insert_job_run, JOB_KIND="chain_snapshot")
    client.execute(
        "INSERT INTO media_job_runs (id, job_kind, status, requested_by) VALUES ($1::TEXT::UUID,$2,'running',$3)",
        &[&run_id.to_string(), &JOB_KIND, &requested_by],
    ).await?;

    let mut upserted = 0u64;
    let mut skipped = 0u64;

    // Collect-then-write per page: keep the walk pure and do DB writes here.
    // Simpler: pass a closure that buffers, then flush after each page. For the
    // first version, buffer the whole page vec via walk_pages callback into a Vec,
    // then upsert. (Page size is bounded by PAGE, so memory is fine.)
    let result = async {
        let mut cursor: Option<String> = None;
        let mut pages = 0u64;
        loop {
            if cancel.is_cancelled() { return Ok::<_, anyhow::Error>((pages, false)); }
            if pages >= MAX_ITERS { return Ok((pages, false)); }
            let res = source.fetch(PAGE, cursor.clone()).await?;
            pages += 1;
            let len = res.posts.len() as u64;
            for p in &res.posts {
                if p.video_uid.is_empty() { skipped += 1; continue; }
                let cp = ChainPost {
                    post_id: p.id.clone(),
                    video_uid: p.video_uid.clone(),
                    creator_principal: p.creator_principal.to_text(),
                    created_at: system_time_to_utc(&p.created_at),
                    status: format!("{:?}", p.status), // PostStatus variant name
                };
                chain_repo::upsert_chain_post(client, &cp, run_id).await?;
                upserted += 1;
            }
            // progress: cursor + totals
            let totals = serde_json::json!({ "pages": pages, "posts_upserted": upserted, "skipped": skipped });
            let _ = client.execute(
                "UPDATE media_job_runs SET cursor=$2, totals=$3 WHERE id=$1::TEXT::UUID",
                &[&run_id.to_string(),
                  &serde_json::json!({ "last": res.last_post_id_fetched }),
                  &totals],
            ).await;
            let next = res.last_post_id_fetched.filter(|s| !s.is_empty());
            let advanced = next.is_some() && next != cursor;
            if res.posts.is_empty() || next.is_none() || !advanced || len < PAGE {
                return Ok((pages, true));
            }
            cursor = next;
        }
    }.await;

    match result {
        Ok((pages, completed)) => {
            if completed {
                // stale FIRST, then rebuild rollup from non-stale
                chain_repo::mark_stale_posts(client, run_id).await?;
                chain_repo::rebuild_yral_users(client).await?;
            }
            let totals = serde_json::json!({ "pages": pages, "posts_upserted": upserted, "skipped": skipped, "completed": completed });
            let status = if completed { "completed" } else { "partial" }; // match repo literals
            client.execute(
                "UPDATE media_job_runs SET status=$2, finished_at=NOW(), totals=$3, error_message=NULL WHERE id=$1::TEXT::UUID",
                &[&run_id.to_string(), &status, &totals],
            ).await?;
            Ok(ChainSnapshotSummary { job_run_id: run_id, posts_upserted: upserted, pages, skipped, completed })
        }
        Err(e) => {
            let _ = client.execute(
                "UPDATE media_job_runs SET status='failed', finished_at=NOW(), error_message=$2 WHERE id=$1::TEXT::UUID",
                &[&run_id.to_string(), &e.to_string()],
            ).await;
            Err(e)
        }
    }
}
```

> Note: the walk logic is duplicated here rather than reusing `walk_pages` because DB writes + progress are interleaved and `walk_pages` stays the pure, unit-tested spec of the termination rules. Keep the two termination conditions **identical** — if you change one, change both. (If you prefer DRY, refactor `walk_pages` to take an async per-page callback and call it here; only do that if the async-closure ergonomics stay clean.)

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p storj-interface --lib chain_snapshot -- --test-threads=1`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/jobs/chain_snapshot.rs
git commit -m "feat(chain-audit): run_chain_snapshot orchestrator with run tracking"
```

---

## Phase 3 — Reconcile audit SQL

### Task 7: Category counts A/B/C/D/E + status aggregation + stale exclusion

**Files:**
- Modify: `src/media_index/chain_repo.rs`

- [ ] **Step 1: Write failing test (seed master/hashes/video_index)**

Seed one video per category and assert counts. Reuse existing seed helpers (`make_row`/`servable_input`/`seed_video` from `repo.rs`/`media_phash.rs` tests — match exact names).

```rust
#[tokio::test]
async fn audit_categorizes_all_five() {
    let c = client().await;
    let run = uuid::Uuid::new_v4();
    // A: master servable + canonical hash
    seed_master(&c, "A", "servable").await; seed_canonical_hash(&c, "A").await;
    // B: master servable, no hash
    seed_master(&c, "B", "servable").await;
    // C: not master, in video_index
    seed_video_index(&c, "C").await;
    // D: nowhere
    // E: master non-servable
    seed_master(&c, "E", "unservable").await; seed_canonical_hash(&c, "E").await;
    for (v, st) in [("A","Uploaded"),("B","Uploaded"),("C","Uploaded"),("D","Uploaded"),("E","Uploaded")] {
        upsert_chain_post(&c, &post(&format!("p{v}"), v, "cc", st), run).await.unwrap();
    }
    // excluded: only-Deleted video
    upsert_chain_post(&c, &post("pX", "X", "cc", "Deleted"), run).await.unwrap();

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
    // same video_uid: one Deleted post + one ReadyToView post → coverage-expected
}

#[tokio::test]
async fn non_canonical_hash_does_not_satisfy_a() {
    // hash row with hash_version='other' → still category B, not A
}
```

- [ ] **Step 2: Run to verify fail**

Run: `cargo test -p storj-interface --lib chain_repo -- --test-threads=1`
Expected: FAIL — `chain_audit`/`ChainAuditReport` undefined.

- [ ] **Step 3: Implement the categorization query**

```rust
pub struct ChainAuditReport {
    pub total_expected: i64,
    pub category_a: i64,
    pub category_b: i64,
    pub category_c: i64,
    pub category_d: i64,
    pub category_e: i64,
    pub excluded_by_status: i64,
    pub b_backing_off: i64, // subset of B currently in backoff (annotation only)
}

const EXPECTED_STATUSES: &str =
    "('Uploaded','ReadyToView','Transcoding','CheckingExplicitness')";

pub async fn chain_audit(client: &Client) -> Result<ChainAuditReport, tokio_postgres::Error> {
    // Distinct, non-stale video_uids that are coverage-expected (ANY post in expected status).
    // One CTE-based query returns all category counts in a single round trip.
    let sql = format!(r#"
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
                WHERE f.video_id = e.video_uid AND f.next_retry_at > now()) AS backing_off
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
    "#, expected = EXPECTED_STATUSES);
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
```

> Confirm `video_index` has a `video_id` column (it does — `src/db.rs:6`). Confirm the servable literal (`'servable'`) matches what writers store.

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p storj-interface --lib chain_repo -- --test-threads=1`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/media_index/chain_repo.rs
git commit -m "feat(chain-audit): chain_audit categorization (A-E + status aggregation)"
```

### Task 8: Join-key validation gate + sample lists (D + worst creators)

**Files:**
- Modify: `src/media_index/chain_repo.rs`

- [ ] **Step 1: Write failing tests**

```rust
#[tokio::test]
async fn join_key_gate_passes_when_matches_high() { /* seed matching → match_rate high */ }
#[tokio::test]
async fn join_key_gate_flags_when_matches_low() { /* seed all-mismatch → match_rate 0 */ }
#[tokio::test]
async fn d_sample_and_worst_creators_returned() { /* one D video, assert it appears + creator charged */ }
```

- [ ] **Step 2: Run to verify fail** — `cargo test -p storj-interface --lib chain_repo -- --test-threads=1` → FAIL.

- [ ] **Step 3: Implement**

```rust
/// Fraction (0.0–1.0) of a sample of expected video_uids that match a video_id
/// in master OR video_index. Low value ⇒ join-key skew ⇒ audit is meaningless.
pub async fn join_key_match_rate(client: &Client, sample: i64) -> Result<f64, tokio_postgres::Error> {
    let row = client.query_one(&format!(r#"
        WITH s AS (
            SELECT DISTINCT video_uid FROM yral_posts WHERE NOT stale
              AND status IN {expected} LIMIT $1
        )
        SELECT count(*) AS total,
               count(*) FILTER (WHERE
                   EXISTS (SELECT 1 FROM all_servable_videos_on_yral m WHERE m.video_id = s.video_uid)
                OR EXISTS (SELECT 1 FROM video_index vi WHERE vi.video_id = s.video_uid)) AS matched
        FROM s"#, expected = EXPECTED_STATUSES), &[&sample]).await?;
    let total: i64 = row.get("total");
    let matched: i64 = row.get("matched");
    Ok(if total == 0 { 1.0 } else { matched as f64 / total as f64 })
}

/// Up to `limit` category-D video_uids with their creator(s), for manual probing.
pub async fn category_d_sample(client: &Client, limit: i64) -> Result<Vec<(String, String)>, tokio_postgres::Error> {
    // expected, non-stale, not in master, not in video_index; join back for a creator
    // ... SELECT p.video_uid, min(p.creator_principal) ... GROUP BY p.video_uid LIMIT $1
    todo!("SQL analogous to chain_audit's D filter")
}

/// Creators ranked by count of non-clean (B/C/D/E) expected videos they authored.
pub async fn worst_creators(client: &Client, limit: i64) -> Result<Vec<(String, i64)>, tokio_postgres::Error> {
    todo!("charge each non-clean video to every creator that authored an expected post for it")
}
```

Fill the two `todo!`s with SQL reusing the same category logic (factor the `cat` CTE into a helper string if convenient).

- [ ] **Step 4: Run to verify pass** — expected PASS.

- [ ] **Step 5: Commit**

```bash
git add src/media_index/chain_repo.rs
git commit -m "feat(chain-audit): join-key gate + D sample + worst-creators"
```

### Task 9: Remediation-B helper (clear failure rows)

**Files:**
- Modify: `src/media_index/chain_repo.rs`

- [ ] **Step 1: Failing test**

```rust
#[tokio::test]
async fn remediate_b_clears_failure_rows_for_category_b_only() {
    // seed a B video with a backing-off media_job_failures row; call remediate_missing_phash; assert row gone
}
```

- [ ] **Step 2: Run → FAIL.**

- [ ] **Step 3: Implement** (mirror `media_phash.rs:322` delete)

```rust
/// Clear failure rows for the given video_ids so the media-phash worker retries.
/// Returns rows deleted. (Category-B remediation.)
pub async fn clear_phash_failures(client: &Client, video_ids: &[String]) -> Result<u64, tokio_postgres::Error> {
    if video_ids.is_empty() { return Ok(0); }
    client.execute(
        "DELETE FROM media_job_failures WHERE video_id = ANY($1)",
        &[&video_ids],
    ).await
}

/// The category-B video_ids (servable master rows missing the canonical tuple).
pub async fn category_b_video_ids(client: &Client, limit: i64) -> Result<Vec<String>, tokio_postgres::Error> {
    todo!("SELECT expected, in master, servable, no canonical hash; LIMIT $1")
}
```

- [ ] **Step 4: Run → PASS.**

- [ ] **Step 5: Commit**

```bash
git add src/media_index/chain_repo.rs
git commit -m "feat(chain-audit): category-B remediation (clear phash failure rows)"
```

---

## Phase 4 — Routes + AppState wiring

### Task 10: AppState field + construction

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1:** Add to `AppState` (beside `job_media_phash_running`):

```rust
pub job_chain_snapshot_running: Arc<AtomicBool>,
```

- [ ] **Step 2:** In `AppState` construction (where the other `Arc::new(AtomicBool::new(false))` flags are built), add:

```rust
job_chain_snapshot_running: Arc::new(AtomicBool::new(false)),
```

- [ ] **Step 3:** Run: `cargo build -p storj-interface` → Expected: FAIL only if some `AppState { .. }` literal is missing the field; fix all construction sites. Then PASS.

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "feat(chain-audit): add job_chain_snapshot_running to AppState"
```

### Task 11: Route handlers module

**Files:**
- Create: `src/routes/chain.rs`
- Modify: `src/routes/mod.rs` (`pub mod chain;`)

- [ ] **Step 1: Implement handlers** (mirror `routes/media.rs` single-flight + spawn):

```rust
use axum::{extract::{Query, State}, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;
use crate::{jobs::JobGuard, jobs::chain_snapshot, media_index::chain_repo, AppState, db};

#[derive(Deserialize)]
pub struct SnapshotParams { pub requested_by: Option<String> }

/// POST /chain/snapshot — trigger the fetch_posts walk. 202 accepted / 409 running.
pub async fn chain_snapshot_start(
    State(state): State<AppState>,
    Query(params): Query<SnapshotParams>,
) -> StatusCode {
    if state.job_chain_snapshot_running
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return StatusCode::CONFLICT;
    }
    let guard = JobGuard(state.job_chain_snapshot_running.clone());
    let db_url = state.db_url.clone();
    let agent = state.ic_agent.clone();
    let cancel = state.media_job_cancel.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let requested_by = params.requested_by
        .map(|s| s.chars().take(256).collect::<String>())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "chain_snapshot_api".into());

    tokio::spawn(async move {
        let _guard = guard;
        let mut client = match db::connect(&db_url).await {
            Ok(c) => c,
            Err(e) => { tracing::error!(error=%e, "chain_snapshot: DB connect failed"); return; }
        };
        let src = chain_snapshot::LivePostSource(&agent);
        match chain_snapshot::run_chain_snapshot(&src, &mut client, &requested_by, &cancel).await {
            Ok(s) => tracing::info!(job_run_id=%s.job_run_id, posts=s.posts_upserted, pages=s.pages, completed=s.completed, "chain_snapshot: done"),
            Err(e) => { tracing::error!(error=%e, "chain_snapshot: failed");
                        sentry::capture_message(&format!("chain_snapshot failed: {e}"), sentry::Level::Error); }
        }
    });
    StatusCode::ACCEPTED
}

#[derive(Serialize)]
pub struct ChainAuditResponse {
    pub total_expected: i64,
    pub category_a: i64, pub category_b: i64, pub category_c: i64,
    pub category_d: i64, pub category_e: i64,
    pub excluded_by_status: i64, pub b_backing_off: i64,
    pub d_sample: Vec<DVideo>,
    pub worst_creators: Vec<CreatorGap>,
    pub remediated: Option<Remediated>,
}
#[derive(Serialize)] pub struct DVideo { pub video_uid: String, pub creator_principal: String }
#[derive(Serialize)] pub struct CreatorGap { pub creator_principal: String, pub non_clean: i64 }
#[derive(Serialize)] pub struct Remediated { pub b_failures_cleared: u64, pub import_triggered: bool }

#[derive(Deserialize)]
pub struct AuditParams { #[serde(default)] pub remediate: bool }

/// GET /chain/audit — read-only reconciliation. `?remediate=true` (opt-in) also
/// clears category-B failure rows and triggers a bulk import run for category C.
pub async fn chain_audit(
    State(state): State<AppState>,
    Query(params): Query<AuditParams>,
) -> Result<Json<ChainAuditResponse>, StatusCode> {
    let client = db::connect(&state.db_url).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Join-key gate: bail loudly if the equality is broken.
    let rate = chain_repo::join_key_match_rate(&client, 200).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if rate < 0.5 {
        tracing::error!(match_rate = rate, "chain_audit: join-key match rate implausibly low — aborting");
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    let rep = chain_repo::chain_audit(&client).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let d_sample = chain_repo::category_d_sample(&client, 100).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let worst = chain_repo::worst_creators(&client, 50).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let remediated = if params.remediate {
        let b_ids = chain_repo::category_b_video_ids(&client, 100_000).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let cleared = chain_repo::clear_phash_failures(&client, &b_ids).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        // C: trigger a bulk import run only if there are category-C rows.
        let import_triggered = rep.category_c > 0;
        if import_triggered {
            // Reuse the existing import single-flight + spawn path. Simplest: set the
            // import flag + spawn import_current_video_index exactly like routes::media::import_video_index.
            // (Factor that spawn into a helper if cleaner.)
        }
        Some(Remediated { b_failures_cleared: cleared, import_triggered })
    } else { None };

    Ok(Json(ChainAuditResponse {
        total_expected: rep.total_expected,
        category_a: rep.category_a, category_b: rep.category_b, category_c: rep.category_c,
        category_d: rep.category_d, category_e: rep.category_e,
        excluded_by_status: rep.excluded_by_status, b_backing_off: rep.b_backing_off,
        d_sample: d_sample.into_iter().map(|(v,c)| DVideo{video_uid:v, creator_principal:c}).collect(),
        worst_creators: worst.into_iter().map(|(c,n)| CreatorGap{creator_principal:c, non_clean:n}).collect(),
        remediated,
    }))
}

/// GET /chain/snapshot/status — latest chain_snapshot run row.
pub async fn chain_snapshot_status(State(state): State<AppState>) -> Result<Json<serde_json::Value>, StatusCode> {
    let client = db::connect(&state.db_url).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let row = client.query_opt(
        "SELECT id::TEXT, status, started_at, finished_at, totals, cursor
         FROM media_job_runs WHERE job_kind='chain_snapshot' ORDER BY started_at DESC LIMIT 1",
        &[]).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let body = match row {
        Some(r) => serde_json::json!({
            "id": r.get::<_,String>(0), "status": r.get::<_,String>(1),
            "totals": r.get::<_,Option<serde_json::Value>>(4),
            "cursor": r.get::<_,Option<serde_json::Value>>(5),
        }),
        None => serde_json::json!({ "status": "none" }),
    };
    Ok(Json(body))
}
```

> Confirm `db::connect` returns an owned `Client` (used by `missing_phash_audit` at `routes/media.rs:215+`). Confirm `AppState.db_url` field name. The category-C import trigger: reuse the exact spawn block from `routes::media::import_video_index` (single-flight on `job_media_import_running`); do not invent a new path.

- [ ] **Step 2:** `pub mod chain;` in `src/routes/mod.rs`.

- [ ] **Step 3:** Run: `cargo build -p storj-interface` → fix types until PASS.

- [ ] **Step 4: Commit**

```bash
git add src/routes/chain.rs src/routes/mod.rs
git commit -m "feat(chain-audit): chain snapshot/status/audit route handlers"
```

### Task 12: Register routes in the router

**Files:**
- Modify: `src/main.rs` (the `Router::new()...` chain at ~335; and the OpenApi `paths(...)` list)

- [ ] **Step 1:** Add three routes (authorize-layered, like the `/media/*` block):

```rust
.route(
    "/chain/snapshot",
    post(routes::chain::chain_snapshot_start)
        .with_state(app_state.clone())
        .layer(middleware::from_fn(authorize)),
)
.route(
    "/chain/snapshot/status",
    get(routes::chain::chain_snapshot_status)
        .with_state(app_state.clone())
        .layer(middleware::from_fn(authorize)),
)
.route(
    "/chain/audit",
    get(routes::chain::chain_audit)
        .with_state(app_state.clone())
        .layer(middleware::from_fn(authorize)),
)
```

- [ ] **Step 2:** (Optional) add the handlers to the `#[openapi(paths(...))]` list if they should appear in swagger. Skip if `utoipa::path` annotations aren't added (they're optional; existing media handlers use them but it's not required to compile).

- [ ] **Step 3:** Run: `cargo build -p storj-interface` → PASS. Then `cargo test -p storj-interface -- --test-threads=1` → PASS.

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "feat(chain-audit): register /chain/* routes"
```

---

## Phase 5 — mirror-client CLI

### Task 13: Client methods + response types

**Files:**
- Modify: `crates/mirror-client/src/lib.rs`

- [ ] **Step 1:** Add response structs (mirror `CoverageStats` style) + three methods:

```rust
#[derive(Debug, serde::Deserialize)]
pub struct ChainAuditResult {
    pub total_expected: i64,
    pub category_a: i64, pub category_b: i64, pub category_c: i64,
    pub category_d: i64, pub category_e: i64,
    pub excluded_by_status: i64, pub b_backing_off: i64,
    pub d_sample: Vec<ChainDVideo>,
    pub worst_creators: Vec<ChainCreatorGap>,
    pub remediated: Option<serde_json::Value>,
}
#[derive(Debug, serde::Deserialize)] pub struct ChainDVideo { pub video_uid: String, pub creator_principal: String }
#[derive(Debug, serde::Deserialize)] pub struct ChainCreatorGap { pub creator_principal: String, pub non_clean: i64 }

impl MirrorClient {
    /// Trigger the chain snapshot walk (202 / 409).
    pub async fn chain_snapshot(&self) -> Result<(), MirrorError> {
        self.post_job("/chain/snapshot", None, None, None, None).await
    }

    /// Latest chain snapshot run status.
    pub async fn chain_status(&self) -> Result<serde_json::Value, MirrorError> {
        let path = "/chain/snapshot/status";
        let (ts, sig) = self.sign("GET", path);
        let resp = self.http.get(format!("{}{}", self.base_url, path))
            .header("X-Timestamp", &ts).header("Authorization", format!("HMAC-SHA256 {sig}"))
            .send().await?;
        match resp.status().as_u16() {
            200 => Ok(resp.json().await?),
            401 | 403 => Err(MirrorError::Unauthorized),
            status => Err(MirrorError::ServerError { status, body: resp.text().await.unwrap_or_default() }),
        }
    }

    /// Run the reconciliation audit. `remediate=true` appends `?remediate=true`.
    /// NOTE: server signs the PATH only (query excluded) — matches media_feed.
    pub async fn chain_audit(&self, remediate: bool) -> Result<ChainAuditResult, MirrorError> {
        let path = "/chain/audit";
        let (ts, sig) = self.sign("GET", path);
        let url = if remediate { format!("{}{}?remediate=true", self.base_url, path) }
                  else { format!("{}{}", self.base_url, path) };
        let resp = self.http.get(url)
            .header("X-Timestamp", &ts).header("Authorization", format!("HMAC-SHA256 {sig}"))
            .send().await?;
        match resp.status().as_u16() {
            200 => Ok(resp.json().await?),
            401 | 403 => Err(MirrorError::Unauthorized),
            422 => Err(MirrorError::ServerError { status: 422, body: "join-key match rate too low — snapshot/audit skew; investigate before trusting counts".into() }),
            status => Err(MirrorError::ServerError { status, body: resp.text().await.unwrap_or_default() }),
        }
    }
}
```

> Confirm `self.sign`, `self.post_job`, `self.http`, `self.base_url`, `MirrorError` names/signatures against existing methods (Task-13 code mirrors `media_audit`/`media_import`/`media_feed`). Confirm `post_job` arity (`path, limit, ?, ?, shard`) from `media_import`.

- [ ] **Step 2:** Run: `cargo build -p mirror-client` → PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/mirror-client/src/lib.rs
git commit -m "feat(chain-audit): mirror-client chain_snapshot/status/audit methods"
```

### Task 14: CLI subcommand dispatch + help

**Files:**
- Modify: `crates/mirror-client/src/main.rs`

- [ ] **Step 1:** Add dispatch arms (match the existing string-match style ~line 116-432):

```rust
"chain-snapshot" => client.chain_snapshot().await.map(|_| {
    println!("chain snapshot started (202). poll with: chain-status");
}),
"chain-status" => match client.chain_status().await {
    Ok(v) => { println!("{}", serde_json::to_string_pretty(&v).unwrap()); Ok(()) }
    Err(e) => Err(e),
},
"chain-audit" => {
    let remediate = args.iter().any(|a| a == "--remediate");
    match client.chain_audit(remediate).await {
        Ok(r) => {
            println!("chain coverage audit:");
            println!("  total expected videos : {}", r.total_expected);
            println!("  A clean               : {}", r.category_a);
            println!("  B no canonical phash  : {}  (backing off: {})", r.category_b, r.b_backing_off);
            println!("  C unimported          : {}", r.category_c);
            println!("  D not in buckets      : {}", r.category_d);
            println!("  E dead in master      : {}", r.category_e);
            println!("  excluded (status)     : {}", r.excluded_by_status);
            if !r.d_sample.is_empty() {
                println!("  category-D sample (video_uid  creator):");
                for d in r.d_sample.iter().take(20) { println!("    {}  {}", d.video_uid, d.creator_principal); }
            }
            if let Some(rem) = &r.remediated { println!("  remediated: {}", rem); }
            Ok(())
        }
        Err(e) => Err(e),
    }
}
```

- [ ] **Step 2:** Add the three subcommands to the top-of-file `Commands:` help block (line ~5).

- [ ] **Step 3:** Run: `cargo build -p mirror-client` → PASS. Sanity: `cargo run -p mirror-client -- chain-audit` against a non-configured env should print a clean auth/connection error (not panic).

- [ ] **Step 4: Commit**

```bash
git add crates/mirror-client/src/main.rs
git commit -m "feat(chain-audit): chain-snapshot/status/audit CLI subcommands"
```

---

## Phase 6 — Full build, docs, preview validation

### Task 15: Workspace build + full test pass + clippy/fmt

- [ ] **Step 1:** `cargo fmt --all`
- [ ] **Step 2:** `cargo clippy --workspace -- -D warnings` → fix warnings.
- [ ] **Step 3:** `cargo test --workspace -- --test-threads=1` → all PASS.
- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "chore(chain-audit): fmt + clippy clean"
```

### Task 16: README / usage note

**Files:**
- Modify: `readme.md` (mirror-client subcommand list — where `media-*` commands are documented)

- [ ] **Step 1:** Document the three new subcommands + the intended flow:
  1. `chain-snapshot` → walk chain into `yral_posts`/`yral_users`
  2. `chain-status` → poll until `completed`
  3. `chain-audit` → read-only report; `chain-audit --remediate` to clear B + kick C import.
  Note: `chain-audit` returns 422 if the join-key match rate is implausibly low.
- [ ] **Step 2: Commit**

```bash
git add readme.md
git commit -m "docs(chain-audit): document chain-* mirror-client subcommands"
```

### Task 17: Manual preview validation (no code — a checklist, run against PREVIEW first)

- [ ] Deploy branch to **preview**.
- [ ] `chain-snapshot`; poll `chain-status` until `completed`. Sanity-check `totals.posts_upserted` looks like the expected post count (hundreds of thousands).
- [ ] `chain-audit` (read-only). Confirm the join-key gate did NOT trip (no 422) and category A is the overwhelming majority. Eyeball B/C/D/E magnitudes for plausibility.
- [ ] Cross-check a handful: pick 3 category-A `video_uid`s, confirm they exist in master + have the canonical hash; pick a category-D `video_uid`, HEAD `<creator>/<uuid>.mp4` in storj to sanity-check "genuinely missing."
- [ ] Only after preview looks right: run against prod (read-only). Report counts to the user.
- [ ] Remediation (`--remediate`) is a separate, explicitly-authorized step — do NOT run it automatically.

---

## Notes for the implementer

- **DRY termination rules:** `walk_pages` (pure, unit-tested) and the orchestrator loop must keep identical terminate-conditions. Prefer refactoring to a single async-callback walk if the closure ergonomics stay clean; otherwise keep both and comment the coupling (as noted in Task 6).
- **Confirm-before-final placeholders:** the `'servable'` status literal and the run success-status literal (`'completed'`) are placeholders — grep the existing writers and match them exactly. A mismatch silently mis-categorizes.
- **`--test-threads=1`** on every PG-backed test invocation (known container flake).
- **No Patroni changes. Feature branch only. `--remediate` never runs without explicit user go.**
