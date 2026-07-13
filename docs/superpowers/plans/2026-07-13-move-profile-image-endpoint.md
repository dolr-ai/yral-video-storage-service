# Move `/profile-image` Endpoint Into This Service — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Serve `POST /profile-image` and `DELETE /profile-image` from `yral-video-storage-service` with the exact wire contract off-chain-agent uses today, so clients migrate by base-URL only (additive dual-run).

**Architecture:** New `routes/user` module. Handlers verify the in-body delegated identity (chain-checked), process + upload the image to Hetzner Object Storage (bucket `yral-profile`, `aws_sdk_s3`), then update `user_info_service.profile_picture_url` using a per-request user agent built from the wire. Responses are bare JSON (`{ profile_image_url }` / `(StatusCode, String)`), matching off-chain-agent — NOT this repo's `ApiResponse` envelope.

**Tech Stack:** Rust, axum, `aws_sdk_s3`, `image` 0.25, `ic-agent`, `yral-canisters-client`.

**Spec:** `docs/superpowers/specs/2026-07-13-move-profile-image-endpoint-design.md`

**Source to port (read first):** `../off-chain-agent/src/user/profile_image.rs`, `../off-chain-agent/src/utils/s3.rs`.

---

## File Structure

- Create `src/routes/user/mod.rs` — module exports + route registration helper.
- Create `src/routes/user/profile_s3.rs` — S3 config, client, `process_image`, upload/delete/delete-prior. Ported from off-chain-agent `utils/s3.rs`.
- Create `src/routes/user/profile_image.rs` — the two handlers (bare-JSON contract).
- Modify `src/routes/upload/auth.rs` — add `verified_identity` returning the built `DelegatedIdentity`.
- Modify `src/routes/mod.rs` — add `pub mod user;`.
- Modify `src/main.rs` — register the two routes.
- Modify `Cargo.toml` — add `image = "0.25"`.
- Modify `deploy/docker-compose.ha.yml`, `.github/workflows/deploy-prakash-servers.yml`, `.env.example`, `readme.md` — `PROFILE_S3_*` config.

---

## Task 0: Dependency gate (BLOCKING — do first)

The spec's F2 risk. This repo pins `yral-canisters-client` at yral-common rev `55e7ec1d`; the canister write needs `update_profile_details` + `ProfileUpdateDetails { profile_picture_url, bio, website_url }` + `Result_`.

**Files:** `Cargo.toml`, `Cargo.lock` (only if a bump is needed).

- [ ] **Step 1: Probe the pinned client for the required API.** Add a temporary compile probe.

Create `src/bin/_probe_profile.rs`:
```rust
fn main() {
    use yral_canisters_client::user_info_service::ProfileUpdateDetails;
    let _ = ProfileUpdateDetails {
        profile_picture_url: Some(String::new()),
        bio: None,
        website_url: None,
    };
}
```

- [ ] **Step 2: Compile the probe.**

Run: `cargo check --bin _probe_profile`
- **PASS** → the API exists at the current rev. Delete the probe (`rm src/bin/_probe_profile.rs`), skip Step 3, continue to Task 1.
- **FAIL** (unresolved import / field mismatch) → do Step 3.

- [ ] **Step 3 (only if FAIL): Bump `yral-canisters-client`.** Update the `yral-canisters-client` (and any coupled `yral-common`) git rev in `Cargo.toml` to a rev that defines `update_profile_details` (use off-chain-agent's rev `b207047b` as a known-good reference; prefer the latest `main`). Run `cargo update -p yral-canisters-client`, then `cargo check` the whole workspace and fix any fallout at the existing `user_info_service` call sites (`src/routes/upload/get_upload_url.rs`, `update_video_metadata.rs`, `mark_post_as_published.rs` — method/enum names like `get_user_profile_details_v_6`/`Result6` may shift). Re-run the probe until it passes, then delete it.

- [ ] **Step 4: Commit.**
```bash
git add Cargo.toml Cargo.lock
git commit -m "chore: ensure yral-canisters-client exposes update_profile_details"
```
(If no bump was needed, skip the commit.)

---

## Task 1: Add `image` dep + profile S3 config

**Files:**
- Modify: `Cargo.toml`
- Create: `src/routes/user/mod.rs`
- Create: `src/routes/user/profile_s3.rs`
- Modify: `src/routes/mod.rs`

- [ ] **Step 1: Add the dependency.** In `Cargo.toml` `[dependencies]`, add (default features — decoders for jpeg/png/webp/gif are required to decode user uploads):
```toml
image = "0.25"
```

- [ ] **Step 2: Create the module.** `src/routes/user/mod.rs`:
```rust
pub mod profile_image;
pub mod profile_s3;
```
Add `pub mod user;` to `src/routes/mod.rs`.

- [ ] **Step 3: Write the config + key-format tests (failing).** In `src/routes/user/profile_s3.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_key_uses_prefix_principal_timestamp() {
        let cfg = ProfileS3Config {
            key_prefix: "users/".into(),
            ..ProfileS3Config::test_defaults()
        };
        let key = cfg.object_key("aaaaa-aa", 1_700_000_000);
        assert_eq!(key, "users/aaaaa-aa/profile-1700000000.jpg");
    }

    #[test]
    fn object_prefix_for_user_targets_only_that_user() {
        let cfg = ProfileS3Config::test_defaults();
        assert_eq!(cfg.user_prefix("aaaaa-aa"), "users/aaaaa-aa/profile-");
    }
}
```

- [ ] **Step 4: Run — verify fail.** Run: `cargo test -p storj-interface profile_s3::tests -- --nocapture`. Expected: FAIL (types not defined).

- [ ] **Step 5: Implement config + client + processing.** Port from `../off-chain-agent/src/utils/s3.rs`, with these deltas: env names `PROFILE_S3_*`; defaults `yral-profile` / `users/` / `https://yral-profile.hel1.your-objectstorage.com`; creds from the existing `HETZNER_S3_ACCESS_KEY`/`HETZNER_S3_SECRET_KEY`, endpoint/region from `HETZNER_S3_ENDPOINT`/`HETZNER_S3_REGION`; add `image::Limits` before decode (F6). Structure:
```rust
use aws_config::{BehaviorVersion, Region};
use aws_sdk_s3::{config::Credentials, primitives::ByteStream, types::ObjectCannedAcl, Client};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use image::{DynamicImage, ImageFormat, ImageReader};
use std::io::Cursor;

pub struct ProfileS3Config {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub key_prefix: String,
    pub public_url_base: String,
}

impl ProfileS3Config {
    pub fn from_env() -> Self { /* std::env::var with the documented defaults */ }
    #[cfg(test)]
    pub fn test_defaults() -> Self { /* hardcoded defaults, no env */ }
    pub fn object_key(&self, principal: &str, ts: u64) -> String {
        format!("{}{}/profile-{}.jpg", self.key_prefix, principal, ts)
    }
    pub fn user_prefix(&self, principal: &str) -> String {
        format!("{}{}/profile-", self.key_prefix, principal)
    }
}

pub async fn create_client(cfg: &ProfileS3Config) -> Result<Client, String> { /* creds from HETZNER_S3_ACCESS_KEY/SECRET_KEY; endpoint/region from cfg */ }

/// Decode (bounded), resize <=1000px Lanczos3, RGB8, JPEG q85. Ported verbatim
/// from off-chain-agent process_image, plus image::Limits (max ~4096x4096).
pub fn process_image(image_bytes: Vec<u8>) -> Result<Vec<u8>, String> { /* ... */ }

pub async fn upload_profile_image(cfg: &ProfileS3Config, client: &Client, image_base64: &str, principal: &str) -> Result<String, String> {
    let bytes = BASE64.decode(image_base64).map_err(|e| format!("decode base64: {e}"))?;
    let processed = process_image(bytes)?;
    // F4: best-effort delete prior objects BEFORE writing the new one
    let _ = delete_profile_images(cfg, client, principal).await;
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|e| e.to_string())?.as_secs();
    let key = cfg.object_key(principal, ts);
    client.put_object().bucket(&cfg.bucket).key(&key)
        .body(ByteStream::from(processed)).content_type("image/jpeg")
        .acl(ObjectCannedAcl::PublicRead).send().await
        .map_err(|e| format!("put_object: {e}"))?;
    Ok(format!("{}/{}", cfg.public_url_base, key))
}

pub async fn delete_profile_images(cfg: &ProfileS3Config, client: &Client, principal: &str) -> Result<(), String> { /* list user_prefix, delete each — ported */ }
```
For `process_image`, copy off-chain-agent's body exactly (resize threshold 1000, Lanczos3, `to_rgb8`, JPEG q85) but decode via `ImageReader` with limits:
```rust
let mut reader = ImageReader::new(Cursor::new(&image_bytes)).with_guessed_format().map_err(|e| e.to_string())?;
let mut limits = image::Limits::default();
limits.max_image_width = Some(4096);
limits.max_image_height = Some(4096);
reader.limits(limits);
let img = reader.decode().map_err(|e| format!("decode image: {e}"))?;
```

- [ ] **Step 6: Run — verify pass.** Run: `cargo test -p storj-interface profile_s3::tests`. Expected: PASS.

- [ ] **Step 7: Add a `process_image` test (multi-format + resize).** Feed a tiny generated PNG and a tiny JPEG (build in-test with `image`), assert output decodes as JPEG and is ≤1000px. Run and pass.

- [ ] **Step 8: Commit.**
```bash
git add Cargo.toml Cargo.lock src/routes/user/mod.rs src/routes/user/profile_s3.rs src/routes/mod.rs
git commit -m "feat: profile-image S3 storage helper (Hetzner yral-profile)"
```

---

## Task 2: `verified_identity` in auth.rs

**Files:**
- Modify: `src/routes/upload/auth.rs`
- Test: same file (`#[cfg(test)]`)

- [ ] **Step 1: Write the failing test.** Add to `auth.rs` tests:
```rust
#[test]
fn verified_identity_returns_identity_and_sender() {
    let (wire, expected) = signed_wire_with_sender();
    let (identity, sender) = verified_identity(&wire).expect("verified");
    assert_eq!(sender, expected);
    assert_eq!(identity.sender().unwrap(), expected);
}
```

- [ ] **Step 2: Run — verify fail.** Run: `cargo test -p storj-interface auth::tests::verified_identity_returns_identity_and_sender`. Expected: FAIL (function missing).

- [ ] **Step 3: Implement.** Refactor so the chain-verified build is shared:
```rust
/// Reconstruct the delegated identity WITH chain verification; return it plus its sender.
pub fn verified_identity(wire: &DelegatedIdentityWire) -> Result<(DelegatedIdentity, Principal), AppError> {
    let to_secret = k256::SecretKey::from_jwk(&wire.to_secret)
        .map_err(|e| AppError::InvalidDelegatedIdentity(e.to_string()))?;
    let to_identity = Secp256k1Identity::from_private_key(to_secret);
    let identity = DelegatedIdentity::new(
        wire.from_key.clone(),
        Box::new(to_identity),
        wire.delegation_chain.clone(),
    ).map_err(|e| AppError::InvalidDelegatedIdentity(e.to_string()))?;
    let sender = identity.sender().map_err(AppError::InvalidDelegatedIdentity)?;
    Ok((identity, sender))
}
```
Reimplement `verified_sender` as `verified_identity(wire).map(|(_, s)| s)`. Drop the now-unneeded `#[allow(dead_code)]` on the used path.

- [ ] **Step 4: Run — verify pass.** Run: `cargo test -p storj-interface auth::tests`. Expected: PASS (all, incl. `forged_from_key_is_rejected`).

- [ ] **Step 5: Commit.**
```bash
git add src/routes/upload/auth.rs
git commit -m "feat: verified_identity returns built DelegatedIdentity for user-agent calls"
```

---

## Task 3: Handlers (`profile_image.rs`)

**Files:**
- Create/extend: `src/routes/user/profile_image.rs`

Handlers replicate off-chain-agent EXACTLY: signature `Result<impl IntoResponse, (StatusCode, String)>`, success body `Json(UploadProfileImageResponse { profile_image_url })`, no `AppState` (principal comes from `verified_identity`). Build a per-request user agent: `Agent::builder().with_url(consts::IC_URL.as_str()).with_identity(identity).build()` — no `fetch_root_key` (mainnet).

- [ ] **Step 1: Request/response types + a validation unit test (failing).**
```rust
#[derive(serde::Deserialize, utoipa::ToSchema)]
pub struct UploadProfileImageRequest { pub delegated_identity_wire: DelegatedIdentityWire, pub image_data: String }
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct UploadProfileImageResponse { pub profile_image_url: String }
#[derive(serde::Deserialize, utoipa::ToSchema)]
pub struct DeleteProfileImageRequest { pub delegated_identity_wire: DelegatedIdentityWire }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strips_data_url_prefix() { assert_eq!(strip_data_url("data:image/png;base64,QUJD"), "QUJD"); }
    #[test]
    fn rejects_oversized() { assert!(validate_base64_len(&"x".repeat(7*1024*1024 + 1)).is_err()); }
}
```

- [ ] **Step 2: Run — verify fail.** Run: `cargo test -p storj-interface profile_image::tests`. Expected: FAIL.

- [ ] **Step 3: Implement helpers + POST handler.** Port off-chain-agent's `handle_upload_profile_image` flow: `strip_data_url`, `validate_base64_len` (≤ `7*1024*1024`, non-empty, decodable), `verified_identity` → `(identity, principal)`, `ProfileS3Config::from_env()` + `create_client`, `upload_profile_image`, then user agent + `UserInfoService(USER_INFO_SERVICE_ID, &agent).update_profile_details(ProfileUpdateDetails { profile_picture_url: Some(url.clone()), bio: None, website_url: None })`. Error mapping mirrors off-chain-agent status codes: 400 bad/oversized/undecodable, 500 upload/agent, 403 canister "not authorized", 500 other canister/`Err`. Return `Json(UploadProfileImageResponse { profile_image_url: url })`.

- [ ] **Step 4: Implement DELETE handler.** `verified_identity` → principal + identity; `delete_profile_images`; then **F5**: build user agent + `update_profile_details` clearing the URL (use the canister API's clear representation confirmed in Task 0 — e.g. `Some(String::new())`). Return `StatusCode::OK`.

- [ ] **Step 5: Run — verify pass.** Run: `cargo test -p storj-interface profile_image::tests`. Expected: PASS.

- [ ] **Step 6: Commit.**
```bash
git add src/routes/user/profile_image.rs
git commit -m "feat: /profile-image upload + delete handlers (off-chain-agent contract)"
```

---

## Task 4: Register routes

**Files:** Modify `src/main.rs` (router block near line 349–362).

- [ ] **Step 1: Add routes.** After the `/mark-post-as-published` route:
```rust
.route(
    "/profile-image",
    post(routes::user::profile_image::handle_upload_profile_image)
        .delete(routes::user::profile_image::handle_delete_profile_image)
        .layer(DefaultBodyLimit::max(8 * 1024 * 1024)), // ~5MB image + base64 overhead
)
```
(No `.with_state` — handlers take only `Json`.)

- [ ] **Step 2: Build.** Run: `cargo build -p storj-interface`. Expected: compiles.

- [ ] **Step 3: Commit.**
```bash
git add src/main.rs
git commit -m "feat: register POST/DELETE /profile-image"
```

---

## Task 5: Config wiring

**Files:** `deploy/docker-compose.ha.yml`, `.github/workflows/deploy-prakash-servers.yml`, `.env.example`, `readme.md`.

- [ ] **Step 1: docker-compose.** In the `storj-interface` service `environment:`, add:
```yaml
      PROFILE_S3_BUCKET: ${PROFILE_S3_BUCKET:-yral-profile}
      PROFILE_S3_KEY_PREFIX: ${PROFILE_S3_KEY_PREFIX:-users/}
      PROFILE_S3_PUBLIC_URL_BASE: ${PROFILE_S3_PUBLIC_URL_BASE:-https://yral-profile.hel1.your-objectstorage.com}
```
(The `HETZNER_S3_*` creds are already passed.)

- [ ] **Step 2: deploy workflow.** Add the three (with defaults) to the job `env:` and export them in the SSH block, mirroring the existing `BACKUP_S3_*` wiring.

- [ ] **Step 3: `.env.example` + readme.** Document `PROFILE_S3_BUCKET` / `PROFILE_S3_KEY_PREFIX` / `PROFILE_S3_PUBLIC_URL_BASE` (defaults + the ⚠️ never-`prakash-yral` note for the fallback).

- [ ] **Step 4: Commit.**
```bash
git add deploy/docker-compose.ha.yml .github/workflows/deploy-prakash-servers.yml .env.example readme.md
git commit -m "chore: wire PROFILE_S3_* config for profile-image"
```

---

## Task 6: Integration tests (env-gated)

**Files:** `src/routes/user/profile_s3.rs` (`#[cfg(test)]`, `#[ignore]`), mirroring `duplicate.rs`'s `hetzner_both_thumbnail_names_uploaded`.

- [ ] **Step 1: Write ignored round-trip tests.** Gated on `PROFILE_S3_*` + `HETZNER_S3_*`: (a) upload a generated image → object exists at expected key with `content-type: image/jpeg`; (b) second upload removes the first (F4); (c) `delete_profile_images` clears all `users/<principal>/profile-*`. Use a random test principal to isolate.

- [ ] **Step 2: Run against a test bucket.**
Run: `HETZNER_S3_ACCESS_KEY=… HETZNER_S3_SECRET_KEY=… PROFILE_S3_BUCKET=<test-bucket> cargo test -p storj-interface profile_s3 -- --ignored`
Expected: PASS.

- [ ] **Step 3: Commit.**
```bash
git add src/routes/user/profile_s3.rs
git commit -m "test: profile-image Hetzner upload/delete round-trip (ignored)"
```

---

## Task 7: Verify green + manual smoke

- [ ] **Step 1: Format + lint + test.**
Run: `cargo fmt --check && cargo clippy -- -D warnings && cargo test -p storj-interface`
Expected: clean.

- [ ] **Step 2: Manual smoke (local run).** Boot the service with `PROFILE_S3_*` + `HETZNER_S3_*` set; `POST /profile-image` with a real base64 image + a valid delegated-identity wire (reuse an e2e fixture) → 200 `{ profile_image_url }`; open the URL (public). `DELETE` → 200; URL 404s; canister `profile_picture_url` cleared. Follow @superpowers:verification-before-completion — paste the actual responses.

- [ ] **Step 3: Final commit if anything changed.**

---

## Notes for the implementer
- **Contract fidelity is the point.** Keep request/response shapes and status codes identical to off-chain-agent so web/mobile switch by base URL only. Do NOT wrap responses in this repo's `ApiResponse`.
- **Never point the fallback bucket at `prakash-yral`** (private DB-backup bucket). If `yral-profile` creds don't work, provision a new public bucket.
- **Out of scope:** deleting off-chain-agent's copy (#2107), GobGob default-avatar migration (#2066), rate limiting (F7).
