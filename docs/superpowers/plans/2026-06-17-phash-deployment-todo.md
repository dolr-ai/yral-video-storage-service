# pHash Deployment TODO

Owner key: `prakash-bhatt-yral`

Use this checklist before enabling the new pHash path outside local development.

## Required Before Deploy

- [ ] Build the deployment image from `deploy/Dockerfile` after the `ffmpeg-next = 8.1.0` bump.
- [ ] Inside that image, run `cargo test -p phash --locked`.
- [ ] Record runtime FFmpeg/libav versions from the image with `ffmpeg -version`.
- [ ] Compare the image-produced fixture hash with `crates/phash/tests/fixtures/test_raw_video.offchain_binary_10x8_v1.txt`.
- [ ] Run DB-backed tests with Docker/Postgres available:

```sh
cargo test -p storj-interface --lib db::tests --locked
```

- [x] Run the media-index DB tests with Docker/Postgres available. Passed locally on 2026-06-17 after Docker Desktop was started:

```sh
cargo test media_index -- --nocapture --test-threads=1
```

- [ ] Run the legacy media-import job tests with Docker/Postgres available:

```sh
cargo test media_imports -- --nocapture --test-threads=1
```

- [ ] Run the identity crypto test with a non-empty secret:

```sh
INTERNAL_ENCRYPTION_SECRET=<deployment-like-test-secret> cargo test -p storj-interface --lib videogen::identity_crypto::tests --locked
```

## Required During DB Rollout

- [ ] Confirm `video_index` receives these columns:
  - `phash_kind`
  - `phash_version`
- [ ] Confirm existing rows with `phash IS NOT NULL` are tagged as:
  - `phash_kind = 'phash'`
  - `phash_version = 'legacy_hex_8x8_v0'`
- [ ] Confirm new rows written by the pHash job are tagged as:
  - `phash_kind = 'phash'`
  - `phash_version = 'offchain_binary_10x8_v1'`
- [ ] Confirm duplicate grouping uses `(phash_kind, phash_version, phash)`, not only `phash`.
- [ ] Check query plan/index usage for duplicate queries after enough rows exist.

## Required During Legacy Media Import (Phase 1C)

- [ ] Run `import_current_video_index` with a small `limit` first (smoke batch) before a full run.
- [ ] Confirm `media_job_runs` records the run lifecycle: `running` -> `succeeded` / `succeeded_with_failures` (`failed` only when the whole job aborts).
- [ ] Confirm imported rows land in `all_servable_videos_on_yral` and `servable_video_sources` with `source_kind = 'legacy_video_index'` and a populated `raw_payload`.
- [ ] Confirm complete `(phash, phash_kind, phash_version)` tuples import into `servable_video_hashes` with `input_media_version = 'legacy_video_index_object_v1'`.
- [ ] Confirm rows missing both `storj_key` and `hetzner_key` are recorded in `media_job_failures`, not silently skipped.
- [ ] Confirm feed events: one `media_visibility_changed` per imported/changed media row, one `hash_upserted` per inserted/changed hash row, and none for unchanged rows.
- [ ] Re-run the import and confirm idempotency: no duplicate source rows and no new feed events for unchanged rows.

## Compatibility Gate

- [ ] Compare hashes for a small shared sample between off-chain commit `3a4072ae07b45875be29377cf3cecf52e80ca218` and this service running in the deployment image.
- [ ] If the bits differ due to FFmpeg/libav or image-hasher drift, do not publish those rows as `offchain_binary_10x8_v1`.
- [ ] If deployment output intentionally differs from off-chain, mint a new `hash_version` and keep legacy rows out of exact-duplicate comparisons.

## Operational Routes Hardening (Phase 1E findings)

These came out of the security + database review of the `/media/*` routes (2026-06-19). All are pre-existing, service-wide concerns surfaced by — not introduced by — Phase 1E. Already fixed in Phase 1E: all three `/media/*` routes require HMAC, and `requested_by` is clamped to 256 chars.

**Gate blocker — must land before the media feed serves real consumers:**

- [ ] Introduce a Postgres connection pool (`deadpool-postgres`/`bb8`) and store it in `AppState` instead of `db_url`; every handler currently calls `db::connect` per request (new TCP + PG connection each time). `GET /media/feed/events` is designed to be polled, so without a pool, consumer polling + the per-request connections can exhaust `max_connections` and deny service. This is a cross-cutting refactor — do it in its own focused PR, not on the media-ownership branch.

**Program-level follow-ups (track as tickets; not media-branch work):**

- [ ] Add rate limiting to public-facing routes (pairs with the pool work).
- [ ] Move the hardcoded Sentry DSN (`src/main.rs`) to an env var AND rotate the leaked key (moving to env does not un-leak the committed value).
- [ ] HMAC replay: the ±5-minute window has no nonce/jti tracking; a signed request can be replayed within the window. Shared by all signed routes — needs client coordination.
- [ ] Tighten CORS (`allow_origin(Any)` / `allow_headers(Any)`).

## After Deploy

- [ ] Run one limited pHash backfill batch and verify `phash`, `phash_kind`, and `phash_version` are populated together.
- [ ] Check `/mirror/duplicates` and `/mirror/duplicates/{video_id}` responses include `hash_kind` and `hash_version`.
- [ ] Confirm downstream consumers either ignore unknown fields safely or have been updated to read the new fields.
- [ ] Watch pHash job errors and Sentry events for FFmpeg decode failures.
