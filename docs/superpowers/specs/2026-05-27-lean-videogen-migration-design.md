# Lean Videogen Migration Design

Date: 2026-05-27

## Summary

Move the mobile-facing video generation flow out of off-chain-agent and into the Prakash service while keeping the endpoint lean. The new Prakash endpoint will accept the same videogen request payload shape mobile sends today, validate the delegated identity, run prompt/image moderation through a moderation service, create a RateLimiter request when allowed, submit directly to the LTX service on Vast, and return the submitted request identifier immediately.

The Vast LTX service owns generation output handling. When generation completes, it uploads the video to the bucket, deletes local disk output only after upload succeeds, and calls a Prakash completion endpoint. Prakash then updates RateLimiter and creates the user's draft when the request asked for server-side draft handling.

## Goals

- Preserve the existing mobile request contract for videogen.
- Keep the Prakash `/api/v2/videogen/generate` path small and synchronous.
- Add a moderation seam for prompt/image NSFW scorecards before rate limit consumption.
- Use RateLimiter as the source of generation request state and in-progress drafts.
- Remove QStash from the LTX path.
- Remove token cost lookup, DOLR/Sats deduction, HON JWT use, DOLR user-agent creation, and rollback behavior from this flow.
- Preserve the current user-facing behavior where an accepted generation becomes an in-progress draft, then becomes an actual draft after generation completes.
- Make Vast responsible for bucket upload and post-upload local disk cleanup.

## Non-Goals

- No mobile code changes are required for this migration.
- Do not migrate unrelated off-chain models or non-LTX providers as part of this spec.
- Do not add billing, balance deduction, paid-token cost calculation, or rollback behavior.
- Do not make RateLimiter responsible for actual draft post creation. RateLimiter tracks generation status and usage; draft creation stays in the service completion pipeline.

## Current State Findings

In off-chain-agent, the current videogen path performs too much work in one flow:

- Validates delegated identity and adapts the request.
- Checks NSFW using the existing Gemini-based prompt moderation helper.
- Creates a RateLimiter video generation request.
- Looks up model cost and calculates payment data.
- Creates delegated DOLR identity context.
- Gets HON worker JWT.
- Deducts balance and configures rollback behavior.
- Encrypts delegated identity.
- Queues generation work to QStash.

For LTX, off-chain submits to ComfyUI/Vast with a callback URL. When ComfyUI/Vast completes, it calls off-chain's `/comfyui/webhook`. Off-chain then updates RateLimiter and, if `handle_video_upload == ServerDraft`, calls the upload-service metadata path to create the user's actual draft.

In the Prakash service today, `src/routes/videogen.rs` already has an in-progress drafts endpoint. It validates delegated identity, checks that the sender matches `user_id`, then reads `Pending` and `Processing` requests from the RateLimiter canister.

In the Vast `videogen` repo, the service already has generation, result, upload/image, webhook, and cleanup concepts. It will need bucket-upload and Prakash-completion behavior added to the job completion path.

The current off-chain callback also has cleanup semantics that matter during migration. On failure, it updates RateLimiter to `Failed`, decrements the video generation counter, and rolls back deducted balance when a deduction exists. On success with `ServerDraft`, it decrypts the delegated identity and queues the draft upload/metadata step. Those behaviors must remain available for legacy in-flight off-chain jobs until the old path is drained, but migrated LTX jobs should not call the off-chain callback or QStash cleanup path.

## Selected Architecture

The selected architecture has two cooperating services:

1. **Prakash videogen API**
   - Owns mobile-facing request validation.
   - Owns moderation call.
   - Owns RateLimiter create/check and status update calls.
   - Owns actual draft creation after Vast completes.
   - Does not own generated-file upload for this flow.

2. **Vast LTX service**
   - Owns LTX generation.
   - Owns upload of generated videos to the bucket.
   - Owns deleting local generated files only after successful bucket upload.
   - Calls Prakash completion endpoint with the uploaded bucket URL and request context.

This keeps the submit endpoint lean while preserving the existing asynchronous lifecycle.

## Review Findings To Carry Into Implementation

The deeper reviews surfaced design constraints that must be explicit before implementation:

1. **Mobile compatibility is stricter than "same params."** Mobile currently sends `delegated_identity`, a nested `request`, `upload_handling`, text prompt, optional base64 image, `token_type`, and `user_id`. It expects the existing success response shape: `operation_id`, `provider`, and `request_key`. Error bodies must remain compatible with the current `VideoGenError` enum shape so mobile can parse the message and map the HTTP status to its existing error UI.
2. **Bucket URL alone may not be enough to create a draft.** The existing draft creation path eventually calls upload metadata with a video id. If Vast uploads directly to a bucket, Prakash must know the resulting video id/object key and must be able to map it to the draft metadata call. The safest design is for Prakash to create or reserve the destination before calling Vast, then pass that destination to Vast with the generation request.
3. **Completion context must be durable and transactional.** The generation request returns immediately while completion happens later. Anything needed to create the draft, authenticate completion, or make callbacks idempotent cannot live only in memory. Prakash must persist a Postgres completion-context row keyed by RateLimiter request key and use atomic state transitions for callback handling.
4. **Cleanup is split across three repos during migration.** Vast owns generated-file cleanup for migrated jobs, Prakash owns completion-context cleanup, and off-chain owns only legacy in-flight QStash/callback cleanup until the old LTX route is drained.
5. **Completion callbacks are a privileged internal API.** Prakash completion must use a concrete authentication scheme before staging: HMAC-SHA256 with a versioned service-secret registry, timestamp skew enforcement, and body hash signing. The endpoint must not be deployable without this authentication.
6. **Late callbacks and stale reconciliation need deterministic precedence.** Terminal Prakash context states are final. Late Vast callbacks after a terminal failure must not revive a context unless a human/operator-triggered recovery tool explicitly reopens it.
7. **Upload destinations must outlive generation.** Any scoped upload URL passed before generation must remain valid beyond the max generation and retry window, or Vast needs an authenticated refresh path before upload.
8. **Vast submission is a formal service contract.** Prakash and Vast must agree that Prakash supplies the non-guessable Vast `request_id` and Vast echoes it on acceptance and completion.

## Explicit Surface Contracts

### Mobile-Facing Contract

Prakash must accept the current mobile request shape:

- `delegated_identity`: delegated identity wire.
- `request.user_id`: principal string claimed by the request.
- `request.model_id`: LTX model id supported by the migrated path.
- `request.prompt`: text prompt.
- `request.image`: optional image, currently base64 in mobile for image flows.
- `request.token_type`: still accepted for RateLimiter compatibility.
- `upload_handling`: currently `ServerDraft` for generated draft flows.

Prakash must return the current success response shape:

- `operation_id`: exactly `<principal>_<counter>`, using an underscore separator and decimal counter with no zero padding. Existing off-chain code uses `format!("{}_{}", request_key.principal, request_key.counter)`. Mobile should receive this exact format, but server logic should use `request_key` for identity rather than parsing `operation_id`.
- `provider`: `Ltx2` or the selected provider label.
- `request_key`: `{ principal, counter }`.

The existing in-progress endpoint can continue to serve `Pending` and `Processing` RateLimiter records. Mobile polling through the RateLimiter request key also remains valid as long as Prakash updates the same request key to a terminal status.

Mobile also has existing provider-list calls under the videogen API base. For a no-code mobile rollout where the videogen base URL points to Prakash, Prakash must expose compatible `/api/v2/videogen/providers` and `/api/v2/videogen/providers-all` responses for the migrated LTX provider set. Keeping provider discovery pointed at off-chain while only moving generate is possible as a temporary rollout tactic, but it should not be the target contract.

The provider response must match the current mobile DTO shape:

```json
{
  "providers": [
    {
      "id": "ltx2",
      "name": "Ltx2",
      "description": "...",
      "cost": { "usd_cents": 0, "dolr": 0, "sats": 0 },
      "supports_image": true,
      "supports_negative_prompt": false,
      "supports_audio": true,
      "supports_seed": true,
      "allowed_aspect_ratios": ["16:9", "9:16", "1:1"],
      "allowed_resolutions": [],
      "allowed_durations": [5],
      "default_aspect_ratio": "16:9",
      "default_resolution": null,
      "default_duration": 5,
      "is_available": true,
      "is_internal": false,
      "model_icon": null,
      "extra_info": {}
    }
  ]
}
```

The implementation should contract-test these endpoints against mobile's `ProvidersResponseDto` fields and off-chain's `ProvidersResponse` shape.

`providers-all` must use the same top-level response envelope and provider item shape as `providers`. The difference is content, not schema: `providers` returns the production-available migrated providers, while `providers-all` may include disabled, unavailable, or internal providers with the same fields and the correct `is_available`/`is_internal` flags.

### Error Payload Contract

The generate endpoint should return errors in the existing `VideoGenError` JSON representation:

- Identity failure: `401` with `AuthError`.
- Invalid model/input: `400` with `InvalidInput(message)`.
- NSFW rejection: `400` with `InvalidInput(message)` and a stable user-safe message such as "Content violates safety guidelines".
- Rate limit exceeded: `429` with a parseable `ProviderError(message)` or existing RateLimiter-compatible error variant.
- Vast submit unavailable: `503` with `NetworkError(message)` or `ProviderError(message)`.
- moderation service unavailable in production: `503` with `NetworkError(message)`.

Mobile maps status codes to existing error types, so introducing a new error variant would require mobile work and is outside this migration.

### Image/Text Input Contract

moderation service must receive the same content the user submitted: prompt plus optional image. For image requests, Prakash must support mobile's current base64 image shape.

For Vast submission, Prakash should normalize image input to the format Vast expects. The current Vast worker supports `image_url` for I2V, and also exposes `/upload/image`. A practical implementation path is:

1. Use the original prompt/image for moderation.
2. After moderation and RateLimiter success, upload the image to Vast `/upload/image` or another configured image staging location.
3. Pass the resulting image URL/reference to Vast `/generate`.

This image staging step is allowed in the lean endpoint because it is part of submitting the job. It is distinct from the removed DOLR, billing, JWT, and QStash work.

### Generate Idempotency Contract

Mobile does not currently send a first-class idempotency key. To avoid duplicate RateLimiter consumption on client-side timeouts and retries, Prakash should implement server-side best-effort dedupe:

- Compute a request fingerprint after identity validation using canonical JSON and SHA-256.
- Before calling RateLimiter, look up a Postgres completion context for the same principal and fingerprint created within `VIDEOGEN_GENERATE_DEDUPE_WINDOW_SECS` (default: `120`).
- If a matching context exists in `context_created`, `submitted`, `uploaded`, `draft_creating`, `draft_created`, or `complete`, return its existing `operation_id`, `provider`, and `request_key` instead of creating a new RateLimiter request.
- If the matching context is terminal failure, do not dedupe; allow a new request.
- This is best-effort because a retry that arrives before the first request persisted its context can still create a second request. A future mobile idempotency key would make this exact.

Fingerprint algorithm:

- Build a canonical JSON object with sorted keys and no whitespace.
- Include `fingerprint_version: 1`, principal, model id, prompt exactly as received, negative prompt exactly as received or null, aspect ratio, duration, resolution, seed, generate-audio flag, upload handling, token type, and image identity.
- For base64 image input, image identity is lowercase hex SHA-256 of the decoded image bytes.
- For URL/image-reference input, image identity is lowercase hex SHA-256 of the exact reference string.
- Store `request_fingerprint` as lowercase hex SHA-256 of the canonical JSON bytes and store `request_fingerprint_version` on the context row. The version is deliberately both inside the hash input and stored as a column: the hash input prevents cross-version dedupe collisions, while the column supports migrations, debugging, and selective reindexing.

### Completion Context Contract

Prakash should persist completion contexts in Postgres. This repo already requires `DATABASE_URL` and initializes a Postgres schema, so this avoids adding Redis/canister persistence for short-lived operational state.

Create a `videogen_completion_contexts` table, keyed by RateLimiter request key:

- `principal`.
- `counter`.
- `operation_id`.
- `request_fingerprint`.
- `request_fingerprint_version`.
- `provider`.
- `model_id`.
- `prompt`.
- `upload_handling`.
- `encrypted_delegated_identity`, only for `ServerDraft`.
- `encryption_key_id`.
- bucket destination or upload destination for Vast, including upload URL expiry when applicable.
- draft video id/object key.
- Vast `request_id` once submission succeeds.
- processing state.
- attempt counters for Vast submit, completion callback, draft creation, and reconciliation.
- `dedupe_expires_at`.
- `generation_expires_at`.
- `upload_destination_expires_at`.
- timestamps.
- `last_error`.
- `last_reconciliation_error`.

Required constraints and indexes:

- Primary or unique key on `(principal, counter)`.
- Unique key on `operation_id`.
- Unique key on `draft_video_id` when present.
- Index on `(principal, request_fingerprint, created_at)` for generate dedupe lookup.
- Index on `(state, updated_at)` for reconciliation.

If Vast submission fails after context creation, Prakash should mark the context failed and update RateLimiter according to the failure policy below.

### Completion Idempotency Contract

Completion handling must be transactionally idempotent. The implementation must not rely on read-then-write checks without a lock.

Use one of these Postgres patterns:

1. Start a transaction and `SELECT ... FOR UPDATE` the context row by `(principal, counter)`.
2. Or use an atomic `UPDATE ... WHERE principal = $1 AND counter = $2 AND state IN (...) RETURNING *` claim step.

The selected behavior:

- A success callback can be claimed only from `submitted`.
- Claiming success changes state to `uploaded` or `draft_creating` before any draft metadata call.
- Concurrent callbacks that find `uploaded` or `draft_creating` return `202 Accepted` or `200 OK` and do not create another draft.
- Duplicate callbacks after `complete` return `200 OK` without mutation.
- Callbacks after terminal failures such as `submit_failed`, `stale_failed`, `draft_failed`, or `failed` return `409 Conflict` with a non-retryable body. Vast must treat `409` as terminal and stop retrying.
- Mismatched principal, counter, Vast `request_id`, or video id/object key returns `409 Conflict` or `401` depending on whether authentication succeeded.

Draft creation idempotency must be enforced at two layers:

- Prakash must use a unique `draft_video_id`/request-key guard so two concurrent completion handlers cannot both start independent draft creation work.
- The downstream upload metadata/draft service must also be idempotent on `video_id` or an equivalent client-provided idempotency key. This is an external contract that must be confirmed before implementation starts. Without that guarantee, a Prakash crash after external draft creation but before recording `draft_created` could create duplicate drafts on retry.

### Encryption Key Management

For `ServerDraft`, store delegated identity encrypted with AES-256-GCM using a 96-bit random nonce. Store the nonce with the ciphertext and store `encryption_key_id` on the context row.

Configuration:

- `VIDEOGEN_IDENTITY_ENCRYPTION_KEYS`: comma-separated key registry, e.g. `v1:<base64-32-byte-key>,v2:<base64-32-byte-key>`.
- `VIDEOGEN_IDENTITY_ACTIVE_KEY_ID`: key id used for new rows.

Rotation rules:

- New contexts use the active key id.
- Existing contexts decrypt with the row's `encryption_key_id`.
- Old keys must remain configured until all contexts encrypted with that key are terminal and past retention.
- Decryption failure during completion does not create a draft and does not silently succeed. It moves the context to a retryable or terminal error according to retry policy and emits an alert.

### Bucket Upload Contract

The team-selected flow requires Vast to upload the generated video to the bucket. To reduce credential exposure on the GPU server, prefer passing a scoped upload destination to Vast rather than giving Vast broad bucket credentials.

Selected destination approach:

1. Prakash obtains a service-issued upload URL and `video_id` before Vast submission. Prefer the existing upload-service `/get-upload-url` contract if it can safely be called server-to-server for generated drafts.
2. Prakash stores the `video_id`, upload URL metadata, and expected object key in the Postgres completion context.
3. Prakash passes the scoped upload URL and `video_id` to Vast.
4. Vast uploads the generated MP4 to that upload URL and never receives delegated identity or broad bucket credentials.

Upload URL lifetime rules:

- `VIDEOGEN_UPLOAD_URL_TTL_SECS` defaults to `4200`.
- The upload URL must expire no earlier than `VIDEOGEN_UPLOAD_URL_PRE_SUBMIT_MARGIN_SECS + VIDEOGEN_LTX_GENERATION_TIMEOUT_SECS + VIDEOGEN_COMPLETION_RETRY_GRACE_SECS + VIDEOGEN_VAST_UPLOAD_RETRY_WINDOW_SECS + VIDEOGEN_UPLOAD_URL_SAFETY_BUFFER_SECS`.
- With the default submission overhead of 10 seconds, generation timeout of 1800 seconds, callback grace of 900 seconds, upload retry window of 900 seconds, and safety buffer of 300 seconds, the upload URL must be valid for at least 3910 seconds. The default is rounded up to 4200 seconds.
- If the upload service cannot issue a scoped URL for that long, Prakash must reserve only the `video_id`/object key before generation and expose an authenticated refresh endpoint for Vast to request a fresh scoped upload URL after generation and before upload.
- The refresh endpoint must require the same completion HMAC scheme and must verify request key, principal, Vast `request_id`, and expected object key before issuing a replacement upload URL.
- Upload URL expiry must be stored in the context row and emitted in Vast submission logs without logging the URL itself.

Before rollout, confirm whether the upload service can issue scoped URLs valid for at least `VIDEOGEN_UPLOAD_URL_TTL_SECS`. If it cannot, the refresh endpoint must ship with upload destination preparation; it is not a later hardening task.

Either way, Vast completion must return:

- `bucket_url`.
- `video_id` or object key.
- file size and content type when available.
- checksum/hash when available.
- request key and Vast `request_id`.

Prakash must not trust or fetch arbitrary `bucket_url` values from the callback. It should validate the returned `video_id`/object key against the stored context and either construct the expected bucket URL itself or verify the callback URL is exactly the expected URL for that object. This avoids SSRF if a callback is spoofed or Vast is compromised.

Reserved upload destination cleanup:

- If Vast is never submitted, Prakash must release or cancel the reserved upload destination if the upload service supports that operation.
- If generation fails before upload, Prakash must release or cancel the reserved upload destination if possible.
- If draft creation fails permanently and the operator retry retention window expires, Prakash or the upload service must mark the orphaned `video_id`/object key for deletion or garbage collection.
- If no explicit release endpoint exists, the upload service must have TTL cleanup for unused reserved `video_id`/object-key records, and Prakash must store the expected expiry for support/debugging.

### Upload URL Refresh Endpoint

If the upload service cannot issue upload URLs valid for `VIDEOGEN_UPLOAD_URL_TTL_SECS`, Prakash must expose a refresh endpoint for Vast:

`POST /api/v2/videogen/upload-url/refresh`

Authentication:

- Same HMAC header scheme as `/api/v2/videogen/complete`.
- Same completion body size limit.

Request:

- Request key.
- User principal.
- Vast `request_id`.
- Expected `video_id` or object key.

Response:

- Fresh scoped upload URL.
- `video_id`.
- object key.
- `expires_at`.

Prakash includes `upload_url_refresh_url` in the Vast submission only when the refresh endpoint is enabled. If the field is absent, Vast must not construct a refresh URL from the Prakash base URL and should rely on the original upload URL TTL.

Vast refresh trigger:

- Before uploading, Vast must compare `upload_destination.expires_at` with current time plus `VIDEOGEN_VAST_UPLOAD_EXPIRY_REFRESH_MARGIN_SECS`.
- If fewer than `VIDEOGEN_VAST_UPLOAD_EXPIRY_REFRESH_MARGIN_SECS` remain, Vast must call `upload_url_refresh_url` to obtain a fresh scoped URL before upload.
- If upload fails with an expiry-compatible response such as `403 Forbidden`, Vast may call refresh once and retry upload before reporting upload failure.
- Handle the refresh response exactly like the initial upload destination and update the local outbox metadata with the refreshed expiry.

Prakash must verify request key, principal, `request_id`, and object key against the stored context before issuing a replacement URL.

### Repository Impact And Cleanup Contract

This migration touches three repositories with different cleanup responsibilities.

**Prakash / yral-video-storage-service**

- Add the mobile-facing generate endpoint and Vast completion endpoint.
- Store durable completion context for migrated jobs.
- Delete or mark encrypted delegated identity as consumed after terminal completion/failure.
- Add a periodic cleanup/reconciliation job for stale completion contexts:
  - `context_created` but never submitted to Vast. This must update RateLimiter to `Failed(reason)`, call `decrement_video_generation_counter_v_1` for `VIDEOGEN`, release the reserved upload destination when possible, and redact encrypted identity.
  - `submitted` but no completion callback after the configured max generation window. This must update RateLimiter to `Failed(reason)`, release the reserved upload destination when possible, and redact encrypted identity.
  - `uploaded` but draft creation did not start within the configured draft timeout.
  - `draft_creating` but no progress after `VIDEOGEN_DRAFT_CREATE_TIMEOUT_SECS`.
  - `draft_created` but RateLimiter was not updated to `Complete(bucket_url)`.
- Reconciliation must process at most `VIDEOGEN_RECONCILIATION_BATCH_SIZE` stale contexts per state per run.
- Keep the existing in-progress draft endpoint backed by RateLimiter.
- Do not call off-chain's QStash video callback for migrated LTX jobs.
- Apply HTTP-layer protection before expensive work: request body limit, IP/service-level rate limiting, and HMAC where required for internal endpoints.

**Vast / videogen**

- Resolve concrete local output file paths from ComfyUI output metadata. Current output metadata has `filename` and `subfolder`, but `local_path` is `None`; the migration needs to derive the path from `COMFYUI_OUTPUT_DIR`.
- Before uploading to the provided upload URL, check `upload_destination.expires_at`. If the URL is expired or fewer than `VIDEOGEN_VAST_UPLOAD_EXPIRY_REFRESH_MARGIN_SECS` remain, call `upload_url_refresh_url` when present to obtain a fresh scoped upload URL before upload. If refresh is unavailable and the URL is too close to expiry, fail the upload with a retryable upload-destination error rather than attempting a likely-expired upload.
- If upload fails with an expiry-compatible response such as `403 Forbidden`, Vast may call `upload_url_refresh_url` once and retry upload before reporting upload failure.
- Handle a refreshed upload destination the same way as the initial destination and persist the refreshed expiry in the local outbox metadata.
- Upload the generated MP4 to the Prakash-provided destination.
- Persist a local completion outbox record after upload succeeds and before calling Prakash completion.
- Delete the generated local MP4 only after upload succeeds and the outbox record has enough durable metadata to retry callback after process restart.
- Keep the existing TTL cleanup task as a fallback, but do not rely on it as the primary cleanup path for successful migrated jobs.
- Add cleanup for staged I2V input images if ComfyUI `/upload/image` stores them in a persistent input directory. Staged images should be deleted after generation reaches a terminal state and the output upload/callback state is durably recorded. Unreferenced staged images, such as images staged before a Prakash crash prior to Vast submission, should expire through `VIDEOGEN_VAST_STAGED_IMAGE_TTL_HOURS`. The current cleanup task only scans the output directory for video extensions.

**off-chain-agent**

- Keep current QStash video generation, `/comfyui/webhook`, and `/qstash/video_gen_callback` behavior until all legacy off-chain LTX jobs have completed or failed.
- Keep legacy failure cleanup for those old jobs: RateLimiter failure update, counter decrement, and balance rollback when applicable.
- After traffic moves to Prakash and old jobs drain, gate or remove the off-chain LTX path so new LTX jobs cannot accidentally be submitted through QStash.
- Do not remove unrelated off-chain videogen providers or callbacks unless their traffic is also migrated.
- Do not share encrypted delegated identity blobs between off-chain and Prakash; each path owns its own completion context.

## Generate Request Flow

1. Mobile calls Prakash `/api/v2/videogen/generate` using the same videogen params it sends today.
2. Prakash applies cheap HTTP-layer protections: body limit and IP/service-level rate limiting.
3. Prakash validates the delegated identity can be parsed.
4. Prakash derives the sender principal from the delegated identity.
5. Prakash parses `user_id` and verifies `identity.sender() == user_id`.
6. Prakash extracts the model id, prompt, image input, token type, and `handle_video_upload`.
7. If `handle_video_upload != ServerDraft`, Prakash returns `400 InvalidInput` before moderation, RateLimiter, image staging, or Vast calls. The migrated LTX endpoint supports only `ServerDraft` in this phase.
8. Prakash computes the generate request fingerprint and checks for a dedupe hit within `VIDEOGEN_GENERATE_DEDUPE_WINDOW_SECS`.
9. Prakash sends the prompt/image combo to moderation.
10. If moderation service returns NSFW, Prakash returns an NSFW error response immediately. No RateLimiter request is created.
11. If moderation passes, Prakash calls RateLimiter to create/check the video generation request.
12. If RateLimiter rejects the request, Prakash returns a rate-limit error immediately.
13. If RateLimiter accepts, Prakash persists the minimal Postgres completion context keyed by the returned RateLimiter request key. If this Postgres write fails, Prakash must immediately update RateLimiter to `Failed(reason)`, call `decrement_video_generation_counter_v_1` for `VIDEOGEN`, release the reserved upload destination if one was created, and return an error to the caller. A RateLimiter request must not remain pending without a completion context row.
14. For image-based generation, Prakash normalizes/stages the image into the format Vast expects using `VIDEOGEN_VAST_IMAGE_STAGE_TIMEOUT_SECS`.
15. For `ServerDraft`, Prakash prepares the upload/draft destination needed after generation by obtaining a service-issued upload URL and `video_id`.
16. Prakash generates a non-guessable Vast `request_id`, preferably UUIDv4 backed by OS entropy, and stores it on the context. The Vast API must accept this caller-provided id as its request id.
17. Prakash submits the LTX job to Vast with:
    - LTX input.
    - RateLimiter request key.
    - User principal.
    - Prakash completion callback URL.
    - Image reference if applicable.
    - Upload URL/bucket destination.
    - Draft video id/object key.
    - Vast `request_id`.
    - No callback secret. Vast signs callbacks with its configured active completion HMAC key; Prakash never sends the raw HMAC secret in the generate request.
18. Vast returns an explicit acceptance response that echoes the same `request_id`.
19. Prakash verifies the echoed `request_id` exactly matches the stored `request_id`, then stores the Vast accepted state on the completion context.
20. Prakash returns the submitted request id, provider, and request key to the client.

Submission timeout policy:

- If Prakash times out waiting for the Vast submission response, Prakash treats the submission as not accepted: mark `submit_failed`, update RateLimiter to `Failed(reason)`, decrement the video generation counter, release the reserved upload destination when possible, and redact encrypted identity.
- It is possible Vast accepted the generation internally but the HTTP response was lost. If that happens and Vast later sends a completion callback for the same `request_id`, Prakash returns `409 Conflict` because the context is already terminal.
- The generated video, if any, is abandoned and should be handled by Vast/upload-service garbage collection. Usage is restored through the RateLimiter counter decrement, so the user can submit another request.
- This edge case is accepted in the lean design to avoid an additional synchronous status query during submission. If this becomes operationally common, add a Vast status lookup/reconciliation endpoint in a later hardening pass.

### Vast Submission API Contract

Prakash should call Vast with a request equivalent to:

```json
{
  "request_id": "<uuid-v4-generated-by-prakash>",
  "request_key": { "principal": "...", "counter": 123 },
  "user_principal": "...",
  "model_id": "ltx2",
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

`upload_url_refresh_url` is optional. Prakash sends it only when upload URL refresh is enabled.

The Vast API field is `request_id`. The value is generated by Prakash as a UUIDv4 and treated as the Vast request identifier everywhere in this flow. Use `uuid::Uuid::new_v4()` or an equivalent generator backed by OS entropy through `getrandom`/`rand`; do not use deterministic or seeded RNGs. Do not introduce a second differently named identifier field for this migration.

Prakash-to-Vast authentication:

- Vast `/generate` must require authenticated submissions.
- Prakash should send `Authorization: Bearer <VAST_API_KEY>` or an equivalent service credential agreed with Vast.
- Vast must reject missing/invalid credentials before accepting or queueing work.
- TLS is required for this request.

Accepted response:

```json
{
  "request_id": "<same-uuid-v4>",
  "status": "submitted",
  "accepted_at": "2026-05-27T11:00:00Z"
}
```

Acceptance rules:

- Prakash considers submission accepted only when Vast returns `200 OK` or `202 Accepted`, `status` is `submitted` or `queued`, and the response `request_id` exactly matches the submitted request id.
- A duplicate submit with the same `request_id` may return the same accepted response and must not create a second Vast generation.
- Any missing/mismatched `request_id`, ambiguous response body, or non-2xx response is treated as submit failure.
- Vast completion callbacks must include the same `request_id`.

Error response rules:

- Prakash must treat any non-2xx response, timeout, invalid JSON body, or missing required success fields as submit failure.
- Vast may return an error body shaped as `{ "error": { "code": "...", "message": "..." } }`, but Prakash must not depend on this body to make the failure decision.
- If an error body is present and parseable, Prakash should log the code/message with request ids and without prompt/image data.

## Completion Flow

1. Vast finishes generating the video.
2. Vast uploads the generated video to the configured bucket.
3. Vast persists a local outbox record containing request key, Vast `request_id`, video id/object key, bucket URL, checksum/size if available, and callback attempt metadata.
4. Vast deletes the local generated video only after the bucket upload succeeds and the outbox record can replay the completion callback after restart.
5. Vast calls Prakash completion endpoint with:
   - RateLimiter request key.
   - User principal.
   - Bucket video URL.
   - Video id or bucket object key.
   - Provider and Vast `request_id`.
   - Success or failure status.
   - Any failure reason.
   - File metadata when available.
   - HMAC-SHA256 completion signature.
6. Prakash validates the completion HMAC, including body hash verification, before parsing request body fields or mutating context.
7. Prakash starts a Postgres transaction and claims the context row with `SELECT ... FOR UPDATE` or an atomic state-transition `UPDATE ... RETURNING`.
8. Prakash verifies the request key, principal, non-guessable Vast `request_id`, and video id/object key match the stored context.
9. On success, Prakash records the bucket URL and uploaded video metadata in the completion context.
10. If `handle_video_upload == ServerDraft`, Prakash atomically transitions the context to `draft_creating` before calling the draft metadata service.
11. Prakash creates the actual draft using the stored encrypted delegated identity and the known video id/object key.
12. After draft creation succeeds, Prakash records `draft_created` and updates RateLimiter to `Complete(bucket_url)`.
13. On generation/upload failure, Prakash updates RateLimiter to `Failed(reason)`, releases the reserved upload destination when possible, and redacts encrypted identity unless an operator retry path is still valid.
14. Because usage has occurred after an accepted LTX job, this flow does not run token rollback or balance rollback.

All completion callbacks require valid HMAC headers, including failure callbacks. Success and failure paths must share the same signing implementation on Vast.

## RateLimiter Behavior

RateLimiter remains responsible for generation request accounting and status:

- `Pending` or `Processing`: visible through in-progress draft queries.
- `Complete(bucket_url)`: generation succeeded and the uploaded video URL is available.
- `Failed(reason)`: generation or bucket upload failed.

RateLimiter does not create the actual draft post. That remains a Prakash completion responsibility because it requires the user's delegated identity and upload metadata call.

The lean RateLimiter create call should preserve the request's token type for canister compatibility, but this flow should not perform paid-token balance deduction or model-cost lookup.

For the current `create_video_generation_request_v_2` canister API, Prakash should pass:

- `token_type`: value from the mobile request, defaulting to `Free` if absent.
- `is_registered`: same registration check behavior as off-chain, if required by the canister policy.
- `is_paid`: `false`.
- `payment_amount`: `None`.

This makes the no-deduction behavior explicit while still storing the user's token type on the RateLimiter request for compatibility.

If the canister supports an explicit transition to `Processing`, Prakash should set the request to `Processing` after Vast accepts the job. If not, leaving the request as `Pending` is still acceptable because the existing in-progress query treats both `Pending` and `Processing` as "being created."

If RateLimiter accepted a request but Vast submission is not accepted, Prakash should mark the context `submit_failed`, update RateLimiter to `Failed(reason)`, and call `decrement_video_generation_counter_v_1` for the `VIDEOGEN` property. The user did not receive service in this case, so usage should not be consumed.

## Draft Creation

For `ServerDraft`, Prakash must preserve enough completion context to create the draft after Vast completes.

Minimum context:

- RateLimiter request key.
- User principal.
- `handle_video_upload`.
- Encrypted delegated identity wire.
- Model/provider metadata needed for draft metadata.
- Draft video id or object key that maps the uploaded bucket object to the metadata/post record.

The delegated identity should be encrypted at rest and retained only long enough to complete the generation flow. It should be deleted or marked consumed after draft creation or terminal failure.

The migrated LTX endpoint only supports `ServerDraft`. Any other `upload_handling` variant is rejected before RateLimiter consumption, so Prakash does not need a non-draft completion path in this migration.

Draft creation must be idempotent. Completion retries must detect an existing draft for the request key/video id and no-op after the first successful draft creation.

Draft creation retry policy:

- Retry transient draft metadata failures up to `VIDEOGEN_DRAFT_CREATE_MAX_ATTEMPTS` (default: `3`) with bounded backoff.
- Keep the encrypted delegated identity until draft creation succeeds or the retry retention window expires.
- If draft creation succeeds, mark context `complete`, update RateLimiter to `Complete(bucket_url)`, and delete or irreversibly redact the encrypted delegated identity from the context.
- If draft creation fails after the retry budget, mark context `draft_failed`, update RateLimiter to `Failed("Draft creation failed after video upload")`, retain non-secret recovery metadata, and retain encrypted identity only for `VIDEOGEN_DRAFT_RETRY_RETENTION_HOURS` (default: `72`) for operator-triggered retry.
- Never leave the public RateLimiter request in `Pending` or `Processing` indefinitely after draft retry exhaustion.

Draft creation retry backoff:

- Initial backoff: `VIDEOGEN_DRAFT_CREATE_INITIAL_BACKOFF_SECS` (default: `5`).
- Max backoff: `VIDEOGEN_DRAFT_CREATE_MAX_BACKOFF_SECS` (default: `60`).
- Backoff strategy: exponential with jitter.
- With the default 3 attempts, retries fit inside `VIDEOGEN_DRAFT_CREATE_TIMEOUT_SECS=600`.

Encrypted delegated identity retention by state:

| State | Ciphertext policy |
| --- | --- |
| `complete` | Redact immediately after draft creation and RateLimiter completion succeed. |
| `draft_failed` | Retain for `VIDEOGEN_DRAFT_RETRY_RETENTION_HOURS` for operator retry, then redact. |
| `submit_failed` | Redact immediately; no draft retry is possible because Vast did not accept the job. |
| `stale_failed` | Redact immediately unless an operator explicitly reopens the context before terminalization. |
| `failed` | Redact immediately for generation/upload failures after RateLimiter is marked failed. |
| no context row | No ciphertext is stored for moderation/rate-limit rejections. |

## Moderation Service

Add a moderation client boundary rather than hard-coding the moderation provider directly into the route handler.

Expected behavior:

- Input: prompt text plus optional image reference or image payload.
- Output: scorecard with a boolean NSFW decision and optional category/confidence details.
- If NSFW is true: return a stable Prakash error response that mobile can map to the existing blocked-content message.
- If moderation service is unavailable in production: return a provider/moderation unavailable error before RateLimiter is consumed.
- For local development and staging before moderation service is live: support a config-gated mock mode that returns safe responses.

Configuration:

- `MODERATION_MODE=remote|mock_allow`.
- `MODERATION_SERVICE_URL`: required when mode is `remote`.
- `MODERATION_TIMEOUT_MS`: default `3000`.

Production guard:

- If `ENVIRONMENT=production`, startup must fail when `MODERATION_MODE=mock_allow`.
- Mock mode should be allowed only in local development or explicitly isolated staging.

This replaces the current Gemini-based off-chain moderation behavior for the migrated LTX flow.

## API Shape

### Generate Endpoint

`POST /api/v2/videogen/generate`

Request:

- Same videogen request body shape currently sent to off-chain.

Success response:

- Submitted operation id.
- Provider name.
- RateLimiter request key or equivalent operation id used by in-progress draft queries.

Error responses:

- `401`: invalid delegated identity or identity/user mismatch.
- `400`: invalid request or unsupported model/input combination.
- `400`: blocked by moderation service, serialized as an existing `VideoGenError::InvalidInput` payload.
- `429`: RateLimiter exceeded.
- `502` or `503`: moderation service, RateLimiter, or Vast submit unavailable.

### Provider Endpoints

`GET /api/v2/videogen/providers`

`GET /api/v2/videogen/providers-all`

If mobile's videogen base URL is switched fully to Prakash, these should return responses compatible with the current off-chain provider endpoints. At minimum, the production provider list should include the migrated LTX provider metadata needed by mobile to render and submit the flow.

### Completion Endpoint

`POST /api/v2/videogen/complete`

Authentication:

- Required for every request.
- Use HMAC-SHA256 with the configured completion HMAC key registry.
- Headers:
  - `X-Timestamp: <unix_seconds>`.
  - `X-Body-SHA256: <hex_sha256_of_raw_body>`.
  - `X-Key-Id: <completion_hmac_key_id>`.
  - `Authorization: HMAC-SHA256 <hex_signature>`.
- Signature message: `METHOD + "\n" + PATH + "\n" + X-Timestamp + "\n" + X-Body-SHA256`.
- Reject requests outside `VIDEOGEN_COMPLETION_HMAC_SKEW_SECS` (default: `120`).
- Query-param signatures are not allowed for this endpoint.
- Unknown `X-Key-Id` values return `401 Unauthorized` without attempting signature validation against other keys.

Completion HMAC key configuration:

- `VIDEOGEN_COMPLETION_HMAC_KEYS`: comma-separated key registry, e.g. `v1:<base64-32-byte-key>,v2:<base64-32-byte-key>`.
- `VIDEOGEN_COMPLETION_HMAC_ACTIVE_KEY_ID`: key id Vast uses for new signatures.
- Prakash validates against the key named by `X-Key-Id` if that key is present in the registry.
- Vast signs callback attempts at send time, not when the outbox record is created, so retried callbacks can use the active key after rotation.

Rotation protocol:

1. Add the new key to Prakash while keeping the old key accepted.
2. Deploy Vast with the new active key id and secret.
3. Verify Vast callbacks are using the new key id.
4. Keep the old key accepted for at least `VIDEOGEN_COMPLETION_HMAC_KEY_RETENTION_HOURS` (default: `72`) and until Vast outbox records signed with the old key have drained.
5. Remove the old key only after that overlap window.

If Prakash receives a callback signed with a known but inactive old key during the overlap window, it should still process the callback and log the old key id for rotation observability.

Request:

- Request key, required for every callback.
- User principal, required for every callback.
- Vast `request_id`, required for every callback.
- Provider.
- Success/failure status.
- Bucket video URL on success.
- Video id or bucket object key on success.
- Failure reason on failure.
- File size, content type, and checksum when available.

The HMAC signature is supplied only through the headers listed above, not through a request body field. This applies to both success and failure callbacks.
Failure callbacks must still include request key, user principal, provider, and Vast `request_id` so Prakash can authenticate and correlate them before marking RateLimiter `Failed`.

Response:

- `200` when the completion was accepted and status handling finished.
- `202` when another handler already claimed the same context and is processing it.
- `401` for invalid signature.
- `409` for authenticated but non-retryable conflicts such as mismatched `request_id` or callback after terminal failure.
- `5xx` only for retryable Prakash processing errors.

The completion endpoint must be idempotent. Repeated success callbacks for the same request key must not create duplicate drafts.

The endpoint should not trust fields that can be recomputed from Prakash state. It should treat the callback as a notification and verify it against the persisted completion context.

Vast retry behavior must treat `200`, `202`, and `409` as non-retryable terminal callback responses. It should retry only timeout, network error, and `5xx`.

A `202 Accepted` response means Prakash acknowledged the callback and Vast's delivery obligation is complete. If Prakash crashes after returning `202`, Prakash reconciliation owns recovery by re-attempting draft creation or terminalization from the persisted context. No additional Vast action is required.

## Vast LTX Changes

The Vast `videogen` service should add completion-side upload behavior:

- Determine the generated output file path after ComfyUI job completion. The current worker returns output `filename`/`subfolder` with `local_path: None`, so implementation must resolve the local path from `COMFYUI_OUTPUT_DIR`, `subfolder`, and `filename`.
- Upload the output file to the Prakash-provided bucket destination or configured scoped bucket path.
- Produce the final bucket URL.
- Delete the local file only after upload succeeds and durable callback replay metadata has been written to the outbox.
- Call Prakash completion endpoint with the final bucket URL and HMAC signature headers.
- On generation or upload failure, call Prakash completion endpoint with failure status and valid HMAC signature headers.

The service should not call off-chain-agent for this migrated flow.

If bucket upload fails, Vast must keep the local output file until retry policy or TTL cleanup decides otherwise. Immediate deletion on upload failure would make the job unrecoverable.

The existing Vast TTL cleanup task can remain as a fallback, but successful migrated jobs should perform immediate post-upload cleanup so disk use is bounded under load.

The webhook sender currently retries only a small fixed number of times. For this migration, Vast must add a durable completion outbox:

- Write an outbox record before the first Prakash completion callback attempt after bucket upload.
- Replay pending outbox records on service startup.
- Retry timeout, network error, and `5xx` responses using the configured exponential backoff policy from the retry section.
- Stop retrying and mark the outbox record terminal on `200`, `202`, or `409`.
- Delete local generated output only after upload success and durable outbox persistence. The file itself can be deleted before callback success because the outbox record contains the uploaded object metadata needed for replay.
- Keep outbox records for at least `VIDEOGEN_VAST_OUTBOX_RETENTION_HOURS` (default: `72`) after terminal callback result for debugging.

## Off-Chain Migration And Cleanup

Off-chain cleanup should be treated as a legacy-drain concern, not as part of the new migrated LTX runtime.

Before switching traffic:

- Identify any off-chain LTX jobs that can still complete through existing QStash and `/comfyui/webhook` callbacks.
- Keep off-chain callback routes live while those jobs are in flight.
- Keep current off-chain failure cleanup semantics for those jobs, including RateLimiter decrement and balance rollback where deducted amounts exist.

During rollout:

- New migrated LTX jobs must receive Prakash completion callback URLs, not off-chain callback URLs.
- New migrated LTX jobs must not include off-chain `encrypted_identity` webhook extras.
- New migrated LTX jobs must not call `/qstash/process_video_gen`, `/qstash/video_gen_callback`, or `/qstash/upload_ai_generated_video_to_canister_in_drafts`.

After drain:

- Disable or remove the off-chain LTX submit path and old LTX callback wiring.
- Leave unrelated off-chain videogen paths intact unless separately migrated.
- Verify no RateLimiter requests remain permanently `Pending`/`Processing` from the old path.
- Verify no QStash messages remain for old LTX video generation.
- Document the rollback switch: if traffic is moved back to off-chain, callback URLs and draft handling must also move back together.

## Security

- Validate delegated identity at Prakash generate time.
- Store encrypted delegated identity only for `ServerDraft`.
- Do not pass plaintext or encrypted delegated identity to Vast. Vast does not need user identity to generate or upload the file.
- Prefer scoped upload URLs over broad bucket credentials on Vast. If direct bucket credentials are required, make them write-only and path-scoped where possible.
- Protect the Prakash completion endpoint with the concrete versioned HMAC-SHA256 scheme defined above.
- Apply a small body size limit to the completion endpoint before buffering the raw body for HMAC verification. The completion payload should contain metadata only, not media bytes.
- Authenticate Prakash-to-Vast submission with a service credential such as `VAST_API_KEY`; Vast must not expose an open generation submission endpoint.
- Require TLS for all Prakash-to-Vast and Vast-to-Prakash traffic.
- Treat completion callbacks as untrusted until authenticated and request-key ownership is checked.
- Make completion idempotent to avoid duplicate draft creation on webhook retries.
- Redact prompt, image data, delegated identity, and bucket credentials from logs.
- Do not fetch arbitrary callback-provided URLs from Prakash. Use stored upload destination metadata and expected object keys to validate completion output.
- Require non-guessable Vast request ids. Prakash should generate UUIDv4 `request_id` values and store the exact expected id before submission.
- Add HTTP-layer rate limiting before delegated identity parsing and before moderation service/RateLimiter calls. Suggested controls: per-IP request rate, body size limit, and optional service gateway/WAF rules.

## Performance And Scope

The generate endpoint is "lean" relative to off-chain billing/QStash/DOLR work, but it still performs synchronous network work: moderation, RateLimiter create/check, optional image staging, upload destination preparation, Postgres context write, and Vast submission.

Target behavior:

- Non-image request p95 should stay under 20 seconds in staging.
- Image request p95 should stay under 45 seconds, assuming image staging is required.
- Each external call must have an explicit timeout.
- Vast submission is only the queueing handshake, not generation. Its timeout should be tight enough to preserve the synchronous endpoint target.

Cancellation is out of scope for this migration. There will be no user-facing cancel endpoint in the first implementation. Submitted jobs finish, fail, or time out through reconciliation.

## Error Handling

- NSFW rejection happens before RateLimiter request creation.
- Rate-limit rejection happens before Vast submission.
- Image staging failure after RateLimiter creation should mark the request failed. It should not call Vast.
- Vast submission failure after RateLimiter creation should mark the request failed and decrement the request counter. If Vast did not accept the job, the user was not served and usage should not be consumed.
- Generation failure after Vast accepts the job should mark RateLimiter failed and should count as usage.
- Bucket upload failure should mark RateLimiter failed and should count as usage because generation ran.
- Completion callback authentication failure should not update RateLimiter or draft state.
- Duplicate completion callbacks should not create duplicate drafts.
- Draft creation failure after successful bucket upload should keep enough context for retry. After retry exhaustion, mark RateLimiter `Failed(...)` rather than leaving the request in-progress indefinitely.
- Legacy off-chain failures should continue using the old cleanup policy until drained. Migrated Prakash failures should use the new policy in this spec and should not trigger off-chain rollback/decrement code.
- Reconciliation and late callback precedence: terminal states are final. A late success callback after `stale_failed` or another terminal failure returns `409 Conflict`; Vast treats this as non-retryable. Operator recovery must be explicit and separate.
- Reconciliation must not move Postgres into a terminal state that depends on a RateLimiter update until the required RateLimiter canister call succeeds. If the RateLimiter canister is unavailable, leave the context in its pre-terminal state, keep `updated_at` unchanged, record the failure in `last_reconciliation_error`/metrics, and retry on the next reconciliation run.
- Reconciliation attempt counters increment only when a reconciliation action successfully mutates Postgres state. Canister-unavailable skips update metrics and `last_reconciliation_error` but do not consume retry budget or bump the reconciliation attempt counter.

## State Machine

Prakash should treat each persisted generation request as a small state machine. Only states that have a Postgres context row belong in the persisted state machine; Vast-local states such as "generated and uploading" stay in Vast logs/outbox unless Vast adds a separate upload-start notification.

Ephemeral handler phases before context creation:

- `moderating`: request accepted by HTTP handler, before moderation decision. No Postgres context row exists.
- `rate_limited`: terminal rejection before usage is consumed. No Postgres context row exists.

Persisted states:

1. `context_created`: RateLimiter accepted and completion context persisted.
2. `submitted`: Vast accepted the job and returned the echoed `request_id`.
3. `uploaded`: Vast uploaded to bucket and called Prakash.
4. `draft_creating`: Prakash claimed the context and is creating draft metadata.
5. `draft_created`: Prakash created the actual draft.
6. `complete`: RateLimiter marked `Complete(bucket_url)`.
7. `submit_failed`: Vast did not accept the job; RateLimiter counter should be decremented.
8. `stale_failed`: reconciliation timed out the job before completion.
9. `draft_failed`: video uploaded but draft creation exhausted retry budget.
10. `failed`: terminal generation/upload failure with reason.

Not every state needs to be exposed publicly, but the persisted context must distinguish submitted, uploaded, draft-created, and failed states for retries and support.

Allowed persisted transitions:

| From | To | Trigger |
| --- | --- | --- |
| `context_created` | `submitted` | Vast accepts submission and echoes `request_id`. |
| `context_created` | `submit_failed` | Vast submission fails or reconciliation times out before submission. |
| `submitted` | `uploaded` | Authenticated success callback from Vast with matching `request_id` and object metadata. |
| `submitted` | `failed` | Authenticated generation/upload failure callback from Vast. |
| `submitted` | `stale_failed` | Reconciliation exceeds generation timeout plus completion grace. |
| `uploaded` | `draft_creating` | Prakash claims context for ServerDraft metadata creation. |
| `draft_creating` | `draft_created` | Draft metadata service succeeds. |
| `draft_creating` | `draft_failed` | Draft retry budget is exhausted. |
| `draft_created` | `complete` | RateLimiter `Complete(bucket_url)` update succeeds. |

Terminal states are absorbing: `complete`, `submit_failed`, `stale_failed`, `draft_failed`, and `failed` accept no further state transitions except explicit operator-triggered reopening.

## Retry And Recovery

- moderation service call failures should be fail-closed in production and fail-open only in explicit local/mock mode.
- Vast submission can be retried by Prakash only if the request is still before `submitted`.
- Vast bucket upload can be retried by Vast while the local file exists.
- Prakash completion can be retried by Vast. Prakash must make this idempotent.
- Draft creation can be retried by Prakash using the persisted completion context.
- Prakash should have a reconciliation job for stale contexts so encrypted identities and half-completed rows do not accumulate indefinitely.
- Reconciliation processes at most `VIDEOGEN_RECONCILIATION_BATCH_SIZE` stale contexts per state per run.
- Reconciliation for `uploaded` starts or resumes draft creation using the persisted completion context.
- Reconciliation for `draft_creating` re-attempts draft creation using the persisted encrypted identity, increments the draft attempt counter toward `VIDEOGEN_DRAFT_CREATE_MAX_ATTEMPTS`, and marks `draft_failed` plus RateLimiter `Failed(reason)` only after attempts are exhausted.
- Reconciliation for `draft_created` retries the RateLimiter `Complete(bucket_url)` call and transitions to `complete` after it succeeds.
- If Prakash is down during completion, Vast should retry webhook delivery with bounded backoff.
- Vast must persist enough outbox metadata to retry completion callbacks after process restart.
- If Vast exhausts callback retries but the outbox record remains, an operator can replay it manually.
- Off-chain should be monitored during rollout for old in-flight LTX requests and QStash messages. Only after that queue drains should old off-chain LTX cleanup code be disabled.

Default timeout and retry config:

- `VIDEOGEN_GENERATE_DEDUPE_WINDOW_SECS=120`.
- `VIDEOGEN_VAST_SUBMIT_TIMEOUT_SECS=10`.
- `VIDEOGEN_UPLOAD_URL_PRE_SUBMIT_MARGIN_SECS=10`. This happens to match the Vast submit HTTP timeout by default, but it is an independent value used only to size upload URL TTL.
- `VIDEOGEN_VAST_IMAGE_STAGE_TIMEOUT_SECS=30`.
- `VIDEOGEN_CONTEXT_CREATED_TIMEOUT_SECS=120`.
- `VIDEOGEN_LTX_GENERATION_TIMEOUT_SECS=1800`.
- `VIDEOGEN_COMPLETION_RETRY_GRACE_SECS=900`.
- `VIDEOGEN_VAST_UPLOAD_RETRY_WINDOW_SECS=900`.
- `VIDEOGEN_VAST_UPLOAD_EXPIRY_REFRESH_MARGIN_SECS=300`.
- `VIDEOGEN_UPLOAD_URL_SAFETY_BUFFER_SECS=300`.
- `VIDEOGEN_UPLOAD_URL_TTL_SECS=4200`.
- `VIDEOGEN_RECONCILIATION_INTERVAL_SECS=60`.
- `VIDEOGEN_RECONCILIATION_BATCH_SIZE=100`.
- Reconciliation marks `submitted` contexts stale after `VIDEOGEN_LTX_GENERATION_TIMEOUT_SECS + VIDEOGEN_COMPLETION_RETRY_GRACE_SECS`, updates RateLimiter to `Failed(reason)`, releases reserved upload destination when possible, and redacts encrypted identity.
- Reconciliation marks `context_created` contexts as `submit_failed` after `VIDEOGEN_CONTEXT_CREATED_TIMEOUT_SECS`, updates RateLimiter to `Failed(reason)`, decrements the video generation counter, releases reserved upload destination when possible, and redacts encrypted identity.
- Reconciliation re-attempts `uploaded` contexts after `VIDEOGEN_DRAFT_CREATE_TIMEOUT_SECS` by starting draft creation.
- Reconciliation re-attempts `draft_creating` contexts after `VIDEOGEN_DRAFT_CREATE_TIMEOUT_SECS`, then marks `draft_failed` only after retry budget is exhausted.
- Reconciliation retries `draft_created` contexts after `VIDEOGEN_DRAFT_CREATED_COMPLETE_TIMEOUT_SECS` by re-running RateLimiter `Complete(bucket_url)`.
- `VIDEOGEN_DRAFT_CREATE_MAX_ATTEMPTS=3`.
- `VIDEOGEN_DRAFT_CREATE_INITIAL_BACKOFF_SECS=5`.
- `VIDEOGEN_DRAFT_CREATE_MAX_BACKOFF_SECS=60`.
- `VIDEOGEN_DRAFT_CREATE_TIMEOUT_SECS=600`.
- `VIDEOGEN_DRAFT_CREATED_COMPLETE_TIMEOUT_SECS=120`.
- `VIDEOGEN_DRAFT_RETRY_RETENTION_HOURS=72`.
- `VIDEOGEN_VAST_OUTBOX_RETENTION_HOURS=72`.
- `VIDEOGEN_VAST_CALLBACK_MAX_RETRIES=10`.
- `VIDEOGEN_VAST_CALLBACK_INITIAL_BACKOFF_SECS=10`.
- `VIDEOGEN_VAST_CALLBACK_MAX_BACKOFF_SECS=120`.
- `VIDEOGEN_COMPLETION_HMAC_SKEW_SECS=120`.
- `VIDEOGEN_COMPLETION_HMAC_KEY_RETENTION_HOURS=72`.
- `VIDEOGEN_VAST_STAGED_IMAGE_TTL_HOURS=24`.

`VIDEOGEN_VAST_STAGED_IMAGE_TTL_HOURS` must be greater than `VIDEOGEN_LTX_GENERATION_TIMEOUT_SECS / 3600`. With the default generation timeout of 1800 seconds, the minimum staged-image TTL is 1 hour; the default 24 hours leaves operational buffer.

The reconciliation job should run every `VIDEOGEN_RECONCILIATION_INTERVAL_SECS` by default. With the default interval of 60 seconds and `VIDEOGEN_CONTEXT_CREATED_TIMEOUT_SECS=120`, a crash before Vast submission can remain visible as in-progress for roughly 2-3 minutes before reconciliation transitions it to failed. Stale contexts must move to a terminal state and must not remain visible as in-progress forever.

There is a possible visibility gap after Vast exhausts callback retries: Vast may stop retrying before Prakash reaches `VIDEOGEN_LTX_GENERATION_TIMEOUT_SECS + VIDEOGEN_COMPLETION_RETRY_GRACE_SECS`. During that gap, mobile can still see the job as in-progress from RateLimiter. This is inherent to the no-polling async design. If LTX jobs usually complete much faster than 30 minutes, lower `VIDEOGEN_LTX_GENERATION_TIMEOUT_SECS` in production to reduce this window.

Vast callback retry policy:

- Retry timeout, network error, and `5xx` responses.
- Use exponential backoff starting at `VIDEOGEN_VAST_CALLBACK_INITIAL_BACKOFF_SECS`, capped by `VIDEOGEN_VAST_CALLBACK_MAX_BACKOFF_SECS`.
- Stop after `VIDEOGEN_VAST_CALLBACK_MAX_RETRIES` attempts or after `VIDEOGEN_COMPLETION_RETRY_GRACE_SECS`, whichever comes first.
- With the default 10 attempts, 10-second initial backoff, and 120-second cap, retries fit inside the 900-second completion grace window.

## Observability

Add structured logs and Sentry context for:

- `operation_id`.
- RateLimiter request key.
- Vast `request_id`.
- moderation decision, without logging raw prompt/image.
- Vast submit result.
- Bucket upload result.
- Completion callback authentication result.
- Draft creation result.
- State transitions and retry counts.

The design should avoid logging delegated identities, image payloads, bucket credentials, or raw prompts.

Metrics to define before rollout:

- `videogen_generate_requests_total{status,provider,model}`.
- `videogen_generate_duration_ms{provider,model}` histogram.
- `videogen_moderation_requests_total{result}` and `videogen_moderation_duration_ms` histogram.
- `videogen_vast_submit_total{result}` and `videogen_vast_submit_duration_ms` histogram.
- `videogen_completion_callbacks_total{result,provider}`.
- `videogen_completion_hmac_failures_total{reason}`.
- `videogen_contexts_by_state{state}` gauge.
- `videogen_reconciliation_actions_total{action,state}`.
- `videogen_draft_creation_total{result}` and `videogen_draft_creation_duration_ms` histogram.
- `videogen_vast_outbox_pending` gauge on Vast.

Alerting should cover sustained HMAC failures, growth in `submitted`/`draft_creating` contexts, stale context reconciliation counts, and Vast outbox backlog.

## Testing Strategy

Unit tests:

- Delegated identity mismatch returns unauthorized.
- NSFW rejection does not create RateLimiter request.
- RateLimit rejection does not submit to Vast.
- Unsupported `upload_handling` returns `400 InvalidInput` before moderation or RateLimiter creation.
- Request fingerprint uses canonical JSON, the expected field set, and decoded-image SHA-256 for base64 images.
- Generate retry with matching fingerprint inside dedupe window returns the existing operation id and request key.
- Base64 image request is sent to moderation service before image staging.
- Image staging uses `VIDEOGEN_VAST_IMAGE_STAGE_TIMEOUT_SECS` and fails the RateLimiter request without submitting to Vast on timeout.
- Image staging failure after RateLimiter acceptance marks request failed and does not submit to Vast.
- Safe request submits to Vast with request key, Prakash callback URL, caller-provided UUID `request_id`, Vast auth header, and upload destination expiry.
- Vast acceptance without a matching echoed `request_id` is treated as submit failure.
- Vast non-2xx, invalid JSON, and parseable error bodies all produce submit failure without consuming usage.
- Safe `ServerDraft` request stores encrypted delegated identity in durable completion context.
- Completion success updates RateLimiter to complete.
- Completion failure updates RateLimiter to failed.
- Completion rejects oversized bodies before buffering/parsing.
- Completion failure callbacks require the same HMAC headers as success callbacks.
- `ServerDraft` completion uses encrypted identity and does not create duplicate drafts on retry.
- Two concurrent completion callbacks cannot both create a draft.
- Completion with invalid auth does not update RateLimiter.
- Completion signed with an old key inside the rotation overlap succeeds; unknown key id fails.
- Completion with mismatched principal or `request_id` is rejected.
- Completion after terminal `stale_failed` returns non-retryable conflict and does not mutate state.
- Delegated identity decryption failure at completion does not create a draft or silently succeed.
- Vast submit failure after RateLimiter creation decrements usage.
- Postgres context creation failure after RateLimiter acceptance updates RateLimiter to failed, decrements usage, releases upload destination when possible, and returns an error.
- `context_created` timeout marks RateLimiter failed, decrements usage, releases upload destination when possible, and redacts encrypted identity.
- RateLimiter canister unavailability during reconciliation leaves the context in its pre-terminal state and does not bump `updated_at`.
- RateLimiter canister unavailability during reconciliation does not increment the reconciliation attempt counter.
- Submission timeout marks `submit_failed`; a later completion for the same `request_id` returns `409`.
- `submitted` stale reconciliation releases upload destination when possible and redacts encrypted identity after RateLimiter failure update succeeds.
- Reconciliation enforces `VIDEOGEN_RECONCILIATION_BATCH_SIZE` per state per run.
- `uploaded` timeout starts or resumes draft creation.
- `draft_creating` timeout retries draft creation or marks `draft_failed` after retry exhaustion.
- `draft_created` timeout retries RateLimiter `Complete(bucket_url)` and transitions to `complete`.
- Terminal states apply the encrypted identity retention table.

Integration tests:

- Mock moderation service, RateLimiter, Vast, and upload metadata service.
- Verify the full safe path from generate to completion.
- Verify success response matches mobile's current `GenerateVideoSuccessDto`.
- Verify `providers` and `providers-all` deserialize through the actual mobile DTO structs copied/imported into the contract-test crate, including field names, optionality, and unknown-field behavior.
- Verify NSFW, rate-limit, auth, and provider errors parse through mobile's current `VideoGenErrorDto` shape.
- Verify upload URL expiry is long enough for configured generation plus retry windows, or Vast can refresh the URL through an authenticated Prakash refresh endpoint.
- Verify unused upload destinations are released or expire through upload-service TTL on generation failure.
- Verify upload metadata/draft creation is idempotent on `video_id` or the agreed idempotency key.
- Verify draft-failed orphaned `video_id`/object keys are marked for deletion or garbage collection after retry retention expires.
- Verify Vast duplicate submit with the same `request_id` does not create a second generation.
- Verify Vast callback retry schedule stays within `VIDEOGEN_COMPLETION_RETRY_GRACE_SECS`.
- Verify upload URL refresh endpoint returns a fresh scoped URL only for matching HMAC-authenticated request key/principal/`request_id`/object key.
- Verify upload URL refresh endpoint with invalid HMAC returns `401`.
- Verify upload URL refresh endpoint with unknown or mismatched `request_id` returns `409`.
- Verify Vast calls upload URL refresh before upload when `expires_at` is within `VIDEOGEN_VAST_UPLOAD_EXPIRY_REFRESH_MARGIN_SECS`.
- Verify staged I2V images older than `VIDEOGEN_VAST_STAGED_IMAGE_TTL_HOURS` are cleaned even if no Vast generation was submitted.
- Verify Vast upload failure calls completion failure and does not delete local file before successful upload.
- Verify Vast deletes local file after successful bucket upload.
- Verify Vast resolves generated output paths correctly from `COMFYUI_OUTPUT_DIR`, filename, and subfolder.
- Verify staged image inputs do not accumulate indefinitely on Vast.
- Verify retrying the same completion callback does not duplicate the draft.
- Verify service restart between generate and completion still allows draft creation from persisted context.
- Verify Vast outbox replay sends completion after Vast process restart.
- Verify reconciliation marks stale `submitted` contexts failed after configured timeout.
- Verify old off-chain LTX jobs still complete through legacy callbacks during rollout.
- Verify migrated LTX jobs never hit off-chain QStash or off-chain ComfyUI callback handlers.

Manual smoke test:

- Submit an LTX request.
- Confirm it appears in in-progress drafts.
- Confirm Vast uploads generated video to bucket.
- Confirm local Vast output file is removed after upload.
- Confirm Prakash completion updates RateLimiter.
- Confirm generated video appears as a draft.

## Rollout

1. Add Prakash generate endpoint and HMAC-protected completion endpoint behind config. The completion endpoint must not be enabled without HMAC verification.
2. Add a moderation client with mock mode for non-production while moderation service is not live.
3. Add Postgres completion-context persistence, encryption key registry, and transactional idempotency.
4. Confirm the upload service can issue scoped upload URLs valid for at least `VIDEOGEN_UPLOAD_URL_TTL_SECS=4200`. If it cannot, implement `/api/v2/videogen/upload-url/refresh` as part of upload destination preparation, not later.
5. Add upload destination preparation for Vast using a service-issued upload URL and `video_id`.
6. Confirm upload metadata/draft service idempotency on `video_id` or an agreed idempotency key.
7. Confirm and implement the Vast submission contract: caller-provided `request_id`, Prakash-to-Vast auth, echoed acceptance response, duplicate submit behavior, and callback `request_id` echo.
8. Add Vast bucket-upload, durable outbox, and Prakash-completion behavior behind config.
9. Add cleanup/reconciliation jobs for Prakash completion contexts and Vast generated/staged files.
10. Add provider endpoint compatibility if mobile's videogen base URL will point fully to Prakash, including contract tests for both `providers` and `providers-all`.
11. Run end-to-end smoke test in staging.
12. Gate mobile cutover on passing generate/error/provider contract tests against mobile DTOs.
13. Switch mobile's videogen base URL to Prakash when staging behavior matches off-chain.
14. Keep off-chain LTX path available as rollback until production confidence is established.
15. Drain old off-chain LTX jobs and QStash messages.
16. Gate or remove old off-chain LTX routing after the drain is verified.

Rollback triggers while off-chain remains available:

- User-facing generation success rate drops below the agreed launch SLO for two consecutive measurement windows.
- `submitted` or `draft_creating` contexts accumulate faster than reconciliation clears them.
- `stale_failed` or `draft_failed` ratio exceeds the agreed launch threshold.
- `videogen_completion_hmac_failures_total` spikes outside expected deploy/rotation windows.
- Vast outbox backlog grows continuously for more than one generation timeout window.
- RateLimiter canister errors prevent reconciliation from terminalizing stale requests for a sustained period.

Rollback means moving both mobile-facing generate traffic and completion callback URLs back to the off-chain path together. Splitting those two paths during rollback can strand in-flight jobs.

## Off-Chain Drain Runbook

Before disabling the old off-chain LTX route:

1. Query RateLimiter for users or recent windows that have `Pending`/`Processing` requests created before the Prakash cutover timestamp.
2. Check QStash for outstanding `/qstash/process_video_gen`, `/qstash/video_gen_callback`, and `/qstash/upload_ai_generated_video_to_canister_in_drafts` messages.
3. Keep off-chain `/comfyui/webhook` and QStash callback routes live until both checks are clear or the remaining requests are manually failed.
4. For requests stuck beyond the legacy timeout, update RateLimiter to `Failed(reason)` using existing off-chain tooling and apply the legacy decrement/rollback policy where applicable.
5. Record the drain timestamp and disable only the old LTX submit/callback wiring, leaving unrelated off-chain providers intact.

## External Follow-Ups

- Confirm the exact moderation service request/response schema when the service is live. The Prakash client boundary is fixed to prompt/image input plus a boolean NSFW decision and optional scorecard metadata.
- Confirm the exact upload-service bucket URL/object-key format while implementing upload destination preparation. The security contract remains fixed: Prakash validates callback output against stored `video_id`/object key and does not fetch arbitrary callback URLs.
- Turn the off-chain drain runbook into release commands before disabling legacy LTX callbacks. The release gate is fixed: old RateLimiter `Pending`/`Processing` requests are resolved and old QStash LTX queues are empty or manually drained.
- Keep the existing unpaginated in-progress endpoint behavior for this migration. Pagination can be handled as a separate hardening task if RateLimiter responses become too large.
