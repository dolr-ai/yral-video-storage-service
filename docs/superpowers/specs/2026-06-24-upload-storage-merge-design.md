# Upload-Service → Storage-Service Merge — Design Spec

**Date:** 2026-06-24
**Status:** rev 3 — locked decisions: (1) **identities are the same** → reuse storage's existing `ic_agent`, no new IC secret; (2) **upload-service is being discarded** → full decommission is the committed end state.
**Author:** Prakash (with Claude)
**Code baseline:** storage @ `4f079f9` (branch `prakash/media-jobs-observability`). Line refs are anchored here and drift with each merge — treat them as approximate locators, not exact addresses; the symbol names are authoritative.

---

## 1. Goal

**Discard `yral-video-upload-service` entirely** by absorbing its full surface (3 HTTP endpoints + IC canister orchestration) into `yral-video-storage-service` (`storj-interface`). Result: one repo, one binary, one deploy. Remove the circular service-to-service HTTP coupling. The upload service is **decommissioned** at the end — not maintained in parallel — so the merge must be complete (all 3 routes ported, both internalizations done); no half-measures.

**One sentence:** Fold the 1.3K-LOC stateless upload facade into the storage service it already calls (and which already calls it back), turning two cross-service HTTP hops into in-process calls.

---

## 2. Why (rationale + the coupling smell)

`upload-service` is a stateless facade: 3 endpoints, no DB, no background jobs, no heavy deps. It mostly forwards to `storage-interface` and to IC canisters.

**Circular dependency exists today:**
- `storage → upload`: `videogen/draft.rs` POSTs to `https://upload.yral.com/update-video-metadata` after VAST completes a generated video. Call site is `complete.rs:231` (`deps.create_draft(draft_req)`); the runtime impl `complete.rs:539-541` delegates to `draft_client_from_env()`.
- `upload → storage`: `StorjInterface` POSTs to `https://storage-interface.prakash.yral.com/duplicate_raw/{upload,finalize}` and builds upload URLs against it. **This storage base URL is hardcoded** in upload `main.rs:135` — there is no env var for it.

Two services call each other over HTTP. Merging collapses both hops to internal function calls. Deploy/CI/ops halves. No meaningful dependency or scaling cost — the merge direction is **upload INTO storage**, and storage already carries every heavy dependency (Postgres, RabbitMQ, ffmpeg, uplink, aws-sdk, IC agent).

---

## 3. Current state — verbatim ground truth

### 3.1 upload-service (source = merge)
- **Pkg:** `yral-video-upload-service`, **edition 2024**, bind `0.0.0.0:3000`.
- **Routes** (`src/main.rs`): the 3 below **plus `GET /health`** (`main.rs:152`) and SwaggerUI at `/explore`. All **public** (no axum auth layer; only Sentry tower layers). ⚠️ Both binaries bind `:3000` and define `/health` → on merge, drop upload's `/health` and add only the **3** business routes (see S4).
  - `POST /get-upload-url` → validates principal exists via `UserInfoService::get_user_profile_details_v_6`, returns `{upload_url, video_id}` where `upload_url = "{storage_base}/duplicate_raw/upload?publisher_user_id=&video_id=&is_nsfw=false"` and `video_id = Uuid::new_v4()`.
  - `POST /update-video-metadata` → reconstructs `DelegatedIdentity` from body, asserts `sender() == post_details.creator_principal` (else 403), injects `post_details` JSON into `meta["post_details"]`, calls `StorjInterface::finalize_upload` (HTTP POST to storage `/duplicate_raw/finalize`), then `UserPostService::add_post_v_1`, then fires offchain event (if Published) + metadata notification.
  - `POST /mark-post-as-published` → reconstructs `DelegatedIdentity`, fetches post via `get_individual_post_details_by_id`, asserts `sender() == creator_principal` (else 403), `update_post_status(post_id, PostStatus::Uploaded)`, fires event + `VideoPublished` notification.
- **AppState:** `{ storj_client: Arc<StorjInterface>, ic_admin_agent: Agent, events_service: EventService, notification_client: NotificationClient }`.
- **IC identity:** `Secp256k1Identity::from_pem(IC_ADMIN_PRIVATE_KEY)`, agent URL `https://ic0.app`. Delegated identity from request is used **only** to verify sender principal — canister calls are signed by the admin agent.
- **Canister IDs:** `USER_INFO_SERVICE_ID = ivkka-7qaaa-aaaas-qbg3q-cai`, `USER_POST_SERVICE_ID = gxhc3-pqaaa-aaaas-qbh3q-cai` (from `yral_canisters_client::ic::`, generated; features `user-post-service` + `user-info-service`).
- **External hosts (all hardcoded):** events `https://offchain.yral.com/api/v2/events` (Bearer `OFFCHAIN_EVENTS_API_TOKEN`), notifications `https://metadata.yral.com/notifications/{principal}/send` (Bearer `YRAL_METADATA_NOTIFICATION_SERVICE_API_TOKEN`), Sentry DSN project `/9`, IC `https://ic0.app`.
- **Env vars:** `IC_ADMIN_PRIVATE_KEY` (PEM — **NOT carried over**; same principal as storage's `BACKEND_ADMIN_IDENTITY`, D2), `OFFCHAIN_EVENTS_API_TOKEN` (carried), `YRAL_METADATA_NOTIFICATION_SERVICE_API_TOKEN` (carried), `APP_ENV` (Sentry env), `RUST_LOG`.
- **Notification call is awaited inline** (`update_video_metadata.rs:207`, `mark_post_as_published.rs:108`), result ignored (logs + Sentry on error). It is **not** spawned — it blocks the request on `metadata.yral.com` latency. (Despite "fire-and-forget" semantics, it is synchronous from the request's POV.)
- **Response envelope:** `ApiResponse<T> { success, data, error_message }` (note: `error_message`, not `message`; plus a `status_code: u16` field with `#[serde(skip_serializing)]` used only for the HTTP status). `AppError` enum maps to status codes (400/400/404/503/500/502/400/404/403/502/500).
- **Dep pin:** Cargo.toml does **not** hard-pin a rev (`version="0.1.0"`, no `rev`); lockfile resolves to yral-common `f83d1b2`.

### 3.2 storage-service (target = host)
- **Pkg:** `storj-interface`, **edition 2021**, bind `0.0.0.0:3000` (`main.rs:493`), lib `storj_interface`. `main.rs` is 613 lines.
- **Router** (`src/main.rs:292`–`491`): auth is **per-route** via `.layer(middleware::from_fn(authorize))` (HMAC-SHA256 over `METHOD\nPATH\nTIMESTAMP`, ±300s, key `SERVICE_SECRET_TOKEN`). Public routes simply omit the layer (e.g. `/api/v2/videogen/providers`, `/health` @ `main.rs:487`). **Adding public upload routes = register with no `authorize` layer.** Global layers: swagger, sentry request logger, permissive CORS (`GET/POST/OPTIONS`, any origin/header, `main.rs:282-285`), sentry tower. Graceful shutdown wired (`main.rs:512`) — ported routes inherit it (upload had none; net improvement).
- **AppState** (`src/main.rs:42-60`): already contains **`ic_agent: Agent`** built from `BACKEND_ADMIN_IDENTITY` PEM via tolerant `if let Ok(pem)` (`src/main.rs:251`) — missing identity = anonymous agent, **no panic**. URL `IC_URL` (default `https://ic0.app`).
- **Deps:** `yral-canisters-client { branch="master", features=["full"] }`, `yral-types`, `ic-agent 0.41`, `candid 0.10`, `axum 0.8`, `reqwest 0.12`, `stringreader`, `utoipa 5`, `sentry 0.42`, `thiserror 2`, `tokio full`. **CI gates `cargo clippy -- -D warnings` + `cargo fmt --check` + `cargo test` on every PR** (`deploy-preview.yml:46,49,52`) — so warnings DO fail the build and the test runner has no IC/storj/network. (Correction: an earlier rev said warnings weren't gated — they are.)
- **Dep pin:** yral-common rev `aa5abf3e` (branch=master, floating). **`PUBLIC_BASE_URL` env already exists** (`generate.rs:37,484`), injected in all deploy points as `https://storage-interface.prakash.yral.com` — reuse it (see D8).
- **The call to internalize:** `videogen/draft.rs` builds JSON `{delegated_identity_wire, meta:{}, post_details:{id, video_uid, creator_principal, status:"Draft", hashtags:[], description:""}}` and POSTs to `{VIDEOGEN_UPLOAD_SERVICE_URL}/update-video-metadata`. Default url resolves on **unset** to `https://upload.yral.com` (`unwrap_or_else`, `draft.rs:48-49`, `consts.rs:32`); only an **empty-string** value (`draft.rs:50` `is_empty()`) selects the no-op `LoggingDraftServiceClient`. Call fired from `complete.rs:231`. `encrypted_identity` is AES-256-GCM decrypted via `IdentityCrypto::from_env()` (key `INTERNAL_ENCRYPTION_SECRET`) into the identity wire.
- **finalize target:** `routes/duplicate.rs:624` `pub async fn handler_raw_finalize(...)` serves `/duplicate_raw/finalize`. Contract: query params `RawFinalizeParams {publisher_user_id, video_id, is_nsfw}` (`duplicate.rs:497`) + JSON body `RawFinalizeBody {metadata: HashMap}` (`duplicate.rs:499`).
- **Error model:** no central enum; per-module `thiserror` + `IntoResponse` (e.g. `duplicate.rs:251-281`) or `(StatusCode, Json(...))` tuples for v2 videogen endpoints.
- **Deploy:** single binary, `deploy/Dockerfile`, `fly.toml` (region sin) + bare-metal HA via `deploy/docker-compose.ha.yml`. Env injected at 3 points: `.github/workflows/deploy-prakash-servers.yml` (Vault → SSH `export`), `.github/workflows/deploy-preview.yml` (Coolify bulk env API), `deploy/docker-compose.ha.yml`. `BACKEND_ADMIN_IDENTITY` sourced from Vault `secret/data/yral-video-storage-service/BACKEND_ADMIN_IDENTITY`.

---

## 4. Surfaces that must change (complete inventory)

| # | Surface | Change |
|---|---------|--------|
| S1 | `Cargo.toml` deps | Reconcile yral-common rev; confirm `full` feature exposes `user_post_service`/`user_info_service`/`ic::USER_*_SERVICE_ID`; no new crates needed (axum/reqwest/ic-agent/candid/utoipa/stringreader all present). |
| S2 | Source modules | Port upload's `api/*`, `utils/{events_interface,notification_client,storj_interface,types}.rs` into storage under a new `src/upload/` (or `src/routes/upload/`) module tree. Adapt to storage's error/idiom conventions. |
| S3 | `AppState` | Add **only** `events_service` + `notification_client` (IC agent reused — D2). **No** new base-URL field — reuse existing `PUBLIC_BASE_URL` (D8). For Phase-2, `complete.rs`'s `RuntimeDeps` must reach the four `update_metadata_impl` borrows: the **existing** `ic_agent`, `events_service`, `notification_client`, and a storj finalize handle. |
| S4 | Router | Register 3 new **public** routes (no `authorize` layer). **Do NOT register a 2nd `/health`** — storage already has one (`main.rs:487`); drop upload's. Single `:3000` bind stays. Body limits: small JSON → default 2MB fine. |
| S4b | OpenAPI/utoipa | Upload handlers have utoipa annotations; storage's `ApiDoc` (`main.rs:62-158`, schemas open at `:103`) registers paths + schemas explicitly. Port `#[utoipa::path]` + register net-new schemas (`ApiResponse<T>`, `UpdateMetadataRequest`, `GetUploadUrlReq/Resp`, `MarkPostAsPublishedRequest`, `DelegatedIdentityWire`). Not a one-liner. |
| S4c | CORS | New routes inherit storage's global permissive CORS (`Any` origin, `POST/OPTIONS` allowed) — preflight on JSON POSTs works. Upload had **no** CORS today; confirm frontend calls are same-origin/server-side (then no-op) or genuinely cross-origin (then global CORS already covers it). Decision, not omission. |
| S5 | IC identity | Reuse storage's existing `ic_agent` (D2). No new agent, no `IC_ADMIN_PRIVATE_KEY`. |
| S6 | `consts.rs` | Use generated `ic::USER_*_SERVICE_ID` consts; reuse `PUBLIC_BASE_URL`; add the **2** new env-var name consts (events + notification tokens). |
| S7 | Internalize `storage→upload` (**THREE edges**) | (1) `draft.rs`→update-video-metadata (Phase 2); (2) `generate.rs:1101 reserve_upload_destination`→get-upload-url; (3) `upload_refresh.rs:322 generate_fresh_upload_url`→get-upload-url (both Phase 2.5). All three POST to `upload.yral.com` today via `VIDEOGEN_UPLOAD_SERVICE_DEFAULT_URL` — all must be internalized before decommission. edge (3) sends `{video_id}` while the in-repo handler reads `{publisher_user_id}`, but it **works in prod** (OQ7 resolved — deployed handler reconciles it; verify exact contract at impl time and preserve). Cutover owner: the merge team (OQ6 — nobody else owns it). |
| S8 | Internalize `upload→storage` finalize | Replace `StorjInterface::finalize_upload` HTTP POST with a direct call into `handler_raw_finalize`'s underlying logic (Phase 3). `get_upload_url` stays a URL builder (frontend uploads directly — cannot internalize). |
| S9 | Deploy secrets/env | **2** new secrets (`OFFCHAIN_EVENTS_API_TOKEN`, `YRAL_METADATA_NOTIFICATION_SERVICE_API_TOKEN`; no `IC_ADMIN_PRIVATE_KEY` — D2). Edit sites (line refs approximate, current `main`): **(a)** write both to Vault `secret/data/yral-video-storage-service/`. **(b) prakash-servers.yml:** add to Vault read-block (`~:152-154`) **and** the SSH `export X='${X}';` block (`~:178-203`). **(c) preview.yml:** add 3 lines *each* — Vault read-block (`~:122-128`), the `--arg foo "$FOO"` jq binding (`~:326-331`), **and** the Coolify payload `{"key":…,"value":$foo,"is_literal":true}` (`~:335-360`). **(d)** add to `deploy/docker-compose.ha.yml` env (`~:106-135`). **Also add `PUBLIC_BASE_URL`** to ha compose — present in prakash (`:201`) + preview (`:351`) but **missing from ha compose** (D8). `VIDEOGEN_UPLOAD_SERVICE_URL` handling: see D7. |
| S10 | Ingress / DNS | Repoint `upload.yral.com` to the storage service (or move frontend to call storage host). Keep old paths (`/get-upload-url` etc.) intact so frontend needs no change initially. |
| S11 | Frontend | Once ingress repoints, no change needed if paths preserved. If host changes, update frontend base URL. |
| S12 | Sentry | Ported notification/error capture goes to storage's Sentry project `/7` (was `/9`). Acceptable; note it. |
| S13 | Decommission | Tear down upload-service deploy (compose service, Coolify app, `docker-publish.yml`, DNS) after cutover + soak. |
| S14 | Dead code | Do NOT port unused upload code (`download_video_from_cf`, `upload_pending`, `duplicate_video_from_cf_to_storj`, serialization-only `AppError` paths). NOTE: `StorageError`/`AgentError`/`CanisterError`/`Unauthorized`/`PostNotFound` ARE used by the 3 handlers — keep them. |
| S15 | Docs | Update storage `README.md` + `.env.example` with the 2 new envs. Do **not** copy upload's README (stale: documents a `/get_upload_url_v3` `workers.dev` endpoint that no longer exists). |
| S16 | Input validation | `get_upload_url` interpolates `publisher_user_id`/`video_id` **unencoded** into a query string (upload `storj_interface.rs:23-28`). Validate/URL-encode on port — else a `&is_nsfw=true` injection is possible. `video_id` is server-minted (uuid) so only `publisher_user_id` is caller-controlled, but it's already validated as a `Principal` in the handler — keep that. |

---

## 5. Target architecture

```
                       ┌─────────────────────────────────────────────┐
   frontend ──────────▶│  storj-interface (merged)                   │
   (get-upload-url,    │                                             │
    update-metadata,   │  PUBLIC routes (no HMAC):                   │
    mark-published,    │   /get-upload-url                          │
    raw upload bytes)  │   /update-video-metadata  ─┐               │
                       │   /mark-post-as-published   │ in-process    │
                       │   /duplicate_raw/{upload,finalize}          │
                       │                             │               │
   VAST callback ─────▶│  /api/v2/videogen/complete ─┘ (Phase 2:     │
                       │     create_draft → in-process update-meta)  │
                       │                                             │
                       │  HMAC routes: /mirror/*, /media/*, /move..  │
                       └───────┬─────────────────┬───────────────────┘
                               │                 │
                   IC canisters│                 │ offchain.yral.com / metadata.yral.com
          (user-post, user-info, rate-limiter)   (events + notifications)
```

**Phase-1 caveat:** until Phase 3, `/update-video-metadata`'s finalize step makes an **HTTP POST back to the same binary** (`PUBLIC_BASE_URL/duplicate_raw/finalize`) — a self-loop that works but doubles connections and depends on `PUBLIC_BASE_URL` resolving from inside the container. Phase 3 collapses it to a direct call.

The public data-plane (upload + videogen) and the internal admin-plane (mirror/media, HMAC-gated) coexist in one binary, separated by **per-route middleware**, not by service boundary. Blast-radius concern (§7 R4) is mitigated by keeping the HMAC layer on all admin routes exactly as today.

---

## 6. Key decisions

**D1 — Edition: keep storage at 2021.** Upload is 2024 but its ported handler code (import grouping, `let-else`, etc.) compiles fine under 2021. Bumping storage to 2024 is a separate, riskier change touching the whole workspace (3 crates). *Decision: stay 2021; fix any 2024-only idioms during port.* Verify with `cargo build`.

**D2 — IC identity: REUSE storage's existing `ic_agent` (decided: identities are the same).** `BACKEND_ADMIN_IDENTITY` (storage Vault) and `IC_ADMIN_PRIVATE_KEY` (upload) are the **same principal** — confirmed by ops (OQ1 resolved). So the ported `add_post_v_1` / `update_post_status` / `get_individual_post_details_by_id` / `get_user_profile_details_v_6` calls use storage's existing `ic_agent` (already in `AppState`, URL `IC_URL`). *Consequences:* **no `upload_ic_agent`, no new `IC_ADMIN_PRIVATE_KEY` secret** — only 2 new secrets remain (`OFFCHAIN_EVENTS_API_TOKEN`, `YRAL_METADATA_NOTIFICATION_SERVICE_API_TOKEN`). The per-request delegated identity is still used only to verify the sender principal; canister calls are signed by the shared admin agent, exactly as upload did.

**D3 — Dependency rev reconciliation: VERIFIED GREEN; pin storage to `aa5abf3e`.** The ported code depends on generated types `PostDetailsFromFrontendV1`, `PostStatusFromFrontend`, `Result_`, `Result2`, `Result6`, `PostStatus`, and `ic::USER_*_SERVICE_ID`. **UPDATE (user decision):** track `branch = "main"` (yral-common renamed master→main), do NOT pin a rev. `Cargo.lock` still pins the resolved commit (now `55e7ec1d`, latest main) for reproducible builds; floating the branch accepts drift, mitigated by Task 0.2's compile-guard. The rev-pin rationale below is retained for context but superseded.

**Review confirmed (symbol-availability half):** storage's `features=["full"]` (→ `backend` → `user-post-service` + `user-info-service`, `canisters-client/Cargo.toml:25,46,51-52`) exposes all required symbols, and upload imports exactly these (`get_upload_url.rs:7-8`, `update_video_metadata.rs:11-13`, `mark_post_as_published.rs:6-7`, `types.rs:14`). `Cargo.lock` pins `aa5abf3e` (`:7029`). **Not independently proven:** that the generated bindings are *byte-identical* across `aa5abf3e`↔`f83d1b2` — the local cargo checkout holds a different rev, so this is asserted, not verified. *Decision: pin storage to `aa5abf3e` (drop floating `branch=master`), update `Cargo.lock`; **Phase-0 compile-guard is the actual proof** of binding compatibility.* Resolves **OQ3** (symbol set), de-risks the rest to Phase-0. Originally rated highest-risk; now **Low** (see re-ranked R1).

**D4 — Internalization phasing (each phase independently shippable + testable):**
- **Phase 0 — Dependency compile-guard:** pin `aa5abf3e`, add a `#[cfg(test)]` test that references `UserPostService`/`UserInfoService`/`USER_*_SERVICE_ID` so type-resolution regressions fail CI. No behavior change. (Cheap — D3 already verified.)
- **Phase 1 — Lift-and-shift:** port the 3 routes into storage, public, registered, swagger'd. `get_upload_url` builds URLs from existing `PUBLIC_BASE_URL`; `finalize_upload` still does an HTTP POST to storage's own `PUBLIC_BASE_URL` (a **self-loop** — explicit, see diagram). One binary, deployable. **No cutover yet** — deploy alongside the still-running upload-service.
- **Phase 2 — Internalize storage→upload (do this WITH/BEFORE cutover):** repoint `complete.rs:539-541` at an in-process draft client. It (1) decrypts `encrypted_identity` → `DelegatedIdentityWire` (reusing `IdentityCrypto`, same as `draft.rs:73-74`), (2) builds an `UpdateMetadataRequest` from `DraftCreationRequest` per the §8 field-mapping, (3) calls `update_metadata_impl` directly. Removes the VAST-completion HTTP hop AND removes storage's dependence on `VIDEOGEN_UPLOAD_SERVICE_URL=https://upload.yral.com`. **Cutover (ingress repoint of `upload.yral.com` → storage) happens at/after this phase** — see §10 for why ordering matters.
- **Phase 3 — Internalize upload→storage finalize:** `finalize_upload` calls `handler_raw_finalize` logic directly (thread query-derived params, §8). Removes the Phase-1 self-loop hop. (`get_upload_url` stays a URL builder — frontend uploads directly to `/duplicate_raw/upload`; cannot be internalized.)

**D5 — Auth model: preserve exactly.** 3 new routes are public (no `authorize`). In-body delegated-identity sender check remains the authorization for update/mark. `get-upload-url` stays unauthenticated (matches today). No change to HMAC on admin routes.

**D6 — Response envelope: preserve upload's `ApiResponse<T>` shape verbatim** (`{success, data, error_message}`) for the 3 ported routes, so the frontend contract is byte-compatible. Port `AppError` + its `IntoResponse` as-is into the upload module (does not need to unify with storage's per-module error style). Prune unused `AppError` variants.

**D7 — `VIDEOGEN_UPLOAD_SERVICE_URL`:** after Phase 2 the in-process draft client replaces the HTTP one outright (`complete.rs:539-541` points at the in-process impl), so the env var is dead. ⚠️ **Important:** while the old `draft_client_from_env()` is still wired, an **unset** `VIDEOGEN_UPLOAD_SERVICE_URL` defaults to `https://upload.yral.com` — only an **empty string** selects the no-op stub. So if you must disable the HTTP path before swapping the factory, set it to `""` (empty), not unset. After the factory swap, drop the var from all 3 points. ⚠️ It is a **hardcoded literal** `'https://upload.yral.com'` in prakash (`:193`) and preview (`:353`), and a default in ha compose (`:125`) — so "disable" means editing the literal to `''` in the workflow YAML, not flipping a secret. Since Phase-2 replaces the factory outright, the cleanest path is: ship Phase-2, then delete these 3 lines.

**D9 — Tolerant startup: do NOT panic on missing upload secrets (was OQ5).** Upload `.unwrap()`s its tokens at startup; the 2 carried-over (`OFFCHAIN_EVENTS_API_TOKEN`, `YRAL_METADATA_NOTIFICATION_SERVICE_API_TOKEN`) ported verbatim would panic the **whole merged binary** (mirror/media/videogen included) if absent. (IC identity is now the reused `ic_agent`, already tolerant.) *Decision: adopt storage's tolerant pattern (`if let Ok(...)`, as `BACKEND_ADMIN_IDENTITY` already does at `main.rs:251`) — if these tokens are absent, log + disable the 3 upload routes (or have them return 503), don't panic.* This makes upload a degradable feature like videogen.

**D8 — get-upload-url base URL: reuse existing `PUBLIC_BASE_URL`.** Storage already has `PUBLIC_BASE_URL` (`generate.rs:37,484`), used for exactly this purpose (videogen builds `/duplicate_raw/upload` URLs from it). `get_upload_url` builds the returned URL from `PUBLIC_BASE_URL` — the frontend must reach it. **Do not add a new env.** (Earlier draft proposed `UPLOAD_PUBLIC_BASE_URL` — rejected as a duplicate.) ⚠️ `PUBLIC_BASE_URL` is injected in only **2 of 3** deploy points (`deploy-prakash-servers.yml:183`, `deploy-preview.yml:351`) — it is **absent from `deploy/docker-compose.ha.yml`**. S9 must add it there, else HA `get_upload_url` builds URLs from an unset var.

---

## 7. Risks & mitigations

**Top risks after rev-3 decisions: R3 (startup panic) and R5 (cutover). R1 and R2 are resolved.**

| ID | Risk | Severity | Mitigation |
|----|------|----------|------------|
| R1 | yral-common rev skew → generated types differ | ~~High~~ **Low** | Symbol set verified present under `full`; binding-identity across revs asserted, **proven by Phase-0 compile-guard**. Pin `aa5abf3e`. |
| R2 | IC identity authorization at runtime | ~~High~~ **Low** | RESOLVED — identities are the same principal (D2/OQ1). Reuse storage's `ic_agent`. Still smoke-test one real publish in preview as belt-and-braces. |
| R3 | Upload `.unwrap()`s its secrets at startup (`main.rs:115,128` for the **2** carried-over tokens). Ported verbatim, **one missing secret panics the entire merged binary** — killing mirror/media/videogen too. | **High** | Adopt storage's tolerant pattern (`if let Ok(...)`, like `BACKEND_ADMIN_IDENTITY`): log + disable upload routes (or return 503) rather than panic. See D9. Add the 2 secrets to storage Vault + injection points BEFORE deploy. |
| R4 | Public upload routes now share a binary with HMAC admin plane → larger blast radius | Medium | Keep HMAC on every admin route (unchanged). Public routes touch only IC canisters + storj finalize, same as today. No new privilege exposed. |
| R5 | Ingress cutover breaks live uploads | Medium | Preserve route paths. Repoint `upload.yral.com` → storage at the LB/DNS layer; run both in parallel during soak; roll back DNS if errors spike. |
| R6 | Sentry events now land in project `/7` not `/9` | Low | Documented (D/S12). Update alerting dashboards. |
| R7 | Edition-2024 idioms fail under 2021 | Low | Caught at compile time in Phase 1; fix idioms. |
| R8 | `meta`/`post_details` contract drift (e.g. `title` not a real field of `PostDetailsFromFrontendV1`) | Low | Contract pinned in §8; reuse generated candid types, don't hand-roll. |
| R9 | Default 2MB body limit on update-metadata | Low | Wire is small (key + chain). If needed add `DefaultBodyLimit` per-route like videogen. |
| R10 | New unauthenticated `/get-upload-url` co-located with admin plane = DoS surface (mints UUIDs, hits IC + later Postgres-backed paths) on same binary as mirror/media jobs | Medium | No rate-limit exists today. At minimum note it; recommend per-IP limit or rely on upstream LB/Caddy rate-limiting. Privilege is unchanged (R4); availability is the concern. |
| R11 | Notification awaited inline → `metadata.yral.com` latency now blocks an in-process request path | Low | Pre-existing behavior; ported verbatim. Optional: spawn it. Note in §3.1. |

---

## 8. Contracts (must be byte-preserved)

**`DelegatedIdentityWire`** — **reuse the shared `yral_types::delegated_identity::DelegatedIdentityWire`, do NOT port upload's local copy.** ⚠️ **Security caveat:** the shared type's `TryFrom<…> for DelegatedIdentity` uses `DelegatedIdentity::new_unchecked` (`aa5abf3/types/src/delegated_identity.rs:33`) — it does NOT verify the delegation chain, whereas upload's local copy used verifying `new`. Reusing as-is weakens the `sender() == creator_principal` gate on the two public mutation routes. Plan Task 1.2c forces an explicit decision: re-verify the chain (recommended) or accept with rationale. **RESOLVED (Option A, implemented `8b5a5d6`):** added `routes::upload::auth::verified_sender` which reconstructs via ic-agent `DelegatedIdentity::new` (verifies the chain) instead of the wire's `new_unchecked` `TryFrom`. Confirmed by test: a forged `from_key` is rejected. Handlers use `verified_sender`, NOT `wire.try_into().sender()`. Storage already uses the shared type (`videogen/identity_crypto.rs:8`); upload declares a structurally-identical *local* duplicate (`utils/types.rs:168`). The shared type ships the same `TryFrom`. Porting upload's local struct would create a needless duplicate type. Shape (for reference):
```rust
struct DelegatedIdentityWire {
    from_key: Vec<u8>,
    to_secret: JwkEcKey,            // k256 JWK Secp256k1 secret
    delegation_chain: Vec<SignedDelegation>,
}
// TryFrom<DelegatedIdentityWire> for DelegatedIdentity:
//   to_secret -> SecretKey::from_jwk -> Secp256k1Identity::from_private_key
//   DelegatedIdentity::new(from_key, to_identity, delegation_chain)
//   used ONLY for .sender() assertion, never to sign canister calls.
```
**Phase-2 boundary (corrected):** at the `complete.rs:231` call site, `DraftCreationRequest` holds only `encrypted_identity: Option<String>` (an AES-256-GCM blob) — NOT a usable identity. Decryption happens *inside* the draft client (`draft.rs:73-74`) → a `DelegatedIdentityWire`, which today is JSON-serialized into the HTTP body. Phase-2 skips the **JSON/HTTP** round-trip but must still: (1) decrypt the blob → `DelegatedIdentityWire`, (2) hand that **wire** type to `update_metadata_impl` (which itself does `DelegatedIdentity::try_from(...)` internally). So Phase-2 passes the *wire*, not a `DelegatedIdentity` object. (Earlier draft said "pass the decrypted identity object straight through, no wire at all" — that was wrong.)

**`UpdateMetadataRequest`** (wire body for `/update-video-metadata`):
```rust
{ delegated_identity_wire: DelegatedIdentityWire,
  meta: HashMap<String,String>,
  post_details: PostDetailsFromFrontendV1 }   // {id, status(Draft|Published), hashtags, description, video_uid, creator_principal}
```
Handler injects `meta["post_details"] = json(RequestPostDetails)` (subset: `{video_uid, description, hashtags, creator_principal, id}`, drops `status`) before storj finalize.

**Offchain event (`offchain.yral.com/api/v2/events`) — byte-preserve the stringified-`params` quirk:** body is `{"event": "<name>", "params": "<JSON-string>"}` where `params` is `json!({...}).to_string()` (a JSON **string**, not a nested object — `events_interface.rs:44-68`). The consumer parses the inner string; do not "clean up" to a nested object. The flags `is_nsfw=false` and `is_hot_or_not=true` are **positional event params** (passed `false, true` at `update_video_metadata.rs:181`, `mark_post_as_published.rs:97`) — they are NOT fields of `PostDetailsFromFrontendV1`. Preserve the literals in the event call.

**`/duplicate_raw/finalize` (Phase-3 internalization target):** query params `{publisher_user_id, video_id, is_nsfw}` + JSON body `{metadata: HashMap<String,String>}`. Upload sends these as query+body (`storj_interface.rs:90-103`); storage reads `RawFinalizeParams` (`duplicate.rs:490`) + `RawFinalizeBody` (`:497`), handler `handler_raw_finalize` (`:624`). The in-process call must thread the query-derived params explicitly (no URL).

**Response envelope (all 3 routes):** `{ "success": bool, "data": T|null, "error_message": string|null }` (keep the `#[serde(skip_serializing)] status_code` field), HTTP status from `AppError::status_code()`. `add_post_v_1` returns bare `Result_::Ok` (no post-id payload).

**`get-upload-url`:** req `{publisher_user_id}` → resp `{upload_url, video_id}`, `video_id = uuid-v4 (dashed)`. `publisher_user_id` validated as `Principal`; URL-encode it into the query string (S16).

**Phase-2 internal draft contract — `DraftCreationRequest` → `UpdateMetadataRequest` mapping** (mirrors the existing HTTP build at `draft.rs:80-92`):

| `DraftCreationRequest` field | → `UpdateMetadataRequest` |
|---|---|
| `encrypted_identity: Option<String>` | decrypt → `delegated_identity_wire` |
| `video_id` | `post_details.id` **and** `post_details.video_uid` |
| `user_principal` | `post_details.creator_principal` |
| — (synthesized) | `post_details.status = Draft`, `hashtags = []`, `description = ""`, `meta = {}` |
| `request_id`, `request_key`, `object_key` | **dropped** (not needed by update path) |

Since `status = Draft`, `update_metadata_impl` does the storj finalize + `add_post_v_1` but the `Published`-only event branch is skipped; the `VideoUploadedToDraft` notification fires.

---

## 9. Test strategy

- **Phase 0:** `cargo build` + a **permanent** `#[cfg(test)]` test that constructs `UserPostService`/`UserInfoService` and references `USER_*_SERVICE_ID` — proves types/features resolve and guards against future rev drift (not throwaway).
- **Phase 1 (TDD per route):**
  - Unit: `DelegatedIdentityWire` → `DelegatedIdentity` → `.sender()` round-trip; sender-mismatch → 403; `RequestPostDetails` From/Into round-trip; `ApiResponse` serializes `error_message`.
  - Integration (hurl, matches existing `e2e-tests.yml`): hit `/get-upload-url`, `/update-video-metadata`, `/mark-post-as-published` against a test harness; assert envelope + status codes. Mock/stub IC + offchain where needed.
  - Build green under edition 2021 with `cargo clippy -- -D warnings` (CI-gated on PR) — dead-code pruning is **mandatory** (same commit), and `cargo test` runs on a no-network runner so unit tests must be pre-network.
- **Phase 2:** test that `complete.rs` success path invokes in-process draft creation (no outbound HTTP to `upload.yral.com`); assert draft row/canister call happens.
- **Phase 3:** test `update_metadata_impl` finalize path calls `handler_raw_finalize` logic without a network hop.
- **Pre-cutover smoke (preview env):** real end-to-end publish of one video using a real delegated identity; confirm canister `add_post_v_1` succeeds with the chosen identity (validates R2).

---

## 10. Cutover & rollback

**Ordering hazard (why Phase 2 gates cutover):** if `upload.yral.com` is repointed to storage while storage's `draft.rs` still calls `VIDEOGEN_UPLOAD_SERVICE_URL=https://upload.yral.com`, the videogen completion path calls **itself** via the repointed DNS — a fragile self-loop that only works if the env still resolves through the LB. Ship Phase 2 (in-process draft client) **at or before** cutover, and set `VIDEOGEN_UPLOAD_SERVICE_URL=""` (empty, per D7) at the same moment.

1. Phase 0–1–2 merged + deployed to storage **without** removing upload-service. Phase 2 wires the in-process draft client.
2. Add the **2** new Vault secrets (`OFFCHAIN_EVENTS_API_TOKEN`, `YRAL_METADATA_NOTIFICATION_SERVICE_API_TOKEN`) across all sites (S9). No `IC_ADMIN_PRIVATE_KEY` — IC agent reused (D2). Set `VIDEOGEN_UPLOAD_SERVICE_URL=""`.
3. Preview smoke test: a **real publish** with a real delegated identity — confirms R2 (canister accepts the chosen identity).
4. Repoint `upload.yral.com` ingress → storage service (OQ6). Keep upload-service running (parallel) during soak.
5. Soak N hours; watch Sentry `/7`, canister `add_post_v_1` success rate, event/notification delivery latency.
6. **Rollback:** revert DNS/ingress to upload-service (still running). No data migration → rollback is DNS-only.
7. After soak: decommission upload-service (compose service, Coolify app, `docker-publish.yml`, DNS record), drop `VIDEOGEN_UPLOAD_SERVICE_URL` from all injection points.

---

## 11. Open questions (need answers before/during implementation)

- **OQ1:** ~~RESOLVED~~ — `BACKEND_ADMIN_IDENTITY` and `IC_ADMIN_PRIVATE_KEY` are the **same principal**. Reuse storage's `ic_agent` (D2); no new IC secret.
- **OQ2 (ops/infra):** Cutover mechanism for `upload.yral.com` — DNS CNAME, LB rule, or Caddy/ingress route? Who owns it?
- **OQ3:** ~~RESOLVED~~ — pin `aa5abf3e`; review confirmed `.did`/bindings byte-identical to upload's `f83d1b2`, all required symbols present under `full`.
- **OQ4:** Keep `upload.yral.com` host (repoint) or move frontend to storage host? Repoint preferred (zero frontend change).
- **OQ5:** ~~Resolved by D9~~ — tolerant startup, no panic.
- **OQ6 (infra):** Three hostnames now in play — `upload.yral.com` (current upload ingress), `storj-interface.yral.com` (Caddyfile → `storj-interface:3000`), `storage-interface.prakash.yral.com` (prakash-servers deploy + the URL upload hardcodes). Cutover must pick which host(s) the frontend hits and reconcile all three. Who owns each?

---

## 12. Out of scope

- No change to storage's mirror/media/videogen-generate logic beyond the `draft.rs` internalization.
- No DB schema change (upload is stateless).
- No edition bump of the workspace.
- No consolidation of the two Sentry projects beyond noting the event re-routing.
