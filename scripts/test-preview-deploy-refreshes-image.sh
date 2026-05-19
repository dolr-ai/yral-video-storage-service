#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "${TMP_DIR}"' EXIT

fail() {
  echo "$1" >&2
  exit 1
}

APP_DIR="${TMP_DIR}/deploy"
FAKE_BIN="${TMP_DIR}/bin"
DOCKER_LOG="${TMP_DIR}/docker.log"
mkdir -p "${APP_DIR}/scripts/deploy" "${FAKE_BIN}"

cat > "${APP_DIR}/scripts/deploy/render-ha-runtime.sh" <<'RENDER'
#!/usr/bin/env bash
set -euo pipefail
mkdir -p "${APP_DIR}/runtime"
touch "${APP_DIR}/runtime/haproxy-ha.cfg"
RENDER
chmod +x "${APP_DIR}/scripts/deploy/render-ha-runtime.sh"
touch "${APP_DIR}/docker-compose.ha.yml"

cat > "${FAKE_BIN}/docker" <<'DOCKER'
#!/usr/bin/env bash
set -euo pipefail
printf 'docker %s\n' "$*" >> "${DOCKER_LOG}"

if [[ "${1:-}" == "run" ]]; then
  exit 1
fi
DOCKER
chmod +x "${FAKE_BIN}/docker"

PATH="${FAKE_BIN}:${PATH}" \
APP_DIR="${APP_DIR}" \
DOCKER_LOG="${DOCKER_LOG}" \
SERVER_1_IP=10.0.0.1 \
SERVER_2_IP=10.0.0.2 \
SERVER_3_IP=10.0.0.3 \
NODE_NAME=server_1 \
NODE_IP=10.0.0.1 \
POSTGRES_SUPERUSER_PASSWORD=postgres \
REPLICATOR_PASSWORD=replicator \
APP_DB_PASSWORD=app \
PATRONI_API_PASSWORD=patroni \
RUN_APP=true \
APP_IMAGE=ghcr.io/dolr-ai/storj-interface \
IMAGE_TAG=pr-123 \
COMPOSE_PROJECT_NAME=yral-video-storage-service \
  bash "${REPO_ROOT}/deploy/scripts/deploy/deploy-ha.sh"

grep -q 'docker compose -f docker-compose.ha.yml pull storj-interface' "${DOCKER_LOG}" \
  || fail "expected app node deploy to pull storj-interface before compose up"

pull_line="$(grep -n 'docker compose -f docker-compose.ha.yml pull storj-interface' "${DOCKER_LOG}" | head -n1 | cut -d: -f1)"
up_line="$(grep -n 'docker compose -f docker-compose.ha.yml up ' "${DOCKER_LOG}" | head -n1 | cut -d: -f1)"

[[ -n "${up_line}" ]] || fail "expected deploy to run docker compose up"
[[ "${pull_line}" -lt "${up_line}" ]] \
  || fail "expected storj-interface pull to happen before docker compose up"

WORKFLOW_PATH="${REPO_ROOT}/.github/workflows/deploy-preview-prakash.yml"

grep -q '${PREVIEW_URL}/api-docs/openapi.json' "${WORKFLOW_PATH}" \
  || fail "expected preview workflow to smoke-check the OpenAPI document"

grep -q '${PREVIEW_URL}/api/v2/videogen/status/not-a-principal/all' "${WORKFLOW_PATH}" \
  || fail "expected preview workflow to smoke-check the videogen status route"

grep -q 'expected videogen invalid principal route to return 400' "${WORKFLOW_PATH}" \
  || fail "expected preview workflow to assert the videogen route returns 400, not 404"

echo "preview deploy image refresh test ok"
