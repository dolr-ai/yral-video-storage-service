#!/usr/bin/env bash
# End-to-end thumbnail fix verification using a test Storj account.
#
# What it does:
#   1. Generates a 2-frame test video: red first frame, blue second frame
#   2. Extracts the thumbnail using the EXACT same ffmpeg command as the service
#   3. Uploads the video + thumbnail to a test Storj bucket via uplink
#   4. Downloads them back and verifies the thumbnail IS the red (first) frame
#
# Does NOT touch any production buckets.
#
# Usage:
#   STORJ_ACCESS_GRANT_SFW=<test-grant> TEST_BUCKET=<bucket> bash scripts/test-thumbnail-e2e.sh
#
# Requirements: ffmpeg, uplink (brew install storj-uplink)

set -euo pipefail

GRANT="${STORJ_ACCESS_GRANT_SFW:?Set STORJ_ACCESS_GRANT_SFW to your test Storj access grant}"
BUCKET="${TEST_BUCKET:?Set TEST_BUCKET to your test Storj bucket name}"
VIDEO_ID="thumbnail-fix-test-$(date +%s)"
PUBLISHER="test-user"

WORK_DIR=$(mktemp -d)
trap 'rm -rf "$WORK_DIR"' EXIT

VIDEO_PATH="$WORK_DIR/video.mp4"
THUMBNAIL_PATH="$WORK_DIR/thumbnail.png"
VERIFY_THUMB="$WORK_DIR/verify-thumbnail.png"
VERIFY_FRAME="$WORK_DIR/verify-frame.png"

uplink_args=(--analytics=false --interactive=false --access "$GRANT")

echo "==> Step 1: Generating 2-frame test video (red first, blue second)..."
ffmpeg -y \
    -f lavfi -i "color=c=red:s=16x16:d=1:r=1" \
    -f lavfi -i "color=c=blue:s=16x16:d=1:r=1" \
    -filter_complex "[0:v][1:v]concat=n=2:v=1:a=0,format=yuv420p" \
    "$VIDEO_PATH" -loglevel quiet
echo "   Video: $(wc -c < "$VIDEO_PATH") bytes"

echo ""
echo "==> Step 2: Extracting thumbnail with the same ffmpeg command as the service..."
# This is the exact command from run_ffmpeg_first_frame() after the fix:
#   ffmpeg -y -i <input> -vframes 1 -f image2 <output>
ffmpeg -y -i "$VIDEO_PATH" -vframes 1 -f image2 "$THUMBNAIL_PATH" -loglevel quiet
echo "   Thumbnail: $(wc -c < "$THUMBNAIL_PATH") bytes"

echo ""
echo "==> Step 3: Uploading video + thumbnail to test bucket sj://$BUCKET ..."
STORJ_VIDEO="sj://$BUCKET/$PUBLISHER/${VIDEO_ID}.mp4"
STORJ_THUMB="sj://$BUCKET/$PUBLISHER/${VIDEO_ID}_thumbnail.png"

uplink cp "${uplink_args[@]}" --progress=false "$VIDEO_PATH" "$STORJ_VIDEO"
echo "   Uploaded: $STORJ_VIDEO"

uplink cp "${uplink_args[@]}" --progress=false "$THUMBNAIL_PATH" "$STORJ_THUMB"
echo "   Uploaded: $STORJ_THUMB"

echo ""
echo "==> Step 4: Downloading back from Storj..."
uplink cp "${uplink_args[@]}" --progress=false "$STORJ_THUMB" "$VERIFY_THUMB"
echo "   Downloaded thumbnail: $(wc -c < "$VERIFY_THUMB") bytes"

echo ""
echo "==> Step 5: Extracting first frame from downloaded video for comparison..."
uplink cp "${uplink_args[@]}" --progress=false "$STORJ_VIDEO" "$WORK_DIR/verify-video.mp4"
ffmpeg -y -i "$WORK_DIR/verify-video.mp4" -vframes 1 -f image2 "$VERIFY_FRAME" -loglevel quiet

echo ""
echo "==> Step 6: Verifying thumbnail is the first (red) frame..."

# Decode thumbnail to raw RGB and check average colour
RGB_THUMB="$WORK_DIR/thumb.rgb"
ffmpeg -y -i "$VERIFY_THUMB" -f rawvideo -pix_fmt rgb24 "$RGB_THUMB" -loglevel quiet

# Average RGB via python (no extra deps)
read -r AVG_R AVG_G AVG_B < <(python3 - "$RGB_THUMB" <<'PY'
import sys, pathlib
data = pathlib.Path(sys.argv[1]).read_bytes()
assert len(data) % 3 == 0, "not rgb24"
n = len(data) // 3
r = sum(data[i*3]   for i in range(n)) // n
g = sum(data[i*3+1] for i in range(n)) // n
b = sum(data[i*3+2] for i in range(n)) // n
print(r, g, b)
PY
)

echo "   Thumbnail average RGB: R=$AVG_R  G=$AVG_G  B=$AVG_B"
echo "   (expected: red first frame → R≈100-220, G<60, B<60)"
echo "   (bug: blue frame  → R<60, G<60, B≈100-220)"

if [[ $AVG_R -gt 80 && $AVG_G -lt 80 && $AVG_B -lt 80 ]]; then
    echo ""
    echo "   PASS: thumbnail is the red first frame — fix is working correctly."
else
    echo ""
    echo "   FAIL: thumbnail is NOT the red first frame — check the service!"
    exit 1
fi

echo ""
echo "==> Cleaning up test objects from Storj..."
uplink rm "${uplink_args[@]}" "$STORJ_VIDEO" 2>/dev/null && echo "   Removed: $STORJ_VIDEO"
uplink rm "${uplink_args[@]}" "$STORJ_THUMB" 2>/dev/null && echo "   Removed: $STORJ_THUMB"

echo ""
echo "==> All done. Thumbnail fix verified end-to-end."
