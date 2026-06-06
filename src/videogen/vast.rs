use std::fmt;

use crate::videogen::rate_limiter::RateLimiterRequestKey;
use crate::videogen::upload_destination::UploadDestination;
use chrono::{DateTime, Utc};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;

#[derive(Clone, PartialEq, serde::Serialize)]
pub struct VastSubmitRequest {
    pub request_id: String,
    pub request_key: RateLimiterRequestKey,
    pub user_principal: String,
    pub model_id: String,
    pub workflow_json: Value,
    pub input: Value,
    pub callback_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_url_refresh_url: Option<String>,
    pub upload_destination: UploadDestination,
}

impl fmt::Debug for VastSubmitRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VastSubmitRequest")
            .field("request_id", &self.request_id)
            .field("request_key", &self.request_key)
            .field("user_principal", &self.user_principal)
            .field("model_id", &self.model_id)
            .field("workflow_json", &"<redacted>")
            .field("input", &"<redacted>")
            .field("callback_url", &self.callback_url)
            .field("upload_url_refresh_url", &self.upload_url_refresh_url)
            .field("upload_destination", &self.upload_destination)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VastSubmitAccepted {
    pub request_id: String,
    pub status: String,
    pub accepted_at: DateTime<Utc>,
}

#[derive(Debug, thiserror::Error)]
pub enum VastSubmitError {
    #[error("failed to build Vast submit request: {0}")]
    BuildRequest(#[from] reqwest::Error),
    #[error("failed to serialize Vast submit request: {0}")]
    SerializeRequest(#[from] serde_json::Error),
    #[error("Vast submit request failed: {0}")]
    RequestFailed(String),
}

#[derive(Clone)]
pub struct VastHttpClient {
    endpoint: String,
    api_key: String,
    http: reqwest::Client,
}

impl fmt::Debug for VastHttpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VastHttpClient")
            .field("endpoint", &self.endpoint)
            .field("api_key", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl VastHttpClient {
    pub fn new(endpoint: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            api_key: api_key.into(),
            http: reqwest::Client::new(),
        }
    }

    pub fn build_submit_request(
        &self,
        request: VastSubmitRequest,
    ) -> Result<reqwest::Request, VastSubmitError> {
        Ok(self
            .http
            .post(&self.endpoint)
            .header(AUTHORIZATION, format!("Bearer {}", self.api_key))
            .header(CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(&request)?)
            .build()?)
    }
}

#[cfg(test)]
mod tests {
    use super::{VastHttpClient, VastSubmitRequest};
    use crate::videogen::rate_limiter::RateLimiterRequestKey;
    use crate::videogen::upload_destination::UploadDestination;
    use chrono::{DateTime, Utc};
    use reqwest::header::AUTHORIZATION;
    use serde_json::json;

    fn request() -> VastSubmitRequest {
        VastSubmitRequest {
            request_id: "018f5fa2-05c7-4b4a-8934-19b1f3c29d49".to_string(),
            request_key: RateLimiterRequestKey {
                principal: "aaaaa-aa".to_string(),
                counter: 123,
            },
            user_principal: "aaaaa-aa".to_string(),
            model_id: "ltx2".to_string(),
            workflow_json: json!({ "nodes": [] }),
            input: json!({
                "prompt": "make a sunrise over mountains",
                "image_url": "https://example.test/image.png"
            }),
            callback_url: "https://prakash.example/api/v2/videogen/complete".to_string(),
            upload_url_refresh_url: Some(
                "https://prakash.example/api/v2/videogen/upload-url/refresh".to_string(),
            ),
            upload_destination: UploadDestination {
                video_id: "video-1".to_string(),
                object_key: "videos/video-1.mp4".to_string(),
                upload_url: "https://upload.example.test/video-1".to_string(),
                expires_at: "2026-05-27T12:00:00Z".parse::<DateTime<Utc>>().unwrap(),
                bucket_url: Some(
                    "https://link.storjshare.io/raw/example/yral-sfw/aaaaa-aa/video-1.mp4"
                        .to_string(),
                ),
                encrypted_identity: None,
            },
        }
    }

    #[test]
    fn submit_request_serializes_bearer_authorization_header() {
        let client = VastHttpClient::new("https://vast.example.test/submit", "test-vast-key");

        let reqwest_request = client.build_submit_request(request()).unwrap();

        assert_eq!(
            reqwest_request.headers().get(AUTHORIZATION).unwrap(),
            "Bearer test-vast-key"
        );
    }

    #[test]
    fn vast_http_client_debug_redacts_api_key() {
        let client = VastHttpClient::new("https://vast.example.test/submit", "test-vast-key");

        let debug = format!("{client:?}");

        assert!(debug.contains("https://vast.example.test/submit"));
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("test-vast-key"));
    }

    #[test]
    fn vast_submit_request_debug_redacts_model_input_and_upload_url() {
        let debug = format!("{:?}", request());

        assert!(debug.contains("018f5fa2-05c7-4b4a-8934-19b1f3c29d49"));
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("make a sunrise over mountains"));
        assert!(!debug.contains("https://upload.example.test/video-1"));
    }
}
