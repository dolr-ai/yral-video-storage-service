#!/usr/bin/env bash
# Generate HMAC-SHA256 auth variables for hurl e2e tests.
#
# Usage: ./scripts/generate-hmac-vars.sh <secret> <output_file>
#
# Produces variables consumed by .hurl test files:
#   hmac_ts          – shared Unix timestamp
#   hmac_sig_move    – signature for POST /move-to-nsfw
#   hmac_sig_hls_dup – signature for POST /hls/duplicate

set -euo pipefail

SECRET="${1:?Usage: $0 <secret> <output_file>}"
OUT="${2:?Usage: $0 <secret> <output_file>}"

TS=$(date +%s)

# sign METHOD PATH TIMESTAMP → hex HMAC-SHA256
sign() {
  local method="$1" path="$2" ts="$3"
  printf '%s\n%s\n%s' "$method" "$path" "$ts" \
    | openssl dgst -sha256 -hmac "$SECRET" -hex 2>/dev/null \
    | sed 's/^.* //'
}

{
  echo "hmac_ts=${TS}"
  echo "hmac_sig_move=$(sign POST /move-to-nsfw "$TS")"
  echo "hmac_sig_hls_dup=$(sign POST /hls/duplicate "$TS")"
} >> "$OUT"

echo "✓ HMAC variables appended to $OUT (ts=$TS)"
