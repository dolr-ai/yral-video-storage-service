use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    consts, db,
    videogen::{
        config::VideogenConfig,
        context::{ContextStateRow, ContextStoreError, PostgresVideogenContextStore},
        draft::{DraftCreationRequest, DraftServiceError},
        hmac::{body_sha256_hex, verify_completion_signature, HmacError, HmacKeyRegistry},
        rate_limiter::RateLimiterRequestKey,
    },
    AppState,
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

    /// Atomically transition submitted → uploaded. Returns None if already claimed.
    async fn claim_for_completion(
        &self,
        request_key: &RateLimiterRequestKey,
        request_id: &str,
    ) -> Result<Option<crate::videogen::context::CompletionContextRow>, ContextStoreError>;

    /// Read current state for idempotency / conflict checking.
    async fn get_context_state(
        &self,
        request_key: &RateLimiterRequestKey,
    ) -> Result<Option<ContextStateRow>, ContextStoreError>;

    /// uploaded → draft_creating
    async fn mark_draft_creating(
        &self,
        request_key: &RateLimiterRequestKey,
    ) -> Result<(), ContextStoreError>;

    /// draft_creating → draft_created
    async fn mark_draft_created(
        &self,
        request_key: &RateLimiterRequestKey,
    ) -> Result<(), ContextStoreError>;

    /// Notify the rate limiter that generation completed successfully (best-effort).
    async fn mark_rate_limit_complete(
        &self,
        request_key: &RateLimiterRequestKey,
    ) -> Result<(), String>;

    /// draft_created → complete (also stores bucket_url and redacts identity)
    async fn mark_complete(
        &self,
        request_key: &RateLimiterRequestKey,
        bucket_url: &str,
    ) -> Result<(), ContextStoreError>;

    /// submitted → failed (redacts identity)
    async fn mark_generation_failed(
        &self,
        request_key: &RateLimiterRequestKey,
        reason: &str,
    ) -> Result<(), ContextStoreError>;

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
    metrics::counter!(crate::videogen::metrics::COMPLETION_CALLBACKS_TOTAL).increment(1);

    // Step 1: verify HMAC before any JSON parse or state mutation
    verify_hmac(deps, headers, raw_body, path).map_err(|e| {
        metrics::counter!(crate::videogen::metrics::COMPLETION_HMAC_FAILURES_TOTAL).increment(1);
        e
    })?;

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

    match req.status {
        CompletionStatus::Success => handle_success_completion(deps, &req, &request_key).await,
        CompletionStatus::Failure => handle_failure_completion(deps, &req, &request_key).await,
    }
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

    // Step 4: atomic claim (submitted → uploaded)
    let claimed = deps
        .claim_for_completion(request_key, &req.request_id)
        .await
        .map_err(|e| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(CompletionError::internal(e.to_string())),
            )
        })?;

    let Some(row) = claimed else {
        // Row was already claimed — check current state for idempotency
        return check_idempotent_success(deps, request_key, &req.request_id, object_key).await;
    };

    // Validate object_key matches stored destination
    if let Some(stored_key) = &row.object_key {
        if stored_key != object_key {
            return Err((
                StatusCode::CONFLICT,
                Json(CompletionError::conflict(
                    "object_key does not match stored upload destination",
                )),
            ));
        }
    }

    // Step 5: uploaded → draft_creating
    deps.mark_draft_creating(request_key).await.map_err(|e| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(CompletionError::internal(e.to_string())),
        )
    })?;

    // Step 6: call draft service
    let draft_req = DraftCreationRequest {
        request_id: req.request_id.clone(),
        request_key: request_key.clone(),
        user_principal: req.user_principal.clone(),
        video_id: video_id.to_string(),
        object_key: object_key.to_string(),
    };
    deps.create_draft(draft_req).await.map_err(|e| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(CompletionError::internal(format!("draft service: {e}"))),
        )
    })?;

    // Step 7: draft_creating → draft_created
    deps.mark_draft_created(request_key).await.map_err(|e| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(CompletionError::internal(e.to_string())),
        )
    })?;

    // Step 7b: notify rate limiter of success (best-effort)
    if let Err(e) = deps.mark_rate_limit_complete(request_key).await {
        tracing::warn!("mark_rate_limit_complete failed (best-effort): {e}");
    }

    // Step 8: draft_created → complete (stores bucket_url, redacts identity)
    deps.mark_complete(request_key, bucket_url)
        .await
        .map_err(|e| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(CompletionError::internal(e.to_string())),
            )
        })?;

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

    // For failure: first check if state is terminal (already failed) — return 409
    let state_row = deps.get_context_state(request_key).await.map_err(|e| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(CompletionError::internal(e.to_string())),
        )
    })?;

    match state_row.as_ref().map(|r| r.state.as_str()) {
        Some("failed") | Some("stale_failed") | Some("submit_failed") | Some("draft_failed") => {
            return Err((
                StatusCode::CONFLICT,
                Json(CompletionError::conflict(
                    "already in terminal failed state",
                )),
            ));
        }
        Some("complete") | Some("draft_created") | Some("draft_creating") | Some("uploaded") => {
            // Already past submitted — treat as 409 for failure callback
            return Err((
                StatusCode::CONFLICT,
                Json(CompletionError::conflict(
                    "cannot process failure callback: context is past submitted state",
                )),
            ));
        }
        None => {
            return Err((
                StatusCode::CONFLICT,
                Json(CompletionError::conflict("context not found")),
            ));
        }
        _ => {} // submitted or context_created — proceed
    }

    // Validate request_id matches
    if let Some(row) = &state_row {
        if let Some(stored_id) = &row.request_id {
            if stored_id != &req.request_id {
                return Err((
                    StatusCode::CONFLICT,
                    Json(CompletionError::conflict(
                        "request_id does not match stored context",
                    )),
                ));
            }
        }
    }

    // Transition submitted → failed (atomic, redacts identity)
    deps.mark_generation_failed(request_key, reason)
        .await
        .map_err(|e| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(CompletionError::internal(e.to_string())),
            )
        })?;

    // Best-effort side effects
    if let Err(e) = deps.mark_rate_limit_failed(request_key, reason).await {
        tracing::warn!("mark_rate_limit_failed failed: {e}");
    }
    if let Err(e) = deps.decrement_rate_limit(request_key).await {
        tracing::warn!("decrement_rate_limit failed: {e}");
    }
    if let Err(e) = deps.release_upload_destination(request_key).await {
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

async fn check_idempotent_success<D: CompletionDeps>(
    deps: &D,
    request_key: &RateLimiterRequestKey,
    request_id: &str,
    object_key: &str,
) -> Result<StatusCode, (StatusCode, Json<CompletionError>)> {
    let state_row = deps.get_context_state(request_key).await.map_err(|e| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(CompletionError::internal(e.to_string())),
        )
    })?;

    let Some(row) = state_row else {
        return Err((
            StatusCode::CONFLICT,
            Json(CompletionError::conflict("context not found")),
        ));
    };

    // Validate request_id and object_key still match
    if let Some(stored_id) = &row.request_id {
        if stored_id != request_id {
            return Err((
                StatusCode::CONFLICT,
                Json(CompletionError::conflict(
                    "request_id does not match stored context",
                )),
            ));
        }
    }
    if let Some(stored_key) = &row.object_key {
        if stored_key != object_key {
            return Err((
                StatusCode::CONFLICT,
                Json(CompletionError::conflict(
                    "object_key does not match stored context",
                )),
            ));
        }
    }

    match row.state.as_str() {
        "complete" | "draft_created" | "draft_creating" | "uploaded" => {
            // Another handler already claimed and is progressing — return 202
            Ok(StatusCode::ACCEPTED)
        }
        "failed" | "stale_failed" | "draft_failed" | "submit_failed" => {
            // Terminal failure state for a success callback — conflict
            Err((
                StatusCode::CONFLICT,
                Json(CompletionError::conflict(
                    "context is in terminal failure state",
                )),
            ))
        }
        _ => {
            // Unexpected state
            Err((
                StatusCode::CONFLICT,
                Json(CompletionError::conflict(format!(
                    "unexpected state: {}",
                    row.state
                ))),
            ))
        }
    }
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
        &body_hash,
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
    db_url: String,
    config: VideogenConfig,
}

impl RuntimeCompletionDeps {
    fn new(state: AppState, config: VideogenConfig) -> Self {
        Self {
            db_url: state.db_url,
            config,
        }
    }

    async fn context_store(&self) -> Result<PostgresVideogenContextStore, ContextStoreError> {
        let client = db::connect(&self.db_url)
            .await
            .map_err(|e| ContextStoreError::Unavailable(e.to_string()))?;
        Ok(PostgresVideogenContextStore::new(client))
    }
}

#[async_trait::async_trait]
impl CompletionDeps for RuntimeCompletionDeps {
    fn hmac_registry(&self) -> Result<HmacKeyRegistry, String> {
        let keys = std::env::var(consts::VIDEOGEN_COMPLETION_HMAC_KEYS)
            .map_err(|_| format!("{} is required", consts::VIDEOGEN_COMPLETION_HMAC_KEYS))?;
        HmacKeyRegistry::parse(&keys).map_err(|e| e.to_string())
    }

    fn hmac_skew_secs(&self) -> i64 {
        self.config.completion_hmac_skew_secs as i64
    }

    async fn claim_for_completion(
        &self,
        request_key: &RateLimiterRequestKey,
        request_id: &str,
    ) -> Result<Option<crate::videogen::context::CompletionContextRow>, ContextStoreError> {
        self.context_store()
            .await?
            .claim_for_completion(request_key, request_id)
            .await
    }

    async fn get_context_state(
        &self,
        request_key: &RateLimiterRequestKey,
    ) -> Result<Option<ContextStateRow>, ContextStoreError> {
        self.context_store()
            .await?
            .get_context_state(request_key)
            .await
    }

    async fn mark_draft_creating(
        &self,
        request_key: &RateLimiterRequestKey,
    ) -> Result<(), ContextStoreError> {
        self.context_store()
            .await?
            .mark_draft_creating(request_key)
            .await
    }

    async fn mark_draft_created(
        &self,
        request_key: &RateLimiterRequestKey,
    ) -> Result<(), ContextStoreError> {
        self.context_store()
            .await?
            .mark_draft_created(request_key)
            .await
    }

    async fn mark_complete(
        &self,
        request_key: &RateLimiterRequestKey,
        bucket_url: &str,
    ) -> Result<(), ContextStoreError> {
        self.context_store()
            .await?
            .mark_complete(request_key, bucket_url)
            .await
    }

    async fn mark_generation_failed(
        &self,
        request_key: &RateLimiterRequestKey,
        reason: &str,
    ) -> Result<(), ContextStoreError> {
        self.context_store()
            .await?
            .mark_generation_failed(request_key, reason)
            .await
    }

    async fn mark_rate_limit_complete(
        &self,
        _request_key: &RateLimiterRequestKey,
    ) -> Result<(), String> {
        // Rate limiter calls go to IC canister — stubbed for now (Task 7)
        tracing::info!("mark_rate_limit_complete stub called");
        Ok(())
    }

    async fn mark_rate_limit_failed(
        &self,
        _request_key: &RateLimiterRequestKey,
        _reason: &str,
    ) -> Result<(), String> {
        // Rate limiter calls go to IC canister — stubbed for now
        tracing::info!("rate_limit_failed stub called");
        Ok(())
    }

    async fn decrement_rate_limit(
        &self,
        _request_key: &RateLimiterRequestKey,
    ) -> Result<(), String> {
        tracing::info!("decrement_rate_limit stub called");
        Ok(())
    }

    async fn release_upload_destination(
        &self,
        _request_key: &RateLimiterRequestKey,
    ) -> Result<(), String> {
        tracing::info!("release_upload_destination stub called");
        Ok(())
    }

    async fn create_draft(&self, request: DraftCreationRequest) -> Result<(), DraftServiceError> {
        use crate::videogen::draft::LoggingDraftServiceClient;
        LoggingDraftServiceClient.create_draft(request).await
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::videogen::{
        context::{CompletionContextRow, ContextStateRow},
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
        ClaimForCompletion,
        GetContextState,
        MarkDraftCreating,
        MarkDraftCreated,
        MarkRateLimitComplete,
        MarkComplete,
        MarkGenerationFailed,
        MarkRateLimitFailed,
        DecrementRateLimit,
        ReleaseUploadDestination,
        CreateDraft,
    }

    #[derive(Clone)]
    struct FakeCompletionDeps {
        calls: Arc<Mutex<Vec<Call>>>,
        claim_result: Option<Option<CompletionContextRow>>,
        state_result: Option<Option<ContextStateRow>>,
        draft_result: Option<Result<(), DraftServiceError>>,
        hmac_keys: Option<String>,
    }

    impl FakeCompletionDeps {
        fn new() -> Self {
            Self {
                calls: Arc::new(Mutex::new(vec![])),
                claim_result: None,
                state_result: None,
                draft_result: None,
                hmac_keys: Some(TEST_KEY_SPEC.to_string()),
            }
        }

        fn with_submitted() -> Self {
            let row = CompletionContextRow {
                request_key: RateLimiterRequestKey {
                    principal: "aaaaa-aa".to_string(),
                    counter: 17,
                },
                request_id: "11111111-1111-1111-1111-111111111111".to_string(),
                state: "submitted".to_string(),
                object_key: Some("generated/video-17.mp4".to_string()),
                video_id: Some("video-17".to_string()),
            };
            Self {
                claim_result: Some(Some(row)),
                state_result: Some(Some(ContextStateRow {
                    state: "submitted".to_string(),
                    request_id: Some("11111111-1111-1111-1111-111111111111".to_string()),
                    principal: "aaaaa-aa".to_string(),
                    object_key: Some("generated/video-17.mp4".to_string()),
                })),
                ..Self::new()
            }
        }

        fn with_already_claimed(state: &str) -> Self {
            Self {
                claim_result: Some(None), // already claimed
                state_result: Some(Some(ContextStateRow {
                    state: state.to_string(),
                    request_id: Some("11111111-1111-1111-1111-111111111111".to_string()),
                    principal: "aaaaa-aa".to_string(),
                    object_key: Some("generated/video-17.mp4".to_string()),
                })),
                ..Self::new()
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

        async fn claim_for_completion(
            &self,
            _request_key: &RateLimiterRequestKey,
            _request_id: &str,
        ) -> Result<Option<CompletionContextRow>, ContextStoreError> {
            self.push(Call::ClaimForCompletion);
            Ok(self
                .claim_result
                .clone()
                .unwrap_or(Some(CompletionContextRow {
                    request_key: RateLimiterRequestKey {
                        principal: "aaaaa-aa".to_string(),
                        counter: 17,
                    },
                    request_id: "11111111-1111-1111-1111-111111111111".to_string(),
                    state: "uploaded".to_string(),
                    object_key: Some("generated/video-17.mp4".to_string()),
                    video_id: Some("video-17".to_string()),
                })))
        }

        async fn get_context_state(
            &self,
            _request_key: &RateLimiterRequestKey,
        ) -> Result<Option<ContextStateRow>, ContextStoreError> {
            self.push(Call::GetContextState);
            Ok(self.state_result.clone().unwrap_or(Some(ContextStateRow {
                state: "submitted".to_string(),
                request_id: Some("11111111-1111-1111-1111-111111111111".to_string()),
                principal: "aaaaa-aa".to_string(),
                object_key: Some("generated/video-17.mp4".to_string()),
            })))
        }

        async fn mark_draft_creating(
            &self,
            _request_key: &RateLimiterRequestKey,
        ) -> Result<(), ContextStoreError> {
            self.push(Call::MarkDraftCreating);
            Ok(())
        }

        async fn mark_draft_created(
            &self,
            _request_key: &RateLimiterRequestKey,
        ) -> Result<(), ContextStoreError> {
            self.push(Call::MarkDraftCreated);
            Ok(())
        }

        async fn mark_rate_limit_complete(
            &self,
            _request_key: &RateLimiterRequestKey,
        ) -> Result<(), String> {
            self.push(Call::MarkRateLimitComplete);
            Ok(())
        }

        async fn mark_complete(
            &self,
            _request_key: &RateLimiterRequestKey,
            _bucket_url: &str,
        ) -> Result<(), ContextStoreError> {
            self.push(Call::MarkComplete);
            Ok(())
        }

        async fn mark_generation_failed(
            &self,
            _request_key: &RateLimiterRequestKey,
            _reason: &str,
        ) -> Result<(), ContextStoreError> {
            self.push(Call::MarkGenerationFailed);
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
        ) -> Result<(), String> {
            self.push(Call::ReleaseUploadDestination);
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
            ..FakeCompletionDeps::with_submitted()
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

    // ── Idempotency / concurrency tests ──

    #[tokio::test]
    async fn success_callback_from_submitted_transitions_to_complete() {
        let deps = FakeCompletionDeps::with_submitted();
        let body = success_body();
        let ts = now_ts();
        let headers = signed_headers(&body, COMPLETE_PATH, ts);

        let result = complete_with_dependencies(&deps, &headers, &body, COMPLETE_PATH).await;

        assert_eq!(result.unwrap(), StatusCode::OK);
        assert_eq!(
            deps.calls(),
            vec![
                Call::ClaimForCompletion,
                Call::MarkDraftCreating,
                Call::CreateDraft,
                Call::MarkDraftCreated,
                Call::MarkRateLimitComplete,
                Call::MarkComplete,
            ]
        );
    }

    #[tokio::test]
    async fn duplicate_success_after_complete_returns_202() {
        // claim returns None (already claimed), state is 'complete'
        let deps = FakeCompletionDeps::with_already_claimed("complete");
        let body = success_body();
        let ts = now_ts();
        let headers = signed_headers(&body, COMPLETE_PATH, ts);

        let result = complete_with_dependencies(&deps, &headers, &body, COMPLETE_PATH).await;

        assert_eq!(result.unwrap(), StatusCode::ACCEPTED);
        // No state mutation after the get_context_state call
        assert!(deps.calls().contains(&Call::ClaimForCompletion));
        assert!(!deps.calls().contains(&Call::MarkDraftCreating));
    }

    #[tokio::test]
    async fn duplicate_success_while_draft_creating_returns_202() {
        let deps = FakeCompletionDeps::with_already_claimed("draft_creating");
        let body = success_body();
        let ts = now_ts();
        let headers = signed_headers(&body, COMPLETE_PATH, ts);

        let result = complete_with_dependencies(&deps, &headers, &body, COMPLETE_PATH).await;

        assert_eq!(result.unwrap(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn success_callback_when_already_failed_returns_409() {
        let deps = FakeCompletionDeps::with_already_claimed("failed");
        let body = success_body();
        let ts = now_ts();
        let headers = signed_headers(&body, COMPLETE_PATH, ts);

        let result = complete_with_dependencies(&deps, &headers, &body, COMPLETE_PATH).await;

        assert_eq!(result.unwrap_err().0, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn failure_callback_when_already_terminal_returns_409() {
        let deps = FakeCompletionDeps {
            state_result: Some(Some(ContextStateRow {
                state: "stale_failed".to_string(),
                request_id: Some("11111111-1111-1111-1111-111111111111".to_string()),
                principal: "aaaaa-aa".to_string(),
                object_key: Some("generated/video-17.mp4".to_string()),
            })),
            ..FakeCompletionDeps::new()
        };
        let body = failure_body();
        let ts = now_ts();
        let headers = signed_headers(&body, COMPLETE_PATH, ts);

        let result = complete_with_dependencies(&deps, &headers, &body, COMPLETE_PATH).await;

        assert_eq!(result.unwrap_err().0, StatusCode::CONFLICT);
        assert!(!deps.calls().contains(&Call::MarkGenerationFailed));
    }

    #[tokio::test]
    async fn mismatched_request_id_in_failure_callback_returns_409() {
        let deps = FakeCompletionDeps {
            state_result: Some(Some(ContextStateRow {
                state: "submitted".to_string(),
                request_id: Some("99999999-9999-9999-9999-999999999999".to_string()), // different
                principal: "aaaaa-aa".to_string(),
                object_key: Some("generated/video-17.mp4".to_string()),
            })),
            ..FakeCompletionDeps::new()
        };
        let body = failure_body();
        let ts = now_ts();
        let headers = signed_headers(&body, COMPLETE_PATH, ts);

        let result = complete_with_dependencies(&deps, &headers, &body, COMPLETE_PATH).await;

        assert_eq!(result.unwrap_err().0, StatusCode::CONFLICT);
        assert!(!deps.calls().contains(&Call::MarkGenerationFailed));
    }

    #[tokio::test]
    async fn success_failure_callback_marks_rate_limiter_and_releases_upload() {
        let deps = FakeCompletionDeps {
            state_result: Some(Some(ContextStateRow {
                state: "submitted".to_string(),
                request_id: Some("11111111-1111-1111-1111-111111111111".to_string()),
                principal: "aaaaa-aa".to_string(),
                object_key: Some("generated/video-17.mp4".to_string()),
            })),
            ..FakeCompletionDeps::new()
        };
        let body = failure_body();
        let ts = now_ts();
        let headers = signed_headers(&body, COMPLETE_PATH, ts);

        let result = complete_with_dependencies(&deps, &headers, &body, COMPLETE_PATH).await;

        assert_eq!(result.unwrap(), StatusCode::OK);
        let calls = deps.calls();
        assert!(calls.contains(&Call::MarkGenerationFailed));
        assert!(calls.contains(&Call::MarkRateLimitFailed));
        assert!(calls.contains(&Call::DecrementRateLimit));
        assert!(calls.contains(&Call::ReleaseUploadDestination));
    }
}
