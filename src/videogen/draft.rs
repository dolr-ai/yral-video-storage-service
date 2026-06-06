use crate::videogen::rate_limiter::RateLimiterRequestKey;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftCreationRequest {
    pub request_id: String,
    pub request_key: RateLimiterRequestKey,
    pub user_principal: String,
    pub video_id: String,
    pub object_key: String,
    /// AES-256-GCM encrypted `DelegatedIdentityWire`.
    /// Required by the upload service for auth and Storj finalization.
    pub encrypted_identity: Option<String>,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum DraftServiceError {
    #[error("draft service unavailable: {0}")]
    Unavailable(String),
    #[error("draft service rejected request: {0}")]
    Rejected(String),
}

#[async_trait::async_trait]
pub trait DraftServiceClient: Send + Sync {
    async fn create_draft(&self, request: DraftCreationRequest) -> Result<(), DraftServiceError>;
}

// ---------------------------------------------------------------------------
// Real implementation — calls upload.yral.com/update-video-metadata
// ---------------------------------------------------------------------------

pub struct UpdateVideoMetadataDraftClient {
    upload_service_url: String,
    http: reqwest::Client,
}

impl UpdateVideoMetadataDraftClient {
    pub fn new(upload_service_url: impl Into<String>) -> Self {
        Self {
            upload_service_url: upload_service_url.into(),
            http: reqwest::Client::new(),
        }
    }

    pub fn from_env() -> Option<Self> {
        use crate::consts::VIDEOGEN_UPLOAD_SERVICE_DEFAULT_URL;
        let url = std::env::var("VIDEOGEN_UPLOAD_SERVICE_URL")
            .unwrap_or_else(|_| VIDEOGEN_UPLOAD_SERVICE_DEFAULT_URL.to_string());
        if url.is_empty() {
            return None;
        }
        Some(Self::new(url))
    }
}

#[async_trait::async_trait]
impl DraftServiceClient for UpdateVideoMetadataDraftClient {
    async fn create_draft(&self, request: DraftCreationRequest) -> Result<(), DraftServiceError> {
        let encrypted_identity = match &request.encrypted_identity {
            Some(e) => e,
            None => {
                tracing::warn!(
                    request_id = %request.request_id,
                    video_id = %request.video_id,
                    "no encrypted_identity — skipping draft registration"
                );
                return Ok(());
            }
        };

        let identity_wire = crate::videogen::identity_crypto::IdentityCrypto::from_env()
            .and_then(|c| c.decrypt(encrypted_identity))
            .map_err(|e| DraftServiceError::Unavailable(format!("identity decrypt failed: {e}")))?;

        let url = format!(
            "{}/update-video-metadata",
            self.upload_service_url.trim_end_matches('/')
        );

        let body = serde_json::json!({
            "delegated_identity_wire": identity_wire,
            "meta": {},
            "post_details": {
                "id": request.video_id,
                "video_uid": request.video_id,
                "creator_principal": request.user_principal,
                "status": "Draft",
                "hashtags": [],
                "description": ""
            }
        });

        tracing::info!(
            request_id = %request.request_id,
            video_id = %request.video_id,
            user_principal = %request.user_principal,
            "calling update-video-metadata to finalize Storj upload and register draft"
        );

        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| DraftServiceError::Unavailable(e.to_string()))?;

        if resp.status().is_success() {
            tracing::info!(
                request_id = %request.request_id,
                video_id = %request.video_id,
                "update-video-metadata succeeded: Storj finalized, draft registered"
            );
            Ok(())
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            tracing::error!(
                request_id = %request.request_id,
                video_id = %request.video_id,
                %status,
                body = %body,
                "update-video-metadata failed"
            );
            Err(DraftServiceError::Unavailable(format!(
                "update-video-metadata returned {status}: {body}"
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// Stub — logs and returns Ok (used when upload service URL is not set)
// ---------------------------------------------------------------------------

pub struct LoggingDraftServiceClient;

#[async_trait::async_trait]
impl DraftServiceClient for LoggingDraftServiceClient {
    async fn create_draft(&self, request: DraftCreationRequest) -> Result<(), DraftServiceError> {
        tracing::info!(
            request_id = %request.request_id,
            principal = %request.request_key.principal,
            video_id = %request.video_id,
            "draft creation stub: VIDEOGEN_UPLOAD_SERVICE_URL not set"
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Factory — picks real or stub based on env
// ---------------------------------------------------------------------------

pub fn draft_client_from_env() -> Box<dyn DraftServiceClient> {
    match UpdateVideoMetadataDraftClient::from_env() {
        Some(client) => Box::new(client),
        None => {
            tracing::warn!("VIDEOGEN_UPLOAD_SERVICE_URL not set — draft registration skipped");
            Box::new(LoggingDraftServiceClient)
        }
    }
}
