#!/bin/sh
# Continuous database-backup loop. Runs inside the `db-backup` compose service on
# every node. Only the node whose LOCAL Patroni is the primary actually dumps
# (gate on GET /primary), so exactly one backup is produced cluster-wide and the
# job follows the primary across failovers.
#
# Each cycle (UTC):
#   1. skip unless local Patroni is primary
#   2. skip the dump if today's object already exists (idempotent across redeploys)
#   3. pg_dump -Fc the DB (as superuser, via postgres-router -> current primary)
#   4. upload to Hetzner Object Storage: BACKUP_S3_BUCKET/BACKUP_S3_PREFIX/<db>_<UTCdate>.dump
#   5. verify the uploaded object size matches the local dump
#   6. prune objects older than BACKUP_RETENTION_DAYS (rolling window -> bounded size)
#
# A failed cycle logs and is retried on the next poll; the loop never exits so the
# container's `restart: always` only matters for host reboots.

log() { echo "[db-backup] $(date -u '+%Y-%m-%dT%H:%M:%SZ') $*"; }

: "${POSTGRES_SUPERUSER_PASSWORD:?POSTGRES_SUPERUSER_PASSWORD required}"
: "${HETZNER_S3_ENDPOINT:?HETZNER_S3_ENDPOINT required}"
: "${HETZNER_S3_ACCESS_KEY:?HETZNER_S3_ACCESS_KEY required}"
: "${HETZNER_S3_SECRET_KEY:?HETZNER_S3_SECRET_KEY required}"

BUCKET="${BACKUP_S3_BUCKET:?BACKUP_S3_BUCKET required}"
PREFIX="${BACKUP_S3_PREFIX:-yral-video-storage-service}"
DB_NAME="${BACKUP_DB_NAME:-video_fingerprint_index}"
DB_HOST="${BACKUP_DB_HOST:-postgres-router}"
DB_PORT="${BACKUP_DB_PORT:-5432}"
DB_USER="${BACKUP_DB_USER:-postgres}"
PATRONI_HOST="${BACKUP_PATRONI_HOST:-patroni}"
PATRONI_PORT="${BACKUP_PATRONI_PORT:-8008}"
RETENTION_DAYS="${BACKUP_RETENTION_DAYS:-30}"
POLL_SECS="${BACKUP_POLL_SECS:-3600}"
REGION="${HETZNER_S3_REGION:-hel1}"

# rclone ad-hoc S3 remote 'hz' configured purely from env (no config file needed).
# Point config at /dev/null so rclone doesn't log a "config not found" notice each call.
export RCLONE_CONFIG=/dev/null
export RCLONE_CONFIG_HZ_TYPE=s3
export RCLONE_CONFIG_HZ_PROVIDER=Other
export RCLONE_CONFIG_HZ_ACCESS_KEY_ID="$HETZNER_S3_ACCESS_KEY"
export RCLONE_CONFIG_HZ_SECRET_ACCESS_KEY="$HETZNER_S3_SECRET_KEY"
export RCLONE_CONFIG_HZ_ENDPOINT="$HETZNER_S3_ENDPOINT"
export RCLONE_CONFIG_HZ_REGION="$REGION"
export RCLONE_CONFIG_HZ_ACL=private
REMOTE="hz:${BUCKET}/${PREFIX}"

is_primary() {
  curl -fsS -o /dev/null --max-time 10 \
    "http://${PATRONI_HOST}:${PATRONI_PORT}/primary"
}

run_cycle() {
  if ! is_primary; then
    log "local node is not the primary — skipping"
    return 0
  fi

  DATE="$(date -u +%Y%m%d)"
  OBJ="${DB_NAME}_${DATE}.dump"

  if rclone lsf "${REMOTE}/" 2>/dev/null | grep -qx "${OBJ}"; then
    log "today's backup ${OBJ} already present — skipping dump"
  else
    TMP="/tmp/${OBJ}"
    rm -f "$TMP"
    log "dumping ${DB_NAME} via ${DB_HOST}:${DB_PORT} as ${DB_USER}"
    if ! PGPASSWORD="$POSTGRES_SUPERUSER_PASSWORD" pg_dump \
        -h "$DB_HOST" -p "$DB_PORT" -U "$DB_USER" -d "$DB_NAME" \
        -Fc -f "$TMP"; then
      log "ERROR: pg_dump failed"
      rm -f "$TMP"
      return 1
    fi

    SIZE="$(wc -c < "$TMP" | tr -d ' ')"
    if [ "${SIZE:-0}" -lt 1000 ]; then
      log "ERROR: dump suspiciously small (${SIZE} bytes) — refusing to upload"
      rm -f "$TMP"
      return 1
    fi

    log "uploading ${OBJ} (${SIZE} bytes) -> ${REMOTE}/"
    if ! rclone copyto "$TMP" "${REMOTE}/${OBJ}" --s3-no-check-bucket; then
      log "ERROR: upload failed"
      rm -f "$TMP"
      return 1
    fi

    RSIZE="$(rclone size --json "${REMOTE}/${OBJ}" 2>/dev/null \
      | sed -n 's/.*"bytes":\([0-9]*\).*/\1/p')"
    if [ "$RSIZE" != "$SIZE" ]; then
      log "ERROR: verify failed (local=${SIZE} remote=${RSIZE:-missing})"
      rm -f "$TMP"
      return 1
    fi
    rm -f "$TMP"
    log "backup ${OBJ} uploaded + verified (${SIZE} bytes)"
  fi

  # Rolling retention: drop anything older than the window so storage stays bounded.
  log "pruning objects older than ${RETENTION_DAYS}d under ${REMOTE}/"
  if ! rclone delete --min-age "${RETENTION_DAYS}d" "${REMOTE}/"; then
    log "WARN: retention prune returned non-zero"
  fi
  return 0
}

log "starting: remote=${REMOTE} retention=${RETENTION_DAYS}d poll=${POLL_SECS}s"
while true; do
  run_cycle || log "cycle failed — retrying next poll"
  sleep "$POLL_SECS"
done
