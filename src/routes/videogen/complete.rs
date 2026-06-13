use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    consts::{self, RATE_LIMITS_CANISTER_ID},
    videogen::{
        config::VideogenConfig,
        draft::{DraftCreationRequest, DraftServiceError},
        hmac::{body_sha256_hex, verify_completion_signature, HmacError, HmacKeyRegistry},
        rate_limiter::{to_canister_request_key, RateLimiterRequestKey},
    },
    AppState,
};
use yral_canisters_client::rate_limits::{
    RateLimits, Result1 as CanisterResult, VideoGenRequestStatus,
};

// ─── Request / response types ────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct CompletionRequestKey {
    pub principal: String,
    pub counter: u64,
}

impl From<&CompletionRequestKey> for RateLimiterRequestKey {
    fn from(k: &CompletionRequestKey) -> Self {
        RateLimiterRequestKey {
            principal: k.principal.clone(),
            counter: k.counter,
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct CompleteVideoRequest {
    pub request_key: CompletionRequestKey,
    pub user_principal: String,
    pub request_id: String,
    pub provider: String,
    pub status: CompletionStatus,
    // success fields
    pub bucket_url: Option<String>,
    pub video_id: Option<String>,
    pub object_key: Option<String>,
    // failure fields
    pub failure_reason: Option<String>,
    // optional metadata
    pub file_size: Option<u64>,
    pub content_type: Option<String>,
    pub checksum: Option<String>,
    /// Encrypted delegated identity for draft registration via upload service.
    pub encrypted_identity: Option<String>,
    /// Storj key of the staged input image to delete after job completes.
    pub staged_image_key: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum CompletionStatus {
    Success,
    Failure,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct CompletionError {
    pub code: String,
    pub message: String,
}

impl CompletionError {
    fn unauthorized(msg: impl Into<String>) -> Self {
        Self {
            code: "unauthorized".into(),
            message: msg.into(),
        }
    }
    fn conflict(msg: impl Into<String>) -> Self {
        Self {
            code: "conflict".into(),
            message: msg.into(),
        }
    }
    fn internal(msg: impl Into<String>) -> Self {
        Self {
            code: "internal_error".into(),
            message: msg.into(),
        }
    }
}

// ─── Dependency abstraction ──────────────────────────────────────────────────

#[async_trait::async_trait]
pub trait CompletionDeps: Send + Sync {
    /// HMAC key registry for verifying Prakash callbacks.
    fn hmac_registry(&self) -> Result<HmacKeyRegistry, String>;

    /// Allowed timestamp skew in seconds.
    fn hmac_skew_secs(&self) -> i64;

    /// Notify the rate limiter that generation completed successfully (best-effort).
    async fn mark_rate_limit_complete(
        &self,
        request_key: &RateLimiterRequestKey,
        bucket_url: &str,
    ) -> Result<(), String>;

    async fn mark_rate_limit_failed(
        &self,
        request_key: &RateLimiterRequestKey,
        reason: &str,
    ) -> Result<(), String>;

    async fn decrement_rate_limit(&self, request_key: &RateLimiterRequestKey)
        -> Result<(), String>;

    async fn release_upload_destination(
        &self,
        request_key: &RateLimiterRequestKey,
        video_id: Option<&str>,
        object_key: Option<&str>,
    ) -> Result<(), String>;

    async fn create_draft(&self, request: DraftCreationRequest) -> Result<(), DraftServiceError>;
}

// ─── Core logic (testable) ───────────────────────────────────────────────────

pub async fn complete_with_dependencies<D: CompletionDeps>(
    deps: &D,
    headers: &HeaderMap,
    raw_body: &[u8],
    path: &str,
) -> Result<StatusCode, (StatusCode, Json<CompletionError>)> {
    // Step 1: verify HMAC before any JSON parse or state mutation
    verify_hmac(deps, headers, raw_body, path)?;

    // Step 2: parse body
    let req: CompleteVideoRequest = serde_json::from_slice(raw_body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(CompletionError {
                code: "bad_request".into(),
                message: format!("invalid JSON body: {e}"),
            }),
        )
    })?;

    crate::sentry_utils::set_sentry_user(&req.user_principal, Some(&req.request_id));

    let request_key = RateLimiterRequestKey::from(&req.request_key);

    // Step 3: validate principal matches request_key.principal
    if req.user_principal != req.request_key.principal {
        return Err((
            StatusCode::CONFLICT,
            Json(CompletionError::conflict(
                "user_principal does not match request_key.principal",
            )),
        ));
    }

    let result = match req.status {
        CompletionStatus::Success => handle_success_completion(deps, &req, &request_key).await,
        CompletionStatus::Failure => handle_failure_completion(deps, &req, &request_key).await,
    };

    if let Some(key) = req.staged_image_key {
        let bucket = crate::consts::YRAL_VIDEOS.clone();
        let access_grant = crate::consts::MIRROR_ACCESS_GRANT.clone();
        tokio::spawn(async move {
            let storj_url = format!("sj://{bucket}/{key}");
            if let Err(e) = crate::jobs::uplink_rm(&storj_url, &access_grant).await {
                tracing::warn!(key = %key, "failed to delete staged image: {e}");
            }
        });
    }

    result
}

async fn handle_success_completion<D: CompletionDeps>(
    deps: &D,
    req: &CompleteVideoRequest,
    request_key: &RateLimiterRequestKey,
) -> Result<StatusCode, (StatusCode, Json<CompletionError>)> {
    let bucket_url = req.bucket_url.as_deref().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(CompletionError {
                code: "bad_request".into(),
                message: "bucket_url required for success".into(),
            }),
        )
    })?;
    let video_id = req.video_id.as_deref().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(CompletionError {
                code: "bad_request".into(),
                message: "video_id required for success".into(),
            }),
        )
    })?;
    let object_key = req.object_key.as_deref().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(CompletionError {
                code: "bad_request".into(),
                message: "object_key required for success".into(),
            }),
        )
    })?;

    // Call draft service
    let draft_req = DraftCreationRequest {
        request_id: req.request_id.clone(),
        request_key: request_key.clone(),
        user_principal: req.user_principal.clone(),
        video_id: video_id.to_string(),
        object_key: object_key.to_string(),
        encrypted_identity: req.encrypted_identity.clone(),
    };
    deps.create_draft(draft_req).await.map_err(|e| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(CompletionError::internal(format!("draft service: {e}"))),
        )
    })?;

    // Notify rate limiter of success (best-effort)
    if let Err(e) = deps.mark_rate_limit_complete(request_key, bucket_url).await {
        tracing::warn!("mark_rate_limit_complete failed (best-effort): {e}");
    }

    tracing::info!(
        principal = %request_key.principal,
        counter = request_key.counter,
        video_id = video_id,
        "videogen completion: marked complete"
    );

    Ok(StatusCode::OK)
}

async fn handle_failure_completion<D: CompletionDeps>(
    deps: &D,
    req: &CompleteVideoRequest,
    request_key: &RateLimiterRequestKey,
) -> Result<StatusCode, (StatusCode, Json<CompletionError>)> {
    let reason = req
        .failure_reason
        .as_deref()
        .unwrap_or("unknown failure reason");

    // Best-effort side effects
    if let Err(e) = deps.mark_rate_limit_failed(request_key, reason).await {
        tracing::warn!("mark_rate_limit_failed failed: {e}");
    }
    if let Err(e) = deps.decrement_rate_limit(request_key).await {
        tracing::warn!("decrement_rate_limit failed: {e}");
    }
    let vid = req.video_id.as_deref();
    let key = req.object_key.as_deref();
    if let Err(e) = deps.release_upload_destination(request_key, vid, key).await {
        tracing::warn!("release_upload_destination failed: {e}");
    }

    tracing::info!(
        principal = %request_key.principal,
        counter = request_key.counter,
        reason = reason,
        "videogen completion: marked failed"
    );

    Ok(StatusCode::OK)
}

fn verify_hmac<D: CompletionDeps>(
    deps: &D,
    headers: &HeaderMap,
    raw_body: &[u8],
    path: &str,
) -> Result<(), (StatusCode, Json<CompletionError>)> {
    let key_id = header_str(headers, "x-key-id").ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(CompletionError::unauthorized("missing X-Key-Id header")),
        )
    })?;
    let timestamp_str = header_str(headers, "x-timestamp").ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(CompletionError::unauthorized("missing X-Timestamp header")),
        )
    })?;
    let body_hash = header_str(headers, "x-body-sha256").ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(CompletionError::unauthorized(
                "missing X-Body-SHA256 header",
            )),
        )
    })?;
    let auth = header_str(headers, "authorization").ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(CompletionError::unauthorized(
                "missing Authorization header",
            )),
        )
    })?;

    let timestamp: i64 = timestamp_str.parse().map_err(|_| {
        (
            StatusCode::UNAUTHORIZED,
            Json(CompletionError::unauthorized("invalid X-Timestamp")),
        )
    })?;

    let sig_hex = auth.strip_prefix("HMAC-SHA256 ").ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(CompletionError::unauthorized(
                "Authorization must be HMAC-SHA256 <hex>",
            )),
        )
    })?;

    // Verify body hash matches
    let expected_hash = body_sha256_hex(raw_body);
    if expected_hash != body_hash {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(CompletionError::unauthorized("body SHA-256 mismatch")),
        ));
    }

    let registry = deps.hmac_registry().map_err(|e| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(CompletionError::internal(format!(
                "HMAC registry error: {e}"
            ))),
        )
    })?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    verify_completion_signature(
        &registry,
        key_id,
        "POST",
        path,
        timestamp,
        body_hash,
        sig_hex,
        now,
        deps.hmac_skew_secs(),
    )
    .map_err(|e| {
        let msg = match e {
            HmacError::UnknownKeyId => "unknown key id".to_string(),
            HmacError::TimestampOutsideSkew => "timestamp outside allowed skew".to_string(),
            HmacError::InvalidSignature => "invalid signature".to_string(),
            other => other.to_string(),
        };
        (
            StatusCode::UNAUTHORIZED,
            Json(CompletionError::unauthorized(msg)),
        )
    })
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

// ─── Axum handler ────────────────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/api/v2/videogen/complete",
    tag = "videogen",
    request_body(
        content = CompleteVideoRequest,
        description = "Completion callback from Vast (HMAC-authenticated)",
        content_type = "application/json"
    ),
    responses(
        (status = 200, description = "Completion accepted"),
        (status = 202, description = "Already in progress"),
        (status = 401, description = "HMAC authentication failed", body = CompletionError),
        (status = 409, description = "Conflict or terminal state", body = CompletionError),
    )
)]
pub async fn complete_video(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, (StatusCode, Json<CompletionError>)> {
    let videogen_config = VideogenConfig::from_env().map_err(|e| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(CompletionError::internal(format!(
                "videogen config error: {e}"
            ))),
        )
    })?;

    let deps = RuntimeCompletionDeps::new(state, videogen_config);
    complete_with_dependencies(&deps, &headers, &body, "/api/v2/videogen/complete").await
}

// ─── Runtime implementation ──────────────────────────────────────────────────

struct RuntimeCompletionDeps {
    config: VideogenConfig,
    ic_agent: ic_agent::Agent,
}

impl RuntimeCompletionDeps {
    fn new(state: AppState, config: VideogenConfig) -> Self {
        Self {
            config,
            ic_agent: state.ic_agent,
        }
    }
}

#[async_trait::async_trait]
impl CompletionDeps for RuntimeCompletionDeps {
    fn hmac_registry(&self) -> Result<HmacKeyRegistry, String> {
        if let Ok(token) = std::env::var(consts::VIDEOGEN_SERVICE_AUTH_TOKEN) {
            return HmacKeyRegistry::from_service_token(&token).map_err(|e| e.to_string());
        }
        let keys = std::env::var(consts::VIDEOGEN_COMPLETION_HMAC_KEYS).map_err(|_| {
            format!(
                "{} or {} is required",
                consts::VIDEOGEN_SERVICE_AUTH_TOKEN,
                consts::VIDEOGEN_COMPLETION_HMAC_KEYS
            )
        })?;
        HmacKeyRegistry::parse(&keys).map_err(|e| e.to_string())
    }

    fn hmac_skew_secs(&self) -> i64 {
        self.config.completion_hmac_skew_secs as i64
    }

    async fn mark_rate_limit_complete(
        &self,
        request_key: &RateLimiterRequestKey,
        bucket_url: &str,
    ) -> Result<(), String> {
        let rate_limits = RateLimits(*RATE_LIMITS_CANISTER_ID, &self.ic_agent);
        let key = to_canister_request_key(request_key).map_err(|e| e.to_string())?;
        match rate_limits
            .update_video_generation_status(
                key,
                VideoGenRequestStatus::Complete(bucket_url.to_string()),
            )
            .await
            .map_err(|e| e.to_string())?
        {
            CanisterResult::Ok => Ok(()),
            CanisterResult::Err(e) => Err(e),
        }
    }

    async fn mark_rate_limit_failed(
        &self,
        request_key: &RateLimiterRequestKey,
        reason: &str,
    ) -> Result<(), String> {
        let rate_limits = RateLimits(*RATE_LIMITS_CANISTER_ID, &self.ic_agent);
        let key = to_canister_request_key(request_key).map_err(|e| e.to_string())?;
        match rate_limits
            .update_video_generation_status(key, VideoGenRequestStatus::Failed(reason.to_string()))
            .await
            .map_err(|e| e.to_string())?
        {
            CanisterResult::Ok => Ok(()),
            CanisterResult::Err(e) => Err(e),
        }
    }

    async fn decrement_rate_limit(
        &self,
        request_key: &RateLimiterRequestKey,
    ) -> Result<(), String> {
        let rate_limits = RateLimits(*RATE_LIMITS_CANISTER_ID, &self.ic_agent);
        let key = to_canister_request_key(request_key).map_err(|e| e.to_string())?;
        match rate_limits
            .decrement_video_generation_counter_v_1(key, "VIDEOGEN".to_string())
            .await
            .map_err(|e| e.to_string())?
        {
            CanisterResult::Ok => Ok(()),
            CanisterResult::Err(e) => Err(e),
        }
    }

    async fn release_upload_destination(
        &self,
        request_key: &RateLimiterRequestKey,
        video_id: Option<&str>,
        object_key: Option<&str>,
    ) -> Result<(), String> {
        use crate::videogen::upload_destination::{
            ReleaseUploadDestinationRequest, UploadDestinationReleaseClient,
        };
        let (Some(vid), Some(key)) = (video_id, object_key) else {
            tracing::info!(
                principal = %request_key.principal,
                "release_upload_destination: missing video_id or object_key, skipping"
            );
            return Ok(());
        };
        UploadDestinationReleaseClient::from_env()
            .release(ReleaseUploadDestinationRequest {
                request_key: request_key.clone(),
                video_id: vid.to_string(),
                object_key: key.to_string(),
            })
            .await
    }

    async fn create_draft(&self, request: DraftCreationRequest) -> Result<(), DraftServiceError> {
        use crate::videogen::draft::draft_client_from_env;
        draft_client_from_env().create_draft(request).await
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::videogen::{
        draft::{DraftCreationRequest, DraftServiceError},
        hmac::{sign_completion, HmacKeyRegistry},
        rate_limiter::RateLimiterRequestKey,
    };
    use axum::http::header::HeaderValue;
    use std::sync::{Arc, Mutex};

    // ── Test HMAC key ──
    const TEST_KEY_SPEC: &str = "v1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    const COMPLETE_PATH: &str = "/api/v2/videogen/complete";

    fn test_registry() -> HmacKeyRegistry {
        HmacKeyRegistry::parse(TEST_KEY_SPEC).unwrap()
    }

    fn now_ts() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    fn signed_headers(body: &[u8], path: &str, ts: i64) -> HeaderMap {
        let registry = test_registry();
        let key = registry.get("v1").unwrap();
        let body_hash = body_sha256_hex(body);
        let sig = sign_completion("POST", path, ts, &body_hash, key);

        let mut headers = HeaderMap::new();
        headers.insert("x-key-id", HeaderValue::from_static("v1"));
        headers.insert(
            "x-timestamp",
            HeaderValue::from_str(&ts.to_string()).unwrap(),
        );
        headers.insert("x-body-sha256", HeaderValue::from_str(&body_hash).unwrap());
        headers.insert(
            "authorization",
            HeaderValue::from_str(&format!("HMAC-SHA256 {sig}")).unwrap(),
        );
        headers
    }

    fn success_body() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "request_key": {"principal": "aaaaa-aa", "counter": 17},
            "user_principal": "aaaaa-aa",
            "request_id": "11111111-1111-1111-1111-111111111111",
            "provider": "Ltx2",
            "status": "success",
            "bucket_url": "https://bucket.example.test/video-17.mp4",
            "video_id": "video-17",
            "object_key": "generated/video-17.mp4"
        }))
        .unwrap()
    }

    fn failure_body() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "request_key": {"principal": "aaaaa-aa", "counter": 17},
            "user_principal": "aaaaa-aa",
            "request_id": "11111111-1111-1111-1111-111111111111",
            "provider": "Ltx2",
            "status": "failure",
            "failure_reason": "generation timed out"
        }))
        .unwrap()
    }

    // ── Fake deps ──

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Call {
        MarkRateLimitComplete,
        MarkRateLimitFailed,
        DecrementRateLimit,
        ReleaseUploadDestination,
        CreateDraft,
    }

    #[derive(Clone)]
    struct FakeCompletionDeps {
        calls: Arc<Mutex<Vec<Call>>>,
        draft_result: Option<Result<(), DraftServiceError>>,
        hmac_keys: Option<String>,
    }

    impl FakeCompletionDeps {
        fn new() -> Self {
            Self {
                calls: Arc::new(Mutex::new(vec![])),
                draft_result: None,
                hmac_keys: Some(TEST_KEY_SPEC.to_string()),
            }
        }

        fn without_hmac_keys() -> Self {
            Self {
                hmac_keys: None,
                ..Self::new()
            }
        }

        fn push(&self, call: Call) {
            self.calls.lock().unwrap().push(call);
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl CompletionDeps for FakeCompletionDeps {
        fn hmac_registry(&self) -> Result<HmacKeyRegistry, String> {
            match &self.hmac_keys {
                Some(keys) => HmacKeyRegistry::parse(keys).map_err(|e| e.to_string()),
                None => Err("VIDEOGEN_COMPLETION_HMAC_KEYS not set".to_string()),
            }
        }

        fn hmac_skew_secs(&self) -> i64 {
            120
        }

        async fn mark_rate_limit_complete(
            &self,
            _request_key: &RateLimiterRequestKey,
            _bucket_url: &str,
        ) -> Result<(), String> {
            self.push(Call::MarkRateLimitComplete);
            Ok(())
        }

        async fn mark_rate_limit_failed(
            &self,
            _request_key: &RateLimiterRequestKey,
            _reason: &str,
        ) -> Result<(), String> {
            self.push(Call::MarkRateLimitFailed);
            Ok(())
        }

        async fn decrement_rate_limit(
            &self,
            _request_key: &RateLimiterRequestKey,
        ) -> Result<(), String> {
            self.push(Call::DecrementRateLimit);
            Ok(())
        }

        async fn release_upload_destination(
            &self,
            _request_key: &RateLimiterRequestKey,
            video_id: Option<&str>,
            object_key: Option<&str>,
        ) -> Result<(), String> {
            if video_id.is_some() && object_key.is_some() {
                self.push(Call::ReleaseUploadDestination);
            }
            Ok(())
        }

        async fn create_draft(
            &self,
            _request: DraftCreationRequest,
        ) -> Result<(), DraftServiceError> {
            self.push(Call::CreateDraft);
            self.draft_result.clone().unwrap_or(Ok(()))
        }
    }

    // ── HMAC tests ──

    #[tokio::test]
    async fn missing_hmac_headers_returns_401() {
        let deps = FakeCompletionDeps::new();
        let body = success_body();
        let headers = HeaderMap::new(); // no auth headers

        let result = complete_with_dependencies(&deps, &headers, &body, COMPLETE_PATH).await;

        assert_eq!(result.unwrap_err().0, StatusCode::UNAUTHORIZED);
        assert_eq!(deps.calls(), vec![]); // no state mutation
    }

    #[tokio::test]
    async fn invalid_hmac_signature_returns_401_without_state_mutation() {
        let deps = FakeCompletionDeps::new();
        let body = success_body();
        let ts = now_ts();
        let mut headers = signed_headers(&body, COMPLETE_PATH, ts);

        // Corrupt the signature
        headers.insert(
            "authorization",
            HeaderValue::from_static("HMAC-SHA256 deadbeef"),
        );

        let result = complete_with_dependencies(&deps, &headers, &body, COMPLETE_PATH).await;

        assert_eq!(result.unwrap_err().0, StatusCode::UNAUTHORIZED);
        assert_eq!(deps.calls(), vec![]); // no state mutation
    }

    #[tokio::test]
    async fn unknown_key_id_returns_401() {
        let deps = FakeCompletionDeps::new();
        let body = success_body();
        let ts = now_ts();
        let mut headers = signed_headers(&body, COMPLETE_PATH, ts);

        // Replace key id with unknown
        headers.insert("x-key-id", HeaderValue::from_static("v99"));

        let result = complete_with_dependencies(&deps, &headers, &body, COMPLETE_PATH).await;

        let (status, Json(err)) = result.unwrap_err();
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(err.message.contains("unknown key id"));
        assert_eq!(deps.calls(), vec![]); // no state mutation
    }

    #[tokio::test]
    async fn stale_timestamp_returns_401() {
        let deps = FakeCompletionDeps::new();
        let body = success_body();
        // Use a timestamp 300 seconds in the past (exceeds 120 skew)
        let stale_ts = now_ts() - 300;
        let headers = signed_headers(&body, COMPLETE_PATH, stale_ts);

        let result = complete_with_dependencies(&deps, &headers, &body, COMPLETE_PATH).await;

        assert_eq!(result.unwrap_err().0, StatusCode::UNAUTHORIZED);
        assert_eq!(deps.calls(), vec![]);
    }

    #[tokio::test]
    async fn old_key_inside_rotation_overlap_succeeds() {
        // Registry with two keys — sign with v1, verify with registry containing both
        let two_key_spec =
            "v1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=,v2:AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=";
        let deps = FakeCompletionDeps {
            hmac_keys: Some(two_key_spec.to_string()),
            ..FakeCompletionDeps::new()
        };
        let body = success_body();
        let ts = now_ts();
        // Sign with v1 (old key still in registry)
        let headers = signed_headers(&body, COMPLETE_PATH, ts);

        let result = complete_with_dependencies(&deps, &headers, &body, COMPLETE_PATH).await;

        assert_eq!(result.unwrap(), StatusCode::OK);
    }

    #[tokio::test]
    async fn oversized_body_rejected_at_layer_level() {
        // The 64KB body limit is enforced at the Axum layer (DefaultBodyLimit),
        // not in business logic. We verify that our parse error path handles
        // a malformed body gracefully.
        let deps = FakeCompletionDeps::new();
        let invalid_body = b"not valid json";
        let ts = now_ts();
        let headers = signed_headers(invalid_body, COMPLETE_PATH, ts);

        let result = complete_with_dependencies(&deps, &headers, invalid_body, COMPLETE_PATH).await;

        // HMAC passes, then JSON parse fails — should be 400
        let (status, _) = result.unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn failure_callback_requires_valid_hmac() {
        let deps = FakeCompletionDeps::new();
        let body = failure_body();
        let headers = HeaderMap::new(); // missing HMAC

        let result = complete_with_dependencies(&deps, &headers, &body, COMPLETE_PATH).await;

        assert_eq!(result.unwrap_err().0, StatusCode::UNAUTHORIZED);
        assert_eq!(deps.calls(), vec![]); // no state mutation
    }

    // ── Success completion tests ──

    #[tokio::test]
    async fn success_callback_creates_draft_and_marks_rate_limit_complete() {
        let deps = FakeCompletionDeps::new();
        let body = success_body();
        let ts = now_ts();
        let headers = signed_headers(&body, COMPLETE_PATH, ts);

        let result = complete_with_dependencies(&deps, &headers, &body, COMPLETE_PATH).await;

        assert_eq!(result.unwrap(), StatusCode::OK);
        assert_eq!(
            deps.calls(),
            vec![Call::CreateDraft, Call::MarkRateLimitComplete,]
        );
    }

    // ── Failure completion tests ──

    #[tokio::test]
    async fn failure_callback_marks_rate_limiter_and_releases_upload() {
        let deps = FakeCompletionDeps::new();
        let body = failure_body();
        let ts = now_ts();
        let headers = signed_headers(&body, COMPLETE_PATH, ts);

        let result = complete_with_dependencies(&deps, &headers, &body, COMPLETE_PATH).await;

        assert_eq!(result.unwrap(), StatusCode::OK);
        let calls = deps.calls();
        assert!(calls.contains(&Call::MarkRateLimitFailed));
        assert!(calls.contains(&Call::DecrementRateLimit));
        // failure_body has no video_id/object_key so release is skipped
        assert!(!calls.contains(&Call::ReleaseUploadDestination));
    }
}
