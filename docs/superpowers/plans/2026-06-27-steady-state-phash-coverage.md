# Steady-State pHash Coverage Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Keep canonical pHash coverage at ~100% with no manual intervention — register new videos into the master at ingest, and a single leased background worker drains the missing set + periodically rediscovers the buckets.

**Architecture:** Inline registration (videogen completion → `on_video_ingested` → master upsert) plus one in-app leased worker that (a) drains `videos_missing_canonical_phash` via the existing `media_phash::run`, and (b) runs a daily full-bucket discovery scan. Single-runner across 3 boxes via a `sweep_lease` heartbeat row (pgbouncer transaction-pooling safe). All hashing is single-pathed through the drain.

**Tech Stack:** Rust, Axum, tokio, tokio-postgres, Patroni HA Postgres + pgbouncer. Tests use the in-repo `PgContainer` (Docker) for DB-backed unit tests.

**Spec:** `docs/superpowers/specs/2026-06-27-steady-state-phash-coverage-design.md`

**Conventions:**
- Local build needs `SDKROOT=$(xcrun --show-sdk-path)` prefix (macOS + brew LLVM).
- Lib/repo tests (DB): `cargo test -p storj-interface --lib <name> -- --test-threads=1` (Docker up).
- Route/bin tests: `cargo test -p storj-interface --bin storj-interface <name>`.
- Commit after each task. `cargo fmt` + `cargo clippy --all-targets` before each commit.

---

## File Structure

| File | Responsibility | New/Modify |
|---|---|---|
| `src/media_index/schema.rs` | `sweep_lease` table in `SCHEMA_SQL` | Modify |
| `src/media_index/repo.rs` | lease acquire/renew/release/read + discovery-cadence helpers | Modify |
| `src/jobs/ingest.rs` | `resolve_source`, `VideoSource`, `on_video_ingested` | Create |
| `src/jobs/worker.rs` | leased loop, `run_one_pass`, `with_heartbeat_renew`, `cas_guarded`, `discovery_due` | Create |
| `src/jobs/mod.rs` | register `ingest` + `worker` modules | Modify |
| `src/routes/videogen/complete.rs` | best-effort `on_video_ingested` in success branch | Modify |
| `src/routes/media.rs` | lease/sweep status view | Modify |
| `src/consts.rs` | `RUN_SWEEP_WORKER`, `DRAIN_INTERVAL`, `DISCOVERY_INTERVAL`, `SWEEP_LEASE_TTL` | Modify |
| `src/main.rs` | spawn worker in `run_server` (gated) | Modify |
| `crates/mirror-client/src/{lib,main}.rs` | `media-sweep` command for the lease view | Modify |
| `.github/workflows/deploy-prakash-servers.yml` | worker env defaults | Modify |

---

## Task 1: `sweep_lease` schema

**Files:**
- Modify: `src/media_index/schema.rs` (inside the `SCHEMA_SQL` r#"..."# string, before the closing `"#;` at ~line 171)
- Test: `src/media_index/schema.rs` `#[cfg(test)]`

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn sweep_lease_table_created_by_schema() {
    let (_pg, client) = test_client().await; // media_index test helper (see repo.rs tests)
    crate::media_index::init_schema(&client).await.unwrap();
    let row = client
        .query_one(
            "SELECT to_regclass('public.sweep_lease') IS NOT NULL AS exists",
            &[],
        )
        .await
        .unwrap();
    assert!(row.get::<_, bool>("exists"), "sweep_lease must exist");
}
```
(Match the existing schema-test helper name in this file; if schema is applied via a differently-named fn, use that.)

- [ ] **Step 2: Run to verify it fails** — `cargo test -p storj-interface --lib sweep_lease_table_created_by_schema -- --test-threads=1` → FAIL (`to_regclass` null).

- [ ] **Step 3: Add the table to `SCHEMA_SQL`**

```sql
CREATE TABLE IF NOT EXISTS sweep_lease (
    id                SMALLINT PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    owner             TEXT NOT NULL,
    heartbeat         TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_discovery_at TIMESTAMPTZ
);
```

- [ ] **Step 4: Run to verify it passes** — same command → PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt && git add src/media_index/schema.rs
git commit -m "feat: sweep_lease table for steady-state worker election"
```

---

## Task 2: Lease repo helpers

**Files:**
- Modify: `src/media_index/repo.rs`
- Modify: `src/media_index/mod.rs` (re-export new helpers if other modules use them)
- Test: `src/media_index/repo.rs` `#[cfg(test)]`

Helpers (all take `&Client`):
- `acquire_or_renew_lease(client, owner: &str, ttl: Duration) -> Result<bool>` — runs the upsert below, returns `true` iff a row came back (this owner now holds the lease).
- `release_lease(client, owner: &str) -> Result<()>` — `DELETE FROM sweep_lease WHERE id=1 AND owner=$1`.
- `read_lease(client) -> Result<Option<LeaseRow>>` — `SELECT owner, heartbeat, last_discovery_at FROM sweep_lease WHERE id=1`.
- `get_last_discovery_at(client) -> Result<Option<DateTime<Utc>>>`.
- `set_last_discovery_at(client, ts) -> Result<()>` — `UPDATE sweep_lease SET last_discovery_at=$1 WHERE id=1`.

The acquire SQL (ttl interpolated as seconds via `make_interval`/parameter — bind ttl seconds as `$2::double precision`):

```sql
INSERT INTO sweep_lease (id, owner, heartbeat) VALUES (1, $1, now())
ON CONFLICT (id) DO UPDATE
   SET owner = $1, heartbeat = now()
   WHERE sweep_lease.owner = $1
      OR sweep_lease.heartbeat < now() - ($2::double precision * interval '1 second')
RETURNING owner;
```

- [ ] **Step 1: Write failing tests**

```rust
#[tokio::test]
async fn lease_single_owner_and_steal_after_ttl() {
    let (_pg, client) = test_client().await;
    init_schema(&client).await.unwrap(); // existing helper that applies SCHEMA_SQL

    let ttl = std::time::Duration::from_secs(60);
    assert!(acquire_or_renew_lease(&client, "box-a", ttl).await.unwrap(), "first acquire");
    assert!(!acquire_or_renew_lease(&client, "box-b", ttl).await.unwrap(), "fresh foreign lease → skip");
    assert!(acquire_or_renew_lease(&client, "box-a", ttl).await.unwrap(), "owner renews own");

    // Simulate staleness: backdate heartbeat beyond ttl.
    client.execute("UPDATE sweep_lease SET heartbeat = now() - interval '120 seconds' WHERE id=1", &[]).await.unwrap();
    assert!(acquire_or_renew_lease(&client, "box-b", ttl).await.unwrap(), "stale lease stolen");
    let lease = read_lease(&client).await.unwrap().unwrap();
    assert_eq!(lease.owner, "box-b");
}

#[tokio::test]
async fn last_discovery_at_roundtrip() {
    let (_pg, client) = test_client().await;
    init_schema(&client).await.unwrap();
    acquire_or_renew_lease(&client, "box-a", std::time::Duration::from_secs(60)).await.unwrap();
    assert!(get_last_discovery_at(&client).await.unwrap().is_none());
    let now = chrono::Utc::now();
    set_last_discovery_at(&client, now).await.unwrap();
    let got = get_last_discovery_at(&client).await.unwrap().unwrap();
    assert!((got - now).num_seconds().abs() < 2);
}
```

- [ ] **Step 2: Run → fail** — `cargo test -p storj-interface --lib repo::tests::lease repo::tests::last_discovery -- --test-threads=1` → FAIL (fns missing).

- [ ] **Step 3: Implement the five helpers + `LeaseRow` struct** (parameterized SQL, no string interpolation of user data).

- [ ] **Step 4: Run → pass** — same command → PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --all-targets 2>&1 | grep -E "^(warning|error)" || true
git add src/media_index/repo.rs src/media_index/mod.rs
git commit -m "feat: sweep_lease acquire/renew/release/read + discovery cadence helpers"
```

---

## Task 3: `resolve_source` (pure, minimal)

**Files:**
- Create: `src/jobs/ingest.rs`
- Modify: `src/jobs/mod.rs` (add `pub mod ingest;`)
- Test: `src/jobs/ingest.rs` `#[cfg(test)]`

YAGNI: only the videogen/Storj-SFW case is wired now (`bucket_url` built from `STORJ_SFW_SHARE_URL`). Recognize that and produce the Storj triple; unknown → `ResolveError`.

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn resolves_videogen_storj_sfw() {
    // bucket_url shape as produced by generate.rs (built from STORJ_SFW_SHARE_URL).
    let src = resolve_source("https://link.storjshare.io/s/.../yral-sfw", "canister/abc.mp4").unwrap();
    assert_eq!(src.storage_provider, "storj");
    assert_eq!(src.bucket, "yral-sfw");
    assert_eq!(src.object_key, "canister/abc.mp4");
}

#[test]
fn rejects_unknown_host() {
    assert!(resolve_source("https://unknown.example/x", "k").is_err());
}
```
(Before writing the assertion URL, read `src/routes/videogen/generate.rs:1165` + `STORJ_SFW_SHARE_URL` to use the real `bucket_url` shape. Match on a stable marker in it — e.g. the `yral-sfw` bucket segment — not the full URL.)

- [ ] **Step 2: Run → fail** — `cargo test -p storj-interface --lib ingest::tests -- --test-threads=1` → FAIL.

- [ ] **Step 3: Implement**

```rust
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("unrecognized bucket_url: {0}")]
    UnknownSource(String),
}

pub struct VideoSource {
    pub storage_provider: &'static str, // "storj" | "hetzner"
    pub bucket: String,
    pub object_key: String,
}

/// Minimal resolver: videogen uploads are always Storj `yral-sfw` today.
/// Extend (host-parse) when upload-merge introduces other backends.
pub fn resolve_source(bucket_url: &str, object_key: &str) -> Result<VideoSource, ResolveError> {
    if bucket_url.contains("yral-sfw") {
        Ok(VideoSource { storage_provider: "storj", bucket: "yral-sfw".into(), object_key: object_key.to_string() })
    } else {
        Err(ResolveError::UnknownSource(bucket_url.to_string()))
    }
}
```

- [ ] **Step 4: Run → pass.**

- [ ] **Step 5: Commit** — `feat: resolve_source for videogen storj-sfw ingest`.

---

## Task 4: `on_video_ingested` (master registration)

**Files:**
- Modify: `src/jobs/ingest.rs`
- Test: `src/jobs/ingest.rs` `#[cfg(test)]` (DB-backed)

- [ ] **Step 1: Write failing test**

```rust
#[tokio::test]
async fn registers_video_into_missing_set() {
    let (_pg, client) = test_client().await;
    crate::media_index::init_schema(&client).await.unwrap();

    register_master_row(&client, "vid-1", &resolve_source("…yral-sfw…", "k/1.mp4").unwrap())
        .await
        .unwrap();

    // appears in the missing-canonical-phash scan with the right provider/key
    let rows = crate::media_index::videos_missing_canonical_phash(
        &client, HASH_KIND, HASH_VERSION, INPUT_MEDIA_VERSION, None, Some(10), None,
    ).await.unwrap();
    let r = rows.iter().find(|r| r.video_id == "vid-1").expect("registered");
    assert_eq!(r.storage_provider.as_deref(), Some("storj"));
    assert_eq!(r.object_key.as_deref(), Some("k/1.mp4"));
}
```

- [ ] **Step 2: Run → fail.**

- [ ] **Step 3: Implement** `register_master_row` (sync core) + `on_video_ingested` (best-effort wrapper). `register_master_row` calls `upsert_servable_video` with `ServableVideoInput { video_id, source_kind: "videogen", source_ref: Some(video_id), servable_status: "servable", storage_provider: Some(src.storage_provider), bucket: Some(&src.bucket), object_key: Some(&src.object_key), discovered_from: "videogen_completion", ..all None }`. **Scope note:** `upsert_servable_video` alone (writing `all_servable_videos_on_yral`) is sufficient — that's the table the missing-scan reads and it carries provider/key. Do **not** over-build a `servable_video_sources` write; the daily discovery import handles source rows, and hashing doesn't need them.

```rust
// Takes db_url (&str), NOT &AppState — the call site (videogen CompletionDeps) has no AppState.
pub async fn on_video_ingested(db_url: &str, video_id: &str, object_key: &str, bucket_url: &str) {
    let src = match resolve_source(bucket_url, object_key) {
        Ok(s) => s,
        Err(e) => { tracing::warn!(video_id, %e, "ingest: unresolved source, skipping inline register (sweep will catch it)"); return; }
    };
    match crate::db::connect(db_url).await {
        Ok(client) => {
            if let Err(e) = register_master_row(&client, video_id, &src).await {
                tracing::warn!(video_id, error=%e, "ingest: register failed (best-effort; sweep backstop)");
            }
        }
        Err(e) => tracing::warn!(video_id, error=%e, "ingest: db connect failed (best-effort)"),
    }
}
```

- [ ] **Step 4: Run → pass.**

- [ ] **Step 5: Commit** — `feat: on_video_ingested registers videos into master at ingest`.

---

## Task 5: Wire ingest into videogen completion (via a `CompletionDeps` method)

**Files:**
- Modify: `src/routes/videogen/complete.rs`
- Test: existing `complete.rs` tests must still pass (FakeCompletionDeps uses the no-op default).

**Why a deps method (not the outer handler):** verified against code — the outer `complete_video` (407) takes raw `body: Bytes` (does NOT parse `CompleteVideoRequest`) and **moves `state` into `RuntimeCompletionDeps::new(state, config)`**, so it has neither `req` nor a usable `state` afterward. The parsed `req` (with `video_id`/`object_key`/`bucket_url`) + the `deps` both live in `handle_success_completion`. So the registration goes through the `CompletionDeps` trait — production resolves it with `db_url`, the fake no-ops.

- [ ] **Step 1: Add the trait method (default no-op)** to `CompletionDeps`:

```rust
/// Register a completed video into the master table for steady-state pHash.
/// Best-effort; default no-op so test fakes need no change.
async fn register_ingested(&self, _video_id: &str, _object_key: &str, _bucket_url: &str) {}
```

- [ ] **Step 2: Give `RuntimeCompletionDeps` a `db_url` + override the method.**
  - Add field `db_url: String` to the struct; in `RuntimeCompletionDeps::new(state, config)` set `db_url: state.db_url.clone()` **before** `state` is otherwise consumed (read the current `new` body — extract `db_url` alongside `ic_agent`).
  - Override:

```rust
async fn register_ingested(&self, video_id: &str, object_key: &str, bucket_url: &str) {
    crate::jobs::ingest::on_video_ingested(&self.db_url, video_id, object_key, bucket_url).await;
}
```

- [ ] **Step 3: Call it on the success path** in `handle_success_completion`, after `create_draft` succeeds and before `Ok(StatusCode::OK)` (the parsed `video_id`/`object_key`/`bucket_url` are already in scope there):

```rust
deps.register_ingested(video_id, object_key, bucket_url).await; // best-effort; on_video_ingested swallows errors
```

- [ ] **Step 4: Run** — `cargo test -p storj-interface --bin storj-interface complete` → PASS (existing tests use the no-op default, unaffected). Build: `SDKROOT=$(xcrun --show-sdk-path) cargo build`.

- [ ] **Step 5: Commit** — `feat: register videogen completions into master for pHash`.

---

## ~~Task 6: Extract `hash_one_video`~~ — REMOVED (YAGNI)

**Dropped after plan review.** In this register-inline design the ingest path does **not** hash — hashing stays single-pathed through `media_phash::run`. The only hashing code is the `buffer_unordered` stream inside `media_phash.rs`; nothing else calls a single-video hash. Extracting `hash_one_video` would be a pure refactor with **zero new consumers** — drop it. (Spec §6 was a holdover from the abandoned "async compute at completion" model; the spec is otherwise current.)

---

## Task 7: `with_heartbeat_renew` + `cas_guarded` + `discovery_due`

**Files:**
- Create: `src/jobs/worker.rs`
- Modify: `src/jobs/mod.rs` (`pub mod worker;`)
- Test: `src/jobs/worker.rs` `#[cfg(test)]`

- [ ] **Step 1: Write failing tests**

```rust
#[tokio::test]
async fn heartbeat_renew_keeps_lease_fresh_during_long_task() {
    let (_pg, client) = test_client().await;
    init_schema(&client).await.unwrap();
    let client = std::sync::Arc::new(client);
    acquire_or_renew_lease(&client, "box-a", Duration::from_secs(2)).await.unwrap();

    // ttl=2s, renew every ttl/3; a 3s task must NOT let a peer steal.
    with_heartbeat_renew(client.clone(), "box-a", Duration::from_secs(2), async {
        tokio::time::sleep(Duration::from_secs(3)).await;
    }).await;

    // peer cannot steal — heartbeat kept fresh
    assert!(!acquire_or_renew_lease(&client, "box-b", Duration::from_secs(2)).await.unwrap());
}
// NOTE: this test uses real ~3s sleeps (the renew loop hits the real DB, so
// tokio::time::pause won't work here). It's intentionally slow — acceptable for one test.

#[test]
fn discovery_due_logic() {
    let now = chrono::Utc::now();
    assert!(discovery_due(None, Duration::from_secs(86400), now));                       // never run
    assert!(discovery_due(Some(now - chrono::Duration::hours(25)), Duration::from_secs(86400), now));
    assert!(!discovery_due(Some(now - chrono::Duration::hours(1)), Duration::from_secs(86400), now));
}
```

- [ ] **Step 2: Run → fail.**

- [ ] **Step 3: Implement**
- `discovery_due(last: Option<DateTime<Utc>>, interval: Duration, now) -> bool` (pure).
- `with_heartbeat_renew(client, owner, ttl, fut)` — spawn a renew task looping `acquire_or_renew_lease` every `ttl/3` until `fut` completes; `select!`/abort on completion; return `fut`'s output.
- `cas_guarded(flag: &Arc<AtomicBool>, f: impl FnOnce() -> Fut)` — takes a **closure** (not a pre-built future): `compare_exchange(false,true)`; if already held, skip (return a "skipped" marker, do NOT build/run `f`); else run `f()`, release the flag on drop (reuse the existing `JobGuard` pattern from `routes/media.rs`). Passing a closure ensures the expensive drain future is never even constructed when the guard is held.

- [ ] **Step 4: Run → pass.**

- [ ] **Step 5: Commit** — `feat: worker primitives — heartbeat renew, cas guard, discovery_due`.

---

## Task 8: Worker loop

**Files:**
- Modify: `src/jobs/worker.rs`
- Modify: `src/media_index/repo.rs` (add `any_missing_canonical_phash` EXISTS helper for the drain pre-check)
- Test: `src/jobs/worker.rs` `#[cfg(test)]` (integration-style with a real `PgContainer` + tiny intervals)

- [ ] **Step 1: Write failing test** — drive `run_one_pass` once on a seeded missing row; assert it hashes/advances OR (since real ffmpeg is heavy) assert the *control flow*: non-owner skips; owner runs the drain CAS; `discovery_due` false → no scan; a forced error in the pass is caught (loop would continue). Prefer testing `run_one_pass` with injected closures over running real downloads.

```rust
#[tokio::test]
async fn pass_skips_when_not_lease_owner() {
    let (_pg, client) = test_client().await;
    init_schema(&client).await.unwrap();
    acquire_or_renew_lease(&client, "other-box", Duration::from_secs(60)).await.unwrap();
    // our box is not owner → run_one_pass must no-op (drain closure never called)
    let drained = std::sync::Arc::new(AtomicBool::new(false));
    run_one_pass_with(/* me */ "me", &client, /* drain */ { let d=drained.clone(); move || { d.store(true,SeqCst); } }, ..).await;
    assert!(!drained.load(SeqCst));
}
```
(Design `run_one_pass` to take injectable drain/discovery closures so it's testable without real ffmpeg/S3. The production `run_one_pass` wires the real `media_phash::run` / scan / import.)

- [ ] **Step 2: Run → fail.**

- [ ] **Step 3: Implement `run_one_pass` + `run_worker_loop`** per spec §3:
  - acquire lease → (own?)
  - **drain pre-check (avoid `media_job_runs` pollution):** `media_phash::run` calls `insert_job_run` *unconditionally* (media_phash.rs:59), so a drain every ~180s would insert ~480 idle run-rows/day forever. **Gate the drain on a cheap `any_missing` check first** — add `repo::any_missing_canonical_phash(client) -> Result<bool>` using `SELECT EXISTS(SELECT 1 FROM all_servable_videos_on_yral v LEFT JOIN servable_video_hashes h ON h.video_id=v.video_id AND h.hash_kind='phash' AND h.hash_version=$1 AND h.input_media_version=$2 WHERE h.video_id IS NULL LIMIT 1)` (short-circuits at the first missing row — far cheaper than `canonical_phash_coverage`'s full `COUNT(*)` over ~583k rows run every tick). Only if `true` do the drain under `with_heartbeat_renew` + `cas_guarded(job_media_phash_running)` calling `media_phash::run(.., requested_by="sweep_drain")`. This also skips the empty anti-join scan on idle ticks.
  - **discovery:** if `discovery_due` (from DB `last_discovery_at`) run scan(full)+scan(full)+import under renew + `cas_guarded(job_media_import_running)`, then `set_last_discovery_at(now())`.
  - every pass wrapped so errors/panics are logged + sentry and the loop continues (never exits).
  - `select!` on `cancel` for graceful shutdown + `release_lease`.

- [ ] **Step 3b: Add a test** that an empty missing-set drain inserts **no** `media_job_runs` row (the gate works): seed zero missing rows, run a pass as lease owner, assert `SELECT count(*) FROM media_job_runs WHERE requested_by='sweep_drain'` is 0.

> **Known limitation (permanent failures):** the `any_missing` gate prevents idle pollution only when `missing == 0`. A *permanently*-failing row (deleted source, temp key that never finalizes — today ≈9 such) keeps `any_missing == true`, so the drain re-runs **every tick** — re-inserting a run row and **re-downloading the dead videos every ~180s**. That's the deferred dead-letter problem (spec §Out of scope) surfacing here. For now it's bounded (small count) and acceptable. **Plan option (not required):** widen the gate to "missing AND not failed within the last `DISCOVERY_INTERVAL`" via a cheap anti-join on `media_job_failures` — a lightweight quarantine that the dead-letter follow-up would later formalize. Decide at implementation time; default is to accept the bounded waste and keep scope small.

- [ ] **Step 4: Run → pass.**

- [ ] **Step 5: Commit** — `feat: leased steady-state worker loop (drain + discovery)`.

---

## Task 9: Spawn worker + consts

**Files:**
- Modify: `src/consts.rs` (env consts/getters)
- Modify: `src/main.rs` `run_server` (spawn, gated)
- Not unit-tested (wiring) — verified by build + boot.

- [ ] **Step 1:** Add to `consts.rs`, following the `PHASH_CONCURRENCY` `Lazy` pattern:
  - `RUN_SWEEP_WORKER` → bool, default `true`.
  - `DRAIN_INTERVAL_SECS` → u64, default `180`.
  - `DISCOVERY_INTERVAL_SECS` → u64, default `86400`.
  - `SWEEP_LEASE_TTL_SECS` → u64, default `120`.

- [ ] **Step 2:** In `run_server`, after `AppState` is built and before/around `axum::serve`, if `RUN_SWEEP_WORKER`: `tokio::spawn(crate::jobs::worker::run_worker_loop(state.clone(), me, cancel.clone()))`. Pass the existing graceful-shutdown `cancel` token.
  - **`$me` = `NODE_NAME`** (`server_1/2/3`), read via `std::env::var("NODE_NAME").unwrap_or_else(|_| hostname fallback)`. **Not** container hostname — in Docker the hostname is the container ID, which **changes on every redeploy** (`compose up` recreates the container) → `$me` would churn and the box couldn't renew its own lease after a deploy (waits a full TTL). `NODE_NAME` is per-box and stable across redeploys (the workflow already exports it). Requires the compose wiring in Task 11 Step 2.

- [ ] **Step 3: Build** — `SDKROOT=$(xcrun --show-sdk-path) cargo build` → OK.

- [ ] **Step 4:** Manual boot note (local): set `RUN_SWEEP_WORKER=false` to disable; with DB up, confirm one log line "sweep worker: started".

- [ ] **Step 5: Commit** — `feat: spawn leased sweep worker in run_server (gated by RUN_SWEEP_WORKER)`.

---

## Task 10: Lease/sweep observability view

**Files:**
- Modify: `src/media_index/repo.rs` (reuse `read_lease`)
- Modify: `src/routes/media.rs` (new handler `media_sweep` or extend `media_jobs_status`)
- Modify: `src/main.rs` (route + ApiDoc), HMAC-guarded like the other media routes
- Modify: `crates/mirror-client/src/{lib,main}.rs` (`media-sweep` command)
- Modify: `crates/mirror-client/README.md` (document `media-sweep` in the media-ownership command table + an example, like the other `media-*` commands)
- Test: `src/routes/media.rs` `#[cfg(test)]` + mirror-client arg test

- [ ] **Step 1: Write failing test** — handler returns `{owner, heartbeat, last_discovery_at}` (nullable when no lease).
- [ ] **Step 2: Run → fail.**
- [ ] **Step 3: Implement** the handler (signed GET `/media/sweep/status`), response struct, route registration, and the `media-sweep` client command (mirrors `media-status`).
- [ ] **Step 4: Run** — `cargo test -p storj-interface --bin storj-interface media_sweep` + `cargo test -p mirror-client` → PASS.
- [ ] **Step 5: Commit** — `feat: media-sweep lease/liveness view (server + client)`.

---

## Task 11: Deploy workflow env defaults

**Files:**
- Modify: `.github/workflows/deploy-prakash-servers.yml` (the `APP_VARS` block — runs on all 3 since `RUN_APP=true`)
- Not unit-testable; YAML validate + review.

- [ ] **Step 1:** Add to `APP_VARS` (near `PHASH_CONCURRENCY='4'`): `export RUN_SWEEP_WORKER='true'; export DRAIN_INTERVAL_SECS='180'; export DISCOVERY_INTERVAL_SECS='86400'; export SWEEP_LEASE_TTL_SECS='120';`
- [ ] **Step 2:** Ensure `docker-compose.ha.yml` **app** service `environment:` block passes these (mirroring `PHASH_CONCURRENCY`):
  - `RUN_SWEEP_WORKER: ${RUN_SWEEP_WORKER:-true}`
  - `DRAIN_INTERVAL_SECS: ${DRAIN_INTERVAL_SECS:-180}`
  - `DISCOVERY_INTERVAL_SECS: ${DISCOVERY_INTERVAL_SECS:-86400}`
  - `SWEEP_LEASE_TTL_SECS: ${SWEEP_LEASE_TTL_SECS:-120}`
  - **`NODE_NAME: ${NODE_NAME}`** — currently only on the patroni service (compose line 52); the app service needs it as the worker's stable `$me` (Task 9). The workflow already exports `NODE_NAME` per node.
- [ ] **Step 3: Validate** — `python3 -c "import yaml; yaml.safe_load(open('.github/workflows/deploy-prakash-servers.yml'))"`.
- [ ] **Step 4:** Update the workflow `deployment-summary` line to mention the sweep worker.
- [ ] **Step 5: Commit** — `ci: steady-state sweep worker env defaults (all 3 nodes)`.

---

## Final verification (after all tasks)

- [ ] `cargo fmt --check`
- [ ] `SDKROOT=$(xcrun --show-sdk-path) cargo build --all-targets`
- [ ] `cargo clippy --all-targets` → no new warnings
- [ ] `cargo test -p storj-interface --lib -- --test-threads=1` (Docker up)
- [ ] `cargo test -p storj-interface --bin storj-interface`
- [ ] `cargo test -p mirror-client`
- [ ] Push branch + PR. **Do not enable on prod until:** the first post-deploy discovery is reviewed (clear any large backlog once via `phash-fleet.sh --of 3`, then let the worker maintain — see spec §Surfaces "Backlog vs steady-state").

## Notes / gotchas for the implementer

- **Single-pathed hashing:** the worker drains via `media_phash::run` only; the ingest path registers (no hash). There is no standalone hash helper (Task 6 dropped — nothing else hashes).
- **Drain pre-check:** never call `media_phash::run` on an empty missing-set — it inserts a `media_job_runs` row unconditionally. Gate on `missing_canonical_phash > 0` first (Task 8).
- **`$me` = `NODE_NAME`, not container hostname** — hostname churns on redeploy (new container ID) and breaks lease self-renewal. `NODE_NAME` is stable per box; wire it into the app compose env (Task 11).
- **pgbouncer transaction pooling:** every lease op is one statement / its own txn. Never hold a session-level lock.
- **Best-effort registration:** `on_video_ingested(db_url, …)` must never fail a completion. Wire via a `CompletionDeps::register_ingested` method (default no-op; `RuntimeCompletionDeps` holds `db_url` + overrides), called from `handle_success_completion`. The outer `complete_video` handler can't host it (no parsed `req`; `state` is moved into the deps). The drain/discovery is the correctness guarantee.
- **CAS guard:** `media_phash::run` is NOT self-guarded — the worker must `cas_guarded(job_media_phash_running)` itself (like `routes/media.rs::run_phash` does), passing a **closure** so the drain future isn't built when skipped.
- **`scan_storj::run`** takes the Storj client; **`scan_hetzner::run`** takes the S3 client — match each signature (read both before calling).
