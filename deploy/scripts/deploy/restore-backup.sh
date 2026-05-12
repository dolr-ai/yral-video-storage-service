#!/usr/bin/env bash
set -euo pipefail

# Restore a pg_dump SQL file into the Patroni primary.
# Must be run on the node where Patroni primary is elected.
#
# Usage:
#   POSTGRES_SUPERUSER_PASSWORD=... APP_DB_PASSWORD=... BACKUP_FILE=/path/to/dump.sql bash restore-backup.sh

APP_DIR="${APP_DIR:-$(cd "$(dirname "$0")/.." && cd .. && pwd)}"
COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-yral-video-storage-service}"
BACKUP_FILE="${BACKUP_FILE:?BACKUP_FILE required — path to .sql dump}"
POSTGRES_SUPERUSER_PASSWORD="${POSTGRES_SUPERUSER_PASSWORD:?POSTGRES_SUPERUSER_PASSWORD required}"
APP_DB_PASSWORD="${APP_DB_PASSWORD:?APP_DB_PASSWORD required}"

# Resolve container ID via Compose to avoid ambiguous grep matching
PATRONI_CONTAINER_ID="$(docker compose \
  --project-name "${COMPOSE_PROJECT_NAME}" \
  -f "${APP_DIR}/docker-compose.ha.yml" \
  ps -q patroni | head -1)"

if [[ -z "${PATRONI_CONTAINER_ID}" ]]; then
  echo "error: patroni container not found — is the HA stack running?" >&2
  exit 1
fi

# Verify this node is the Patroni primary before writing
PATRONI_IP="$(docker inspect "${PATRONI_CONTAINER_ID}" \
  --format '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' | head -1)"

if ! curl -fsS "http://${PATRONI_IP}:8008/primary" > /dev/null; then
  echo "error: Patroni on this node is not the primary — run restore-backup.sh on the primary node" >&2
  exit 1
fi

echo "Primary confirmed. Container: ${PATRONI_CONTAINER_ID}"

# Create storj user and database (idempotent — errors suppressed)
# Use psql variables (:'varname') to avoid SQL injection from password content
docker exec -e PGPASSWORD="${POSTGRES_SUPERUSER_PASSWORD}" "${PATRONI_CONTAINER_ID}" \
  psql -U postgres -v app_db_password="${APP_DB_PASSWORD}" \
  -c "CREATE USER storj WITH PASSWORD :'app_db_password';" 2>/dev/null || true

docker exec -e PGPASSWORD="${POSTGRES_SUPERUSER_PASSWORD}" "${PATRONI_CONTAINER_ID}" \
  psql -U postgres -c "CREATE DATABASE video_fingerprint_index OWNER storj;" 2>/dev/null || true

docker exec -e PGPASSWORD="${POSTGRES_SUPERUSER_PASSWORD}" "${PATRONI_CONTAINER_ID}" \
  psql -U postgres -c "GRANT ALL PRIVILEGES ON DATABASE video_fingerprint_index TO storj;" 2>/dev/null || true

# Restore as superuser — required for SET commands, ownership, extensions in pg_dump output
echo "Restoring ${BACKUP_FILE} ..."
docker exec -i -e PGPASSWORD="${POSTGRES_SUPERUSER_PASSWORD}" "${PATRONI_CONTAINER_ID}" \
  psql -U postgres -d video_fingerprint_index < "${BACKUP_FILE}"

# Transfer ownership of all objects to storj
docker exec -e PGPASSWORD="${POSTGRES_SUPERUSER_PASSWORD}" "${PATRONI_CONTAINER_ID}" \
  psql -U postgres -d video_fingerprint_index \
  -c "REASSIGN OWNED BY postgres TO storj;" 2>/dev/null || true

echo "Restore complete."
