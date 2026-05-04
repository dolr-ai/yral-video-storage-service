# Storj Mirror Index Design

**Date:** 2026-05-04
**Repo:** yral-video-storage-service
**Goal:** Make `yral-sfw` Storj bucket a 1:1 mirror of the Hetzner SFW bucket.
**Scale:** ~600K videos (MP4 + dash-thumbnail pairs)

---

## Problem

- All videos currently live in Hetzner S3 only
- Existing `yral-videos` Storj bucket has corruption — replaced by fresh `yral-sfw`
- No global index of what exists where
- No phash-based deduplication audit
- Temp/pending S3 objects accumulate and consume storage quota

## Scope

- **SFW only** — one Hetzner bucket → one Storj bucket (`yral-sfw`)
- **MP4 + `-thumbnail.png` (dash variant)** — `_thumbnail.png` (underscore) excluded
- **Folder structure preserved** — S3 keys mirrored verbatim
- **No HLS** — MP4 only

---

## Architecture

**Approach:** HTTP-triggered background jobs in main service (Approach B).

Jobs live in `src/jobs/`, triggered via `POST /mirror/jobs/*` with existing `SERVICE_SECRET_TOKEN` auth. Postgres tracks state. phash runs in `spawn_blocking`. Sentry + tracing already wired.

### Migration sequence

```
1. rclone (manual, one-off) — bulk copy Hetzner → yral-sfw
2. Job 0: Scan Storj      — reconcile what rclone copied
3. Job 1: Scan Hetzner    — populate video_index
4. Job 2: Phash backfill  — compute phash for all videos
5. Job 3: Mirror          — copy any videos missing from Storj (post-rclone gaps)
6. Job 4: Cleanup         — delete confirmed-mirrored temp/pending Hetzner objects
```

Jobs 0 and 1 write different columns — can run in either order or concurrently.

---

## Build Changes

**Current:** `x86_64-unknown-linux-musl` + Alpine base image
**New:** `x86_64-unknown-linux-gnu` + Debian Slim base image

Reason: `ffmpeg-next` crate (needed for phash) links against libavcodec/libavformat C libs, incompatible with musl cross-compile. `rustls-tls` already used so no OpenSSL concern.

**CI (`build-binary.yml`):**
- Remove `musl-tools` apt package
- Change target to `x86_64-unknown-linux-gnu`
- Install `ffmpeg` dev headers: `libavformat-dev libavcodec-dev libavutil-dev libswscale-dev clang`

**Dockerfile (multi-stage):**
```
Stage 1 (builder): rust:bookworm + ffmpeg-dev headers → compile binary
Stage 2 (runtime): debian:bookworm-slim + runtime ffmpeg + uplink CLI
```

---

## New Infrastructure

### Postgres (docker-compose)

```yaml
postgres:
  image: postgres:16-alpine
  environment:
    POSTGRES_DB: mirror_index
    POSTGRES_USER: storj
    POSTGRES_PASSWORD: ${POSTGRES_PASSWORD}
  volumes:
    - postgres_data:/var/lib/postgresql/data
  networks:
    - storj-net
  restart: unless-stopped
```

Pattern: `tokio_postgres` + `batch_execute` for schema init (same as counter repo). One connection per job run, reconnect per batch loop iteration. No pool needed — DB writes are fast, bottleneck is network/CPU.

### Storj S3 Client (`src/storj_s3_client.rs`)

New `StorjS3Client` using Storj's S3-compatible gateway (`gateway.storjshare.io`). Same `aws-sdk-s3` code as existing `S3Client`, different endpoint + credentials. Used by Job 0 (scan) and Job 4 (verify before delete).

Credentials: S3-compatible access key + secret derived from Storj access grant (one-time `uplink share --register` command).

### phash crate (`crates/phash/`)

Ported verbatim from `off-chain-agent/src/duplicate_video/phash.rs`. Dependencies: `ffmpeg-next`, `image_hasher`, `image`. No API changes.

---

## Database Schema

```sql
CREATE TABLE IF NOT EXISTS video_index (
    video_id      TEXT PRIMARY KEY,
    -- stem of mp4 filename: "user123/abc.mp4" → video_id = "abc"
    -- actually full relative path without extension for uniqueness
    hetzner_key   TEXT,          -- "user123/abc.mp4"  NULL = not in Hetzner
    storj_key     TEXT,          -- "user123/abc.mp4"  NULL = not in Storj
    phash         TEXT,          -- 640-char binary string, NULL = not computed
    is_temp       BOOLEAN NOT NULL DEFAULT FALSE,
    status        TEXT NOT NULL DEFAULT 'pending',
    -- pending | phash_computed | mirrored | done
    error_message TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_phash_null    ON video_index (video_id)
    WHERE phash IS NULL;
CREATE INDEX IF NOT EXISTS idx_missing_storj ON video_index (video_id)
    WHERE storj_key IS NULL AND is_temp = FALSE;
CREATE INDEX IF NOT EXISTS idx_temp_cleanup  ON video_index (video_id)
    WHERE is_temp = TRUE AND status = 'mirrored';
CREATE INDEX IF NOT EXISTS idx_status        ON video_index (status);
```

**video_id:** Full relative path without extension (e.g. `"user123/abc123"`). This ensures uniqueness across folders. `hetzner_key` = `video_id + ".mp4"`. Thumbnail key = `video_id + "-thumbnail.png"`.

**Status state machine:**
```
pending → phash_computed → mirrored → done
                                    ↑ (temp files only: cleanup removes hetzner_key)
```
Errors set `error_message` only; `status` stays at current value for retry on next run.

---

## Job Specifications

### Backpressure

All jobs use a `tokio::sync::Semaphore` to bound concurrent in-flight operations. Never holds more than `CONCURRENCY` videos in memory simultaneously. Downloads stream to tempfile (not in-memory).

```
PHASH_CONCURRENCY=4      # concurrent phash ops (download + decode)
MIRROR_CONCURRENCY=8     # concurrent mirror ops (download + uplink upload)
SCAN_PAGE_SIZE=1000      # S3 list_objects_v2 page size
```

Progress logged every 1000 videos via `tracing::info!`. Per-video errors: `tracing::error!` + `sentry::capture_error`.

---

### Job 0 — Scan Storj

`POST /mirror/jobs/scan-storj`

Reconciles the Postgres index with what rclone already copied. Must run after rclone and before Job 3 to avoid re-uploading 600K already-mirrored files.

```
for each page of objects in yral-sfw (StorjS3Client.list_objects_v2):
    for each key ending in ".mp4":
        video_id = key without extension
        UPSERT video_index (video_id, storj_key=key)
        ON CONFLICT DO UPDATE SET storj_key = EXCLUDED.storj_key
        WHERE video_index.storj_key IS NULL
```

Idempotent: re-running is safe. Does not touch phash or status.

---

### Job 1 — Scan Hetzner

`POST /mirror/jobs/scan-hetzner`

```
for each page of objects in Hetzner bucket (S3Client.list_objects):
    skip keys ending in "_thumbnail.png" or "-thumbnail.png"
    for each key ending in ".mp4":
        video_id = key without extension
        is_temp = key contains TEMP_KEY_PREFIX (default "_pending/")
        UPSERT video_index (video_id, hetzner_key=key, is_temp)
        ON CONFLICT DO UPDATE SET hetzner_key = EXCLUDED.hetzner_key,
            is_temp = EXCLUDED.is_temp
        WHERE video_index.hetzner_key IS NULL
```

Idempotent. Does not touch phash or status.

---

### Job 2 — Phash Backfill

`POST /mirror/jobs/phash`

```
loop:
    rows = SELECT video_id, hetzner_key FROM video_index
           WHERE phash IS NULL
           LIMIT SCAN_PAGE_SIZE

    if empty: break

    semaphore = Semaphore(PHASH_CONCURRENCY)
    results = join_all(rows.map(|row| async {
        _permit = semaphore.acquire()

        // download to tempfile (streaming, not in-memory)
        tmp = stream hetzner_key to NamedTempFile

        // CPU-intensive: run in blocking thread pool
        phash_result = spawn_blocking(|| PHasher::new().compute_hash(&tmp.path()))

        // always cleanup, success or error
        drop(tmp)  // NamedTempFile auto-deletes on drop

        (row.video_id, phash_result)
    }))

    for (video_id, result) in results:
        if Ok(phash):
            UPDATE video_index SET phash=$phash, status='phash_computed',
                error_message=NULL, updated_at=NOW()
            WHERE video_id=$video_id
        if Err(e):
            tracing::error!(video_id, error=%e, "phash failed")
            sentry::capture_error(&e)
            UPDATE video_index SET error_message=$e, updated_at=NOW()
            WHERE video_id=$video_id
            // status stays 'pending' → retried next run
```

Idempotent: WHERE clause filters already-computed rows.
Tempfile cleanup: `NamedTempFile` from `tempfile` crate auto-deletes on drop (success or panic).

---

### Job 3 — Mirror Incremental

`POST /mirror/jobs/mirror`

Handles new videos uploaded after rclone ran. For bulk initial migration, rclone + Job 0 handle the 600K; this job is for the ongoing delta.

```
loop:
    rows = SELECT video_id, hetzner_key FROM video_index
           WHERE storj_key IS NULL
             AND hetzner_key IS NOT NULL
             AND is_temp = FALSE
             AND status = 'phash_computed'
           LIMIT SCAN_PAGE_SIZE

    if empty: break

    semaphore = Semaphore(MIRROR_CONCURRENCY)
    for each row (bounded by semaphore):
        // 1. Copy MP4
        tmp_mp4 = download hetzner_key to NamedTempFile
        uplink cp --access=$ACCESS_GRANT tmp_mp4 sj://yral-sfw/{hetzner_key}
        drop(tmp_mp4)

        // 2. Copy thumbnail if present (best-effort, non-fatal)
        thumb_hetzner = hetzner_key.replace(".mp4", "-thumbnail.png")
        if S3Client.object_exists(thumb_hetzner):
            tmp_thumb = download thumb_hetzner to NamedTempFile
            uplink cp --access=$ACCESS_GRANT tmp_thumb sj://yral-sfw/{thumb_hetzner}
            drop(tmp_thumb)

        // 3. Commit to index
        UPDATE video_index SET storj_key=hetzner_key, status='mirrored',
            updated_at=NOW()
        WHERE video_id=$video_id

        // on any error: log + sentry, storj_key stays NULL, retried next run
```

All-or-none per video: DB update only after successful upload. If process crashes after upload but before DB update, next run re-uploads (uplink cp to same key = idempotent overwrite).

---

### Job 4 — Temp Cleanup

`POST /mirror/jobs/cleanup`

Deletes confirmed-mirrored temp/pending Hetzner objects.

```
rows = SELECT video_id, hetzner_key, storj_key FROM video_index
       WHERE is_temp = TRUE AND status = 'mirrored'
         AND hetzner_key IS NOT NULL AND storj_key IS NOT NULL

for each row:
    // Safety: verify Storj copy exists before deleting Hetzner
    assert StorjS3Client.object_exists(storj_key)

    S3Client.delete_video(hetzner_key)

    UPDATE video_index SET hetzner_key=NULL, is_temp=FALSE,
        status='done', updated_at=NOW()
    WHERE video_id=$video_id

    // on error: log + sentry, hetzner_key stays, retried next run
```

---

### Audit Endpoint

`GET /mirror/audit`

Single aggregate SELECT — no full scan.

```sql
SELECT
    COUNT(*)                                         AS total,
    COUNT(phash)                                     AS phash_computed,
    COUNT(*) FILTER (WHERE status = 'mirrored'
                        OR status = 'done')          AS mirrored,
    COUNT(*) FILTER (WHERE storj_key IS NULL
                       AND is_temp = FALSE)          AS missing_storj,
    COUNT(*) FILTER (WHERE hetzner_key IS NULL
                       AND is_temp = FALSE)          AS missing_hetzner,
    COUNT(*) FILTER (WHERE is_temp = TRUE
                       AND status = 'mirrored')      AS cleanup_pending,
    COUNT(*) FILTER (WHERE error_message IS NOT NULL) AS error_count
FROM video_index;
```

Plus a second query for duplicate phashes:
```sql
SELECT phash, array_agg(video_id) AS video_ids
FROM video_index WHERE phash IS NOT NULL
GROUP BY phash HAVING COUNT(*) > 1
LIMIT 100;
```

Response: `application/json`.

---

## File Layout

```
yral-video-storage-service/
  crates/
    phash/
      Cargo.toml
      src/lib.rs          -- ported from off-chain-agent/src/duplicate_video/phash.rs
  src/
    db.rs                 -- postgres connect, schema init, all query functions
    storj_s3_client.rs    -- aws-sdk-s3 client pointed at gateway.storjshare.io
    jobs/
      mod.rs              -- log_every_n helper, semaphore constants
      scan_storj.rs       -- Job 0
      scan_hetzner.rs     -- Job 1
      phash_backfill.rs   -- Job 2
      mirror.rs           -- Job 3
      cleanup.rs          -- Job 4
    routes/
      mirror.rs           -- POST /mirror/jobs/* + GET /mirror/audit
      mod.rs              -- add mirror router
  deploy/
    docker-compose.yml    -- add postgres service + postgres_data volume
    Dockerfile            -- multi-stage: builder + runtime
  .github/workflows/
    build-binary.yml      -- gnu target + ffmpeg-dev install
```

---

## New Environment Variables

| Variable | Default | Purpose |
|---|---|---|
| `DATABASE_URL` | required | `postgres://storj:$PW@postgres:5432/mirror_index` |
| `POSTGRES_PASSWORD` | required | Postgres password (GitHub secret) |
| `STORJ_GATEWAY_ACCESS_KEY` | required | S3-compat key for `gateway.storjshare.io` |
| `STORJ_GATEWAY_SECRET_KEY` | required | S3-compat secret for `gateway.storjshare.io` |
| `STORJ_SFW_BUCKET` | `yral-sfw` | Target Storj bucket name |
| `PHASH_CONCURRENCY` | `4` | Max parallel phash operations |
| `MIRROR_CONCURRENCY` | `8` | Max parallel mirror operations |
| `SCAN_PAGE_SIZE` | `1000` | S3 list_objects page size |
| `TEMP_KEY_PREFIX` | `_pending/` | Hetzner key prefix for temp objects |

---

## Operational Steps (one-time migration)

```bash
# 1. Deploy with new Postgres + env vars
docker compose up -d

# 2. Bulk copy (run on server with rclone configured)
rclone copy hetzner:yral-videos storj:yral-sfw \
    --exclude "*_thumbnail.png" \
    --transfers=32 --checkers=64 \
    --progress

# 3. Reconcile index (order: 0 then 1, or parallel)
curl -X POST https://storj-interface.yral.com/mirror/jobs/scan-storj \
    -H "Authorization: Bearer $SERVICE_SECRET_TOKEN"
curl -X POST https://storj-interface.yral.com/mirror/jobs/scan-hetzner \
    -H "Authorization: Bearer $SERVICE_SECRET_TOKEN"

# 4. Phash backfill (long-running, re-trigger if needed)
curl -X POST https://storj-interface.yral.com/mirror/jobs/phash \
    -H "Authorization: Bearer $SERVICE_SECRET_TOKEN"

# 5. Mirror any gaps
curl -X POST https://storj-interface.yral.com/mirror/jobs/mirror \
    -H "Authorization: Bearer $SERVICE_SECRET_TOKEN"

# 6. Audit
curl https://storj-interface.yral.com/mirror/audit \
    -H "Authorization: Bearer $SERVICE_SECRET_TOKEN"

# 7. Cleanup temp files when ready
curl -X POST https://storj-interface.yral.com/mirror/jobs/cleanup \
    -H "Authorization: Bearer $SERVICE_SECRET_TOKEN"
```

---

## Production Requirements Checklist

- [x] Idempotent jobs — upsert + WHERE guards, safe to re-run
- [x] All-or-none per video — DB update only after successful upload
- [x] Backpressure — semaphore bounds concurrent in-flight ops
- [x] Tempfile cleanup — `NamedTempFile` auto-deletes on drop
- [x] Observability — `tracing::info!` every 1000, `tracing::error!` per failure
- [x] Sentry — `sentry::capture_error` on every per-video failure
- [x] Auth — `SERVICE_SECRET_TOKEN` middleware on all job endpoints
- [x] Graceful handling — errors update `error_message`, job continues with next video
- [x] Resumable — jobs pick up from WHERE clause on next trigger
