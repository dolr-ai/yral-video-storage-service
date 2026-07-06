use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FeedEventKind {
    HashUpserted,
    MediaVisibilityChanged,
    StorageLocationChanged,
    MediaDeleted,
}

impl FeedEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HashUpserted => "hash_upserted",
            Self::MediaVisibilityChanged => "media_visibility_changed",
            Self::StorageLocationChanged => "storage_location_changed",
            Self::MediaDeleted => "media_deleted",
        }
    }
}

impl FromStr for FeedEventKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "hash_upserted" => Ok(Self::HashUpserted),
            "media_visibility_changed" => Ok(Self::MediaVisibilityChanged),
            "storage_location_changed" => Ok(Self::StorageLocationChanged),
            "media_deleted" => Ok(Self::MediaDeleted),
            _ => Err(value.to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpsertOutcome {
    Inserted,
    Changed,
    Unchanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServableVideoUpsertOutcome {
    pub media: UpsertOutcome,
    pub source_inserted: bool,
}

#[derive(Debug, Clone)]
pub struct ServableVideoInput<'a> {
    pub video_id: &'a str,
    pub publisher_user_id: Option<&'a str>,
    pub post_id: Option<&'a str>,
    pub source_kind: &'a str,
    pub source_ref: Option<&'a str>,
    pub servable_status: &'a str,
    pub nsfw_state: Option<&'a str>,
    pub storage_provider: Option<&'a str>,
    pub bucket: Option<&'a str>,
    pub object_key: Option<&'a str>,
    pub canonical_url: Option<&'a str>,
    pub thumbnail_key: Option<&'a str>,
    pub duration_ms: Option<i64>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub fps: Option<f64>,
    pub container: Option<&'a str>,
    pub video_codec: Option<&'a str>,
    pub audio_codec: Option<&'a str>,
    pub moov_atom_front: Option<bool>,
    pub canonical_encoding_version: Option<&'a str>,
    pub discovered_from: &'a str,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServableVideo {
    pub video_id: String,
    pub publisher_user_id: Option<String>,
    pub post_id: Option<String>,
    pub source_kind: String,
    pub source_ref: Option<String>,
    pub servable_status: String,
    pub nsfw_state: Option<String>,
    pub storage_provider: Option<String>,
    pub bucket: Option<String>,
    pub object_key: Option<String>,
    pub canonical_url: Option<String>,
    pub thumbnail_key: Option<String>,
    pub duration_ms: Option<i64>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub fps: Option<f64>,
    pub container: Option<String>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub moov_atom_front: Option<bool>,
    pub canonical_encoding_version: Option<String>,
    pub discovered_from: String,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct HashRecordInput<'a> {
    pub video_id: &'a str,
    pub hash_kind: &'a str,
    pub hash_version: &'a str,
    pub input_media_version: &'a str,
    pub hash_value: &'a str,
    pub hash_bit_length: i32,
    pub num_frames: i32,
    pub hash_size: i32,
    pub computed_from_provider: Option<&'a str>,
    pub computed_from_bucket: Option<&'a str>,
    pub computed_from_key: Option<&'a str>,
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HashRecord {
    pub video_id: String,
    pub hash_kind: String,
    pub hash_version: String,
    pub input_media_version: String,
    pub hash_value: String,
    pub hash_bit_length: i32,
    pub num_frames: i32,
    pub hash_size: i32,
    pub computed_from_provider: Option<String>,
    pub computed_from_bucket: Option<String>,
    pub computed_from_key: Option<String>,
    pub computed_at: DateTime<Utc>,
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct FeedEventInput<'a> {
    pub event_kind: FeedEventKind,
    pub video_id: &'a str,
    pub hash_kind: Option<&'a str>,
    pub hash_version: Option<&'a str>,
    pub input_media_version: Option<&'a str>,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FeedEvent {
    pub cursor: i64,
    pub event_kind: FeedEventKind,
    pub video_id: String,
    pub hash_kind: Option<String>,
    pub hash_version: Option<String>,
    pub input_media_version: Option<String>,
    pub payload: Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct ExactDuplicateQuery<'a> {
    pub hash_kind: &'a str,
    pub hash_version: &'a str,
    pub hash_value: &'a str,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MediaJobRun {
    pub id: Uuid,
    pub job_kind: String,
    pub status: String,
    pub requested_by: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub cursor: Option<Value>,
    pub totals: Option<Value>,
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MediaJobFailure {
    pub id: i64,
    pub job_run_id: Option<Uuid>,
    pub job_kind: String,
    pub item_key: String,
    pub video_id: Option<String>,
    pub phase: String,
    pub source_ref: Option<String>,
    pub retry_count: i32,
    pub last_error: String,
    pub next_retry_at: Option<DateTime<Utc>>,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::FeedEventKind;

    #[test]
    fn feed_event_kind_parses_all_planned_event_kinds() {
        assert_eq!(
            FeedEventKind::from_str("hash_upserted").unwrap(),
            FeedEventKind::HashUpserted
        );
        assert_eq!(
            FeedEventKind::from_str("media_visibility_changed").unwrap(),
            FeedEventKind::MediaVisibilityChanged
        );
        assert_eq!(
            FeedEventKind::from_str("storage_location_changed").unwrap(),
            FeedEventKind::StorageLocationChanged
        );
        assert_eq!(
            FeedEventKind::from_str("media_deleted").unwrap(),
            FeedEventKind::MediaDeleted
        );
    }
}
