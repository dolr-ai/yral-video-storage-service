use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    consts,
    videogen::{
        config::VideogenConfig,
        hmac::{body_sha256_hex, verify_completion_signature, HmacError, HmacKeyRegistry},
        rate_limiter::RateLimiterRequestKey,
        upload_destination::{serialize_datetime_utc, UploadDestination},
    },
    AppState,
};

const REFRESH_PATH: &str = "/api/v2/videogen/upload-url/refresh";

// ─── Request / response types ─────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct UploadRefreshRequest {
    pub request_key: RefreshRequestKey,
    pub user_principal: String,
    pub request_id: String,
    pub video_id: String,
    pub object_key: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct RefreshRequestKey {
    pub principal: String,
    pub counter: u64,
}

impl From<&RefreshRequestKey> for RateLimiterRequestKey {
    fn from(k: &RefreshRequestKey) -> Self {
        RateLimiterRequestKey {
            principal: k.principal.clone(),
            counter: k.counter,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct UploadRefreshResponse {
    pub video_id: String,
    pub object_key: String,
    pub upload_url: String,
    #[serde(serialize_with = "serialize_datetime_utc")]
    #[schema(value_type = String, format = DateTime)]
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct RefreshError {
    pub code: String,
    pub message: String,
}

impl RefreshError {
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

// ─── Dependency abstraction ───────────────────────────────────────────────────

#[async_trait::async_trait]
pub trait UploadRefreshDeps: Send + Sync {
    fn hmac_registry(&self) -> Result<HmacKeyRegistry, String>;
    fn hmac_skew_secs(&self) -> i64;

    async fn generate_fresh_upload_url(
        &self,
        request_key: &RateLimiterRequestKey,
        video_id: &str,
        object_key: &str,
    ) -> Result<UploadDestination, String>;
}

// ─── Core logic (testable) ────────────────────────────────────────────────────

pub async fn refresh_with_dependencies<D: UploadRefreshDeps>(
    deps: &D,
    headers: &HeaderMap,
    raw_body: &[u8],
) -> Result<Json<UploadRefreshResponse>, (StatusCode, Json<RefreshError>)> {
    // Step 1: verify HMAC before any parsing
    verify_hmac(deps, headers, raw_body)?;

    // Step 2: parse body
    let req: UploadRefreshRequest = serde_json::from_slice(raw_body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(RefreshError {
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
            Json(RefreshError::conflict(
                "user_principal does not match request_key.principal",
            )),
        ));
    }

    // Step 4: generate fresh URL
    let destination = deps
        .generate_fresh_upload_url(&request_key, &req.video_id, &req.object_key)
        .await
        .map_err(|e| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(RefreshError::internal(e)),
            )
        })?;

    Ok(Json(UploadRefreshResponse {
        video_id: destination.video_id,
        object_key: destination.object_key,
        upload_url: destination.upload_url,
        expires_at: destination.expires_at,
    }))
}

fn verify_hmac<D: UploadRefreshDeps>(
    deps: &D,
    headers: &HeaderMap,
    raw_body: &[u8],
) -> Result<(), (StatusCode, Json<RefreshError>)> {
    let key_id = header_str(headers, "x-key-id").ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(RefreshError::unauthorized("missing X-Key-Id header")),
        )
    })?;
    let timestamp_str = header_str(headers, "x-timestamp").ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(RefreshError::unauthorized("missing X-Timestamp header")),
        )
    })?;
    let body_hash = header_str(headers, "x-body-sha256").ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(RefreshError::unauthorized("missing X-Body-SHA256 header")),
        )
    })?;
    let auth = header_str(headers, "authorization").ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(RefreshError::unauthorized("missing Authorization header")),
        )
    })?;

    let timestamp: i64 = timestamp_str.parse().map_err(|_| {
        (
            StatusCode::UNAUTHORIZED,
            Json(RefreshError::unauthorized("invalid X-Timestamp")),
        )
    })?;

    let sig_hex = auth.strip_prefix("HMAC-SHA256 ").ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(RefreshError::unauthorized(
                "Authorization must be HMAC-SHA256 <hex>",
            )),
        )
    })?;

    let expected_hash = body_sha256_hex(raw_body);
    if expected_hash != body_hash {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(RefreshError::unauthorized("body SHA-256 mismatch")),
        ));
    }

    let registry = deps.hmac_registry().map_err(|e| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(RefreshError::internal(format!("HMAC registry error: {e}"))),
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
        REFRESH_PATH,
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
            Json(RefreshError::unauthorized(msg)),
        )
    })
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

// ─── Axum handler ─────────────────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/api/v2/videogen/upload-url/refresh",
    tag = "videogen",
    request_body(
        content = UploadRefreshRequest,
        description = "Request a fresh upload URL (HMAC-authenticated)",
        content_type = "application/json"
    ),
    responses(
        (status = 200, description = "Fresh upload URL", body = UploadRefreshResponse),
        (status = 401, description = "HMAC authentication failed", body = RefreshError),
        (status = 409, description = "Conflict or unknown request", body = RefreshError),
    )
)]
pub async fn refresh_upload_url(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<UploadRefreshResponse>, (StatusCode, Json<RefreshError>)> {
    let videogen_config = VideogenConfig::from_env().map_err(|e| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(RefreshError::internal(format!(
                "videogen config error: {e}"
            ))),
        )
    })?;

    let deps = RuntimeUploadRefreshDeps::new(state, videogen_config);
    refresh_with_dependencies(&deps, &headers, &body).await
}

// ─── Runtime implementation ───────────────────────────────────────────────────

struct RuntimeUploadRefreshDeps {
    config: VideogenConfig,
    http: reqwest::Client,
}

impl RuntimeUploadRefreshDeps {
    fn new(_state: AppState, config: VideogenConfig) -> Self {
        Self {
            config,
            http: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl UploadRefreshDeps for RuntimeUploadRefreshDeps {
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

    async fn generate_fresh_upload_url(
        &self,
        _request_key: &RateLimiterRequestKey,
        video_id: &str,
        _object_key: &str,
    ) -> Result<UploadDestination, String> {
        use chrono::Duration;
        use reqwest::header::CONTENT_TYPE;
        use serde_json::json;

        let base_url = std::env::var(crate::consts::VIDEOGEN_UPLOAD_SERVICE_URL_ENV)
            .or_else(|_| std::env::var(crate::consts::VIDEOGEN_LEGACY_UPLOAD_SERVICE_URL_ENV))
            .map_err(|_| {
                format!(
                    "{} or {} is required",
                    crate::consts::VIDEOGEN_UPLOAD_SERVICE_URL_ENV,
                    crate::consts::VIDEOGEN_LEGACY_UPLOAD_SERVICE_URL_ENV
                )
            })?;
        let url = format!("{}/get-upload-url", base_url.trim_end_matches('/'));

        let body =
            serde_json::to_vec(&json!({ "video_id": video_id })).map_err(|e| e.to_string())?;

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
            .map_err(|e| e.to_string())?;

        if !response.status().is_success() {
            return Err(format!("upload service returned {}", response.status()));
        }

        #[derive(serde::Deserialize)]
        struct Resp {
            success: bool,
            data: Option<Data>,
            error_message: Option<String>,
        }
        #[derive(serde::Deserialize)]
        struct Data {
            upload_url: Option<String>,
        }

        let body = response.text().await.map_err(|e| e.to_string())?;
        let resp: Resp = serde_json::from_str(&body).map_err(|e| e.to_string())?;
        if !resp.success {
            return Err(resp
                .error_message
                .unwrap_or_else(|| "upload service did not return success".to_string()));
        }
        let upload_url = resp
            .data
            .and_then(|d| d.upload_url)
            .ok_or_else(|| "upload service response missing upload_url".to_string())?;

        Ok(UploadDestination {
            video_id: video_id.to_string(),
            object_key: _object_key.to_string(),
            upload_url,
            expires_at: Utc::now() + Duration::seconds(self.config.upload_url_ttl_secs as i64),
        })
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::videogen::{
        hmac::{sign_completion, HmacKeyRegistry},
        rate_limiter::RateLimiterRequestKey,
    };
    use axum::http::header::HeaderValue;
    use chrono::Utc;
    use std::sync::{Arc, Mutex};

    const TEST_KEY_SPEC: &str = "v1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

    fn test_registry() -> HmacKeyRegistry {
        HmacKeyRegistry::parse(TEST_KEY_SPEC).unwrap()
    }

    fn now_ts() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    fn signed_headers(body: &[u8], ts: i64) -> HeaderMap {
        let registry = test_registry();
        let key = registry.get("v1").unwrap();
        let body_hash = body_sha256_hex(body);
        let sig = sign_completion("POST", REFRESH_PATH, ts, &body_hash, key);

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

    fn refresh_body() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "request_key": {"principal": "aaaaa-aa", "counter": 17},
            "user_principal": "aaaaa-aa",
            "request_id": "11111111-1111-1111-1111-111111111111",
            "video_id": "video-17",
            "object_key": "generated/video-17.mp4"
        }))
        .unwrap()
    }

    // ── Fake deps ──

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Call {
        GenerateFreshUrl,
    }

    #[derive(Clone)]
    struct FakeRefreshDeps {
        calls: Arc<Mutex<Vec<Call>>>,
        hmac_keys: Option<String>,
        fresh_url_error: Option<String>,
    }

    impl FakeRefreshDeps {
        fn new() -> Self {
            Self {
                calls: Arc::new(Mutex::new(vec![])),
                hmac_keys: Some(TEST_KEY_SPEC.to_string()),
                fresh_url_error: None,
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
    impl UploadRefreshDeps for FakeRefreshDeps {
        fn hmac_registry(&self) -> Result<HmacKeyRegistry, String> {
            match &self.hmac_keys {
                Some(keys) => HmacKeyRegistry::parse(keys).map_err(|e| e.to_string()),
                None => Err("no hmac keys".to_string()),
            }
        }

        fn hmac_skew_secs(&self) -> i64 {
            120
        }

        async fn generate_fresh_upload_url(
            &self,
            _request_key: &RateLimiterRequestKey,
            video_id: &str,
            object_key: &str,
        ) -> Result<UploadDestination, String> {
            self.push(Call::GenerateFreshUrl);
            if let Some(err) = &self.fresh_url_error {
                return Err(err.clone());
            }
            Ok(UploadDestination {
                video_id: video_id.to_string(),
                object_key: object_key.to_string(),
                upload_url: "https://upload.example.test/fresh-url".to_string(),
                expires_at: Utc::now() + chrono::Duration::seconds(3600),
            })
        }
    }

    // ── Tests ──

    #[tokio::test]
    async fn missing_hmac_headers_returns_401() {
        let deps = FakeRefreshDeps::new();
        let body = refresh_body();
        let headers = HeaderMap::new();

        let result = refresh_with_dependencies(&deps, &headers, &body).await;

        assert_eq!(result.unwrap_err().0, StatusCode::UNAUTHORIZED);
        assert_eq!(deps.calls(), vec![]);
    }

    #[tokio::test]
    async fn invalid_hmac_returns_401() {
        let deps = FakeRefreshDeps::new();
        let body = refresh_body();
        let ts = now_ts();
        let mut headers = signed_headers(&body, ts);

        headers.insert(
            "authorization",
            HeaderValue::from_static("HMAC-SHA256 deadbeef"),
        );

        let result = refresh_with_dependencies(&deps, &headers, &body).await;

        assert_eq!(result.unwrap_err().0, StatusCode::UNAUTHORIZED);
        assert_eq!(deps.calls(), vec![]);
    }

    #[tokio::test]
    async fn unknown_key_id_returns_401() {
        let deps = FakeRefreshDeps::new();
        let body = refresh_body();
        let ts = now_ts();
        let mut headers = signed_headers(&body, ts);
        headers.insert("x-key-id", HeaderValue::from_static("v99"));

        let result = refresh_with_dependencies(&deps, &headers, &body).await;

        let (status, _) = result.unwrap_err();
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(deps.calls(), vec![]);
    }

    #[tokio::test]
    async fn valid_refresh_returns_fresh_url() {
        let deps = FakeRefreshDeps::new();
        let body = refresh_body();
        let ts = now_ts();
        let headers = signed_headers(&body, ts);

        let result = refresh_with_dependencies(&deps, &headers, &body).await;

        let Json(resp) = result.unwrap();
        assert_eq!(resp.video_id, "video-17");
        assert_eq!(resp.object_key, "generated/video-17.mp4");
        assert_eq!(resp.upload_url, "https://upload.example.test/fresh-url");
        assert_eq!(deps.calls(), vec![Call::GenerateFreshUrl]);
    }
}
