#!/bin/sh
# Restore a Hetzner-stored pg_dump (-Fc custom format) back into the cluster primary.
# Runs inside the db-backup image (has rclone + pg_restore). Invoke on demand:
#
# The service entrypoint is the backup loop, so override it (--entrypoint sh) to run
# this script instead; the run-command args after the script name are its arguments:
#
#   cd deploy
#   COMPOSE_PROJECT_NAME=yral-video-storage-service \
#     docker compose -f docker-compose.ha.yml --profile backup \
#     run --rm --entrypoint sh db-backup /restore-from-hetzner.sh [YYYYMMDD]
#
# With no date it restores the newest object under the backup prefix; otherwise the
# object for the given UTC date. This is destructive: --clean drops+recreates objects.

set -eu

log() { echo "[restore] $(date -u '+%Y-%m-%dT%H:%M:%SZ') $*"; }

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
REGION="${HETZNER_S3_REGION:-hel1}"

export RCLONE_CONFIG=/dev/null
export RCLONE_CONFIG_HZ_TYPE=s3
export RCLONE_CONFIG_HZ_PROVIDER=Other
export RCLONE_CONFIG_HZ_ACCESS_KEY_ID="$HETZNER_S3_ACCESS_KEY"
export RCLONE_CONFIG_HZ_SECRET_ACCESS_KEY="$HETZNER_S3_SECRET_KEY"
export RCLONE_CONFIG_HZ_ENDPOINT="$HETZNER_S3_ENDPOINT"
export RCLONE_CONFIG_HZ_REGION="$REGION"
REMOTE="hz:${BUCKET}/${PREFIX}"

if [ "${1:-}" != "" ]; then
  OBJ="${DB_NAME}_${1}.dump"
else
  # newest by name; date-stamped names sort chronologically.
  OBJ="$(rclone lsf "${REMOTE}/" | grep -E "^${DB_NAME}_[0-9]{8}\.dump$" | sort | tail -1)"
  [ -n "$OBJ" ] || { log "ERROR: no backups found under ${REMOTE}/"; exit 1; }
fi

TMP="/tmp/${OBJ}"
log "downloading ${REMOTE}/${OBJ}"
rclone copyto "${REMOTE}/${OBJ}" "$TMP"

log "restoring ${OBJ} into ${DB_NAME} via ${DB_HOST}:${DB_PORT} (destructive: --clean)"
PGPASSWORD="$POSTGRES_SUPERUSER_PASSWORD" pg_restore \
  -h "$DB_HOST" -p "$DB_PORT" -U "$DB_USER" -d "$DB_NAME" \
  --clean --if-exists --no-owner --disable-triggers \
  "$TMP"

rm -f "$TMP"
log "restore complete from ${OBJ}"
