# Check SFW video exists in Storj (from multipart /duplicate_raw/finalize endpoint)
HEAD {{sfw_share}}/{{publisher}}/{{video_id}}_raw_mp.mp4
HTTP 200

# Check NSFW video exists in Storj (from multipart /duplicate_raw/finalize endpoint)
HEAD {{nsfw_share}}/{{publisher}}/{{video_id}}_raw_mp_nsfw.mp4
HTTP 200
