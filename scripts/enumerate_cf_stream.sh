#!/usr/bin/env bash
#
# Enumerate ALL Cloudflare Stream video uids -> a plain uid-per-line file.
# Read-only. Used to produce the shard input for delete_cf_stream.sh matrix runs.
#
# Prereq:  export CF_STREAM_API_TOKEN=<token with Stream:Read>
# Usage:   ./enumerate_cf_stream.sh [output_file]     (default: cf_uids.txt)
#
# Pages via the `after` cursor on `created` (asc). Retries transient/rate-limited
# list calls; stops on a genuine empty page or a stalled cursor (ties on created).

set -uo pipefail

ACCOUNT_ID="a209c523d2d9646cc56227dbe6ce3ede"
API="https://api.cloudflare.com/client/v4/accounts/${ACCOUNT_ID}/stream"
OUT="${1:-cf_uids.txt}"

: "${CF_STREAM_API_TOKEN:?export CF_STREAM_API_TOKEN first}"
command -v jq >/dev/null || { echo "jq not found"; exit 1; }

: > "$OUT"
after=""; prev_after="__none__"; pages=0

while : ; do
  url="$API?limit=1000&asc=true"
  [ -n "$after" ] && url="$url&after=$(printf '%s' "$after" | sed 's/:/%3A/g')"

  # retry the list call: it is rate-limited too
  body=""; a=1
  while [ "$a" -le 8 ]; do
    body=$(curl -s -H "Authorization: Bearer $CF_STREAM_API_TOKEN" "$url")
    [ "$(printf '%s' "$body" | jq -r '.success // false' 2>/dev/null)" = "true" ] && break
    sleep $((a * 3)); a=$((a + 1)); body=""
  done
  [ -z "$body" ] && { echo "ERROR: list failed after retries (rate limited?)"; exit 1; }

  n=$(printf '%s' "$body" | jq '.result | length')
  [ "$n" = "0" ] && break

  printf '%s' "$body" | jq -r '.result[].uid' >> "$OUT"
  after=$(printf '%s' "$body" | jq -r '.result[-1].created')
  pages=$((pages + 1))
  echo "page=$pages total=$(wc -l < "$OUT" | tr -d ' ') after=$after"

  [ "$after" = "$prev_after" ] && { echo "cursor stalled — stopping"; break; }
  prev_after="$after"
  [ "$n" -lt 1000 ] && break
done

sort -u "$OUT" -o "$OUT"
echo "ENUM_DONE unique_uids=$(wc -l < "$OUT" | tr -d ' ') -> $OUT"
