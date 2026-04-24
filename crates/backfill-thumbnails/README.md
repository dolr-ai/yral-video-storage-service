# backfill-thumbnails

`backfill-thumbnails` is the operator crate for staged thumbnail backfills.

It generates first-frame thumbnails as `<video_id>-thumbnail.png` for historical videos without touching existing `<video_id>_thumbnail.png` objects. The binary is intended for controlled operational runs, not for request-path use.

The CLI is implemented with `clap`, so top-level and subcommand help are generated directly from the command definitions.
Every operator mode also prints a compact human-readable summary to the terminal after it finishes, while still writing JSON artifacts under the manifest directory.

## Safety Model

- Dry-run is the default. Writes require `--execute`.
- Existing `_thumbnail.png` objects are never overwritten by this tool.
- The tool never deletes thumbnails or videos.
- Test buckets must be validated before any production write run.
- `test-sfw-hetzner` requires an explicit `--bucket` so it cannot silently fall through to the production Hetzner bucket.
- Resume state is bucket-scoped. Completed work for one bucket does not suppress work in another.

## Backends

- Storj: uses `uplink` subprocesses and machine-readable `ls --utc -o json` output for stable audit/listing behavior
- Hetzner: uses the shared Rust S3 client from the main service crate

## Commands

### `seed-test-data`

Creates deterministic test videos and intentionally wrong legacy thumbnails in test buckets only.
Prints the seeded object count to the terminal.

### `run`

Discovers eligible `.mp4` objects, skips already staged items, extracts the first frame, and uploads `<video_id>-thumbnail.png`.
Prints dry-run or execute counts to the terminal, including the number of existing staged `-thumbnail.png` objects.

### `verify`

Read-only validation that compares the staged thumbnail against a fresh first-frame extraction of the source video.
Prints pass / fail / skip counts to the terminal.

### `audit`

Read-only reporting for candidate counts, staged counts, manifest counts, and remaining work.
Prints the same high-level counts to the terminal in addition to writing `audit.json`, with labels that distinguish total bucket objects from candidate videos and remaining videos to backfill.

## Prerequisites

- `ffmpeg` installed and available on `PATH`
- `uplink` installed and available on `PATH` for Storj scopes
- Environment configured for the target backend:
  - Storj: `STORJ_ACCESS_GRANT_SFW`, `STORJ_ACCESS_GRANT_NSFW`, `TEST_BUCKET` as needed
  - Hetzner: `HETZNER_S3_ENDPOINT`, `HETZNER_S3_BUCKET`, `HETZNER_S3_ACCESS_KEY`, `HETZNER_S3_SECRET_KEY`, `HETZNER_S3_REGION`

## Supported Scopes

Scopes are fixed on purpose. They map the currently approved operational surfaces for this migration, let the tool pick the correct backend adapter automatically, and keep safety rules enforceable instead of turning the operator into an unrestricted bucket copier.

- `test-sfw-storj`
- `test-sfw-hetzner`
- `prod-sfw-storj`
- `prod-sfw-hetzner`
- `prod-nsfw-storj`

## Common Flags

- `--scope <scope>`
- `--cutoff-before <date-or-rfc3339>`
- `--manifest-dir <path>`
- `--prefix <prefix>`
- `--execute`
- `--download-concurrency <n>`
- `--ffmpeg-concurrency <n>`
- `--upload-concurrency <n>`
- `--verify-sample <n>`
- `--bucket <bucket>`

Notes:

- `--cutoff-before` is required for `run`, `verify`, and `audit`.
- `--thumbnail-seek` exists for controlled test flows, but production `run` requires first-frame extraction.
- Default concurrency is `8 / 1 / 8` for download / ffmpeg / upload.
- Parallelism is handled with Tokio semaphores around async I/O and `ffmpeg` subprocesses; `rayon` is intentionally not used because the heavy work is not in-process CPU-bound Rust code.

Build the operator:

```sh
cargo build -p backfill-thumbnails
```

Show the generated CLI help:

```sh
cargo run -p backfill-thumbnails -- --help
```

## Artifact Layout

State is bucket-scoped under the manifest directory:

```text
<manifest-dir>/<scope>/<bucket>/
  manifest.jsonl
  runs/
    <run-id>-run/
      summary.json
      failures.jsonl
    <run-id>-verify/
      summary.json
      verify.jsonl
    <run-id>-audit/
      summary.json
      audit.json
```

The manifest is append-only JSONL. A torn final line is ignored on reload, but invalid complete lines still fail the run.

## Examples

Seed test data into a Storj test bucket:

```sh
cargo run -p backfill-thumbnails -- \
  seed-test-data \
  --scope test-sfw-storj \
  --bucket your-test-bucket \
  --manifest-dir artifacts/backfill
```

Audit a Storj test bucket (shows remote state, manifest counts, and remaining work):

```sh
cargo run -p backfill-thumbnails -- \
  audit \
  --scope test-sfw-storj \
  --bucket your-test-bucket \
  --cutoff-before 2026-04-22 \
  --manifest-dir artifacts/backfill
```

Audit scoped to a specific folder prefix:

```sh
cargo run -p backfill-thumbnails -- \
  audit \
  --scope test-sfw-storj \
  --bucket your-test-bucket \
  --cutoff-before 2026-04-22 \
  --manifest-dir artifacts/backfill \
  --prefix some-user/
```

Dry-run a Storj test bucket:

```sh
cargo run -p backfill-thumbnails -- \
  run \
  --scope test-sfw-storj \
  --bucket your-test-bucket \
  --cutoff-before 2026-04-22 \
  --manifest-dir artifacts/backfill
```

Execute against a Storj test bucket scoped to a folder:

```sh
cargo run -p backfill-thumbnails -- \
  run \
  --scope test-sfw-storj \
  --bucket your-test-bucket \
  --cutoff-before 2026-04-22 \
  --manifest-dir artifacts/backfill \
  --prefix some-user/ \
  --execute
```

Execute against a Hetzner test bucket:

```sh
cargo run -p backfill-thumbnails -- \
  run \
  --scope test-sfw-hetzner \
  --bucket your-test-bucket \
  --cutoff-before 2026-04-22 \
  --manifest-dir artifacts/backfill \
  --execute
```

Verify a sample after a run:

```sh
cargo run -p backfill-thumbnails -- \
  verify \
  --scope test-sfw-storj \
  --bucket your-test-bucket \
  --cutoff-before 2026-04-22 \
  --manifest-dir artifacts/backfill \
  --verify-sample 25
```

## Operational Rules

- Do not run production writes without explicit approval.
- Use test buckets first.
- Keep the plan document in `docs/superpowers/plans/2026-04-23-backfill-thumbnails.md` updated as runs complete.
