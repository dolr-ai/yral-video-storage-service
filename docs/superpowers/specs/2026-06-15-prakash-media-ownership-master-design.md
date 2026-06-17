# Prakash Media Ownership Master Design

**Date:** 2026-06-15
**Owner key:** `prakash-bhatt-yral`
**Repo context:** `yral-video-storage-service`
**Primary goal:** Move pHash ownership out of off-chain and make this service the source of truth for servable-video indexing, media fingerprints, and cleanup readiness.

---

## Summary

This spec turns the adjacent media-infrastructure tickets into one phased program. The immediate center is [#2113](https://github.com/dolr-ai/yral/issues/2113), moving pHash from off-chain to a `prakash-bhatt-yral` owned service. The pHash work should not be implemented as an isolated hash helper. It should become a versioned fingerprint attached to a master index of all servable Yral videos, because the next decisions depend on that index:

- which videos are currently servable;
- which storage provider and object key serve each video;
- which pHash was computed, with which algorithm version, and from which media form;
- whether old Cloudflare, Hetzner, Storj, QStash, BigQuery, Kvrocks, or off-chain paths are still needed;
- whether future re-encoding changes invalidate or supersede old fingerprints.

The recommended path is an umbrella rollout: first make pHash parity explicit, then build the master servable-video index, then move live pHash compute into this service, then use the index to retire legacy storage and off-chain responsibilities.

---

## Ticket Map

### Core Phase Tickets

| Ticket | Title | Role in this spec |
|---|---|---|
| [#2113](https://github.com/dolr-ai/yral/issues/2113) | Refactor pHash and move it from offchain to prakash owned service | Core pHash ownership migration |
| [#2112](https://github.com/dolr-ai/yral/issues/2112) | Create a master index of all servable videos on our platform | Source-of-truth index for pHash, storage, and cleanup |
| [#2108](https://github.com/dolr-ai/yral/issues/2108) | Cleanup yral-video-storage-service | Cleanup after ownership boundary is settled |
| [#2106](https://github.com/dolr-ai/yral/issues/2106) | Cleanup off-chain-agent | Remove migrated pHash/media paths from off-chain |

### Storage and Serving Cleanup

| Ticket | Title | Role in this spec |
|---|---|---|
| [#2026](https://github.com/dolr-ai/yral/issues/2026) | Storj buckets cleanup | Use master index to prove which buckets and keys are still used |
| [#2065](https://github.com/dolr-ai/yral/issues/2065) | Confirm Cloudflare stream is not being uploaded to or delivered from | Use index and service audit to retire Cloudflare Stream safely |
| [#2066](https://github.com/dolr-ai/yral/issues/2066) | Image hosting is also on Cloudflare | Follow-up image hosting cleanup after video serving is clean |
| [#2107](https://github.com/dolr-ai/yral/issues/2107) | Cleanup yral-video-upload-service | Remove upload-service assumptions after this service owns final media index state |

### Encoding, Quality, and Observability

| Ticket | Title | Role in this spec |
|---|---|---|
| [#1992](https://github.com/dolr-ai/yral/issues/1992) | Standardize video re-encoding after upload | Defines canonical media form and `moov` atom policy |
| [#2077](https://github.com/dolr-ai/yral/issues/2077) | Add standardized re-encoding for all uploaded videos | Implementation follow-up after research and policy |
| [#2074](https://github.com/dolr-ai/yral/issues/2074) | Draft video first-frame and transition behavior | Adjacent video quality check; must not silently change pHash semantics |
| [#2104](https://github.com/dolr-ai/yral/issues/2104) | Missing Sentry detail on video generation failure | Observability required for new media jobs and callbacks |

### Product and Integration Dependencies

| Ticket | Title | Role in this spec |
|---|---|---|
| [#2134](https://github.com/dolr-ai/yral/issues/2134) | Premium account vs free account video generation | Future policy can use the same index fields for resolution, duration, and generation source |
| [#2168](https://github.com/dolr-ai/yral/issues/2168) | Mobile hits prakash endpoint | Live mobile flow must write into the same media index |

### Coordination Tickets

| Ticket | Title | Role in this spec |
|---|---|---|
| [#2114](https://github.com/dolr-ai/yral/issues/2114) | Migrate off-chain QStash endpoints to a self-hosted queue service | pHash compute currently has QStash-shaped off-chain paths |
| [#2038](https://github.com/dolr-ai/yral/issues/2038) | Audit data-science endpoints on off-chain | pHash and duplicate search overlap with data-science owned endpoints |
| [#2046](https://github.com/dolr-ai/yral/issues/2046) | Shut down `ai-video-detection-backfill` QStash usage | Related queue retirement and backfill cleanup |
| [#2037](https://github.com/dolr-ai/yral/issues/2037) | Consume events from self-hosted Kafka instead of BigQuery | Future live ingestion source for the master index |
| [#2023](https://github.com/dolr-ai/yral/issues/2023) | Self host Milvus for the vector store when implementing personalization for the home feed | DS-owned personalization vector infrastructure; coordinate only to avoid conflating personalization embeddings with pHash duplicate-detection vectors |
| [#2045](https://github.com/dolr-ai/yral/issues/2045) | Move Kvrocks to ansuman-* servers and return the kvrocks-1,2,3,4 to the common pool | DS-owned cache/materialized-data infrastructure; not the pHash source of truth |

### Excluded From This Spec

Per current planning direction, this spec does not plan the following tickets: [#2129](https://github.com/dolr-ai/yral/issues/2129), [#2142](https://github.com/dolr-ai/yral/issues/2142), [#1940](https://github.com/dolr-ai/yral/issues/1940), [#1939](https://github.com/dolr-ai/yral/issues/1939), [#1924](https://github.com/dolr-ai/yral/issues/1924), [#2033](https://github.com/dolr-ai/yral/issues/2033).

---

## Ownership Boundary

This migration separates "this repo calls a service" from "this repo owns that domain."

| Surface | This service owns? | Scope decision |
|---|---:|---|
| Master servable-video index | Yes | Source of truth for which videos are servable and where their current media lives |
| pHash computation and storage | Yes | Source of truth moves here from off-chain |
| pHash compatibility contract | Yes | Verbatim off-chain behavior first, versioned before refactor |
| Exact duplicate lookup | Yes | Postgres-backed by indexed pHash rows |
| Media/hash read feed | Yes | Cursor-paginated feed via `media_feed_events` |
| Feed consumer checkpoints | Future PR | Needed for DS/Kvrocks/Milvus replay SLAs, not first-slice blocking |
| Media artifact/rendition catalog | Future PR | Required for original/canonical/HLS/thumbnail/staged objects, not first-slice blocking |
| Post/canister reference catalog | Future PR | Required for multi-post and multi-canister references, not first-slice blocking |
| Duplicate canonicalization groups | Future PR | Required to distinguish all matching hashes from canonical originals |
| BigQuery pHash writes | No | Retire/import-only; new writes do not target BigQuery |
| Kvrocks materialization | No | Downstream/cache only, rebuildable from this service |
| Redis/Dragonfly exact-match cache | No | Optional downstream cache, not source of truth |
| Milvus pHash near-duplicate search | No | DS-owned if product still needs near-duplicate search |
| Personalization embeddings | No | DS-owned vector space; not a pHash feed consumer by default |
| Moderation policy/classification | No | Ansuman-owned policy/service boundary |
| Videogen moderation client | Client only | Existing call path may remain, but this repo does not own moderation decisions |
| Upload service | No | Integration/source evidence only |
| Destructive storage cleanup | Not in first slice | Reports first; deletion requires later approval gate |

The first implementation must avoid schema or API choices that prevent the future PRs above. It does not need to implement all of them immediately.

---

## Current-State Findings

### In This Repo

The repo already has pHash-related infrastructure:

- `crates/phash/src/lib.rs` provides `PHasher`.
- `src/jobs/phash_backfill.rs` computes hashes for rows discovered by mirror jobs.
- `src/db.rs` has `video_index` and `mirror_jobs`.
- `src/routes/mirror.rs` exposes mirror audit, pHash backfill, and duplicate pHash endpoints.

`video_index.phash` is legacy mirror compatibility storage, not the new canonical pHash table. During the transition it may carry both old local hashes and new off-chain-compatible hashes, so every value must be interpreted with its version columns:

- legacy mirror rows: `phash_kind = 'phash'`, `phash_version = 'legacy_hex_8x8_v0'`;
- new off-chain-compatible rows: `phash_kind = 'phash'`, `phash_version = 'offchain_binary_10x8_v1'`.

Duplicate grouping against `video_index` must use `(phash_kind, phash_version, phash)`. Long-term media identity must move to `servable_video_hashes`; do not treat `video_index.phash` as source of truth for the ownership migration.

As of the current foundation branch, `crates/phash` contains the off-chain-compatible `offchain_binary_10x8_v1` implementation. The old timestamp-seek/hex behavior remains only as legacy mirror compatibility where explicitly versioned; it must not be treated as the canonical pHash implementation.

The compatibility port was required because the prior local implementation had two independent incompatibilities:

- **Format gap:** local returns concatenated hex hashes, while off-chain returns a 640-character binary string.
- **Frame-selection gap:** local seeks by timestamp and hashes the first decoded frame after each seek, while off-chain sequentially decodes and selects frames by frame index, then cyclically fills short videos. Even if local changed hex to binary, it would still hash different frames for many videos.

The migration must preserve off-chain's pHash source behavior before refactoring. Reimplementing a cleaner pHash algorithm first would break exact-match cache compatibility and legacy pHash comparisons.

### In Off-Chain

Off-chain currently owns more than the raw hash algorithm:

- pHash computation from Storj and arbitrary URLs;
- video metadata extraction;
- BigQuery/Kvrocks writes for `videohash_phash`-style data;
- Redis exact-match checks;
- Milvus near-duplicate checks;
- QStash compute and bulk-compute endpoints;
- duplicate-path integration before publishing continues.

Moving pHash means deciding which of these behaviors move now, which become compatibility bridges, and which are retired.

### External Policy Dependencies

Off-chain's pHash/dedup function is co-located with moderation and approval work, but this spec does not move those domains. This repo already has a videogen moderation client that calls the Ansuman-owned moderation service for prompt/image gating; that remains a client integration, not a moderation ownership transfer.

Policy domains that stay outside this pHash migration:

- AI detection and moderation policy;
- user-upload approval;
- bot/country logic;
- NSFW classification or NSFW decisioning.

This service may store an `nsfw_state` value imported from the appropriate owner for indexing and cleanup decisions, but it must not become the moderation service as part of this pHash migration. A future PR should add moderation-state provenance if cleanup or servability starts depending on it: policy owner, policy source, policy version, state updated time, and import time.

### Existing Index Concepts

This repo already maintains the equivalent of `all_videos_uploaded_to_hetzner_and_storj_buckets` through the mirror index. Ticket [#2112](https://github.com/dolr-ai/yral/issues/2112) asks for a broader master table named `all_servable_videos_on_yral` that includes:

- all new videos uploaded to the network;
- all videos from `videos_unique_v2` with corresponding metadata;
- all user principals from canister `ivkka-7qaaa-aaaas-qbg3q-cai`;
- posts and video IDs from canister `gxhc3-pqaaa-aaaas-qbh3q-cai`.

The master table should be the durable bridge between storage reality, user-visible posts, pHash, and cleanup decisions.

---

## Design Principles

1. **One source of truth for servability.** Storage scans alone prove objects exist, but not that users can see them. Canister posts alone prove product references, but not that objects exist. The master index reconciles both.
2. **pHash is versioned data, not a transient computation.** Store the algorithm version, input media form, and metadata used to compute it.
3. **Deletes require evidence.** No bucket, key, QStash endpoint, or off-chain route is removed until the master index and traffic audit show it is unused.
4. **Backfills are resumable and idempotent.** Every import and compute job must tolerate retries without duplicate rows or status regression.
5. **Live paths and backfills share the same write model.** New mobile/upload/videogen completions should write the same tables that backfills populate.
6. **Canonical encoding must be explicit.** If pHash is computed before re-encoding today and after re-encoding tomorrow, those hashes must have different versions.

---

## Recommended Architecture

### Service Boundary

Keep the first implementation inside `yral-video-storage-service`. This repo already has storage clients, mirror jobs, ffmpeg-linked pHash code, Postgres schema initialization, Sentry/tracing, and operational routes. A separate service can be created later if pHash throughput or ownership boundaries justify it.

### Rust Crate Boundary

`crates/phash` should be the only Rust workspace crate introduced or expanded in the first slice. It is reusable because it has a clean boundary: deterministic local video fingerprinting plus metadata extraction, with no network, database, auth, routing, or job-run concerns.

Do not create a `crates/media-index` crate for the first slice. Media ownership logic is service-owned and tightly coupled to this service's Postgres schema initialization, operational jobs, S3/Storj access, auth, routes, Sentry/tracing, and `AppState`. Keep it as a focused service module tree under `src/media_index/` so the code is separated by responsibility without pretending to be a reusable library.

A future crate such as `crates/media-feed-types` or `crates/media-identity-types` is reasonable only when another Rust binary or repo needs stable shared types. Until then, keep feed payload structs internal and version the external contract through the database/API schema.

### Core Modules

| Module | Responsibility |
|---|---|
| `crates/phash` | Deterministic video pHash library plus metadata extraction. No network, no database. |
| `src/media_index/` | Service module tree for servable-video domain types, schema, repository operations, exact duplicate lookup, and feed outbox writes. Not a workspace crate in the first slice. |
| `src/jobs/media_imports` | Backfills from existing mirror index, legacy datasets, and canisters. |
| `src/jobs/media_phash` | Resumable pHash compute for master-index rows. |
| `src/routes/media` | Authenticated operational endpoints for backfills, audit, duplicate lookup, and recompute. |
| `src/media_cleanup` | Read-only cleanup reports first; destructive cleanup only after explicit later approval. |

### Data Stores

Postgres in this service should be the operational source of truth. BigQuery is being retired and should not be part of the target pHash design. Legacy BigQuery rows can be imported once if needed for historical continuity, but new pHash ownership should not write BigQuery.

Kvrocks and Milvus should be treated as downstream systems, not sources of truth for media identity. Kvrocks currently stores `offchain:*` materialized records and is being moved under `ansuman-*` ownership in [#2045](https://github.com/dolr-ai/yral/issues/2045). Milvus hosting for personalization is covered by [#2023](https://github.com/dolr-ai/yral/issues/2023), but personalization embeddings are a separate vector space from pHash duplicate-detection vectors. This service should expose the durable media/hash feed that downstream duplicate-detection consumers can consume if DS keeps pHash in Milvus.

The master index should support exact pHash duplicate queries directly in Postgres through indexed pHash columns. Redis/Kvrocks exact-match caches may exist downstream for performance, but they must be rebuildable from this service's source-of-truth data.

The canonical pHash table for the migration is `servable_video_hashes`, not the existing mirror `video_index.phash` column. The mirror column can remain as an operational bridge while old mirror jobs still exist, but canonical media identity, downstream feeds, and exact duplicate lookup should read/write the versioned rows in `servable_video_hashes`.

### Downstream Feed Contract

This service should provide a monotonic read feed for downstream consumers instead of writing directly into every downstream store.

The feed cursor must come from `media_feed_events.cursor`, not from `updated_at`, `video_id`, or the `servable_video_hashes` composite key. `updated_at` is feed data, not a cursor, because timestamp collisions and clock skew can otherwise cause skipped or duplicated rows.

`BIGSERIAL` only orders cursor allocation, not transaction commit visibility. For v1, all `media_feed_events` appends must go through one serialized outbox append path so consumers cannot observe cursor 101, advance, and later miss cursor 100 from a slower transaction. pHash computation may remain concurrent, but the final transaction that writes `servable_video_hashes` plus `media_feed_events` must acquire the outbox serialization gate before assigning the feed cursor. A future PR may replace this with a safe high-watermark or commit-ordered sequencing mechanism, but naive concurrent `BIGSERIAL` inserts plus `WHERE cursor > $last_cursor` paging are not allowed.

Feed rows are appended transactionally through `media_feed_events` whenever a feed-visible write commits:

- inserting or updating a `servable_video_hashes` row;
- changing `all_servable_videos_on_yral.servable_status`;
- changing `storage_provider`, `bucket`, `object_key`, or the canonical serving location;
- marking a video deleted or no longer servable.

For master-index changes that affect an already-hashed video, append one feed event per current hash row that downstream hash consumers may have materialized. Consumers page with `WHERE cursor > $last_cursor ORDER BY cursor LIMIT $limit`; the response cursor is the last returned `media_feed_events.cursor`.

Minimum feed fields:

- `cursor`
- `event_kind`
- `video_id`
- `hash_kind`
- `hash_version`
- `input_media_version`
- `hash_value`
- `hash_bit_length`
- `metadata`
- `servable_status`
- `storage_provider`
- `bucket`
- `object_key`
- `updated_at`

Consumers:

- DS/Milvus ingestion uses the feed for pHash duplicate-search collections only if DS keeps pHash near-duplicate lookup in Milvus.
- DS/Kvrocks migration can materialize cache keys from the feed if Kvrocks is still useful.
- Personalization Milvus collections use model embeddings and are not consumers of this pHash feed by default. They should not be conflated with the pHash duplicate-detection collection even if they share Milvus infrastructure.

---

## Proposed Data Model

### `all_servable_videos_on_yral`

One row per product-level video that can appear in the feed, profile, drafts, or future serving surfaces.

The first slice may keep one canonical serving location directly on this table to move quickly. Long term, this table should not become the only place media objects live. Original uploads, canonical MP4s, HLS renditions, thumbnails, staged images, mirrored legacy objects, and deleted artifacts should move to a future `media_artifacts` table keyed by artifact ID.

Suggested fields:

```sql
CREATE TABLE all_servable_videos_on_yral (
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
```

`source_kind` examples:

- `user_upload`
- `videogen`
- `legacy_video_unique_v2`
- `canister_post`
- `storage_scan`

`servable_status` examples:

- `candidate`
- `servable`
- `missing_storage`
- `missing_post_reference`
- `blocked`
- `deleted`

### `servable_video_sources`

Many source systems can claim the same video. Keep the evidence instead of overwriting it.

```sql
CREATE TABLE servable_video_sources (
    id BIGSERIAL PRIMARY KEY,
    video_id TEXT NOT NULL REFERENCES all_servable_videos_on_yral(video_id),
    source_kind TEXT NOT NULL,
    source_ref TEXT NOT NULL,
    raw_payload JSONB,
    imported_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (video_id, source_kind, source_ref)
);
```

### `servable_video_hashes`

Hashes are versioned because the current local pHash crate and off-chain semantics differ, and future canonical re-encoding may change the input.

```sql
CREATE TABLE servable_video_hashes (
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

CREATE INDEX idx_servable_video_hash_exact
    ON servable_video_hashes(hash_kind, hash_version, hash_value);
```

Recommended initial values:

- `hash_kind = 'phash'`
- `hash_version = 'offchain_binary_10x8_v1'` for compatibility with off-chain's 640-bit binary pHash
- `input_media_version = 'current_stored_object_v1'` until canonical re-encoding is defined

Existing `video_index.phash` rows produced by the legacy mirror implementation must be tagged with `phash_kind = 'phash'` and `phash_version = 'legacy_hex_8x8_v0'`. Keep those rows out of canonical exact-duplicate comparisons with `offchain_binary_10x8_v1`.

### `media_feed_events`

The downstream feed is a transactional outbox over the master index and hash tables. It exists because the feed fields span multiple independently updated tables, and neither table's primary key nor `updated_at` is a safe monotonic cursor.

```sql
CREATE TABLE media_feed_events (
    cursor BIGSERIAL PRIMARY KEY,
    event_kind TEXT NOT NULL,
    video_id TEXT NOT NULL REFERENCES all_servable_videos_on_yral(video_id),
    hash_kind TEXT,
    hash_version TEXT,
    input_media_version TEXT,
    payload JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_media_feed_events_hash_cursor
    ON media_feed_events(hash_kind, hash_version, cursor);

CREATE INDEX idx_media_feed_events_video
    ON media_feed_events(video_id);
```

`payload` should contain the denormalized feed row at the time of the event: hash fields from `servable_video_hashes`, servability and storage fields from `all_servable_videos_on_yral`, and any tombstone/status data a downstream consumer needs to remove or demote a materialized record. Expected `event_kind` values include `hash_upserted`, `media_visibility_changed`, `storage_location_changed`, and `media_deleted`.

Events must be appended in the same database transaction as the source write. Consumers should treat the feed as at-least-once, use `cursor` as the replay/event id, and use `(video_id, hash_kind, hash_version, input_media_version)` as the materialized record key.

For v1 implementation, the same transaction must acquire a single outbox serialization lock before inserting into `media_feed_events`. In Postgres this can be a transaction-scoped advisory lock with a stable key reserved for media feed appends. The lock is held only around the source write plus outbox insert transaction, not around video download or pHash computation.

Repository helpers for feed-visible writes should be transaction-first. A caller that inserts or updates `all_servable_videos_on_yral` or `servable_video_hashes` must be able to perform the source write and append the feed event through the same transaction without duplicating SQL. Raw `&Client` write helpers are acceptable only for non-feed setup or tests; the production path should use `&Transaction` helpers or eventful wrappers.

Media and hash upserts must return whether the row was inserted, materially changed, or unchanged. Media upserts must also report whether a new source-evidence row was inserted so imports can account for newly discovered sources even when the canonical media row is unchanged. Feed events are emitted for inserted/materially changed canonical rows only. Recomputing the same hash and metadata must not advance `computed_at` or produce a duplicate `hash_upserted` event.

Master-index upserts must define merge semantics for sparse imports. A source row that lacks optional storage or metadata should not erase a previously discovered non-null canonical value unless the caller explicitly marks the field as cleared, deleted, or superseded. This keeps legacy imports, storage scans, upload callbacks, and videogen callbacks from clobbering each other while the index is still accumulating evidence.

Feed readers must not silently coerce unknown `event_kind` values into a known kind. Unknown event kinds should either fail parsing or be returned as an unknown/raw variant so consumers do not misclassify feed data.

Schema initialization must be safe for rolling deploys with multiple service instances starting at once. Trigger/function setup for `media_feed_events` should be serialized with a stable schema-init advisory lock or moved to a real migration runner before production deployment. While this branch is pre-deploy, the additive startup schema should still include `ALTER TABLE ... ADD COLUMN IF NOT EXISTS` guards for first-slice tables so partially initialized local or staging databases can recover.

### `media_job_runs`

All imports and cleanup reports need a durable run record.

```sql
CREATE TABLE media_job_runs (
    id UUID PRIMARY KEY,
    job_kind TEXT NOT NULL,
    status TEXT NOT NULL,
    requested_by TEXT NOT NULL,
    started_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    finished_at TIMESTAMPTZ,
    cursor JSONB,
    totals JSONB,
    error_message TEXT
);
```

`requested_by` for manual/operator routes should use `prakash-bhatt-yral` when this owner triggers a planned run.

### `media_job_failures`

Run-level state is not enough for resumable imports and pHash backfills. Per-row failures need their own retry state so one bad video, canister row, or storage object does not poison the whole job.

```sql
CREATE TABLE media_job_failures (
    id BIGSERIAL PRIMARY KEY,
    job_run_id UUID REFERENCES media_job_runs(id) ON DELETE SET NULL,
    job_kind TEXT NOT NULL,
    item_key TEXT NOT NULL,
    video_id TEXT,
    phase TEXT NOT NULL,
    source_ref TEXT,
    retry_count INTEGER NOT NULL DEFAULT 0,
    last_error TEXT NOT NULL,
    next_retry_at TIMESTAMPTZ,
    status TEXT NOT NULL DEFAULT 'pending_retry',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (job_kind, item_key, phase)
);

CREATE INDEX idx_media_job_failures_retry
    ON media_job_failures(status, next_retry_at);

CREATE INDEX idx_media_job_failures_video
    ON media_job_failures(video_id);
```

`item_key` should be stable for the failed unit of work, such as a video ID, object key, principal, post ID, or feed cursor value. Successful retry must either mark the row `resolved` or delete it after the successful write is committed. `ON DELETE SET NULL` is intentional: per-row failure evidence may outlive a run-retention cleanup.

---

## pHash Contract

The pHash contract must be settled before any cleanup or dedup migration.

The compatibility implementation must start by copying off-chain's `phash.rs` source behavior verbatim into `crates/phash`. That includes sequential frame decoding, frame-index selection, cyclic fill for short videos, 640-character binary formatting, and metadata extraction. Refactoring is allowed only after fixture tests prove byte-for-byte equality with the copied behavior.

This is a compatibility port, not a chance to design a cleaner algorithm. The current local crate has two independent incompatibilities: hex output versus binary output, and timestamp-seek frame selection versus off-chain sequential decode by frame index. Fixing only the formatter is insufficient.

### Required Behavior

- Compute exactly 10 frame hashes.
- Use 8x8 hash size.
- Return a 640-character binary string for compatibility with off-chain data.
- Select frames using the off-chain sequential-decode-by-frame-index behavior; do not use timestamp seeking for `offchain_binary_10x8_v1`.
- Cyclically fill frame hashes for short videos using the off-chain behavior.
- Extract metadata: duration, width, height, fps, container, codec where available.
- Fail explicitly on non-video files and unreadable media.
- Be deterministic for the same input file and ffmpeg/image-hasher versions.
- Run CPU/ffmpeg work in blocking threads, never on async runtime worker threads.

### Golden Tests

Create a small deterministic video fixture and test:

- output length is 640;
- output contains only `0` and `1`;
- repeated runs produce identical output;
- metadata fields are populated;
- old off-chain fixture outputs match the new crate for the same input.

If fixture output differs after the verbatim port, treat ffmpeg/image-hasher dependency drift as a blocker to investigate and pin before minting `offchain_binary_10x8_v1`. Future intentionally different algorithms may be added under a new `hash_version`, but they must not reuse old binary-hash rows for exact duplicate decisions.

---

## Scope Split: First Slice vs Future PRs

The first PR should prove ownership without trying to build the whole long-term media platform.

### Foundational First Slice

The first slice should include only the pieces needed to move pHash ownership safely:

- copy off-chain pHash behavior verbatim into `crates/phash`;
- add golden compatibility tests and deterministic fixture tests;
- version transitional mirror pHash storage so old local hashes and new off-chain-compatible hashes are never mixed;
- store pHash in Postgres with `hash_version` and `input_media_version`;
- create the basic master servable-video index;
- create `media_feed_events` for monotonic downstream replay;
- create `media_job_runs` and `media_job_failures`;
- import current `video_index` rows as source evidence;
- expose an audit for servable videos missing the required pHash.

### Future PRs That The First Slice Must Not Block

These surfaces are important for the long-term goals, but they should not bloat the first implementation PR:

- `media_artifacts` for original objects, canonical encodes, HLS renditions, thumbnails, staged images, mirrored legacy objects, and deletion evidence;
- `media_post_refs` for multiple posts, principals, canisters, and visibility states pointing at one video;
- `media_duplicate_groups` or equivalent canonicalization to distinguish exact hash matches from canonical originals;
- `media_feed_consumers` for downstream checkpointing, replay lag, retention, and SLA monitoring;
- richer pHash provenance: source artifact ID, implementation commit, ffmpeg version, image-hasher version, compute host, and supersession/current marker;
- row-level pHash job leases or a dedicated `media_hash_jobs` table for concurrent workers;
- videogen/upload lifecycle states such as reserved, submitted, uploaded, draft-created, servable, failed, and expired;
- table-by-table legacy import matrix for BigQuery, Kvrocks, Redis/Dragonfly, and Milvus;
- migration-file structure or split schema files instead of growing one large embedded SQL string;
- moderation-state provenance when imported policy state affects cleanup or servability decisions.

### Still Out Of Scope

These remain outside this program unless ownership changes explicitly:

- moderation policy and classification implementation;
- NSFW model ownership;
- upload approval policy;
- bot/country decisioning;
- personalization embedding generation and serving;
- DS-owned Milvus infrastructure;
- destructive storage deletion without a later approval gate.

---

## Phase Plan

### Phase 0: Discovery and Contract Freeze

**Tickets:** [#2113](https://github.com/dolr-ai/yral/issues/2113), [#2112](https://github.com/dolr-ai/yral/issues/2112), [#2106](https://github.com/dolr-ai/yral/issues/2106), [#2108](https://github.com/dolr-ai/yral/issues/2108), [#2114](https://github.com/dolr-ai/yral/issues/2114), [#2038](https://github.com/dolr-ai/yral/issues/2038), [#2046](https://github.com/dolr-ai/yral/issues/2046)

**Goal:** Produce a precise inventory of current pHash producers, consumers, storage sinks, and queue paths.

**Work:**

- List every off-chain pHash endpoint, job, queue, and helper.
- List every table/cache/vector store receiving pHash data.
- List every current consumer of `videohash_phash`, `videos_unique_v2`, Redis pHash keys, and Milvus pHash vectors.
- Document current local `crates/phash` mismatches: hex-vs-binary format and timestamp-seek-vs-frame-index frame selection.
- Decide whether near-duplicate search must remain Milvus-backed in the first rollout.
- Define a no-delete rule: cleanup tickets may produce reports, but not remove code or buckets until Phase 6 or later.

**Exit criteria:**

- pHash contract is documented.
- All known pHash producers and consumers are mapped.
- Phase 1 implementation can be done without breaking off-chain's current behavior.

### Phase 1: pHash Library Parity and Versioning

**Tickets:** [#2113](https://github.com/dolr-ai/yral/issues/2113), [#2108](https://github.com/dolr-ai/yral/issues/2108)

**Goal:** Make this repo's pHash library trustworthy and versioned.

**Work:**

- Copy off-chain `phash.rs` behavior verbatim into `crates/phash` as the `offchain_binary_10x8_v1` compatibility implementation.
- Preserve sequential frame decode, frame-index selection, cyclic fill, 640-character binary formatting, and metadata extraction.
- Add only the API wrapper needed by this repo after golden tests lock the copied behavior.
- Preserve the current mirror job behavior only if it is given an explicit hash version. Existing local mirror hashes must be tagged as `legacy_hex_8x8_v0`; new off-chain-compatible rows must be tagged as `offchain_binary_10x8_v1`.
- Add golden tests and fixture-based determinism tests.
- Do not add Milvus vector-packing or near-duplicate helpers to this service's production API; those belong with the DS-owned Milvus ingestion path if it remains in use.
- Keep any hamming-distance or vector-packing code test-only unless Phase 5 explicitly needs it for sampled parity checks.

**Exit criteria:**

- Tests prove the repo matches off-chain pHash output for the compatibility fixtures.
- Hash records can identify their `hash_version` and `input_media_version`.
- Existing mirror pHash data is not silently mixed with off-chain-compatible pHash data.

### Phase 2: Master Servable Video Index MVP

**Tickets:** [#2112](https://github.com/dolr-ai/yral/issues/2112), [#2026](https://github.com/dolr-ai/yral/issues/2026), [#2065](https://github.com/dolr-ai/yral/issues/2065)

**Goal:** Create `all_servable_videos_on_yral` and populate it from current reliable sources.

**Work:**

- Add schema for servable videos, source evidence, hash records, and job runs.
- Import existing mirror `video_index` rows as `storage_scan` evidence.
- Import `videos_unique_v2` and related legacy pHash metadata as legacy evidence.
- Import canister principals from `ivkka-7qaaa-aaaas-qbg3q-cai` through an explicit IC-agent client with pagination or chunked cursor state.
- Import posts/video IDs from `gxhc3-pqaaa-aaaas-qbh3q-cai` by paging per principal or per canister cursor, storing raw source evidence before reconciliation.
- Store canister import cursors in `media_job_runs.cursor`; store per-principal, per-post, or per-video failures in `media_job_failures`.
- Reconcile canister rows idempotently into `all_servable_videos_on_yral`, marking missing storage and missing post references instead of dropping unresolved rows.
- Reconcile rows into `servable_status`.
- Add audit endpoints that show counts by source, status, provider, bucket, and missing data.

**Exit criteria:**

- The index can answer "which videos are servable right now?"
- The index can answer "which servable videos have no pHash?"
- The index can answer "which servable videos still reference Cloudflare URLs?"
- No storage cleanup has happened yet.

### Phase 3: Live Write Path Integration

**Tickets:** [#2168](https://github.com/dolr-ai/yral/issues/2168), [#2134](https://github.com/dolr-ai/yral/issues/2134), [#2112](https://github.com/dolr-ai/yral/issues/2112), [#2037](https://github.com/dolr-ai/yral/issues/2037)

**Goal:** Ensure new upload and videogen flows write to the master index.

**Work:**

- On successful user upload, upsert a master-index row and source evidence.
- On successful videogen completion, upsert the same row type.
- Record generation/source metadata needed for future premium policy: resolution, duration, provider, model, queue path, and completion status.
- Ensure retries and duplicate callbacks do not create duplicate source rows.
- Keep mobile-facing behavior unchanged.

**Exit criteria:**

- Newly uploaded or generated videos appear in the master index without waiting for storage scans.
- The index can distinguish user uploads from generated videos.
- Premium/free policy work has reliable metadata but is not implemented in this phase.

### Phase 4: Prakash-Owned pHash Compute Pipeline

**Tickets:** [#2113](https://github.com/dolr-ai/yral/issues/2113), [#2114](https://github.com/dolr-ai/yral/issues/2114), [#2046](https://github.com/dolr-ai/yral/issues/2046), [#2104](https://github.com/dolr-ai/yral/issues/2104), [#2045](https://github.com/dolr-ai/yral/issues/2045)

**Goal:** Move pHash compute scheduling and storage into this service.

**Work:**

- Add an internal pHash job that selects master-index videos missing the required pHash version.
- Download source media from the current canonical storage provider.
- Compute pHash and metadata in bounded concurrency.
- Store hash rows in `servable_video_hashes`.
- Do not write new BigQuery rows.
- Do not make Kvrocks part of the source-of-truth write path; if a downstream Kvrocks cache is still needed, it should consume the read feed.
- Add retry state and dead-letter reporting through `media_job_runs` and `media_job_failures`.
- Replace QStash-shaped pHash backfills with self-hosted queue or service-local background jobs.
- Append `media_feed_events` in the same serialized transaction as hash and feed-visible master-index writes.
- Expose a cursor-paginated read feed for downstream DS/Kvrocks/Milvus consumers.

**Exit criteria:**

- New pHash values are computed without calling off-chain.
- Backfills can be paused, resumed, and audited.
- BigQuery pHash writes are not part of the target path.
- Kvrocks is either downstream/cache-only or removed from the pHash path.
- DS/Milvus consumers can ingest from the service feed using `media_feed_events.cursor`.

### Phase 5: Duplicate Lookup Migration

**Tickets:** [#2113](https://github.com/dolr-ai/yral/issues/2113), [#2038](https://github.com/dolr-ai/yral/issues/2038), [#2045](https://github.com/dolr-ai/yral/issues/2045)

**Goal:** Move exact duplicate lookup responsibility out of off-chain. Near-duplicate pHash search is a DS-owned follow-on only if product still needs it.

**Work:**

- Implement exact duplicate lookup from Postgres using `(hash_kind, hash_version, hash_value)`.
- Treat exact duplicate grouping from Postgres as sufficient for the first Prakash-owned milestone.
- Decide with DS whether pHash near-duplicate lookup remains Milvus-backed after the exact-lookup milestone.
- If Milvus remains, DS owns Milvus hosting, collection design, and ingestion from this service's feed.
- If Kvrocks/Redis exact-match cache remains, DS owns cache materialization from the feed and treats it as rebuildable, not source of truth.
- Treat [#2023](https://github.com/dolr-ai/yral/issues/2023) as coordination-only: personalization embeddings are not pHash feed consumers by default.
- Add an authenticated duplicate-check endpoint if a caller still needs online duplicate checks.
- Preserve "compute-only" and "compute-and-store" use cases separately.

**Exit criteria:**

- Off-chain no longer needs to compute pHash for duplicate checks.
- Exact duplicate detection has a Postgres source of truth.
- Near-duplicate detection has a documented DS owner and feed-based ingestion path.

### Phase 6: Storage and Cloudflare Retirement Reports

**Tickets:** [#2026](https://github.com/dolr-ai/yral/issues/2026), [#2065](https://github.com/dolr-ai/yral/issues/2065), [#2066](https://github.com/dolr-ai/yral/issues/2066), [#2107](https://github.com/dolr-ai/yral/issues/2107), [#2108](https://github.com/dolr-ai/yral/issues/2108)

**Goal:** Use the master index to prove what can be cleaned up.

**Work:**

- Produce a Cloudflare Stream usage report from code references, URLs in data, and traffic/log evidence.
- Produce a Storj bucket report: buckets, provisioned keys, code references, object counts, and index references.
- Produce a yral-video-upload-service cleanup report for routes or fields now superseded by the master index.
- Produce a yral-video-storage-service cleanup report for old mirror-only routes that are superseded by master media routes.
- Keep destructive operations behind a separate approval gate.

**Exit criteria:**

- Every candidate bucket/key/product has a report with evidence.
- Cloudflare Stream retirement has no unknown active consumer.
- Cleanup PRs can be scoped to one service at a time.

### Phase 7: Canonical Encoding Policy

**Tickets:** [#1992](https://github.com/dolr-ai/yral/issues/1992), [#2077](https://github.com/dolr-ai/yral/issues/2077), [#2074](https://github.com/dolr-ai/yral/issues/2074)

**Goal:** Decide when and how video re-encoding happens, and how that affects pHash.

**Work:**

- Research target container, codec, resolution ladder, bitrate, audio policy, and `moov` atom placement.
- Decide whether pHash is computed on original input, canonical encoded output, or both.
- If both, store two `input_media_version` values.
- Add metadata fields to prove `moov` atom is front-loaded for feed MP4s.
- Treat seeded-image first-frame behavior as a generated-video quality issue, not as a reason to silently alter pHash semantics.

**Exit criteria:**

- Encoding policy is written and approved before global re-encoding.
- pHash versioning makes old and new hashes distinguishable.
- Feed-serving MP4s can be audited for fast-start behavior.

### Phase 8: Off-Chain and Service Cleanup

**Tickets:** [#2106](https://github.com/dolr-ai/yral/issues/2106), [#2108](https://github.com/dolr-ai/yral/issues/2108), [#2114](https://github.com/dolr-ai/yral/issues/2114), [#2038](https://github.com/dolr-ai/yral/issues/2038)

**Goal:** Remove migrated paths only after replacements are live and audited.

**Work:**

- Remove off-chain pHash compute endpoints after no callers remain.
- Remove off-chain QStash pHash scheduling after the self-hosted path is live.
- Remove stale BigQuery write dependencies after any one-time historical import is complete.
- Remove direct Kvrocks write dependencies from this service; downstream cache materialization should consume the feed.
- Remove obsolete mirror-only pHash routes or rename them under the master media surface.
- Keep rollback notes for one release cycle.

**Exit criteria:**

- No live route, queue, or scheduled job points to off-chain pHash compute.
- The master index and pHash pipeline have passed a production backfill and live-write soak.
- Cleanup PRs cite the evidence reports from Phase 6.

---

## Data Flow

### Backfill Flow

1. Storage scan imports object evidence.
2. Legacy datasets import historic pHash and unique-video evidence.
3. Canister import maps principals to posts and video IDs.
4. Reconciler upserts `all_servable_videos_on_yral`.
5. pHash job computes missing required hash versions.
6. Audit endpoints expose coverage, duplicate groups, missing storage, and legacy-provider references.

### Live Flow

1. Upload or videogen completion succeeds.
2. Service upserts `all_servable_videos_on_yral`.
3. Service appends source evidence.
4. pHash compute is queued or marked pending.
5. Worker computes and stores `servable_video_hashes`, appending `media_feed_events` in the same transaction.
6. Downstream duplicate/cache/vector consumers read changes from the monotonic feed if they remain configured.

### Cleanup Flow

1. Cleanup report queries master index and code references.
2. Candidate deletion is listed with evidence.
3. Human approval happens outside this spec.
4. Deletion job runs idempotently.
5. Master index marks deleted or no-longer-referenced state.

---

## Error Handling and Operations

- All long-running jobs must have run IDs.
- All jobs must store cursor/progress so they can resume.
- Per-video failures should not fail the whole batch.
- Failed rows must be recorded in `media_job_failures` with `retry_count`, `last_error`, and `next_retry_at`.
- Success updates must mark stale failure rows `resolved` or delete them after the successful write commits.
- pHash compute must use bounded concurrency and temp files, not full in-memory video buffers.
- Every destructive cleanup candidate must have a dry-run report first.
- Sentry events for media jobs should include job ID, video ID, source provider, object key, hash version, and phase.
- Add counters for discovered, imported, pHash computed, pHash failed, missing storage, duplicate exact matches, legacy references, and cleanup candidates.

Ticket [#2104](https://github.com/dolr-ai/yral/issues/2104) should be treated as a general observability requirement for this program: missing detailed failure events are not acceptable for pHash, backfill, upload completion, or cleanup jobs.

---

## Testing Strategy

### Unit Tests

- pHash binary output contract.
- pHash deterministic fixture.
- pHash compatibility fixtures covering frame selection, cyclic fill, and metadata extraction.
- source evidence dedupe.
- row status transitions.
- media feed outbox event creation for feed-visible writes.

### Integration Tests

- Import from existing mirror index into master index.
- Idempotent canister import with fixture data.
- pHash job against a local fixture video.
- Duplicate exact-match lookup from Postgres.
- Cursor-paginated feed replay from `media_feed_events`.
- Job retry and dead-letter behavior.

### Operational Tests

- Dry-run import against production-like data with write disabled.
- Backfill limited to a small publisher or fixed limit.
- Audit count comparison between mirror index, legacy dataset, canister posts, and master index.
- Cloudflare URL reference report before retirement.
- Bucket cleanup report before deletion.

### Rollback Tests

- Old off-chain pHash path can remain enabled during Phase 4 compatibility window.
- New pHash write path can be disabled with an env flag.
- Duplicate lookup callers can fall back to off-chain until Phase 5 exits.

---

## Rollout Gates

| Gate | Required evidence |
|---|---|
| Before Phase 1 merge | pHash contract and version naming agreed |
| Before Phase 2 production import | Schema tested locally and import dry-run count reviewed |
| Before Phase 4 live compute | Golden pHash tests passing and legacy compatibility decision documented |
| Before Phase 5 migration | Exact duplicate behavior compared against off-chain for sampled videos |
| Before Phase 6 cleanup | Master index coverage report shows no unknown references |
| Before production deployment | Deployment image pHash parity checklist completed from `docs/superpowers/plans/2026-06-17-phash-deployment-todo.md` |
| Before enabling pHash outside local development or running production backfill | Deployment-image tests, FFmpeg/libav version capture, off-chain sample comparison, and transitional `video_index` column verification from `docs/superpowers/plans/2026-06-17-phash-deployment-todo.md` completed |
| Before Phase 8 off-chain removal | No callers, queues, or jobs still target off-chain pHash |

---

Deployment validation is a final enablement gate. It must not block the next implementation slice that creates canonical media-index storage.

## Next Execution Slice

Phase 1A has established the pHash compatibility contract and transitional mirror versioning. Phase 1B has added the canonical media-index schema, versioned hash repository helpers, and serialized feed outbox. The next implementation slice should populate and expose that foundation:

1. Import current `video_index` rows as `legacy_video_index` source evidence.
2. Import versioned mirror hashes from `video_index.phash`, `video_index.phash_kind`, and `video_index.phash_version` into `servable_video_hashes` when all three fields are present.
3. Append `media_feed_events` in the same serialized transaction for inserted or materially changed canonical rows.
4. Add audit queries for servable videos missing the required pHash and for source/status/provider coverage.
5. Keep `video_index.phash`, `video_index.phash_kind`, and `video_index.phash_version` only for mirror compatibility until mirror routes are retired or renamed.

This slice advances [#2113](https://github.com/dolr-ai/yral/issues/2113), starts [#2112](https://github.com/dolr-ai/yral/issues/2112), and creates the evidence needed for [#2108](https://github.com/dolr-ai/yral/issues/2108).

---

## Open Questions

1. Which historical BigQuery rows, if any, need a one-time import before BigQuery is fully ignored for pHash?
2. What feed retention window and replay SLA do DS/Kvrocks/Milvus consumers need after `media_feed_events` is live?
3. Which media form should define the primary pHash: original uploaded/generated object, canonical encoded output, or both?
4. Should this repo expose a public duplicate-check endpoint, or only authenticated internal/operator endpoints?
5. What is the minimum production soak period before off-chain pHash routes can be removed?

---

## Recommended Next Step

Write a focused implementation plan for the first execution slice only. Do not plan all eight phases as one implementation batch. The phases are coupled at the program level, but each phase should produce independently testable software and its own reviewable PR.
