#!/usr/bin/env bash
#
# Delete ALL Cloudflare Stream videos for the account below.
# =========================================================================
#  ⚠️  IRREVERSIBLE. Deletes every video in the Stream library.
#  Coverage was verified separately: all live posts exist in yral-sfw /
#  yral-nsfw-videos (2,305-sample = 0 misses + chain-coverage audit).
# =========================================================================
#
# Prereq:  export CF_STREAM_API_TOKEN=<token with Stream:Edit>
#          (jq + curl installed)
#
# Usage:   ./delete_cf_stream.sh
#          PARALLEL=64 ./delete_cf_stream.sh      # tune concurrency
#
# Resumable: safe to Ctrl-C and re-run. Each pass re-fetches the remaining
# videos from CF, so it always continues where it left off. Deleted uids are
# logged to ~/cf_stream_delete/deleted.log; unrecoverable ones to failed.log.

set -uo pipefail

ACCOUNT_ID="a209c523d2d9646cc56227dbe6ce3ede"
# Measured: P=40 -> ~33 req/s with ZERO 429s. P=200 -> ~50% 429s and CF also
# throttles the LIST endpoint. Going higher does not help; it just burns retries.
PARALLEL="${PARALLEL:-40}"
API="https://api.cloudflare.com/client/v4/accounts/${ACCOUNT_ID}/stream"
LOG_DIR="${LOG_DIR:-$HOME/cf_stream_delete}"
DELETED_LOG="$LOG_DIR/deleted.log"
FAILED_LOG="$LOG_DIR/failed.log"
THROTTLE_LOG="$LOG_DIR/throttle.log"

# ---- preflight ----
: "${CF_STREAM_API_TOKEN:?export CF_STREAM_API_TOKEN (needs Stream:Edit) first}"
command -v jq   >/dev/null || { echo "jq not found"; exit 1; }
command -v curl >/dev/null || { echo "curl not found"; exit 1; }
mkdir -p "$LOG_DIR"; touch "$DELETED_LOG" "$FAILED_LOG" "$THROTTLE_LOG"

ok=$(curl -s -H "Authorization: Bearer $CF_STREAM_API_TOKEN" \
     https://api.cloudflare.com/client/v4/user/tokens/verify | jq -r '.success // false')
[ "$ok" = "true" ] || { echo "token verify FAILED — check CF_STREAM_API_TOKEN"; exit 1; }

# ---- confirmation ----
cat <<WARN

============================================================
 DELETE ALL Cloudflare Stream videos
 account: $ACCOUNT_ID
 This is IRREVERSIBLE. Logs -> $LOG_DIR
============================================================
WARN
if [ "${ASSUME_YES:-}" = "DELETE ALL" ]; then
  echo "ASSUME_YES set — proceeding without prompt."
else
  printf 'Type exactly "DELETE ALL" to proceed: '
  read -r ans
  [ "$ans" = "DELETE ALL" ] || { echo "aborted."; exit 1; }
fi

# ---- per-uid deleter (exported for xargs/bash) ----
del_one() {
  local uid="$1" a=1 code
  while [ "$a" -le 6 ]; do
    code=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE \
      -H "Authorization: Bearer $CF_STREAM_API_TOKEN" "$API/$uid")
    case "$code" in
      200|404) printf '%s\n' "$uid" >> "$DELETED_LOG"; return 0 ;;  # gone
      429)     printf 1 >> "$THROTTLE_LOG"; sleep $((a * 3)) ;;      # rate limited: back off
      5[0-9][0-9]) sleep 2 ;;                                        # transient server err
      *)       printf '%s %s\n' "$uid" "$code" >> "$FAILED_LOG"; return 0 ;;
    esac
    a=$((a + 1))
  done
  printf '%s EXHAUSTED\n' "$uid" >> "$FAILED_LOG"
}
export -f del_one
export API CF_STREAM_API_TOKEN DELETED_LOG FAILED_LOG THROTTLE_LOG

# ---- fetch a page of remaining uids ----
# Returns uids on stdout. Distinguishes "genuinely empty" (exit 0, no output) from
# "fetch failed / rate-limited" (exit 1) so a throttled LIST call is never mistaken
# for "all done". CF throttles the list endpoint too under load.
fetch_page() {
  local a=1 body ok
  while [ "$a" -le 8 ]; do
    body=$(curl -s -H "Authorization: Bearer $CF_STREAM_API_TOKEN" "$API?limit=1000&asc=true")
    ok=$(printf '%s' "$body" | jq -r '.success // false' 2>/dev/null)
    if [ "$ok" = "true" ]; then
      printf '%s' "$body" | jq -r '.result[]?.uid'
      return 0
    fi
    sleep $((a * 3))            # backoff: list is rate-limited too
    a=$((a + 1))
  done
  return 1                       # could not fetch — do NOT treat as done
}

# ---- SHARD MODE ----------------------------------------------------------
# For parallel/matrix runs: delete only this shard of a pre-enumerated uid list
# (produced by enumerate_cf_stream.sh). Shards are disjoint, so N runners never
# collide. Live-fetch mode (below) is used when UID_FILE is unset.
#
#   UID_FILE=cf_uids.txt SHARD_INDEX=0 SHARD_TOTAL=8 ./delete_cf_stream.sh
#
if [ -n "${UID_FILE:-}" ]; then
  [ -r "$UID_FILE" ] || { echo "UID_FILE not readable: $UID_FILE"; exit 1; }
  SHARD_INDEX="${SHARD_INDEX:-0}"; SHARD_TOTAL="${SHARD_TOTAL:-1}"
  echo "shard mode: index=$SHARD_INDEX total=$SHARD_TOTAL file=$UID_FILE"

  # this shard's uids, minus anything already deleted (resume support)
  sort -u "$DELETED_LOG" > "$LOG_DIR/.done"
  awk -v i="$SHARD_INDEX" -v n="$SHARD_TOTAL" 'NR % n == i' "$UID_FILE" | sort -u > "$LOG_DIR/.shard"
  comm -23 "$LOG_DIR/.shard" "$LOG_DIR/.done" > "$LOG_DIR/.todo"
  todo=$(wc -l < "$LOG_DIR/.todo" | tr -d ' ')
  echo "shard size=$(wc -l < "$LOG_DIR/.shard" | tr -d ' ')  remaining=$todo"
  [ "$todo" -eq 0 ] && { echo "nothing to do for this shard — DONE"; exit 0; }

  start=$(date +%s)
  base_deleted=$(wc -l < "$DELETED_LOG" | tr -d ' ')

  # ---- periodic progress: a 100k-uid shard runs for hours; don't go silent ----
  progress_loop() {
    while : ; do
      sleep "${PROGRESS_EVERY:-60}"
      el=$(( $(date +%s) - start )); [ "$el" -eq 0 ] && el=1
      d=$(( $(wc -l < "$DELETED_LOG" | tr -d ' ') - base_deleted ))
      r=$(( d / el ))
      left=$(( todo - d )); [ "$left" -lt 0 ] && left=0
      if [ "$r" -gt 0 ]; then eta="$(( left / r / 60 ))m"; else eta="?"; fi
      echo "progress shard=$SHARD_INDEX deleted=$d/$todo ($(( d * 100 / todo ))%) rate=${r}/s 429s=$(wc -c < "$THROTTLE_LOG" | tr -d ' ') failed=$(wc -l < "$FAILED_LOG" | tr -d ' ') eta=$eta"
    done
  }
  progress_loop & PROG_PID=$!
  trap 'kill "$PROG_PID" 2>/dev/null' EXIT INT TERM

  # Optional graceful time-box (MAX_RUNTIME_SECS). CI jobs get hard-killed at their
  # timeout, which can skip cache/artifact post-steps; stopping ourselves a bit early
  # means the run ends cleanly and progress is always persisted. Re-push to continue.
  timeboxed=0
  if [ -n "${MAX_RUNTIME_SECS:-}" ] && command -v timeout >/dev/null 2>&1; then
    timeout "$MAX_RUNTIME_SECS" \
      xargs -P "$PARALLEL" -n 1 bash -c 'del_one "$1"' _ < "$LOG_DIR/.todo" || {
        [ "$?" -eq 124 ] && timeboxed=1
      }
  else
    xargs -P "$PARALLEL" -n 1 bash -c 'del_one "$1"' _ < "$LOG_DIR/.todo"
  fi

  kill "$PROG_PID" 2>/dev/null; trap - EXIT INT TERM

  if [ "$timeboxed" -eq 1 ]; then
    deleted_now=$(( $(wc -l < "$DELETED_LOG" | tr -d ' ') - base_deleted ))
    left=$(( todo - deleted_now )); [ "$left" -lt 0 ] && left=0
    echo "TIME-BOXED after ${MAX_RUNTIME_SECS}s: deleted=$deleted_now of $todo, $left remaining."
    echo "::notice::shard $SHARD_INDEX time-boxed — re-push to continue (progress is cached)"
    exit 0
  fi

  # ---- retry pass: 429-exhausted / transient failures deserve a second try ----
  # Only retry uids in THIS shard that are still not deleted.
  sort -u "$DELETED_LOG" > "$LOG_DIR/.done2"
  awk '{print $1}' "$FAILED_LOG" 2>/dev/null | sort -u > "$LOG_DIR/.failed_uids"
  comm -23 "$LOG_DIR/.failed_uids" "$LOG_DIR/.done2" \
    | comm -12 - "$LOG_DIR/.shard" > "$LOG_DIR/.retry"
  retry_n=$(wc -l < "$LOG_DIR/.retry" | tr -d ' ')
  if [ "$retry_n" -gt 0 ]; then
    echo "retry pass: $retry_n failed uids, at gentler concurrency"
    mv "$FAILED_LOG" "$LOG_DIR/failed.prev"; : > "$FAILED_LOG"
    RETRY_PAR=$(( PARALLEL / 2 )); [ "$RETRY_PAR" -lt 1 ] && RETRY_PAR=1
    xargs -P "$RETRY_PAR" -n 1 bash -c 'del_one "$1"' _ < "$LOG_DIR/.retry"
  fi

  # ---- final status ----
  dur=$(( $(date +%s) - start )); [ "$dur" -eq 0 ] && dur=1
  deleted_now=$(( $(wc -l < "$DELETED_LOG" | tr -d ' ') - base_deleted ))
  residual=$(wc -l < "$FAILED_LOG" | tr -d ' ')
  echo "SHARD_DONE index=$SHARD_INDEX processed=$todo deleted=$deleted_now rate=$(( deleted_now / dur ))/s 429s=$(wc -c < "$THROTTLE_LOG" | tr -d ' ') retried=$retry_n residual_failures=$residual"

  if [ "$residual" -gt 0 ]; then
    echo "::warning::shard $SHARD_INDEX: $residual uids still failing after retry (see failed.log)"
    echo "--- first failures ---"; head -5 "$FAILED_LOG"
    # fail the job only if it's a meaningful share (>1%), so a handful doesn't go red
    if [ "$residual" -gt $(( todo / 100 )) ]; then
      echo "::error::shard $SHARD_INDEX: residual failures exceed 1% of shard — re-push to resume"
      exit 1
    fi
  fi
  echo "(re-push to resume: deleted.log is cached, so completed uids are skipped)"
  exit 0
fi

# ---- LIVE-FETCH delete loop (single-runner default) ----
start=$(date +%s); pass=0; prev_total=-1
while : ; do
  if ! uids=$(fetch_page); then
    echo "ERROR: could not list videos after retries (rate limited?). Re-run to resume."
    exit 1
  fi
  [ -z "$uids" ] && { echo "no videos left — DONE"; break; }

  printf '%s\n' "$uids" | xargs -P "$PARALLEL" -n 1 bash -c 'del_one "$1"' _

  pass=$((pass + 1))
  total=$(wc -l < "$DELETED_LOG" | tr -d ' ')
  dur=$(( $(date +%s) - start )); [ "$dur" -eq 0 ] && dur=1
  thr=$(wc -c < "$THROTTLE_LOG" | tr -d ' ')
  echo "pass=$pass  total_deleted=$total  rate=$(( total / dur ))/s  429s=$thr  failed=$(wc -l < "$FAILED_LOG" | tr -d ' ')"

  # no-progress guard: if a full pass deleted nothing new, stop (all remaining are failures)
  if [ "$total" = "$prev_total" ]; then
    echo "no progress this pass — remaining videos are failing; see $FAILED_LOG"; break
  fi
  prev_total="$total"
done

echo "FINISHED. deleted=$(wc -l < "$DELETED_LOG" | tr -d ' ')  failed=$(wc -l < "$FAILED_LOG" | tr -d ' ')"
[ -s "$FAILED_LOG" ] && echo "Review failures: $FAILED_LOG"
exit 0
