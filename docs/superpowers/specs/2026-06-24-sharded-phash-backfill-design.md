# Sharded Distributed pHash Backfill Design

**Date:** 2026-06-24
**Owner key:** `prakash-bhatt-yral`
**Branch:** `prakash/media-jobs-observability` (same branch/PR as the observability slice)
**Status:** approved design, pending implementation plan

## Problem

The canonical pHash backfill (~583k videos, download + ffmpeg each) runs **in-process in the single `storj-interface` app instance**, which the deploy starts on **server_1 only** (`RUN_APP=true` gated to server_1 in `deploy-prakash-servers.yml`; storage creds exported only when `RUN_APP`). prakash-2/3 run only the Patroni DB cluster — their CPU sits idle. At a CPU-safe concurrency the backfill is a multi-week grind on one box, while two boxes are wasted.

## Goal

Use all three boxes: run the pHash job on each, partitioned so they do disjoint work (no duplicated downloads), at a concurrency that uses spare CPU without starving live traffic (server_1 observed ~27% at concurrency 2). The operator chooses which servers participate per run.

## Decisions (locked in brainstorming)

1. **Run the full app on all 3 boxes** (not a separate job-runner). Verified safe: the app starts **no background consumers or schedulers** on boot (no RabbitMQ consumer anywhere; nothing spawned in `run_server` before the router) — it is purely demand-driven, so 3 instances are idle until triggered. Only server_1 keeps the public domain; 2/3 are internal.
2. **Shard by `((hashtext(video_id)::bigint % of) + of) % of = shard`** — even distribution; rejected video_id-range (skew risk). NOTE: do NOT use `abs(hashtext(...))` — `hashtext` returns signed `int4` and `abs(INT_MIN::int4)` raises `ERROR: integer out of range` in Postgres, which would abort the whole scan; casting to `bigint` before `%` and normalizing the sign is total over all hash values and always yields `[0, of)`.
3. **Concurrency 4** fleet-wide (≈50% CPU per box), set as a sticky deploy env default.
4. **SSH orchestrator script** for triggering (not a coordinator endpoint — deferred; see Out of Scope). Operator picks the server set via a map in the script.
5. Ships in the **same PR** as the observability slice (one deploy).

## Design

### 1. Shard predicate — `src/media_index/repo.rs`

Extend the existing missing-hash scan with an optional shard:
```rust
pub async fn videos_missing_canonical_phash(
    client, hash_kind, hash_version, input_media_version,
    after: Option<&str>, limit: Option<i64>,
    shard: Option<(i64 /* of */, i64 /* idx */)>,   // NEW
) -> Result<Vec<MissingHashRow>, Error>
```
When `shard = Some((of, idx))`, add `AND (((hashtext(v.video_id)::bigint % $of) + $of) % $of) = $idx` to the WHERE clause (bind `of`, `idx`). The `::bigint` cast avoids the `abs(INT_MIN)` int4-overflow error; `(x % of + of) % of` normalizes a possibly-negative result into `[0, of)`, giving a **total, deterministic, uniform** partition (every `video_id` lands in exactly one shard). `None` = no shard filter (current behavior). The existing `video_id` cursor + PK anti-join are unchanged: each shard still walks the whole `video_id` range (cheap, index-only) but the shard filter keeps only its 1/of of the missing rows, so the expensive download+ffmpeg work is partitioned with no overlap.

`media_phash::run` / `run_inner` gain a `shard: Option<(i64, i64)>` param, threaded into the scan call.

### 2. Route + client

- Route `POST /media/phash/run?limit=&requested_by=&shard=&of=` — `shard`/`of` both optional; if either is present, both are required. Validate `of >= 1` and `0 <= shard < of` (else 400). Pass `Some((of, shard))` into `media_phash::run`. Absent → `None` (whole set, today's behavior).
- `mirror-client media-phash --shard i --of n` (both or neither). Add `parse_of` helper; thread into the `?shard&of` query.

### 3. Deploy change — `.github/workflows/deploy-prakash-servers.yml`

- Set `RUN_APP=true` for **server_2 and server_3** (today only server_1). This makes `deploy-ha.sh` start the `app` compose profile on those nodes (`COMPOSE_PROFILES=app`).
- Export the full `APP_VARS` (storage + service creds — the same block currently gated to server_1) for all three nodes, since booting the app `.expect()`s those at startup. NOTE: `DATABASE_URL` is **not** in `APP_VARS` — it's built inside `deploy/docker-compose.ha.yml` from `APP_DB_PASSWORD` (which is already exported unconditionally for all nodes), so it's available once the `app` profile starts. Don't add `DATABASE_URL` to `APP_VARS`.
- Add `PHASH_CONCURRENCY=4` to the app env on all three. This is **mandatory**: `consts::PHASH_CONCURRENCY` defaults to **10** when unset, so without the env var the boxes would run pHash at 10 (~too hot). Sticky default; survives redeploy, unlike in-memory `config-set`.
- server_1 keeps the public domain + health check; 2/3 run the app on `localhost:3005` only (no public route). No compose/port change needed beyond `RUN_APP`.
- Update the workflow's `deployment-summary` step (currently prints "**App:** server_1 only") to reflect app-on-all-3.

### 4. Fleet trigger — `scripts/phash-fleet.sh`

A bash script (run from a workstation/CI with the deploy SSH key + `SERVICE_SECRET_TOKEN`) that:
- Takes a configurable server→shard map and a single `--of N` (the map is the operator's "which servers" config).
- For each server: SSH in, compute an HMAC signature inline (no `mirror-client` binary needed on the box), and:
  1. signed `GET localhost:3005/media/jobs/status` → **skip the box if `phash_running` is true** (avoids a wasteful retrigger; the server-side `job_media_phash_running` 409 guard backs this up).
  2. signed `POST localhost:3005/media/phash/run?shard=<idx>&of=<N>`.
- Reuses the deploy's SSH key + access pattern; no new network path (boxes are not made mutually HTTP-reachable). The script enforces a single consistent `--of` across the chosen servers (prevents mis-aligned shard partitions).

### 5. Monitoring (from the observability slice, same PR)

The pHash job already writes per-run rows to the shared `media_job_runs` (one per shard invocation) and failures to `media_job_failures`. A single signed call to **server_1's public domain** aggregates all shards' runs/failures/coverage (shared Patroni DB), so monitoring needs no SSH. `media-audit` shows fleet-wide coverage; `media-runs` shows each shard's live totals; `media-failures` shows grouped reasons.

## Surfaces

- **Safety of 3× app:** confirmed no auto-start consumers/schedulers; routes are demand-driven; 2/3 receive no public traffic. server_1's behavior is unchanged. Videogen publisher/moderation are per-request only — never invoked on 2/3 (no traffic).
- **No duplicate work:** disjoint shards via the modulo predicate; per-box `job_media_phash_running` 409 guard prevents two pHash jobs on one box; the script skips busy boxes and enforces one `--of`.
- **Resumability:** each shard is independently resumable — re-run a shard, its missing-hash scan skips done rows (the skip-existing anti-join already shipped).
- **Idempotency:** if shard configs are mis-aligned across boxes (operator error using inconsistent `--of`), the DB upserts stay correct (some videos done twice — wasteful, not corrupt; some deferred to a clean run). The script enforcing one `--of` prevents this in practice.
- **Security:** `?shard`/`?of` are integers, parameterized in SQL (no injection). The trigger uses the existing HMAC + SSH. Deploy env for 2/3 reuses existing Vault/GitHub secrets — no new secrets.
- **Deploy risk:** the workflow change (app on 3) is the main risk surface; mitigated by the no-auto-consumer finding and that the compose `app` profile + creds are the same proven set already running on server_1.

## Out of scope (deferred)

- A `POST /media/phash/run-fleet` coordinator endpoint (server_1 fans shards to peers over HTTP). Deferred until fleet-triggering becomes recurring/unattended (e.g. the periodic-refresh loop), because it needs opening port 3005 between boxes + peer config + partial-failure handling (~1–2 days + a prod network change) vs ~1 hour for the script. The shard code built here is exactly what it would call.

## Testing

- `videos_missing_canonical_phash` shard filter: seed N missing rows, call with `shard=Some((3, idx))` for idx 0/1/2, assert the three result sets are **disjoint and their union = all N** (every row lands in exactly one shard). Include `video_id`s whose `hashtext` is negative (and ideally exercise the total-partition property) so the `(x % of + of) % of` normalization is actually verified — a naive `abs()`/`%` would mis-bucket or error on these.
- Route: `shard`/`of` validation (400 on `of<1`, `shard>=of`, or only one provided); `None` path unchanged.
- Client: `parse_of` + `--shard/--of` arg parsing.
- `phash-fleet.sh`: not unit-tested (bash/SSH orchestration); document a dry-run note. The HMAC-signing recipe matches the proven `mirror-client` signing.
- Deploy workflow: not unit-testable; the change is reviewed + validated by the deploy's own health check (server_1) + a manual post-deploy check that the app booted on 2/3 (signed `media-status` on each box's localhost).

## Files to touch

- `src/media_index/repo.rs` — `shard` param on `videos_missing_canonical_phash`. Only one production caller exists (`media_phash.rs::run_inner`); also update the **5 test call sites in `media_phash.rs`** (pass `None`).
- `src/jobs/media_phash.rs` — `shard` param on `run`/`run_inner` → scan call.
- `src/routes/media.rs` — `?shard&of` on the run handler + validation.
- `crates/mirror-client/src/{lib,main}.rs` — `--shard/--of` on `media-phash`.
- `.github/workflows/deploy-prakash-servers.yml` — `RUN_APP` + `APP_VARS` + `PHASH_CONCURRENCY=4` for all 3 nodes.
- `scripts/phash-fleet.sh` — new orchestrator.
- `crates/mirror-client/README.md` — document `--shard/--of` + the fleet script.

No schema change. No change to the import job or the feed.
