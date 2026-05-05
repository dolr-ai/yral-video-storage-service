# proper input
POST {{host}}/move-to-nsfw
X-Timestamp: {{hmac_ts}}
Authorization: HMAC-SHA256 {{hmac_sig_move}}
{
  "publisher_user_id": "{{publisher}}",
  "video_id": "{{video_id}}"
}
HTTP 200

# Without auth token
POST {{host}}/move-to-nsfw
{
  "publisher_user_id": "{{publisher}}",
  "video_id": "{{video_id}}"
}
HTTP 401

# With wrong signature
POST {{host}}/move-to-nsfw
X-Timestamp: {{hmac_ts}}
Authorization: HMAC-SHA256 0000000000000000000000000000000000000000000000000000000000000000
{
  "publisher_user_id": "{{publisher}}",
  "video_id": "{{video_id}}"
}
HTTP 401

# malformed input
POST {{host}}/move-to-nsfw
X-Timestamp: {{hmac_ts}}
Authorization: HMAC-SHA256 {{hmac_sig_move}}
`malformed json input`
HTTP 415

# missing required information
POST {{host}}/move-to-nsfw
X-Timestamp: {{hmac_ts}}
Authorization: HMAC-SHA256 {{hmac_sig_move}}
{
  "video_id": "{{video_id}}"
}
HTTP 422
