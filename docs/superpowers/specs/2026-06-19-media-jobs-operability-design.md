# Media Jobs Operability Design

**Date:** 2026-06-19
**Owner key:** `prakash-bhatt-yral`
**Branch:** `prakash/migrate-phash`
**Status:** approved design, pending implementation plan

## Goal

The media-ownership jobs from the foundation work are callable from code and tests but cannot be operated on a running deployment:

- `media_imports::import_current_video_index` (Phase 1C) — exposed via `POST /media/import/video-index`, but **not cancellable** (no cancellation token).
- `media_phash::run` (Phase 1D) — **wired to no route at all**: it cannot be started or cancelled over HTTP.
- `crates/mirror-client` — the HMAC-signing Rust CLI — has **no `/media/*` commands**.

This slice makes both media jobs runnable and cancellable over HTTP and drivable from the Rust client, so the full `1C import -> 1D pHash -> 1E read` chain can be operated and tested end-to-end (including on the PR preview deployment).

Out of scope: connection pooling, rate limiting, and the other pre-existing service-wide items already tracked in `docs/superpowers/plans/2026-06-17-phash-deployment-todo.md`. No changes to the hashing algorithms, schema semantics, or the dedup/Milvus boundary.

## Background / current state

- `AppState` (`src/main.rs`) holds a single shared `job_cancel: Arc<Mutex<CancellationToken>>` plus per-job `Arc<AtomicBool>` running flags. `/mirror/jobs/cancel-all` cancels that token and swaps in a fresh one. Mirror job handlers clone the token, spawn the job with a `JobGuard`, and return `202 Accepted`; a second start while running returns `409 Conflict`.
- `job_media_import_running: Arc<AtomicBool>` already exists (added in Phase 1E).
- `media_phash::run(s3, storj, db_url: String, cancel: CancellationToken, limit, requested_by: &str)` already accepts a cancellation token and checks it per batch (it opens its own connection from `db_url` internally) — it just has no caller wired to a route. Its loop currently `break`s on cancel and then finalizes via `run_status(..)`, yielding `succeeded`/`succeeded_with_failures`; the `cancelled` finalization is a real new change, not a no-op.
- `media_job_runs.status` currently takes `running` / `succeeded` / `succeeded_with_failures` / `failed`.
- All `/media/*` routes are HMAC-protected (`authorize` middleware).

## Decisions (locked during brainstorming)

1. **Separate media cancellation**, not the shared mirror token. Media jobs get their own token + cancel endpoint, independent of the mirror family.
2. **Cancel covers BOTH media jobs** — `import` (1C) and `media_phash` (1D). This requires adding a cancellation token to `import_current_video_index`.
3. **Route surface:** add `POST /media/phash/run`, `POST /media/jobs/cancel`, `GET /media/jobs/status`. All HMAC-protected.
4. **Client:** add 6 commands to `mirror-client`: `media-import`, `media-phash`, `media-cancel`, `media-status`, `media-audit`, `media-feed`.
5. **Cancelled runs get a distinct `cancelled` status** in `media_job_runs` (not folded into `succeeded_with_failures`).

## Design

### 1. AppState + cancellation

Add to `AppState`:

- `media_job_cancel: Arc<Mutex<CancellationToken>>` — media-specific cancellation, separate from `job_cancel`.
- `job_media_phash_running: Arc<AtomicBool>` — running guard for the pHash job (the import flag `job_media_import_running` already exists).

Initialize both in `run_server()` alongside the existing flags.

### 2. `import_current_video_index` cancellation (Phase 1C signature change)

New signature:

```rust
pub async fn import_current_video_index(
    client: &mut Client,
    requested_by: &str,
    limit: Option<i64>,
    cancel: &CancellationToken,
) -> Result<ImportSummary, ImportError>
```

- Note: unlike `media_phash` (which pages via a cursor), `import_current_video_index` does a single un-paged `fetch_legacy_rows` then loops one row per transaction. The natural cancellation point is therefore **per row, at the top of the `for row in rows` loop, between `tx.commit()` calls** — never mid-transaction. Do NOT introduce paging just to create a "batch" boundary.
- On cancellation: stop processing further rows, finalize the run with status `cancelled`, and return the partial `ImportSummary`. Already-committed per-row transactions remain; the job is resumable on a later run.
- The existing `POST /media/import/video-index` handler passes a clone of `media_job_cancel`.

### 3. `media_job_runs` cancelled status

- Add `cancelled` to the accepted status set. Both jobs write `cancelled` when their loop observes the token before natural completion.
- `run_status()` helpers stay as-is for the non-cancelled terminal states; cancellation is handled on its own path (the loop breaks and finalizes `cancelled`).

### 4. New routes (`src/routes/media.rs`), all HMAC

- `POST /media/phash/run?limit=<i64>&requested_by=<str>` — `compare_exchange` on `job_media_phash_running` (409 if already running), `JobGuard`, clone `media_job_cancel` + the S3/Storj clients, `tokio::spawn` `media_phash::run`, return `202 Accepted`. Mirrors the mirror-job start pattern. `requested_by` clamped to 256 chars (as the import handler already does).
- `POST /media/jobs/cancel` — lock `media_job_cancel`, call `.cancel()`, swap in a fresh `CancellationToken` (the `cancel_all` pattern), return `200`. Stops both running media jobs.
- `GET /media/jobs/status` — return `{ "import_running": bool, "phash_running": bool }` from the two atomics.

Register all three in the router with `.layer(authorize)`, and add their paths + response schemas + reuse the `media` OpenAPI tag.

### 5. `mirror-client` — 6 media commands

Add methods to the `mirror_client` library (each signs `METHOD\nPATH\nTIMESTAMP` via the existing helper) and dispatch + USAGE entries to the bin:

- `media-import` (`--limit`) -> `POST /media/import/video-index`
- `media-phash` (`--limit`) -> `POST /media/phash/run`
- `media-cancel` -> `POST /media/jobs/cancel`
- `media-status` -> `GET /media/jobs/status`
- `media-audit` -> `GET /media/audit/missing-phash`
- `media-feed` (`--after`, `--limit`) -> `GET /media/feed/events`

### Data flow

```
operator / mirror-client
   │  (HMAC-signed)
   ▼
POST /media/import/video-index ──> import_current_video_index(.., &media_job_cancel)
POST /media/phash/run          ──> media_phash::run(.., media_job_cancel.clone(), ..)
POST /media/jobs/cancel        ──> media_job_cancel.cancel() + swap fresh
GET  /media/jobs/status        ──> {import_running, phash_running}
GET  /media/audit/missing-phash, GET /media/feed/events  (observe progress)
```

## Error handling

- Start endpoints: 409 if that job is already running; 202 on accept. Background task failures are logged + sent to Sentry (existing pattern), never surfaced to the HTTP caller.
- Cancel endpoint: idempotent — cancelling when nothing runs is a no-op 200.
- Import/pHash on cancel: finalize `cancelled`, preserve committed work; never leave a run stuck in `running`.

## Testing

**Unit / DB (testcontainers):**
- `import_current_video_index` honors cancellation: seed rows, pass a pre-cancelled token, assert it stops early and the run is finalized `cancelled` (not `running`).
- `media_job_runs` accepts and reports `cancelled`.
- `GET /media/jobs/status` reflects the running flags.
- Route handlers: 409-on-already-running for phash; cancel is a no-op 200 when idle.
- pHash cancellation is already covered by the existing media_phash loop test surface.

**Client:** command dispatch + arg parsing for the 6 new commands.

**Preview end-to-end (manual, the "test everything" pass):**
1. Load a bounded `video_index` subset (hundreds of rows, from the real dump) into the preview DB.
2. `media-import` -> `media-audit` (counts) -> `media-feed` (events) -> `media-phash` -> `media-audit` (canonical now populated) -> exercise `media-cancel` / `media-status`.
3. Driven by the Rust client against the HMAC-protected preview.

## Files to touch

- `src/main.rs` — AppState fields + init, 3 route registrations, OpenAPI paths/schemas.
- `src/routes/media.rs` — 3 handlers + response structs + tests.
- `src/jobs/media_imports.rs` — cancellation param + `cancelled` finalization + tests. The new `cancel: &CancellationToken` param ripples to the ~5 existing test call sites in this file AND the `POST /media/import/video-index` handler in `src/routes/media.rs`; the plan must budget updating those call sites, not only adding new code.
- `src/jobs/media_phash.rs` — `cancelled` finalization (loop already checks the token).
- `src/media_index/` — `media_job_runs` `cancelled` status acceptance if validated anywhere.
- `crates/mirror-client/` — lib methods + bin commands + USAGE.

Do not change the hashing algorithms, the dedup boundary, or unrelated routes.
