use std::fmt;

use chrono::{DateTime, Utc};
use serde::Serializer;

#[derive(Clone, PartialEq, Eq, serde::Serialize)]
pub struct UploadDestination {
    pub video_id: String,
    pub object_key: String,
    pub upload_url: String,
    #[serde(serialize_with = "serialize_datetime_utc")]
    pub expires_at: DateTime<Utc>,
}

impl fmt::Debug for UploadDestination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UploadDestination")
            .field("video_id", &self.video_id)
            .field("object_key", &self.object_key)
            .field("upload_url", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

pub fn serialize_datetime_utc<S>(value: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&value.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

#[cfg(test)]
mod tests {
    use super::UploadDestination;
    use chrono::{DateTime, Utc};

    #[test]
    fn debug_redacts_scoped_upload_url() {
        let destination = UploadDestination {
            video_id: "video-1".to_string(),
            object_key: "videos/video-1.mp4".to_string(),
            upload_url: "https://upload.example.test/secret-token".to_string(),
            expires_at: "2026-05-27T12:00:00Z".parse::<DateTime<Utc>>().unwrap(),
        };

        let debug = format!("{destination:?}");

        assert!(debug.contains("video-1"));
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("secret-token"));
    }
}
