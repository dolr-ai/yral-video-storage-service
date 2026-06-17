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

## Compatibility Gate

- [ ] Compare hashes for a small shared sample between off-chain commit `3a4072ae07b45875be29377cf3cecf52e80ca218` and this service running in the deployment image.
- [ ] If the bits differ due to FFmpeg/libav or image-hasher drift, do not publish those rows as `offchain_binary_10x8_v1`.
- [ ] If deployment output intentionally differs from off-chain, mint a new `hash_version` and keep legacy rows out of exact-duplicate comparisons.

## After Deploy

- [ ] Run one limited pHash backfill batch and verify `phash`, `phash_kind`, and `phash_version` are populated together.
- [ ] Check `/mirror/duplicates` and `/mirror/duplicates/{video_id}` responses include `hash_kind` and `hash_version`.
- [ ] Confirm downstream consumers either ignore unknown fields safely or have been updated to read the new fields.
- [ ] Watch pHash job errors and Sentry events for FFmpeg decode failures.
