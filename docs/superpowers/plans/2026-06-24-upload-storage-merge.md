# Upload-Service → Storage-Service Merge — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Absorb the entire `yral-video-upload-service` (3 HTTP endpoints + IC orchestration) into `storj-interface`, then decommission it — one repo, one binary, one deploy, no circular HTTP coupling.

**Architecture:** Port the 3 public routes into a new `src/routes/upload/` module on storage's existing axum app (public, no HMAC). Reuse storage's existing `ic_agent` (identities are the same — spec D2). Then internalize the two cross-process hops: videogen→update-metadata (Phase 2) and the storj finalize self-hop (Phase 3). Finally wire deploy secrets and cut over `upload.yral.com`.

**Tech Stack:** Rust 2021, axum 0.8, ic-agent 0.41, candid 0.10, reqwest 0.12, utoipa 5, `yral-canisters-client` (feature `full`), `yral_types::delegated_identity::DelegatedIdentityWire`.

**Spec:** `docs/superpowers/specs/2026-06-24-upload-storage-merge-design.md` (rev 3). Read it first.

**Source of truth for ported code:** `/Users/prk-jr/Desktop/work/dolr/yral-video-upload-service` (the verbatim handlers being moved).

---

## Conventions & ground rules

- **Code baseline:** storage `main` (line refs in spec/plan are approximate locators — symbol names are authoritative; `grep` to re-anchor).
- **TDD:** every behavioral change starts with a failing test. Reuse storage's existing test patterns (`#[cfg(test)] mod tests`, `#[tokio::test]`, the `FakeCompletionDeps` style in `complete.rs:631`).
- **No `unwrap()` on env at startup** (spec D9) — tolerant load, log + disable routes if a secret is missing.
- **Commit after every passing task.** Conventional commits (`feat:`, `test:`, `refactor:`, `ci:`, `chore:`).
- **Auth:** the 3 routes are PUBLIC — register with NO `authorize` layer. Body-level delegated-identity check is the only auth (preserve verbatim).
- **Reuse, don't duplicate:** use the shared `yral_types::delegated_identity::DelegatedIdentityWire` (NOT upload's local copy). Reuse storage's `ic_agent` and `PUBLIC_BASE_URL`.

---

## File structure (what gets created / changed)

**New module `src/routes/upload/` (cohesive — files that change together):**
- `src/routes/upload/mod.rs` — module decl + `UploadState` (the upload-specific shared deps) + tolerant constructor.
- `src/routes/upload/types.rs` — `ApiResponse<T>`, `EmptyResp`, `AppError` (+ `IntoResponse`, status map, `From` impls), `RequestPostDetails`.
- `src/routes/upload/get_upload_url.rs` — `GET-equivalent POST /get-upload-url` handler + req/resp structs.
- `src/routes/upload/update_video_metadata.rs` — `POST /update-video-metadata` + `UpdateMetadataRequest` + `update_metadata_impl` (the reusable core).
- `src/routes/upload/mark_post_as_published.rs` — `POST /mark-post-as-published`.
- `src/routes/upload/events.rs` — `EventService` (offchain events client).
- `src/routes/upload/notification.rs` — `NotificationClient` + `NotificationType`.
- `src/routes/upload/storj_finalize.rs` — Phase-1 finalize-via-HTTP helper (`finalize_url` + `finalize_via_http`); retired in Phase 3.
- `src/routes/upload/test_support.rs` — `#[cfg(test)]` delegated-identity wire fixture (ported from upload).

**Modified:**
- `Cargo.toml` — pin `yral-canisters-client`/`yral-types` to `aa5abf3e`; confirm `features=["full"]`.
- `src/main.rs` — add `mod routes;` already exists → add `pub mod upload;` under routes; extend `AppState` with `events_service` + `notification_client`; build them tolerantly; register 3 routes + swagger.
- `src/consts.rs` — add 2 env-key consts (`OFFCHAIN_EVENTS_API_TOKEN`, `YRAL_METADATA_NOTIFICATION_SERVICE_API_TOKEN`).
- `src/routes/mod.rs` — `pub mod upload;`.
- `src/routes/videogen/complete.rs` — Phase 2: `RuntimeCompletionDeps` gains upload deps; `create_draft` calls in-process.
- `src/videogen/draft.rs` — Phase 2: add in-process `DraftServiceClient` impl (or repoint factory).
- `src/routes/upload/update_video_metadata.rs` + `src/routes/duplicate.rs` — Phase 3: finalize in-process.
- Deploy: `.github/workflows/deploy-prakash-servers.yml`, `.github/workflows/deploy-preview.yml`, `deploy/docker-compose.ha.yml`, `README.md`, `.env.example`.

---

# PHASE 0 — Dependency pin + compile guard

*Goal: lock the canister-client rev and prove the generated symbols resolve. No behavior change.*

### Task 0.1: Pin yral-common rev

**Files:** Modify `Cargo.toml`

- [ ] **Step 1:** In `Cargo.toml`, change the `yral-canisters-client` and `yral-types` git deps from `branch = "master"` to `rev = "aa5abf3e"` (the rev `Cargo.lock` currently resolves). Keep `features = ["full"]` on `yral-canisters-client`.
- [ ] **Step 2:** Run `cargo update -p yral-canisters-client --precise aa5abf3e` (and same for `yral-types` if separately pinned) or `cargo build` to refresh `Cargo.lock`. Confirm `Cargo.lock` still shows `aa5abf3e`.
- [ ] **Step 3:** `cargo build` — expect success, no behavior change.
- [ ] **Step 4:** Commit.

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore: pin yral-canisters-client to aa5abf3e for upload merge"
```

### Task 0.2: Compile-guard test for canister symbols

**Files:** Create `tests/canister_symbols_guard.rs`

- [ ] **Step 1: Write the test** — references every generated symbol the ported code needs, so a future rev drift fails CI.

```rust
//! Compile-guard: proves the canister-client symbols the merged upload routes
//! depend on resolve under the pinned rev. Pure type/const references; no network.
#![allow(unused_imports, dead_code)]

use yral_canisters_client::ic::{USER_INFO_SERVICE_ID, USER_POST_SERVICE_ID};
use yral_canisters_client::user_info_service::{Result6, UserInfoService};
use yral_canisters_client::user_post_service::{
    PostDetailsFromFrontendV1, PostStatus, PostStatusFromFrontend, Result2, Result_, UserPostService,
};

#[test]
fn canister_symbols_resolve() {
    // Service IDs are consts — referencing them is enough to fail compile on drift.
    let _ = USER_INFO_SERVICE_ID;
    let _ = USER_POST_SERVICE_ID;
    // Touch the variant constructors / types so signatures are checked.
    let _draft = PostStatusFromFrontend::Draft;
    let _pub = PostStatusFromFrontend::Published;
    let _uploaded = PostStatus::Uploaded;
}
```

- [ ] **Step 2: Run it** — `cargo test --test canister_symbols_guard`. Expected: PASS. If it fails to compile, the pinned rev lacks a symbol → stop and reconcile rev before continuing (spec R1).
- [ ] **Step 3: Commit.**

```bash
git add tests/canister_symbols_guard.rs
git commit -m "test: compile-guard for canister-client symbols used by upload merge"
```

---

# PHASE 1 — Lift-and-shift the 3 routes (one binary)

*Goal: the 3 upload endpoints serve from storj-interface, public, swagger'd. Deploy alongside the still-running upload-service. No cutover yet.*

### Task 1.1: Add env-key consts + url-encoding dep

**Files:** Modify `src/consts.rs`, `Cargo.toml`

- [ ] **Step 1:** Add env-key consts near the other `pub const X: &str` entries. **Note (review B4):** `PUBLIC_BASE_URL` is NOT currently a const — it's read via `std::env::var("PUBLIC_BASE_URL")` in `generate.rs`. Add a const for it so the upload module shares one definition:

```rust
pub const OFFCHAIN_EVENTS_API_TOKEN: &str = "OFFCHAIN_EVENTS_API_TOKEN";
pub const YRAL_METADATA_NOTIFICATION_SERVICE_API_TOKEN: &str =
    "YRAL_METADATA_NOTIFICATION_SERVICE_API_TOKEN";
pub const PUBLIC_BASE_URL: &str = "PUBLIC_BASE_URL"; // env key; read tolerantly at call sites
```

- [ ] **Step 2 (review B1):** URL-encoding is needed in Tasks 1.7/1.8 but `url`/`urlencoding` are only transitive deps. Add a direct dep: `cargo add urlencoding`. (Alternatively use the already-direct `reqwest`'s `reqwest::Url::parse_with_params`, which percent-encodes — if you go that route, skip the `cargo add` and adjust 1.7/1.8.)
- [ ] **Step 3:** `cargo build`. Expected: PASS (unused-const warning OK — no `-Dwarnings`).
- [ ] **Step 4:** Commit. `git commit -am "chore: add upload env-key consts + urlencoding dep"`

### Task 1.2: Port the error envelope + types

**Files:** Create `src/routes/upload/types.rs`, `src/routes/upload/mod.rs`; Modify `src/routes/mod.rs`

- [ ] **Step 1: Write failing test** in `src/routes/upload/types.rs` (`#[cfg(test)] mod tests`):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn err_serializes_error_message_field_and_status() {
        let r: ApiResponse<EmptyResp> = AppError::Unauthorized("nope".into()).to_api_response();
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"error_message\":\"Unauthorized: nope\""));
        assert!(!json.contains("status_code")); // skip_serializing
        assert_eq!(r.status_code, 403);
    }
    #[test]
    fn ok_wraps_data() {
        let r: ApiResponse<u32> = Ok::<_, AppError>(7u32).into();
        assert!(r.success && r.data == Some(7) && r.status_code == 200);
    }
}
```

- [ ] **Step 2: Run** `cargo test -p storj-interface routes::upload::types` → FAIL (module not found).
- [ ] **Step 3: Implement** `src/routes/upload/types.rs` — port verbatim from `yral-video-upload-service/src/utils/types.rs`: `AppError` enum, `status_code()`, `to_api_response()`, `From` impls (`AgentError`, `candid::Error`, `PrincipalError`, `Box<dyn Error>`, `serde_json::Error`), `ApiResponse<T>` (with `#[serde(skip_serializing, default)] status_code: u16`), `EmptyResp`, the `IntoResponse` impl, the `From<Result<T,AppError>>`, and `RequestPostDetails` (+ `From<PostDetailsFromFrontendV1>`). **Prune** the dead `From<Result<T, Box<dyn Error>>>` if unused.
  - **(review C2 — orphan rule):** Use `yral_types::delegated_identity::DelegatedIdentityWire`. Do NOT port upload's local `DelegatedIdentityWire` NOR its hand-written `ToSchema`/`PartialSchema` block — the shared type already `#[derive(ToSchema)]`, and you cannot impl `ToSchema` for a foreign type anyway. Register the shared type's derived schema in Task 1.10.
  - **(review C1 — error type):** the shared wire's `TryFrom<DelegatedIdentityWire> for DelegatedIdentity` has `type Error = k256::elliptic_curve::Error` (NOT `Box<dyn Error>`). So the `From<Box<dyn Error>> for AppError` impl will NOT auto-fire on identity conversion. Keep the explicit `.map_err(|e| AppError::InvalidDelegatedIdentity(e.to_string()))` at every `DelegatedIdentity::try_from(...)` call site (Tasks 1.7, 1.9, 2.1).
- [ ] **Step 4:** Create `src/routes/upload/mod.rs` with `pub mod types;` (other submodules added later). Add `pub mod upload;` to `src/routes/mod.rs`.
- [ ] **Step 5: Run** the test → PASS.
- [ ] **Step 6: Commit.** `git commit -am "feat(upload): port ApiResponse/AppError envelope + RequestPostDetails"`

### Task 1.2b: Port the delegated-identity test fixture (review B2 — REQUIRED before 1.7/1.9)

**Files:** Create `src/routes/upload/test_support.rs` (gated `#[cfg(test)]`); Modify `mod.rs`

Storage has **no** helper to build a valid signed `DelegatedIdentityWire`. `identity_crypto.rs:90` uses a hardcoded JSON whose `sender()` is meaningless — it cannot drive the `sender() == creator_principal` (403) tests. The real builder is upload's `create_delegated_identity_wire` (`yral-video-upload-service/src/utils/types.rs:285-322`).

- [ ] **Step 1:** Port `create_delegated_identity_wire(...) -> (DelegatedIdentityWire, Principal)` into `test_support.rs` under `#[cfg(test)]`, producing a wire **plus** the principal its `.sender()` resolves to (so tests can assert match/mismatch). It needs `k256::SecretKey::random`, `pkcs8::EncodePublicKey`, `k256::elliptic_curve::rand_core::OsRng`, `ic_agent::Identity::sign_delegation`.
- [ ] **Step 2:** Verify required crate features: `k256` needs `jwk` (present) + `pkcs8`/`std` for `EncodePublicKey`/`OsRng`; add via `cargo add k256 --features jwk,pkcs8,std` if a feature is missing. `cargo build --tests`.
- [ ] **Step 3:** Add `#[cfg(test)] pub mod test_support;` to `mod.rs`. Write a smoke test: `let (wire, p) = create_delegated_identity_wire(); assert_eq!(DelegatedIdentity::try_from(wire).unwrap().sender().unwrap(), p);` → PASS.
- [ ] **Step 4: Commit.** `git commit -am "test(upload): port delegated-identity wire fixture"`

### Task 1.3: Port EventService

**Files:** Create `src/routes/upload/events.rs`; Modify `src/routes/upload/mod.rs`

- [ ] **Step 1: Write failing test** — assert the offchain body shape (the stringified-`params` quirk, spec §8):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn event_body_params_is_a_json_string() {
        // Build the params object exactly as send_video_upload_successful_event does,
        // assert params serializes to a STRING, not a nested object.
        let params = serde_json::json!({"video_id":"v","is_nsfw":false,"is_hot_or_not":true}).to_string();
        let body = serde_json::json!({"event":"video_upload_successful","params": params});
        assert!(body["params"].is_string());
    }
}
```

- [ ] **Step 2: Run** → FAIL (module missing).
- [ ] **Step 3: Implement** `events.rs` — port `EventService` verbatim (base `https://offchain.yral.com/`, bearer default-header, `send_video_upload_successful_event`, `send_video_event_unsuccessful`). Keep `params = json!(...).to_string()` and the `is_nsfw=false` / `is_hot_or_not=true` literals. Add `pub mod events;` to `mod.rs`.
- [ ] **Step 4: Run** → PASS. `cargo build`.
- [ ] **Step 5: Commit.** `git commit -am "feat(upload): port offchain EventService"`

### Task 1.4: Port NotificationClient

**Files:** Create `src/routes/upload/notification.rs`; Modify `mod.rs`

- [ ] **Step 1: Write failing test** — `NotificationType` serde tag + Display title:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use candid::Principal;
    #[test]
    fn notification_serializes_with_type_tag() {
        let n = NotificationType::VideoPublished { user_principal: Principal::anonymous(), post_id: "p".into() };
        let j = serde_json::to_value(&n).unwrap();
        assert_eq!(j["type"], "VideoPublished");
        assert!(n.to_string().contains("published"));
    }
}
```

- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement** `notification.rs` — port `NotificationClient` (`https://metadata.yral.com`, bearer), `NotificationType` (serde `tag="type"`), `Notification`/`NotificationInfo`, `Display`. Keep `send_notification` awaited-but-result-ignored (log + `sentry::capture_message` on error). Add `pub mod notification;`.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit.** `git commit -am "feat(upload): port metadata NotificationClient"`

### Task 1.5: UploadState + tolerant constructor (spec D9)

**Files:** Modify `src/routes/upload/mod.rs`

- [ ] **Step 1: Write failing test** — constructor returns `None`/disabled when tokens missing, `Some` when present:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn upload_state_disabled_when_token_missing() {
        // With no env tokens, from_env() yields None (routes will 503), never panics.
        // (Use a helper that reads explicit args rather than real env to keep test hermetic.)
        assert!(UploadState::build(None, None).is_none());
        assert!(UploadState::build(Some("a".into()), Some("b".into())).is_some());
    }
}
```

- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement** in `mod.rs`:

```rust
use crate::routes::upload::{events::EventService, notification::NotificationClient};

#[derive(Clone)]
pub struct UploadState {
    pub events_service: EventService,
    pub notification_client: NotificationClient,
}

impl UploadState {
    /// Tolerant build: returns None if either token is absent (routes then 503; never panics).
    pub fn build(events_token: Option<String>, notif_token: Option<String>) -> Option<Self> {
        let (e, n) = (events_token?, notif_token?);
        Some(Self {
            events_service: EventService::with_auth_token(e),
            notification_client: NotificationClient::new(n),
        })
    }
    pub fn from_env() -> Option<Self> {
        Self::build(
            std::env::var(crate::consts::OFFCHAIN_EVENTS_API_TOKEN).ok(),
            std::env::var(crate::consts::YRAL_METADATA_NOTIFICATION_SERVICE_API_TOKEN).ok(),
        )
    }
}
```

- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit.** `git commit -am "feat(upload): tolerant UploadState constructor"`

### Task 1.6: Wire UploadState into AppState

**Files:** Modify `src/main.rs`

- [ ] **Step 1:** Add field to `AppState` (`src/main.rs:42-60`): `pub upload: Option<std::sync::Arc<routes::upload::UploadState>>,`
- [ ] **Step 2:** In the `AppState { .. }` construction (`~:265-279`), add `upload: routes::upload::UploadState::from_env().map(std::sync::Arc::new),` and log a warning if `None`:

```rust
let upload = routes::upload::UploadState::from_env().map(std::sync::Arc::new);
if upload.is_none() {
    tracing::warn!("upload routes disabled: OFFCHAIN_EVENTS_API_TOKEN / YRAL_METADATA_NOTIFICATION_SERVICE_API_TOKEN not set");
}
```

- [ ] **Step 3:** `cargo build` → PASS.
- [ ] **Step 4: Commit.** `git commit -am "feat(upload): add UploadState to AppState (tolerant)"`

### Task 1.7a: Finalize-via-HTTP helper (review B3)

**Files:** Create `src/routes/upload/storj_finalize.rs`; Modify `mod.rs`

Upload's verbatim `update_metadata_impl` calls `StorjInterface::finalize_upload` — a reqwest POST to `/duplicate_raw/finalize`. That client is NOT being ported wholesale; write just the one helper it needs. (Phase 3 replaces this with an in-process call.)

- [ ] **Step 1: Write failing test** — URL + query encoding:

```rust
#[test]
fn finalize_url_encodes_query() {
    let u = finalize_url("https://x.test", "p abc", "v1", false);
    assert!(u.starts_with("https://x.test/duplicate_raw/finalize?"));
    assert!(u.contains("publisher_user_id=p%20abc") && u.contains("is_nsfw=false"));
}
```

- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement** `finalize_url(base, publisher, video_id, is_nsfw) -> String` (encoded, mirrors `yral-video-upload-service/src/utils/storj_interface.rs:90-103`) + `async fn finalize_via_http(base, publisher, video_id, is_nsfw, metadata: HashMap<String,String>) -> Result<(), AppError>` using the direct `reqwest` dep, body `{"metadata": metadata}`, mapping non-2xx → `AppError::StorageError`. Add `pub mod storj_finalize;`.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit.** `git commit -am "feat(upload): finalize-via-HTTP helper (Phase-1 self-hop)"`

### Task 1.7b: `update_metadata_impl` core

**Files:** Create `src/routes/upload/update_video_metadata.rs`; Modify `mod.rs`

The reusable core (called by the handler now; in-process in Phase 2). Holds `AppState` so Phase 3 can swap finalize to in-process without a signature change.

- [ ] **Step 1: Write failing tests** (use the Task 1.2b fixture):
  - `sender_mismatch_returns_403`: `UpdateMetadataRequest` whose `delegated_identity_wire.sender() != post_details.creator_principal` → `AppError::Unauthorized`.
  - `meta_gets_post_details_injected`: after injection, `meta["post_details"]` equals the JSON of `RequestPostDetails`.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement** `UpdateMetadataRequest { delegated_identity_wire, meta: HashMap<String,String>, post_details: PostDetailsFromFrontendV1 }` (manual deser; for swagger see C2 note) and:
  - `async fn update_metadata_impl(state: &AppState, events: &EventService, notif: &NotificationClient, req) -> Result<(), AppError>` — body: `DelegatedIdentity::try_from(wire).map_err(|e| AppError::InvalidDelegatedIdentity(e.to_string()))` → `sender()` check (403 on mismatch) → inject `meta["post_details"]` → **finalize**: Phase 1 calls `storj_finalize::finalize_via_http(&public_base_url(), &publisher, &req.post_details.id, false, req.meta.clone())` where `public_base_url()` reads `std::env::var(consts::PUBLIC_BASE_URL)` → `UserPostService(USER_POST_SERVICE_ID, &state.ic_agent).add_post_v_1(post_details)` (returns bare `Result_::Ok` — no payload) → on Ok fire `events.send_video_upload_successful_event(...)` (Published only) + `notif.send_notification(...)`; on Err fire `send_video_event_unsuccessful`. Keep `is_nsfw=false`, `enable_hot_or_not=true` literals.
- [ ] **Step 4: Run** → PASS. `cargo build`.
- [ ] **Step 5: Commit.** `git commit -am "feat(upload): update_metadata_impl core (holds AppState)"`

### Task 1.7c: `update-video-metadata` axum handler

**Files:** Modify `src/routes/upload/update_video_metadata.rs`

- [ ] **Step 1: Implement** the handler returning `ApiResponse<()>` (its `IntoResponse` drives the public route):

```rust
#[utoipa::path(post, path = "/update-video-metadata", request_body = UpdateMetadataRequest, responses(
    (status = 200, body = ApiResponse<EmptyResp>), (status = 401), (status = 500)))]
pub async fn update_video_metadata(
    State(state): State<AppState>,
    Json(req): Json<UpdateMetadataRequest>,
) -> ApiResponse<()> {
    let Some(upload) = state.upload.clone() else {
        return AppError::InternalError("upload routes disabled (missing tokens)".into()).to_api_response();
    };
    update_metadata_impl(&state, &upload.events_service, &upload.notification_client, req)
        .await
        .into()
}
```

- [ ] **Step 2:** `cargo build && cargo test routes::upload::update_video_metadata` → PASS.
- [ ] **Step 3: Commit.** `git commit -am "feat(upload): update-video-metadata handler (503 when disabled)"`

### Task 1.8: Port `get_upload_url` handler

**Files:** Create `src/routes/upload/get_upload_url.rs`; Modify `mod.rs`

- [ ] **Step 1: Write failing test** — URL built from `PUBLIC_BASE_URL`, params encoded, `video_id` is a uuid:

```rust
#[test]
fn builds_upload_url_from_base() {
    let url = build_upload_url("https://x.test", "principal abc", "vid-1", false);
    assert!(url.starts_with("https://x.test/duplicate_raw/upload?"));
    assert!(url.contains("publisher_user_id=principal%20abc")); // encoded
    assert!(url.contains("is_nsfw=false"));
}
```

- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement** — `GetUploadUrlReq{publisher_user_id}` / `GetUploadUrlResp{upload_url, video_id}`. Handler: validate principal via `UserInfoService(USER_INFO_SERVICE_ID, &state.ic_agent).get_user_profile_details_v_6(principal)` (discard result, just validates existence), mint `Uuid::new_v4()`, read base from `std::env::var(consts::PUBLIC_BASE_URL)`, build URL via `build_upload_url(base, publisher, video_id, is_nsfw)` (encoded). `is_nsfw=false`. **(review N4)** upload's original `get_upload_url(video_id, publisher, is_nsfw)` puts video_id first — do NOT transpose; the query keys are `publisher_user_id=` and `video_id=`, map args to the right keys.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit.** `git commit -am "feat(upload): port get-upload-url handler"`

### Task 1.9: Port `mark_post_as_published` handler

**Files:** Create `src/routes/upload/mark_post_as_published.rs`; Modify `mod.rs`

- [ ] **Step 1: Write failing test** — sender-mismatch → 403 (mirror Task 1.7 pattern).
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement** — port verbatim: decode wire → `get_individual_post_details_by_id` → sender==creator check → `update_post_status(post_id, PostStatus::Uploaded)` → fire `video_upload_successful` event + `VideoPublished` notification. Use `state.ic_agent`, `state.upload` (503 if None).
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit.** `git commit -am "feat(upload): port mark-post-as-published handler"`

### Task 1.10: Register routes + swagger

**Files:** Modify `src/main.rs`

- [ ] **Step 1:** In the `Router::new()` chain (`~:292`), add 3 PUBLIC routes (NO `authorize` layer), each `.with_state(app_state.clone())`. **Do NOT add a second `/health`.**

```rust
.route("/get-upload-url", post(routes::upload::get_upload_url::get_upload_url).with_state(app_state.clone()))
.route("/update-video-metadata", post(routes::upload::update_video_metadata::update_video_metadata).with_state(app_state.clone()))
.route("/mark-post-as-published", post(routes::upload::mark_post_as_published::mark_post_as_published).with_state(app_state.clone()))
```

- [ ] **Step 2 (review M1):** the ported handlers already carry their `#[utoipa::path(...)]` + req/resp `#[derive(ToSchema)]` (done in Tasks 1.7c/1.8/1.9). Here you ONLY register them: add the 3 paths to `#[openapi(paths(...))]` and add `ApiResponse`, `UpdateMetadataRequest`, `GetUploadUrlReq/Resp`, `MarkPostAsPublishedRequest`, and the shared `DelegatedIdentityWire` (its derived schema — C2) to `components(schemas(...))` (`src/main.rs:62-158`).
- [ ] **Step 2b (review M4 — body limit, spec R9):** leave the 3 routes at axum's default 2 MB body limit — the wire+meta JSON is small. Decision: no `DefaultBodyLimit` needed (unlike `/duplicate_raw/upload`). Confirm in code review.
- [ ] **Step 3:** `cargo build` → PASS. Run full suite `cargo test`.
- [ ] **Step 4:** Manual smoke (local): `cargo run`, then `curl -s localhost:3000/get-upload-url -d '{"publisher_user_id":"aaaaa-aa"}' -H 'content-type: application/json'` → expect a JSON envelope (will 400/canister-error without a real principal, but proves routing + public access).
- [ ] **Step 5: Commit.** `git commit -am "feat(upload): register 3 public routes + swagger"`

### Task 1.11: Integration test (hurl) + docs

**Files:** Create `tests/upload_routes.hurl` (match existing `e2e-tests.yml` style if present); Modify `README.md`, `.env.example`

- [ ] **Step 1:** Add hurl cases hitting the 3 routes, asserting the `{success,data,error_message}` envelope + status codes (use a stub/fake identity; assert 403 on sender mismatch).
- [ ] **Step 2:** Update `README.md` (new endpoints) + `.env.example` (the 2 new tokens). Do NOT copy upload's stale README.
- [ ] **Step 3:** `cargo build && cargo test`. Run hurl if wired in CI.
- [ ] **Step 4: Commit.** `git commit -am "test(upload): integration cases + docs for merged routes"`

**✅ Phase 1 done = mergeable. The binary now serves all 3 routes. Upload-service still runs in parallel (no cutover).**

---

# PHASE 2 — Internalize videogen → update-metadata (enables cutover)

*Goal: VAST completion registers the draft via an in-process call, not an HTTP POST to `upload.yral.com`. Removes storage's dependence on `VIDEOGEN_UPLOAD_SERVICE_URL`.*

### Task 2.1: In-process DraftServiceClient

**Files:** Modify `src/videogen/draft.rs`

- [ ] **Step 1: Write failing test** — an `InProcessDraftServiceClient` maps `DraftCreationRequest` → `UpdateMetadataRequest` per spec §8 (decrypt identity, `video_id`→`id`+`video_uid`, `user_principal`→`creator_principal`, `status=Draft`, empty hashtags/desc/meta) and invokes `update_metadata_impl`. Use a fake/mock for the canister + finalize (inject via the impl's deps). Assert the mapped request fields.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement** `InProcessDraftServiceClient` holding `{ state: AppState, events: EventService, notif: NotificationClient }` (review C4 — hold the whole `AppState`, which is `#[derive(Clone)]`, so Phase 3 can call `update_metadata_impl(&state, ...)` unchanged). Its `create_draft`:
  - if `encrypted_identity` is `None` → log + `Ok(())` (preserve current behavior, draft.rs:60-69);
  - decrypt via `IdentityCrypto::from_env()?.decrypt(blob)?` → `DelegatedIdentityWire`;
  - build `UpdateMetadataRequest { delegated_identity_wire, meta: {}, post_details: PostDetailsFromFrontendV1 { id: video_id.clone(), video_uid: video_id, creator_principal: Principal::from_text(user_principal)?, status: PostStatusFromFrontend::Draft, hashtags: vec![], description: String::new() } }`;
  - call `update_metadata_impl(&self.state, &self.events, &self.notif, req)`, map `AppError` → `DraftServiceError::Unavailable`.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit.** `git commit -am "feat(videogen): in-process draft client calling update_metadata_impl"`

### Task 2.2a: RuntimeCompletionDeps holds AppState (review C4)

**Files:** Modify `src/routes/videogen/complete.rs`

`RuntimeCompletionDeps` (`complete.rs:427`) currently stores extracted fields incl. `ic_agent` (moved from `state` at `:436`). To feed the in-process draft client (which needs the whole `AppState`), store `AppState` directly.

- [ ] **Step 1:** Change `RuntimeCompletionDeps` to hold `state: AppState` (clone in the constructor; `AppState` is `#[derive(Clone)]`). Replace internal `self.ic_agent` uses with `self.state.ic_agent` (lines `:466,486,502`). **Watch the partial-move:** clone what you need before any move, or just keep `state` and reference through it.
- [ ] **Step 2:** `cargo build && cargo test` → PASS (pure refactor, no behavior change).
- [ ] **Step 3: Commit.** `git commit -am "refactor(videogen): RuntimeCompletionDeps holds AppState"`

### Task 2.2b: create_draft goes in-process

**Files:** Modify `src/routes/videogen/complete.rs`

- [ ] **Step 1:** Change `RuntimeCompletionDeps::create_draft` (`:539-541`) to: if `self.state.upload` is `Some`, build `InProcessDraftServiceClient { state: self.state.clone(), events, notif }` and call it; else `LoggingDraftServiceClient` (disabled fallback). Remove `draft_client_from_env()` from the production path.
- [ ] **Step 2:** Keep `FakeCompletionDeps` (tests `:631`) compiling — its `create_draft` returns canned results, untouched.
- [ ] **Step 3:** `cargo build && cargo test` → PASS.
- [ ] **Step 4: Commit.** `git commit -am "refactor(videogen): register draft in-process (no HTTP hop)"`

### Task 2.3: Retire the HTTP draft client

**Files:** Modify `src/videogen/draft.rs`

- [ ] **Step 1 (review N3 — delete, don't deprecate):** `rg draft_client_from_env` to confirm Task 2.2b removed the last production caller, then **delete** `UpdateVideoMetadataDraftClient` + `draft_client_from_env()`. Deleting (not deprecating) is what kills the `VIDEOGEN_UPLOAD_SERVICE_URL` unset→`upload.yral.com` default for good. Keep `LoggingDraftServiceClient` as the disabled-fallback.
- [ ] **Step 2:** `cargo build` (warnings OK). `cargo test`.
- [ ] **Step 3: Commit.** `git commit -am "chore(videogen): remove HTTP draft client (replaced by in-process)"`

**✅ Phase 2 done = the videogen→upload hop is gone. Cutover is now safe (see Phase 4).**

---

# PHASE 3 — Internalize the storj finalize self-hop

*Goal: `update_metadata_impl`'s finalize step calls `handler_raw_finalize` logic directly instead of HTTP-POSTing to `{PUBLIC_BASE_URL}/duplicate_raw/finalize`.*

### Task 3.1: Extract a callable finalize core

**Files:** Modify `src/routes/duplicate.rs`

- [ ] **Step 1: Write failing test** — assert `handler_raw_finalize` delegates to a new `finalize_core(...)` (a true HTTP-free unit test isn't feasible — finalize does storj/uplink side-effects; the meaningful test is "handler is a thin wrapper over core", or an env-gated integration test). State which you're doing.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Refactor** `handler_raw_finalize` (`duplicate.rs:624`) to parse `RawFinalizeParams`/`RawFinalizeBody` then delegate to `finalize_core(&AppState, publisher_user_id, video_id, is_nsfw, metadata: BTreeMap<String,String>) -> Result<(), Error>`. Handler becomes a thin wrapper. (Note: body type is `BTreeMap`, not `HashMap` — spec §8 wording is loose here.)
- [ ] **Step 4: Run** → PASS. `cargo test`.
- [ ] **Step 5: Commit.** `git commit -am "refactor(duplicate): extract finalize_core from handler_raw_finalize"`

### Task 3.2: Call finalize_core from update_metadata_impl

**Files:** Modify `src/routes/upload/update_video_metadata.rs`

- [ ] **Step 1:** `update_metadata_impl` already takes `&AppState` (Task 1.7b / C4) — no signature change. Just replace the `storj_finalize::finalize_via_http(...)` call with `finalize_core(state, &publisher, &req.post_details.id, false, req.meta.clone().into_iter().collect())` (`HashMap` → `BTreeMap` via `.into_iter().collect()`).
- [ ] **Step 2:** Delete `src/routes/upload/storj_finalize.rs` and its `mod` decl (no longer used). `cargo build && cargo test` → PASS.
- [ ] **Step 3: Commit.** `git commit -am "feat(upload): finalize in-process (remove self-HTTP hop)"`

**✅ Phase 3 done = zero internal HTTP hops. Both circular edges collapsed.**

---

# PHASE 4 — Deploy, cutover, decommission

*Ops-coordinated. Reference spec §10 + S9. Line refs approximate.*

### Task 4.1: Add the 2 secrets to all deploy sites

**Files:** Modify `.github/workflows/deploy-prakash-servers.yml`, `.github/workflows/deploy-preview.yml`, `deploy/docker-compose.ha.yml`

- [ ] **Step 1: Vault** — write `OFFCHAIN_EVENTS_API_TOKEN` + `YRAL_METADATA_NOTIFICATION_SERVICE_API_TOKEN` to `secret/data/yral-video-storage-service/`.
- [ ] **Step 2: prakash-servers.yml** — add both to the Vault read-block (`~:152-154`) AND the SSH `export X='${X}';` block (`~:178-203`).
- [ ] **Step 3: preview.yml** — add, for each secret: a Vault read-block line (`~:122-128`), a `--arg foo "$FOO"` jq binding (`~:326-331`), and a Coolify payload entry `{"key":"…","value":$foo,"is_literal":true}` (`~:335-360`).
- [ ] **Step 4: docker-compose.ha.yml** — add both to the `environment:` block (`~:106-135`), AND add `PUBLIC_BASE_URL: ${PUBLIC_BASE_URL}` (missing there — spec D8).
- [ ] **Step 5: Commit.** `git commit -am "ci: inject upload-service secrets + PUBLIC_BASE_URL across deploy targets"`

### Task 4.2: Disable the legacy draft HTTP env (post-Phase-2)

**Files:** Modify both workflows + ha compose

- [ ] **Step 1:** Since Phase 2 removed the HTTP draft client, delete the hardcoded `VIDEOGEN_UPLOAD_SERVICE_URL='https://upload.yral.com'` lines (prakash `:193`, preview `:353`) and the ha-compose default (`:125`).
- [ ] **Step 2: Commit.** `git commit -am "ci: drop VIDEOGEN_UPLOAD_SERVICE_URL (draft now in-process)"`

### Task 4.3: Preview smoke test (validates spec R2)

- [ ] **Step 1:** Deploy to preview. With a **real delegated identity**, run a full publish: `/get-upload-url` → upload bytes to `/duplicate_raw/upload` → `/update-video-metadata`.
- [ ] **Step 2:** Confirm `add_post_v_1` **succeeds** with storage's reused `ic_agent` identity (proves identities-are-same, R2). Confirm offchain event + notification delivered.
- [ ] **Step 3:** If `add_post_v_1` is rejected → STOP; identities are not equivalent after all → revisit spec D2/OQ1 with ops.

### Task 4.4: Cutover `upload.yral.com`

- [ ] **Step 1 (OQ6 — ops):** Repoint `upload.yral.com` ingress/DNS → storage service (reconcile the 3 hostnames: `upload.yral.com`, `storj-interface.yral.com`, `storage-interface.prakash.yral.com`). Preserve route paths so the frontend needs no change.
- [ ] **Step 2:** Keep upload-service running in parallel during soak.
- [ ] **Step 3: Soak** N hours — watch Sentry (project `/7`), `add_post_v_1` success rate, event/notification latency.
- [ ] **Rollback (if needed):** revert DNS/ingress to upload-service (still running). DNS-only, no data migration.

### Task 4.5: Decommission upload-service

- [ ] **Step 1:** After soak: tear down upload-service — its `docker-compose` service, Coolify app, `docker-publish.yml`, and DNS record.
- [ ] **Step 2:** Confirm no remaining references to `upload.yral.com` in storage repo (`rg upload.yral.com`).
- [ ] **Step 3: Commit** any cleanup. `git commit -am "chore: decommission yral-video-upload-service"`

**✅ Phase 4 done = upload-service discarded. Goal met.**

---

## Open items (do not block Phase 0–3; needed for Phase 4)

- **OQ6:** Who owns the `upload.yral.com` cutover, and which hostname does the frontend hit? (Infra.)
- Confirm the frontend calls these endpoints same-origin or relies on storage's permissive CORS (spec S4c) — verify before cutover.

## Test strategy summary

- Phase 0: compile-guard (permanent).
- Phase 1: unit (envelope, event body, notification tag, sender-mismatch 403, URL build) + hurl integration + manual curl.
- Phase 2: in-process draft mapping + complete.rs no-outbound-HTTP.
- Phase 3: finalize_core callable without HTTP.
- Phase 4: real-publish preview smoke (the R2 gate).
- Build green under edition 2021 (warnings not CI-gated — dead-code pruning is hygiene).
