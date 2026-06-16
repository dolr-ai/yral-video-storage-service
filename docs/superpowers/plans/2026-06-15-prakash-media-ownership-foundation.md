# Prakash Media Ownership Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the first execution slice from the Prakash media ownership master design: byte-compatible off-chain pHash behavior in this repo, Postgres source-of-truth tables for servable media identity, a safe feed-event outbox cursor, legacy `video_index` import, and operational audit surfaces. Owner key: `prakash-bhatt-yral`.

**Architecture:** Keep the migration split between reusable library compatibility and service-owned media indexing. `crates/phash` is the only Rust workspace crate for this slice and owns the off-chain-compatible hash algorithm plus metadata extraction. Media ownership logic stays in focused service modules under `src/media_index/`; no `crates/media-index` crate is created yet. Existing `video_index` and mirror paths remain intact during this slice; new tables are additive. Feed events are appended through a serialized transaction path so cursor paging cannot skip late-committing lower cursors.

**Tech Stack:** Rust 2021, Tokio, Axum, utoipa, tokio-postgres, Postgres `BIGSERIAL` outbox, Postgres transaction-scoped advisory locks, `ffmpeg-next`, `image_hasher`, existing S3/Storj clients, existing HMAC auth, `cargo test`, `cargo fmt`.

---

## Spec Review Result

The master spec is ready to execute for the first slice after the latest feed-event fix. The important constraints are clear:

- Copy off-chain pHash behavior from `../off-chain-agent/src/duplicate_video/phash.rs`; do not reimplement it from the current local timestamp-seek implementation.
- Preserve both compatibility dimensions: 640-character binary format and sequential frame-index selection with cyclic fill.
- Keep moderation, AI detection, approval, bot/country logic, Milvus near-duplicate search, personalization embeddings, BigQuery writes, Redis exact cache, and Kvrocks materialization outside this implementation.
- Store source-of-truth media identity in Postgres.
- Feed consumers must page from `media_feed_events.cursor`, not `updated_at`.
- `BIGSERIAL` alone is not commit-order safe under concurrent writers, so append feed events through a serialized write path using a transaction-scoped advisory lock.

Plan review note: the writing-plans skill recommends subagent review, but the available subagent tool requires the user to explicitly ask for delegation/parallel agents. This plan is written for direct execution unless that request is made.

---

## Phase Map

This plan implements Phase 1 only and prepares the contracts for later phases.

| Phase | Scope | Tickets |
| --- | --- | --- |
| Phase 0 | Confirm current off-chain behavior and repo baseline | Master spec prep |
| Phase 1 | pHash compatibility, media index schema, feed-event outbox, import/audit surfaces | pHash ownership migration |
| Phase 2 | Canister import and reconciliation | Future PR |
| Phase 3 | Live write integration from uploads/videogen flows | Future PR |
| Phase 4 | Downstream read-feed API and DS ingestion coordination | Related to Milvus ownership work |
| Phase 5 | DS-owned Milvus near-duplicate ingestion, not implemented here | #2023 coordination-only if relevant |
| Phase 6 | Artifact/rendition catalog | Future PR |
| Phase 7 | Post/canister reference catalog and duplicate canonicalization | Future PR |
| Phase 8 | Off-chain cutover and deletion of legacy dedup ownership | Final migration PR |

---

## Files To Touch

- `crates/phash/src/lib.rs`
- `crates/phash/tests/offchain_compat.rs`
- `crates/phash/tests/fixtures/README.md`
- `crates/phash/tests/fixtures/test_raw_video.offchain_binary_10x8_v1.txt`
- `src/media_index/mod.rs`
- `src/media_index/schema.rs`
- `src/media_index/types.rs`
- `src/media_index/feed.rs`
- `src/media_index/repo.rs`
- `src/jobs/mod.rs`
- `src/jobs/media_imports.rs`
- `src/jobs/media_phash.rs`
- `src/routes/mod.rs`
- `src/routes/media.rs`
- `src/main.rs`
- `docs/superpowers/specs/2026-06-15-prakash-media-ownership-master-design.md` only if implementation discovers a spec correction

Do not remove or rewrite existing `video_index`, mirror, or moderation code in this slice.

Do not create a new Rust workspace crate for media ownership in this slice. `crates/phash` is reusable and belongs in the workspace; `media_index` is service-owned because it depends on this service's database, jobs, routing, auth, and operational state. A future shared crate is only justified once another Rust consumer needs stable media-feed or media-identity types.

---

## Phase 0: Baseline And Reference Capture

- [ ] Run the current phash crate tests:

  ```bash
  cargo test -p phash
  ```

- [ ] Inspect the off-chain source file used as the compatibility contract:

  ```bash
  sed -n '1,260p' ../off-chain-agent/src/duplicate_video/phash.rs
  ```

- [ ] Record the off-chain public behavior in implementation notes:
  - Produces a 640-character binary string.
  - Uses ten 8x8 frame hashes.
  - Selects frames by sequential decode and target frame indexes.
  - Uses cyclic fill when fewer frames are available.
  - Extracts metadata alongside hash computation.

- [ ] Confirm no staging is required before implementation:

  ```bash
  git status --short
  ```

Expected result: only intentional working-tree changes are present; do not stage files unless explicitly asked.

---

## Phase 1A: pHash Compatibility Contract

### Test First

- [ ] Create `crates/phash/tests/offchain_compat.rs` with tests that fail against the current local implementation:

  ```rust
  use phash::{PHashError, PHasher};

  const FIXTURE_VIDEO: &str = "test/test-raw-files/test-raw-video.mp4";
  const EXPECTED_HASH: &str =
      include_str!("fixtures/test_raw_video.offchain_binary_10x8_v1.txt");

  #[test]
  fn offchain_binary_hash_contract_matches_fixture() -> Result<(), PHashError> {
      let hasher = PHasher::new();
      let actual = hasher.hash_video(FIXTURE_VIDEO)?;
      let expected = EXPECTED_HASH.trim();

      assert_eq!(actual.len(), 640);
      assert!(actual.bytes().all(|b| b == b'0' || b == b'1'));
      assert_eq!(actual, expected);
      Ok(())
  }

  #[test]
  fn metadata_is_extracted_with_hash() -> Result<(), PHashError> {
      let hasher = PHasher::new();
      let result = hasher.hash_video_with_metadata(FIXTURE_VIDEO)?;

      assert_eq!(result.hash.len(), 640);
      assert!(result.metadata.duration_seconds > 0.0);
      assert!(result.metadata.frame_count > 0);
      assert!(result.metadata.width > 0);
      assert!(result.metadata.height > 0);
      Ok(())
  }
  ```

- [ ] Create `crates/phash/tests/fixtures/README.md`:

  ```markdown
  # pHash Fixtures

  `test_raw_video.offchain_binary_10x8_v1.txt` is the golden output produced by the off-chain pHash implementation for `test/test-raw-files/test-raw-video.mp4`.

  This fixture protects the migration from changing either the output format or frame-selection behavior.
  ```

- [ ] Populate `crates/phash/tests/fixtures/test_raw_video.offchain_binary_10x8_v1.txt` using the off-chain implementation before changing local behavior.

- [ ] Run the focused test and confirm it fails before implementation:

  ```bash
  cargo test -p phash offchain_compat -- --nocapture
  ```

### Implement

- [ ] Replace the timestamp-seek local pHash behavior in `crates/phash/src/lib.rs` with the off-chain-compatible behavior.

- [ ] Preserve or add a public API shaped like this:

  ```rust
  #[derive(Debug, Clone, PartialEq)]
  pub struct VideoMetadata {
      pub duration_seconds: f64,
      pub frame_count: usize,
      pub width: u32,
      pub height: u32,
      pub fps: f64,
  }

  #[derive(Debug, Clone, PartialEq)]
  pub struct VideoHashResult {
      pub hash: String,
      pub metadata: VideoMetadata,
      pub hash_kind: &'static str,
      pub hash_version: &'static str,
  }

  pub struct PHasher;

  impl PHasher {
      pub fn new() -> Self;
      pub fn hash_video(&self, path: impl AsRef<std::path::Path>) -> Result<String, PHashError>;
      pub fn hash_video_with_metadata(
          &self,
          path: impl AsRef<std::path::Path>,
      ) -> Result<VideoHashResult, PHashError>;
  }
  ```

- [ ] Use constants for the compatibility labels:

  ```rust
  pub const HASH_KIND: &str = "phash";
  pub const HASH_VERSION: &str = "offchain_binary_10x8_v1";
  pub const EXPECTED_BINARY_HASH_LEN: usize = 640;
  ```

- [ ] Keep Milvus packing and Hamming-distance helpers out of the public API. If a Hamming helper is needed for tests, keep it private to tests.

### Verify

- [ ] Run:

  ```bash
  cargo test -p phash offchain_compat -- --nocapture
  cargo test -p phash
  ```

- [ ] Commit only if the execution session has explicit commit approval:

  ```bash
  git add crates/phash
  git commit -m "Port off-chain compatible pHash"
  ```

---

## Phase 1B: Media Index Schema And Serialized Feed Outbox

`src/media_index/` is a service module tree, not a workspace crate:

- `mod.rs` exports the service-facing API.
- `schema.rs` owns additive table creation and schema tests.
- `types.rs` owns Rust structs/enums for media rows, hash rows, job runs, feed events, and query inputs.
- `feed.rs` owns serialized outbox append/read helpers and the advisory-lock rule.
- `repo.rs` owns repository operations over `all_servable_videos_on_yral`, `servable_video_sources`, `servable_video_hashes`, and exact duplicate lookup.

### Test First

- [ ] Create `src/media_index/schema.rs`, `src/media_index/feed.rs`, and `src/media_index/repo.rs` with tests that follow the repo's existing Postgres test style.

- [ ] Add tests for:
  - Schema initialization creates all first-slice tables.
  - `append_feed_event_txn` requires a transaction.
  - Feed event cursor is monotonic for serialized appends.
  - Feed payload is denormalized and can be read without joining source tables.
  - Exact-duplicate lookup returns every video sharing the same `(hash_kind, hash_version, hash_value)` while allowing each video's primary-keyed hash row to persist.

Representative test shape:

```rust
#[tokio::test]
async fn feed_event_append_serializes_cursor_visible_writes() {
    let client = test_client().await;
    media_index::init_schema(&client).await.unwrap();

    let tx = client.transaction().await.unwrap();
    let cursor = media_index::append_feed_event_txn(
        &tx,
        media_index::FeedEventInput {
            event_kind: media_index::FeedEventKind::HashUpserted,
            video_id: "video-a",
            payload: serde_json::json!({
                "video_id": "video-a",
                "hash_kind": "phash",
                "hash_version": "offchain_binary_10x8_v1",
                "hash_value": "0101"
            }),
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    assert!(cursor > 0);
}
```

### Implement

- [ ] Add `src/media_index/mod.rs`:

  ```rust
  pub mod feed;
  pub mod repo;
  pub mod schema;
  pub mod types;

  pub use feed::{append_feed_event_txn, list_feed_events_after};
  pub use repo::{find_exact_duplicates, upsert_hash_record, upsert_servable_video};
  pub use schema::init_schema;
  pub use types::*;
  ```

- [ ] Add `src/media_index/schema.rs` with `init_schema(client: &tokio_postgres::Client)` and the following additive tables:

  ```sql
  CREATE TABLE IF NOT EXISTS all_servable_videos_on_yral (
      video_id TEXT PRIMARY KEY,
      publisher_user_id TEXT,
      post_id TEXT,
      source_kind TEXT NOT NULL,
      source_ref TEXT,
      servable_status TEXT NOT NULL,
      nsfw_state TEXT,
      storage_provider TEXT,
      bucket TEXT,
      object_key TEXT,
      canonical_url TEXT,
      thumbnail_key TEXT,
      duration_ms BIGINT,
      width INTEGER,
      height INTEGER,
      fps DOUBLE PRECISION,
      container TEXT,
      video_codec TEXT,
      audio_codec TEXT,
      moov_atom_front BOOLEAN,
      canonical_encoding_version TEXT,
      discovered_from TEXT NOT NULL,
      first_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
      last_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
      updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
  );

  CREATE TABLE IF NOT EXISTS servable_video_sources (
      id BIGSERIAL PRIMARY KEY,
      video_id TEXT NOT NULL REFERENCES all_servable_videos_on_yral(video_id),
      source_kind TEXT NOT NULL,
      source_ref TEXT NOT NULL,
      raw_payload JSONB,
      imported_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
      UNIQUE (video_id, source_kind, source_ref)
  );

  CREATE TABLE IF NOT EXISTS servable_video_hashes (
      video_id TEXT NOT NULL REFERENCES all_servable_videos_on_yral(video_id),
      hash_kind TEXT NOT NULL,
      hash_version TEXT NOT NULL,
      input_media_version TEXT NOT NULL,
      hash_value TEXT NOT NULL,
      hash_bit_length INTEGER NOT NULL,
      num_frames INTEGER NOT NULL,
      hash_size INTEGER NOT NULL,
      computed_from_provider TEXT,
      computed_from_bucket TEXT,
      computed_from_key TEXT,
      computed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
      metadata JSONB,
      PRIMARY KEY (video_id, hash_kind, hash_version, input_media_version)
  );

  CREATE INDEX IF NOT EXISTS idx_servable_video_hash_exact
      ON servable_video_hashes (hash_kind, hash_version, hash_value);

  CREATE TABLE IF NOT EXISTS media_feed_events (
      cursor BIGSERIAL PRIMARY KEY,
      event_kind TEXT NOT NULL,
      video_id TEXT NOT NULL REFERENCES all_servable_videos_on_yral(video_id),
      hash_kind TEXT,
      hash_version TEXT,
      input_media_version TEXT,
      payload JSONB NOT NULL,
      created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
  );

  CREATE INDEX IF NOT EXISTS idx_media_feed_events_hash_cursor
      ON media_feed_events (hash_kind, hash_version, cursor);

  CREATE INDEX IF NOT EXISTS idx_media_feed_events_video
      ON media_feed_events (video_id);

  CREATE TABLE IF NOT EXISTS media_job_runs (
      id UUID PRIMARY KEY,
      job_kind TEXT NOT NULL,
      status TEXT NOT NULL,
      requested_by TEXT NOT NULL,
      started_at TIMESTAMPTZ NOT NULL DEFAULT now(),
      finished_at TIMESTAMPTZ,
      summary JSONB NOT NULL DEFAULT '{}'::jsonb
  );

  CREATE TABLE IF NOT EXISTS media_job_failures (
      id BIGSERIAL PRIMARY KEY,
      job_run_id UUID NOT NULL REFERENCES media_job_runs(id),
      row_key TEXT NOT NULL,
      failure_kind TEXT NOT NULL,
      retry_count INTEGER NOT NULL DEFAULT 0,
      last_error TEXT NOT NULL,
      next_retry_at TIMESTAMPTZ,
      resolved_at TIMESTAMPTZ,
      created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
      updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
      UNIQUE (job_run_id, row_key, failure_kind)
  );
  ```

- [ ] Add a stable advisory lock key for feed-event append serialization:

  ```rust
  const MEDIA_FEED_EVENT_APPEND_LOCK: i64 = 904_648_332_137_142_901;
  ```

- [ ] Add `src/media_index/feed.rs` and implement all feed-visible writes through a transaction helper that runs:

  ```sql
  SELECT pg_advisory_xact_lock($1)
  ```

  before inserting into `media_feed_events`.

- [ ] Make the helper comment explicit:

  ```rust
  // BIGSERIAL is allocation-ordered, not commit-ordered. The advisory lock keeps
  // feed-visible event insertion serialized so cursor paging cannot skip a
  // late-committing lower cursor.
  ```

- [ ] Add `src/media_index/types.rs` for structs/enums shared by the module tree.

- [ ] Add `src/media_index/repo.rs` for media row upserts, hash row upserts, and exact duplicate lookup.

- [ ] Wire schema initialization in `src/main.rs` immediately after the existing `db::init_schema` call:

  ```rust
  media_index::init_schema(&db_client).await?;
  ```

- [ ] Add `mod media_index;` in `src/main.rs`.

### Verify

- [ ] Run:

  ```bash
  cargo test media_index -- --nocapture
  ```

- [ ] Commit only if the execution session has explicit commit approval:

  ```bash
  git add src/media_index src/main.rs
  git commit -m "Add media index source-of-truth schema"
  ```

---

## Phase 1C: Legacy video_index Import

### Test First

- [ ] Add tests in `src/media_index/repo.rs` or `src/jobs/media_imports.rs`:
  - Imports rows from existing `video_index` into `all_servable_videos_on_yral`.
  - Creates one `servable_video_sources` row per imported legacy row.
  - Is idempotent when run twice.
  - Does not emit duplicate feed events for unchanged rows.
  - Records per-row failures in `media_job_failures`.

### Implement

- [ ] Create `src/jobs/media_imports.rs`.

- [ ] Add job entry function:

  ```rust
  pub async fn import_current_video_index(
      client: &tokio_postgres::Client,
      requested_by: &str,
      limit: Option<i64>,
  ) -> Result<ImportSummary, ImportError>
  ```

- [ ] Read from existing `video_index` only. Do not import BigQuery in this slice.

- [ ] Map current fields conservatively:
  - `video_id` from existing canonical video id.
  - storage fields from existing S3/Storj columns where present.
  - `servable_status = 'servable'` when the current row is usable.
  - `source_kind = 'legacy_video_index'`.
  - `raw_payload` contains the raw source row for reconciliation.

- [ ] For every changed imported row, insert one `media_feed_events` row using the serialized outbox helper.

- [ ] Add `media_job_runs` lifecycle:
  - `running` at start.
  - `succeeded` when all rows import.
  - `succeeded_with_failures` when import completes with row failures.
  - `failed` only when the whole job cannot run.

- [ ] Store row-level errors in `media_job_failures`; do not hide them in `media_job_runs.summary`.

- [ ] Export the module from `src/jobs/mod.rs`.

### Verify

- [ ] Run:

  ```bash
  cargo test media_imports -- --nocapture
  cargo test media_index -- --nocapture
  ```

- [ ] Commit only if the execution session has explicit commit approval:

  ```bash
  git add src/jobs/media_imports.rs src/jobs/mod.rs src/media_index
  git commit -m "Import legacy video index into media index"
  ```

---

## Phase 1D: pHash Job Writes Source-Of-Truth Rows

### Test First

- [ ] Add tests for pHash persistence:
  - A successful compute writes `servable_video_hashes`.
  - A successful compute emits exactly one `hash_upserted` feed event.
  - Recomputing the same hash is idempotent.
  - Two videos with the same pHash both persist and are returned by exact-duplicate lookup.
  - Worker failures are captured in `media_job_failures`.

### Implement

- [ ] Create `src/jobs/media_phash.rs`.

- [ ] Build on the existing S3 download and blocking pHash pattern from `src/jobs/phash_backfill.rs`, but write to the new media tables.

- [ ] Use constants from `crates/phash` for:
  - `hash_kind = "phash"`
  - `hash_version = "offchain_binary_10x8_v1"`
  - `input_media_version = "current_stored_object_v1"` until canonical re-encoding is defined.

- [ ] Persist pHash metadata as JSONB:

  ```json
  {
    "duration_seconds": 0.0,
    "frame_count": 0,
    "width": 0,
    "height": 0,
    "fps": 0.0
  }
  ```

- [ ] Emit feed event payload as a denormalized snapshot:

  ```json
  {
    "video_id": "string",
    "servable_status": "servable",
    "storage_provider": "s3",
    "bucket": "string",
    "object_key": "string",
    "hash_kind": "phash",
    "hash_version": "offchain_binary_10x8_v1",
    "input_media_version": "current_stored_object_v1",
    "hash_value": "640-bit binary string",
    "metadata": {}
  }
  ```

- [ ] Do not call moderation APIs from this job.

- [ ] Do not call Milvus, Redis, Kvrocks, or BigQuery from this job.

- [ ] Export the module from `src/jobs/mod.rs`.

### Verify

- [ ] Run:

  ```bash
  cargo test media_phash -- --nocapture
  cargo test -p phash
  ```

- [ ] Commit only if the execution session has explicit commit approval:

  ```bash
  git add src/jobs/media_phash.rs src/jobs/mod.rs src/media_index
  git commit -m "Write pHash results to media index"
  ```

---

## Phase 1E: Operational Routes

### Test First

- [ ] Add route tests following existing route test conventions:
  - Import route requires existing auth.
  - Missing-pHash audit route returns deterministic JSON.
  - Feed read route pages by `cursor > after`.
  - Feed read route does not join source tables at request time.

### Implement

- [ ] Create `src/routes/media.rs`.

- [ ] Add routes:

  ```text
  POST /media/import/video-index
  GET  /media/audit/missing-phash
  GET  /media/feed/events?after=<cursor>&limit=<limit>
  ```

- [ ] Route behavior:
  - `POST /media/import/video-index` starts or runs the read-only legacy import job.
  - `GET /media/audit/missing-phash` returns media rows without required `phash/offchain_binary_10x8_v1/current_stored_object_v1`.
  - `GET /media/feed/events` returns ordered outbox rows with `cursor > after`, capped to a safe max limit.

- [ ] Use existing HMAC/auth middleware patterns from current protected routes.

- [ ] Register routes in `src/routes/mod.rs` and `src/main.rs`.

- [ ] Register OpenAPI paths in the existing `ApiDoc` declaration.

### Verify

- [ ] Run:

  ```bash
  cargo test routes::media -- --nocapture
  cargo test media_index -- --nocapture
  ```

- [ ] Commit only if the execution session has explicit commit approval:

  ```bash
  git add src/routes/media.rs src/routes/mod.rs src/main.rs
  git commit -m "Expose media index operations"
  ```

---

## Phase 1F: Final Verification

- [ ] Run formatting:

  ```bash
  cargo fmt
  ```

- [ ] Run focused tests:

  ```bash
  cargo test -p phash
  cargo test media_index -- --nocapture
  cargo test media_imports -- --nocapture
  cargo test media_phash -- --nocapture
  cargo test routes::media -- --nocapture
  ```

- [ ] Run broader service tests:

  ```bash
  cargo test
  ```

- [ ] Run clippy if the branch is expected to be PR-ready:

  ```bash
  cargo clippy --all-targets --all-features -- -D warnings
  ```

- [ ] Confirm working tree state:

  ```bash
  git status --short
  ```

- [ ] Update the master spec only if implementation discovers a real design mismatch.

---

## Deferred Work For Future PRs

Do not fold these into the first execution slice:

- Canister import with IC agent pagination and reconciliation.
- `media_artifacts` catalog for originals, canonical encodes, HLS, thumbnails, staged objects, mirrored objects, and deletion state.
- `media_post_refs` or canister reference catalog.
- `media_duplicate_groups` and canonical duplicate policy.
- `media_feed_consumers` checkpoint table.
- Dedicated row-level `media_hash_jobs` lease table.
- Moderation provenance tables beyond imported state.
- Milvus ingestion, vector packing, Hamming-distance search, or personalization embeddings.
- BigQuery writes.
- Kvrocks materialization.
- Redis exact-cache replacement.
- Destructive cleanup of off-chain code or stored media.

---

## Handoff Checklist

- [ ] pHash crate returns byte-compatible 640-character binary hashes.
- [ ] Golden fixture protects off-chain compatibility.
- [ ] Metadata extraction is present and tested.
- [ ] New Postgres tables initialize additively.
- [ ] Feed events use a serialized append path.
- [ ] Legacy `video_index` import is idempotent.
- [ ] pHash source-of-truth rows are written with versioned metadata.
- [ ] Missing-pHash audit is available.
- [ ] Feed read API pages by cursor.
- [ ] No moderation, Milvus, Redis, Kvrocks, or BigQuery write paths are introduced.
