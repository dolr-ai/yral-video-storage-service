#!/bin/bash

# CI/CD cleanup script for S3 test files
# This script is used by GitHub Actions to clean up test files from Hetzner S3

set -e

# Configuration from environment variables
S3_ACCESS_KEY="${HETZNER_S3_ACCESS_KEY}"
S3_SECRET_KEY="${HETZNER_S3_SECRET_KEY}"
S3_ENDPOINT="${HETZNER_S3_ENDPOINT}"
S3_BUCKET="${HETZNER_S3_BUCKET}"
PUBLISHER="${HURL_publisher}"
VIDEO_ID="${HURL_video_id}"
HLS_VIDEO_ID="${HURL_hls_video_id}"

# Check if required variables are set
if [ -z "$S3_ACCESS_KEY" ] || [ -z "$S3_SECRET_KEY" ] || [ -z "$S3_ENDPOINT" ] || [ -z "$S3_BUCKET" ]; then
    echo "S3 credentials not configured, skipping S3 cleanup"
    exit 0
fi

# Install rclone if not present
if ! command -v rclone &> /dev/null; then
    echo "Installing rclone..."
    curl https://rclone.org/install.sh | sudo bash || true
fi

# Create rclone config
RCLONE_CONFIG="/tmp/rclone-cleanup.conf"
cat > "$RCLONE_CONFIG" << EOF
[hetzner-s3]
type = s3
provider = Other
access_key_id = $S3_ACCESS_KEY
secret_access_key = $S3_SECRET_KEY
endpoint = $S3_ENDPOINT
region = hel1
EOF

echo "Cleaning up S3 test files..."

# Clean up raw multipart upload files if they exist
if [ -n "$PUBLISHER" ] && [ -n "$VIDEO_ID" ]; then
    echo "Removing raw multipart test video: $PUBLISHER/${VIDEO_ID}_raw_mp.mp4"
    rclone delete "hetzner-s3:$S3_BUCKET/$PUBLISHER/${VIDEO_ID}_raw_mp.mp4" --config "$RCLONE_CONFIG" || true

    echo "Removing raw multipart thumbnail: $PUBLISHER/${VIDEO_ID}_raw_mp_thumbnail.png"
    rclone delete "hetzner-s3:$S3_BUCKET/$PUBLISHER/${VIDEO_ID}_raw_mp_thumbnail.png" --config "$RCLONE_CONFIG" || true

    echo "Removing raw multipart thumbnail: $PUBLISHER/${VIDEO_ID}_raw_mp-thumbnail.png"
    rclone delete "hetzner-s3:$S3_BUCKET/$PUBLISHER/${VIDEO_ID}_raw_mp-thumbnail.png" --config "$RCLONE_CONFIG" || true
fi

# Clean up HLS test files if they exist
if [ -n "$HLS_VIDEO_ID" ]; then
    echo "Removing HLS test files: $HLS_VIDEO_ID/"
    rclone delete "hetzner-s3:$S3_BUCKET/$HLS_VIDEO_ID/" --config "$RCLONE_CONFIG" --rmdirs || true
fi

# Clean up config
rm -f "$RCLONE_CONFIG"

echo "S3 cleanup complete"
