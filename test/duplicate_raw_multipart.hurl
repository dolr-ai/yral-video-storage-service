# ============================================================================
# Multipart Upload Tests
# ============================================================================

# Multipart upload - SFW video
POST {{host}}/duplicate_raw/upload?publisher_user_id={{publisher}}&video_id={{video_id}}_raw_mp&is_nsfw=false
[Multipart]
file: file,test-raw-files/test-raw-video.mp4;
HTTP 200
[Asserts]
jsonpath "$.status" == "pending"
jsonpath "$.expires_in_hours" == 1

# Multipart upload - NSFW video
POST {{host}}/duplicate_raw/upload?publisher_user_id={{publisher}}&video_id={{video_id}}_raw_mp_nsfw&is_nsfw=true
[Multipart]
file: file,test-raw-files/test-raw-video.mp4;
HTTP 200
[Asserts]
jsonpath "$.status" == "pending"

# Multipart upload - missing 'file' field (wrong field name)
POST {{host}}/duplicate_raw/upload?publisher_user_id={{publisher}}&video_id={{video_id}}_raw_mp_bad&is_nsfw=false
[Multipart]
video: file,test-raw-files/test-raw-video.mp4;
HTTP 500

# ============================================================================
# Finalize multipart uploads
# ============================================================================

# Finalize - SFW multipart video
POST {{host}}/duplicate_raw/finalize?publisher_user_id={{publisher}}&video_id={{video_id}}_raw_mp&is_nsfw=false
{
  "metadata": {
    "test": "multipart",
    "title": "Multipart Test Video"
  }
}
HTTP 200
[Asserts]
jsonpath "$.status" == "completed"

# Finalize - NSFW multipart video
POST {{host}}/duplicate_raw/finalize?publisher_user_id={{publisher}}&video_id={{video_id}}_raw_mp_nsfw&is_nsfw=true
{
  "metadata": {
    "test": "multipart"
  }
}
HTTP 200
[Asserts]
jsonpath "$.status" == "completed"
