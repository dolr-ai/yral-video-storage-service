use axum::{extract::State, http::StatusCode, Json};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use candid::Principal;
use chrono::{DateTime, Duration, Utc};
use ic_agent::identity::{DelegatedIdentity, Identity};
use ic_agent::Agent;
use reqwest::header::CONTENT_TYPE;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashMap, fmt, str::FromStr};
use utoipa::ToSchema;
use uuid::Uuid;
use yral_canisters_client::rate_limits::{
    RateLimits, Result1 as CanisterResult, Result_ as CanisterCreateResult,
    TokenType as CanisterTokenType, VideoGenRequestStatus,
};
use yral_types::delegated_identity::DelegatedIdentityWire;

use crate::{
    consts::RATE_LIMITS_CANISTER_ID,
    videogen::{
        config::{ModerationMode, VideogenConfig},
        fingerprint::{compute_request_fingerprint, ImageIdentityInput, RequestFingerprintInput},
        moderation::{ModerationDecision, ModerationError, ModerationInput, ModerationSubject},
        rate_limiter::{
            prepare_create_request_options, to_canister_request_key, RateLimiterCreateOptions,
            RateLimiterRequestKey, RateLimiterTokenType,
        },
        upload_destination::UploadDestination,
        vast::{VastHttpClient, VastSubmitAccepted, VastSubmitError, VastSubmitRequest},
    },
    AppState,
};

const VIDEOGEN_PROPERTY: &str = "VIDEOGEN";
const LTX_PROVIDER: &str = "Ltx2";
const PUBLIC_BASE_URL_ENV: &str = "PRAKASH_PUBLIC_BASE_URL";
const UPLOAD_URL_REFRESH_ENABLED_ENV: &str = "VIDEOGEN_UPLOAD_URL_REFRESH_ENABLED";
const VAST_GENERATE_URL_ENV: &str = "VIDEOGEN_VAST_GENERATE_URL";
const VAST_API_KEY_ENV: &str = "VAST_API_KEY";

#[derive(Deserialize, ToSchema)]
pub struct GenerateVideoRequest {
    pub request: GenerateVideoRequestBody,
    #[schema(value_type = Object)]
    pub delegated_identity: DelegatedIdentityWire,
    #[serde(default)]
    pub upload_handling: Option<VideoUploadHandling>,
}

#[derive(Deserialize, ToSchema)]
pub struct GenerateVideoRequestBody {
    #[serde(rename = "user_id")]
    pub user_id: String,
    pub prompt: String,
    pub model_id: String,
    #[serde(default)]
    pub token_type: Option<GenerateTokenType>,
    #[serde(default)]
    pub negative_prompt: Option<String>,
    #[serde(default)]
    pub image: Option<ImageSource>,
    #[serde(default)]
    pub aspect_ratio: Option<String>,
    #[serde(default)]
    pub duration_seconds: Option<u8>,
    #[serde(default)]
    pub resolution: Option<String>,
    #[serde(default)]
    pub generate_audio: Option<bool>,
    #[serde(default)]
    pub seed: Option<u64>,
    #[serde(default)]
    #[schema(value_type = Object)]
    pub extra_params: HashMap<String, Value>,
}

#[derive(Debug, Clone, Copy, Deserialize, ToSchema, PartialEq, Eq)]
pub enum VideoUploadHandling {
    Client,
    ServerDraft,
}

#[derive(Debug, Clone, Copy, Deserialize, ToSchema, PartialEq, Eq)]
pub enum GenerateTokenType {
    Sats,
    Dolr,
    Free,
    YralProSubscription,
}

#[derive(Clone, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(tag = "type", content = "value")]
pub enum ImageSource {
    Base64(ImageInput),
    Url(String),
}

#[derive(Clone, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ImageInput {
    pub data: String,
    pub mime_type: String,
}

impl fmt::Debug for GenerateVideoRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GenerateVideoRequest")
            .field("request", &"<redacted>")
            .field("delegated_identity", &"<redacted>")
            .field("upload_handling", &self.upload_handling)
            .finish()
    }
}

impl fmt::Debug for GenerateVideoRequestBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GenerateVideoRequestBody")
            .field("user_id", &self.user_id)
            .field("prompt", &"<redacted>")
            .field("model_id", &self.model_id)
            .field("token_type", &self.token_type)
            .field(
                "negative_prompt",
                &self.negative_prompt.as_ref().map(|_| "<redacted>"),
            )
            .field("image", &self.image.as_ref().map(|_| "<redacted>"))
            .field("aspect_ratio", &self.aspect_ratio)
            .field("duration_seconds", &self.duration_seconds)
            .field("resolution", &self.resolution)
            .field("generate_audio", &self.generate_audio)
            .field("seed", &self.seed)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for ImageSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Base64(input) => f
                .debug_struct("Base64")
                .field("mime_type", &input.mime_type)
                .field("data", &"<redacted>")
                .finish(),
            Self::Url(_) => f.debug_tuple("Url").field(&"<redacted>").finish(),
        }
    }
}

impl fmt::Debug for ImageInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImageInput")
            .field("data", &"<redacted>")
            .field("mime_type", &self.mime_type)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, ToSchema, PartialEq, Eq)]
pub struct GenerateResponse {
    pub operation_id: String,
    pub provider: String,
    pub request_key: RateLimiterRequestKey,
}

#[derive(Debug, Serialize, ToSchema, PartialEq, Eq)]
#[allow(dead_code)]
pub enum VideoGenError {
    ProviderError(String),
    InvalidInput(String),
    AuthError,
    NetworkError(String),
    InsufficientBalance,
    InvalidSignature,
    UnsupportedModel(String),
}

#[derive(Debug)]
pub struct GenerateHttpError {
    pub status: StatusCode,
    pub error: VideoGenError,
}

#[derive(Clone)]
pub struct GenerateRequest {
    pub user_id: String,
    pub identity_principal: String,
    pub upload_handling: VideoUploadHandling,
    pub delegated_identity_wire: DelegatedIdentityWire,
    pub prompt: String,
    pub model_id: String,
    pub token_type: Option<GenerateTokenType>,
    pub negative_prompt: Option<String>,
    pub image: Option<ImageSource>,
    pub aspect_ratio: Option<String>,
    pub duration_seconds: Option<u8>,
    pub resolution: Option<String>,
    pub generate_audio: Option<bool>,
    pub seed: Option<u64>,
    pub extra_params: HashMap<String, Value>,
}

impl fmt::Debug for GenerateRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GenerateRequest")
            .field("user_id", &self.user_id)
            .field("identity_principal", &self.identity_principal)
            .field("upload_handling", &self.upload_handling)
            .field("prompt", &"<redacted>")
            .field("model_id", &self.model_id)
            .field("token_type", &self.token_type)
            .field(
                "negative_prompt",
                &self.negative_prompt.as_ref().map(|_| "<redacted>"),
            )
            .field("image", &self.image.as_ref().map(|_| "<redacted>"))
            .field("aspect_ratio", &self.aspect_ratio)
            .field("duration_seconds", &self.duration_seconds)
            .field("resolution", &self.resolution)
            .field("generate_audio", &self.generate_audio)
            .field("seed", &self.seed)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct GenerateConfig {
    pub vast_image_stage_timeout_secs: u64,
    pub callback_url: String,
    pub upload_url_refresh_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UploadDestinationRequest {
    pub user_principal: String,
}

pub use crate::videogen::rate_limiter::RateLimiterError;

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum UploadDestinationError {
    #[error("upload destination unavailable: {0}")]
    Unavailable(String),
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum ImageStageError {
    #[error("image staging timed out")]
    Timeout,
    #[error("image staging failed: {0}")]
    Failed(String),
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum WorkflowError {
    #[error("unsupported workflow: {0}")]
    Unsupported(String),
    #[error("workflow unavailable: {0}")]
    Unavailable(String),
}

#[async_trait::async_trait]
pub trait GenerateDeps: Send + Sync {
    async fn moderate(&self, input: ModerationInput)
        -> Result<ModerationDecision, ModerationError>;
    async fn create_rate_limit(
        &self,
        request: &GenerateRequest,
        options: RateLimiterCreateOptions,
    ) -> Result<RateLimiterRequestKey, RateLimiterError>;
    async fn mark_rate_limit_failed(
        &self,
        request_key: &RateLimiterRequestKey,
        reason: &str,
    ) -> Result<(), RateLimiterError>;
    async fn decrement_rate_limit(
        &self,
        request_key: &RateLimiterRequestKey,
        property: &str,
    ) -> Result<(), RateLimiterError>;
    async fn reserve_upload_destination(
        &self,
        request: UploadDestinationRequest,
    ) -> Result<UploadDestination, UploadDestinationError>;
    async fn release_upload_destination(
        &self,
        request_key: &RateLimiterRequestKey,
        destination: &UploadDestination,
    ) -> Result<(), UploadDestinationError>;
    /// Returns `(cdn_url, storj_key)` for base64 images, `(url, None)` for URL images.
    async fn stage_image(
        &self,
        image: Option<ImageSource>,
        user_principal: &str,
        timeout_secs: u64,
    ) -> Result<Option<(String, Option<String>)>, ImageStageError>;
    async fn workflow_json(
        &self,
        request: &GenerateRequest,
        staged_image_url: Option<&str>,
    ) -> Result<Value, WorkflowError>;
    async fn submit_vast(
        &self,
        request: VastSubmitRequest,
    ) -> Result<VastSubmitAccepted, VastSubmitError>;
}

#[derive(Debug, thiserror::Error)]
pub enum GenerateError {
    #[error("identity mismatch")]
    IdentityMismatch,
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("moderation failed: {0}")]
    Moderation(#[from] ModerationError),
    #[error("rate limiter failed: {0}")]
    RateLimiter(#[from] RateLimiterError),
    #[error("upload destination failed: {0}")]
    UploadDestination(#[from] UploadDestinationError),
    #[error("image staging failed: {0}")]
    ImageStage(#[from] ImageStageError),
    #[error("workflow failed: {0}")]
    Workflow(#[from] WorkflowError),
    #[error("vast submit failed: {0}")]
    Vast(#[from] VastSubmitError),
}

impl From<GenerateError> for GenerateHttpError {
    fn from(error: GenerateError) -> Self {
        match error {
            GenerateError::IdentityMismatch => Self {
                status: StatusCode::UNAUTHORIZED,
                error: VideoGenError::AuthError,
            },
            GenerateError::InvalidInput(message) => Self {
                status: StatusCode::BAD_REQUEST,
                error: VideoGenError::InvalidInput(message),
            },
            GenerateError::Moderation(ref e) => {
                tracing::error!(error = %e, "moderation check failed");
                Self {
                    status: StatusCode::SERVICE_UNAVAILABLE,
                    error: VideoGenError::NetworkError("Moderation unavailable".to_string()),
                }
            }
            GenerateError::RateLimiter(RateLimiterError::Limited(message)) => Self {
                status: StatusCode::TOO_MANY_REQUESTS,
                error: VideoGenError::ProviderError(message),
            },
            GenerateError::RateLimiter(error) => Self {
                status: StatusCode::SERVICE_UNAVAILABLE,
                error: VideoGenError::NetworkError(error.to_string()),
            },
            GenerateError::ImageStage(error) => Self {
                status: StatusCode::SERVICE_UNAVAILABLE,
                error: VideoGenError::NetworkError(error.to_string()),
            },
            GenerateError::Vast(error) => Self {
                status: StatusCode::SERVICE_UNAVAILABLE,
                error: VideoGenError::NetworkError(error.to_string()),
            },
            GenerateError::UploadDestination(error) => Self {
                status: StatusCode::SERVICE_UNAVAILABLE,
                error: VideoGenError::NetworkError(error.to_string()),
            },
            GenerateError::Workflow(WorkflowError::Unsupported(model)) => Self {
                status: StatusCode::BAD_REQUEST,
                error: VideoGenError::UnsupportedModel(model),
            },
            GenerateError::Workflow(WorkflowError::Unavailable(message)) => Self {
                status: StatusCode::SERVICE_UNAVAILABLE,
                error: VideoGenError::ProviderError(message),
            },
        }
    }
}

/// Generate a video through the lean Prakash -> Vast migration path.
#[utoipa::path(
    post,
    path = "/api/v2/videogen/generate",
    tag = "videogen",
    request_body = GenerateVideoRequest,
    responses(
        (status = 200, description = "Video generation submitted", body = GenerateResponse),
        (status = 400, description = "Invalid input", body = VideoGenError),
        (status = 401, description = "Invalid or mismatched identity", body = VideoGenError),
        (status = 429, description = "Rate limit exceeded", body = VideoGenError),
        (status = 503, description = "Provider unavailable", body = VideoGenError),
    )
)]
pub async fn generate_video(
    State(state): State<AppState>,
    Json(request): Json<GenerateVideoRequest>,
) -> Result<Json<GenerateResponse>, (StatusCode, Json<VideoGenError>)> {
    let request = match request.try_into_generate_request() {
        Ok(request) => request,
        Err(error) => {
            let http = GenerateHttpError::from(error);
            return Err((http.status, Json(http.error)));
        }
    };

    let videogen_config = VideogenConfig::from_env().map_err(|error| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(VideoGenError::ProviderError(format!(
                "Videogen configuration error: {error}"
            ))),
        )
    })?;
    let config = GenerateConfig::from_runtime_config(&videogen_config);
    let deps = RuntimeGenerateDeps::new(state, videogen_config);

    generate_with_dependencies(request, &deps, config)
        .await
        .map(Json)
        .map_err(|error| {
            tracing::error!(status = %error.status, error = ?error.error, "generate_video failed");
            (error.status, Json(error.error))
        })
}

impl GenerateVideoRequest {
    fn try_into_generate_request(self) -> Result<GenerateRequest, GenerateError> {
        let delegated_identity_wire = self.delegated_identity.clone();
        let identity: DelegatedIdentity = self
            .delegated_identity
            .try_into()
            .map_err(|_error: k256::elliptic_curve::Error| GenerateError::IdentityMismatch)?;
        let identity_principal = identity
            .sender()
            .map_err(|_| GenerateError::IdentityMismatch)?
            .to_string();

        Ok(GenerateRequest {
            user_id: self.request.user_id,
            identity_principal,
            upload_handling: self.upload_handling.ok_or_else(|| {
                GenerateError::InvalidInput("upload_handling must be ServerDraft".to_string())
            })?,
            delegated_identity_wire,
            prompt: self.request.prompt,
            model_id: self.request.model_id,
            token_type: self.request.token_type,
            negative_prompt: self.request.negative_prompt,
            image: self.request.image,
            aspect_ratio: self.request.aspect_ratio,
            duration_seconds: self.request.duration_seconds,
            resolution: self.request.resolution,
            generate_audio: self.request.generate_audio,
            seed: self.request.seed,
            extra_params: self.request.extra_params,
        })
    }
}

impl GenerateConfig {
    fn from_runtime_config(config: &VideogenConfig) -> Self {
        let public_base_url = std::env::var(PUBLIC_BASE_URL_ENV)
            .unwrap_or_else(|_| "http://localhost:3000".to_string())
            .trim_end_matches('/')
            .to_string();
        let refresh_enabled = std::env::var(UPLOAD_URL_REFRESH_ENABLED_ENV)
            .map(|value| value.eq_ignore_ascii_case("true") || value == "1")
            .unwrap_or(false);

        Self {
            vast_image_stage_timeout_secs: config.vast_image_stage_timeout_secs,
            callback_url: format!("{public_base_url}/api/v2/videogen/complete"),
            upload_url_refresh_url: refresh_enabled
                .then(|| format!("{public_base_url}/api/v2/videogen/upload-url/refresh")),
        }
    }
}

pub async fn generate_with_dependencies<D: GenerateDeps>(
    request: GenerateRequest,
    deps: &D,
    config: GenerateConfig,
) -> Result<GenerateResponse, GenerateHttpError> {
    generate_inner(request, deps, config)
        .await
        .map_err(GenerateHttpError::from)
}

async fn generate_inner<D: GenerateDeps>(
    request: GenerateRequest,
    deps: &D,
    config: GenerateConfig,
) -> Result<GenerateResponse, GenerateError> {
    let claimed_principal = Principal::from_str(&request.user_id)
        .map_err(|error| GenerateError::InvalidInput(format!("Invalid user_id: {error}")))?;
    if request.identity_principal != claimed_principal.to_string() {
        return Err(GenerateError::IdentityMismatch);
    }

    if request.upload_handling != VideoUploadHandling::ServerDraft {
        return Err(GenerateError::InvalidInput(
            "Only ServerDraft upload_handling is supported".to_string(),
        ));
    }
    if request.model_id != "ltx2" {
        return Err(GenerateError::Workflow(WorkflowError::Unsupported(
            request.model_id.clone(),
        )));
    }
    if request.prompt.trim().is_empty() {
        return Err(GenerateError::InvalidInput(
            "Prompt must not be empty".to_string(),
        ));
    }

    let fingerprint = compute_request_fingerprint(&fingerprint_input(&request)?)
        .map_err(|error| GenerateError::InvalidInput(error.to_string()))?;

    // Base64 images must be staged to Storj before moderation because the
    // moderation service only supports URL-based image checks (detect-base64
    // returns 500 server-side). URL images and text-only are moderated directly.
    let (pre_staged_url, pre_staged_key) = match request.image.as_ref() {
        Some(ImageSource::Base64(_)) => match deps
            .stage_image(
                request.image.clone(),
                &request.user_id,
                config.vast_image_stage_timeout_secs,
            )
            .await
        {
            Ok(Some((url, key))) => (Some(url), key),
            Ok(None) => (None, None),
            Err(error) => return Err(error.into()),
        },
        _ => (None, None),
    };

    let moderation_subject = match request.image.as_ref() {
        None => ModerationSubject::TextOnly,
        Some(ImageSource::Url(url)) => ModerationSubject::ImageUrl(url.clone()),
        Some(ImageSource::Base64(_)) => match pre_staged_url.clone() {
            Some(url) => ModerationSubject::ImageUrl(url),
            None => ModerationSubject::TextOnly,
        },
    };
    let moderation_decision = deps
        .moderate(ModerationInput {
            request_id: fingerprint.request_fingerprint.clone(),
            user_principal: request.user_id.clone(),
            prompt: request.prompt.clone(),
            subject: moderation_subject,
        })
        .await
        .map_err(|e| {
            if let Some(key) = pre_staged_key.clone() {
                let bucket = crate::consts::YRAL_VIDEOS.clone();
                let access_grant = crate::consts::MIRROR_ACCESS_GRANT.clone();
                tokio::spawn(async move {
                    let _ = crate::jobs::uplink_rm(&format!("sj://{bucket}/{key}"), &access_grant)
                        .await;
                });
            }
            GenerateError::Moderation(e)
        })?;
    if moderation_decision == ModerationDecision::Unsafe {
        if let Some(key) = pre_staged_key {
            let bucket = crate::consts::YRAL_VIDEOS.clone();
            let access_grant = crate::consts::MIRROR_ACCESS_GRANT.clone();
            tokio::spawn(async move {
                let _ =
                    crate::jobs::uplink_rm(&format!("sj://{bucket}/{key}"), &access_grant).await;
            });
        }
        return Err(GenerateError::InvalidInput(
            "Content violates safety guidelines".to_string(),
        ));
    }

    let rate_limit_options = prepare_create_request_options(
        RateLimiterRequestKey {
            principal: request.user_id.clone(),
            counter: 0,
        },
        request.token_type.map(rate_limiter_token_type),
    );
    let request_key = deps.create_rate_limit(&request, rate_limit_options).await?;
    let operation_id = operation_id(&request_key);

    // For base64 images already staged above; for URL images, stage_image is a no-op passthrough.
    let (staged_image_url, staged_image_key) = if pre_staged_url.is_some() {
        (pre_staged_url, pre_staged_key.clone())
    } else {
        match deps
            .stage_image(
                request.image.clone(),
                &request.user_id,
                config.vast_image_stage_timeout_secs,
            )
            .await
        {
            Ok(Some((url, key))) => (Some(url), key),
            Ok(None) => (None, None),
            Err(error) => {
                fail_after_rate_limit(deps, &request_key, None, "image staging failed").await;
                return Err(error.into());
            }
        }
    };

    let workflow_json = match deps
        .workflow_json(&request, staged_image_url.as_deref())
        .await
    {
        Ok(workflow_json) => workflow_json,
        Err(error) => {
            fail_after_rate_limit(deps, &request_key, None, "workflow selection failed").await;
            return Err(error.into());
        }
    };

    let mut upload_destination = match deps
        .reserve_upload_destination(UploadDestinationRequest {
            user_principal: request.user_id.clone(),
        })
        .await
    {
        Ok(destination) => destination,
        Err(error) => {
            fail_after_rate_limit(deps, &request_key, None, "upload destination failed").await;
            return Err(error.into());
        }
    };

    upload_destination.encrypted_identity =
        match crate::videogen::identity_crypto::IdentityCrypto::from_env() {
            Ok(crypto) => match crypto.encrypt(&request.delegated_identity_wire) {
                Ok(blob) => Some(blob),
                Err(e) => {
                    tracing::warn!("identity encryption failed: {e} — draft will be skipped");
                    None
                }
            },
            Err(e) => {
                tracing::warn!("identity crypto not configured: {e} — draft will be skipped");
                None
            }
        };

    let request_id = Uuid::new_v4().to_string();
    let vast_request = VastSubmitRequest {
        request_id: request_id.clone(),
        request_key: request_key.clone(),
        user_principal: request.user_id.clone(),
        model_id: request.model_id.clone(),
        workflow_json,
        input: vast_input(&request, staged_image_url.as_deref()),
        callback_url: config.callback_url,
        upload_url_refresh_url: config.upload_url_refresh_url,
        upload_destination: upload_destination.clone(),
        staged_image_key,
    };

    match deps.submit_vast(vast_request).await {
        Ok(accepted)
            if accepted.request_id == request_id && is_accepted_status(&accepted.status) =>
        {
            // submitted successfully
        }
        Ok(accepted) => {
            let reason = format!(
                "Vast did not accept request: status={}, echoed_request_id_matches={}",
                accepted.status,
                accepted.request_id == request_id
            );
            fail_after_rate_limit(deps, &request_key, Some(&upload_destination), &reason).await;
            return Err(GenerateError::Vast(VastSubmitError::RequestFailed(reason)));
        }
        Err(error) => {
            fail_after_rate_limit(
                deps,
                &request_key,
                Some(&upload_destination),
                "Vast submit failed",
            )
            .await;
            return Err(error.into());
        }
    }

    Ok(GenerateResponse {
        operation_id,
        provider: LTX_PROVIDER.to_string(),
        request_key,
    })
}

async fn fail_after_rate_limit<D: GenerateDeps>(
    deps: &D,
    request_key: &RateLimiterRequestKey,
    upload_destination: Option<&UploadDestination>,
    reason: &str,
) {
    let _ = deps.mark_rate_limit_failed(request_key, reason).await;
    let _ = deps
        .decrement_rate_limit(request_key, VIDEOGEN_PROPERTY)
        .await;
    if let Some(destination) = upload_destination {
        let _ = deps
            .release_upload_destination(request_key, destination)
            .await;
    }
}

fn fingerprint_input(request: &GenerateRequest) -> Result<RequestFingerprintInput, GenerateError> {
    Ok(RequestFingerprintInput {
        principal: request.user_id.clone(),
        model_id: request.model_id.clone(),
        prompt: request.prompt.clone(),
        negative_prompt: request.negative_prompt.clone(),
        aspect_ratio: request
            .aspect_ratio
            .clone()
            .unwrap_or_else(|| "16:9".to_string()),
        duration: request.duration_seconds.unwrap_or(5) as u32,
        resolution: request
            .resolution
            .clone()
            .unwrap_or_else(|| "720p".to_string()),
        seed: request.seed.map(|seed| seed as i64),
        generate_audio: request.generate_audio.unwrap_or(false),
        upload_handling: "ServerDraft".to_string(),
        token_type: format!(
            "{:?}",
            request.token_type.unwrap_or(GenerateTokenType::Free)
        ),
        image: match &request.image {
            None => ImageIdentityInput::None,
            Some(ImageSource::Url(url)) => ImageIdentityInput::Reference(url.clone()),
            Some(ImageSource::Base64(image)) => ImageIdentityInput::Base64(image.data.clone()),
        },
    })
}

fn rate_limiter_token_type(token_type: GenerateTokenType) -> RateLimiterTokenType {
    match token_type {
        GenerateTokenType::Free => RateLimiterTokenType::Free,
        GenerateTokenType::Sats => RateLimiterTokenType::Sats,
        GenerateTokenType::Dolr => RateLimiterTokenType::Dolr,
        GenerateTokenType::YralProSubscription => RateLimiterTokenType::YralProSubscription,
    }
}

fn operation_id(key: &RateLimiterRequestKey) -> String {
    format!("{}_{}", key.principal, key.counter)
}

fn vast_input(request: &GenerateRequest, staged_image_url: Option<&str>) -> Value {
    let mut input = json!({
        "prompt": request.prompt,
        "negative_prompt": request.negative_prompt,
        "aspect_ratio": request.aspect_ratio.as_deref().unwrap_or("16:9"),
        "duration_seconds": request.duration_seconds.unwrap_or(5),
        "resolution": request.resolution.as_deref().unwrap_or("720p"),
        "generate_audio": request.generate_audio.unwrap_or(false),
        "seed": request.seed,
    });
    if let Some(staged_image_url) = staged_image_url {
        input["image_url"] = Value::String(staged_image_url.to_string());
    }
    if let Some(object) = input.as_object_mut() {
        object.extend(request.extra_params.clone());
    }
    input
}

fn is_accepted_status(status: &str) -> bool {
    matches!(status, "submitted" | "queued")
}

#[derive(Clone)]
struct RuntimeGenerateDeps {
    ic_agent: Agent,
    config: VideogenConfig,
    http: reqwest::Client,
}

impl RuntimeGenerateDeps {
    fn new(state: AppState, config: VideogenConfig) -> Self {
        Self {
            ic_agent: state.ic_agent,
            config,
            http: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl GenerateDeps for RuntimeGenerateDeps {
    async fn moderate(
        &self,
        input: ModerationInput,
    ) -> Result<ModerationDecision, ModerationError> {
        if self.config.moderation_mode == ModerationMode::MockAllow {
            return Ok(ModerationDecision::Safe);
        }

        let secret = std::env::var(crate::consts::MODERATION_HMAC_SECRET).map_err(|_| {
            ModerationError::RequestFailed(format!(
                "{} is required",
                crate::consts::MODERATION_HMAC_SECRET
            ))
        })?;

        let (path, request_body) = match &input.subject {
            ModerationSubject::TextOnly => (
                "/v1/text/detect",
                serde_json::to_vec(&json!({ "text": input.prompt }))
                    .map_err(|e| ModerationError::RequestFailed(e.to_string()))?,
            ),
            ModerationSubject::ImageUrl(url) => (
                "/v1/images/detect-url",
                serde_json::to_vec(&json!({ "image_url": url, "prompt": input.prompt }))
                    .map_err(|e| ModerationError::RequestFailed(e.to_string()))?,
            ),
        };

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let signature = crate::videogen::hmac::sign_moderation_request(
            secret.as_bytes(),
            "POST",
            path,
            timestamp,
            &request_body,
        );

        let url = format!(
            "{}{}",
            crate::consts::MODERATION_SERVICE_URL.trim_end_matches('/'),
            path
        );
        let subject_url = match &input.subject {
            ModerationSubject::ImageUrl(u) => Some(u.as_str()),
            _ => None,
        };
        tracing::debug!(path, subject_url, "calling moderation service");
        let response = self
            .http
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .header("X-Internal-Timestamp", timestamp.to_string())
            .header("X-Internal-Signature", signature)
            .timeout(std::time::Duration::from_millis(
                self.config.moderation_timeout_ms,
            ))
            .body(request_body)
            .send()
            .await
            .map_err(|e| {
                tracing::warn!(path, subject_url, error = %e, "moderation request failed");
                ModerationError::RequestFailed(e.to_string())
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let body_text = response.text().await.unwrap_or_default();
            tracing::warn!(path, subject_url, %status, body = %body_text, "moderation service error response");
            return Err(ModerationError::RequestFailed(format!(
                "moderation service returned {status}: {body_text}"
            )));
        }

        let body: Value = response
            .json()
            .await
            .map_err(|e| ModerationError::RequestFailed(e.to_string()))?;

        let is_nsfw = body
            .get("is_nsfw")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if is_nsfw {
            Ok(ModerationDecision::Unsafe)
        } else {
            Ok(ModerationDecision::Safe)
        }
    }

    async fn create_rate_limit(
        &self,
        request: &GenerateRequest,
        options: RateLimiterCreateOptions,
    ) -> Result<RateLimiterRequestKey, RateLimiterError> {
        let principal = Principal::from_text(&options.request_key.principal)
            .map_err(|error| RateLimiterError::Rejected(error.to_string()))?;
        let token_type = match options.token_type {
            RateLimiterTokenType::Free => CanisterTokenType::Free,
            RateLimiterTokenType::Sats => CanisterTokenType::Sats,
            RateLimiterTokenType::Dolr => CanisterTokenType::Dolr,
            RateLimiterTokenType::YralProSubscription => CanisterTokenType::YralProSubscription,
        };
        let payment_amount = options.payment_amount.map(|amount| amount.to_string());
        let rate_limits = RateLimits(*RATE_LIMITS_CANISTER_ID, &self.ic_agent);
        let result = rate_limits
            .create_video_generation_request_v_2(
                principal,
                request.model_id.clone(),
                request.prompt.clone(),
                VIDEOGEN_PROPERTY.to_string(),
                token_type,
                true,
                options.is_paid,
                payment_amount,
            )
            .await
            .map_err(|error| RateLimiterError::Unavailable(error.to_string()))?;
        match result {
            CanisterCreateResult::Ok(key) => Ok(RateLimiterRequestKey {
                principal: key.principal.to_string(),
                counter: key.counter,
            }),
            CanisterCreateResult::Err(error)
                if error.contains("Rate limit exceeded")
                    || error.contains("Property rate limit exceeded") =>
            {
                Err(RateLimiterError::Limited(error))
            }
            CanisterCreateResult::Err(error) => Err(RateLimiterError::Rejected(error)),
        }
    }

    async fn mark_rate_limit_failed(
        &self,
        request_key: &RateLimiterRequestKey,
        reason: &str,
    ) -> Result<(), RateLimiterError> {
        let rate_limits = RateLimits(*RATE_LIMITS_CANISTER_ID, &self.ic_agent);
        let key = to_canister_request_key(request_key)?;
        match rate_limits
            .update_video_generation_status(key, VideoGenRequestStatus::Failed(reason.to_string()))
            .await
            .map_err(|error| RateLimiterError::Unavailable(error.to_string()))?
        {
            CanisterResult::Ok => Ok(()),
            CanisterResult::Err(error) => Err(RateLimiterError::Rejected(error)),
        }
    }

    async fn decrement_rate_limit(
        &self,
        request_key: &RateLimiterRequestKey,
        property: &str,
    ) -> Result<(), RateLimiterError> {
        let rate_limits = RateLimits(*RATE_LIMITS_CANISTER_ID, &self.ic_agent);
        let key = to_canister_request_key(request_key)?;
        match rate_limits
            .decrement_video_generation_counter_v_1(key, property.to_string())
            .await
            .map_err(|error| RateLimiterError::Unavailable(error.to_string()))?
        {
            CanisterResult::Ok => Ok(()),
            CanisterResult::Err(error) => Err(RateLimiterError::Rejected(error)),
        }
    }

    async fn reserve_upload_destination(
        &self,
        request: UploadDestinationRequest,
    ) -> Result<UploadDestination, UploadDestinationError> {
        let url = format!(
            "{}/get-upload-url",
            crate::consts::VIDEOGEN_UPLOAD_SERVICE_DEFAULT_URL
        );
        let body = serde_json::to_vec(&json!({
            "publisher_user_id": request.user_principal,
        }))
        .map_err(|error| UploadDestinationError::Unavailable(error.to_string()))?;
        tracing::info!(url = %url, user = %request.user_principal, "calling upload service");
        let response = self
            .http
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .timeout(std::time::Duration::from_secs(
                self.config.upload_destination_timeout_secs,
            ))
            .body(body)
            .send()
            .await
            .map_err(|error| {
                tracing::error!(cause = %error, is_builder = error.is_builder(), is_connect = error.is_connect(), "upload service send failed");
                UploadDestinationError::Unavailable(error.to_string())
            })?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| UploadDestinationError::Unavailable(error.to_string()))?;
        if !status.is_success() {
            tracing::error!(status = %status, body = %body, "upload service non-success");
            return Err(UploadDestinationError::Unavailable(format!(
                "upload service returned {status}: {body}"
            )));
        }
        let response: UploadServiceResponse<UploadServiceData> = serde_json::from_str(&body)
            .map_err(|error| {
                tracing::error!(body = %body, cause = %error, "upload service response parse failed");
                UploadDestinationError::Unavailable(error.to_string())
            })?;
        if !response.success {
            let msg = response
                .error_message
                .unwrap_or_else(|| "upload service did not return success".to_string());
            tracing::error!(body = %msg, "upload service returned success=false");
            return Err(UploadDestinationError::Unavailable(msg));
        }
        let data = response.data.ok_or_else(|| {
            UploadDestinationError::Unavailable("upload service response missing data".to_string())
        })?;
        let video_id = data.video_id.ok_or_else(|| {
            UploadDestinationError::Unavailable(
                "upload service response missing video_id".to_string(),
            )
        })?;
        let upload_url = data.upload_url.ok_or_else(|| {
            UploadDestinationError::Unavailable(
                "upload service response missing upload_url".to_string(),
            )
        })?;

        let bucket_url = crate::consts::STORJ_SFW_SHARE_URL
            .as_ref()
            .map(|base| format!("{}/{}/{}.mp4", base, request.user_principal, video_id));
        Ok(UploadDestination {
            object_key: format!("{video_id}.mp4"),
            upload_url,
            video_id,
            expires_at: Utc::now() + Duration::seconds(self.config.upload_url_ttl_secs as i64),
            bucket_url,
            encrypted_identity: None,
        })
    }

    async fn release_upload_destination(
        &self,
        request_key: &RateLimiterRequestKey,
        destination: &UploadDestination,
    ) -> Result<(), UploadDestinationError> {
        use crate::videogen::upload_destination::{
            ReleaseUploadDestinationRequest, UploadDestinationReleaseClient,
        };
        UploadDestinationReleaseClient::from_env()
            .release(ReleaseUploadDestinationRequest {
                request_key: request_key.clone(),
                video_id: destination.video_id.clone(),
                object_key: destination.object_key.clone(),
            })
            .await
            .map_err(UploadDestinationError::Unavailable)
    }

    async fn stage_image(
        &self,
        image: Option<ImageSource>,
        user_principal: &str,
        timeout_secs: u64,
    ) -> Result<Option<(String, Option<String>)>, ImageStageError> {
        match image {
            None => Ok(None),
            Some(ImageSource::Url(url)) => Ok(Some((url, None))),
            Some(ImageSource::Base64(image)) => {
                let bytes = STANDARD
                    .decode(image.data.as_bytes())
                    .map_err(|error| ImageStageError::Failed(error.to_string()))?;

                let cdn_base = crate::consts::STORJ_SFW_SHARE_URL
                    .as_deref()
                    .ok_or_else(|| {
                        ImageStageError::Failed("SFW_SHARE_EU1_URL is required".to_string())
                    })?
                    .trim_end_matches('/');
                let bucket = crate::consts::YRAL_VIDEOS.as_str();
                let access_grant = crate::consts::MIRROR_ACCESS_GRANT.as_str();

                let extension = image_extension(&image.mime_type);
                let key = format!("_input/{}/{}.{}", user_principal, Uuid::new_v4(), extension);

                let tmp = tempfile::Builder::new()
                    .suffix(&format!(".{extension}"))
                    .tempfile()
                    .map_err(|e| ImageStageError::Failed(e.to_string()))?;
                tokio::fs::write(tmp.path(), &bytes)
                    .await
                    .map_err(|e| ImageStageError::Failed(e.to_string()))?;

                let dst = format!("sj://{bucket}/{key}");
                tokio::time::timeout(
                    std::time::Duration::from_secs(timeout_secs),
                    crate::jobs::uplink_cp(tmp.path(), &dst, access_grant),
                )
                .await
                .map_err(|_| ImageStageError::Timeout)?
                .map_err(|e| ImageStageError::Failed(e.to_string()))?;

                Ok(Some((format!("{cdn_base}/{key}"), Some(key))))
            }
        }
    }

    async fn workflow_json(
        &self,
        request: &GenerateRequest,
        staged_image_url: Option<&str>,
    ) -> Result<Value, WorkflowError> {
        if request.model_id != "ltx2" {
            return Err(WorkflowError::Unsupported(request.model_id.clone()));
        }
        Ok(build_ltx2_workflow(request, staged_image_url))
    }

    async fn submit_vast(
        &self,
        request: VastSubmitRequest,
    ) -> Result<VastSubmitAccepted, VastSubmitError> {
        use crate::videogen::config::VastSubmitTransport;
        match self.config.vast_submit_transport {
            VastSubmitTransport::Http => self.submit_vast_http(request).await,
            VastSubmitTransport::RabbitMq => self.submit_vast_rabbitmq(request).await,
        }
    }
}

impl RuntimeGenerateDeps {
    async fn submit_vast_http(
        &self,
        request: VastSubmitRequest,
    ) -> Result<VastSubmitAccepted, VastSubmitError> {
        let endpoint = std::env::var(VAST_GENERATE_URL_ENV).map_err(|_| {
            VastSubmitError::RequestFailed(format!("{VAST_GENERATE_URL_ENV} is required"))
        })?;
        let api_key = std::env::var(VAST_API_KEY_ENV).map_err(|_| {
            VastSubmitError::RequestFailed(format!("{VAST_API_KEY_ENV} is required"))
        })?;
        let client = VastHttpClient::new(endpoint, api_key);
        let reqwest_request = client.build_submit_request(request)?;
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(self.config.vast_submit_timeout_secs),
            self.http.execute(reqwest_request),
        )
        .await
        .map_err(|_| VastSubmitError::RequestFailed("Vast submit timed out".to_string()))?
        .map_err(|error| VastSubmitError::RequestFailed(error.to_string()))?;
        let response = response
            .error_for_status()
            .map_err(|error| VastSubmitError::RequestFailed(error.to_string()))?;
        response
            .text()
            .await
            .map_err(|error| VastSubmitError::RequestFailed(error.to_string()))
            .and_then(|body| {
                serde_json::from_str::<VastSubmitAcceptedWire>(&body)
                    .map_err(|error| VastSubmitError::RequestFailed(error.to_string()))
            })
            .map(|body| VastSubmitAccepted {
                request_id: body.request_id,
                status: body.status,
                accepted_at: body
                    .accepted_at
                    .and_then(|value| DateTime::parse_from_rfc3339(&value).ok())
                    .map(|value| value.with_timezone(&Utc))
                    .unwrap_or_else(Utc::now),
            })
    }

    async fn submit_vast_rabbitmq(
        &self,
        request: VastSubmitRequest,
    ) -> Result<VastSubmitAccepted, VastSubmitError> {
        use crate::videogen::rabbitmq::{RabbitMqPublishConfig, RabbitMqPublisher};
        let config = RabbitMqPublishConfig {
            amqps_urls: self.config.rabbitmq_amqps_urls.clone(),
            exchange: self.config.rabbitmq_exchange.clone(),
            routing_key: self.config.rabbitmq_routing_key.clone(),
            connection_name: self.config.rabbitmq_connection_name.clone(),
            publish_timeout_secs: self.config.rabbitmq_publish_timeout_secs,
            tls_ca_cert_pem_b64: self.config.rabbitmq_tls_ca_cert_pem_b64.clone(),
        };
        RabbitMqPublisher::new(config)
            .publish(request)
            .await
            .map_err(|e| {
                tracing::error!(cause = %e, "rabbitmq publish failed");
                VastSubmitError::RequestFailed(e.to_string())
            })
    }
}

#[derive(Deserialize)]
struct VastSubmitAcceptedWire {
    request_id: String,
    status: String,
    accepted_at: Option<String>,
}

#[derive(Deserialize)]
struct UploadServiceResponse<T> {
    success: bool,
    data: Option<T>,
    error_message: Option<String>,
}

#[derive(Deserialize)]
struct UploadServiceData {
    video_id: Option<String>,
    upload_url: Option<String>,
}

/// Build the LTX-2.3 two-pass ComfyUI workflow. Ported from off-chain-agent.
/// Pass 1: half-res (640x360) with euler_ancestral_cfg_pp.
/// Pass 2: upscale 2x then refine with euler_cfg_pp.
fn build_ltx2_workflow(request: &GenerateRequest, staged_image_url: Option<&str>) -> Value {
    let prompt_text = request.prompt.as_str();
    let is_t2v = staged_image_url.is_none();
    let image_input = staged_image_url.unwrap_or("example.png");
    const MAX_DURATION_SECS: u8 = 15;
    let duration_seconds = request
        .duration_seconds
        .filter(|&d| d <= MAX_DURATION_SECS)
        .unwrap_or(MAX_DURATION_SECS) as u64;

    serde_json::json!({
        "267:236": { "inputs": { "ckpt_name": "ltx-2.3-22b-dev-fp8.safetensors" }, "class_type": "CheckpointLoaderSimple" },
        "267:243": { "inputs": { "text_encoder": "gemma_3_12B_it_fp4_mixed.safetensors", "ckpt_name": "ltx-2.3-22b-dev-fp8.safetensors", "device": "default" }, "class_type": "LTXAVTextEncoderLoader" },
        "267:221": { "inputs": { "ckpt_name": "ltx-2.3-22b-dev-fp8.safetensors" }, "class_type": "LTXVAudioVAELoader" },
        "267:232": { "inputs": { "lora_name": "ltx-2.3-22b-distilled-lora-384.safetensors", "strength_model": 0.5, "model": ["267:236", 0] }, "class_type": "LoraLoaderModelOnly" },
        "267:233": { "inputs": { "model_name": "ltx-2.3-spatial-upscaler-x2-1.1.safetensors" }, "class_type": "LatentUpscaleModelLoader" },
        "267:201": { "inputs": { "value": is_t2v }, "class_type": "PrimitiveBoolean" },
        "267:260": { "inputs": { "value": 25 }, "class_type": "PrimitiveInt" },
        "267:225": { "inputs": { "value": duration_seconds }, "class_type": "PrimitiveInt" },
        "267:257": { "inputs": { "value": 720 }, "class_type": "PrimitiveInt" },
        "267:258": { "inputs": { "value": 1280 }, "class_type": "PrimitiveInt" },
        "267:261": { "inputs": { "expression": "a", "values.a": ["267:260", 0] }, "class_type": "ComfyMathExpression" },
        "267:277": { "inputs": { "expression": "a * b + 1", "values.a": ["267:225", 0], "values.b": ["267:260", 0] }, "class_type": "ComfyMathExpression" },
        "267:256": { "inputs": { "expression": "a/2", "values.a": ["267:257", 0] }, "class_type": "ComfyMathExpression" },
        "267:259": { "inputs": { "expression": "a/2", "values.a": ["267:258", 0] }, "class_type": "ComfyMathExpression" },
        "267:266": { "inputs": { "value": prompt_text }, "class_type": "PrimitiveStringMultiline" },
        "267:240": { "inputs": { "text": ["267:266", 0], "clip": ["267:243", 0] }, "class_type": "CLIPTextEncode" },
        "267:247": { "inputs": { "text": "pc game, console game, video game, cartoon, childish, ugly", "clip": ["267:243", 0] }, "class_type": "CLIPTextEncode" },
        "267:239": { "inputs": { "frame_rate": ["267:261", 0], "positive": ["267:240", 0], "negative": ["267:247", 0] }, "class_type": "LTXVConditioning" },
        "267:214": { "inputs": { "frames_number": ["267:277", 1], "frame_rate": ["267:261", 1], "batch_size": 1, "audio_vae": ["267:221", 0] }, "class_type": "LTXVEmptyLatentAudio" },
        "267:276": { "inputs": { "image": image_input }, "class_type": "LoadImage" },
        "267:238": { "inputs": { "resize_type": "scale dimensions", "resize_type.width": ["267:257", 0], "resize_type.height": ["267:258", 0], "resize_type.crop": "center", "scale_method": "lanczos", "input": ["267:276", 0] }, "class_type": "ResizeImageMaskNode" },
        "267:235": { "inputs": { "longer_edge": 1536, "images": ["267:238", 0] }, "class_type": "ResizeImagesByLongerEdge" },
        "267:248": { "inputs": { "img_compression": 18, "image": ["267:235", 0] }, "class_type": "LTXVPreprocess" },
        "267:228": { "inputs": { "width": ["267:256", 1], "height": ["267:259", 1], "length": ["267:277", 1], "batch_size": 1 }, "class_type": "EmptyLTXVLatentVideo" },
        "267:249": { "inputs": { "strength": 0.7, "bypass": ["267:201", 0], "vae": ["267:236", 2], "image": ["267:248", 0], "latent": ["267:228", 0] }, "class_type": "LTXVImgToVideoInplace" },
        "267:222": { "inputs": { "video_latent": ["267:249", 0], "audio_latent": ["267:214", 0] }, "class_type": "LTXVConcatAVLatent" },
        "267:237": { "inputs": { "noise_seed": 0 }, "class_type": "RandomNoise" },
        "267:209": { "inputs": { "sampler_name": "euler_ancestral_cfg_pp" }, "class_type": "KSamplerSelect" },
        "267:252": { "inputs": { "sigmas": "1.0, 0.99375, 0.9875, 0.98125, 0.975, 0.909375, 0.725, 0.421875, 0.0" }, "class_type": "ManualSigmas" },
        "267:231": { "inputs": { "cfg": 1, "model": ["267:232", 0], "positive": ["267:239", 0], "negative": ["267:239", 1] }, "class_type": "CFGGuider" },
        "267:215": { "inputs": { "noise": ["267:237", 0], "guider": ["267:231", 0], "sampler": ["267:209", 0], "sigmas": ["267:252", 0], "latent_image": ["267:222", 0] }, "class_type": "SamplerCustomAdvanced" },
        "267:217": { "inputs": { "av_latent": ["267:215", 0] }, "class_type": "LTXVSeparateAVLatent" },
        "267:253": { "inputs": { "samples": ["267:217", 0], "upscale_model": ["267:233", 0], "vae": ["267:236", 2] }, "class_type": "LTXVLatentUpsampler" },
        "267:230": { "inputs": { "strength": 1.0, "bypass": ["267:201", 0], "vae": ["267:236", 2], "image": ["267:248", 0], "latent": ["267:253", 0] }, "class_type": "LTXVImgToVideoInplace" },
        "267:229": { "inputs": { "video_latent": ["267:230", 0], "audio_latent": ["267:217", 1] }, "class_type": "LTXVConcatAVLatent" },
        "267:212": { "inputs": { "positive": ["267:239", 0], "negative": ["267:239", 1], "latent": ["267:217", 0] }, "class_type": "LTXVCropGuides" },
        "267:216": { "inputs": { "noise_seed": 0 }, "class_type": "RandomNoise" },
        "267:246": { "inputs": { "sampler_name": "euler_cfg_pp" }, "class_type": "KSamplerSelect" },
        "267:211": { "inputs": { "sigmas": "0.85, 0.7250, 0.4219, 0.0" }, "class_type": "ManualSigmas" },
        "267:213": { "inputs": { "cfg": 1, "model": ["267:232", 0], "positive": ["267:212", 0], "negative": ["267:212", 1] }, "class_type": "CFGGuider" },
        "267:219": { "inputs": { "noise": ["267:216", 0], "guider": ["267:213", 0], "sampler": ["267:246", 0], "sigmas": ["267:211", 0], "latent_image": ["267:229", 0] }, "class_type": "SamplerCustomAdvanced" },
        "267:218": { "inputs": { "av_latent": ["267:219", 0] }, "class_type": "LTXVSeparateAVLatent" },
        "267:251": { "inputs": { "tile_size": 768, "overlap": 64, "temporal_size": 4096, "temporal_overlap": 4, "samples": ["267:218", 0], "vae": ["267:236", 2] }, "class_type": "VAEDecodeTiled" },
        "267:220": { "inputs": { "samples": ["267:218", 1], "audio_vae": ["267:221", 0] }, "class_type": "LTXVAudioVAEDecode" },
        "267:242": { "inputs": { "fps": ["267:261", 0], "images": ["267:251", 0], "audio": ["267:220", 0] }, "class_type": "CreateVideo" },
        "75": { "inputs": { "filename_prefix": "video/ltx2-3", "format": "auto", "codec": "auto", "video-preview": "", "video": ["267:242", 0] }, "class_type": "SaveVideo" }
    })
}

fn image_extension(mime_type: &str) -> &'static str {
    match mime_type {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/png" => "png",
        _ => "png",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::videogen::{
        moderation::{ModerationDecision, ModerationError, ModerationInput, ModerationSubject},
        rate_limiter::{RateLimiterCreateOptions, RateLimiterRequestKey},
        upload_destination::UploadDestination,
        vast::{VastSubmitAccepted, VastSubmitError, VastSubmitRequest},
    };
    use chrono::{DateTime, Utc};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Call {
        Moderate,
        RateLimiterCreate,
        ReserveUpload,
        StageImage,
        WorkflowJson,
        VastSubmit,
        RateLimiterFailed,
        RateLimiterDecrement,
        ReleaseUpload,
    }

    #[derive(Default)]
    struct FakeDeps {
        calls: Arc<Mutex<Vec<Call>>>,
        moderation: Option<ModerationDecision>,
        rate_limit: Option<Result<RateLimiterRequestKey, RateLimiterError>>,
        stage_image: Option<Result<Option<(String, Option<String>)>, ImageStageError>>,
        vast: Option<Result<VastSubmitAccepted, String>>,
    }

    impl FakeDeps {
        fn with_calls() -> Self {
            Self::default()
        }

        fn push(&self, call: Call) {
            self.calls.lock().unwrap().push(call);
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl GenerateDeps for FakeDeps {
        async fn moderate(
            &self,
            input: ModerationInput,
        ) -> Result<ModerationDecision, ModerationError> {
            assert_eq!(input.prompt, "make a sunrise over mountains");
            self.push(Call::Moderate);
            Ok(self.moderation.unwrap_or(ModerationDecision::Safe))
        }

        async fn create_rate_limit(
            &self,
            _request: &GenerateRequest,
            _options: RateLimiterCreateOptions,
        ) -> Result<RateLimiterRequestKey, RateLimiterError> {
            self.push(Call::RateLimiterCreate);
            self.rate_limit.clone().unwrap_or_else(|| Ok(request_key()))
        }

        async fn mark_rate_limit_failed(
            &self,
            _request_key: &RateLimiterRequestKey,
            _reason: &str,
        ) -> Result<(), RateLimiterError> {
            self.push(Call::RateLimiterFailed);
            Ok(())
        }

        async fn decrement_rate_limit(
            &self,
            _request_key: &RateLimiterRequestKey,
            _property: &str,
        ) -> Result<(), RateLimiterError> {
            self.push(Call::RateLimiterDecrement);
            Ok(())
        }

        async fn reserve_upload_destination(
            &self,
            _request: UploadDestinationRequest,
        ) -> Result<UploadDestination, UploadDestinationError> {
            self.push(Call::ReserveUpload);
            Ok(upload_destination())
        }

        async fn release_upload_destination(
            &self,
            _request_key: &RateLimiterRequestKey,
            _destination: &UploadDestination,
        ) -> Result<(), UploadDestinationError> {
            self.push(Call::ReleaseUpload);
            Ok(())
        }

        async fn stage_image(
            &self,
            _image: Option<ImageSource>,
            _user_principal: &str,
            _timeout_secs: u64,
        ) -> Result<Option<(String, Option<String>)>, ImageStageError> {
            self.push(Call::StageImage);
            self.stage_image.clone().unwrap_or_else(|| {
                Ok(Some((
                    "https://vast.example.test/staged/input-image.png".to_string(),
                    None,
                )))
            })
        }

        async fn workflow_json(
            &self,
            _request: &GenerateRequest,
            staged_image_url: Option<&str>,
        ) -> Result<serde_json::Value, WorkflowError> {
            assert_eq!(
                staged_image_url,
                Some("https://vast.example.test/staged/input-image.png")
            );
            self.push(Call::WorkflowJson);
            Ok(json!({ "workflow": "ltx2" }))
        }

        async fn submit_vast(
            &self,
            request: VastSubmitRequest,
        ) -> Result<VastSubmitAccepted, VastSubmitError> {
            assert_eq!(request.request_id.len(), 36);
            assert_eq!(request.workflow_json, json!({ "workflow": "ltx2" }));
            assert_eq!(
                request.input["image_url"],
                "https://vast.example.test/staged/input-image.png"
            );
            assert_eq!(
                request.callback_url,
                "https://prakash.example.test/api/v2/videogen/complete"
            );
            assert_eq!(
                request.upload_url_refresh_url.as_deref(),
                Some("https://prakash.example.test/api/v2/videogen/upload-url/refresh")
            );
            self.push(Call::VastSubmit);
            if let Some(result) = self.vast.clone() {
                return result.map_err(VastSubmitError::RequestFailed);
            }
            Ok(VastSubmitAccepted {
                request_id: request.request_id,
                status: "submitted".to_string(),
                accepted_at: accepted_at(),
            })
        }
    }

    fn dummy_identity_wire() -> DelegatedIdentityWire {
        serde_json::from_str(r#"{"from_key":[1,2,3],"to_secret":{"crv":"secp256k1","d":"AAECBAUGAAECBAUGAAECBAUGAAECBAUGAAECBAUGAAE=","kty":"EC","x":"AAECBAUGAAECBAUGAAECBAUGAAECBAUGAAECBAUGAAE=","y":"AAECBAUGAAECBAUGAAECBAUGAAECBAUGAAECBAUGAAE="},"delegation_chain":[]}"#).unwrap()
    }

    fn request() -> GenerateRequest {
        GenerateRequest {
            user_id: "aaaaa-aa".to_string(),
            identity_principal: "aaaaa-aa".to_string(),
            upload_handling: VideoUploadHandling::ServerDraft,
            delegated_identity_wire: dummy_identity_wire(),
            prompt: "make a sunrise over mountains".to_string(),
            model_id: "ltx2".to_string(),
            token_type: None,
            negative_prompt: None,
            image: Some(ImageSource::Url(
                "https://example.test/input.png".to_string(),
            )),
            aspect_ratio: Some("16:9".to_string()),
            duration_seconds: Some(5),
            resolution: Some("720p".to_string()),
            generate_audio: Some(false),
            seed: Some(42),
            extra_params: Default::default(),
        }
    }

    fn config() -> GenerateConfig {
        GenerateConfig {
            vast_image_stage_timeout_secs: 30,
            callback_url: "https://prakash.example.test/api/v2/videogen/complete".to_string(),
            upload_url_refresh_url: Some(
                "https://prakash.example.test/api/v2/videogen/upload-url/refresh".to_string(),
            ),
        }
    }

    fn request_key() -> RateLimiterRequestKey {
        RateLimiterRequestKey {
            principal: "aaaaa-aa".to_string(),
            counter: 17,
        }
    }

    fn upload_destination() -> UploadDestination {
        UploadDestination {
            video_id: "video-17".to_string(),
            object_key: "generated/video-17.mp4".to_string(),
            upload_url: "https://upload.example.test/video-17".to_string(),
            expires_at: "2026-05-27T12:00:00Z".parse().unwrap(),
            bucket_url: Some("https://bucket.example.test/video-17.mp4".to_string()),
            encrypted_identity: None,
        }
    }

    fn accepted_at() -> DateTime<Utc> {
        "2026-05-27T11:00:00Z".parse().unwrap()
    }

    #[tokio::test]
    async fn delegated_identity_mismatch_returns_unauthorized() {
        let deps = FakeDeps::with_calls();
        let mut request = request();
        request.identity_principal = "bbbbbb-bb".to_string();

        let result = generate_with_dependencies(request, &deps, config()).await;

        assert_eq!(
            result.unwrap_err().status,
            axum::http::StatusCode::UNAUTHORIZED
        );
        assert_eq!(deps.calls(), vec![]);
    }

    #[tokio::test]
    async fn non_server_draft_returns_bad_request_before_moderation_and_rate_limiter() {
        let deps = FakeDeps::with_calls();
        let mut request = request();
        request.upload_handling = VideoUploadHandling::Client;

        let result = generate_with_dependencies(request, &deps, config()).await;

        assert_eq!(
            result.unwrap_err().status,
            axum::http::StatusCode::BAD_REQUEST
        );
        assert_eq!(deps.calls(), vec![]);
    }

    #[tokio::test]
    async fn unsupported_model_returns_bad_request_before_moderation_and_rate_limiter() {
        let deps = FakeDeps::with_calls();
        let mut request = request();
        request.model_id = "wan2_5".to_string();

        let result = generate_with_dependencies(request, &deps, config()).await;

        let err = result.unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(
            err.error,
            VideoGenError::UnsupportedModel("wan2_5".to_string())
        );
        assert_eq!(deps.calls(), vec![]);
    }

    #[tokio::test]
    async fn nsfw_returns_invalid_input_without_rate_limiter() {
        let deps = FakeDeps {
            moderation: Some(ModerationDecision::Unsafe),
            ..FakeDeps::with_calls()
        };

        let result = generate_with_dependencies(request(), &deps, config()).await;

        let err = result.unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
        assert!(matches!(err.error, VideoGenError::InvalidInput(_)));
        assert_eq!(deps.calls(), vec![Call::Moderate]);
    }

    #[tokio::test]
    async fn rate_limiter_rejection_returns_too_many_requests_without_vast() {
        let deps = FakeDeps {
            rate_limit: Some(Err(RateLimiterError::Limited("limit reached".to_string()))),
            ..FakeDeps::with_calls()
        };

        let result = generate_with_dependencies(request(), &deps, config()).await;

        assert_eq!(
            result.unwrap_err().status,
            axum::http::StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(deps.calls(), vec![Call::Moderate, Call::RateLimiterCreate]);
    }

    #[tokio::test]
    async fn image_staging_timeout_marks_rate_limiter_failed_without_vast() {
        let deps = FakeDeps {
            stage_image: Some(Err(ImageStageError::Timeout)),
            ..FakeDeps::with_calls()
        };

        let result = generate_with_dependencies(request(), &deps, config()).await;

        assert_eq!(
            result.unwrap_err().status,
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            deps.calls(),
            vec![
                Call::Moderate,
                Call::RateLimiterCreate,
                Call::StageImage,
                Call::RateLimiterFailed,
                Call::RateLimiterDecrement,
            ]
        );
    }

    #[tokio::test]
    async fn workflow_failure_marks_rate_limiter_failed_without_vast() {
        struct WorkflowFailingDeps(FakeDeps);

        #[async_trait::async_trait]
        impl GenerateDeps for WorkflowFailingDeps {
            async fn moderate(
                &self,
                input: ModerationInput,
            ) -> Result<ModerationDecision, ModerationError> {
                self.0.moderate(input).await
            }

            async fn create_rate_limit(
                &self,
                request: &GenerateRequest,
                options: RateLimiterCreateOptions,
            ) -> Result<RateLimiterRequestKey, RateLimiterError> {
                self.0.create_rate_limit(request, options).await
            }

            async fn mark_rate_limit_failed(
                &self,
                request_key: &RateLimiterRequestKey,
                reason: &str,
            ) -> Result<(), RateLimiterError> {
                self.0.mark_rate_limit_failed(request_key, reason).await
            }

            async fn decrement_rate_limit(
                &self,
                request_key: &RateLimiterRequestKey,
                property: &str,
            ) -> Result<(), RateLimiterError> {
                self.0.decrement_rate_limit(request_key, property).await
            }

            async fn reserve_upload_destination(
                &self,
                request: UploadDestinationRequest,
            ) -> Result<UploadDestination, UploadDestinationError> {
                self.0.reserve_upload_destination(request).await
            }

            async fn release_upload_destination(
                &self,
                request_key: &RateLimiterRequestKey,
                destination: &UploadDestination,
            ) -> Result<(), UploadDestinationError> {
                self.0
                    .release_upload_destination(request_key, destination)
                    .await
            }

            async fn stage_image(
                &self,
                image: Option<ImageSource>,
                user_principal: &str,
                timeout_secs: u64,
            ) -> Result<Option<(String, Option<String>)>, ImageStageError> {
                self.0
                    .stage_image(image, user_principal, timeout_secs)
                    .await
            }

            async fn workflow_json(
                &self,
                _request: &GenerateRequest,
                _staged_image_url: Option<&str>,
            ) -> Result<serde_json::Value, WorkflowError> {
                self.0.push(Call::WorkflowJson);
                Err(WorkflowError::Unavailable("missing workflow".to_string()))
            }

            async fn submit_vast(
                &self,
                request: VastSubmitRequest,
            ) -> Result<VastSubmitAccepted, VastSubmitError> {
                self.0.submit_vast(request).await
            }
        }

        let deps = WorkflowFailingDeps(FakeDeps::with_calls());

        let result = generate_with_dependencies(request(), &deps, config()).await;

        assert_eq!(
            result.unwrap_err().status,
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            deps.0.calls(),
            vec![
                Call::Moderate,
                Call::RateLimiterCreate,
                Call::StageImage,
                Call::WorkflowJson,
                Call::RateLimiterFailed,
                Call::RateLimiterDecrement,
            ]
        );
    }

    #[tokio::test]
    async fn safe_path_submits_to_vast_and_returns_operation_id() {
        let deps = FakeDeps::with_calls();

        let response = generate_with_dependencies(request(), &deps, config())
            .await
            .unwrap();

        assert_eq!(response.operation_id, "aaaaa-aa_17");
        assert_eq!(response.provider, "Ltx2");
        assert_eq!(response.request_key, request_key());
        assert_eq!(
            deps.calls(),
            vec![
                Call::Moderate,
                Call::RateLimiterCreate,
                Call::StageImage,
                Call::WorkflowJson,
                Call::ReserveUpload,
                Call::VastSubmit,
            ]
        );
    }

    #[tokio::test]
    async fn rabbitmq_submit_success_returns_operation_id() {
        let deps = FakeDeps::with_calls();

        let response = generate_with_dependencies(request(), &deps, config())
            .await
            .unwrap();

        assert_eq!(response.operation_id, "aaaaa-aa_17");
        assert!(deps.calls().contains(&Call::VastSubmit));
    }

    #[tokio::test]
    async fn rabbitmq_submit_failure_rolls_back_rate_limiter() {
        let deps = FakeDeps {
            vast: Some(Err("RabbitMQ publish timed out".to_string())),
            ..FakeDeps::with_calls()
        };

        let err = generate_with_dependencies(request(), &deps, config())
            .await
            .unwrap_err();

        assert_eq!(err.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            deps.calls(),
            vec![
                Call::Moderate,
                Call::RateLimiterCreate,
                Call::StageImage,
                Call::WorkflowJson,
                Call::ReserveUpload,
                Call::VastSubmit,
                Call::RateLimiterFailed,
                Call::RateLimiterDecrement,
                Call::ReleaseUpload,
            ]
        );
    }
}
