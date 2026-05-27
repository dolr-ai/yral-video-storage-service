# Lean Videogen Migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move the LTX videogen flow from off-chain-agent into Prakash, with Ansuman moderation, RateLimiter accounting, direct Vast submission, Vast-owned bucket upload, and Prakash-owned completion/draft handling.

**Architecture:** Prakash becomes the mobile-facing videogen API and durable orchestration point. Vast owns generation, bucket upload, local output cleanup, and authenticated completion callbacks. Off-chain-agent remains live only for legacy in-flight LTX jobs until the old QStash/callback path drains.

**Tech Stack:** Rust, Axum, tokio-postgres, ic-agent/yral-canisters-client, reqwest, HMAC-SHA256, AES-256-GCM, uuid v4, Postgres, ComfyUI/Vast worker, existing mobile/off-chain videogen DTO contracts.

---

## Source Of Truth

Implement against `docs/superpowers/specs/2026-05-27-lean-videogen-migration-design.md` in `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service`.

Do not reintroduce these removed off-chain responsibilities into Prakash:

- QStash enqueue/callback processing.
- DOLR/Sats balance deduction.
- model-cost lookup.
- HON worker JWT.
- DOLR user-agent creation.
- balance rollback for migrated LTX jobs.

## Repository Sections

| Repo | Path | Responsibility |
| --- | --- | --- |
| Prakash | `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service` | Mobile API, moderation, RateLimiter calls, completion context, completion endpoint, draft creation, reconciliation, provider compatibility. |
| Vast LTX | `/Users/prk-jr/Desktop/work/dolr/videogen` | Accept Prakash submissions, run ComfyUI/LTX, upload generated MP4 to scoped destination, delete local output after upload/outbox persistence, sign callbacks, replay completion outbox. |
| off-chain-agent | `/Users/prk-jr/Desktop/work/dolr/off-chain-agent` | Keep legacy jobs alive, then gate/remove old LTX submit path after drain. No new migrated LTX traffic should go through QStash or `/comfyui/webhook`. |

## File Structure To Create Or Modify

### Prakash

- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/Cargo.toml`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/main.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/lib.rs` only for the new pure `src/videogen` support module. Do not export `routes` from the library unless `AppState` is first moved into the library crate.
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/consts.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/db.rs`
- Replace: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/routes/videogen.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/routes/videogen/mod.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/routes/videogen/drafts.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/routes/videogen/generate.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/routes/videogen/complete.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/routes/videogen/providers.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/routes/videogen/upload_refresh.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/mod.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/config.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/types.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/context.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/fingerprint.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/hmac.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/identity_crypto.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/ansuman.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/rate_limiter.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/upload_destination.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/vast.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/draft.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/reconcile.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/tests/videogen_generate.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/tests/videogen_completion.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/tests/videogen_contract.rs`

Keep the existing in-progress draft behavior by moving the current code from `src/routes/videogen.rs` to `src/routes/videogen/drafts.rs`.

### Vast LTX

- Modify: `/Users/prk-jr/Desktop/work/dolr/videogen/Cargo.toml`
- Modify: `/Users/prk-jr/Desktop/work/dolr/videogen/src/config.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/videogen/src/routes/generate.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/videogen/src/backend/mod.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/videogen/src/backend/comfyui/mod.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/videogen/src/backend/comfyui/client.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/videogen/src/cleanup.rs`
- Modify or replace: `/Users/prk-jr/Desktop/work/dolr/videogen/src/webhook.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/videogen/src/prakash_completion.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/videogen/src/upload_destination.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/videogen/src/outbox.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/videogen/src/output_file.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/videogen/tests/prakash_completion.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/videogen/tests/upload_destination.rs`

### off-chain-agent

- Modify: `/Users/prk-jr/Desktop/work/dolr/off-chain-agent/src/videogen/handlers_v2.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/off-chain-agent/src/videogen/router.rs` only if route gating is cleaner there.
- Modify: `/Users/prk-jr/Desktop/work/dolr/off-chain-agent/src/consts.rs` or equivalent config module.
- Create: `/Users/prk-jr/Desktop/work/dolr/off-chain-agent/docs/videogen-ltx-drain-runbook.md`

## Shared Contract Shapes

Use `request_id` consistently as the Prakash-generated UUIDv4 and Vast job identifier. Do not introduce `job_id` in new cross-service JSON.

### Prakash To Vast Submit

```json
{
  "request_id": "<uuid-v4-generated-by-prakash>",
  "request_key": { "principal": "...", "counter": 123 },
  "user_principal": "...",
  "model_id": "ltx2",
  "workflow_json": { "...": "ComfyUI workflow JSON" },
  "input": { "prompt": "...", "image_url": "..." },
  "callback_url": "https://prakash.example/api/v2/videogen/complete",
  "upload_url_refresh_url": "https://prakash.example/api/v2/videogen/upload-url/refresh",
  "upload_destination": {
    "video_id": "...",
    "object_key": "...",
    "upload_url": "...",
    "expires_at": "2026-05-27T12:00:00Z"
  }
}
```

`upload_url_refresh_url` is optional and must be included only when refresh is enabled.

These Prakash-to-Vast fields are top-level fields on Vast `GenerateRequest`. Do not nest `request_id`, `request_key`, `user_principal`, `callback_url`, or `upload_destination` under the `input` object. The `input` object contains only LTX model input such as `prompt` and optional `image_url`.

`workflow_json` is required for the current Vast ComfyUI worker. If Vast later derives workflow from `model_id` server-side, update this shared contract and both Prakash/Vast tests in the same change.

Accepted response:

```json
{
  "request_id": "<same-uuid-v4>",
  "status": "submitted",
  "accepted_at": "2026-05-27T11:00:00Z"
}
```

### Vast To Prakash Completion

Every success and failure callback requires the same HMAC headers:

- `X-Timestamp`
- `X-Body-SHA256`
- `X-Key-Id`
- `Authorization: HMAC-SHA256 <hex_signature>`

Signature message:

```text
METHOD + "\n" + PATH + "\n" + X-Timestamp + "\n" + X-Body-SHA256
```

Required body fields on every callback:

```json
{
  "request_key": { "principal": "...", "counter": 123 },
  "user_principal": "...",
  "request_id": "<uuid-v4>",
  "provider": "Ltx2",
  "status": "success"
}
```

Success adds `bucket_url`, `video_id` or `object_key`, and file metadata when available. Failure adds `failure_reason`.

## Pre-Implementation Gates

### Task 0: Confirm External Contracts Before Coding

**Files:**
- Modify if decisions differ: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/docs/superpowers/specs/2026-05-27-lean-videogen-migration-design.md`
- Modify if decisions differ: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/docs/superpowers/plans/2026-05-27-lean-videogen-migration-implementation.md`

- [ ] **Step 1: Confirm upload service URL capability**

Confirm the upload service can issue scoped upload URLs valid for at least `VIDEOGEN_UPLOAD_URL_TTL_SECS=4200`.

Expected:
- If yes, Prakash may include the original scoped upload URL in Vast submission.
- If no, Prakash Task 6 refresh endpoint is required before staging and Vast Task 10 must call it based on `expires_at`.

- [ ] **Step 2: Confirm draft service idempotency**

Confirm the upload metadata/draft service deduplicates on `video_id` or a named idempotency key.

Expected:
- If the draft service already deduplicates on `video_id`, record that fact.
- If it does not, add or schedule that guard before enabling Prakash completion in any shared environment.

- [ ] **Step 3: Confirm Vast duplicate submit semantics**

Confirm duplicate `request_id` submissions return the same accepted response and do not start a second generation.

Expected:
- Vast accepts Prakash-generated UUIDv4 `request_id`.
- Vast echoes the same `request_id` in acceptance and completion.
- Duplicate submit with same `request_id` is idempotent.

- [ ] **Step 4: Confirm workflow contract**

Confirm whether Vast requires Prakash to send `workflow_json` for migrated requests.

Expected:
- Current plan assumes `workflow_json` is required and top-level in Vast `GenerateRequest`.
- If Vast derives workflow from `model_id`, update the shared JSON contract, Prakash Task 5, and Vast Task 9 before implementation starts.

- [ ] **Step 5: Record decisions**

Update the spec or this plan only if the confirmed behavior differs from the assumptions above.

- [ ] **Step 6: Verify and commit any docs update**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
git diff --check
```

Expected: no whitespace errors.

Commit if files changed:

```bash
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service add docs/superpowers/specs/2026-05-27-lean-videogen-migration-design.md docs/superpowers/plans/2026-05-27-lean-videogen-migration-implementation.md
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service commit -m "docs: record videogen external contract gates"
```

## Prakash Tasks

### Task 1: Split Videogen Routes Without Behavior Change

**Files:**
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/routes/mod.rs`
- Replace: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/routes/videogen.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/routes/videogen/mod.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/routes/videogen/drafts.rs`

- [ ] **Step 1: Write the failing route preservation test**

Create the new module shell with a crate-local test proving `routes::videogen::get_in_progress_drafts` is re-exported. Do not import routes from the library crate for this test: route modules currently depend on binary `AppState`, so exporting `routes` from `src/lib.rs` would fail unless `AppState` is moved into the library.

```rust
pub mod drafts;
pub use drafts::{
    get_in_progress_drafts, InProgressDraftItem, InProgressDraftsRequest,
    InProgressDraftsResponse,
};

#[cfg(test)]
mod tests {
    use super::get_in_progress_drafts;

    #[test]
    fn videogen_drafts_handler_is_exported() {
        let _ = get_in_progress_drafts;
    }
}
```

- [ ] **Step 2: Run test to verify it fails before exports are public**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test videogen_drafts_handler_is_exported
```

Expected: compile failure because both `src/routes/videogen.rs` and `src/routes/videogen/mod.rs` exist, or because `drafts.rs` has not yet been created.

- [ ] **Step 3: Move current code unchanged**

Move the current in-progress implementation into `src/routes/videogen/drafts.rs`. Delete `src/routes/videogen.rs`; Rust cannot keep both `src/routes/videogen.rs` and `src/routes/videogen/mod.rs` for the same module.

Final `src/routes/videogen/mod.rs`:

```rust
pub mod drafts;

pub use drafts::{
    get_in_progress_drafts, InProgressDraftItem, InProgressDraftsRequest,
    InProgressDraftsResponse,
};
```

Update `src/routes/mod.rs` to keep `pub mod videogen;`. Do not update `src/lib.rs` in this task.

- [ ] **Step 4: Run tests**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test videogen_drafts_handler_is_exported
cargo test
```

Expected: `videogen_drafts_handler_is_exported` passes and the existing suite remains green.

- [ ] **Step 5: Commit**

```bash
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service add -A src/routes/mod.rs src/routes/videogen.rs src/routes/videogen tests/videogen_generate.rs
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service commit -m "refactor: split videogen route modules"
```

### Task 2: Add Prakash Videogen Config And Postgres Context Schema

**Files:**
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/Cargo.toml`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/lib.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/main.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/consts.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/db.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/mod.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/config.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/types.rs`

- [ ] **Step 1: Write failing schema/config tests**

Add tests for default config values and schema states:

```rust
#[test]
fn upload_url_ttl_default_exceeds_required_window() {
    let cfg = VideogenConfig::test_defaults();
    assert!(cfg.upload_url_ttl_secs >= cfg.upload_url_pre_submit_margin_secs
        + cfg.ltx_generation_timeout_secs
        + cfg.completion_retry_grace_secs
        + cfg.vast_upload_retry_window_secs
        + cfg.upload_url_safety_buffer_secs);
    assert_eq!(cfg.upload_url_ttl_secs, 4200);
}

#[test]
fn terminal_states_are_absorbing() {
    assert!(VideogenContextState::Complete.is_terminal());
    assert!(VideogenContextState::SubmitFailed.is_terminal());
    assert!(VideogenContextState::StaleFailed.is_terminal());
    assert!(VideogenContextState::DraftFailed.is_terminal());
    assert!(VideogenContextState::Failed.is_terminal());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test videogen::config
```

Expected: compile failure because modules do not exist.

- [ ] **Step 3: Add dependencies and config**

Add dependencies as needed:

```toml
aes-gcm = "0.10"
base64 = "0.22"
async-trait = "0.1"
uuid = { version = "1", features = ["v4", "serde"] }
rand = "0.8"
tokio-postgres = { version = "0.7.15", features = ["with-chrono-0_4", "with-serde_json-1"] }
```

If `tokio-postgres` already exists as a plain dependency, replace it with the feature-enabled form above. The completion context stores `TIMESTAMPTZ` and `JSONB`, so `chrono` and `serde_json` feature support must be enabled before context code is written.

Wire the support module into both crate targets:

```rust
// src/lib.rs
pub mod videogen;

// src/main.rs
mod videogen;
```

Do not export `routes` from `src/lib.rs`. Keep `src/videogen/*` free of binary-only `AppState` dependencies so pure helper tests can run through the library crate.

Implement `VideogenConfig` with every timeout/key setting from the spec, including:

- `VIDEOGEN_GENERATE_DEDUPE_WINDOW_SECS=120`
- `VIDEOGEN_VAST_SUBMIT_TIMEOUT_SECS=10`
- `VIDEOGEN_UPLOAD_URL_PRE_SUBMIT_MARGIN_SECS=10`
- `VIDEOGEN_VAST_IMAGE_STAGE_TIMEOUT_SECS=30`
- `VIDEOGEN_CONTEXT_CREATED_TIMEOUT_SECS=120`
- `VIDEOGEN_LTX_GENERATION_TIMEOUT_SECS=1800`
- `VIDEOGEN_COMPLETION_RETRY_GRACE_SECS=900`
- `VIDEOGEN_VAST_UPLOAD_RETRY_WINDOW_SECS=900`
- `VIDEOGEN_VAST_UPLOAD_EXPIRY_REFRESH_MARGIN_SECS=300`
- `VIDEOGEN_UPLOAD_URL_SAFETY_BUFFER_SECS=300`
- `VIDEOGEN_UPLOAD_URL_TTL_SECS=4200`
- `VIDEOGEN_RECONCILIATION_INTERVAL_SECS=60`
- `VIDEOGEN_RECONCILIATION_BATCH_SIZE=100`
- `VIDEOGEN_DRAFT_CREATE_MAX_ATTEMPTS=3`
- `VIDEOGEN_DRAFT_CREATE_TIMEOUT_SECS=600`
- `VIDEOGEN_DRAFT_CREATED_COMPLETE_TIMEOUT_SECS=120`
- `VIDEOGEN_DRAFT_RETRY_RETENTION_HOURS=72`
- `VIDEOGEN_COMPLETION_HMAC_SKEW_SECS=120`

Production startup must reject `ANSUMAN_MODERATION_MODE=mock_allow` when `ENVIRONMENT=production`.

- [ ] **Step 4: Add context state enum**

Implement:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideogenContextState {
    ContextCreated,
    Submitted,
    Uploaded,
    DraftCreating,
    DraftCreated,
    Complete,
    SubmitFailed,
    StaleFailed,
    DraftFailed,
    Failed,
}
```

Add `as_str`, `try_from_db`, `is_terminal`, and transition validation helpers.

- [ ] **Step 5: Extend inline schema**

Add `videogen_completion_contexts` to `src/db.rs` `SCHEMA_SQL` with at least:

```sql
CREATE TABLE IF NOT EXISTS videogen_completion_contexts (
    principal TEXT NOT NULL,
    counter BIGINT NOT NULL,
    operation_id TEXT NOT NULL UNIQUE,
    request_fingerprint TEXT NOT NULL,
    request_fingerprint_version INTEGER NOT NULL DEFAULT 1,
    provider TEXT NOT NULL,
    model_id TEXT NOT NULL,
    prompt TEXT NOT NULL,
    upload_handling TEXT NOT NULL,
    encrypted_delegated_identity BYTEA,
    identity_nonce BYTEA,
    encryption_key_id TEXT,
    upload_destination JSONB,
    draft_video_id TEXT,
    object_key TEXT,
    bucket_url TEXT,
    request_id TEXT,
    state TEXT NOT NULL CHECK (state IN (
        'context_created','submitted','uploaded','draft_creating','draft_created',
        'complete','submit_failed','stale_failed','draft_failed','failed'
    )),
    vast_submit_attempts INTEGER NOT NULL DEFAULT 0,
    completion_attempts INTEGER NOT NULL DEFAULT 0,
    draft_attempts INTEGER NOT NULL DEFAULT 0,
    reconciliation_attempts INTEGER NOT NULL DEFAULT 0,
    dedupe_expires_at TIMESTAMPTZ NOT NULL,
    generation_expires_at TIMESTAMPTZ NOT NULL,
    upload_destination_expires_at TIMESTAMPTZ,
    last_error TEXT,
    last_reconciliation_error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (principal, counter)
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_videogen_context_draft_video_id
    ON videogen_completion_contexts (draft_video_id)
    WHERE draft_video_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_videogen_context_dedupe
    ON videogen_completion_contexts (principal, request_fingerprint, created_at);
CREATE INDEX IF NOT EXISTS idx_videogen_context_state_updated
    ON videogen_completion_contexts (state, updated_at);
CREATE UNIQUE INDEX IF NOT EXISTS idx_videogen_context_request_id
    ON videogen_completion_contexts (request_id)
    WHERE request_id IS NOT NULL;
```

Do not attach the generic `update_updated_at()` trigger to `videogen_completion_contexts`. State-progress updates must explicitly set `updated_at = NOW()`. Reconciliation canister-unavailable handling must be able to record `last_reconciliation_error` while preserving the old `updated_at`, because stale-row selection depends on that timestamp.

- [ ] **Step 6: Run tests**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test videogen::config
cargo test
```

Expected: config and state tests pass.

- [ ] **Step 7: Commit**

```bash
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service add Cargo.toml Cargo.lock src/consts.rs src/db.rs src/lib.rs src/main.rs src/videogen
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service commit -m "feat: add videogen config and context schema"
```

### Task 3: Implement Pure Prakash Helpers

**Files:**
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/fingerprint.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/hmac.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/identity_crypto.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/mod.rs`

- [ ] **Step 1: Write failing fingerprint tests**

Test canonical JSON stability, field inclusion, and decoded-image hashing:

```rust
#[test]
fn fingerprint_hashes_decoded_base64_image_bytes() {
    let req = fingerprint_fixture_with_base64_image("aGVsbG8=");
    let fp = compute_request_fingerprint(&req).unwrap();
    assert_eq!(fp.version, 1);
    assert_eq!(fp.image_hash_hex, sha256_hex(b"hello"));
}
```

- [ ] **Step 2: Write failing HMAC tests**

```rust
#[test]
fn completion_signature_round_trips() {
    let registry = HmacKeyRegistry::parse("v1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap();
    let body_hash = sha256_hex(br#"{"status":"success"}"#);
    let sig = sign_completion("POST", "/api/v2/videogen/complete", 1_777_000_000, &body_hash, registry.get("v1").unwrap());
    assert!(verify_completion_signature(&registry, "v1", "POST", "/api/v2/videogen/complete", 1_777_000_000, &body_hash, &sig, 1_777_000_001, 120).is_ok());
}

#[test]
fn unknown_key_id_fails_without_fallback() {
    let registry = HmacKeyRegistry::parse("v1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap();
    assert!(matches!(
        verify_completion_signature(&registry, "v2", "POST", "/api/v2/videogen/complete", 1, "hash", "sig", 1, 120),
        Err(HmacError::UnknownKeyId)
    ));
}
```

- [ ] **Step 3: Write failing identity encryption tests**

Test encrypt/decrypt with `encryption_key_id`, and decryption failure with missing old key.

- [ ] **Step 4: Run tests to verify they fail**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test videogen::fingerprint videogen::hmac videogen::identity_crypto
```

Expected: compile failures.

- [ ] **Step 5: Implement helpers**

Implement canonical fingerprinting exactly as the spec says:

- sorted-key JSON object
- `fingerprint_version: 1`
- principal, model id, prompt, negative prompt/null, aspect ratio, duration, resolution, seed, generate-audio flag, upload handling, token type, image identity
- lowercase hex SHA-256 of canonical JSON bytes

Implement HMAC signing/verification with timestamp skew and raw-body SHA-256.

Implement AES-256-GCM identity encryption with a 96-bit random nonce and key registry parsing from base64 32-byte keys.

- [ ] **Step 6: Run tests**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test videogen::fingerprint
cargo test videogen::hmac
cargo test videogen::identity_crypto
```

Expected: all helper tests pass.

- [ ] **Step 7: Commit**

```bash
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service add Cargo.toml Cargo.lock src/videogen/fingerprint.rs src/videogen/hmac.rs src/videogen/identity_crypto.rs src/videogen/mod.rs
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service commit -m "feat: add videogen fingerprint and auth helpers"
```

### Task 4: Add Prakash Service Boundaries And Test Doubles

**Files:**
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/ansuman.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/rate_limiter.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/upload_destination.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/vast.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/draft.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/mod.rs`

- [ ] **Step 1: Write failing boundary tests**

Use trait-based boundaries so route tests can run without real Ansuman, RateLimiter, upload service, or Vast:

```rust
#[async_trait::async_trait]
pub trait ModerationClient {
    async fn moderate(&self, input: ModerationInput) -> Result<ModerationDecision, ModerationError>;
}

#[async_trait::async_trait]
pub trait VastClient {
    async fn submit(&self, request: VastSubmitRequest) -> Result<VastSubmitAccepted, VastSubmitError>;
}
```

Test that:

- Ansuman `mock_allow` returns safe.
- production `mock_allow` config is rejected.
- Vast accepted response must echo the exact `request_id`.
- Vast submit serializes `Authorization: Bearer <VAST_API_KEY>`.

- [ ] **Step 2: Run tests to verify they fail**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test videogen::ansuman videogen::vast
```

Expected: compile failures.

- [ ] **Step 3: Implement clients and request/response DTOs**

Implement DTOs for:

- `ModerationInput`
- `ModerationDecision`
- `RateLimiterRequestKey`
- `UploadDestination`
- `VastSubmitRequest`
- `VastSubmitAccepted`
- `DraftCreationRequest`

The real RateLimiter wrapper should preserve the no-deduction behavior:

- `token_type`: from mobile request, default `Free` if absent.
- `is_paid`: `false`.
- `payment_amount`: `None`.

- [ ] **Step 4: Run tests**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test videogen::ansuman
cargo test videogen::vast
cargo test videogen::rate_limiter
```

Expected: service-boundary tests pass.

- [ ] **Step 5: Commit**

```bash
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service add src/videogen
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service commit -m "feat: add videogen service boundaries"
```

### Task 5: Implement Prakash Generate Endpoint

**Files:**
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/routes/videogen/generate.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/routes/videogen/mod.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/main.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/context.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/tests/videogen_generate.rs`

- [ ] **Step 1: Write failing route tests for rejection order**

Tests must prove:

- delegated identity mismatch returns `401`.
- `upload_handling != ServerDraft` returns `400` before moderation and RateLimiter.
- NSFW returns `400 InvalidInput` and does not call RateLimiter.
- RateLimiter rejection returns `429` and does not call Vast.
- image staging timeout marks RateLimiter failed and does not call Vast.

- [ ] **Step 2: Write failing safe-path test**

Test a safe request:

- validates identity
- moderates prompt/image before staging
- creates RateLimiter request
- persists context
- encrypts identity
- reserves upload destination
- stages image when present
- builds or retrieves the LTX `workflow_json` required by the Vast ComfyUI worker
- generates UUIDv4 `request_id`
- submits to Vast with bearer auth, `workflow_json`, callback URL, upload destination, optional refresh URL
- verifies echoed `request_id`
- returns `operation_id`, `provider`, `request_key`

- [ ] **Step 3: Run tests to verify they fail**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test --test videogen_generate
```

Expected: failing tests because the route does not exist.

- [ ] **Step 4: Implement the route as a thin orchestration layer**

Route order must be:

1. body limit and cheap HTTP-layer protection
2. delegated identity parse
3. user principal equality check
4. model/input extraction
5. `ServerDraft` validation
6. fingerprint dedupe lookup
7. Ansuman moderation
8. RateLimiter create/check
9. Postgres context create
10. image staging
11. workflow JSON selection/adaptation
12. upload destination reservation
13. UUIDv4 `request_id` generation
14. Vast submit
15. update state to `submitted`
16. return existing success response shape

If Postgres context creation fails after RateLimiter accepts, immediately call RateLimiter `Failed(reason)`, decrement the `VIDEOGEN` counter, release upload destination if created, and return an error.

If Vast submit times out or is not accepted, mark `submit_failed`, update RateLimiter failed, decrement usage, release upload destination, redact encrypted identity, and return `503`.

- [ ] **Step 5: Register route and OpenAPI**

Add:

```rust
.route("/api/v2/videogen/generate", post(routes::videogen::generate_video))
```

Add OpenAPI path and schemas for the request, success, and error DTOs.

- [ ] **Step 6: Run tests**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test --test videogen_generate
cargo test
```

Expected: generate route tests and full suite pass.

- [ ] **Step 7: Commit**

```bash
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service add src/main.rs src/routes/videogen src/videogen tests/videogen_generate.rs
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service commit -m "feat: add lean videogen generate endpoint"
```

### Task 6: Implement Prakash Completion And Upload URL Refresh Endpoints

**Files:**
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/routes/videogen/complete.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/routes/videogen/upload_refresh.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/context.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/draft.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/main.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/tests/videogen_completion.rs`

- [ ] **Step 1: Write failing HMAC and body-limit tests**

Tests must cover:

- invalid HMAC returns `401` and does not mutate RateLimiter or Postgres.
- unknown `X-Key-Id` returns `401`.
- old key inside rotation overlap succeeds.
- oversized completion body is rejected before parsing.
- failure callbacks require the same HMAC headers as success callbacks.
- authenticated generation/upload failure callbacks from `submitted` update RateLimiter to `Failed(reason)`, release the reserved upload destination when possible, redact encrypted identity, and transition the context to `failed`.

- [ ] **Step 2: Write failing idempotency/concurrency tests**

Tests must cover:

- success callback from `submitted` claims row and transitions to `uploaded`/`draft_creating`.
- duplicate success after `complete` returns `200` without mutation.
- two concurrent success callbacks cannot both create a draft.
- terminal failure callback after `stale_failed` returns `409`.
- mismatched principal, `request_id`, or object key returns `409`.
- `202` is returned when another handler already claimed the row.

- [ ] **Step 3: Write failing refresh endpoint tests**

Tests must cover:

- valid refresh request returns a fresh scoped URL.
- invalid HMAC returns `401`.
- unknown or mismatched `request_id` returns `409`.
- refresh validates request key, principal, `request_id`, and object key against context.

- [ ] **Step 4: Run tests to verify they fail**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test --test videogen_completion
```

Expected: failures because endpoints do not exist.

- [ ] **Step 5: Implement completion endpoint**

Implement `POST /api/v2/videogen/complete` with:

- raw-body SHA-256 verification
- HMAC verification before JSON parsing or state mutation
- 120 second timestamp skew by default
- transaction or atomic claim using `SELECT ... FOR UPDATE` or `UPDATE ... WHERE state ... RETURNING`
- request key/principal/`request_id`/object-key validation
- success flow: `submitted -> uploaded -> draft_creating -> draft_created -> complete`
- failure flow: `submitted -> failed`
- terminal conflict policy exactly as the spec states
- generation/upload failure side effects: RateLimiter `Failed(reason)`, reserved upload destination release when possible, encrypted identity redaction, no draft creation attempt

Completion step wording must be sequential:

1. transition to `draft_creating`
2. call draft metadata service
3. transition to `draft_created`
4. call RateLimiter `Complete(bucket_url)`
5. transition to `complete`

- [ ] **Step 6: Implement refresh endpoint**

Implement `POST /api/v2/videogen/upload-url/refresh` with the same HMAC scheme and small body limit. Include this route only when refresh is enabled by config.

- [ ] **Step 7: Run tests**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test --test videogen_completion
cargo test
```

Expected: completion, refresh, and full suite pass.

- [ ] **Step 8: Commit**

```bash
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service add src/main.rs src/routes/videogen src/videogen tests/videogen_completion.rs
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service commit -m "feat: add videogen completion callbacks"
```

### Task 7: Add Prakash Reconciliation And Retention Cleanup

**Files:**
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/reconcile.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/main.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/context.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/tests/videogen_completion.rs`

- [ ] **Step 1: Write failing reconciliation tests**

Cover:

- `context_created` timeout marks RateLimiter failed, decrements usage, releases upload destination, redacts identity.
- RateLimiter canister unavailable leaves the row in pre-terminal state, does not bump `updated_at`, and does not increment reconciliation attempts.
- `submitted` stale releases upload destination and redacts identity after RateLimiter failure succeeds.
- batch size limits processing to `VIDEOGEN_RECONCILIATION_BATCH_SIZE` per state.
- `uploaded` timeout starts draft creation.
- `draft_creating` timeout retries then marks `draft_failed`.
- `draft_created` timeout retries RateLimiter `Complete(bucket_url)` and transitions to `complete`.
- draft-failed orphan cleanup marks video/object key for deletion after retention.

- [ ] **Step 2: Run tests to verify they fail**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test reconciliation
```

Expected: failures because reconciler does not exist.

- [ ] **Step 3: Implement reconciler**

Implement a periodic task started from `run_server()` with `VIDEOGEN_RECONCILIATION_INTERVAL_SECS`.

Rules:

- process max `VIDEOGEN_RECONCILIATION_BATCH_SIZE` stale rows per state per run.
- never terminalize Postgres before the required RateLimiter call succeeds.
- canister-unavailable skips update `last_reconciliation_error`/metrics only.
- uploaded and draft_creating use draft retry backoff.
- terminal states are absorbing unless an explicit operator tool later reopens them.

- [ ] **Step 4: Run tests**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test reconciliation
cargo test
```

Expected: reconciliation and full suite pass.

- [ ] **Step 5: Commit**

```bash
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service add src/main.rs src/videogen/reconcile.rs src/videogen/context.rs tests/videogen_completion.rs
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service commit -m "feat: reconcile videogen completion contexts"
```

### Task 8: Add Provider Compatibility, Metrics, And Contract Tests

**Files:**
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/routes/videogen/providers.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/routes/videogen/mod.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/main.rs`
- Create or modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/metrics.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/tests/videogen_contract.rs`

- [ ] **Step 1: Write failing provider contract tests**

Contract tests must deserialize Prakash responses using copied/imported mobile DTO structures equivalent to `GenerateVideoDtos.kt`, not only compare hard-coded JSON strings.

Cover:

- `/api/v2/videogen/providers`
- `/api/v2/videogen/providers-all`
- generate success response
- `VideoGenError` compatible bodies for auth, invalid input, NSFW, rate limit, and provider unavailable

- [ ] **Step 2: Write failing metrics smoke test**

Ensure these metric names are emitted or registered:

- `videogen_generate_requests_total`
- `videogen_generate_duration_ms`
- `videogen_ansuman_requests_total`
- `videogen_vast_submit_total`
- `videogen_completion_callbacks_total`
- `videogen_completion_hmac_failures_total`
- `videogen_contexts_by_state`
- `videogen_reconciliation_actions_total`
- `videogen_draft_creation_total`

- [ ] **Step 3: Run tests to verify they fail**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test --test videogen_contract
```

Expected: failures because provider endpoints/metrics are incomplete.

- [ ] **Step 4: Implement provider endpoints**

Return the same envelope and item shape as off-chain. `providers` returns production-available migrated providers. `providers-all` returns the same schema and may include internal/unavailable providers with flags set.

- [ ] **Step 5: Add structured metrics/logging**

Do not log raw prompt, image payload, delegated identity, upload URL, or bucket credentials.

- [ ] **Step 6: Run tests**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test --test videogen_contract
cargo test
```

Expected: contract and full suite pass.

- [ ] **Step 7: Commit**

```bash
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service add src/main.rs src/routes/videogen src/videogen tests/videogen_contract.rs
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service commit -m "feat: add videogen provider contracts and metrics"
```

## Vast LTX Tasks

### Task 9: Add Vast Submission Contract And Auth

**Files:**
- Modify: `/Users/prk-jr/Desktop/work/dolr/videogen/Cargo.toml`
- Modify: `/Users/prk-jr/Desktop/work/dolr/videogen/src/config.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/videogen/src/backend/mod.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/videogen/src/routes/generate.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/videogen/src/auth.rs`

- [ ] **Step 1: Write failing contract tests**

In `/Users/prk-jr/Desktop/work/dolr/videogen/tests/prakash_completion.rs`, test:

- `GenerateRequest` accepts top-level Prakash fields.
- `request_id` is required and no `job_id` field is used.
- duplicate `request_id` acceptance does not queue a second generation.
- missing/invalid bearer auth rejects before queueing.
- acceptance response echoes `request_id`.

- [ ] **Step 2: Run tests to verify they fail**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/videogen
cargo test --test prakash_completion
```

Expected: failures because the request shape/auth are not implemented.

- [ ] **Step 3: Extend request/response DTOs**

Replace the migrated Vast submission DTO with top-level Prakash fields. Keep `input` only for LTX model input:

```rust
pub struct GenerateRequest {
    pub request_id: String,
    pub request_key: RequestKey,
    pub user_principal: String,
    pub model_id: String,
    pub callback_url: String,
    #[serde(default)]
    pub upload_url_refresh_url: Option<String>,
    pub upload_destination: UploadDestination,
    pub input: LtxInput,
    pub workflow_json: serde_json::Value,
}

pub struct LtxInput {
    pub prompt: String,
    #[serde(default)]
    pub image_url: Option<String>,
}
```

Keep legacy `webhook` only for old flows if still needed; migrated Prakash flow should use the new completion fields.

Update `src/backend/comfyui/mod.rs` to read `request.request_id`, not `request.input.request_id`, and to read `request.input.image_url` for migrated requests. If legacy nested requests must remain supported temporarily, model them as an explicit `LegacyGenerateRequest` or compatibility enum so the new Prakash JSON contract stays top-level.

- [ ] **Step 4: Enforce Vast inbound auth**

`AUTH_TOKEN`/`VAST_API_KEY` must be required for production/staging. Reject invalid credentials before queueing work.

- [ ] **Step 5: Run tests**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/videogen
cargo test --test prakash_completion
cargo test
```

Expected: contract and full suite pass.

- [ ] **Step 6: Commit**

```bash
git -C /Users/prk-jr/Desktop/work/dolr/videogen add Cargo.toml Cargo.lock src/config.rs src/backend/mod.rs src/routes/generate.rs src/auth.rs tests/prakash_completion.rs
git -C /Users/prk-jr/Desktop/work/dolr/videogen commit -m "feat: accept prakash videogen submissions"
```

### Task 10: Add Vast Output Path Resolution, Upload, And Refresh

**Files:**
- Create: `/Users/prk-jr/Desktop/work/dolr/videogen/src/output_file.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/videogen/src/upload_destination.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/videogen/src/backend/comfyui/client.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/videogen/src/backend/comfyui/mod.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/videogen/src/config.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/videogen/tests/upload_destination.rs`

- [ ] **Step 1: Write failing upload tests**

Cover:

- output local path resolves from `COMFYUI_OUTPUT_DIR`, `subfolder`, and `filename`.
- path traversal in `filename` or `subfolder` is rejected.
- Vast checks `expires_at` before upload.
- if fewer than `VIDEOGEN_VAST_UPLOAD_EXPIRY_REFRESH_MARGIN_SECS=300` remain and refresh URL is present, Vast refreshes before upload.
- if upload returns expiry-compatible `403`, Vast refreshes once and retries.
- if refresh URL is absent, Vast does not construct one.

- [ ] **Step 2: Run tests to verify they fail**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/videogen
cargo test --test upload_destination
```

Expected: failures because upload helpers do not exist.

- [ ] **Step 3: Implement upload helpers**

Implement scoped upload via `reqwest::put(upload_url).body(file_bytes)` or streaming file body if available. Store refreshed `expires_at` in the job/outbox metadata.

Do not delete the local MP4 on upload failure.

- [ ] **Step 4: Run tests**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/videogen
cargo test --test upload_destination
cargo test
```

Expected: upload tests and full suite pass.

- [ ] **Step 5: Commit**

```bash
git -C /Users/prk-jr/Desktop/work/dolr/videogen add src/output_file.rs src/upload_destination.rs src/backend/comfyui src/config.rs tests/upload_destination.rs
git -C /Users/prk-jr/Desktop/work/dolr/videogen commit -m "feat: upload generated videos to prakash destinations"
```

### Task 11: Add Vast HMAC Completion Outbox

**Files:**
- Create: `/Users/prk-jr/Desktop/work/dolr/videogen/src/prakash_completion.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/videogen/src/outbox.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/videogen/src/config.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/videogen/src/main.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/videogen/src/backend/comfyui/mod.rs`
- Modify or replace: `/Users/prk-jr/Desktop/work/dolr/videogen/src/webhook.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/videogen/tests/prakash_completion.rs`

- [ ] **Step 1: Write failing signer/outbox tests**

Cover:

- signer creates the exact HMAC headers Prakash expects.
- success and failure callbacks use the same signing path.
- outbox record is written after bucket upload and before first callback attempt.
- outbox replays on startup.
- `200`, `202`, and `409` mark terminal.
- timeout/network/`5xx` retry with exponential backoff: initial 10s, cap 120s, max 10, total within 900s.
- signing happens at send time so key rotation affects retries.

- [ ] **Step 2: Run tests to verify they fail**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/videogen
cargo test --test prakash_completion
```

Expected: failures because signer/outbox do not exist.

- [ ] **Step 3: Implement completion module**

Use config:

- `VIDEOGEN_COMPLETION_HMAC_KEYS`
- `VIDEOGEN_COMPLETION_HMAC_ACTIVE_KEY_ID`
- `VIDEOGEN_VAST_CALLBACK_MAX_RETRIES=10`
- `VIDEOGEN_VAST_CALLBACK_INITIAL_BACKOFF_SECS=10`
- `VIDEOGEN_VAST_CALLBACK_MAX_BACKOFF_SECS=120`
- `VIDEOGEN_COMPLETION_RETRY_GRACE_SECS=900`
- `VIDEOGEN_VAST_OUTBOX_RETENTION_HOURS=72`

Persist outbox records in a durable local store available on Vast restart. If the repo already has a configured data directory, use it. Otherwise add an explicit `VIDEOGEN_VAST_OUTBOX_PATH` file path and use atomic write/rename semantics.

- [ ] **Step 4: Wire ComfyUI monitor**

On success:

1. resolve output path
2. upload to destination
3. persist outbox record
4. delete local generated MP4
5. send Prakash completion

On generation/upload failure:

1. persist failure outbox record when enough request metadata exists
2. send signed failure completion
3. keep output file on upload failure

- [ ] **Step 5: Run tests**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/videogen
cargo test --test prakash_completion
cargo test
```

Expected: completion/outbox tests and full suite pass.

- [ ] **Step 6: Commit**

```bash
git -C /Users/prk-jr/Desktop/work/dolr/videogen add src/prakash_completion.rs src/outbox.rs src/config.rs src/main.rs src/backend/comfyui src/webhook.rs tests/prakash_completion.rs
git -C /Users/prk-jr/Desktop/work/dolr/videogen commit -m "feat: add prakash completion outbox"
```

### Task 12: Add Vast Local Cleanup For Migrated Jobs

**Files:**
- Modify: `/Users/prk-jr/Desktop/work/dolr/videogen/src/cleanup.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/videogen/src/backend/comfyui/mod.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/videogen/src/config.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/videogen/tests/upload_destination.rs`

- [ ] **Step 1: Write failing cleanup tests**

Cover:

- successful migrated job deletes generated MP4 only after upload success and durable outbox persistence.
- upload failure does not delete generated MP4.
- staged I2V input images are removed after terminal generation state.
- orphan staged images older than `VIDEOGEN_VAST_STAGED_IMAGE_TTL_HOURS=24` are cleaned.
- staged image TTL must be greater than generation timeout in config validation.

- [ ] **Step 2: Run tests to verify they fail**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/videogen
cargo test cleanup
```

Expected: failures until cleanup is implemented.

- [ ] **Step 3: Implement cleanup**

Keep existing output TTL cleanup as fallback. Add migrated-job cleanup on success and staged input cleanup by TTL.

- [ ] **Step 4: Run tests**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/videogen
cargo test cleanup
cargo test
```

Expected: cleanup and full suite pass.

- [ ] **Step 5: Commit**

```bash
git -C /Users/prk-jr/Desktop/work/dolr/videogen add src/cleanup.rs src/backend/comfyui/mod.rs src/config.rs tests/upload_destination.rs
git -C /Users/prk-jr/Desktop/work/dolr/videogen commit -m "feat: clean up migrated ltx artifacts"
```

## off-chain-agent Tasks

### Task 13: Gate Legacy Off-Chain LTX Submission

**Files:**
- Modify: `/Users/prk-jr/Desktop/work/dolr/off-chain-agent/src/videogen/handlers_v2.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/off-chain-agent/src/consts.rs` or the repo's env config module.
- Test: existing off-chain videogen handler tests or new focused test file near the existing videogen tests.

- [ ] **Step 1: Write failing tests**

Cover:

- when `OFFCHAIN_LTX_SUBMIT_ENABLED=false`, new LTX V2 generate requests are rejected with a clear service-disabled/provider error.
- non-LTX providers still route normally.
- existing `/comfyui/webhook`, `/qstash/process_video_gen`, and `/qstash/video_gen_callback` routes remain available for legacy in-flight jobs.

- [ ] **Step 2: Run tests to verify they fail**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/off-chain-agent
cargo test videogen
```

Expected: LTX gate tests fail before config exists.

- [ ] **Step 3: Implement gate**

Add an env-backed boolean defaulting to `true` until rollout switch. Apply it only to the old LTX submit path. Do not delete legacy callback code yet.

- [ ] **Step 4: Run tests**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/off-chain-agent
cargo test videogen
```

Expected: gate tests pass.

- [ ] **Step 5: Commit**

```bash
git -C /Users/prk-jr/Desktop/work/dolr/off-chain-agent add src/videogen/handlers_v2.rs src/consts.rs
git -C /Users/prk-jr/Desktop/work/dolr/off-chain-agent commit -m "feat: gate legacy ltx submissions"
```

### Task 14: Add Off-Chain Legacy Drain Runbook

**Files:**
- Create: `/Users/prk-jr/Desktop/work/dolr/off-chain-agent/docs/videogen-ltx-drain-runbook.md`

- [ ] **Step 1: Write the runbook**

Include:

- how to list old QStash LTX messages.
- how to verify `/comfyui/webhook` legacy callbacks are still accepted.
- how to query RateLimiter for old `Pending`/`Processing` LTX requests.
- how to decide old jobs are drained.
- how to switch `OFFCHAIN_LTX_SUBMIT_ENABLED=false`.
- rollback requirement: callback URLs and draft handling must move back together if traffic is moved back to off-chain.

- [ ] **Step 2: Verify docs render**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/off-chain-agent
git diff --check
```

Expected: no whitespace errors.

- [ ] **Step 3: Commit**

```bash
git -C /Users/prk-jr/Desktop/work/dolr/off-chain-agent add docs/videogen-ltx-drain-runbook.md
git -C /Users/prk-jr/Desktop/work/dolr/off-chain-agent commit -m "docs: add ltx drain runbook"
```

## Cross-Repo Integration And Rollout Tasks

### Task 15: Reconfirm External Contracts Before Staging

**Repos:** all three.

- [ ] **Step 1: Verify Task 0 decisions are recorded**

Before staging, re-read the Task 0 decisions. If Task 0 was skipped, stop here and complete it before deploying Prakash or Vast to a shared environment.

- [ ] **Step 2: Reconfirm upload service URL capability**

Confirm the upload service can issue scoped upload URLs valid for at least `VIDEOGEN_UPLOAD_URL_TTL_SECS=4200`.

If not, Prakash Task 6 refresh endpoint is required for staging and Vast Task 10 must call it based on `expires_at`.

- [ ] **Step 3: Reconfirm draft service idempotency**

Confirm the upload metadata/draft service deduplicates on `video_id` or an agreed idempotency key. If it does not, add that guard before enabling completion endpoint in staging.

- [ ] **Step 4: Reconfirm Vast duplicate submit semantics**

Confirm duplicate `request_id` submissions return the same accepted response and do not start another generation.

- [ ] **Step 5: Reconfirm workflow contract**

Confirm the deployed Vast build expects the same `workflow_json` contract implemented by Prakash.

- [ ] **Step 6: Record differences**

Update the spec or add an implementation note only if staging-confirmed behavior differs from Task 0:

- upload URL TTL capability result
- draft idempotency guarantee
- Vast duplicate submit behavior
- workflow JSON ownership

- [ ] **Step 7: Commit docs update if needed**

```bash
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service add docs/superpowers/specs/2026-05-27-lean-videogen-migration-design.md docs/superpowers/plans/2026-05-27-lean-videogen-migration-implementation.md
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service commit -m "docs: record videogen external contract decisions"
```

### Task 16: Local End-To-End Test With Mocks

**Repos:** Prakash and Vast.

- [ ] **Step 1: Start mock dependencies**

Use mock HTTP servers for:

- Ansuman
- RateLimiter wrapper if canister integration is not available locally
- upload destination service
- draft metadata service
- Prakash completion endpoint when testing Vast alone

- [ ] **Step 2: Run Prakash integration suite**

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test --test videogen_generate --test videogen_completion --test videogen_contract
```

Expected: all tests pass.

- [ ] **Step 3: Run Vast integration suite**

```bash
cd /Users/prk-jr/Desktop/work/dolr/videogen
cargo test --test prakash_completion --test upload_destination
```

Expected: all tests pass.

- [ ] **Step 4: Run full suites**

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test
cd /Users/prk-jr/Desktop/work/dolr/videogen
cargo test
cd /Users/prk-jr/Desktop/work/dolr/off-chain-agent
cargo test videogen
```

Expected: all suites pass.

### Task 17: Staging Rollout

**Repos:** all three.

- [ ] **Step 1: Deploy Prakash endpoints behind config**

Completion endpoint must not be enabled without HMAC verification. Generate endpoint must be disabled or protected until Vast is deployed with matching submit contract and callback signing.

- [ ] **Step 2: Deploy Vast with Prakash callback support**

Configure:

- `AUTH_TOKEN` or `VAST_API_KEY`
- `VIDEOGEN_COMPLETION_HMAC_KEYS`
- `VIDEOGEN_COMPLETION_HMAC_ACTIVE_KEY_ID`
- outbox path
- callback retry values

- [ ] **Step 3: Run staging smoke**

Submit:

- safe text-to-video
- safe image-to-video
- NSFW prompt/image
- rate-limited user
- forced Vast generation failure
- forced upload failure
- duplicate completion callback
- forced Prakash restart before callback replay

Expected:

- safe jobs become drafts and RateLimiter `Complete(bucket_url)`.
- NSFW does not create RateLimiter request.
- rate-limited request does not submit to Vast.
- generation/upload failures mark RateLimiter failed.
- duplicate callbacks do not create duplicate drafts.
- Vast outbox replays after restart.

- [ ] **Step 4: Monitor required metrics**

Check:

- `videogen_completion_hmac_failures_total`
- `videogen_contexts_by_state`
- `videogen_reconciliation_actions_total`
- `videogen_vast_outbox_pending`
- generation success rate
- `submitted` and `draft_creating` accumulation

### Task 18: Production Cutover And Rollback Rules

**Repos:** all three.

- [ ] **Step 1: Prepare rollback**

Keep off-chain LTX path available until production confidence is established. Rollback must move both submission target and callback/draft handling back to off-chain together.

- [ ] **Step 2: Define rollback triggers**

Rollback if any of these sustain beyond the agreed window:

- HMAC failures spike above baseline.
- `submitted` stale failures exceed the launch threshold.
- draft creation failures exceed launch threshold.
- Vast outbox backlog grows continuously.
- user-facing generation success rate falls below threshold.

- [ ] **Step 3: Switch mobile videogen base URL or gateway routing**

Route mobile videogen calls to Prakash with the same params and verify provider endpoints are compatible.

- [ ] **Step 4: Drain off-chain**

Use `/Users/prk-jr/Desktop/work/dolr/off-chain-agent/docs/videogen-ltx-drain-runbook.md`.

- [ ] **Step 5: Disable off-chain LTX submit**

Set `OFFCHAIN_LTX_SUBMIT_ENABLED=false` after drain. Keep callbacks until no legacy in-flight jobs remain.

- [ ] **Step 6: Final verification**

Run:

```bash
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service status --short
git -C /Users/prk-jr/Desktop/work/dolr/videogen status --short
git -C /Users/prk-jr/Desktop/work/dolr/off-chain-agent status --short
```

Expected: only intentional deployment/config changes remain.

## Final Verification Before PRs

Run these before requesting code review:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test
cargo clippy --all-targets --all-features -- -D warnings

cd /Users/prk-jr/Desktop/work/dolr/videogen
cargo test
cargo clippy --all-targets --all-features -- -D warnings

cd /Users/prk-jr/Desktop/work/dolr/off-chain-agent
cargo test videogen
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: all tests pass and clippy reports no warnings.
