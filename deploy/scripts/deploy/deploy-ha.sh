#!/usr/bin/env bash
set -euo pipefail

# Required env vars:
#   SERVER_1_IP, SERVER_2_IP, SERVER_3_IP
#   NODE_NAME              (server_1 | server_2 | server_3)
#   NODE_IP                (this node's public IP)
#   POSTGRES_SUPERUSER_PASSWORD
#   REPLICATOR_PASSWORD
#   APP_DB_PASSWORD        (password for the 'storj' application user)
#   PATRONI_API_PASSWORD   (password for Patroni REST API)
#
# Optional:
#   APP_DIR         defaults to the deploy/ directory (parent of scripts/)
#   IMAGE_REF       if set, pulls this image ref for storj-interface
#   IMAGE_TAG       tag for ghcr.io/dolr-ai/storj-interface (default: latest)
#   RUN_APP         set to "true" to start storj-interface on this node (all 3 nodes; public domain on server_1 only)

# Resolve to deploy/ directory regardless of where script is invoked from
export APP_DIR="${APP_DIR:-$(cd "$(dirname "$0")/.." && cd .. && pwd)}"

# Explicit project name so Docker Compose volumes are named consistently
# regardless of the directory name APP_DIR resolves to.
export COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-yral-video-storage-service}"

export SERVER_1_IP="${SERVER_1_IP:?SERVER_1_IP required}"
export SERVER_2_IP="${SERVER_2_IP:?SERVER_2_IP required}"
export SERVER_3_IP="${SERVER_3_IP:?SERVER_3_IP required}"
export NODE_NAME="${NODE_NAME:?NODE_NAME required}"
export NODE_IP="${NODE_IP:?NODE_IP required}"
export POSTGRES_SUPERUSER_PASSWORD="${POSTGRES_SUPERUSER_PASSWORD:?POSTGRES_SUPERUSER_PASSWORD required}"
export REPLICATOR_PASSWORD="${REPLICATOR_PASSWORD:?REPLICATOR_PASSWORD required}"
export APP_DB_PASSWORD="${APP_DB_PASSWORD:?APP_DB_PASSWORD required}"
export PATRONI_API_PASSWORD="${PATRONI_API_PASSWORD:?PATRONI_API_PASSWORD required}"

export ETCD_INITIAL_CLUSTER="server_1=http://${SERVER_1_IP}:12380,server_2=http://${SERVER_2_IP}:12380,server_3=http://${SERVER_3_IP}:12380"

bash "${APP_DIR}/scripts/deploy/render-ha-runtime.sh"

# Detect etcd bootstrap state (new vs existing) by checking the named volume.
if docker run --rm \
     -v "${COMPOSE_PROJECT_NAME}_etcd_data:/data" \
     alpine:3 \
     test -f /data/member/snap/db 2>/dev/null; then
  export ETCD_INITIAL_CLUSTER_STATE=existing
else
  export ETCD_INITIAL_CLUSTER_STATE=new
fi

if [[ "${RUN_APP:-false}" == "true" ]]; then
  export COMPOSE_PROFILES="app"
else
  export COMPOSE_PROFILES=""
fi

cd "${APP_DIR}"

if [[ "${RUN_APP:-false}" == "true" ]]; then
  docker compose -f docker-compose.ha.yml pull storj-interface || {
    echo "warning: pull failed, continuing with cached image" >&2
  }
fi

if [[ -n "${IMAGE_REF:-}" ]]; then
  docker compose -f docker-compose.ha.yml build patroni
  docker compose -f docker-compose.ha.yml up -d --no-build --remove-orphans
else
  docker compose -f docker-compose.ha.yml up -d --build --remove-orphans
fi

echo "Done. node=${NODE_NAME} app=${RUN_APP:-false}"
