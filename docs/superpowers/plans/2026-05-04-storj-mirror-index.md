# Storj Mirror Index Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a 5-job HTTP-triggered system that mirrors Hetzner SFW videos to fresh Storj `yral-sfw` bucket, backed by Postgres index with phash fingerprints for dedup audit.

**Architecture:** Jobs triggered via `POST /mirror/jobs/*` return 202 immediately and run as background tokio tasks. `CancellationToken` in `AppState` wires SIGTERM to graceful mid-batch exit. `tokio_postgres` per-job connection. `buffer_unordered` bounds concurrent in-flight tempfiles. DB writes are always sequential after parallel compute.

**Tech Stack:** Rust/Axum, tokio-postgres 0.7, aws-sdk-s3 (Hetzner + Storj S3 gateway), uplink CLI (Storj uploads), ffmpeg-next 7 + image_hasher 3 (phash), futures::StreamExt (buffer_unordered), tokio-util (CancellationToken), PostgreSQL 16.

**Spec:** `docs/superpowers/specs/2026-05-04-storj-mirror-index-design.md`

---

## File Map

| File | Action | Responsibility |
|---|---|---|
| `crates/phash/Cargo.toml` | Create | phash crate manifest |
| `crates/phash/src/lib.rs` | Create | PHasher — port from off-chain-agent |
| `Cargo.toml` | Modify | Add phash crate + new deps |
| `.github/workflows/build-binary.yml` | Modify | gnu target + ffmpeg-dev headers |
| `deploy/Dockerfile` | Modify | Multi-stage: builder + runtime |
| `src/consts.rs` | Modify | 9 new env var statics |
| `src/s3_client.rs` | Modify | Add `download_to_file` streaming method |
| `src/storj_s3_client.rs` | Create | StorjS3Client → gateway.storjshare.io |
| `src/db.rs` | Create | Postgres connect, schema init, all query functions |
| `src/main.rs` | Modify | AppState + CancellationToken + DB init |
| `src/lib.rs` | Modify | Re-export new modules |
| `src/jobs/mod.rs` | Create | Key helpers, uplink_cp, log_progress |
| `src/jobs/scan_storj.rs` | Create | Job 0 |
| `src/jobs/scan_hetzner.rs` | Create | Job 1 |
| `src/jobs/phash_backfill.rs` | Create | Job 2 |
| `src/jobs/mirror.rs` | Create | Job 3 |
| `src/jobs/cleanup.rs` | Create | Job 4 |
| `src/routes/mirror.rs` | Create | 202 endpoints + GET /mirror/audit |
| `src/routes/mod.rs` | Modify | Wire mirror router |
| `deploy/docker-compose.yml` | Modify | Add postgres service + healthcheck |
| `.env.example` | Modify | Document new env vars |

---

### Task 1: phash crate

Port `PHasher` from off-chain-agent. Only port the struct and its methods — skip VideoMetadata, download helpers, and Storj-specific functions.

**Files:**
- Create: `crates/phash/Cargo.toml`
- Create: `crates/phash/src/lib.rs`
- Modify: `Cargo.toml` (root workspace members)

- [ ] **Step 1: Create crate manifest**

```toml
# crates/phash/Cargo.toml
[package]
name = "phash"
version = "0.1.0"
edition = "2021"

[dependencies]
anyhow = "1"
ffmpeg-next = "7"
image = { version = "0.25", default-features = false }
image_hasher = "3.0"
log = "0.4"
```

- [ ] **Step 2: Port PHasher to lib.rs**

Copy only `PHasher` struct + all its methods + `Default` impl from `off-chain-agent/src/duplicate_video/phash.rs`. Do NOT copy `VideoMetadata`, `extract_metadata`, `download_video_from_storj`, `download_video_from_url`, `compute_phash_from_url`, `compute_phash_from_storj` — those are off-chain-agent internals.

The resulting file starts with:
```rust
use anyhow::{Context, Result};
use image::DynamicImage;
use image_hasher::{HasherConfig, ImageHash};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct PHasher {
    num_frames: usize,
    hash_size: u32,
}
// ... rest of impl verbatim ...
```

Keep the existing `#[cfg(test)]` block at the bottom (the two unit tests for PHasher creation).

- [ ] **Step 3: Add phash to workspace**

In root `Cargo.toml`, add to `[workspace] members`:
```toml
members = [".", "crates/backfill-thumbnails", "crates/phash"]
```

- [ ] **Step 4: Verify crate compiles (will fail — ffmpeg-next needs gnu target)**

```bash
cargo check -p phash 2>&1 | head -30
```

Expected: compile error about missing libav headers (this is expected — fixed in Task 2).

- [ ] **Step 5: Run existing tests to confirm no regressions**

```bash
cargo test -p storj-interface 2>&1 | tail -20
```

Expected: existing tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/phash/ Cargo.toml Cargo.lock
git commit -m "feat: add phash crate ported from off-chain-agent"
```

---

### Task 2: Build changes — gnu target + multi-stage Dockerfile

Switch from musl to gnu to enable ffmpeg-next native C lib linking.

**Files:**
- Modify: `.github/workflows/build-binary.yml`
- Modify: `deploy/Dockerfile`

- [ ] **Step 1: Update CI workflow**

In `.github/workflows/build-binary.yml`, replace the `Rust Setup` step and `Build binary` step:

```yaml
      - name: Rust Setup
        uses: dtolnay/rust-toolchain@master
        with:
          toolchain: "stable"
          targets: "x86_64-unknown-linux-gnu"
          components: "rustfmt"

      - name: Cache and install ffmpeg build tools
        uses: awalsh128/cache-apt-pkgs-action@v1
        with:
          packages: libavformat-dev libavcodec-dev libavutil-dev libswscale-dev clang pkg-config
          version: 1.0

      - name: Build binary
        run: cargo build --release --target x86_64-unknown-linux-gnu
```

Remove the `Cache and install Musl build tools` step entirely.

In the `Upload artifact` step, update the path:
```yaml
          path: target/x86_64-unknown-linux-gnu/release/storj-interface
```

- [ ] **Step 2: Update deploy workflow artifact path**

In `.github/workflows/deploy-baremetal.yml`, update `Place binary in expected path`:
```yaml
      - name: Place binary in expected path
        run: |
          mkdir -p target/x86_64-unknown-linux-gnu/release
          mv storj-interface target/x86_64-unknown-linux-gnu/release/storj-interface
          chmod +x target/x86_64-unknown-linux-gnu/release/storj-interface
```

- [ ] **Step 3: Replace Dockerfile with multi-stage build**

Replace entire `deploy/Dockerfile`:
```dockerfile
# Stage 1: builder — needs ffmpeg dev headers to compile ffmpeg-next
FROM rust:bookworm AS builder

RUN apt-get update && apt-get install -y \
    libavformat-dev libavcodec-dev libavutil-dev libswscale-dev \
    clang pkg-config curl unzip \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY . .
RUN cargo build --release --target x86_64-unknown-linux-gnu

# Stage 2: runtime — only ffmpeg runtime libs + uplink CLI
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ffmpeg ca-certificates curl unzip \
    && rm -rf /var/lib/apt/lists/*

# Pin uplink version for reproducible builds
RUN curl -L https://github.com/storj/storj/releases/download/v1.112.4/uplink_linux_amd64.zip \
    -o /tmp/uplink.zip \
    && unzip /tmp/uplink.zip -d /tmp \
    && install /tmp/uplink /usr/local/bin/uplink \
    && rm -rf /tmp/uplink /tmp/uplink.zip

WORKDIR /app
COPY --from=builder /app/target/x86_64-unknown-linux-gnu/release/storj-interface .

EXPOSE 3000
ENTRYPOINT ["./storj-interface"]
```

- [ ] **Step 4: Verify phash crate now compiles locally**

```bash
cargo check -p phash
```

Expected: PASS (ffmpeg headers now available if running on Linux/macOS with ffmpeg installed). On macOS without libav, this may still fail — that is OK, CI will verify it.

- [ ] **Step 5: Confirm existing tests still pass**

```bash
cargo test -p storj-interface
```

Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add .github/workflows/build-binary.yml .github/workflows/deploy-baremetal.yml deploy/Dockerfile
git commit -m "feat: switch to gnu target + multi-stage Dockerfile for ffmpeg-next support"
```

---

### Task 3: Add new cargo dependencies

**Files:**
- Modify: `Cargo.toml` (root crate dependencies)

- [ ] **Step 1: Add deps to [dependencies] in root Cargo.toml**

```toml
# New deps for mirror index feature
futures = "0.3"
phash = { path = "crates/phash" }
tempfile = "3"          # already present — verify, don't duplicate
tokio-postgres = "0.7.15"
tokio-util = { version = "0.7", features = ["sync"] }
```

Note: `tempfile` is already in Cargo.toml — skip if present.

- [ ] **Step 2: Verify Cargo.toml parses**

```bash
cargo check --message-format short 2>&1 | head -20
```

Expected: no "failed to parse" errors. (Compile errors about missing source files are expected.)

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore: add tokio-postgres, tokio-util, futures deps for mirror index"
```

---

### Task 4: New constants in consts.rs

**Files:**
- Modify: `src/consts.rs`

- [ ] **Step 1: Add 9 new Lazy statics at end of file**

```rust
// Storj S3 gateway credentials (for listing/verifying, not uploads)
pub static STORJ_GATEWAY_ACCESS_KEY: Lazy<String> = Lazy::new(|| {
    std::env::var("STORJ_GATEWAY_ACCESS_KEY")
        .expect("STORJ_GATEWAY_ACCESS_KEY required")
});
pub static STORJ_GATEWAY_SECRET_KEY: Lazy<String> = Lazy::new(|| {
    std::env::var("STORJ_GATEWAY_SECRET_KEY")
        .expect("STORJ_GATEWAY_SECRET_KEY required")
});
pub static STORJ_SFW_BUCKET: Lazy<String> = Lazy::new(|| {
    std::env::var("STORJ_SFW_BUCKET").unwrap_or_else(|_| "yral-sfw".to_string())
});

// Database
pub static DATABASE_URL: Lazy<String> = Lazy::new(|| {
    std::env::var("DATABASE_URL").expect("DATABASE_URL required")
});

// Job tuning
pub static PHASH_CONCURRENCY: Lazy<usize> = Lazy::new(|| {
    std::env::var("PHASH_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4)
});
pub static MIRROR_CONCURRENCY: Lazy<usize> = Lazy::new(|| {
    std::env::var("MIRROR_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8)
});
pub static SCAN_PAGE_SIZE: Lazy<i64> = Lazy::new(|| {
    std::env::var("SCAN_PAGE_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000)
});
pub static MAX_PHASH_RETRIES: Lazy<i32> = Lazy::new(|| {
    std::env::var("MAX_PHASH_RETRIES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5)
});
pub static TEMP_KEY_PREFIX: Lazy<String> = Lazy::new(|| {
    std::env::var("TEMP_KEY_PREFIX").unwrap_or_else(|_| "_pending/".to_string())
});
```

- [ ] **Step 2: Verify**

```bash
cargo check --message-format short 2>&1 | grep "src/consts"
```

Expected: no errors from consts.rs.

- [ ] **Step 3: Commit**

```bash
git add src/consts.rs
git commit -m "feat: add mirror index env var constants"
```

---

### Task 5: Add streaming download to S3Client

The existing `download_video` loads into memory. Jobs need streaming to `NamedTempFile` for 600K large videos.

**Files:**
- Modify: `src/s3_client.rs`

- [ ] **Step 1: Write the failing test**

At the bottom of `src/s3_client.rs`, in `#[cfg(test)]`:
```rust
#[cfg(test)]
mod tests {
    // Key parsing tests live here too — see Task 9
    // download_to_file tested via integration in Task 10
}
```

(Stub for now — streaming download is tested implicitly through phash job integration.)

- [ ] **Step 2: Add download_to_file method to S3Client impl**

Add after the existing `download_object` method:

```rust
/// Stream an S3 object directly to an open file — avoids loading into memory
pub async fn download_to_file(
    &self,
    key: &str,
    file: &mut tokio::fs::File,
) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;

    let resp = self
        .client
        .get_object()
        .bucket(&self.bucket)
        .key(key)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let mut body = resp.body;
    while let Some(chunk) = body.next().await {
        let bytes = chunk.map_err(|e| e.to_string())?;
        file.write_all(&bytes).await.map_err(|e| e.to_string())?;
    }
    Ok(())
}
```

Add `use futures_util::StreamExt;` at the top (already imported — check existing imports first).

- [ ] **Step 3: Verify**

```bash
cargo check -p storj-interface --message-format short 2>&1 | grep "s3_client"
```

Expected: no errors.

- [ ] **Step 4: Commit**

```bash
git add src/s3_client.rs
git commit -m "feat: add streaming download_to_file to S3Client"
```

---

### Task 6: StorjS3Client

S3-compatible client pointing at Storj gateway for listing and HEAD checks (Jobs 0 and 4). Uploads still use uplink CLI.

**Files:**
- Create: `src/storj_s3_client.rs`

- [ ] **Step 1: Write the test first**

```rust
// src/storj_s3_client.rs — at the bottom
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storj_s3_client_uses_gateway_endpoint() {
        // Verify the endpoint constant is correct (no runtime required)
        assert_eq!(STORJ_GATEWAY_ENDPOINT, "https://gateway.storjshare.io");
    }
}
```

- [ ] **Step 2: Run test — it will fail (file doesn't exist yet)**

```bash
cargo test storj_s3_client 2>&1 | tail -10
```

Expected: compile error (module not declared).

- [ ] **Step 3: Implement StorjS3Client**

```rust
// src/storj_s3_client.rs
use aws_config::BehaviorVersion;
use aws_sdk_s3::config::{Credentials, Region};
use aws_sdk_s3::Client;

use crate::consts::{STORJ_GATEWAY_ACCESS_KEY, STORJ_GATEWAY_SECRET_KEY, STORJ_SFW_BUCKET};
use crate::s3_client::{S3Client, S3ObjectInfo};

pub const STORJ_GATEWAY_ENDPOINT: &str = "https://gateway.storjshare.io";

#[derive(Clone)]
pub struct StorjS3Client(S3Client);

impl StorjS3Client {
    pub async fn new() -> Self {
        let creds = Credentials::new(
            STORJ_GATEWAY_ACCESS_KEY.as_str(),
            STORJ_GATEWAY_SECRET_KEY.as_str(),
            None,
            None,
            "storj_gateway",
        );

        let config = aws_sdk_s3::config::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1")) // Storj gateway ignores region
            .endpoint_url(STORJ_GATEWAY_ENDPOINT)
            .credentials_provider(creds)
            .force_path_style(true)
            .build();

        let client = Client::from_conf(config);

        // Re-use S3Client internals by constructing it directly
        // S3Client is a public struct with public fields — check s3_client.rs
        // If fields are private, expose a constructor: S3Client::from_parts(client, bucket)
        // For now, delegate to the inner client directly
        let inner = S3Client::from_raw(client, STORJ_SFW_BUCKET.clone());
        Self(inner)
    }

    pub async fn list_objects(
        &self,
        prefix: Option<&str>,
    ) -> Result<Vec<S3ObjectInfo>, String> {
        self.0.list_objects(prefix).await
    }

    pub async fn object_exists(&self, key: &str) -> Result<bool, String> {
        self.0.object_exists(key).await
    }
}
```

**Note:** `S3Client` currently has private fields. You must add a `pub fn from_raw(client: aws_sdk_s3::Client, bucket: String) -> Self` constructor to `src/s3_client.rs`:

```rust
// Add to S3Client impl in s3_client.rs
pub fn from_raw(client: aws_sdk_s3::Client, bucket: String) -> Self {
    Self { client, bucket }
}
```

- [ ] **Step 4: Declare module in lib.rs and main.rs**

In `src/lib.rs`, add:
```rust
pub mod storj_s3_client;
```

In `src/main.rs`, add:
```rust
mod storj_s3_client;
```

- [ ] **Step 5: Run test**

```bash
cargo test storj_s3_client_uses_gateway_endpoint
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/storj_s3_client.rs src/s3_client.rs src/lib.rs src/main.rs
git commit -m "feat: add StorjS3Client for S3-gateway-compatible listing"
```

---

### Task 7: Database layer (db.rs)

All Postgres logic in one module: connect, schema init, and every query the jobs need.

**Files:**
- Create: `src/db.rs`

- [ ] **Step 1: Write the tests first**

These are integration tests that spin up a real postgres container (same pattern as counter repo):

```rust
// At the bottom of src/db.rs
#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct PgContainer {
        name: String,
        url: String,
    }

    impl PgContainer {
        async fn spawn() -> Self {
            let port = TcpListener::bind("127.0.0.1:0").unwrap()
                .local_addr().unwrap().port();
            let name = format!("mirror-test-{}-{}", std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed));
            Command::new("docker").args([
                "run", "--rm", "--detach", "--name", &name,
                "-e", "POSTGRES_PASSWORD=test",
                "-e", "POSTGRES_USER=test",
                "-e", "POSTGRES_DB=test",
                "-p", &format!("{port}:5432"),
                "postgres:16-alpine",
            ]).status().expect("docker run");
            let url = format!("postgres://test:test@127.0.0.1:{port}/test");
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            Self { name, url }
        }
    }

    impl Drop for PgContainer {
        fn drop(&mut self) {
            Command::new("docker").args(["rm", "-f", &self.name]).status().ok();
        }
    }

    #[tokio::test]
    async fn schema_init_is_idempotent() {
        let pg = PgContainer::spawn().await;
        let client = connect(&pg.url).await.unwrap();
        init_schema(&client).await.unwrap();
        init_schema(&client).await.unwrap(); // second call must not error
    }

    #[tokio::test]
    async fn upsert_storj_key_overwrites_existing() {
        let pg = PgContainer::spawn().await;
        let client = connect(&pg.url).await.unwrap();
        init_schema(&client).await.unwrap();

        upsert_storj_key(&client, "user/abc", "user/abc.mp4").await.unwrap();
        upsert_storj_key(&client, "user/abc", "user/abc-v2.mp4").await.unwrap(); // overwrite

        let rows = fetch_pending_phash_batch(&client, 10).await.unwrap();
        assert!(rows.iter().any(|r| r.video_id == "user/abc"));
    }

    #[tokio::test]
    async fn phash_failure_increments_retry_count_atomically() {
        let pg = PgContainer::spawn().await;
        let client = connect(&pg.url).await.unwrap();
        init_schema(&client).await.unwrap();

        upsert_hetzner_key(&client, "user/vid", "user/vid.mp4", false).await.unwrap();
        let r1 = update_phash_failure(&client, "user/vid", "err", 5).await.unwrap();
        let r2 = update_phash_failure(&client, "user/vid", "err", 5).await.unwrap();
        assert_eq!(r1, 1);
        assert_eq!(r2, 2);
    }

    #[tokio::test]
    async fn phash_failure_marks_failed_at_max_retries() {
        let pg = PgContainer::spawn().await;
        let client = connect(&pg.url).await.unwrap();
        init_schema(&client).await.unwrap();

        upsert_hetzner_key(&client, "user/vid", "user/vid.mp4", false).await.unwrap();
        for _ in 0..5 {
            update_phash_failure(&client, "user/vid", "err", 5).await.unwrap();
        }

        let stats = get_audit_stats(&client).await.unwrap();
        assert_eq!(stats.failed, 1);
    }

    #[tokio::test]
    async fn audit_stats_counts_correctly() {
        let pg = PgContainer::spawn().await;
        let client = connect(&pg.url).await.unwrap();
        init_schema(&client).await.unwrap();

        upsert_hetzner_key(&client, "user/a", "user/a.mp4", false).await.unwrap();
        upsert_storj_key(&client, "user/b", "user/b.mp4").await.unwrap();

        let stats = get_audit_stats(&client).await.unwrap();
        assert_eq!(stats.total, 2);
        assert_eq!(stats.missing_storj, 1); // user/a has no storj_key
    }
}
```

- [ ] **Step 2: Run tests — they must fail**

```bash
cargo test -p storj-interface db:: 2>&1 | tail -5
```

Expected: compile error (db module not yet created).

- [ ] **Step 3: Implement db.rs**

```rust
// src/db.rs
use tokio_postgres::{Client, NoTls};

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS video_index (
    video_id      TEXT PRIMARY KEY,
    hetzner_key   TEXT,
    storj_key     TEXT,
    phash         TEXT,
    is_temp       BOOLEAN NOT NULL DEFAULT FALSE,
    retry_count   INTEGER NOT NULL DEFAULT 0,
    status        TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending','phash_computed','mirrored','failed','done')),
    error_message TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_phash_null ON video_index (video_id, hetzner_key)
    WHERE phash IS NULL AND hetzner_key IS NOT NULL AND status != 'failed';

CREATE INDEX IF NOT EXISTS idx_missing_storj ON video_index (video_id, hetzner_key)
    WHERE storj_key IS NULL AND is_temp = FALSE
      AND hetzner_key IS NOT NULL AND status = 'phash_computed';

CREATE INDEX IF NOT EXISTS idx_temp_cleanup ON video_index (video_id)
    WHERE is_temp = TRUE AND status = 'mirrored';

CREATE INDEX IF NOT EXISTS idx_status ON video_index (status);

CREATE INDEX IF NOT EXISTS idx_phash_val ON video_index (phash)
    WHERE phash IS NOT NULL;

CREATE OR REPLACE FUNCTION update_updated_at()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN NEW.updated_at = NOW(); RETURN NEW; END;
$$;

DROP TRIGGER IF EXISTS video_index_updated_at ON video_index;
CREATE TRIGGER video_index_updated_at
    BEFORE UPDATE ON video_index
    FOR EACH ROW EXECUTE FUNCTION update_updated_at();
";

pub struct VideoRow {
    pub video_id: String,
    pub hetzner_key: String,
}

pub struct CleanupRow {
    pub video_id: String,
    pub hetzner_key: String,
    pub storj_key: String,
}

pub struct AuditStats {
    pub total: i64,
    pub phash_computed: i64,
    pub mirrored: i64,
    pub missing_storj: i64,
    pub missing_hetzner: i64,
    pub cleanup_pending: i64,
    pub failed: i64,
    pub error_count: i64,
}

pub struct DuplicatePhash {
    pub phash: String,
    pub video_ids: Vec<String>,
}

pub async fn connect(url: &str) -> Result<Client, tokio_postgres::Error> {
    let (client, connection) = tokio_postgres::connect(url, NoTls).await?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            tracing::error!(error = %e, "postgres connection closed");
        }
    });
    Ok(client)
}

pub async fn init_schema(client: &Client) -> Result<(), tokio_postgres::Error> {
    client.batch_execute(SCHEMA_SQL).await
}

// --- Job 0: Scan Storj ---

pub async fn upsert_storj_key(
    client: &Client,
    video_id: &str,
    storj_key: &str,
) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            "INSERT INTO video_index (video_id, storj_key)
             VALUES ($1, $2)
             ON CONFLICT (video_id) DO UPDATE
             SET storj_key = EXCLUDED.storj_key,
                 error_message = NULL",
            &[&video_id, &storj_key],
        )
        .await?;
    Ok(())
}

// --- Job 1: Scan Hetzner ---

pub async fn upsert_hetzner_key(
    client: &Client,
    video_id: &str,
    hetzner_key: &str,
    is_temp: bool,
) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            "INSERT INTO video_index (video_id, hetzner_key, is_temp)
             VALUES ($1, $2, $3)
             ON CONFLICT (video_id) DO UPDATE
             SET hetzner_key = EXCLUDED.hetzner_key,
                 is_temp = EXCLUDED.is_temp,
                 error_message = NULL",
            &[&video_id, &hetzner_key, &is_temp],
        )
        .await?;
    Ok(())
}

// --- Job 2: Phash Backfill ---

pub async fn fetch_pending_phash_batch(
    client: &Client,
    limit: i64,
) -> Result<Vec<VideoRow>, tokio_postgres::Error> {
    let rows = client
        .query(
            "SELECT video_id, hetzner_key FROM video_index
             WHERE phash IS NULL
               AND hetzner_key IS NOT NULL
               AND status = 'pending'
             LIMIT $1",
            &[&limit],
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| VideoRow {
            video_id: r.get(0),
            hetzner_key: r.get(1),
        })
        .collect())
}

pub async fn update_phash_success(
    client: &Client,
    video_id: &str,
    phash: &str,
) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            "UPDATE video_index
             SET phash = $1, status = 'phash_computed', error_message = NULL
             WHERE video_id = $2 AND status = 'pending'",
            &[&phash, &video_id],
        )
        .await?;
    Ok(())
}

/// Atomically increments retry_count and sets status to 'failed' when threshold reached.
/// Returns the new retry_count.
pub async fn update_phash_failure(
    client: &Client,
    video_id: &str,
    error: &str,
    max_retries: i32,
) -> Result<i32, tokio_postgres::Error> {
    let row = client
        .query_one(
            "UPDATE video_index
             SET retry_count = retry_count + 1,
                 status = CASE WHEN retry_count + 1 >= $3
                               THEN 'failed' ELSE 'pending' END,
                 error_message = $2
             WHERE video_id = $1
             RETURNING retry_count",
            &[&video_id, &error, &max_retries],
        )
        .await?;
    Ok(row.get(0))
}

// --- Job 3: Mirror ---

pub async fn fetch_pending_mirror_batch(
    client: &Client,
    limit: i64,
) -> Result<Vec<VideoRow>, tokio_postgres::Error> {
    let rows = client
        .query(
            "SELECT video_id, hetzner_key FROM video_index
             WHERE storj_key IS NULL
               AND hetzner_key IS NOT NULL
               AND is_temp = FALSE
               AND status = 'phash_computed'
             LIMIT $1",
            &[&limit],
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| VideoRow {
            video_id: r.get(0),
            hetzner_key: r.get(1),
        })
        .collect())
}

pub async fn update_mirror_success(
    client: &Client,
    video_id: &str,
    storj_key: &str,
) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            "UPDATE video_index
             SET storj_key = $1, status = 'mirrored', error_message = NULL
             WHERE video_id = $2",
            &[&storj_key, &video_id],
        )
        .await?;
    Ok(())
}

pub async fn update_error(
    client: &Client,
    video_id: &str,
    error: &str,
) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            "UPDATE video_index SET error_message = $1 WHERE video_id = $2",
            &[&error, &video_id],
        )
        .await?;
    Ok(())
}

// --- Job 4: Cleanup ---

pub async fn fetch_pending_cleanup_batch(
    client: &Client,
    limit: i64,
) -> Result<Vec<CleanupRow>, tokio_postgres::Error> {
    let rows = client
        .query(
            "SELECT video_id, hetzner_key, storj_key FROM video_index
             WHERE is_temp = TRUE AND status = 'mirrored'
               AND hetzner_key IS NOT NULL AND storj_key IS NOT NULL
             LIMIT $1",
            &[&limit],
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| CleanupRow {
            video_id: r.get(0),
            hetzner_key: r.get(1),
            storj_key: r.get(2),
        })
        .collect())
}

pub async fn update_cleanup_done(
    client: &Client,
    video_id: &str,
) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            "UPDATE video_index
             SET hetzner_key = NULL, is_temp = FALSE,
                 status = 'done', error_message = NULL
             WHERE video_id = $1",
            &[&video_id],
        )
        .await?;
    Ok(())
}

// --- Audit ---

pub async fn get_audit_stats(client: &Client) -> Result<AuditStats, tokio_postgres::Error> {
    let row = client
        .query_one(
            "SELECT
                COUNT(*),
                COUNT(phash),
                COUNT(*) FILTER (WHERE status IN ('mirrored','done')),
                COUNT(*) FILTER (WHERE storj_key IS NULL AND is_temp = FALSE),
                COUNT(*) FILTER (WHERE hetzner_key IS NULL AND is_temp = FALSE AND status != 'done'),
                COUNT(*) FILTER (WHERE is_temp = TRUE AND status = 'mirrored'),
                COUNT(*) FILTER (WHERE status = 'failed'),
                COUNT(*) FILTER (WHERE error_message IS NOT NULL)
             FROM video_index",
            &[],
        )
        .await?;

    Ok(AuditStats {
        total: row.get(0),
        phash_computed: row.get(1),
        mirrored: row.get(2),
        missing_storj: row.get(3),
        missing_hetzner: row.get(4),
        cleanup_pending: row.get(5),
        failed: row.get(6),
        error_count: row.get(7),
    })
}

pub async fn get_duplicate_phashes(
    client: &Client,
) -> Result<Vec<DuplicatePhash>, tokio_postgres::Error> {
    let rows = client
        .query(
            "SELECT phash, array_agg(video_id) FROM video_index
             WHERE phash IS NOT NULL
             GROUP BY phash HAVING COUNT(*) > 1
             LIMIT 100",
            &[],
        )
        .await?;

    Ok(rows
        .into_iter()
        .map(|r| DuplicatePhash {
            phash: r.get(0),
            video_ids: r.get(1),
        })
        .collect())
}
```

- [ ] **Step 4: Declare module in lib.rs and main.rs**

In `src/lib.rs`:
```rust
pub mod db;
```
In `src/main.rs`:
```rust
mod db;
```

- [ ] **Step 5: Run the DB tests**

```bash
cargo test -p storj-interface db::tests:: -- --test-threads=1
```

Expected: all 5 tests PASS (requires docker).

- [ ] **Step 6: Commit**

```bash
git add src/db.rs src/lib.rs src/main.rs
git commit -m "feat: add db layer with schema init and all job queries"
```

---

### Task 8: Update AppState and main.rs

Add `CancellationToken` and `db_url` to state. Wire DB init at startup. Wire cancel to shutdown signal.

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1: Add imports at top of main.rs**

```rust
use tokio_util::sync::CancellationToken;
use consts::{
    DATABASE_URL, /* existing consts */ ...
};
```

- [ ] **Step 2: Define shared AppState struct**

In `main.rs`, add (or update if already defined inline):
```rust
#[derive(Clone)]
pub(crate) struct AppState {
    pub s3_client: s3_client::S3Client,
    pub storj_client: storj_s3_client::StorjS3Client,
    pub db_url: String,
    pub cancel: CancellationToken,
}
```

- [ ] **Step 3: Update run_server() to init DB and build AppState**

In `run_server()`, after forcing lazy consts, add:

```rust
// Force new lazy consts
Lazy::force(&consts::DATABASE_URL);
Lazy::force(&consts::STORJ_GATEWAY_ACCESS_KEY);
Lazy::force(&consts::STORJ_GATEWAY_SECRET_KEY);

// Init DB schema at startup
let db_client = db::connect(DATABASE_URL.as_str()).await
    .context("Failed to connect to postgres")?;
db::init_schema(&db_client).await
    .context("Failed to init DB schema")?;
drop(db_client); // jobs create their own connections

let storj_client = storj_s3_client::StorjS3Client::new().await;
let cancel = CancellationToken::new();

let app_state = AppState {
    s3_client: s3_client.clone(),
    storj_client,
    db_url: DATABASE_URL.clone(),
    cancel: cancel.clone(),
};
```

- [ ] **Step 4: Update shutdown signal handler to cancel token**

Replace existing Notify-based handler:
```rust
let cancel_clone = cancel.clone();
tokio::spawn(async move {
    if let Err(err) = signal::ctrl_c().await {
        tracing::error!("Failed to listen for shutdown signal: {err:#}");
    }
    cancel_clone.cancel();
    notify_clone.notify_one();
});
```

- [ ] **Step 5: Pass AppState to existing routes**

Update each route's `.with_state(s3_client.clone())` to `.with_state(app_state.clone())`. This requires updating `duplicate.rs`, `duplicate_hls.rs`, `move2nsfw.rs` handlers to accept `State(AppState)` instead of `State(S3Client)` and use `state.s3_client`.

> **Important:** This is the largest mechanical change. Go file by file:
> - `src/routes/duplicate.rs`: change `State(s3_client): State<S3Client>` → `State(state): State<AppState>`, use `state.s3_client`
> - `src/routes/duplicate_hls.rs`: same
> - `src/routes/move2nsfw.rs`: same

- [ ] **Step 6: Verify build**

```bash
cargo check -p storj-interface --message-format short 2>&1 | grep "^error"
```

Expected: 0 errors.

- [ ] **Step 7: Run tests**

```bash
cargo test -p storj-interface 2>&1 | tail -10
```

Expected: all pass.

- [ ] **Step 8: Commit**

```bash
git add src/main.rs src/routes/duplicate.rs src/routes/duplicate_hls.rs src/routes/move2nsfw.rs
git commit -m "feat: add AppState with CancellationToken and DB init at startup"
```

---

### Task 9: Shared job helpers (jobs/mod.rs)

Key parsing functions, uplink_cp helper, progress logger. Unit test the key parsing.

**Files:**
- Create: `src/jobs/mod.rs`

- [ ] **Step 1: Write failing unit tests**

```rust
// src/jobs/mod.rs tests
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_id_from_mp4_key_strips_suffix() {
        assert_eq!(
            video_id_from_key("user123/abc.mp4"),
            Some("user123/abc".to_string())
        );
    }

    #[test]
    fn video_id_from_key_rejects_non_mp4() {
        assert_eq!(video_id_from_key("user123/abc.mov"), None);
        assert_eq!(video_id_from_key("user123/abc_thumbnail.png"), None);
        assert_eq!(video_id_from_key("user123/abc-thumbnail.png"), None);
    }

    #[test]
    fn video_id_from_key_handles_nested_paths() {
        assert_eq!(
            video_id_from_key("a/b/c/video.mp4"),
            Some("a/b/c/video".to_string())
        );
    }

    #[test]
    fn thumb_key_from_mp4_key_uses_strip_suffix() {
        assert_eq!(
            thumb_key_from_mp4_key("user/abc.mp4"),
            Some("user/abc-thumbnail.png".to_string())
        );
    }

    #[test]
    fn thumb_key_does_not_mutate_folder_with_mp4_in_name() {
        // folder named "video.mp4.bak" should not be touched
        assert_eq!(
            thumb_key_from_mp4_key("video.mp4.bak/file.mp4"),
            Some("video.mp4.bak/file-thumbnail.png".to_string())
        );
    }
}
```

- [ ] **Step 2: Run — verify fail**

```bash
cargo test jobs::tests 2>&1 | tail -5
```

Expected: compile error.

- [ ] **Step 3: Implement jobs/mod.rs**

```rust
// src/jobs/mod.rs
pub mod cleanup;
pub mod mirror;
pub mod phash_backfill;
pub mod scan_hetzner;
pub mod scan_storj;

use std::path::Path;
use tokio::process::Command;
use anyhow::{Context, Result};

/// Extract video_id from an S3 key — returns None if not an .mp4 file.
/// video_id = full path without .mp4 extension, e.g. "user123/abc"
pub fn video_id_from_key(key: &str) -> Option<String> {
    key.strip_suffix(".mp4").map(|s| s.to_string())
}

/// Derive thumbnail key from mp4 key using strip_suffix (safe for paths with ".mp4" in dirs).
pub fn thumb_key_from_mp4_key(key: &str) -> Option<String> {
    key.strip_suffix(".mp4")
        .map(|stem| format!("{stem}-thumbnail.png"))
}

/// Upload a local file to Storj via uplink CLI.
pub async fn uplink_cp(src: &Path, dst: &str, access_grant: &str) -> Result<()> {
    let output = Command::new("uplink")
        .args([
            "cp",
            "--interactive=false",
            "--analytics=false",
            "--progress=false",
            "--access",
            access_grant,
            src.to_str().context("non-UTF8 path")?,
            dst,
        ])
        .output()
        .await
        .context("failed to spawn uplink")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("uplink cp to {dst} failed: {stderr}");
    }
    Ok(())
}

/// Log progress every N items using tracing::info.
pub fn log_progress(processed: usize, label: &str) {
    if processed % 1000 == 0 && processed > 0 {
        tracing::info!(processed, "{label}: processed {processed} items");
    }
}
```

- [ ] **Step 4: Declare jobs module in main.rs**

```rust
// src/main.rs
mod jobs;
```

And in `src/lib.rs`:
```rust
pub mod jobs;
```

- [ ] **Step 5: Run unit tests**

```bash
cargo test jobs::tests
```

Expected: all 5 tests PASS.

- [ ] **Step 6: Commit**

```bash
git add src/jobs/mod.rs src/main.rs src/lib.rs
git commit -m "feat: add jobs module with key helpers, uplink_cp, and unit tests"
```

---

### Task 10: Job 0 — Scan Storj

**Files:**
- Create: `src/jobs/scan_storj.rs`

- [ ] **Step 1: Implement**

```rust
// src/jobs/scan_storj.rs
use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::db;
use crate::jobs::video_id_from_key;
use crate::storj_s3_client::StorjS3Client;

pub async fn run(storj: StorjS3Client, db_url: String, cancel: CancellationToken) -> Result<()> {
    tracing::info!("Job 0 (scan-storj): starting");
    let client = db::connect(&db_url).await?;
    let mut total = 0usize;
    let mut continuation: Option<String> = None;

    loop {
        if cancel.is_cancelled() {
            tracing::info!("Job 0 (scan-storj): cancelled at {total} objects");
            return Ok(());
        }

        // list_objects returns a page; for pagination we need the raw continuation token
        // Use the underlying S3 list_objects_v2 with continuation support
        // StorjS3Client.list_objects() handles pagination internally and returns all objects
        // For large buckets, we page manually to check cancellation between pages.
        // If StorjS3Client.list_objects returns all at once, this is one batch.
        // Refactor to paginate if needed — for now use list_objects which paginates internally.
        let objects = storj.list_objects(None).await
            .map_err(|e| anyhow::anyhow!("Storj list failed: {e}"))?;

        for obj in &objects {
            let Some(video_id) = video_id_from_key(&obj.key) else {
                continue; // skip thumbnails and non-mp4
            };

            db::upsert_storj_key(&client, &video_id, &obj.key).await
                .map_err(|e| anyhow::anyhow!("DB upsert failed for {}: {e}", obj.key))?;

            total += 1;
            crate::jobs::log_progress(total, "scan-storj");
        }

        // list_objects in S3Client paginates internally — returns all objects at once.
        // If future performance requires chunked page-by-page, refactor S3Client.list_objects
        // to accept a continuation_token. For now this is complete.
        break;
    }

    tracing::info!(total, "Job 0 (scan-storj): complete");
    Ok(())
}
```

> **Note on pagination:** `S3Client::list_objects` already handles S3 pagination internally (see `src/s3_client.rs:list_objects` loop). It returns all objects in one `Vec`. For 600K objects this loads all keys into memory (~600K × ~50 bytes key = ~30MB, acceptable). If memory becomes a concern, refactor `list_objects` to accept a callback. For now this is fine.

- [ ] **Step 2: Cargo check**

```bash
cargo check -p storj-interface --message-format short 2>&1 | grep "scan_storj"
```

Expected: 0 errors.

- [ ] **Step 3: Commit**

```bash
git add src/jobs/scan_storj.rs
git commit -m "feat: add Job 0 (scan-storj) to reconcile post-rclone index"
```

---

### Task 11: Job 1 — Scan Hetzner

**Files:**
- Create: `src/jobs/scan_hetzner.rs`

- [ ] **Step 1: Implement**

```rust
// src/jobs/scan_hetzner.rs
use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::consts::TEMP_KEY_PREFIX;
use crate::db;
use crate::jobs::video_id_from_key;
use crate::s3_client::S3Client;

pub async fn run(s3: S3Client, db_url: String, cancel: CancellationToken) -> Result<()> {
    tracing::info!("Job 1 (scan-hetzner): starting");
    let client = db::connect(&db_url).await?;
    let mut total = 0usize;

    if cancel.is_cancelled() {
        return Ok(());
    }

    let objects = s3
        .list_objects(None)
        .await
        .map_err(|e| anyhow::anyhow!("Hetzner list failed: {e}"))?;

    for obj in &objects {
        // Skip thumbnails (both variants) — only index .mp4 files
        if obj.key.ends_with("_thumbnail.png") || obj.key.ends_with("-thumbnail.png") {
            continue;
        }
        let Some(video_id) = video_id_from_key(&obj.key) else {
            continue;
        };

        let is_temp = obj.key.contains(TEMP_KEY_PREFIX.as_str());

        db::upsert_hetzner_key(&client, &video_id, &obj.key, is_temp)
            .await
            .map_err(|e| anyhow::anyhow!("DB upsert failed for {}: {e}", obj.key))?;

        total += 1;
        crate::jobs::log_progress(total, "scan-hetzner");
    }

    tracing::info!(total, "Job 1 (scan-hetzner): complete");
    Ok(())
}
```

- [ ] **Step 2: Cargo check**

```bash
cargo check -p storj-interface --message-format short 2>&1 | grep "scan_hetzner"
```

Expected: 0 errors.

- [ ] **Step 3: Commit**

```bash
git add src/jobs/scan_hetzner.rs
git commit -m "feat: add Job 1 (scan-hetzner) to populate video_index from Hetzner"
```

---

### Task 12: Job 2 — Phash Backfill

**Files:**
- Create: `src/jobs/phash_backfill.rs`

- [ ] **Step 1: Implement**

```rust
// src/jobs/phash_backfill.rs
use std::sync::Arc;

use anyhow::Result;
use futures::StreamExt;
use phash::PHasher;
use tempfile::NamedTempFile;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::consts::{MAX_PHASH_RETRIES, PHASH_CONCURRENCY, SCAN_PAGE_SIZE};
use crate::db;
use crate::s3_client::S3Client;

pub async fn run(s3: S3Client, db_url: String, cancel: CancellationToken) -> Result<()> {
    tracing::info!("Job 2 (phash-backfill): starting");
    let client = db::connect(&db_url).await?;
    let semaphore = Arc::new(Semaphore::new(*PHASH_CONCURRENCY));
    let mut grand_total = 0usize;

    loop {
        if cancel.is_cancelled() {
            tracing::info!("Job 2 (phash-backfill): cancelled at {grand_total} videos");
            return Ok(());
        }

        let rows = db::fetch_pending_phash_batch(&client, *SCAN_PAGE_SIZE).await?;
        if rows.is_empty() {
            break;
        }

        // Process up to PHASH_CONCURRENCY simultaneously using buffer_unordered.
        // NamedTempFile is created in the async closure (not moved into spawn_blocking)
        // so it is dropped in the async context after spawn_blocking returns.
        let results: Vec<(String, Result<String>)> = futures::stream::iter(rows)
            .map(|row| {
                let s3 = s3.clone();
                let sem = semaphore.clone();
                async move {
                    let _permit = sem.acquire().await.unwrap();

                    // Create tempfile in async scope
                    let mut tmp = NamedTempFile::new()
                        .map_err(|e| anyhow::anyhow!("tempfile: {e}"))?;
                    {
                        let f = tokio::fs::File::from_std(
                            tmp.as_file().try_clone()
                                .map_err(|e| anyhow::anyhow!("file clone: {e}"))?,
                        );
                        let mut f = f;
                        s3.download_to_file(&row.hetzner_key, &mut f)
                            .await
                            .map_err(|e| anyhow::anyhow!("download {}: {e}", row.hetzner_key))?;
                    }

                    // Pass path only (not file handle) into blocking thread
                    let path = tmp.path().to_owned();
                    let phash_result = tokio::task::spawn_blocking(move || {
                        PHasher::new().compute_hash(&path)
                    })
                    .await
                    .map_err(|e| anyhow::anyhow!("spawn_blocking panic: {e}"))
                    .and_then(|r| r.map_err(|e| anyhow::anyhow!("phash: {e}")));

                    // Drop (delete) tempfile before returning — always runs
                    drop(tmp);

                    (row.video_id, phash_result)
                }
            })
            .buffer_unordered(*PHASH_CONCURRENCY)
            .collect()
            .await;

        // Sequential DB writes after all parallel work
        for (video_id, result) in results {
            match result {
                Ok(phash) => {
                    db::update_phash_success(&client, &video_id, &phash).await?;
                }
                Err(e) => {
                    tracing::error!(video_id = %video_id, error = %e, "phash failed");
                    sentry::capture_message(&format!("phash failed for {video_id}: {e}"),
                        sentry::Level::Error);
                    let retries = db::update_phash_failure(
                        &client, &video_id, &e.to_string(), *MAX_PHASH_RETRIES,
                    ).await?;
                    tracing::warn!(video_id = %video_id, retries, "phash retry scheduled");
                }
            }
            grand_total += 1;
            crate::jobs::log_progress(grand_total, "phash-backfill");
        }
    }

    tracing::info!(grand_total, "Job 2 (phash-backfill): complete");
    Ok(())
}
```

- [ ] **Step 2: Add `phash` to main crate dependencies if not already done**

Verify `Cargo.toml` has `phash = { path = "crates/phash" }`.

- [ ] **Step 3: Cargo check**

```bash
cargo check -p storj-interface --message-format short 2>&1 | grep "phash_backfill"
```

Expected: 0 errors.

- [ ] **Step 4: Commit**

```bash
git add src/jobs/phash_backfill.rs
git commit -m "feat: add Job 2 (phash-backfill) with buffer_unordered + NamedTempFile"
```

---

### Task 13: Job 3 — Mirror Incremental

**Files:**
- Create: `src/jobs/mirror.rs`

- [ ] **Step 1: Implement**

```rust
// src/jobs/mirror.rs
use std::sync::Arc;

use anyhow::Result;
use futures::StreamExt;
use tempfile::NamedTempFile;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::consts::{ACCESS_GRANT_SFW, MIRROR_CONCURRENCY, SCAN_PAGE_SIZE, STORJ_SFW_BUCKET};
use crate::db;
use crate::jobs::{thumb_key_from_mp4_key, uplink_cp};
use crate::s3_client::S3Client;

pub async fn run(s3: S3Client, db_url: String, cancel: CancellationToken) -> Result<()> {
    tracing::info!("Job 3 (mirror): starting");
    let client = db::connect(&db_url).await?;
    let semaphore = Arc::new(Semaphore::new(*MIRROR_CONCURRENCY));
    let mut grand_total = 0usize;

    loop {
        if cancel.is_cancelled() {
            tracing::info!("Job 3 (mirror): cancelled at {grand_total} videos");
            return Ok(());
        }

        let rows = db::fetch_pending_mirror_batch(&client, *SCAN_PAGE_SIZE).await?;
        if rows.is_empty() {
            break;
        }

        let results: Vec<(String, String, Result<()>)> = futures::stream::iter(rows)
            .map(|row| {
                let s3 = s3.clone();
                let sem = semaphore.clone();
                let bucket = STORJ_SFW_BUCKET.clone();
                let grant = ACCESS_GRANT_SFW.clone();
                async move {
                    let _permit = sem.acquire().await.unwrap();

                    let result = mirror_one(&s3, &row.hetzner_key, &bucket, &grant).await;
                    (row.video_id, row.hetzner_key, result)
                }
            })
            .buffer_unordered(*MIRROR_CONCURRENCY)
            .collect()
            .await;

        // Sequential DB writes
        for (video_id, hetzner_key, result) in results {
            match result {
                Ok(()) => {
                    db::update_mirror_success(&client, &video_id, &hetzner_key).await?;
                }
                Err(e) => {
                    tracing::error!(video_id = %video_id, error = %e, "mirror failed");
                    sentry::capture_message(&format!("mirror failed for {video_id}: {e}"),
                        sentry::Level::Error);
                    db::update_error(&client, &video_id, &e.to_string()).await?;
                }
            }
            grand_total += 1;
            crate::jobs::log_progress(grand_total, "mirror");
        }
    }

    tracing::info!(grand_total, "Job 3 (mirror): complete");
    Ok(())
}

async fn mirror_one(s3: &S3Client, hetzner_key: &str, bucket: &str, grant: &str) -> Result<()> {
    // 1. Copy MP4
    let mut tmp_mp4 = NamedTempFile::new()?;
    {
        let mut f = tokio::fs::File::from_std(tmp_mp4.as_file().try_clone()?);
        s3.download_to_file(hetzner_key, &mut f)
            .await
            .map_err(|e| anyhow::anyhow!("download mp4 {hetzner_key}: {e}"))?;
    }
    uplink_cp(tmp_mp4.path(), &format!("sj://{bucket}/{hetzner_key}"), grant).await?;
    drop(tmp_mp4);

    // 2. Copy thumbnail (best-effort: warn if absent, error if S3 check fails)
    if let Some(thumb_key) = thumb_key_from_mp4_key(hetzner_key) {
        match s3.object_exists(&thumb_key).await {
            Ok(true) => {
                let mut tmp_thumb = NamedTempFile::new()?;
                {
                    let mut f = tokio::fs::File::from_std(tmp_thumb.as_file().try_clone()?);
                    s3.download_to_file(&thumb_key, &mut f)
                        .await
                        .map_err(|e| anyhow::anyhow!("download thumb {thumb_key}: {e}"))?;
                }
                uplink_cp(tmp_thumb.path(), &format!("sj://{bucket}/{thumb_key}"), grant).await?;
                drop(tmp_thumb);
            }
            Ok(false) => {
                tracing::warn!(hetzner_key, "thumbnail missing on Hetzner — mirroring MP4 only");
            }
            Err(e) => {
                // Transient S3 error checking thumbnail — abort this video, retry next run
                anyhow::bail!("S3 check for thumbnail {thumb_key}: {e}");
            }
        }
    }

    Ok(())
}
```

- [ ] **Step 2: Cargo check**

```bash
cargo check -p storj-interface --message-format short 2>&1 | grep "mirror"
```

Expected: 0 errors.

- [ ] **Step 3: Commit**

```bash
git add src/jobs/mirror.rs
git commit -m "feat: add Job 3 (mirror) for incremental Hetzner→Storj copy"
```

---

### Task 14: Job 4 — Temp Cleanup

**Files:**
- Create: `src/jobs/cleanup.rs`

- [ ] **Step 1: Implement**

```rust
// src/jobs/cleanup.rs
use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::consts::SCAN_PAGE_SIZE;
use crate::db;
use crate::s3_client::S3Client;
use crate::storj_s3_client::StorjS3Client;

pub async fn run(
    s3: S3Client,
    storj: StorjS3Client,
    db_url: String,
    cancel: CancellationToken,
) -> Result<()> {
    tracing::info!("Job 4 (cleanup): starting");
    let client = db::connect(&db_url).await?;
    let mut total = 0usize;

    loop {
        if cancel.is_cancelled() {
            tracing::info!("Job 4 (cleanup): cancelled at {total} rows");
            return Ok(());
        }

        let rows = db::fetch_pending_cleanup_batch(&client, *SCAN_PAGE_SIZE).await?;
        if rows.is_empty() {
            break;
        }

        // Intentionally serial — deletion is irreversible
        for row in rows {
            match storj.object_exists(&row.storj_key).await {
                Err(e) => {
                    tracing::error!(
                        video_id = %row.video_id,
                        storj_key = %row.storj_key,
                        error = %e,
                        "transient error checking Storj — skipping delete"
                    );
                    sentry::capture_message(
                        &format!("cleanup storj check failed for {}: {e}", row.video_id),
                        sentry::Level::Error,
                    );
                    continue;
                }
                Ok(false) => {
                    tracing::error!(
                        video_id = %row.video_id,
                        storj_key = %row.storj_key,
                        "CRITICAL: Storj copy missing before cleanup — data loss risk, skipping"
                    );
                    sentry::capture_message(
                        &format!("CRITICAL: storj copy missing for {} before delete", row.video_id),
                        sentry::Level::Fatal,
                    );
                    continue;
                }
                Ok(true) => {}
            }

            if let Err(e) = s3.delete_video(&row.hetzner_key).await {
                tracing::error!(
                    video_id = %row.video_id,
                    hetzner_key = %row.hetzner_key,
                    error = %e,
                    "failed to delete from Hetzner"
                );
                sentry::capture_message(
                    &format!("cleanup delete failed for {}: {e:?}", row.video_id),
                    sentry::Level::Error,
                );
                continue;
            }

            db::update_cleanup_done(&client, &row.video_id).await?;
            total += 1;
            crate::jobs::log_progress(total, "cleanup");
        }
    }

    tracing::info!(total, "Job 4 (cleanup): complete");
    Ok(())
}
```

- [ ] **Step 2: Cargo check**

```bash
cargo check -p storj-interface --message-format short 2>&1 | grep "cleanup"
```

Expected: 0 errors.

- [ ] **Step 3: Commit**

```bash
git add src/jobs/cleanup.rs
git commit -m "feat: add Job 4 (cleanup) for safe temp file deletion with Storj verify"
```

---

### Task 15: HTTP routes — mirror endpoints + audit

**Files:**
- Create: `src/routes/mirror.rs`
- Modify: `src/routes/mod.rs`
- Modify: `src/main.rs` (wire routes)

- [ ] **Step 1: Create routes/mirror.rs**

```rust
// src/routes/mirror.rs
use axum::{extract::State, http::StatusCode, Json};
use serde::Serialize;

use crate::db;
use crate::jobs;
use crate::main::AppState; // adjust import path if AppState is defined in lib.rs

#[derive(Serialize)]
pub struct AuditResponse {
    pub total: i64,
    pub phash_computed: i64,
    pub mirrored: i64,
    pub missing_storj: i64,
    pub missing_hetzner: i64,
    pub cleanup_pending: i64,
    pub failed: i64,
    pub error_count: i64,
    pub duplicate_phashes: Vec<DuplicateEntry>,
}

#[derive(Serialize)]
pub struct DuplicateEntry {
    pub phash: String,
    pub video_ids: Vec<String>,
}

pub async fn scan_storj(State(state): State<AppState>) -> StatusCode {
    let storj = state.storj_client.clone();
    let db_url = state.db_url.clone();
    let cancel = state.cancel.clone();
    tokio::spawn(async move {
        if let Err(e) = jobs::scan_storj::run(storj, db_url, cancel).await {
            tracing::error!(error = %e, "Job 0 (scan-storj) error");
            sentry::capture_message(&format!("scan-storj job failed: {e}"), sentry::Level::Error);
        }
    });
    StatusCode::ACCEPTED
}

pub async fn scan_hetzner(State(state): State<AppState>) -> StatusCode {
    let s3 = state.s3_client.clone();
    let db_url = state.db_url.clone();
    let cancel = state.cancel.clone();
    tokio::spawn(async move {
        if let Err(e) = jobs::scan_hetzner::run(s3, db_url, cancel).await {
            tracing::error!(error = %e, "Job 1 (scan-hetzner) error");
            sentry::capture_message(&format!("scan-hetzner job failed: {e}"), sentry::Level::Error);
        }
    });
    StatusCode::ACCEPTED
}

pub async fn phash_backfill(State(state): State<AppState>) -> StatusCode {
    let s3 = state.s3_client.clone();
    let db_url = state.db_url.clone();
    let cancel = state.cancel.clone();
    tokio::spawn(async move {
        if let Err(e) = jobs::phash_backfill::run(s3, db_url, cancel).await {
            tracing::error!(error = %e, "Job 2 (phash-backfill) error");
            sentry::capture_message(&format!("phash job failed: {e}"), sentry::Level::Error);
        }
    });
    StatusCode::ACCEPTED
}

pub async fn mirror(State(state): State<AppState>) -> StatusCode {
    let s3 = state.s3_client.clone();
    let db_url = state.db_url.clone();
    let cancel = state.cancel.clone();
    tokio::spawn(async move {
        if let Err(e) = jobs::mirror::run(s3, db_url, cancel).await {
            tracing::error!(error = %e, "Job 3 (mirror) error");
            sentry::capture_message(&format!("mirror job failed: {e}"), sentry::Level::Error);
        }
    });
    StatusCode::ACCEPTED
}

pub async fn cleanup(State(state): State<AppState>) -> StatusCode {
    let s3 = state.s3_client.clone();
    let storj = state.storj_client.clone();
    let db_url = state.db_url.clone();
    let cancel = state.cancel.clone();
    tokio::spawn(async move {
        if let Err(e) = jobs::cleanup::run(s3, storj, db_url, cancel).await {
            tracing::error!(error = %e, "Job 4 (cleanup) error");
            sentry::capture_message(&format!("cleanup job failed: {e}"), sentry::Level::Error);
        }
    });
    StatusCode::ACCEPTED
}

pub async fn audit(State(state): State<AppState>) -> Result<Json<AuditResponse>, StatusCode> {
    let client = db::connect(&state.db_url)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let stats = db::get_audit_stats(&client)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let dups = db::get_duplicate_phashes(&client)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(AuditResponse {
        total: stats.total,
        phash_computed: stats.phash_computed,
        mirrored: stats.mirrored,
        missing_storj: stats.missing_storj,
        missing_hetzner: stats.missing_hetzner,
        cleanup_pending: stats.cleanup_pending,
        failed: stats.failed,
        error_count: stats.error_count,
        duplicate_phashes: dups
            .into_iter()
            .map(|d| DuplicateEntry {
                phash: d.phash,
                video_ids: d.video_ids,
            })
            .collect(),
    }))
}
```

- [ ] **Step 2: Add to routes/mod.rs**

```rust
// src/routes/mod.rs
pub mod duplicate;
pub mod duplicate_hls;
pub mod mirror;
pub mod move2nsfw;
```

- [ ] **Step 3: Wire routes in main.rs**

In `run_server()`, add to the router:

```rust
.route("/mirror/jobs/scan-storj",  post(routes::mirror::scan_storj))
.route("/mirror/jobs/scan-hetzner", post(routes::mirror::scan_hetzner))
.route("/mirror/jobs/phash",       post(routes::mirror::phash_backfill))
.route("/mirror/jobs/mirror",      post(routes::mirror::mirror))
.route("/mirror/jobs/cleanup",     post(routes::mirror::cleanup))
.route("/mirror/audit",            get(routes::mirror::audit))
```

All mirror routes must go through the `authorize` middleware:
```rust
.route("/mirror/jobs/scan-storj",
    post(routes::mirror::scan_storj)
        .with_state(app_state.clone())
        .layer(middleware::from_fn(authorize)))
// ... repeat for all 5 POST routes and GET audit
```

- [ ] **Step 4: Full cargo check**

```bash
cargo check -p storj-interface --message-format short 2>&1 | grep "^error"
```

Expected: 0 errors.

- [ ] **Step 5: Run all tests**

```bash
cargo test -p storj-interface 2>&1 | tail -20
```

Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add src/routes/mirror.rs src/routes/mod.rs src/main.rs
git commit -m "feat: add mirror job HTTP endpoints and audit route"
```

---

### Task 16: Docker Compose — add Postgres

**Files:**
- Modify: `deploy/docker-compose.yml`
- Modify: `.env.example` (or create if absent)

- [ ] **Step 1: Update docker-compose.yml**

Add `postgres` service and update `storj-interface` with `depends_on` and `DATABASE_URL`:

```yaml
services:
  storj-interface:
    image: ${APP_IMAGE:-ghcr.io/dolr-ai/storj-interface}:${IMAGE_TAG:-latest}
    restart: unless-stopped
    depends_on:
      postgres:
        condition: service_healthy
    environment:
      # --- existing vars ---
      STORJ_ACCESS_GRANT_SFW: ${STORJ_ACCESS_GRANT_SFW}
      SFW_BUCKET: ${SFW_BUCKET:-yral-videos}
      STORJ_ACCESS_GRANT_NSFW: ${STORJ_ACCESS_GRANT_NSFW}
      NSFW_BUCKET: ${NSFW_BUCKET:-yral-nsfw-videos}
      HETZNER_S3_ENDPOINT: ${HETZNER_S3_ENDPOINT}
      HETZNER_S3_BUCKET: ${HETZNER_S3_BUCKET}
      HETZNER_S3_ACCESS_KEY: ${HETZNER_S3_ACCESS_KEY}
      HETZNER_S3_SECRET_KEY: ${HETZNER_S3_SECRET_KEY}
      HETZNER_S3_REGION: ${HETZNER_S3_REGION:-eu-central}
      SERVICE_SECRET_TOKEN: ${SERVICE_SECRET_TOKEN}
      ENVIRONMENT: ${ENVIRONMENT:-production}
      # --- new vars ---
      DATABASE_URL: postgres://storj:${POSTGRES_PASSWORD}@postgres:5432/mirror_index
      STORJ_GATEWAY_ACCESS_KEY: ${STORJ_GATEWAY_ACCESS_KEY}
      STORJ_GATEWAY_SECRET_KEY: ${STORJ_GATEWAY_SECRET_KEY}
      STORJ_SFW_BUCKET: ${STORJ_SFW_BUCKET:-yral-sfw}
      PHASH_CONCURRENCY: ${PHASH_CONCURRENCY:-4}
      MIRROR_CONCURRENCY: ${MIRROR_CONCURRENCY:-8}
    networks:
      - storj-net

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

  caddy:
    # unchanged
    ...

networks:
  storj-net:
    driver: bridge

volumes:
  caddy_data:
  caddy_config:
  postgres_data:    # new
```

- [ ] **Step 2: Update .env.example with new variables**

Add to `.env.example` (create if absent):
```bash
# Mirror index — new vars
POSTGRES_PASSWORD=change-me-in-production
STORJ_GATEWAY_ACCESS_KEY=  # from: uplink share --register --access=$STORJ_ACCESS_GRANT_SFW sj://yral-sfw
STORJ_GATEWAY_SECRET_KEY=
STORJ_SFW_BUCKET=yral-sfw
PHASH_CONCURRENCY=4
MIRROR_CONCURRENCY=8
SCAN_PAGE_SIZE=1000
MAX_PHASH_RETRIES=5
TEMP_KEY_PREFIX=_pending/
```

- [ ] **Step 3: Commit**

```bash
git add deploy/docker-compose.yml .env.example
git commit -m "feat: add postgres service to docker-compose with healthcheck"
```

---

### Task 17: Final verification

- [ ] **Step 1: Full cargo build**

```bash
cargo build -p storj-interface 2>&1 | tail -20
```

Expected: `Finished release [optimized]` with 0 errors. (If on macOS without libav, the phash crate may fail to link — this is expected; CI with debian runner will succeed.)

- [ ] **Step 2: Run full test suite**

```bash
cargo test -p storj-interface -- --test-threads=1 2>&1 | tail -30
```

Expected: all tests pass. DB tests require docker.

- [ ] **Step 3: Smoke test health endpoint**

```bash
# Start service locally (requires docker compose with postgres)
docker compose -f deploy/docker-compose.yml up -d postgres
DATABASE_URL=postgres://storj:test@localhost:15432/mirror_index \
  STORJ_ACCESS_GRANT_SFW=dummy \
  STORJ_ACCESS_GRANT_NSFW=dummy \
  STORJ_GATEWAY_ACCESS_KEY=dummy \
  STORJ_GATEWAY_SECRET_KEY=dummy \
  HETZNER_S3_ENDPOINT=http://localhost \
  HETZNER_S3_BUCKET=test \
  HETZNER_S3_ACCESS_KEY=dummy \
  HETZNER_S3_SECRET_KEY=dummy \
  SERVICE_SECRET_TOKEN=test \
  cargo run --bin storj-interface &

sleep 3
curl -f http://localhost:3000/health
```

Expected: `alive`

- [ ] **Step 4: Smoke test audit endpoint**

```bash
curl http://localhost:3000/mirror/audit \
    -H "Authorization: Bearer test" | jq
```

Expected: JSON with all zero counts.

- [ ] **Step 5: Final commit**

```bash
git add -A
git commit -m "feat: Storj mirror index — complete implementation"
```

- [ ] **Step 6: Push branch**

```bash
git push -u origin feat/storj-mirror-index
```

---

## Operational Checklist (post-deploy)

Before running migration jobs on production:

```bash
# 1. Generate Storj S3 gateway credentials (one-time)
uplink share --register --readonly=false \
    --access=$STORJ_ACCESS_GRANT_SFW sj://yral-sfw
# → Save output as STORJ_GATEWAY_ACCESS_KEY + STORJ_GATEWAY_SECRET_KEY in GitHub secrets

# 2. Check disk space on server
df -h /tmp   # need ≥ 10GB free

# 3. Run rclone bulk copy
rclone copy ${HETZNER_S3_BUCKET}: storj:yral-sfw \
    --exclude "*_thumbnail.png" \
    --transfers=32 --checkers=64 --progress

# 4. Trigger jobs in order
curl -X POST https://storj-interface.yral.com/mirror/jobs/scan-storj  -H "Authorization: Bearer $TOKEN"
curl -X POST https://storj-interface.yral.com/mirror/jobs/scan-hetzner -H "Authorization: Bearer $TOKEN"
curl       https://storj-interface.yral.com/mirror/audit               -H "Authorization: Bearer $TOKEN" | jq
curl -X POST https://storj-interface.yral.com/mirror/jobs/phash        -H "Authorization: Bearer $TOKEN"
curl -X POST https://storj-interface.yral.com/mirror/jobs/mirror       -H "Authorization: Bearer $TOKEN"
curl       https://storj-interface.yral.com/mirror/audit               -H "Authorization: Bearer $TOKEN" | jq
# Only when missing_storj = 0:
curl -X POST https://storj-interface.yral.com/mirror/jobs/cleanup      -H "Authorization: Bearer $TOKEN"
```
