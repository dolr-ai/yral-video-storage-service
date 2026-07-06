# Steady-State pHash Coverage Design

**Date:** 2026-06-27
**Owner key:** `prakash-bhatt-yral`
**Status:** approved design, pending implementation plan
**Related:** sharded-phash-backfill (one-time bulk, done), media-master-population, upload-storage-merge

## Problem

The one-time pHash backfill drained the current master set (`all_servable_videos_on_yral`, ~583,607 rows → 99.998% covered). But coverage decays after that:

1. **New uploads** (videogen completions today; external upload routes once `prakash/upload-storage-merge` lands) add videos with no canonical pHash.
2. **Extra bucket videos** already in the S3/Hetzner buckets but never imported into `video_index` (master == `video_index` count == 583,607, so anything beyond that is undiscovered).

There is **no scheduler today** — `media_phash` is HTTP-triggered only; the app deliberately starts no boot-time background tasks. So nothing keeps coverage current.

The existing pipeline is three incremental, skip-existing stages, all already built:

```
scan-hetzner / scan-storj   bucket objects → video_index   (discovers new/extra)
media-import                video_index → all_servable_videos_on_yral (master)
media-phash                 master → canonical pHash
```

## Goal

Keep pHash coverage at ~100% with no manual intervention, reusing the existing jobs and adding only trigger surfaces. Two facts drive the design:

- The **`videos_missing_canonical_phash` anti-join already IS a durable work queue.** Anything registered into master and not yet hashed is, by definition, pending work — survives restarts, no separate queue needed.
- **Videogen completion does not own the storage mapping and does not write this service's DB** — it hands off to an external draft service (`complete.rs`), carrying only `object_key` + `bucket_url`. It may also fire while the object is still at a temporary staged key (`is_temp`).

Therefore: **register new videos into master promptly (cheap, synchronous), and let a single leased background worker drain the missing set on a short interval.** One hash path for new uploads, the existing backlog, and retries.

## Decisions (locked in brainstorming, refined after deep review)

1. **Register-inline + one leased continuous drain** (not hash-at-completion). The earlier "async compute at completion" model was reversed after review: completion lacks the storage mapping and objects may be temp-staged, so hashing there is fragile and duplicates the drain. Registration is the only synchronous request-path work; **all hashing happens in the worker.** Latency for a brand-new video = ≤ the drain interval (~60s), immaterial for coverage.
2. Sweep trigger: **in-app leased background worker** (a `tokio` task in `run_server`) — the app's first boot-time background task, scoped to this.
3. Single-runner election across 3 boxes: **DB lease row** (heartbeat), because pgbouncer runs `POOL_MODE: transaction` — a *session* advisory lock is unsafe (connection not pinned across statements) and a *transaction-scoped* lock cannot be held across a long cycle. The lease (single-statement upserts, each its own txn) is pgbouncer-safe, survives failover (lives in the replicated DB), needs no env pinning.
4. Inline hook: one generic **`on_video_ingested(video_id, object_key, bucket_url)`** function = `resolve_source` + register master row (sync, no hash). Wired into `videogen/complete.rs` now; the upload-merge routes call the same fn later. The discovery scan covers anything not wired.
5. **Steady-state runs single-box** (small delta; the leased box drains locally at `PHASH_CONCURRENCY`). The one-time **extra-bucket backlog** is cleared once via the existing 3-box `phash-fleet.sh` before the worker maintains steady state — the worker is for maintenance, not bulk.
6. Cadence (env-configurable): **drain ~120–300s** (near-real-time enough for coverage; avoids ~1440 idle missing-scans/day a 60s tick would cause), **discovery full-scan ~daily** (the expensive bucket enumeration).
7. **Inline is best-effort for *registration*; the drain is the correctness guarantee for *hashing*.** A registration failure or a video that never hits the hook is recovered by the discovery scan; a temp-staged object simply gets hashed on a later drain once it is final.

## Design

### 1. Source resolution — `resolve_source`

A pure function (new, e.g. `src/jobs/ingest.rs`):

```rust
/// Map a completion's (bucket_url, object_key) to the storage triple the missing-hash
/// scan + worker use to download.
fn resolve_source(bucket_url: &str, object_key: &str) -> Result<VideoSource, ResolveError>;

struct VideoSource { storage_provider: &'static str /* "storj" | "hetzner" */, bucket: String, object_key: String }
```

Verified end-to-end: `all_servable_videos_on_yral` stores `storage_provider` + `bucket` + `object_key`; `videos_missing_canonical_phash` returns those three; `media_phash` downloads by branching on `storage_provider` ("hetzner" → s3, "storj"/None → storj) using `object_key`. So registration must record the **correct provider + key**, and `resolve_source` produces exactly that triple.

**YAGNI — minimal now:** the only wired path (videogen) is **always Storj/`yral-sfw`** (`bucket_url` is built from `STORJ_SFW_SHARE_URL`, generate.rs:1165). So `resolve_source` is implemented minimally for that case — recognize the videogen/Storj-SFW `bucket_url` → `VideoSource { "storj", "yral-sfw", object_key }` — with a clear extension point. The general `bucket_url`-host parser is **deferred** until the upload-merge routes introduce other backends (don't build a parser for URL shapes we haven't seen). Unknown/unrecognized host → `ResolveError` (registration skipped + logged — the discovery scan still catches the object later).

### 2. Inline registration — `on_video_ingested`

```rust
pub async fn on_video_ingested(state: &AppState, video_id: &str, object_key: &str, bucket_url: &str) {
    // resolve_source → upsert the MASTER row directly (all_servable_videos_on_yral +
    // servable_video_sources) with the resolved storage_provider/bucket/object_key.
    // Synchronous, cheap (one upsert). No hashing, no video_index write. Errors are logged,
    // not propagated to the completion response (best-effort; discovery scan is the backstop).
}
```

- Registers **directly into the master** via the existing public `upsert_servable_video` (`ServableVideoInput { storage_provider, bucket, object_key, servable_status: "servable", source_kind: "videogen", discovered_from: "videogen_completion", .. }`). No need to synthesize a legacy row or write `video_index` inline — the master is what the missing-hash scan reads, and it carries the download key + provider.
- Idempotent with a later discovery scan that may re-discover the same `video_id` (the upsert / skip-existing handles it). Registering at completion (earlier than the scan would) is intentional and harmless.
- Wired into `videogen/complete.rs` success branch (it already has `video_id`, `object_key`, `bucket_url` in `handle_success_completion`); the call is **best-effort** and must not fail the completion response.

### 3. Leased drain worker — `src/jobs/worker.rs`

A single `tokio` task spawned once in `run_server` (gated by env `RUN_SWEEP_WORKER`, **ships default off** — flip on after validating the first prod discovery; the lease — not the env — is the single-runner mechanism, so once enabled on all 3 boxes only one runs). Main loop:

```
loop {
    select! biased {
        _ = cancel.cancelled() => { release_lease_best_effort(me); break; }   // graceful shutdown
        _ = run_one_pass() => {}
    }
    sleep_or_cancel(DRAIN_INTERVAL, &cancel)
}

async fn run_one_pass() {
    // EVERY pass is wrapped so a panic/error logs + sentry + returns — the loop NEVER exits.
    let result = catch_and_log(async {
        if !acquire_or_renew_lease(me, ttl)? { return Ok(()); }   // §4 — not owner → skip
        // a) drain — wrapped in heartbeat-renew because a post-discovery drain can be LONG
        with_heartbeat_renew(me, ttl, async {
            cas_guarded(job_media_phash_running, || media_phash::run(.., shard=None, requested_by="sweep_drain"))
        }).await?;
        // b) discovery — only if due by PERSISTED last_discovery_at (survives restart / lease move)
        if discovery_due_from_db()? {
            with_heartbeat_renew(me, ttl, async {
                cas_guarded(job_media_import_running, || {
                    scan_hetzner(full_scan=true); scan_storj(full_scan=true);   // §5
                    media_imports::import_current_video_index(..)
                })
            }).await?;
            set_last_discovery_at_db(now())?;     // persist cadence
        }
        Ok(())
    }).await;
    if result.is_err() { sentry::capture(..); }   // never propagate out of the loop
}
```

- **Resilience (#3):** every pass is wrapped in `catch_and_log`; any error (DB blip, scan failure) or panic is logged + reported to sentry, and the loop continues to the next `sleep`. The task **never exits** — a silently-dead worker is the worst failure mode for an unattended system, so it is structurally prevented.
- **Drain renews too (#1):** the drain is wrapped in `with_heartbeat_renew`, not just discovery. "Drain passes are short" holds only in steady state; immediately after a discovery the drain hashes the whole new delta (minutes–hours), during which the lease must keep being renewed or a peer double-runs.
- **Persisted discovery cadence (#2):** `discovery_due` reads `last_discovery_at` from the DB (a column on `sweep_lease` / a `job_state` row), not memory. So a restart does not re-fire a full scan, and the cadence is consistent regardless of which box currently holds the lease.
- **Guard ownership:** `media_phash::run` does *not* take the `job_media_phash_running` atomic itself — the route handler `compare_exchange`s before calling it. So the worker performs the **same CAS** (`cas_guarded`) around the drain (and the import atomic around discovery), and **skips that stage if a manual run holds it**. Prevents a worker drain overlapping a manual run on the same box (idempotent if it did, but the CAS avoids the wasted double-download).
- **Graceful shutdown (#9):** the loop `select!`s on the existing ctrl_c `cancel` token; on shutdown it best-effort releases the lease (so another box picks up immediately post-deploy, without a TTL wait) and exits.
- When the missing set is empty the drain is a cheap index-only anti-join scan.

### 4. Lease election — `sweep_lease` table + concurrent renew

```sql
CREATE TABLE IF NOT EXISTS sweep_lease (
    id                SMALLINT PRIMARY KEY DEFAULT 1 CHECK (id = 1),  -- single row
    owner             TEXT NOT NULL,
    heartbeat         TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_discovery_at TIMESTAMPTZ                                     -- persisted cadence (#2)
);
```

Acquire-or-renew (one statement, one txn — pgbouncer-safe):

```sql
INSERT INTO sweep_lease (id, owner, heartbeat) VALUES (1, $me, now())
ON CONFLICT (id) DO UPDATE
   SET owner = $me, heartbeat = now()
   WHERE sweep_lease.owner = $me                          -- renew my own
      OR sweep_lease.heartbeat < now() - $lease_ttl        -- steal a stale one
RETURNING owner;
```

- Returns a row iff this box now owns the lease. On a fresh-but-foreign lease the `WHERE` is false → no update, no row → this box skips.
- **`$me` = hostname (#7):** stable per box (one app container per box), so a restarted process immediately renews *its own* lease (`owner = $me`) instead of waiting a full TTL to steal its own stale row.
- **Concurrent renew during long work:** `with_heartbeat_renew` spawns a sibling task that re-runs the acquire/renew statement every `lease_ttl/3` until the wrapped future completes (then aborts the renew task). Wraps **both** the drain and the discovery (a post-discovery drain is long). Without it the lease expires mid-work and a peer double-runs.
- **`last_discovery_at` (#2):** read to decide `discovery_due` (`now() - last_discovery_at >= DISCOVERY_INTERVAL`), written after each successful discovery — a single-row update, same table. Persisted so cadence survives restarts and lease moves.
- `$lease_ttl` (env `SWEEP_LEASE_TTL`, default ~2 min) must exceed the renew interval; the drain/discovery are kept under the TTL by the concurrent renew, not by being short. Schema added to `schema.rs` `SCHEMA_SQL` (idempotent, applied at startup).

### 5. Discovery scan — full-scan + anti-join

Bucket keys are `<canister-id>/<uuid>.mp4` — random UUIDs, **non-monotonic** — so the existing incremental scan (`start_after = max_key`) is **lossy**: a new object whose key sorts before the current max is skipped. The discovery path therefore uses the existing **`full_scan = true`** code path of `scan_hetzner` / `scan_storj` (`list_objects` from the start) + the skip-existing import. Incremental scan stays available for ad-hoc CLI use; it is **not** relied on for steady-state correctness.

### 6. Hashing stays single-pathed (no extraction)

All hashing flows through `media_phash::run` unchanged — the worker's drain calls it directly. The ingest path registers only; nothing else computes a single-video hash, so there is **no second consumer** for a standalone helper. (An earlier draft proposed extracting `hash_one_video`; dropped during plan review as YAGNI — it would be a refactor with zero new callers.)

### 7. Concurrency & resource isolation

- Hashing is **single-pathed** through the drain at `PHASH_CONCURRENCY` (deployed at 4). No separate inline executor (deleted vs the first draft) → no inline/drain overlap, no extra ffmpeg pressure from the request path.
- Registration adds only a synchronous DB upsert to completion — no new concurrency surface.
- Single-box steady state — the lease guarantees one worker, so the three boxes never scan/hash the same delta.

### 8. Worker observability (#4)

An unattended worker is only safe if its liveness is visible. Surface the lease state through the existing observability slice:
- The drain already writes `media_job_runs` rows (`requested_by="sweep_drain"`); discovery writes its own run rows. `media-runs` therefore shows the worker's activity + timestamps.
- Extend `media-status` (or a small `media-sweep` view) to read the `sweep_lease` row: **`owner`** (which box holds it), **`heartbeat`** (last seen — staleness = worker dead), **`last_discovery_at`**. This is the single signal that answers "is steady-state coverage actually running, and where." Cheap (one row read), and it is what makes "no manual intervention" trustworthy rather than blind.
- Coverage itself stays observable via `media-audit` (`missing_canonical_phash`); a rising `missing` count is the lagging indicator if the worker silently stalls. (A threshold alert on it is a deferred follow-up, see Out of scope.)

## Surfaces

- **First boot-time background task:** the worker departs from today's "app starts nothing on boot" property. Mitigated: demand-independent of request traffic, gated by `RUN_SWEEP_WORKER`, single-runner via the lease (3 instances do not triplicate work). Videogen publisher/moderation stay per-request.
- **Lease vs failover:** all boxes reach the leader DB via `pgbouncer → postgres-router`; the lease row is on the leader and replicates. On failover the lease persists; the owner keeps renewing or a peer takes over after TTL. No split-brain — the jobs are idempotent/skip-existing even if two passes briefly overlap.
- **pgbouncer transaction pooling:** ruled out session advisory locks; the lease and renew are single-statement/per-txn. The long scan/import/phash use their own short-lived connections per existing job code (no held session).
- **Registration correctness depends on `resolve_source`:** a wrong/unknown `bucket_url` → registration skipped (logged), and the discovery full-scan still discovers the object from the bucket. So a `resolve_source` gap degrades to "covered a bit later," never "lost."
- **Temp-staged objects:** an inline-registered video whose object is still at a temp key will fail to download on the first drain (recorded in `media_job_failures`, phase `phash_download`) and succeed on a later drain once final — by design, not a bug.
- **Discovery cost:** a daily full bucket list over both buckets is O(objects); acceptable daily, configurable. Heartbeat renew covers its duration.
- **Backlog vs steady-state — sequencing (#6):** the worker drains **single-box, no shard**. The first discovery full-scan may surface a large extra-bucket backlog; draining that single-box is slow, and running `phash-fleet.sh` (3-box sharded) *concurrently* with the worker would double-hash the same rows (idempotent but wasteful). **Operational order:** after the first discovery import, if `missing` is large, clear it once via `phash-fleet.sh --of 3` with the worker's drain effectively idle (it shares the per-box CAS guard on server-1, and the other two shards have no worker), then let the worker maintain the small ongoing delta. (Today's live `missing` ≈ 9, so this only matters at the first post-deploy discovery.) Optionally the worker backs off its drain when `missing` exceeds a threshold, deferring bulk to the fleet — a simple guard, listed as a plan option.
- **DB connection pressure:** the worker adds a periodic connection on one box; negligible vs the per-request no-pool load already tracked.
- **Security:** no new external surface — the worker is internal; registration is reached only through the authenticated completion. No new secrets (env knobs only).

## Out of scope (deferred)

- A coordinator endpoint to fan the drain across all 3 boxes (steady-state delta is small → single-box suffices; the sharded fleet remains the bulk tool).
- Wiring registration into the upload-merge external routes (done when that branch lands; they call the same `on_video_ingested`).
- A *full* dead-letter quarantine. (A lightweight version IS in scope: the drain pre-check `any_eligible_for_hash` skips rows that failed within the last `DISCOVERY_INTERVAL`, so dead rows don't trigger re-downloads on idle ticks. A formal dead-letter table/status is still deferred.)
- Mark-orphaned-`running`-rows-`interrupted` on startup (observability cleanup, tracked separately).
- S3 bucket event-notification → queue ingestion (heavier infra; the register-inline + drain achieves near-real-time without it).
- A **threshold alert** on rising `missing_canonical_phash` (lagging-indicator alerting if the worker silently stalls) — the `media-status` lease view (§8) is the primary liveness signal; a paging alert is a follow-up.

## Testing

- **`resolve_source`**: representative SFW (Storj `yral-sfw`) and NSFW/Hetzner `bucket_url`s map to the correct `VideoSource` column; unknown host → `ResolveError`.
- **`on_video_ingested`**: writes the correct key column synchronously; idempotent on repeat; a `resolve_source` error logs and does not propagate (completion still succeeds); the registered row appears in `videos_missing_canonical_phash`.
- **Lease**: two concurrent acquirers → exactly one row returned; a stale heartbeat lets a different `$me` steal it; an active owner renewing blocks a second acquirer; a fresh foreign lease yields no row (skip); same `$me` (hostname) after "restart" renews its own lease without waiting for TTL (#7).
- **Concurrent renew**: `with_heartbeat_renew` keeps the heartbeat fresh across a simulated long task; a peer cannot steal mid-task; the renew task stops after completion. Wraps the **drain** as well as discovery (#1).
- **Persisted cadence (#2)**: `discovery_due` reads `last_discovery_at` from the DB; a simulated restart does not re-fire discovery; setting `last_discovery_at` defers the next discovery.
- **Loop resilience (#3)**: an error/panic in one pass is caught + logged and the loop continues to the next tick (task does not exit).
- **Graceful shutdown (#9)**: the cancel token breaks the loop and releases the lease.
- **Discovery full-scan**: a bucket key sorting *before* `max_key` is discovered by the full-scan + anti-join (the case incremental-by-max-key misses).
- **Worker loop**: drains missing each pass; runs discovery only when due; no-ops cleanly when nothing missing/due; skips a stage if the per-box CAS guard is held by a manual run.
- **`hash_one_video`**: shared by drain; download(Storj|Hetzner)+ffmpeg+`persist_one`; failure returns the typed phase.
- **Lease view (#4)**: `media-status`/`media-sweep` returns the `sweep_lease` row (`owner`, `heartbeat`, `last_discovery_at`).
- **Schema**: `sweep_lease` created idempotently by `SCHEMA_SQL`.

## Files to touch

- `src/jobs/ingest.rs` — new: `resolve_source`, `VideoSource`, `on_video_ingested`.
- `src/jobs/worker.rs` — new: the resilient leased loop + cadence + `with_heartbeat_renew` + `cas_guarded` + graceful-shutdown select. Drain gated on `missing > 0` (avoids polluting `media_job_runs`). `$me` = `NODE_NAME`.
- `src/media_index/repo.rs` — lease acquire/renew + `last_discovery_at` read/write helpers; lease-row read for the status view. (Registration reuses the existing public `upsert_servable_video` — no new upsert needed.)
- `src/media_index/schema.rs` — `sweep_lease` (incl. `last_discovery_at`) in `SCHEMA_SQL`.
- `src/routes/videogen/complete.rs` — call `on_video_ingested` (best-effort) in the success branch.
- `src/routes/media.rs` — extend `media-status` (or add `media-sweep`) with the lease view (#4); `crates/mirror-client` surfaces it.
- `src/main.rs` / `run_server` — spawn the worker (gated by `RUN_SWEEP_WORKER`), passing the existing `cancel` token.
- `src/consts.rs` — `RUN_SWEEP_WORKER`, `DRAIN_INTERVAL` (default ~180s), `DISCOVERY_INTERVAL` (~24h), `SWEEP_LEASE_TTL` (~2m).
- `.github/workflows/deploy-prakash-servers.yml` — env defaults (worker on; intervals).

No change to the import job's core logic or the feed. No new external endpoint. No separate inline hash executor.
