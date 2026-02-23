# SFW videos now go to both Storj and Hetzner S3
# Check SFW video exists in Storj (from /duplicate endpoint)
HEAD {{sfw_share}}/{{publisher}}/{{video_id}}.mp4
HTTP 200

# Check NSFW video exists in Storj (from /duplicate endpoint)
HEAD {{nsfw_share}}/{{publisher}}/{{video_id}}.mp4
HTTP 200

# Check SFW video exists in Storj (from multipart /duplicate_raw/finalize endpoint)
HEAD {{sfw_share}}/{{publisher}}/{{video_id}}_raw_mp.mp4
HTTP 200

# Check NSFW video exists in Storj (from multipart /duplicate_raw/finalize endpoint)
HEAD {{nsfw_share}}/{{publisher}}/{{video_id}}_raw_mp_nsfw.mp4
HTTP 200
