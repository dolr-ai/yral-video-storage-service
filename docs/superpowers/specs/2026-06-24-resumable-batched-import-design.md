# Resumable, Batched video_index Import Design

**Date:** 2026-06-24
**Owner key:** `prakash-bhatt-yral`
**Status:** approved design, pending implementation plan

## Problem

`crate::jobs::media_imports::import_current_video_index` (Phase 1C) cannot complete the production backfill. Prod `video_index` is ~700k rows today and headed toward ~1M. The current job:

1. **Loads every row into memory up front** (`fetch_legacy_rows` does one un-paged `SELECT`), an OOM risk at 1M.
2. **Is non-resumable** — no cursor. Every run re-scans from row 1, so after any interruption a retry re-processes all already-imported rows (expensive per-row work) before reaching new ones.
3. **Commits one transaction per row** — one fsync per row, ~40–60 rows/sec against the remote Postgres → multi-hour runs.

A prod run was observed to stop at 158,369 rows: a merge-to-main triggered a redeploy that replaced the container and killed the in-flight background task. The data persisted (separate DB), but the current job's non-resumability means the retry re-churns the 158k before making new progress, and the per-row commit rate makes a full pass take hours — fragile against the next deploy.

## Goal

Make `import_current_video_index` complete reliably at 700k–1M:
- **Resumable** — survive deploys/crashes/cancels; a re-run continues cheaply, not from scratch.
- **Memory-safe** — stream, never load the whole table.
- **Fast** — far fewer commits/fsyncs.

No change to the HTTP route, the `mirror-client` command, the `media_phash` job, or the schema. Same `POST /media/import/video-index` / `media-import`.

## Decisions (locked in brainstorming + design review)

1. **Resume via skip-existing (anti-join), not a persisted cursor.** Re-runs import whatever is missing by filtering out rows already present in the master table. Stateless across runs; self-correcting (no cursor blind spot for out-of-order inserts).
2. **Batch commits with optimistic batch + per-row fallback** — NOT savepoint-per-row. (Design review found savepoint-per-row at batch 500 exceeds Postgres's 64-subtransaction-ID cache → SLRU/`SubtransSLRU` degradation. Optimistic batching uses zero subtransactions on the happy path.)
3. **Batch size 500**, env-overridable.

## Design

### 1. Scan — skip-existing, paged

Replace `fetch_legacy_rows`'s load-all with a paged anti-join, paging by a `video_id` cursor (never `OFFSET` — O(n²) at 1M):

```sql
SELECT v.video_id, v.storj_key, v.hetzner_key, v.phash, v.phash_kind, v.phash_version
FROM video_index v
WHERE ($1::TEXT IS NULL OR v.video_id > $1)
  AND NOT EXISTS (
    SELECT 1 FROM all_servable_videos_on_yral m WHERE m.video_id = v.video_id
  )
ORDER BY v.video_id
LIMIT $2
```

- Use the **same cursor convention as the existing `videos_missing_canonical_phash`** helper (`repo.rs`): `after: Option<&str>`, guarded by `($1::TEXT IS NULL OR v.video_id > $1)`. Start `after = None`, advance to the last `video_id` of each page, stop when a page returns zero rows. (Do NOT use an empty-string sentinel — a row whose `video_id` is literally `''` would be skipped, and the `Option`/NULL guard is the established codebase pattern.)
- Both `video_index.video_id` and `all_servable_videos_on_yral.video_id` are primary keys → the anti-join and the cursor range are index-driven. On a resumed run the already-imported rows are skipped by an index-only check, not re-processed.
- Streams page-by-page → bounded memory regardless of table size.

### 2. Import — optimistic batch, per-row fallback

For each page (a `Vec` of rows held in memory):

- **Happy path:** open ONE transaction, import every row in the page (the existing per-row work — `upsert_servable_video_txn` for master + source `raw_payload`, `upsert_hash_record_txn`, and the two feed-event appends via the serialized outbox helper), then `COMMIT`. **One fsync per page, zero subtransactions.**
- Keyless rows (no `storj_key`/`hetzner_key`) are **not** errors: they record a `media_job_failures` row and continue within the same batch (unchanged Phase 1C behavior).
- **Fallback path:** if importing a row raises a real SQL error (which poisons the open transaction), `ROLLBACK` the batch, then reprocess that same page **row-by-row, each in its own transaction**. This isolates the offending row (recorded in `media_job_failures`) while committing the rest. Genuine row errors are rare, so this path almost never runs.
- **Counter accounting (explicit):** accumulate each page's counts in **local variables**, and fold them into the run `summary` (`scanned_rows`, `imported_media_rows`, `hash_rows_upserted`, `hash_feed_events_appended`, `row_failures`) **only after the batch `COMMIT` succeeds** — so a rolled-back batch contributes nothing and there is no "restore" to do. On the fallback path, accumulate from the per-row commits instead. `scanned_rows` counts **each row exactly once** (a row reprocessed by the fallback is not double-counted). This preserves the exact totals semantics the current tests assert (e.g. `scanned_rows == 2`).
- **Crash-consistency:** because a batch commits atomically, a process kill mid-batch rolls the batch back — no half-imported rows. The next run re-imports that page via skip-existing.

### `limit` semantics under paging

`limit: Option<i64>` stays a **global cap on rows scanned across the whole run** (unchanged meaning; still validated `>= 0`). Implement by tracking `remaining`: each page fetches `LIMIT min(batch_size, remaining)`, decrement `remaining` by the page's row count, and stop when `remaining == 0` or a page is empty. `None` means unbounded (the full backfill).

### 3. Cancellation, job lifecycle

- `cancel.is_cancelled()` checked between pages (the existing media cancel token, already wired). On cancel: stop, finalize the run `cancelled`, return the partial summary.
- `media_job_runs` lifecycle unchanged: `running` → `succeeded` / `succeeded_with_failures` / `cancelled` / `failed`. `media_job_runs.cursor` remains null (skip-existing is stateless).
- The feed advisory lock (`pg_advisory_xact_lock`) is acquired once per batch transaction and held until that batch commits — correct (the batch's cursors become visible atomically and in order). It serializes any concurrent feed writer (e.g. `media_phash`) for the batch duration; acceptable because backfill runs import and pHash sequentially.

### 4. Config

New `MEDIA_IMPORT_BATCH_SIZE` in `src/consts.rs`, modeled exactly like the existing `SCAN_PAGE_SIZE` — a `Lazy<AtomicI64>` defaulting to **500**, env-overridable (so it's runtime-tunable via the same mechanism, not a hard `const`). 500 keeps the transaction small enough to avoid long advisory-lock holds while cutting fsyncs ~500×.

## Accepted limitations (documented, not bugs)

- **No update re-sync.** Skip-existing keys on master-row *presence*, so it will not re-touch a row already in master. This covers two cases, both acceptable: (a) a `video_index` row whose data changed after first import (e.g. a `storj_key` added later) is not re-synced; (b) a row left in master *without* its hash row / hash feed event by an older partial run of the previous per-row job is skipped (master present) and its hash event is never emitted. Both are fine for the effectively append-only legacy table. No full-rescan mode (YAGNI) until update-sync is actually needed.
- **Keyless rows re-attempted each run.** They never get a master row, so the anti-join re-selects them on every run and re-records the same `media_job_failures` row (upsert, no duplicates). Tiny minority; acceptable.

## Testing

DB-backed (testcontainers), seeding `video_index` + `all_servable_videos_on_yral` directly:

1. **Skip-existing:** seed N legacy rows, pre-insert M into master, run import → only the N−M missing rows are imported.
2. **Resume after partial:** import with a small limit/interruption, then re-run → only the remainder is imported; no duplicate master/source rows, no new feed events for already-imported rows.
3. **Batched commit:** a page imports in a single transaction (assert all rows + their feed events are present after one run).
4. **Per-row fallback isolation:** force one row in a page to raise a SQL error → that row is recorded in `media_job_failures`, the rest of the page commits, run ends `succeeded_with_failures`.
5. **Cancel between batches:** pre-cancelled token → stops early, run finalized `cancelled`, partial progress committed.
6. **Idempotent re-run:** running twice over the same data produces no duplicate source rows and no new feed events on the second run.

## Files to touch

- `src/jobs/media_imports.rs` — paged skip-existing scan, batched import loop with fallback, cancellation between pages. Replaces the load-all `fetch_legacy_rows` and the row-at-a-time loop.
- `src/consts.rs` — add `MEDIA_IMPORT_BATCH_SIZE`.

No changes to `src/routes/media.rs`, `crate::jobs::media_phash`, `mirror-client`, or the schema.
