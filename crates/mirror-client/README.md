# mirror-client

CLI tool for managing the Hetzner ↔ Storj mirror pipeline. Communicates with the video storage service via HMAC-SHA256 authenticated HTTP requests.

## Setup

```bash
# Required environment variables (or add to .env)
export MIRROR_SERVICE_URL=https://storj-interface-preview.prakash.yral.com
export SERVICE_SECRET_TOKEN=your_secret_token
```

## Usage

```bash
# Source .env and run (recommended)
set -a && source .env && set +a && cargo run -p mirror-client -- <command> [options]
```

## Commands

| Command | Description | Background Job | Supports `--limit` |
|---------|-------------|:--------------:|:------------------:|
| `audit` | Print index statistics (total, mirrored, missing, duplicates) | No | No |
| `duplicates` | List videos sharing identical perceptual hashes with their Storj and Hetzner keys | No | No |
| `status` | Show which background jobs are currently running | No | No |
| `scan-storj` | Scan Storj bucket and index all video keys into the DB | Yes | Yes |
| `scan-hetzner` | Scan Hetzner S3 bucket and index all video keys into the DB | Yes | Yes |
| `phash` | Compute perceptual hashes for videos missing them | Yes | Yes |
| `mirror` | Copy pending videos from Hetzner → Storj | Yes | Yes |
| `cancel` | Cancel all running background jobs | No | No |

## Examples

```bash
# Check mirror pipeline health
set -a && source .env && set +a && cargo run -p mirror-client -- audit

# Scan first 10 videos from Hetzner into the index
set -a && source .env && set +a && cargo run -p mirror-client -- scan-hetzner --limit 10

# List duplicate videos (same perceptual hash)
set -a && source .env && set +a && cargo run -p mirror-client -- duplicates

# Check if any jobs are running
set -a && source .env && set +a && cargo run -p mirror-client -- status

# Cancel all running jobs
set -a && source .env && set +a && cargo run -p mirror-client -- cancel
```

## Notes

- **Background jobs** return `202 Accepted` immediately. The job runs on the server — use `audit` or `status` to monitor progress.
- **`--limit N`** processes the first N items from the bucket listing. Scans are idempotent (upserts), so re-running is safe.
- **`--prefix PREFIX`** filters bucket listing to keys starting with PREFIX (e.g. `publisher-id/`). Only applies to `scan-storj` and `scan-hetzner`.
- **Only one instance** of each job type can run at a time. Starting a duplicate returns `409 Conflict`.
- **Ctrl+C** on the client only kills the client — the server-side job continues. Use `cancel` to stop server-side jobs.
- **Auth** uses HMAC-SHA256 signatures with the shared `SERVICE_SECRET_TOKEN`.
