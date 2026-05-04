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

**Approach:** HTTP-triggered background jobs in main service.

Jobs live in `src/jobs/`, triggered via `POST /mirror/jobs/*`. Endpoints return **202 Accepted immediately** and run the job as a background tokio task — HTTP connections do not block for job completion. Existing `SERVICE_SECRET_TOKEN` auth middleware protects all endpoints. Postgres tracks state. phash runs in `spawn_blocking`. Sentry + tracing already wired in `main.rs`.

### Migration sequence

```
1. rclone (manual, one-off) — bulk copy Hetzner → yral-sfw
2. Job 0: Scan Storj      — reconcile what rclone copied
3. Job 1: Scan Hetzner    — populate video_index
4. Job 2: Phash backfill  — compute phash for all videos
5. Job 3: Mirror          — copy any videos missing from Storj (post-rclone gaps)
6. Job 4: Cleanup         — delete confirmed-mirrored temp/pending Hetzner objects
```

Jobs 0 and 1 write different columns and can run concurrently. Jobs 2–4 must run in order.

For ongoing delta (new uploads after initial rclone): re-trigger Job 1 periodically to discover new Hetzner objects, then Jobs 2 and 3 for phash + mirror.

---

## Build Changes

**Current:** `x86_64-unknown-linux-musl` + Alpine base image
**New:** `x86_64-unknown-linux-gnu` + Debian Slim base image

Reason: `ffmpeg-next` crate links against libavcodec/libavformat C libs, incompatible with musl. `rustls-tls` already used so no OpenSSL concern from dropping musl.

**CI (`build-binary.yml`):**
- Remove `musl-tools` apt package
- Change target to `x86_64-unknown-linux-gnu`
- Install ffmpeg dev headers before build: `libavformat-dev libavcodec-dev libavutil-dev libswscale-dev clang`

**Dockerfile (multi-stage):**
```dockerfile
# Stage 1: builder
FROM rust:bookworm AS builder
RUN apt-get update && apt-get install -y \
    libavformat-dev libavcodec-dev libavutil-dev libswscale-dev clang pkg-config
WORKDIR /app
COPY . .
RUN cargo build --release --target x86_64-unknown-linux-gnu

# Stage 2: runtime
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ffmpeg ca-certificates && rm -rf /var/lib/apt/lists/*
# install uplink CLI — pin version for reproducible builds
RUN curl -L https://github.com/storj/storj/releases/download/v1.112.4/uplink_linux_amd64.zip \
    -o /tmp/uplink.zip && unzip /tmp/uplink.zip -d /tmp && install /tmp/uplink /usr/local/bin/uplink
COPY --from=builder /app/target/x86_64-unknown-linux-gnu/release/storj-interface /app/storj-interface
EXPOSE 3000
ENTRYPOINT ["/app/storj-interface"]
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
  healthcheck:
    test: ["CMD-SHELL", "pg_isready -U storj -d mirror_index"]
    interval: 10s
    timeout: 5s
    retries: 5

storj-interface:
  # ... existing config ...
  depends_on:
    postgres:
      condition: service_healthy
  environment:
    # Single source of truth: DATABASE_URL contains the password.
    # Do NOT set POSTGRES_PASSWORD separately in storj-interface env.
    DATABASE_URL: postgres://storj:${POSTGRES_PASSWORD}@postgres:5432/mirror_index
```

Pattern: `tokio_postgres` + `batch_execute` for schema init (matches counter repo). One connection per job run. DB writes happen in a sequential post-processing loop after parallel compute/network work completes — never inside concurrent futures.

### Storj Credential Architecture

Two Storj auth paths exist; both must derive from the same root access grant:

| Path | Used by | Credential |
|---|---|---|
| `uplink` CLI | Jobs 3 (mirror upload) | `STORJ_ACCESS_GRANT_SFW` (existing env var) |
| `gateway.storjshare.io` S3 API | Jobs 0, 4 (scan/verify) | `STORJ_GATEWAY_ACCESS_KEY` + `STORJ_GATEWAY_SECRET_KEY` |

Generate S3-compatible credentials once:
```bash
uplink share --register --readonly=false \
    --access=$STORJ_ACCESS_GRANT_SFW \
    sj://yral-sfw
# Outputs: Access Key ID and Secret Key → set as STORJ_GATEWAY_ACCESS_KEY/SECRET_KEY
```

**Caveat:** Storj's S3 gateway (`gateway.storjshare.io`) supports `ListObjectsV2` but compatibility is partial (no object versioning, no multipart listing). Validate object count from Job 0 against `uplink ls -r sj://yral-sfw | wc -l` on the first run to confirm no objects are missed.

### Storj S3 Client (`src/storj_s3_client.rs`)

New `StorjS3Client` struct: same `aws-sdk-s3` code as existing `S3Client`, endpoint overridden to `https://gateway.storjshare.io`, credentials from `STORJ_GATEWAY_ACCESS_KEY` / `STORJ_GATEWAY_SECRET_KEY`. Used by Jobs 0 and 4.

### phash crate (`crates/phash/`)

Ported verbatim from `off-chain-agent/src/duplicate_video/phash.rs`. Dependencies: `ffmpeg-next`, `image_hasher`, `image`. No API changes. Produces 640-char deterministic binary string — idempotent for the same video file.

---

## Database Schema

```sql
CREATE TABLE IF NOT EXISTS video_index (
    -- Full S3 key without extension: "user123/abc123"
    -- Derived keys: hetzner_key = video_id || '.mp4'
    --               storj_key   = video_id || '.mp4'
    --               thumbnail   = video_id || '-thumbnail.png'
    video_id      TEXT PRIMARY KEY,
    hetzner_key   TEXT,     -- NULL = not known to be in Hetzner
    storj_key     TEXT,     -- NULL = not known to be in Storj
    phash         TEXT,     -- 640-char binary, NULL = not computed
    is_temp       BOOLEAN NOT NULL DEFAULT FALSE,
    retry_count   INTEGER NOT NULL DEFAULT 0,
    status        TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'phash_computed', 'mirrored', 'failed', 'done')),
    error_message TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Covering index: Job 2 SELECTs video_id + hetzner_key
CREATE INDEX IF NOT EXISTS idx_phash_null    ON video_index (video_id, hetzner_key)
    WHERE phash IS NULL AND hetzner_key IS NOT NULL AND status != 'failed';
-- Covers Job 3 WHERE clause fully: storj_key IS NULL AND hetzner_key IS NOT NULL
-- AND is_temp = FALSE AND status = 'phash_computed'
CREATE INDEX IF NOT EXISTS idx_missing_storj ON video_index (video_id, hetzner_key)
    WHERE storj_key IS NULL AND is_temp = FALSE AND hetzner_key IS NOT NULL
      AND status = 'phash_computed';
CREATE INDEX IF NOT EXISTS idx_temp_cleanup  ON video_index (video_id)
    WHERE is_temp = TRUE AND status = 'mirrored';
CREATE INDEX IF NOT EXISTS idx_status        ON video_index (status);
-- Supports duplicate phash audit query
CREATE INDEX IF NOT EXISTS idx_phash_val     ON video_index (phash)
    WHERE phash IS NOT NULL;

-- Auto-update updated_at
CREATE OR REPLACE FUNCTION update_updated_at()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN NEW.updated_at = NOW(); RETURN NEW; END;
$$;
CREATE TRIGGER video_index_updated_at
    BEFORE UPDATE ON video_index
    FOR EACH ROW EXECUTE FUNCTION update_updated_at();
```

**video_id** is the full path without extension (e.g. `"user123/abc123"`), not the filename stem alone. This prevents collisions between `user1/abc.mp4` and `user2/abc.mp4`.

**Status state machine:**
```
pending → phash_computed → mirrored
                         → done        (temp files only, after cleanup removes hetzner_key)
pending → failed                       (after retry_count >= MAX_PHASH_RETRIES, default 5)
```

`mirrored` is the **terminal state for permanent (non-temp) videos**. Only temp videos advance to `done`. `failed` is a terminal state for unrecoverable errors (corrupt video, permanently missing file). Manual intervention required to reset: `UPDATE video_index SET status='pending', retry_count=0 WHERE video_id=$1`.

---

## Job Specifications

### Shared: Backpressure + Cancellation

All jobs:
- Use `tokio::sync::Semaphore` for bounded parallel I/O
- Check `CancellationToken` between batch iterations
- DB writes happen sequentially after parallel work, not inside concurrent futures
- Downloads stream to `NamedTempFile` (disk, not in-memory); auto-deleted on drop

```
PHASH_CONCURRENCY=4      # semaphore permits for phash ops
MIRROR_CONCURRENCY=8     # semaphore permits for mirror ops
SCAN_PAGE_SIZE=1000      # S3 list_objects_v2 page size
MAX_PHASH_RETRIES=5      # retry_count ceiling before status → 'failed'
```

**CancellationToken wiring:** Create a `CancellationToken` at startup, store in `AppState`. In the shutdown signal handler, call `token.cancel()` alongside `notify_clone.notify_one()`. Each HTTP job endpoint returns 202 immediately, then `tokio::spawn`s the job task with a token clone from `AppState`. Each job checks `token.is_cancelled()` at the top of each batch iteration and returns early if set.

```rust
// In run_server():
let cancel = CancellationToken::new();
// In signal handler:
cancel.cancel();
notify_clone.notify_one();
// AppState holds cancel: CancellationToken
// Job endpoint:
let token = state.cancel.clone();
tokio::spawn(async move { run_job_phash(db, token).await });
return StatusCode::ACCEPTED;
```

Progress: `tracing::info!` every 1000 videos. Errors: `tracing::error!` + `sentry::capture_error` per video. All success UPDATE statements include `error_message = NULL` to avoid stale error counts in audit.

**Disk space:** Minimum 10GB free on temp partition before running phash/mirror jobs. `MIRROR_CONCURRENCY=8` at average 50MB/video = ~400MB simultaneous temp usage. Check with `df -h /tmp` before triggering jobs.

---

### Job 0 — Scan Storj

`POST /mirror/jobs/scan-storj` → 202 Accepted, runs in background

Reconciles the Postgres index with what rclone already copied. Must run before Job 3 to avoid re-uploading 600K already-mirrored files.

```
for each page of objects in yral-sfw (StorjS3Client.list_objects_v2):
    check CancellationToken

    for each key ending in ".mp4":
        video_id = strip ".mp4" suffix from key

        -- Always update storj_key (not first-write-wins) to allow
        -- correction if a stale or wrong key was previously recorded
        INSERT INTO video_index (video_id, storj_key)
        VALUES ($video_id, $key)
        ON CONFLICT (video_id) DO UPDATE
            SET storj_key = EXCLUDED.storj_key,
                error_message = NULL
```

After completion: validate object count against `uplink ls -r` count on first run.

---

### Job 1 — Scan Hetzner

`POST /mirror/jobs/scan-hetzner` → 202 Accepted, runs in background

```
for each page of objects in Hetzner bucket (S3Client.list_objects):
    check CancellationToken
    skip keys NOT ending in ".mp4"  -- excludes both thumbnail variants
    skip keys ending in "_thumbnail.png" or "-thumbnail.png" (redundant safety)

    video_id = strip ".mp4" suffix from key
    is_temp = key.contains(TEMP_KEY_PREFIX)  -- default "_pending/"

    INSERT INTO video_index (video_id, hetzner_key, is_temp)
    VALUES ($video_id, $key, $is_temp)
    ON CONFLICT (video_id) DO UPDATE
        SET hetzner_key = EXCLUDED.hetzner_key,
            is_temp = EXCLUDED.is_temp,
            error_message = NULL
```

Re-trigger periodically for ongoing delta (new uploads after initial rclone).

---

### Job 2 — Phash Backfill

`POST /mirror/jobs/phash` → 202 Accepted, runs in background

```
loop:
    check CancellationToken

    rows = SELECT video_id, hetzner_key FROM video_index
           WHERE phash IS NULL
             AND hetzner_key IS NOT NULL   -- skip Storj-only rows (Job 1 not yet run)
             AND status = 'pending'        -- skip mirrored/failed rows
           LIMIT SCAN_PAGE_SIZE

    if empty: break

    // Use buffer_unordered instead of join_all so at most PHASH_CONCURRENCY
    // NamedTempFiles exist on disk simultaneously (join_all would open all 1000)
    results: Vec<(video_id, Result<phash>)> =
        futures::stream::iter(rows)
            .map(|row| async {
                _permit = semaphore.acquire()

                // NamedTempFile created in async scope; only .path() passed into
                // spawn_blocking — file handle stays in async scope, not moved
                // into the blocking thread, so drop always runs here
                tmp = NamedTempFile::new()
                stream_download(hetzner_key, tmp.as_file_mut()).await?

                phash_result = spawn_blocking({
                    let path = tmp.path().to_owned()
                    move || PHasher::new().compute_hash(&path)
                }).await

                drop(tmp)  // file deleted here, always, before permit released

                (row.video_id, phash_result)
            })
            .buffer_unordered(PHASH_CONCURRENCY)
            .collect()
            .await

    // Sequential DB writes after all parallel work done
    for (video_id, result) in results:
        if Ok(phash):
            UPDATE video_index
            SET phash=$phash, status='phash_computed', error_message=NULL
            WHERE video_id=$video_id AND status='pending'
            -- status='pending' guard prevents regression if a concurrent Job 3
            -- already advanced this row to 'mirrored'

        if Err(e):
            // Atomic increment + conditional status: single round trip, no TOCTOU
            UPDATE video_index
            SET retry_count = retry_count + 1,
                status = CASE WHEN retry_count + 1 >= $MAX_PHASH_RETRIES
                              THEN 'failed' ELSE 'pending' END,
                error_message = $e
            WHERE video_id=$video_id
            RETURNING retry_count AS new_retries INTO new_retries

            tracing::error!(video_id, retries=new_retries, error=%e, "phash failed")
            sentry::capture_error(&e)
```

---

### Job 3 — Mirror Incremental

`POST /mirror/jobs/mirror` → 202 Accepted, runs in background

```
loop:
    check CancellationToken

    rows = SELECT video_id, hetzner_key FROM video_index
           WHERE storj_key IS NULL
             AND hetzner_key IS NOT NULL
             AND is_temp = FALSE
             AND status = 'phash_computed'
           LIMIT SCAN_PAGE_SIZE

    if empty: break

    // Parallel upload; DB writes happen AFTER each video completes
    semaphore = Semaphore(MIRROR_CONCURRENCY)
    results: Vec<(video_id, Result<()>)> = join_all(rows.map(|row| async {
        _permit = semaphore.acquire()

        // 1. Copy MP4 (required)
        tmp_mp4 = NamedTempFile::new()
        stream_download(hetzner_key, tmp_mp4.as_file_mut()).await?
        uplink_cp(tmp_mp4.path(), format!("sj://yral-sfw/{hetzner_key}")).await?
        drop(tmp_mp4)

        // 2. Copy thumbnail (in scope per spec; missing thumbnail = warn not error)
        // Use strip_suffix to avoid replacing ".mp4" within folder names
        thumb_key = hetzner_key.strip_suffix(".mp4")
            .map(|stem| format!("{stem}-thumbnail.png"))
        if let Some(thumb_key) = thumb_key:
            match S3Client.object_exists(&thumb_key).await:
                Ok(true):
                    tmp_thumb = NamedTempFile::new()
                    stream_download(&thumb_key, tmp_thumb.as_file_mut()).await?
                    uplink_cp(tmp_thumb.path(), format!("sj://yral-sfw/{thumb_key}")).await?
                    drop(tmp_thumb)
                Ok(false):
                    // Thumbnail absent on Hetzner — warn but do not block MP4 mirror
                    tracing::warn!(video_id, "thumbnail missing on Hetzner, mirroring MP4 only")
                    sentry::add_breadcrumb(...)  // not a capture_error; expected for some videos
                Err(e):
                    // Transient S3 error checking thumbnail — treat as hard error, retry row
                    return Err(e)

        Ok(row.video_id)
    }))

    // Sequential DB writes
    for (video_id, result) in results:
        if Ok(_):
            UPDATE video_index
            SET storj_key=hetzner_key, status='mirrored', error_message=NULL
            WHERE video_id=$video_id
        if Err(e):
            UPDATE video_index SET error_message=$e WHERE video_id=$video_id
            -- storj_key stays NULL → retried next run
            tracing::error!(video_id, error=%e, "mirror failed")
            sentry::capture_error(&e)
```

All-or-none per video pair: DB commit only after both MP4 and thumbnail uploads succeed. If process crashes after upload but before DB update, next run re-uploads (uplink cp to same key = idempotent overwrite, no duplicate data).

---

### Job 4 — Temp Cleanup

`POST /mirror/jobs/cleanup` → 202 Accepted, runs in background

**Intentionally serial** (no concurrency semaphore) — deletion is irreversible; safety over throughput. Do not trigger Job 4 until `GET /mirror/audit` shows `missing_storj = 0`.

```
loop:
    check CancellationToken

    rows = SELECT video_id, hetzner_key, storj_key FROM video_index
           WHERE is_temp = TRUE AND status = 'mirrored'
             AND hetzner_key IS NOT NULL AND storj_key IS NOT NULL
           LIMIT SCAN_PAGE_SIZE

    if empty: break

    for each row:
        // object_exists returns Ok(true), Ok(false), or Err(e)
        // Err(e)     → transient error → abort row, log, retry next run (do NOT delete)
        // Ok(false)  → Storj copy missing → CRITICAL alert, do NOT delete Hetzner copy
        // Ok(true)   → safe to delete
        match StorjS3Client.object_exists(storj_key):
            Err(e)      → tracing::error!; sentry::capture_error(&e); continue next row
            Ok(false)   → tracing::error!("Storj copy missing before cleanup — data loss risk");
                          sentry::capture_error(...); continue next row
            Ok(true)    → proceed

        S3Client.delete_video(hetzner_key)?

        UPDATE video_index
        SET hetzner_key=NULL, is_temp=FALSE, status='done', error_message=NULL
        WHERE video_id=$video_id

        // on delete error: log + sentry, row retried next run
```

---

### Audit Endpoint

`GET /mirror/audit` — auth required, no side effects

```sql
SELECT
    COUNT(*)                                              AS total,
    COUNT(phash)                                          AS phash_computed,
    COUNT(*) FILTER (WHERE status IN ('mirrored','done')) AS mirrored,
    COUNT(*) FILTER (WHERE storj_key IS NULL
                       AND is_temp = FALSE)               AS missing_storj,
    COUNT(*) FILTER (WHERE hetzner_key IS NULL
                       AND is_temp = FALSE
                       AND status != 'done')              AS missing_hetzner,
    COUNT(*) FILTER (WHERE is_temp = TRUE
                       AND status = 'mirrored')           AS cleanup_pending,
    COUNT(*) FILTER (WHERE status = 'failed')             AS failed,
    COUNT(*) FILTER (WHERE error_message IS NOT NULL)     AS error_count
FROM video_index;
```

```sql
-- Duplicate phash audit (uses idx_phash_val)
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
    storj_s3_client.rs    -- aws-sdk-s3 → gateway.storjshare.io
    jobs/
      mod.rs              -- CancellationToken wiring, log_progress helper
      scan_storj.rs       -- Job 0
      scan_hetzner.rs     -- Job 1
      phash_backfill.rs   -- Job 2
      mirror.rs           -- Job 3
      cleanup.rs          -- Job 4
    routes/
      mirror.rs           -- 202 trigger endpoints + GET /mirror/audit
      mod.rs              -- wire mirror router
  deploy/
    docker-compose.yml    -- postgres service + healthcheck + postgres_data volume
    Dockerfile            -- multi-stage debian build
  .github/workflows/
    build-binary.yml      -- x86_64-unknown-linux-gnu + ffmpeg-dev headers
```

---

## New Environment Variables

| Variable | Default | Purpose |
|---|---|---|
| `DATABASE_URL` | required | Full postgres URL including password |
| `POSTGRES_PASSWORD` | required | Used only in docker-compose to build `DATABASE_URL` |
| `STORJ_GATEWAY_ACCESS_KEY` | required | From `uplink share --register` (see setup below) |
| `STORJ_GATEWAY_SECRET_KEY` | required | From `uplink share --register` |
| `STORJ_SFW_BUCKET` | `yral-sfw` | Target Storj bucket name |
| `PHASH_CONCURRENCY` | `4` | Semaphore permits for phash ops |
| `MIRROR_CONCURRENCY` | `8` | Semaphore permits for mirror ops |
| `SCAN_PAGE_SIZE` | `1000` | S3 list_objects_v2 page size |
| `MAX_PHASH_RETRIES` | `5` | Retry ceiling before status → 'failed' |
| `TEMP_KEY_PREFIX` | `_pending/` | Hetzner key prefix identifying temp objects |

`STORJ_ACCESS_GRANT_SFW` is existing — reused as-is by uplink CLI in Job 3.

---

## Operational Steps

### One-time setup: Storj gateway credentials

```bash
# Run once; save output as GitHub secrets
uplink share --register --readonly=false \
    --access=$STORJ_ACCESS_GRANT_SFW \
    sj://yral-sfw
# → outputs Access Key ID  → STORJ_GATEWAY_ACCESS_KEY
# → outputs Secret Key     → STORJ_GATEWAY_SECRET_KEY
```

### Initial migration

```bash
# 0. Check disk space (need ≥10GB free on /tmp)
df -h /tmp

# 1. Deploy updated service with Postgres
docker compose up -d

# 2. Bulk copy via rclone (use actual HETZNER_S3_BUCKET value, not hardcoded name)
rclone copy ${HETZNER_S3_BUCKET}: storj:yral-sfw \
    --exclude "*_thumbnail.png" \
    --transfers=32 --checkers=64 \
    --progress

# 3. Reconcile index (can run concurrently)
curl -X POST https://storj-interface.yral.com/mirror/jobs/scan-storj \
    -H "Authorization: Bearer $SERVICE_SECRET_TOKEN"
curl -X POST https://storj-interface.yral.com/mirror/jobs/scan-hetzner \
    -H "Authorization: Bearer $SERVICE_SECRET_TOKEN"

# 4. Validate Job 0 object count (uplink outputs one object per line with metadata;
#    --recursive lists all; awk extracts the last field which is the key path)
uplink ls --recursive sj://yral-sfw | awk '{print $NF}' | grep '\.mp4$' | wc -l
# compare against: curl /mirror/audit | jq .total

# 5. Phash backfill (long-running; re-trigger if interrupted)
curl -X POST https://storj-interface.yral.com/mirror/jobs/phash \
    -H "Authorization: Bearer $SERVICE_SECRET_TOKEN"

# 6. Mirror any gaps
curl -X POST https://storj-interface.yral.com/mirror/jobs/mirror \
    -H "Authorization: Bearer $SERVICE_SECRET_TOKEN"

# 7. Audit
curl https://storj-interface.yral.com/mirror/audit \
    -H "Authorization: Bearer $SERVICE_SECRET_TOKEN" | jq

# 8. Cleanup temp files (only when audit shows missing_storj=0)
curl -X POST https://storj-interface.yral.com/mirror/jobs/cleanup \
    -H "Authorization: Bearer $SERVICE_SECRET_TOKEN"
```

### Ongoing delta (after initial migration)

Re-trigger Jobs 1, 2, 3 on a schedule to pick up new uploads. Jobs are idempotent — already-processed rows are skipped by their WHERE clauses.

---

## Production Requirements Checklist

- [x] Idempotent jobs — upsert without first-write-wins, WHERE guards on status
- [x] All-or-none per video — DB update only after successful upload (MP4 + thumbnail)
- [x] Backpressure — semaphore bounds concurrent in-flight ops
- [x] Tempfile cleanup — `NamedTempFile` auto-deletes on drop; path passed to spawn_blocking, file handle stays in async scope
- [x] Graceful shutdown — CancellationToken checked between batch iterations; HTTP endpoints return 202 immediately
- [x] Retry cap — `retry_count` + `MAX_PHASH_RETRIES`; terminal `failed` state prevents infinite retry storms
- [x] Observability — `tracing::info!` every 1000, `tracing::error!` per failure
- [x] Sentry — `sentry::capture_error` on every per-video and infrastructure failure
- [x] Auth — `SERVICE_SECRET_TOKEN` middleware on all job endpoints
- [x] Resumable — jobs pick up from WHERE clause on next trigger
- [x] Disk space — documented minimum 10GB free before running jobs
- [x] Storj dual-auth — documented: uplink CLI (Job 3) + S3 gateway (Jobs 0, 4) from same root grant
- [x] DB writes sequential — parallel block collects results, sequential loop writes to Postgres
- [x] error_message cleared — all success UPDATE paths include `error_message = NULL`
