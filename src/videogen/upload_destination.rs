use std::fmt;

use chrono::{DateTime, Utc};
use serde::Serializer;

use crate::videogen::rate_limiter::RateLimiterRequestKey;

#[derive(Clone, PartialEq, Eq, serde::Serialize)]
pub struct UploadDestination {
    pub video_id: String,
    pub object_key: String,
    pub upload_url: String,
    #[serde(serialize_with = "serialize_datetime_utc")]
    pub expires_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bucket_url: Option<String>,
}

impl fmt::Debug for UploadDestination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UploadDestination")
            .field("video_id", &self.video_id)
            .field("object_key", &self.object_key)
            .field("upload_url", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("bucket_url", &self.bucket_url)
            .finish()
    }
}

pub fn serialize_datetime_utc<S>(value: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&value.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

// ---------------------------------------------------------------------------
// Upload destination release client
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadDestinationReleaseMode {
    Endpoint,
    DisabledNoEndpoint,
}

pub struct ReleaseUploadDestinationRequest {
    pub request_key: RateLimiterRequestKey,
    pub video_id: String,
    pub object_key: String,
}

impl ReleaseUploadDestinationRequest {
    pub fn to_json_body(&self) -> serde_json::Value {
        serde_json::json!({
            "request_key": {
                "principal": self.request_key.principal,
                "counter": self.request_key.counter,
            },
            "video_id": self.video_id,
            "object_key": self.object_key,
        })
    }
}

pub struct UploadDestinationReleaseClient {
    mode: UploadDestinationReleaseMode,
    endpoint: Option<String>,
    http: reqwest::Client,
}

impl UploadDestinationReleaseClient {
    pub fn from_env() -> Self {
        use crate::consts::VIDEOGEN_UPLOAD_DESTINATION_RELEASE_URL;
        match std::env::var(VIDEOGEN_UPLOAD_DESTINATION_RELEASE_URL) {
            Ok(url) if !url.is_empty() => Self {
                mode: UploadDestinationReleaseMode::Endpoint,
                endpoint: Some(url),
                http: reqwest::Client::new(),
            },
            _ => Self::disabled(),
        }
    }

    pub fn disabled() -> Self {
        Self {
            mode: UploadDestinationReleaseMode::DisabledNoEndpoint,
            endpoint: None,
            http: reqwest::Client::new(),
        }
    }

    pub async fn release(&self, req: ReleaseUploadDestinationRequest) -> Result<(), String> {
        match self.mode {
            UploadDestinationReleaseMode::DisabledNoEndpoint => {
                tracing::info!(
                    principal = %req.request_key.principal,
                    counter = req.request_key.counter,
                    mode = "disabled_no_endpoint",
                    "release_upload_destination skipped"
                );
                Ok(())
            }
            UploadDestinationReleaseMode::Endpoint => {
                let url = self.endpoint.as_deref().unwrap_or_default();
                self.http
                    .post(url)
                    .json(&req.to_json_body())
                    .send()
                    .await
                    .map_err(|e| e.to_string())?
                    .error_for_status()
                    .map_err(|e| e.to_string())?;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::videogen::rate_limiter::RateLimiterRequestKey;
    use chrono::{DateTime, Utc};

    #[test]
    fn release_request_payload_uses_video_id_and_object_key() {
        let request = ReleaseUploadDestinationRequest {
            request_key: RateLimiterRequestKey {
                principal: "aaaaa-aa".to_string(),
                counter: 7,
            },
            video_id: "video-17".to_string(),
            object_key: "generated/video-17.mp4".to_string(),
        };
        let body = request.to_json_body();
        assert_eq!(body["video_id"], "video-17");
        assert_eq!(body["object_key"], "generated/video-17.mp4");
    }

    #[test]
    fn debug_redacts_upload_url() {
        let destination = UploadDestination {
            video_id: "video-1".to_string(),
            object_key: "videos/video-1.mp4".to_string(),
            upload_url: "https://upload.example.test/secret-token".to_string(),
            expires_at: "2026-05-27T12:00:00Z".parse::<DateTime<Utc>>().unwrap(),
            bucket_url: None,
        };

        let debug = format!("{destination:?}");

        assert!(debug.contains("video-1"));
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("secret-token"));
    }
}
