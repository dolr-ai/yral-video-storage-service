# Videogen pHash: correct Storj key + exponential failure backoff

**Date:** 2026-06-30
**Status:** Design (approved, pending spec review)
**Author:** Prakash (with Claude)

## Summary

Two coupled fixes to the steady-state pHash inline path:

1. **Core bug:** the videogen-completion inline registration records the wrong
   Storj `object_key` (a bare `<uuid>.mp4` with no principal prefix), so the
   pHash worker's Storj GET 404s and the new video never gets hashed.
2. **Retry policy:** replace the flat 24h `updated_at` quarantine (added in the
   prior steady-state fix) with exponential backoff driven by the existing-but-
   unused `retry_count` / `next_retry_at` columns, so a *fresh* video whose first
   download fails transiently retries quickly, while a genuinely-dead row backs
   off to ~24h instead of churning every drain tick.

Out of scope: NSFW videogen registration (separate spec), a real DB backup
system, the deeper `apply-firewall.sh` container-port audit.

## Background / root cause (verified live)

A synthetic prod videogen on 2026-06-30 exercised the inline path end to end:

- `videogen completion: marked complete` — completion callback **does** land on
  these servers (`video_id=5a087732-1242-4ce4-b809-e6e89132f0d2`).
- Inline register **fired** — a master row appeared
  (`discovered_from='videogen_completion'`, `storage_provider='storj'`,
  `bucket='yral-sfw'`, `object_key='5a087732-…-f0d2.mp4'`).
- Worker **drained** it (`scanned_rows=10`), then the hash **failed**:
  `storj download 5a087732-…-f0d2.mp4: service error`.

Storj linkshare HEADs proved the key is wrong:

| key | HTTP |
|---|---|
| `yral-sfw/5a087732-…-f0d2.mp4` (bare — what was registered) | **404** |
| `yral-sfw/<user_principal>/5a087732-…-f0d2.mp4` (real location) | **200** |
| a known-good backfilled key `yral-sfw/5dudy-…-zae/2426cb6e….mp4` | 200 |

Every object in `yral-sfw` is stored under a `<principal-or-canister>/<uuid>.mp4`
prefix. The completion's `bucket_url`
(`{base}/yral-sfw/{user_principal}/{uuid}.mp4`, per the existing doc-comment in
`src/jobs/ingest.rs`) already contains the correct full path, but
`resolve_source` ignores it and stores the bare `object_key` request field
instead.

Secondary issue surfaced by the same test: the worker re-attempted all 10
missing rows (the new one + 9 permanently-dead rows) in one drain. The prior
quarantine gate (`any_eligible_for_hash`, flat `updated_at > now()-24h`) only
decides **whether** to run a drain; once running, the drain selects **all**
missing rows with no backoff filter, and a single failure locks a row for a full
24h regardless of whether it is fresh or dead.

## Goals

- A completed videogen video is registered with the **real** Storj key and gets
  hashed within one drain interval (~3 min) on the happy path.
- A fresh video whose first download fails transiently is retried within minutes.
- A genuinely-dead row backs off geometrically to a ~24h ceiling — no per-tick
  churn.
- Drains only attempt rows that are actually due.

## Non-goals

- NSFW videogen (`yral-nsfw-videos`) inline registration.
- Changing the videogen upload/finalize protocol or `upload.yral.com`.
- Backfill / discovery changes (discovery remains suppressed/deferred).

## Design

### 1. Core fix — derive the key from `bucket_url`

`src/jobs/ingest.rs`:

- Change `resolve_source(bucket_url: &str) -> Result<VideoSource, ResolveError>`
  (drop the redundant `object_key` parameter — it is the bare key that caused the
  bug).
- Implementation: locate the **first** `/yral-sfw/` in `bucket_url`; the
  remainder (with any `?query`/`#fragment` stripped) is the bucket-relative
  `object_key`, e.g. `{user_principal}/{uuid}.mp4`.
- If the marker is absent, or the remainder is empty, return
  `ResolveError::UnknownSource` → `on_video_ingested` logs a warn and skips
  (the daily discovery sweep is the backstop; best-effort contract unchanged).
- `storage_provider="storj"`, `bucket="yral-sfw"` unchanged.

Callers:

- `on_video_ingested(db_url, video_id, object_key, bucket_url)` keeps its
  signature for now but stops forwarding `object_key` to `resolve_source`. The
  `object_key` param becomes unused; remove it from `on_video_ingested` and from
  the `CompletionDeps::register_ingested` trait method + call site in
  `src/routes/videogen/complete.rs` to avoid a dead parameter. (Net: the
  completion passes only `video_id` + `bucket_url` into the ingest hook.)

Rationale: `bucket_url` is the authoritative download location; parsing it cannot
disagree with where the object actually is. Prefixing with `user_principal`
(approach B) would require threading the principal through and assumes the prefix
is always the principal — `bucket_url` parsing handles any prefix scheme.

### 2. Retry policy — exponential backoff

`src/jobs/media_phash.rs` failure upsert — populate the existing columns:

```sql
INSERT INTO media_job_failures
  (job_run_id, job_kind, item_key, video_id, phase, source_ref,
   last_error, status, retry_count, next_retry_at)
VALUES ($1::TEXT::UUID, $2, $3, $4, $5, $6, $7, 'pending_retry', 1,
        now() + interval '5 minutes')          -- attempt 1: +5m (2^0)
ON CONFLICT (job_kind, item_key, phase) DO UPDATE SET
  job_run_id   = EXCLUDED.job_run_id,
  video_id     = EXCLUDED.video_id,
  source_ref   = EXCLUDED.source_ref,
  last_error   = EXCLUDED.last_error,
  status       = EXCLUDED.status,
  retry_count  = media_job_failures.retry_count + 1,
  next_retry_at = now()
      + LEAST(interval '5 minutes' * power(2, media_job_failures.retry_count),
              interval '24 hours');
```

Backoff schedule (base 5 min, cap 24 h): attempt 1 → +5m, 2 → +10m, 3 → +20m,
4 → +40m, 5 → +80m, … capped at +24h (reached ~attempt 9–10). `power()` returns
`double precision`; multiplying an `interval` by it is valid Postgres.

**Eligibility + drain selection** — a missing row is *due* iff no failure row for
it has `next_retry_at > now()`. Apply this single predicate in both places that
currently diverge:

- `videos_missing_canonical_phash` (drain selection): add
  `AND NOT EXISTS (SELECT 1 FROM media_job_failures f
                    WHERE f.video_id = v.video_id AND f.next_retry_at > now())`.
  → a drain now attempts only due rows (fixes `scanned_rows=10`).
- `any_eligible_for_hash` (the gate): becomes the same predicate — drop the
  `updated_at`-window clause and the `$4` window parameter. (The gate is now
  "does at least one due, missing row exist", i.e. the `EXISTS` of the selection.)

Keep the two as separate functions for now (the gate is a cheap `EXISTS`, the
selection is a `LIMIT`ed fetch) but they MUST share the identical backoff
predicate. Consider a shared SQL fragment/const to prevent drift.

**NULL `next_retry_at` semantics.** The predicate `NOT EXISTS (f WHERE
next_retry_at > now())` treats a NULL `next_retry_at` as *due* (NULL `>` now() is
NULL → not matched → eligible). This is correct for never-failed rows (no failure
row at all) but means **failure rows that predate this change** (the 9 dead +
`5a08…`, all written by the old upsert with `next_retry_at=NULL`) are immediately
eligible again. They will be re-attempted once, fail, and *then* enter backoff
(`+5m, +10m, …`) — a brief ramp, not the old per-tick churn, but a burst on
deploy. See Rollout step 5 for the one-off mitigation.

**Caller impact (other than the worker).** `videos_missing_canonical_phash` and
`any_eligible_for_hash` are also used by the manual/backfill `media-phash`
command. Adding the backoff filter means manual runs also skip backed-off rows;
a forced re-attempt now requires clearing the relevant `media_job_failures` rows
first. Acceptable (don't hammer dead rows), but call it out so the plan updates
all callers — notably `any_eligible_for_hash` loses its `$4` window parameter, so
its worker call site (`run_one_pass` in `src/jobs/worker.rs`) must drop that
argument.

### 3. Clear failure on success

`src/jobs/media_phash.rs`, on a successful hash upsert for a video: delete its
`media_job_failures` row(s) for `(job_kind='media_phash', video_id, phase)` so
`retry_count` resets to a clean slate if the video ever fails again later. Done
in the same transaction as the hash upsert. Low-risk, idempotent.

## Data flow (happy path, post-fix)

```
videogen complete webhook (video_id, object_key=bare, bucket_url=full)
  → handle_success_completion
  → register_ingested(video_id, bucket_url)
  → on_video_ingested → resolve_source(bucket_url)
        → object_key = "<principal>/<uuid>.mp4"   (from bucket_url path)
  → master upsert (storj / yral-sfw / <principal>/<uuid>.mp4)
  → worker drain (within ~3 min): row is due (no failure) → storj GET 200
  → hash upserted, failure row (if any) deleted
```

## Error handling

- `resolve_source` failure → warn + skip (best-effort; sweep backstop).
- Storj GET failure → failure upsert with incremented `retry_count` + backoff
  `next_retry_at`; row re-attempted when due.
- `next_retry_at` reaches the 24h cap for persistently-dead rows → at most one
  attempt/day.

## Testing (TDD)

`src/jobs/ingest.rs`:
- `resolve_source` derives `<principal>/<uuid>.mp4` from a realistic
  `bucket_url`; the bare `object_key` is irrelevant.
- empty tail / missing `/yral-sfw/` / query-string present → correct error / key.
- update `registers_video_into_missing_set` to assert the derived key
  (`principal/vid-1.mp4`, not `k/1.mp4`).

`src/media_index/repo.rs`:
- failure upsert sets `retry_count=1`, `next_retry_at≈now()+5m` on first failure;
  increments + doubles on conflict; caps at 24h.
- a row with a future `next_retry_at` is excluded from
  `videos_missing_canonical_phash` and from `any_eligible_for_hash`; once
  `next_retry_at<=now()` it is included again.
- (replaces the existing `any_eligible_for_hash_quarantines_recent_failures`
  test, which is `updated_at`-based.)

`src/jobs/media_phash.rs`:
- successful hash deletes the video's failure row.

DB tests run serialized (`--test-threads=1`) per the known PgContainer
parallelism flake.

## Rollout (preview first)

1. Branch + TDD implement; independent code review.
2. **Preview**: deploy to the preview app, run a real videogen, watch logs +
   `media-audit` / `media-runs` confirm register → hash **succeeds**; confirm a
   forced failure backs off (not 24h-locked).
3. Merge → deploy to the 3 prod servers.
4. **Re-seed `last_discovery_at = now()`** immediately before deploy (discovery
   stays suppressed).
5. **Fix the existing `5a08…` row + tame the deploy burst** (the core fix only
   corrects *future* completions; the existing master row still holds the bare
   key):
   - One-off UPDATE the `5a08…` master row's `object_key` to the prefixed key
     `<user_principal>/5a087732-…-f0d2.mp4` (the known-good 200 path), then DELETE
     its `media_job_failures` row so the next drain re-hashes it correctly.
   - For the 9 permanently-dead rows (all `next_retry_at=NULL` pre-deploy): either
     accept the short backoff ramp, or pre-set
     `next_retry_at = now() + interval '24 hours'` to suppress the deploy burst.
6. Prod videogen smoke test → confirm register → hash green end to end.

## Risks

- **`bucket_url` shape assumption.** If a completion sends `bucket_url` without
  the `/yral-sfw/<path>` tail, the key can't be derived → skip (sweep backstop).
  Acceptable; logged.
- **Backoff predicate drift.** The gate and the selection must use the identical
  `next_retry_at` predicate or a row could be selected-but-not-gated (or vice
  versa). Mitigate with a shared fragment + tests on both.
- **Existing prefix-less rows.** Any videogen rows already registered with bare
  keys before this ships stay broken until re-registered/corrected (step 5).
  Only `5a08…` exists today.
