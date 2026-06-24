# Media Jobs Operability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the committed media jobs (1C import, 1D pHash) runnable and cancellable over HTTP and drivable from the `mirror-client` Rust CLI, so the full import → pHash → read chain can be operated and tested end-to-end.

**Architecture:** Add a media-specific cancellation token to `AppState` (separate from the mirror `job_cancel`), give `import_current_video_index` a `CancellationToken`, finalize cancelled runs with a new `cancelled` status, expose `POST /media/phash/run` + `POST /media/jobs/cancel` + `GET /media/jobs/status` (all HMAC), and add six `media-*` commands to `mirror-client`. Everything mirrors the existing mirror-job patterns.

**Tech Stack:** Rust 2021, Tokio, Axum, tokio-postgres, `tokio_util::sync::CancellationToken`, utoipa, reqwest (client), `cargo test` with testcontainers.

**Spec:** `docs/superpowers/specs/2026-06-19-media-jobs-operability-design.md`

---

## File Structure

- `src/jobs/media_imports.rs` — add `cancel: &CancellationToken` param + per-row cancel check + `cancelled` finalization; update its 6 existing test call sites (lines ~460, 572, 657, 660, 721, 805).
- `src/jobs/media_phash.rs` — finalize `cancelled` when the loop breaks on cancel (loop already checks the token).
- `src/main.rs` — `AppState` gets `media_job_cancel` + `job_media_phash_running`; initialize both; register 3 routes; add OpenAPI paths/schemas; pass cancel token to the existing import handler call.
- `src/routes/media.rs` — 3 new handlers (`run_phash`, `cancel_media_jobs`, `media_jobs_status`) + response structs + tests; update `import_video_index` to pass the cancel token.
- `crates/mirror-client/src/lib.rs` — 6 client methods + response structs.
- `crates/mirror-client/src/main.rs` — 6 command dispatches + USAGE text.

Reference patterns (read before implementing): `src/routes/mirror.rs::scan_storj` (background start), `src/routes/mirror.rs::cancel_all` (token cancel + swap), `src/routes/mirror.rs::status` (status JSON).

---

## Task 1: Import job cancellation + `cancelled` status

**Files:**
- Modify: `src/jobs/media_imports.rs`
- Test: `src/jobs/media_imports.rs` (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing test**

Add to the tests module. Mirrors the existing import tests (use `test_client`, `init_test_schema`, seed `video_index`). A pre-cancelled token must stop the job before any row is imported and finalize the run `cancelled`.

```rust
#[tokio::test]
async fn import_honors_cancellation_and_finalizes_cancelled() {
    let (_pg, mut client) = test_client().await;
    init_test_schema(&client).await;
    client
        .execute(
            "INSERT INTO video_index (video_id, storj_key, hetzner_key, phash, phash_kind, phash_version)
             VALUES ('vid-cancel', 'creator/vid-cancel.mp4', 'legacy/vid-cancel.mp4', 'abc', 'phash', 'legacy_hex_8x8_v0')",
            &[],
        )
        .await
        .unwrap();

    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel(); // pre-cancelled

    let summary = super::import_current_video_index(&mut client, "test-runner", None, &cancel)
        .await
        .unwrap();

    // No rows processed.
    assert_eq!(summary.scanned_rows, 0);

    // Run finalized as cancelled, not running/succeeded.
    let status: String = client
        .query_one(
            "SELECT status FROM media_job_runs WHERE id = $1::TEXT::UUID",
            &[&summary.job_run_id.to_string()],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(status, "cancelled");
}
```

- [ ] **Step 2: Run test to verify it fails to compile (signature mismatch)**

Run: `cargo test media_imports::tests::import_honors_cancellation -- --test-threads=1`
Expected: FAIL — `import_current_video_index` takes 3 args, not 4.

- [ ] **Step 3: Add the cancel param + per-row check + cancelled finalization**

In `import_current_video_index`, add `cancel: &CancellationToken` as the last param and pass it to `import_current_video_index_inner` (also add the param there). Add the import at the top of the file: `use tokio_util::sync::CancellationToken;`.

In `import_current_video_index_inner`, change the row loop to break on cancel and pick the finalize status:

```rust
let mut cancelled = false;
for row in rows {
    if cancel.is_cancelled() {
        cancelled = true;
        break;
    }
    summary.scanned_rows += 1;
    // ... existing per-row transaction body unchanged ...
}

let status = if cancelled {
    "cancelled"
} else if summary.row_failures == 0 {
    "succeeded"
} else {
    "succeeded_with_failures"
};
complete_job_run(client, &summary, status).await?;
Ok(summary)
```

(The cancel check is at the TOP of the loop, between per-row `tx.commit()` calls — never mid-transaction. Do NOT add paging.)

- [ ] **Step 4: Update the 6 existing in-file test call sites**

There are 6 existing `super::import_current_video_index(...)` calls (lines ~460, 572, 657, 660, 721, 805 — the idempotency test has two). Each gains a final `&CancellationToken::new()` (a fresh, uncancelled token). Add `use tokio_util::sync::CancellationToken;` to the test module if not already imported. Let the compiler confirm none are missed.

- [ ] **Step 5: Run the import tests**

Run: `cargo test media_imports -- --test-threads=1`
Expected: PASS — all existing tests + the new cancellation test (6 total).

- [ ] **Step 6: Commit**

```bash
git add src/jobs/media_imports.rs
git commit -m "feat: make legacy import cancellable with cancelled run status"
```

---

## Task 2: pHash job `cancelled` finalization

**Files:**
- Modify: `src/jobs/media_phash.rs`
- Test: `src/jobs/media_phash.rs` (`#[cfg(test)] mod tests`)

`run_inner`'s loop already `break`s on `cancel.is_cancelled()`, but then calls `complete_job_run(.., run_status(..))` which yields `succeeded`/`succeeded_with_failures`. Make a cancelled break finalize `cancelled`.

- [ ] **Step 1: Write the failing test**

`persist_one` is the testable surface, but cancellation lives in `run_inner` (the untested shell). Instead, test the finalize-status decision directly. Add a small pure helper and test it:

```rust
#[test]
fn cancelled_run_finalizes_cancelled_status() {
    assert_eq!(terminal_status(true, 0), "cancelled");
    assert_eq!(terminal_status(true, 5), "cancelled");
    assert_eq!(terminal_status(false, 0), "succeeded");
    assert_eq!(terminal_status(false, 3), "succeeded_with_failures");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test media_phash::tests::cancelled_run_finalizes -- --test-threads=1`
Expected: FAIL — `terminal_status` not defined.

- [ ] **Step 3: Add `terminal_status` and use it in `run_inner`**

Replace the existing `run_status(row_failures)` helper usage. Add:

```rust
fn terminal_status(cancelled: bool, row_failures: i64) -> &'static str {
    if cancelled {
        "cancelled"
    } else if row_failures == 0 {
        "succeeded"
    } else {
        "succeeded_with_failures"
    }
}
```

In `run_inner`, track a `cancelled` flag set where the loop breaks on the token, and finalize with `terminal_status(cancelled, summary.row_failures)`. Remove the now-unused `run_status` if it has no other callers (otherwise leave it).

- [ ] **Step 4: Run the pHash tests**

Run: `cargo test media_phash -- --test-threads=1`
Expected: PASS — existing tests + the new status test.

- [ ] **Step 5: Commit**

```bash
git add src/jobs/media_phash.rs
git commit -m "feat: finalize cancelled status for media pHash job"
```

---

## Task 3: AppState fields + media routes (run / cancel / status)

**Files:**
- Modify: `src/main.rs` (AppState ~line 42, init ~line 257, router ~line 384, ApiDoc ~line 58)
- Modify: `src/routes/media.rs` (new handlers + structs + tests; update `import_video_index`)

- [ ] **Step 1: Add AppState fields + init**

In `AppState`:
```rust
pub media_job_cancel: Arc<Mutex<CancellationToken>>,
pub job_media_phash_running: Arc<AtomicBool>,
```
In `run_server()` where `app_state` is built:
```rust
media_job_cancel: Arc::new(Mutex::new(CancellationToken::new())),
job_media_phash_running: Arc::new(AtomicBool::new(false)),
```
(`job_media_import_running` already exists; `CancellationToken` is already imported in main.rs — verify.)

- [ ] **Step 2: Write the failing handler tests**

Add to `src/routes/media.rs` tests:

```rust
#[tokio::test]
async fn media_status_reports_running_flags() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let import = Arc::new(AtomicBool::new(false));
    let phash = Arc::new(AtomicBool::new(true));
    let body = super::media_jobs_status_body(&import, &phash);
    assert!(!body.import_running);
    assert!(body.phash_running);
    phash.store(false, Ordering::Release);
    let body = super::media_jobs_status_body(&import, &phash);
    assert!(!body.phash_running);
}
```

(Factor the status response out of the handler into a pure `media_jobs_status_body(import, phash) -> MediaJobsStatus` so it is unit-testable without an HTTP server — same decomposition style as `run_status`/`persist_one`.)

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test routes::media::tests::media_status_reports -- --test-threads=1`
Expected: FAIL — `media_jobs_status_body` / `MediaJobsStatus` not defined.

- [ ] **Step 4: Implement the three handlers + structs**

In `src/routes/media.rs`:

```rust
#[derive(Serialize, ToSchema)]
pub struct MediaJobsStatus {
    pub import_running: bool,
    pub phash_running: bool,
}

pub fn media_jobs_status_body(
    import: &std::sync::atomic::AtomicBool,
    phash: &std::sync::atomic::AtomicBool,
) -> MediaJobsStatus {
    use std::sync::atomic::Ordering;
    MediaJobsStatus {
        import_running: import.load(Ordering::Acquire),
        phash_running: phash.load(Ordering::Acquire),
    }
}
```

- `run_phash` — model on `import_video_index` + `mirror::scan_storj`: `compare_exchange` on `job_media_phash_running` (409 if running), `JobGuard`, clone `media_job_cancel` (lock, clone), clone `s3_client`/`storj_client`/`db_url`, read `?limit` + `?requested_by` (clamp 256 chars), `tokio::spawn` calling `crate::jobs::media_phash::run(s3, storj, db_url, cancel, limit, &requested_by)`, log/sentry on error, return `StatusCode::ACCEPTED`. `#[utoipa::path(post, path="/media/phash/run", tag="media", ...)]` with 202/409/401.
- `cancel_media_jobs` — model on `mirror::cancel_all` (lock via `lock().unwrap_or_else(|e| e.into_inner())`, clone old token, assign a fresh `CancellationToken::new()` into the guard, then `.cancel()` the old one outside the lock). Return `Json` of a small `{message, cancelled: ["media_import","media_phash"]}` response — define `MediaCancelResponse` with `#[derive(Serialize, ToSchema)]` (the `ToSchema` derive is required for the ApiDoc schema registration in Step 5 to compile). `#[utoipa::path(post, path="/media/jobs/cancel", ...)]` 200/401.
- `media_jobs_status` — `State(AppState)`, return `Json(media_jobs_status_body(&state.job_media_import_running, &state.job_media_phash_running))`. `#[utoipa::path(get, path="/media/jobs/status", ...)]` 200/401.

Update `import_video_index` to pass the cancel token: clone `state.media_job_cancel` (lock+clone) and pass `&cancel` as the new 4th arg to `import_current_video_index`.

- [ ] **Step 5: Register routes + OpenAPI in main.rs**

Add three `.route(...)` entries near the existing `/media/*` routes, each `.with_state(app_state.clone()).layer(middleware::from_fn(authorize))`:
```
POST /media/phash/run     -> routes::media::run_phash
POST /media/jobs/cancel   -> routes::media::cancel_media_jobs
GET  /media/jobs/status   -> routes::media::media_jobs_status
```
Add to `ApiDoc` `paths(...)`: `routes::media::run_phash`, `routes::media::cancel_media_jobs`, `routes::media::media_jobs_status`. Add to `components(schemas(...))`: `routes::media::MediaJobsStatus`, `routes::media::MediaCancelResponse`.

- [ ] **Step 6: Build + run tests**

Run: `cargo build && cargo test routes::media -- --test-threads=1`
Expected: PASS — build clean (main.rs compiles), media route tests green.

- [ ] **Step 7: Commit**

```bash
git add src/main.rs src/routes/media.rs
git commit -m "feat: media phash run, cancel, and status routes"
```

---

## Task 4: mirror-client media commands

**Files:**
- Modify: `crates/mirror-client/src/lib.rs` (methods + response structs)
- Modify: `crates/mirror-client/src/main.rs` (dispatch + USAGE)
- Test: `crates/mirror-client/src/main.rs` (`#[cfg(test)]` for arg parsing) or lib

- [ ] **Step 1: Write the failing test (arg parsing)**

Add to `crates/mirror-client/src/main.rs` tests (mirror the existing `parse_limit` style). Add a `parse_after` helper and test it:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_after_flag() {
        let args = vec!["bin".into(), "media-feed".into(), "--after".into(), "42".into()];
        assert_eq!(parse_after(&args), Some(42));
        let none: Vec<String> = vec!["bin".into(), "media-feed".into()];
        assert_eq!(parse_after(&none), None);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mirror-client parses_after -- --nocapture`
Expected: FAIL — `parse_after` not defined.

- [ ] **Step 3: Add lib response structs + methods**

In `crates/mirror-client/src/lib.rs`:

```rust
#[derive(Debug, serde::Deserialize)]
pub struct CoverageStats {
    pub total_servable: i64,
    pub with_canonical_phash: i64,
    pub missing_canonical_phash: i64,
}

#[derive(Debug, serde::Deserialize)]
pub struct MediaJobsStatus {
    pub import_running: bool,
    pub phash_running: bool,
}

#[derive(Debug, serde::Deserialize)]
pub struct MediaFeedEvent {
    pub cursor: i64,
    pub event_kind: String,
    pub video_id: String,
    pub hash_kind: Option<String>,
    pub hash_version: Option<String>,
    pub input_media_version: Option<String>,
    pub payload: serde_json::Value,
    pub created_at: String,
}

#[derive(Debug, serde::Deserialize)]
pub struct MediaFeedResponse {
    pub events: Vec<MediaFeedEvent>,
}
```

Methods on `MirrorClient` (POSTs reuse `post_job` where the shape matches; otherwise add focused helpers):

```rust
pub async fn media_import(&self, limit: Option<u64>) -> Result<(), MirrorError> {
    self.post_job("/media/import/video-index", limit, None, None).await
}

pub async fn media_phash(&self, limit: Option<u64>) -> Result<(), MirrorError> {
    self.post_job("/media/phash/run", limit, None, None).await
}
```

For `media_cancel` (POST returns 200, not 202) add a dedicated POST helper that accepts 200, and `media_status` / `media_audit` / `media_feed` as signed GETs returning the deserialized structs (model on the existing `audit()` GET method; `media_feed` adds `?after=&limit=` query params). `media_cancel` may return its JSON body as `serde_json::Value` or a typed struct.

- [ ] **Step 4: Add `parse_after` + 6 command dispatches + USAGE in main.rs**

Add `parse_after` (mirror `parse_limit`). Add match arms for `media-import`, `media-phash`, `media-cancel`, `media-status`, `media-audit`, `media-feed` that call the lib methods and print results (mirror the existing arms' print style). Add the six commands + their flags to the `USAGE` string.

- [ ] **Step 5: Build + test the client**

Run: `cargo build -p mirror-client && cargo test -p mirror-client`
Expected: PASS — client builds, arg-parse test green.

- [ ] **Step 6: Commit**

```bash
git add crates/mirror-client/src/lib.rs crates/mirror-client/src/main.rs
git commit -m "feat: add media-* commands to mirror-client"
```

---

## Task 5: Final verification

**Files:** none (verification only)

- [ ] **Step 1: Format + lint**

Run: `cargo fmt && cargo clippy --all-targets --all-features 2>&1 | grep -c '^warning'`
Expected: no NEW warnings beyond the known pre-existing videogen lints (`generate.rs`, `complete.rs`).

- [ ] **Step 2: Focused suites**

Run each:
```bash
cargo test media_imports -- --test-threads=1
cargo test media_phash -- --test-threads=1
cargo test routes::media -- --test-threads=1
cargo test -p mirror-client
```
Expected: all PASS.

- [ ] **Step 3: Broad build/test**

Run: `cargo build && cargo test --lib --bins -- --test-threads=1`
Expected: 0 failed.

- [ ] **Step 4: Commit any fmt-only changes**

```bash
git add -A && git commit -m "style: cargo fmt" || true
```

---

## Manual Preview End-to-End (run after merge/deploy, not a code task)

Driven by `mirror-client` against the HMAC-protected preview (`MIRROR_SERVICE_URL`, `SERVICE_SECRET_TOKEN`). Requires seeding a bounded `video_index` subset (hundreds of rows from `../video_fingerprint_index_20260512_131721.sql`) into the preview DB (`138.201.129.173:5433`, firewalled — via the box).

1. `mirror-client media-import --limit 200` → 202.
2. `mirror-client media-status` → `import_running` toggles true→false.
3. `mirror-client media-audit` → `{total_servable: ~200, with_canonical_phash: 0, missing_canonical_phash: ~200}`.
4. `mirror-client media-feed --limit 10` → `media_visibility_changed` + `hash_upserted` (legacy) events.
5. `mirror-client media-phash --limit 50` → 202; then `media-audit` → `with_canonical_phash` rises.
6. `mirror-client media-cancel` mid-run → `media-status` shows both false; a run row is finalized `cancelled`.
