#!/usr/bin/env bash
#
# phash-fleet.sh — trigger the sharded pHash backfill across the prakash fleet.
#
# Each chosen server runs one shard (idx of N) against its own localhost:3005, so
# the boxes do DISJOINT download+ffmpeg work (no duplicated downloads). A box that
# is already running pHash is skipped (avoids a wasteful retrigger; the server-side
# 409 guard on /media/phash/run backs this up).
#
# HMAC signatures are computed LOCALLY on this workstation — the SERVICE_SECRET_TOKEN
# never travels over SSH. Only the signed headers (timestamp + signature) and the
# localhost curl run on each box, so no mirror-client binary is needed there and the
# boxes are never made mutually HTTP-reachable.
#
# The server-side signing string is "METHOD\nPATH\nTIMESTAMP" (path only, query
# excluded), so the query params on the POST do not affect the signature.
#
# Usage:
#   SERVICE_SECRET_TOKEN=... [PHASH_FLEET_SSH_KEY=~/.ssh/key] \
#     ./scripts/phash-fleet.sh --of 3 [--limit N] [--dry-run]
#
#   --of N        Global shard count (required, >= 1). Every box runs idx = its
#                 mapped shard, of N. Must be consistent across the whole fleet —
#                 this single flag enforces that (mis-aligned --of across boxes
#                 would mean overlapping/under-covered shards).
#   --limit N     Optional per-shard row cap (passed through to the job).
#   --dry-run     Print what would be triggered; do not SSH or POST.
#
# Config: edit SERVER_SHARDS below. Key = ssh target, value = shard index for that
# box. The map is the operator's "which servers participate" config — its size need
# not equal --of, but every mapped index must satisfy 0 <= idx < of.
#
# Monitoring (no SSH needed): a single signed call to server_1's public domain
# aggregates all shards via the shared Patroni DB —
#   mirror-client media-runs        # each shard's live totals (requested_by tells them apart)
#   mirror-client media-failures    # grouped failure reasons
#   mirror-client media-audit       # fleet-wide coverage
set -euo pipefail

# ── Server → shard map ────────────────────────────────────────────────────────
# Edit to your fleet. Replace the IPs/hosts with the deploy targets.
# IPs match the prakash fleet in .github/workflows/deploy-prakash-servers.yml.
declare -A SERVER_SHARDS=(
  ["deploy@94.130.13.115"]=0    # server_1 (public domain)
  ["deploy@88.99.151.102"]=1    # server_2
  ["deploy@138.201.129.173"]=2  # server_3
)

PORT=3005
BASE="http://localhost:${PORT}"
STATUS_PATH="/media/jobs/status"
RUN_PATH="/media/phash/run"

# SSH identity. Optional: if PHASH_FLEET_SSH_KEY points to a key file it is passed
# via `ssh -i`; otherwise ssh falls back to ssh-agent / ~/.ssh/config (no key is
# hardcoded). Set it to whichever deploy key you use; default = your ssh-agent /
# ~/.ssh/config identity.
SSH_OPTS=(-o ConnectTimeout=10)
if [[ -n "${PHASH_FLEET_SSH_KEY:-}" ]]; then
  SSH_OPTS+=(-i "${PHASH_FLEET_SSH_KEY}")
fi

OF=""
LIMIT=""
DRY_RUN=false

# ── Args ──────────────────────────────────────────────────────────────────────
need_val() { [[ $# -ge 2 ]] || { echo "error: $1 requires a value" >&2; exit 2; }; }
while [[ $# -gt 0 ]]; do
  case "$1" in
    --of)      need_val "$@"; OF="$2"; shift 2 ;;
    --limit)   need_val "$@"; LIMIT="$2"; shift 2 ;;
    --dry-run) DRY_RUN=true; shift ;;
    -h|--help) sed -n '2,40p' "$0"; exit 0 ;;
    *) echo "error: unknown arg '$1'" >&2; exit 2 ;;
  esac
done

# ── Validation ──────────────────────────────────────────────────────────────────
if ! [[ "${OF}" =~ ^[0-9]+$ ]] || (( OF < 1 )); then
  echo "error: --of N is required and must be an integer >= 1" >&2
  exit 2
fi
if [[ -n "${LIMIT}" ]] && ! [[ "${LIMIT}" =~ ^[0-9]+$ ]]; then
  echo "error: --limit must be a non-negative integer" >&2
  exit 2
fi
if [[ -z "${SERVICE_SECRET_TOKEN:-}" ]]; then
  echo "error: SERVICE_SECRET_TOKEN must be set in the environment" >&2
  exit 2
fi

# Every mapped shard index must be in [0, of); warn on duplicate indices.
declare -A SEEN_IDX=()
for target in "${!SERVER_SHARDS[@]}"; do
  idx="${SERVER_SHARDS[$target]}"
  if ! [[ "${idx}" =~ ^[0-9]+$ ]] || (( idx < 0 )) || (( idx >= OF )); then
    echo "error: ${target} → shard ${idx} is out of range for --of ${OF} (need 0 <= idx < of)" >&2
    exit 2
  fi
  if [[ -n "${SEEN_IDX[$idx]:-}" ]]; then
    echo "error: shard ${idx} mapped to both ${SEEN_IDX[$idx]} and ${target} — would duplicate work; fix SERVER_SHARDS" >&2
    exit 2
  fi
  SEEN_IDX[$idx]="${target}"
done

# ── HMAC signing (local) ─────────────────────────────────────────────────────
# Echoes "<timestamp> <hex-signature>" for METHOD + PATH, signed now.
sign() {
  local method="$1" path="$2" ts sig
  ts="$(date +%s)"
  sig="$(printf '%s\n%s\n%s' "${method}" "${path}" "${ts}" \
    | openssl dgst -sha256 -hmac "${SERVICE_SECRET_TOKEN}" \
    | awk '{print $NF}')"
  printf '%s %s' "${ts}" "${sig}"
}

# ── Trigger loop ────────────────────────────────────────────────────────────────
echo "pHash fleet trigger: of=${OF} limit=${LIMIT:-<none>} dry_run=${DRY_RUN}"
rc=0
for target in "${!SERVER_SHARDS[@]}"; do
  idx="${SERVER_SHARDS[$target]}"
  requested_by="phash-shard-${idx}-of-${OF}"
  run_query="shard=${idx}&of=${OF}&requested_by=${requested_by}"
  [[ -n "${LIMIT}" ]] && run_query="${run_query}&limit=${LIMIT}"

  echo "── ${target}  shard ${idx}/${OF}"

  if [[ "${DRY_RUN}" == "true" ]]; then
    echo "   dry-run: would check ${BASE}${STATUS_PATH}, then POST ${BASE}${RUN_PATH}?${run_query}"
    continue
  fi

  read -r s_ts s_sig <<<"$(sign GET "${STATUS_PATH}")"
  read -r r_ts r_sig <<<"$(sign POST "${RUN_PATH}")"

  # One SSH session per box: read status, skip if already running, else POST.
  ssh "${SSH_OPTS[@]}" "${target}" \
    "STATUS=\$(curl -fsS -H 'X-Timestamp: ${s_ts}' -H 'Authorization: HMAC-SHA256 ${s_sig}' '${BASE}${STATUS_PATH}'); \
     echo \"   status: \${STATUS}\"; \
     if echo \"\${STATUS}\" | grep -q '\"phash_running\":[[:space:]]*true'; then \
       echo '   SKIP: pHash already running on this box'; exit 0; \
     fi; \
     CODE=\$(curl -fsS -o /dev/null -w '%{http_code}' -X POST \
       -H 'X-Timestamp: ${r_ts}' -H 'Authorization: HMAC-SHA256 ${r_sig}' \
       '${BASE}${RUN_PATH}?${run_query}'); \
     echo \"   POST ${RUN_PATH}?${run_query} → HTTP \${CODE}\"; \
     [ \"\${CODE}\" = '202' ] || { echo '   ERROR: expected 202'; exit 1; }" \
    || { echo "   FAILED on ${target}"; rc=1; }
done

echo "done (exit ${rc}). Monitor with: mirror-client media-runs / media-failures / media-audit"
exit "${rc}"
