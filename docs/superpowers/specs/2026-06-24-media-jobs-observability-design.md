# Media Jobs Observability + Smooth pHash Progress Design

**Date:** 2026-06-24
**Owner key:** `prakash-bhatt-yral`
**Branch:** new (off `main`)
**Status:** approved design, pending implementation plan

## Problem

Operating the media backfill (583k+ rows, multi-day pHash run) is currently **blind over HTTP**:

- `media-status` shows only `{import_running, phash_running}` booleans.
- `media-audit` shows coverage counts (`total_servable` / `with_canonical_phash` / `missing`).
- **No way to see per-run totals, throughput/ETA, or *why* rows failed** without DB or container-log access.
- `media_phash` downloads+hashes a whole page (`buffer_unordered(...).collect()`) and only *then* commits each row, so `with_canonical_phash` sits flat for a whole page and then jumps — it *looks* stuck mid-page.

During the first prod pHash run, the flat-then-jump progress + no failures view led to a misdiagnosis ("stuck/failing") of a job that was actually working (a 1000-row page in flight). Per-row failures *do* log (`media_phash.rs` `tracing::error!`) and run totals *are* persisted (`media_job_runs.totals`) — the gap is purely **read access over HTTP** plus **legible progress**.

## Goal

Make the backfill observable and its progress legible, without DB/SSH:
- Read recent job runs (status, totals, timing) over HTTP → derive rate/ETA.
- Read a failure summary (why rows fail, how many) over HTTP.
- Smooth `media_phash` progress so coverage climbs continuously.

All read endpoints are HMAC-protected (consistent with every `/media/*` route). No schema change. No new secrets.

## Decisions (locked in brainstorming)

1. **Scope:** read endpoints **and** the `media_phash` stream-persist fix.
2. **Runs endpoint:** return recent runs with raw fields (status, totals, timestamps, error_message); the client/operator derives rate/ETA — no server-side math.
3. **Failures endpoint:** a **grouped summary**, grouped by **`phase`** (low-cardinality) with `count` + up to 5 sample `last_error` strings per phase. Grouping on raw `last_error` is rejected: it embeds per-video keys (e.g. `"hetzner download creator/abc.mp4: 404"`), so it would produce ~one group per row.
4. **Sharper grouping:** split the `media_phash` failure `phase` into `phash_download` vs `phash_decode` (vs the existing persist path) so phase-grouping is informative.
5. **Live per-run progress:** flush `totals` to `media_job_runs` once per page so the runs endpoint shows the *running* job's live scanned/failed (not just completed runs) — this is what turns "is it stuck?" into a glance. Plus: `?job_kind` filter on runs, per-row cancel in the pHash stream loop, and distinct failure samples.

## Design

### 1. Read helpers — `src/media_index/repo.rs`

```rust
pub struct JobRunRow {
    pub job_kind: String,
    pub status: String,
    pub requested_by: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub totals: Option<serde_json::Value>,
    pub error_message: Option<String>,
}

pub async fn recent_job_runs(client: &Client, limit: i64) -> Result<Vec<JobRunRow>, Error>;
// SELECT job_kind, status, requested_by, started_at, finished_at, totals, error_message
// FROM media_job_runs ORDER BY started_at DESC LIMIT $1
```

```rust
pub struct FailureGroup {
    pub phase: String,
    pub count: i64,
    pub samples: Vec<String>, // up to 5 distinct recent last_error values, each truncated
}

pub async fn failure_summary(
    client: &Client,
    job_kind: Option<&str>,
    limit: i64,
) -> Result<Vec<FailureGroup>, Error>;
// counts grouped by phase, plus a bounded sample of last_error per phase.
```

Implementation note for `failure_summary`: group by `phase` for counts, then gather samples. **Do NOT use `SELECT DISTINCT left(last_error,200) ... ORDER BY created_at DESC`** — Postgres rejects `DISTINCT` with an `ORDER BY` on an expression not in the select list. Use one of:
- `DISTINCT ON (left(last_error,200)) left(last_error,200), created_at ... ORDER BY left(last_error,200), created_at DESC` (then take 5), or
- **two queries** (counts grouped by phase; recent rows per phase) deduped/capped in Rust — simplest, recommended.

Filter by `job_kind` when provided. Order groups by `count` desc.

### 2. Routes — `src/routes/media.rs` (HMAC)

- `GET /media/jobs/runs?job_kind=<str>&limit=N` — default 20, clamp to `1..=100`. Optional `job_kind` filter (e.g. just `media_phash` runs). Returns `{ runs: [JobRunView] }` where `JobRunView` mirrors `JobRunRow` with `started_at`/`finished_at` as RFC3339 strings and `totals` passed through as JSON. (`recent_job_runs` takes `job_kind: Option<&str>`.)
- `GET /media/jobs/failures?job_kind=<str>&limit=N` — default 20, clamp `1..=100`. Returns `{ failures: [FailureGroup] }`.

Both registered with `.layer(authorize)`, paths + schemas added to `ApiDoc`. Errors map to generic 500; bad limit handled by clamping.

### 3. Response structs (`Serialize, ToSchema`)

`JobRunView`, `JobRunsResponse { runs }`, `FailureGroupView { phase, count, samples }`, `FailuresResponse { failures }`.

### 4. `mirror-client` — two commands

- lib: `media_runs(limit) -> JobRunsResponse`, `media_failures(job_kind: Option<&str>, limit) -> FailuresResponse` (+ matching `Deserialize` structs), both signed GETs modeled on the existing `audit()`/`media_audit()`.
- bin: `media-runs [--limit N]`, `media-failures [--job-kind X] [--limit N]` dispatch arms + USAGE. Print runs as a table-ish list (incl. totals); print failure groups as `phase  count` then indented samples.

### 5. `media_phash` stream-persist — `run_inner`

Replace, per page:
```rust
let results: Vec<_> = stream::iter(rows).map(download_and_hash).buffer_unordered(concurrency).collect().await;
for (row, res) in results { summary.scanned_rows += 1; persist_one(client, …).await?; }
```
with:
```rust
let mut s = stream::iter(rows).map(download_and_hash).buffer_unordered(concurrency);
while let Some((row, res)) = s.next().await {
    if cancel.is_cancelled() { cancelled = true; break; }  // per-row cancel (drop `s` → in-flight aborted)
    summary.scanned_rows += 1;
    persist_one(client, job_run_id, &row, res, &mut summary).await?;
}
if cancelled { break; }  // exit the page loop
```
- Persists each row as its download+hash completes → `with_canonical_phash` climbs continuously; a kill loses only the ≤`concurrency` in-flight, not the whole page.
- **Per-row cancel:** checking the token each iteration (and dropping the stream `s` on cancel, which aborts the in-flight download futures) makes cancellation respond within ~one row instead of waiting out the whole page — fixing the slow cancel we hit (a 1000-row page had to drain first).
- `persist_one` is unchanged (still its own per-row transaction). Cursor advance, cancel-between-pages, and limit logic are unchanged.
- Trade-off: while a `persist_one` awaits, the buffered download futures aren't polled, so effective concurrency dips slightly during each (fast) persist. Negligible vs download+ffmpeg time; accepted.

### 6a. Live per-run progress (so the runs endpoint is useful for the RUNNING job)

Problem: `media_job_runs.totals` is written **only at completion** (`complete_job_run`). A running job's row has `totals=NULL`, so `GET /media/jobs/runs` shows nothing live for the job we most want to watch.

Fix: both job loops flush progress periodically (once per page) so the run row reflects live state.
- Add `update_job_run_totals(client, job_run_id, &summary) -> Result<()>` to `media_imports.rs` / `media_phash.rs` (or a shared `media_index` helper): `UPDATE media_job_runs SET totals = $2 WHERE id = $1::TEXT::UUID` with the current counter snapshot. (Keeps `status='running'`, `finished_at=NULL` until the existing `complete_job_run`.)
- Call it once per page (after the page's rows are persisted), in both `import_current_video_index_inner` and `media_phash::run_inner`.

Result: `GET /media/jobs/runs` shows the running job's **live `scanned`/`imported`/`failures`**. The operator gets:
- **rate** = `totals.scanned / (now − started_at)` from a single poll,
- **stuck-vs-working** = `totals.scanned` advancing (or not) across two polls — the exact ambiguity that misled us before,
- **failures-so-far** without waiting for completion.

No schema change (uses the existing `totals` column). One extra `UPDATE` per page — negligible. Liveness is two-poll (no server heartbeat timestamp column added, to avoid a migration); rate is single-poll via `started_at`.

### 6. Sharper failure phase

In `media_phash`'s failure path, record `phase` by error class instead of the single `"phash_compute"`:
- tempfile/clone setup + download/fetch errors → `"phash_download"`
- ffmpeg/decode/hash (incl. `spawn_blocking` panic) errors → `"phash_decode"`

**Do this with a typed stage, not string-prefix matching** (the reviewer flagged that `persist_one` currently gets a flat `Result<VideoHashResult, String>`, so a phase can only be recovered by brittle prefix matching otherwise). Change the download-and-hash closure to carry the failed stage, e.g. return `Result<VideoHashResult, (&'static str, String)>` where the `&'static str` is the phase (`"phash_download"` for the tempfile/clone/download steps, `"phash_decode"` for the `spawn_blocking`/`hash_video_with_metadata` step). `persist_one` then records that phase verbatim instead of the hardcoded `"phash_compute"`. Setup errors (tempfile/clone) bucket under `"phash_download"` since they precede decode.

## Surfaces

- **Security:** both routes HMAC-gated. `last_error` samples + `totals` may contain `object_key`/storage paths → acceptable behind HMAC, and `last_error` is truncated (~200 chars) to bound payloads. No new env/secrets.
- **Performance:** `media_job_runs` is tiny (one row per run). `failure_summary` groups by `phase` (a handful of groups) with a bounded sample subquery → cheap even at hundreds of thousands of failure rows. No new index required for low-QPS admin endpoints.
- **Connection pool:** same pre-existing per-request `db::connect`; these are low-QPS admin reads, not materially worsening the tracked pool gap.
- **Deploy interaction:** shipping this **interrupts the running pHash backfill** (deploy replaces the container and resets in-memory `config-set` values to env defaults). The backfill is only ~1005/583k done and is resumable, so this is acceptable — re-run after deploy with the new visibility + a CPU-safe concurrency.

## Testing

DB-backed (testcontainers):
- `recent_job_runs`: insert runs with varied `started_at`, assert newest-first + fields (incl. `totals`) returned.
- `failure_summary`: seed `media_job_failures` across phases (with key-varying `last_error`s), assert grouping by `phase`, correct counts, ≤5 samples per phase, and the `job_kind` filter.
- Route handlers: JSON shape, limit clamping (≤100, ≥1), `job_kind` passthrough.  (Auth layering verified structurally as with the other media routes.)
- Live progress flush: after an import run processes ≥1 page over a seeded `video_index`, assert the `media_job_runs` row's `totals` is non-NULL and reflects progress *before* completion (e.g. call `update_job_run_totals` directly with a snapshot and read it back; the per-page wiring itself lives in the untested loop shell).
- `media_phash`: existing `persist_one` tests still pass (unchanged). The stream-persist change is in the untested orchestration shell (`run_inner`), consistent with how the codebase treats the I/O shell. The phase-split is covered by asserting a forced download-vs-decode failure records the expected `phase` via `persist_one`. **Note:** the existing test that asserts the failure `phase == "phash_compute"` (around `media_phash.rs:806`) must be UPDATED to the new phase values (`phash_download`/`phash_decode`), not merely added to.

Minor: confirm `#[derive(ToSchema)]` works on `JobRunView.totals: Option<serde_json::Value>` (the feed precedent uses a non-Option `Value`); trivial, glance during impl.

## Files to touch

- `src/media_index/repo.rs` — `recent_job_runs(job_kind, limit)`, `failure_summary(job_kind, limit)` (+ `JobRunRow`, `FailureGroup`).
- `src/media_index/mod.rs` — re-exports.
- `src/routes/media.rs` — 2 handlers (`?job_kind`+`?limit`) + response structs + tests.
- `src/main.rs` — 2 route registrations + ApiDoc paths/schemas.
- `src/jobs/media_phash.rs` — stream-persist consumption loop (per-row cancel) + `phash_download`/`phash_decode` phase split + per-page `update_job_run_totals` flush.
- `src/jobs/media_imports.rs` — per-page `update_job_run_totals` flush (live progress for the import job too). Add the shared `update_job_run_totals` helper here or in `media_index` and reuse.
- `crates/mirror-client/src/lib.rs` + `src/main.rs` — 2 commands + structs + USAGE.

No schema change (live progress reuses the existing `media_job_runs.totals` column). No change to the feed.
