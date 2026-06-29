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
| `src/jobs/media_phash.rs` | extract `hash_one_video` (shared single-video hash) | Modify |
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

- [ ] **Step 3: Implement** `register_master_row` (sync core) + `on_video_ingested` (best-effort wrapper). `register_master_row` calls `upsert_servable_video` with `ServableVideoInput { video_id, source_kind: "videogen", source_ref: Some(video_id), servable_status: "servable", storage_provider: Some(src.storage_provider), bucket: Some(&src.bucket), object_key: Some(&src.object_key), discovered_from: "videogen_completion", ..all None }`.

```rust
pub async fn on_video_ingested(state: &AppState, video_id: &str, object_key: &str, bucket_url: &str) {
    let src = match resolve_source(bucket_url, object_key) {
        Ok(s) => s,
        Err(e) => { tracing::warn!(video_id, %e, "ingest: unresolved source, skipping inline register (sweep will catch it)"); return; }
    };
    match crate::db::connect(&state.db_url).await {
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

## Task 5: Wire `on_video_ingested` into videogen completion

**Files:**
- Modify: `src/routes/videogen/complete.rs` (`handle_success_completion`, after `create_draft` succeeds)
- Test: existing `complete.rs` tests must still pass; add one asserting the call is best-effort (does not change the 200).

- [ ] **Step 1:** Read `handle_success_completion` (success branch returns `Ok(StatusCode::OK)`). Add, before the `Ok`, a best-effort call. Completion handlers use a `CompletionDeps` trait — add `on_video_ingested` as a no-op-defaulted dep method OR call `crate::jobs::ingest::on_video_ingested(state, …)` directly if the handler has `AppState`. Inspect which is available and follow the existing pattern (the `deps` test seam).

- [ ] **Step 2: Add/confirm a test** that a success completion still returns `OK` even when ingest registration would fail (best-effort). Reuse the existing `FakeCompletionDeps` pattern.

- [ ] **Step 3:** Implement the wiring (spawn-free; it's a cheap awaited upsert, but must not turn a completion failure — wrap so its error never changes the response).

- [ ] **Step 4: Run** — `cargo test -p storj-interface --bin storj-interface complete -- ` → PASS (all existing + new).

- [ ] **Step 5: Commit** — `feat: register videogen completions into master for pHash`.

---

## Task 6: Extract `hash_one_video` (shared helper)

**Files:**
- Modify: `src/jobs/media_phash.rs`
- Test: existing `media_phash` tests must stay green; add a focused unit if feasible.

This is a refactor — behavior-preserving. Lift the download+ffmpeg closure (currently embedded in the `buffer_unordered` stream, ~lines 131–205) into:

```rust
pub(crate) async fn hash_one_video(
    s3: &S3Client,
    storj: &StorjS3Client,
    storage_provider: Option<&str>,
    object_key: Option<&str>,
) -> Result<VideoHashResult, (&'static str, String)> { /* download (hetzner|storj) + ffmpeg */ }
```

- [ ] **Step 1:** Extract, calling it from the stream closure (pass `row.storage_provider.as_deref()`, `row.object_key.as_deref()`).
- [ ] **Step 2: Run the full media_phash suite** — `cargo test -p storj-interface --lib media_phash -- --test-threads=1` → all PASS (no behavior change).
- [ ] **Step 3:** (optional) add a direct `hash_one_video` error-path unit (missing key → `("phash_download", _)`).
- [ ] **Step 4: Commit** — `refactor: extract hash_one_video from media_phash stream`.

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
- `cas_guarded(flag: &Arc<AtomicBool>, fut)` — `compare_exchange(false,true)`; if held, skip (return a "skipped" marker); else run `fut`, release on drop (reuse the existing `JobGuard` pattern from `routes/media.rs`).

- [ ] **Step 4: Run → pass.**

- [ ] **Step 5: Commit** — `feat: worker primitives — heartbeat renew, cas guard, discovery_due`.

---

## Task 8: Worker loop

**Files:**
- Modify: `src/jobs/worker.rs`
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

- [ ] **Step 3: Implement `run_one_pass` + `run_worker_loop`** per spec §3: acquire lease → (own?) drain under `with_heartbeat_renew` + `cas_guarded(job_media_phash_running)` calling `media_phash::run(.., requested_by="sweep_drain")` → if `discovery_due` (from DB) run scan(full)+scan(full)+import under renew + `cas_guarded(job_media_import_running)` then `set_last_discovery_at` → every pass wrapped so errors/panics are logged+sentry and the loop continues → `select!` on `cancel` for shutdown + `release_lease`.

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

- [ ] **Step 2:** In `run_server`, after `AppState` is built and before/around `axum::serve`, if `RUN_SWEEP_WORKER`: `tokio::spawn(crate::jobs::worker::run_worker_loop(state.clone(), cancel.clone()))`. `$me` = `hostname::get()` (add `hostname` crate or read `HOSTNAME`/`std::env`) — stable per box. Pass the existing graceful-shutdown `cancel` token.

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
- [ ] **Step 2:** Ensure `docker-compose.ha.yml` app service passes these env (add `RUN_SWEEP_WORKER: ${RUN_SWEEP_WORKER:-true}` etc. to the app `environment:` block, mirroring `PHASH_CONCURRENCY`).
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

- **Don't double-path hashing:** the worker drains via `media_phash::run` only. No per-completion hash spawn.
- **pgbouncer transaction pooling:** every lease op is one statement / its own txn. Never hold a session-level lock.
- **Best-effort registration:** `on_video_ingested` must never fail a completion. The drain/discovery is the correctness guarantee.
- **CAS guard:** `media_phash::run` is NOT self-guarded — the worker must `cas_guarded(job_media_phash_running)` itself (like `routes/media.rs::run_phash` does).
- **`scan_storj::run`** takes the Storj client; **`scan_hetzner::run`** takes the S3 client — match each signature (read both before calling).
