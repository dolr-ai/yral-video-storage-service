use axum::body::Body;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use candid::Principal;
use reqwest::header::CONTENT_TYPE;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use utoipa::ToSchema;
use yral_canisters_client::user_post_service::{PostDetailsFromFrontendV1, PostStatusFromFrontend};

// AppError variants cover the full upload-route contract.
// Handlers in later tasks (get-upload-url, update-video-metadata, mark-post-as-published)
// reference all of these variants; allow(dead_code) suppresses any clippy warning until
// those handlers land.
#[allow(dead_code)]
#[derive(Error, Debug)]
pub enum AppError {
    #[error("Invalid principal: {0}")]
    InvalidPrincipal(String),

    #[error("Failed to fetch user profile: {0}")]
    UserProfileFetchError(String),

    #[error("User not found")]
    UserNotFound,

    #[error("Storage error: {0}")]
    StorageError(String),

    #[error("Internal server error: {0}")]
    InternalError(String),

    #[error("Agent error: {0}")]
    AgentError(String),

    #[error("Invalid delegated identity: {0}")]
    InvalidDelegatedIdentity(String),

    #[error("Post not found: {0}")]
    PostNotFound(String),

    #[error("Unauthorized: {0}")]
    Unauthorized(String),

    #[error("Canister error: {0}")]
    CanisterError(String),

    #[error("Serialization error: {0}")]
    SerializationError(String),
}

impl From<ic_agent::agent::AgentError> for AppError {
    fn from(error: ic_agent::agent::AgentError) -> Self {
        AppError::AgentError(error.to_string())
    }
}

impl From<candid::error::Error> for AppError {
    fn from(error: candid::error::Error) -> Self {
        // Preserve the upload-service mapping (HTTP 400). This is a port; keep
        // observable behavior identical even though candid errors are broader.
        AppError::InvalidPrincipal(error.to_string())
    }
}

impl From<Box<dyn std::error::Error>> for AppError {
    fn from(error: Box<dyn std::error::Error>) -> Self {
        AppError::InternalError(error.to_string())
    }
}

impl From<candid::types::principal::PrincipalError> for AppError {
    fn from(error: candid::types::principal::PrincipalError) -> Self {
        AppError::InvalidPrincipal(error.to_string())
    }
}

impl From<serde_json::Error> for AppError {
    fn from(error: serde_json::Error) -> Self {
        AppError::SerializationError(error.to_string())
    }
}

// Methods are used by upload handler tasks added in later tasks.
// TODO(handler-tasks): remove #[allow(dead_code)] once get-upload-url handler uses all variants.
#[allow(dead_code)]
impl AppError {
    pub fn status_code(&self) -> StatusCode {
        match self {
            AppError::InvalidPrincipal(_) => StatusCode::BAD_REQUEST,
            AppError::UserProfileFetchError(_) => StatusCode::BAD_REQUEST,
            AppError::UserNotFound => StatusCode::NOT_FOUND,
            AppError::StorageError(_) => StatusCode::SERVICE_UNAVAILABLE,
            AppError::InternalError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::AgentError(_) => StatusCode::BAD_GATEWAY,
            AppError::InvalidDelegatedIdentity(_) => StatusCode::BAD_REQUEST,
            AppError::PostNotFound(_) => StatusCode::NOT_FOUND,
            AppError::Unauthorized(_) => StatusCode::FORBIDDEN,
            AppError::CanisterError(_) => StatusCode::BAD_GATEWAY,
            AppError::SerializationError(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn to_api_response<T>(&self) -> ApiResponse<T> {
        ApiResponse {
            success: false,
            data: None,
            error_message: Some(self.to_string()),
            status_code: self.status_code(),
        }
    }
}

// Used by upload handler tasks added in later tasks.
#[allow(dead_code)]
#[derive(Serialize, Deserialize, ToSchema)]
pub struct EmptyResp {}

// Used by upload handler tasks added in later tasks.
#[allow(dead_code)]
#[derive(Serialize, Deserialize, ToSchema)]
pub struct ApiResponse<T> {
    pub success: bool,
    pub data: Option<T>,
    pub error_message: Option<String>,
    /// Excluded from the JSON body; carried in memory so `IntoResponse` can set the HTTP status.
    #[serde(skip_serializing, skip_deserializing, default = "default_status_code")]
    pub status_code: StatusCode,
}

fn default_status_code() -> StatusCode {
    StatusCode::OK
}

impl<T: Serialize> IntoResponse for ApiResponse<T> {
    fn into_response(self) -> Response {
        let status = self.status_code;
        let body = serde_json::to_string(&self).unwrap_or_else(|e| {
            format!(
                "{{\"success\":false,\"error_message\":\"response serialization failed: {e}\"}}"
            )
        });
        Response::builder()
            .header(CONTENT_TYPE, "application/json")
            .status(status)
            .body(Body::from(body))
            .expect("status code is always valid")
    }
}

impl<T: Serialize> From<Result<T, AppError>> for ApiResponse<T> {
    fn from(result: Result<T, AppError>) -> Self {
        match result {
            Ok(data) => ApiResponse {
                success: true,
                data: Some(data),
                error_message: None,
                status_code: StatusCode::OK,
            },
            Err(e) => e.to_api_response(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestPostDetails {
    pub video_uid: String,
    pub description: String,
    pub hashtags: Vec<String>,
    pub creator_principal: Principal,
    pub id: String,
}

impl From<PostDetailsFromFrontendV1> for RequestPostDetails {
    fn from(value: PostDetailsFromFrontendV1) -> Self {
        // `status` is intentionally dropped: RequestPostDetails carries no status field.
        Self {
            video_uid: value.video_uid,
            description: value.description,
            hashtags: value.hashtags,
            id: value.id,
            creator_principal: value.creator_principal,
        }
    }
}

impl From<RequestPostDetails> for PostDetailsFromFrontendV1 {
    fn from(value: RequestPostDetails) -> Self {
        // INVARIANT: this conversion always yields Draft. The mark-as-published handler
        // must not use this From impl — it should call the canister API directly with
        // PostStatusFromFrontend::Published.
        Self {
            video_uid: value.video_uid,
            description: value.description,
            hashtags: value.hashtags,
            id: value.id,
            creator_principal: value.creator_principal,
            status: PostStatusFromFrontend::Draft,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn err_serializes_error_message_field_and_status() {
        let r: ApiResponse<EmptyResp> = AppError::Unauthorized("nope".into()).to_api_response();
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"error_message\":\"Unauthorized: nope\""));
        assert!(!json.contains("status_code")); // skip_serializing
        assert_eq!(r.status_code, StatusCode::FORBIDDEN);
    }

    #[test]
    fn ok_wraps_data() {
        let r: ApiResponse<u32> = Ok::<_, AppError>(7u32).into();
        assert!(r.success && r.data == Some(7) && r.status_code == StatusCode::OK);
    }
}
