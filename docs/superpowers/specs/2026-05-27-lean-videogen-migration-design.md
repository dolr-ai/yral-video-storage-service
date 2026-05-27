# Lean Videogen Migration Design

Date: 2026-05-27

## Summary

Move the mobile-facing video generation flow out of off-chain-agent and into the Prakash service while keeping the endpoint lean. The new Prakash endpoint will accept the same videogen request payload shape mobile sends today, validate the delegated identity, run prompt/image moderation through Ansuman, create a RateLimiter request when allowed, submit directly to the LTX service on Vast, and return the submitted request identifier immediately.

The Vast LTX service owns generation output handling. When generation completes, it uploads the video to the bucket, deletes local disk output only after upload succeeds, and calls a Prakash completion endpoint. Prakash then updates RateLimiter and creates the user's draft when the request asked for server-side draft handling.

## Goals

- Preserve the existing mobile request contract for videogen.
- Keep the Prakash `/api/v2/videogen/generate` path small and synchronous.
- Add an Ansuman moderation seam for prompt/image NSFW scorecards before rate limit consumption.
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
   - Owns Ansuman moderation call.
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

The deeper review surfaced three design constraints that must be explicit before implementation:

1. **Mobile compatibility is stricter than "same params."** Mobile currently sends `delegated_identity`, a nested `request`, `upload_handling`, text prompt, optional base64 image, `token_type`, and `user_id`. It expects the existing success response shape: `operation_id`, `provider`, and `request_key`. Error bodies must remain compatible with the current `VideoGenError` enum shape so mobile can parse the message and map the HTTP status to its existing error UI.
2. **Bucket URL alone may not be enough to create a draft.** The existing draft creation path eventually calls upload metadata with a video id. If Vast uploads directly to a bucket, Prakash must know the resulting video id/object key and must be able to map it to the draft metadata call. The safest design is for Prakash to create or reserve the destination before calling Vast, then pass that destination to Vast with the generation request.
3. **Completion context must be durable.** The generation request returns immediately while completion happens later. Anything needed to create the draft, authenticate completion, or make callbacks idempotent cannot live only in memory. Prakash should persist a small completion-context row keyed by RateLimiter request key.
4. **Cleanup is split across three repos during migration.** Vast owns generated-file cleanup for migrated jobs, Prakash owns completion-context cleanup, and off-chain owns only legacy in-flight QStash/callback cleanup until the old LTX route is drained.

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

- `operation_id`: string formatted so existing mobile logging and UI can keep using it. The current shape is `<principal>_<counter>`.
- `provider`: `Ltx2` or the selected provider label.
- `request_key`: `{ principal, counter }`.

The existing in-progress endpoint can continue to serve `Pending` and `Processing` RateLimiter records. Mobile polling through the RateLimiter request key also remains valid as long as Prakash updates the same request key to a terminal status.

Mobile also has existing provider-list calls under the videogen API base. For a no-code mobile rollout, Prakash should either expose compatible `/api/v2/videogen/providers` and `/api/v2/videogen/providers-all` responses, or the rollout must keep provider discovery pointed at off-chain while only moving generate. The cleaner migration is to expose the compatible provider endpoints in Prakash for the migrated LTX provider set.

### Error Payload Contract

The generate endpoint should return errors in the existing `VideoGenError` JSON representation:

- Identity failure: `401` with `AuthError`.
- Invalid model/input: `400` with `InvalidInput(message)`.
- NSFW rejection: `400` with `InvalidInput(message)` and a stable user-safe message such as "Content violates safety guidelines".
- Rate limit exceeded: `429` with a parseable `ProviderError(message)` or existing RateLimiter-compatible error variant.
- Vast submit unavailable: `503` with `NetworkError(message)` or `ProviderError(message)`.
- Ansuman unavailable in production: `503` with `NetworkError(message)`.

Mobile maps status codes to existing error types, so introducing a new error variant would require mobile work and is outside this migration.

### Image/Text Input Contract

Ansuman must receive the same content the user submitted: prompt plus optional image. For image requests, Prakash must support mobile's current base64 image shape.

For Vast submission, Prakash should normalize image input to the format Vast expects. The current Vast worker supports `image_url` for I2V, and also exposes `/upload/image`. A practical implementation path is:

1. Use the original prompt/image for Ansuman moderation.
2. After moderation and RateLimiter success, upload the image to Vast `/upload/image` or another configured image staging location.
3. Pass the resulting image URL/reference to Vast `/generate`.

This image staging step is allowed in the lean endpoint because it is part of submitting the job. It is distinct from the removed DOLR, billing, JWT, and QStash work.

### Completion Context Contract

Prakash should persist a completion context after RateLimiter accepts the request and before Vast submission:

- `principal`.
- `counter`.
- `operation_id`.
- `provider`.
- `model_id`.
- `prompt`.
- `upload_handling`.
- encrypted delegated identity, only for `ServerDraft`.
- bucket destination or upload destination for Vast.
- draft video id/object key.
- Vast job id once submission succeeds.
- terminal processing state: pending, submitted, completed, draft_created, failed.
- timestamps and last error.

This context should have a unique key on `(principal, counter)` so duplicate completion callbacks cannot create duplicate drafts.

If Vast submission fails after context creation, Prakash should mark the context failed and update RateLimiter according to the failure policy below.

### Bucket Upload Contract

The team-selected flow requires Vast to upload the generated video to the bucket. To reduce credential exposure on the GPU server, prefer passing a scoped upload destination to Vast rather than giving Vast broad bucket credentials.

Recommended destination options:

1. Prakash obtains a pre-signed or service-issued upload URL and a `video_id`, then passes both to Vast.
2. Prakash generates a deterministic bucket object key from the RateLimiter request key and passes that key to Vast, with Vast configured with narrowly scoped write credentials.

Either way, Vast completion must return:

- `bucket_url`.
- `video_id` or object key.
- file size and content type when available.
- checksum/hash when available.
- request key and Vast job id.

Prakash should not attempt draft creation from only an opaque URL unless the downstream metadata service can accept that URL without a video id.

### Repository Impact And Cleanup Contract

This migration touches three repositories with different cleanup responsibilities.

**Prakash / yral-video-storage-service**

- Add the mobile-facing generate endpoint and Vast completion endpoint.
- Store durable completion context for migrated jobs.
- Delete or mark encrypted delegated identity as consumed after terminal completion/failure.
- Add a periodic cleanup/reconciliation job for stale completion contexts:
  - `context_created` but never submitted to Vast.
  - `submitted` but no completion callback after the expected max generation window.
  - `uploaded` but draft creation failed and retry budget is exhausted.
- Keep the existing in-progress draft endpoint backed by RateLimiter.
- Do not call off-chain's QStash video callback for migrated LTX jobs.

**Vast / videogen**

- Resolve concrete local output file paths from ComfyUI output metadata. Current output metadata has `filename` and `subfolder`, but `local_path` is `None`; the migration needs to derive the path from `COMFYUI_OUTPUT_DIR`.
- Upload the generated MP4 to the Prakash-provided destination.
- Delete the generated local MP4 only after upload succeeds and Prakash completion callback is accepted or safely retryable.
- Keep the existing TTL cleanup task as a fallback, but do not rely on it as the primary cleanup path for successful migrated jobs.
- Add cleanup for staged I2V input images if ComfyUI `/upload/image` stores them in a persistent input directory. The current cleanup task only scans the output directory for video extensions.

**off-chain-agent**

- Keep current QStash video generation, `/comfyui/webhook`, and `/qstash/video_gen_callback` behavior until all legacy off-chain LTX jobs have completed or failed.
- Keep legacy failure cleanup for those old jobs: RateLimiter failure update, counter decrement, and balance rollback when applicable.
- After traffic moves to Prakash and old jobs drain, gate or remove the off-chain LTX path so new LTX jobs cannot accidentally be submitted through QStash.
- Do not remove unrelated off-chain videogen providers or callbacks unless their traffic is also migrated.
- Do not share encrypted delegated identity blobs between off-chain and Prakash; each path owns its own completion context.

## Generate Request Flow

1. Mobile calls Prakash `/api/v2/videogen/generate` using the same videogen params it sends today.
2. Prakash validates the delegated identity can be parsed.
3. Prakash derives the sender principal from the delegated identity.
4. Prakash parses `user_id` and verifies `identity.sender() == user_id`.
5. Prakash extracts the model id, prompt, image input, token type, and `handle_video_upload`.
6. Prakash sends the prompt/image combo to Ansuman moderation.
7. If Ansuman returns NSFW, Prakash returns an NSFW error response immediately. No RateLimiter request is created.
8. If moderation passes, Prakash calls RateLimiter to create/check the video generation request.
9. If RateLimiter rejects the request, Prakash returns a rate-limit error immediately.
10. If RateLimiter accepts, Prakash persists the minimal completion context keyed by the returned RateLimiter request key.
11. For image-based generation, Prakash normalizes/stages the image into the format Vast expects.
12. For `ServerDraft`, Prakash prepares the upload/draft destination needed after generation. This can be a reserved `video_id`, a bucket object key, or a scoped upload URL.
13. Prakash submits the LTX job to Vast with:
    - LTX input.
    - RateLimiter request key.
    - User principal.
    - Prakash completion callback URL.
    - Image reference if applicable.
    - Bucket upload destination.
    - Draft video id/object key.
    - Completion auth material or callback signature configuration.
14. Prakash stores the Vast job id on the completion context.
15. Prakash returns the submitted request id, provider, and request key to the client.

## Completion Flow

1. Vast finishes generating the video.
2. Vast uploads the generated video to the configured bucket.
3. Vast deletes the local generated video only after the bucket upload succeeds.
4. Vast calls Prakash completion endpoint with:
   - RateLimiter request key.
   - User principal.
   - Bucket video URL.
   - Video id or bucket object key.
   - Provider/job id.
   - Success or failure status.
   - Any failure reason.
   - File metadata when available.
   - Completion authentication token or signature.
5. Prakash validates the completion request.
6. Prakash loads the persisted completion context and verifies the request key, principal, and job id match.
7. On success, Prakash records the bucket URL and uploaded video metadata in the completion context.
8. If `handle_video_upload == ServerDraft`, Prakash creates the actual draft using the stored encrypted delegated identity and the known video id/object key.
9. After draft creation succeeds, Prakash updates RateLimiter to `Complete(bucket_url)`.
10. On generation/upload failure, Prakash updates RateLimiter to `Failed(reason)`.
11. Because usage has occurred after an accepted LTX job, this flow does not run token rollback or balance rollback.

## RateLimiter Behavior

RateLimiter remains responsible for generation request accounting and status:

- `Pending` or `Processing`: visible through in-progress draft queries.
- `Complete(bucket_url)`: generation succeeded and the uploaded video URL is available.
- `Failed(reason)`: generation or bucket upload failed.

RateLimiter does not create the actual draft post. That remains a Prakash completion responsibility because it requires the user's delegated identity and upload metadata call.

The lean RateLimiter create call should preserve the request's token type for canister compatibility, but this flow should not perform paid-token balance deduction or model-cost lookup. If the canister API requires `is_paid` and `payment_amount`, the migration should pass values that represent no service-side payment collection for this path.

If the canister supports an explicit transition to `Processing`, Prakash should set the request to `Processing` after Vast accepts the job. If not, leaving the request as `Pending` is still acceptable because the existing in-progress query treats both `Pending` and `Processing` as "being created."

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

If a request does not require `ServerDraft`, Prakash should not store delegated identity beyond synchronous validation.

Draft creation must be idempotent. Completion retries should either detect an existing draft for the request key/video id or no-op after the first successful draft creation.

## Ansuman Moderation

Add a moderation client boundary rather than hard-coding Ansuman directly into the route handler.

Expected behavior:

- Input: prompt text plus optional image reference or image payload.
- Output: scorecard with a boolean NSFW decision and optional category/confidence details.
- If NSFW is true: return a stable Prakash error response that mobile can map to the existing blocked-content message.
- If Ansuman is unavailable in production: return a provider/moderation unavailable error before RateLimiter is consumed.
- For local development and staging before Ansuman is live: support a config-gated mock mode that returns safe responses.

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
- `400`: blocked by Ansuman, serialized as an existing `VideoGenError::InvalidInput` payload.
- `429`: RateLimiter exceeded.
- `502` or `503`: Ansuman, RateLimiter, or Vast submit unavailable.

### Provider Endpoints

`GET /api/v2/videogen/providers`

`GET /api/v2/videogen/providers-all`

If mobile's videogen base URL is switched fully to Prakash, these should return responses compatible with the current off-chain provider endpoints. At minimum, the production provider list should include the migrated LTX provider metadata needed by mobile to render and submit the flow.

### Completion Endpoint

`POST /api/v2/videogen/complete`

Request:

- Request key.
- User principal.
- Vast job id.
- Provider.
- Success/failure status.
- Bucket video URL on success.
- Video id or bucket object key on success.
- Failure reason on failure.
- File size, content type, and checksum when available.
- Completion authentication token or signature.

Response:

- `200` when the completion was accepted and status handling finished.
- Non-2xx for invalid signature, invalid request key, or unrecoverable Prakash processing error.

The completion endpoint should be idempotent where practical. Repeated success callbacks for the same request key should not create duplicate drafts.

The endpoint should not trust fields that can be recomputed from Prakash state. It should treat the callback as a notification and verify it against the persisted completion context.

## Vast LTX Changes

The Vast `videogen` service should add completion-side upload behavior:

- Determine the generated output file path after ComfyUI job completion. The current worker returns output `filename`/`subfolder` with `local_path: None`, so implementation must resolve the local path from `COMFYUI_OUTPUT_DIR`, `subfolder`, and `filename`.
- Upload the output file to the Prakash-provided bucket destination or configured scoped bucket path.
- Produce the final bucket URL.
- Delete the local file only after upload success.
- Call Prakash completion endpoint with the final bucket URL.
- On generation or upload failure, call Prakash completion endpoint with failure status.

The service should not call off-chain-agent for this migrated flow.

If bucket upload fails, Vast must keep the local output file until retry policy or TTL cleanup decides otherwise. Immediate deletion on upload failure would make the job unrecoverable.

The existing Vast TTL cleanup task can remain as a fallback, but successful migrated jobs should perform immediate post-upload cleanup so disk use is bounded under load.

The webhook sender currently retries only a small fixed number of times. For this migration, the completion callback should either use a stronger retry policy or persist enough upload/completion state on Vast to retry after process restarts. Otherwise a successful bucket upload could fail to notify Prakash and leave the user stuck in in-progress state.

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
- Protect the Prakash completion endpoint with a shared secret, HMAC signature, mTLS, or equivalent service authentication.
- Treat completion callbacks as untrusted until authenticated and request-key ownership is checked.
- Make completion idempotent to avoid duplicate draft creation on webhook retries.
- Redact prompt, image data, delegated identity, and bucket credentials from logs.

## Error Handling

- NSFW rejection happens before RateLimiter request creation.
- Rate-limit rejection happens before Vast submission.
- Image staging failure after RateLimiter creation should mark the request failed. It should not call Vast.
- Vast submission failure after RateLimiter creation should mark the request failed and may decrement the request counter only if the team decides unsubmitted jobs should not count as usage.
- Generation failure after Vast accepts the job should mark RateLimiter failed and should count as usage.
- Bucket upload failure should mark RateLimiter failed and should count as usage because generation ran.
- Completion callback authentication failure should not update RateLimiter or draft state.
- Duplicate completion callbacks should not create duplicate drafts.
- Draft creation failure after successful bucket upload should keep enough context for retry. The safer default is to avoid marking RateLimiter `Complete` until draft creation succeeds for `ServerDraft`, because the user expectation is "in-progress becomes draft." If the team wants bucket URL fallback, that should be a conscious product decision.
- Legacy off-chain failures should continue using the old cleanup policy until drained. Migrated Prakash failures should use the new policy in this spec and should not trigger off-chain rollback/decrement code.

## State Machine

Prakash should treat each generation request as a small state machine:

1. `moderating`: request accepted by HTTP handler, before Ansuman decision.
2. `rate_limited`: terminal rejection before usage is consumed.
3. `context_created`: RateLimiter accepted and completion context persisted.
4. `submitted`: Vast accepted the job and returned a job id.
5. `generated_uploading`: Vast has generated output and is uploading it.
6. `uploaded`: Vast uploaded to bucket and called Prakash.
7. `draft_created`: Prakash created the actual draft.
8. `complete`: RateLimiter marked `Complete(bucket_url)`.
9. `failed`: terminal failure with reason.

Not every state needs to be exposed publicly, but the persisted context should be able to distinguish submitted, uploaded, draft-created, and failed states for retries and support.

## Retry And Recovery

- Ansuman call failures should be fail-closed in production and fail-open only in explicit local/mock mode.
- Vast submission can be retried by Prakash only if the request is still before `submitted`.
- Vast bucket upload can be retried by Vast while the local file exists.
- Prakash completion can be retried by Vast. Prakash must make this idempotent.
- Draft creation can be retried by Prakash using the persisted completion context.
- Prakash should have a reconciliation job for stale contexts so encrypted identities and half-completed rows do not accumulate indefinitely.
- If Prakash is down during completion, Vast should retry webhook delivery with bounded backoff.
- If Vast exhausts callback retries, the job remains recoverable only if Vast stores enough job/upload metadata or Prakash can reconcile by polling Vast status. This should be considered for operations before production rollout.
- Off-chain should be monitored during rollout for old in-flight LTX requests and QStash messages. Only after that queue drains should old off-chain LTX cleanup code be disabled.

## Observability

Add structured logs and Sentry context for:

- `operation_id`.
- RateLimiter request key.
- Vast job id.
- Ansuman decision, without logging raw prompt/image.
- Vast submit result.
- Bucket upload result.
- Completion callback authentication result.
- Draft creation result.
- State transitions and retry counts.

The design should avoid logging delegated identities, image payloads, bucket credentials, or raw prompts.

## Testing Strategy

Unit tests:

- Delegated identity mismatch returns unauthorized.
- NSFW rejection does not create RateLimiter request.
- RateLimit rejection does not submit to Vast.
- Base64 image request is sent to Ansuman before image staging.
- Image staging failure after RateLimiter acceptance marks request failed and does not submit to Vast.
- Safe request submits to Vast with request key and Prakash callback URL.
- Safe `ServerDraft` request stores encrypted delegated identity in durable completion context.
- Completion success updates RateLimiter to complete.
- Completion failure updates RateLimiter to failed.
- `ServerDraft` completion uses encrypted identity and does not create duplicate drafts on retry.
- Completion with invalid auth does not update RateLimiter.
- Completion with mismatched principal/job id is rejected.

Integration tests:

- Mock Ansuman, RateLimiter, Vast, and upload metadata service.
- Verify the full safe path from generate to completion.
- Verify success response matches mobile's current `GenerateVideoSuccessDto`.
- Verify NSFW, rate-limit, auth, and provider errors parse through mobile's current `VideoGenErrorDto` shape.
- Verify Vast upload failure calls completion failure and does not delete local file before successful upload.
- Verify Vast deletes local file after successful bucket upload.
- Verify Vast resolves generated output paths correctly from `COMFYUI_OUTPUT_DIR`, filename, and subfolder.
- Verify staged image inputs do not accumulate indefinitely on Vast.
- Verify retrying the same completion callback does not duplicate the draft.
- Verify service restart between generate and completion still allows draft creation from persisted context.
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

1. Add Prakash generate and completion endpoints behind config.
2. Add Ansuman moderation client with mock mode for non-production while Ansuman is not live.
3. Add durable Prakash completion-context persistence.
4. Add upload destination preparation for Vast.
5. Add Vast bucket-upload and Prakash-completion behavior behind config.
6. Add cleanup/reconciliation jobs for Prakash completion contexts and Vast generated/staged files.
7. Run end-to-end smoke test in staging.
8. Switch mobile's videogen base URL to Prakash when staging behavior matches off-chain.
9. Keep off-chain LTX path available as rollback until production confidence is established.
10. Drain old off-chain LTX jobs and QStash messages.
11. Gate or remove old off-chain LTX routing after the drain is verified.

## Open Decisions

- Exact Ansuman request/response schema.
- Exact bucket URL format returned by Vast.
- Completion endpoint authentication mechanism.
- Whether Prakash should pass Vast a pre-signed upload URL or a scoped bucket object key.
- Exact video id/object key format for generated drafts.
- Exact durable store schema for completion contexts.
- Exact stale-context cleanup windows for Prakash.
- Exact local generated-file and staged-image cleanup policy for Vast.
- Exact off-chain drain signal for safely disabling legacy LTX callbacks.
- Whether draft creation failure after successful bucket upload should leave RateLimiter as `Complete(bucket_url)` or keep it non-complete until draft creation succeeds.
- Whether an accepted but not submitted Vast job should decrement RateLimiter counter on immediate submit failure.
