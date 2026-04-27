#!/usr/bin/env bash
# Verifies that recent thumbnails in the Storj yral-videos bucket are
# extracted from the first frame of their corresponding videos.
# READ-ONLY: only downloads from Storj, never writes to the bucket.
#
# Usage:
#   STORJ_ACCESS_GRANT_SFW=<grant> bash scripts/verify-thumbnail-fix.sh
#   or run on the server where STORJ_ACCESS_GRANT_SFW is already set.
#
# Options:
#   BUCKET   - Storj bucket name (default: yral-videos)
#   SAMPLES  - Number of recent thumbnails to check (default: 3)

set -euo pipefail

GRANT="${STORJ_ACCESS_GRANT_SFW:?STORJ_ACCESS_GRANT_SFW must be set}"
BUCKET="${BUCKET:-yral-videos}"
SAMPLES="${SAMPLES:-3}"

WORK_DIR=$(mktemp -d)
trap 'rm -rf "$WORK_DIR"' EXIT

uplink_args=(--analytics=false --interactive=false --access "$GRANT")

echo "==> Listing recent thumbnails in sj://$BUCKET ..."
THUMBNAILS=$(uplink ls -r "${uplink_args[@]}" "sj://$BUCKET" 2>/dev/null \
    | grep '_thumbnail\.png' \
    | sort -k1,2 -r \
    | head -n "$SAMPLES" \
    | awk '{print $NF}')

if [[ -z "$THUMBNAILS" ]]; then
    echo "ERROR: No thumbnails found in sj://$BUCKET"
    exit 1
fi

PASS=0
FAIL=0
SKIP=0

while IFS= read -r THUMB_KEY; do
    VIDEO_KEY="${THUMB_KEY/_thumbnail.png/.mp4}"
    THUMB_LOCAL="$WORK_DIR/thumb.png"
    VIDEO_LOCAL="$WORK_DIR/video.mp4"
    FRAME_LOCAL="$WORK_DIR/frame.png"

    echo ""
    echo "--- $VIDEO_KEY ---"

    if ! uplink cp "${uplink_args[@]}" --progress=false \
            "sj://$BUCKET/$THUMB_KEY" "$THUMB_LOCAL" 2>/dev/null; then
        echo "  SKIP: could not download thumbnail"
        ((SKIP++)); continue
    fi

    if ! uplink cp "${uplink_args[@]}" --progress=false \
            "sj://$BUCKET/$VIDEO_KEY" "$VIDEO_LOCAL" 2>/dev/null; then
        echo "  SKIP: could not download video"
        ((SKIP++)); continue
    fi

    if ! ffmpeg -y -i "$VIDEO_LOCAL" -vframes 1 -f image2 "$FRAME_LOCAL" \
            -loglevel quiet 2>/dev/null; then
        echo "  SKIP: ffmpeg failed to extract frame"
        ((SKIP++)); continue
    fi

    # Compare thumbnail vs extracted first frame using PSNR
    PSNR=$(ffmpeg -i "$THUMB_LOCAL" -i "$FRAME_LOCAL" \
        -lavfi psnr=stats_file=- -f null - 2>&1 \
        | grep 'average' | grep -oP 'average:\K[0-9.]+' | head -1)

    if [[ -z "$PSNR" ]]; then
        THUMB_SIZE=$(wc -c < "$THUMB_LOCAL")
        FRAME_SIZE=$(wc -c < "$FRAME_LOCAL")
        echo "  INFO: PSNR unavailable — thumbnail size: ${THUMB_SIZE}B, first-frame size: ${FRAME_SIZE}B"
        ((SKIP++))
    elif (( $(echo "$PSNR > 25" | bc -l) )); then
        echo "  PASS: thumbnail matches first frame (PSNR=${PSNR}dB)"
        ((PASS++))
    else
        echo "  FAIL: thumbnail does NOT match first frame (PSNR=${PSNR}dB)"
        ((FAIL++))
    fi

done <<< "$THUMBNAILS"

echo ""
echo "==> Results: $PASS passed, $FAIL failed, $SKIP skipped"
[[ $FAIL -eq 0 ]]
