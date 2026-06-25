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
| `config` | Show dynamic concurrency configuration | No | No |
| `config-set` | Update concurrency config (`--phash`, `--mirror`, `--page`) | No | No |
| `scan-storj` | Scan Storj bucket and index all video keys into the DB | Yes | Yes |
| `scan-hetzner` | Scan Hetzner S3 bucket and index all video keys into the DB | Yes | Yes |
| `phash` | Compute perceptual hashes for videos missing them | Yes | Yes |
| `mirror` | Copy pending videos from Hetzner → Storj | Yes | Yes |
| `run-pipeline` | Run full pipeline: scan-hetzner → phash → mirror | Yes | Yes |
| `cancel` | Cancel all running background jobs | No | No |

*Note: S3 scanning jobs (`scan-storj`, `scan-hetzner`, `run-pipeline`) automatically resume from the last successfully indexed key by default. Use `--full-scan` to disable this and perform a full scan from the beginning (which also resets failed jobs).*

### Media ownership commands

Operate the media-ownership subsystem (canonical master table + pHash + feed outbox). The import and pHash jobs are resumable (skip-existing) and share a dedicated cancellation token, separate from the mirror jobs above.

| Command | Description | Background Job | Options |
|---------|-------------|:--------------:|---------|
| `media-import` | Import legacy `video_index` rows into the master table (skip-existing, resumable) | Yes | `--limit N` |
| `media-phash` | Compute canonical pHash for master rows missing it (resumable) | Yes | `--limit N`, `--shard I --of N` |
| `media-cancel` | Cancel the running media jobs (import + pHash) | No | — |
| `media-status` | Show whether the media import / pHash jobs are running | No | — |
| `media-audit` | pHash coverage: `total_servable` / `with_canonical_phash` / `missing` | No | — |
| `media-feed` | Page the denormalized media outbox feed (`cursor > after`) | No | `--after N`, `--limit N` |
| `media-runs` | Recent media job runs with live totals (scanned/failed) — derive rate/ETA | No | `--job-kind K`, `--limit N` |
| `media-failures` | Failure summary grouped by phase, with counts + sample errors | No | `--job-kind K`, `--limit N` |

*Note: media job concurrency/page size reuse the same `config-set --phash`/`--page` knobs (and `PHASH_CONCURRENCY`/`SCAN_PAGE_SIZE` env). Lower `--phash` to cap CPU during a backfill. Config set via `config-set` is in-memory and resets on redeploy.*

## Examples

```bash
# Check mirror pipeline health
set -a && source .env && set +a && cargo run -p mirror-client -- audit

# Scan first 10 videos from Hetzner into the index
set -a && source .env && set +a && cargo run -p mirror-client -- scan-hetzner --limit 10

# Scan only a specific publisher's videos from Storj
set -a && source .env && set +a && cargo run -p mirror-client -- scan-storj --prefix "prefix/"

# Combine --prefix with --limit to scan a subset
set -a && source .env && set +a && cargo run -p mirror-client -- scan-hetzner --prefix "prefix/" --limit 5

# Run a full S3 scan from the very beginning (ignores last saved key) and reset all failed jobs
set -a && source .env && set +a && cargo run -p mirror-client -- scan-hetzner --full-scan

# Run the full pipeline for a publisher (scan → phash → mirror, waits between steps)
set -a && source .env && set +a && cargo run -p mirror-client -- run-pipeline --prefix "prefix/"

# Run the full pipeline for a publisher (scan → phash → mirror, no waits between steps)
set -a && source .env && set +a && cargo run -p mirror-client -- run-pipeline-async --prefix "prefix/"

# List duplicate videos (same perceptual hash)
set -a && source .env && set +a && cargo run -p mirror-client -- duplicates

# Check if any jobs are running
set -a && source .env && set +a && cargo run -p mirror-client -- status

# Cancel all running jobs
set -a && source .env && set +a && cargo run -p mirror-client -- cancel

# Show dynamic concurrency config
set -a && source .env && set +a && cargo run -p mirror-client -- config

# Update concurrency config dynamically (without restarting the server)
set -a && source .env && set +a && cargo run -p mirror-client -- config-set --phash 10 --mirror 30 --page 2500

# --- Media ownership backfill ---

# Import legacy video_index into the master table (resumable; re-run to continue)
set -a && source .env && set +a && cargo run -p mirror-client -- media-import

# Compute canonical pHash (validate small first, then scale; lower --phash to cap CPU)
set -a && source .env && set +a && cargo run -p mirror-client -- config-set --phash 2
set -a && source .env && set +a && cargo run -p mirror-client -- media-phash --limit 50
set -a && source .env && set +a && cargo run -p mirror-client -- media-phash          # full, resumable

# Run only one shard of N (the box processes its 1/N slice of the missing rows).
# Used to split the backfill across the fleet so boxes do disjoint download+ffmpeg work.
set -a && source .env && set +a && cargo run -p mirror-client -- media-phash --shard 0 --of 3

# Monitor coverage, live run progress, and failure reasons
set -a && source .env && set +a && cargo run -p mirror-client -- media-audit
set -a && source .env && set +a && cargo run -p mirror-client -- media-runs --job-kind media_phash
set -a && source .env && set +a && cargo run -p mirror-client -- media-failures --job-kind media_phash

# Cancel the running media jobs
set -a && source .env && set +a && cargo run -p mirror-client -- media-cancel
```

### Sharded fleet backfill

`media-phash --shard I --of N` partitions the missing-pHash set by
`((hashtext(video_id) % of) + of) % of = shard` — a total, deterministic, uniform
split, so each shard owns exactly `1/N` of the work with no overlap. Both flags are
required together; the server validates `of >= 1` and `0 <= shard < of` (else 400).

To drive all three prakash boxes at once (each running one shard against its own
`localhost:3005`), use `scripts/phash-fleet.sh` from a workstation with SSH access:

```bash
# Trigger shard 0/1/2 across the fleet; skips any box already running pHash.
SERVICE_SECRET_TOKEN=… ./scripts/phash-fleet.sh --of 3

# Optional: explicit deploy key (default = ssh-agent / ~/.ssh/config), per-shard cap, dry run.
SERVICE_SECRET_TOKEN=… PHASH_FLEET_SSH_KEY=~/.ssh/your_deploy_key \
  ./scripts/phash-fleet.sh --of 3 --limit 1000 --dry-run
```

HMAC signatures are computed locally — the token never travels over SSH. Each run is
labeled `requested_by=phash-shard-<i>-of-<N>`, so the concurrent runs are distinguishable
in `media-runs`. Monitoring needs no SSH: a single signed `media-audit` / `media-runs` /
`media-failures` against server_1's public domain aggregates all shards (shared DB).

## Notes

- **Background jobs** return `202 Accepted` immediately. The job runs on the server — use `audit` or `status` to monitor progress.
- **Resuming**: Scanning jobs remember the last key they successfully processed. Passing `--full-scan` bypasses this resume logic to rescan the entire bucket and resets jobs with a `failed` status to `pending`.
- **`--limit N`** processes the first N items from the bucket listing. Scans are idempotent (upserts), so re-running is safe.
- **`--prefix PREFIX`** filters bucket listing to keys starting with PREFIX (e.g. `publisher-id/`). Only applies to `scan-storj`, `scan-hetzner`, and `run-pipeline`.
- **Only one instance** of each job type can run at a time. Starting a duplicate returns `409 Conflict`.
- **Ctrl+C** on the client only kills the client — the server-side job continues. Use `cancel` to stop server-side jobs.
- **Auth** uses HMAC-SHA256 signatures with the shared `SERVICE_SECRET_TOKEN`.
