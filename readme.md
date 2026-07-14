YRAL VIDEO STORAGE SERVICE

Storj interface is a rest interface to upload objects to storj via the uplink cli.

## Configuration

Storj interface is configured via the following environment variables.

| Name                                               | Description                                                                      | Default (no default means `required`) |
|----------------------------------------------------|----------------------------------------------------------------------------------|---------------------------------------|
| `STORJ_MIRROR_ACCESS_GRANT`                        | Storj access grant for current sfw raw/HLS uploads and mirror jobs               |                                       |
| `STORJ_SFW_BUCKET`                                 | Current sfw Storj bucket                                                         | yral-sfw                              |
| `STORJ_ACCESS_GRANT_NSFW`                          | Storj access grant that is used when accessing the nsfw bucket                   |                                       |
| `NSFW_BUCKET`                                      | The name of the nsfw bucket                                                      | yral-nsfw-videos                      |
| `HETZNER_S3_ACCESS_KEY`                            | Hetzner S3 access key for current sfw raw/HLS uploads                            |                                       |
| `HETZNER_S3_SECRET_KEY`                            | Hetzner S3 secret key for current sfw raw/HLS uploads                            |                                       |
| `HETZNER_S3_BUCKET`                                | Hetzner S3 bucket for current sfw raw/HLS uploads                                |                                       |
| `STORJ_ACCESS_GRANT_SFW`                           | Legacy sfw access grant kept for compatibility with older startup configuration  |                                       |
| `SFW_BUCKET`                                       | Legacy sfw bucket kept for compatibility with older startup configuration        | yral-videos                           |
| `SERVICE_SECRET_TOKEN`                             | Shared secret between storj interface and the caller                             |                                       |
| `PUBLIC_BASE_URL`                                  | Externally-reachable base URL; used to build upload URLs                         |                                       |
| `OFFCHAIN_EVENTS_API_TOKEN`                        | Bearer token for offchain analytics events                                      | Optional                              |
| `YRAL_METADATA_NOTIFICATION_SERVICE_API_TOKEN`     | Bearer token for push notifications                                              | Optional                              |
| `RUN_DB_BACKUP`                                    | Enable the `db-backup` service (daily pg_dump -> Hetzner)                        | false                                 |
| `BACKUP_S3_BUCKET`                                 | Hetzner bucket for DB backups (same project as `HETZNER_S3_*` creds)            | prakash-yral                          |
| `BACKUP_S3_PREFIX`                                 | Key prefix (folder) for DB backups within the bucket                            | yral-video-storage-service            |
| `BACKUP_RETENTION_DAYS`                            | Rolling retention window; objects older than this are pruned each cycle          | 30                                    |
| `BACKUP_POLL_SECS`                                 | How often the backup loop wakes to check/back up (one dump per UTC day)          | 3600                                  |
| `PROFILE_S3_BUCKET`                                | Hetzner bucket for profile images (reuses `HETZNER_S3_*` creds/endpoint)         | prakash-yral                          |
| `PROFILE_S3_KEY_PREFIX`                            | Key prefix for profile images (`<prefix><principal>/profile-<ts>.jpg`)           | yral-profile/users/                   |
| `PROFILE_S3_PUBLIC_URL_BASE`                       | Public base URL images are served from                                          | https://prakash-yral.hel1.your-objectstorage.com |

For running locally, a Storj account is required.
- `cp .env.example .env`
- Create/update the current sfw bucket (`yral-sfw`) and nsfw bucket (`yral-nsfw-videos`). Update `.env` file accordingly.
- Create access grants to the buckets. Current sfw raw/HLS uploads use `STORJ_MIRROR_ACCESS_GRANT`; nsfw uploads use `STORJ_ACCESS_GRANT_NSFW`.

## Upload routes (merged from yral-video-upload-service)

These public routes (no HMAC; auth is the in-body chain-verified delegated identity)
were merged in from the former upload service:

- `POST /get-upload-url` — `{publisher_user_id}` → `{upload_url, video_id}`. Validates
  the principal via user-info-service, mints a video id, returns an upload URL built
  from `PUBLIC_BASE_URL`.
- `POST /update-video-metadata` — `{delegated_identity_wire, meta, post_details}`.
  Verifies `sender() == creator_principal`, finalizes the Storj upload, registers the
  post on user-post-service, fires analytics + notification.
- `POST /mark-post-as-published` — `{post_id, delegated_identity_wire}`. Verifies
  ownership, flips the post to `Uploaded`, fires analytics + notification.

`/update-video-metadata` and `/mark-post-as-published` require `OFFCHAIN_EVENTS_API_TOKEN`
and `YRAL_METADATA_NOTIFICATION_SERVICE_API_TOKEN`; without them those routes return 503
(the service still starts and all other routes work).

## Profile image routes (moved from off-chain-agent)

Same public auth model (in-body chain-verified delegated identity). Images live in the
Hetzner `PROFILE_S3_BUCKET` (reusing the `HETZNER_S3_*` creds); the canister write runs
as the user via a per-request agent. Contract matches off-chain-agent so clients migrate
by base URL only.

- `POST /api/v1/user/profile-image` — `{delegated_identity_wire, image_data}` (base64,
  optional `data:` prefix; ≤5 MB) → `{profile_image_url}`. Resizes ≤1000px, encodes JPEG
  q85, deletes the user's prior images, uploads public-read, then sets `profile_picture_url`
  on user-info-service.
- `DELETE /api/v1/user/profile-image` — `{delegated_identity_wire}` → 200. Deletes the
  user's images and clears `profile_picture_url` (reads fall back to the GobGob default).

Path matches off-chain-agent's `/api/v1/user/profile-image` so clients migrate by base URL
only. Registered in the served OpenAPI (`/api-docs`).

Status codes: 401 (bad/forged identity), 400 (empty/oversized/undecodable image), 403
(canister not authorized), 500 (storage/canister error). Runs on all app nodes; the
`off-chain-agent` copy stays live during migration (dual-run).

## Running prebuilt image

Given an appropriate `.env` file, the prebuilt image can be run using docker.
```sh
docker run --env-file .env --rm -it -p 3000:3000 ghcr.io/dolr-ai/storj-interface:latest
```

This has only been tested with x86_64 linux so far.

## Running locally built image (linux)

Ensure that your system can build for target `x86_64-unknown-linux-musl`.

Given an appropriate `.env` file, a local image can be built and run like so.
```sh
cargo build --release --target=x86_64-unknown-linux-musl
docker build -t storj-interface . # or any tag you want
docker run --env-file .env --rm -it -p 3000:3000 storj-interface
```

## Local Testing

The rest api is tested via `hurl`, so make sure that is installed on the machine.
An example variable file is available at `test/example-test.vars`.

Following command can be used to test the REST API side of the storage interface's raw upload request.

```sh
hurl test/duplicate_raw_multipart.hurl --variables-file test/local-test.vars --test
```

For more thorough e2e testing,
create [linksharing links to your buckets](https://storj.dev/learn/concepts/linksharing-service)
and refer to [testing workflow](./.github/workflows/e2e-tests.yml).

## Database backups (Hetzner Object Storage)

The HA stack includes a `db-backup` compose service (profile `backup`, enabled by
`RUN_DB_BACKUP=true`). It runs on every node but only the node whose local Patroni is
the primary actually dumps — it gates on `GET patroni:8008/primary` — so exactly one
backup is produced cluster-wide and the job follows the primary across failovers.

Each cycle (`BACKUP_POLL_SECS`, default hourly), on the primary:

1. Skip if today's object already exists (idempotent across redeploys).
2. `pg_dump -Fc` the DB as the `postgres` superuser via `postgres-router` (always the primary).
3. Upload to `s3://$BACKUP_S3_BUCKET/$BACKUP_S3_PREFIX/video_fingerprint_index_<UTCdate>.dump`
   on `HETZNER_S3_ENDPOINT` (reuses the `HETZNER_S3_*` creds — the bucket must be in the
   same Hetzner project), then verify the uploaded size.
4. Prune objects older than `BACKUP_RETENTION_DAYS` → a rolling ~30-file window, so
   storage stays bounded.

**Precondition:** the `BACKUP_S3_BUCKET` (default `prakash-yral`) must exist in the same
Hetzner project as `HETZNER_S3_ACCESS_KEY`.

### Restore

Backups are custom-format (`-Fc`) → restore with `pg_restore`, not the plain-SQL
`restore-backup.sh`. Run inside the same image (has `rclone` + `pg_restore`) on any node:

```sh
cd deploy
COMPOSE_PROJECT_NAME=yral-video-storage-service \
  docker compose -f docker-compose.ha.yml --profile backup \
  run --rm --entrypoint sh db-backup /restore-from-hetzner.sh [YYYYMMDD]
```

(`--entrypoint sh` is required — the service's default entrypoint is the backup loop.)

No date → newest backup. This is destructive (`pg_restore --clean`); it routes to the
current primary via `postgres-router`.

## Deployment

- For pr previews and e2e testing, fly is used.
- For production deployment, theta is used.

## Why not use storj S3 interface

Storj provides an S3 compatible interface that in theory allows uploading videos
via any s3 client like the sdk or the aws cli. But, it has minor inconsistencies
in implementation and doesn't seem to be prioritized in case an incompatibility
occurs.

For example, https://github.com/storj/gateway-st/issues/89 breaks compatibility with `aws cli`.

## Why not use uplink crate

Uplink crate, as of writing this documentation, doesn't compile for `wasm32-unknown-unknown`.


## NOTE: Hetzner ie sfw upload is indirectly tested via move2nsfw test (the test that copies from hetzner to storj and then verifies on storj)
