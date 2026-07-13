# Move `/profile-image` Endpoint Into This Service

**Date:** 2026-07-13
**Repo context:** `yral-video-storage-service`
**Related tickets:** [#2066](https://github.com/dolr-ai/yral/issues/2066) (image hosting on Cloudflare — cleanup), [#2107](https://github.com/dolr-ai/yral/issues/2107) (cleanup off-chain-agent media assumptions)
**Primary goal:** Own user profile-image upload/delete in this media-storage service, so profile media stops depending on `off-chain-agent`.

---

## Summary

Today `off-chain-agent` owns the profile-image endpoints: `POST /profile-image` (upload) and `DELETE /profile-image`. Uploads already go to **self-hosted Hetzner Object Storage** (bucket `yral-profile` on `hel1.your-objectstorage.com`) — not Cloudflare — and the endpoint also writes `profile_picture_url` back to the `user_info_service` canister as the user.

This spec moves those two endpoints into `yral-video-storage-service` as an **additive, dual-run** change: we implement an identical request/response contract here, leave off-chain-agent's copy live, and let web/mobile clients migrate to this service's base URL opportunistically. No lockstep client deploy; nothing is deleted from off-chain-agent in this spec.

**Non-goals:** the GobGob default-avatar migration off Cloudflare Images (#2066 proper) and retiring off-chain-agent's handlers (#2107) are separate follow-ups.

---

## Current Behavior (off-chain-agent, to replicate)

Source: `off-chain-agent/src/user/profile_image.rs`, `off-chain-agent/src/utils/s3.rs`.

**`POST /profile-image`** — body `{ delegated_identity_wire, image_data }` (base64, optional `data:` prefix) → `{ profile_image_url }`:
1. Verify the delegated identity, resolve the user principal.
2. Validate: non-empty; base64 length ≤ `7 * 1024 * 1024` (~5 MB decoded); valid standard base64.
3. Upload: decode → if width/height > 1000, resize (Lanczos3, aspect preserved) → convert RGB8 → encode JPEG q85 → `put_object` with `public-read` ACL, key `users/<principal>/profile-<unix_ts>.jpg`, `content-type: image/jpeg` → return `<public_url_base>/<key>`.
4. Update canister: build a **user** agent from the delegated identity wire, call `UserInfoService(USER_INFO_SERVICE_ID, user_agent).update_profile_details({ profile_picture_url: Some(url), bio: None, website_url: None })`.

**`DELETE /profile-image`** — body `{ delegated_identity_wire }` → 200:
1. Verify identity, resolve principal.
2. List `users/<principal>/profile-` and delete all matching objects. (Does **not** clear `profile_picture_url` in the canister — matching current behavior.)

Error mapping: 401 (identity/user-info failure), 400 (bad/oversized/undecodable image), 403 (canister "not authorized"), 500 (upload / agent / canister errors).

---

## Design

### Placement
- New module `src/routes/user/mod.rs` + `src/routes/user/profile_image.rs` (handlers).
- Storage helper `src/routes/user/profile_s3.rs` — ports `upload_profile_image_to_s3` / `delete_profile_image_from_s3` and the `process_image` resize/encode step verbatim.
- Register `POST` and `DELETE /profile-image` in the router alongside the existing public upload routes.

The endpoints reuse this repo's existing public-route conventions (JSON body carrying the delegated identity, no HMAC — auth is the chain-verified in-body identity), matching `routes/upload/*`.

### Auth + canister write
- Extend `src/routes/upload/auth.rs`: today `verified_sender(wire) -> Principal` builds a chain-verified `DelegatedIdentity` (via `DelegatedIdentity::new`, which validates the delegation chain and `delegated_principal == to.sender()`) then returns `.sender()`. Add `verified_identity(wire) -> DelegatedIdentity` (or return `(DelegatedIdentity, Principal)`) so the handler can both learn the principal and build a user `ic_agent` from the same verified identity. `verified_sender` stays (used by the video routes) and can delegate to the new function.
- Handler flow: verify → upload → build user agent from the verified identity → `update_profile_details`. The canister write is **as the user**; `BACKEND_ADMIN_IDENTITY` is not used.

### Credentials + bucket (env-driven, one creds set)
Reuse this service's existing `HETZNER_S3_ACCESS_KEY` / `HETZNER_S3_SECRET_KEY` / `HETZNER_S3_ENDPOINT` / `HETZNER_S3_REGION` (already verified to write `prakash-yral` on hel1; expected to cover the whole hel1 project including `yral-profile`). Bucket, key prefix, and public URL base are configurable so the fallback is a config flip, not a code change:

| Env | Default (primary) | Fallback |
|---|---|---|
| `PROFILE_S3_BUCKET` | `yral-profile` | `prakash-yral` |
| `PROFILE_S3_KEY_PREFIX` | `users/` | `profile-images/users/` |
| `PROFILE_S3_PUBLIC_URL_BASE` | `https://yral-profile.hel1.your-objectstorage.com` | `https://prakash-yral.hel1.your-objectstorage.com` |

Object key: `<PROFILE_S3_KEY_PREFIX><principal>/profile-<unix_ts>.jpg`. DELETE lists/deletes under `<PROFILE_S3_KEY_PREFIX><principal>/profile-`.

**Verification step (implementation):** confirm the existing creds can `put_object`/`list`/`delete` in `yral-profile`. If they cannot, set the three envs to the fallback so images land in `prakash-yral` beside the `yral-video-storage-service/` backup prefix.

**Dual-run caveat (only if the effective bucket differs from off-chain-agent's `yral-profile`):** uploads split across two buckets by which service handled them. Reads stay correct because the canister stores the full, self-describing `profile_picture_url`. Cross-service DELETE does not reach the other bucket (a user who uploaded via the new service but deletes via an old app build leaves an orphaned public object). Accepted for the migration window.

### Dependencies + config wiring
- Add `image` (with the `jpeg` feature) to the main crate `Cargo.toml` (already a workspace dep for `backfill-thumbnails`/`phash`).
- Add the three `PROFILE_S3_*` envs to `deploy/docker-compose.ha.yml` (storj-interface service), `.github/workflows/deploy-prakash-servers.yml`, `.env.example`, and the readme config table. The `HETZNER_S3_*` creds are already passed to the app.

### Data flow
```
client ──POST /profile-image {wire, image_data}──▶ storj-interface
  verify wire (chain-checked) ─▶ principal + user identity
  decode+process image ─▶ put_object(PROFILE_S3_BUCKET, users/<principal>/profile-<ts>.jpg, public-read)
  user_agent(identity) ─▶ UserInfoService.update_profile_details(profile_picture_url)
  ◀── { profile_image_url }
```

### Error handling
Mirror off-chain-agent's status mapping exactly (401/400/403/500 as above), using this repo's `AppError`/response conventions. Internal errors are logged (`tracing`) and returned as generic messages per repo security rules; no internal paths leaked.

---

## Testing

- **Unit:** `process_image` (resize threshold at 1000px, non-resize passthrough, JPEG output); base64 validation (empty, oversized >5 MB, invalid); `data:` prefix stripping; object-key formatting from prefix + principal + timestamp.
- **Auth:** reuse `routes/upload/test_support::signed_wire_with_sender` — valid wire resolves to expected sender; forged `from_key` rejected (extends the existing `auth.rs` tests to the new `verified_identity`).
- **Integration (`#[ignore]`, env-gated like `hetzner_both_thumbnail_names_uploaded`):** real upload → object exists at expected key/URL with `image/jpeg`; DELETE removes all `users/<principal>/profile-*`. Gated on `HETZNER_S3_*` + `PROFILE_S3_*`.
- **Canister update:** exercised manually against a test principal (off-chain-agent's canister path has no automated test to port); handler unit-tested up to the canister boundary.

---

## Rollout

1. Implement endpoints + config here; verify creds against `yral-profile` (else flip to fallback env).
2. Deploy this service (endpoints live, additive — off-chain-agent unchanged).
3. Migrate web (`hot-or-not-web-leptos-ssr`) then mobile (`yral-mobile`) to call this service's `/profile-image` base URL — separate client PRs, no coordination required.
4. Once client traffic to off-chain-agent's `/profile-image` drains (old mobile builds aged out), retire off-chain-agent's handlers + the dead `CF_IMAGES_API_TOKEN` — tracked under #2107 (out of scope here).

## Open Items

- Confirm existing `HETZNER_S3_*` creds write `yral-profile` (drives primary-vs-fallback bucket).
- Client base-URL config: how web/mobile currently point at off-chain-agent's `/profile-image` (env vs hardcoded) — needed for step 3, tracked in the client repos.
