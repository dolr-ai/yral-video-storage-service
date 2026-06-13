use axum::{
    body::{Body, Bytes},
    extract::Request,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use http_body_util::BodyExt;
use sentry::Level;
use std::collections::BTreeMap;
use std::time::Instant;

const BODY_CAPTURE_LIMIT: usize = 10 * 1024;

const SENSITIVE_FIELDS: &[&str] = &[
    "delegated_identity",
    "delegated_identity_wire",
    "authorization",
    "bearer",
    "token",
    "api_key",
    "secret",
    "password",
    "private_key",
    "access_token",
    "refresh_token",
];

// ─── Public helpers ───────────────────────────────────────────────────────────

/// Set Sentry user context. Call in handlers after body parse.
pub fn set_sentry_user(principal: &str, request_id: Option<&str>) {
    sentry::configure_scope(|scope| {
        scope.set_user(Some(sentry::User {
            id: Some(principal.to_string()),
            ..Default::default()
        }));
        scope.set_tag("user.principal", principal);
        if let Some(rid) = request_id {
            scope.set_tag("request_id", rid);
        }
    });
}

// ─── Middleware ───────────────────────────────────────────────────────────────

/// HTTP middleware: buffers request body for error breadcrumbs, scrubs sensitive fields.
/// On success (< 400): lightweight breadcrumb only.
/// On error (>= 400): adds request + response body (scrubbed) to Sentry breadcrumbs.
pub async fn sentry_request_logger(
    req: Request,
    next: Next,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let start = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(str::to_string);

    let user_agent = req
        .headers()
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let content_type = req
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    sentry::configure_scope(|scope| {
        scope.set_tag("http.method", method.as_str());
        scope.set_tag("http.path", &path);
        if let Some(ref rid) = request_id {
            scope.set_tag("request_id", rid);
        }
        if let Some(ref ua) = user_agent {
            scope.set_extra("user_agent", ua.clone().into());
        }
    });

    let should_capture = should_capture_body(content_type.as_deref());

    let (req, request_body_bytes) = if should_capture {
        match buffer_request_body(req).await {
            Ok(result) => result,
            Err(e) => {
                tracing::warn!("sentry: failed to buffer request body: {e}");
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "failed to process request".to_string(),
                ));
            }
        }
    } else {
        (req, None)
    };

    let response = next.run(req).await;

    let status = response.status();
    let duration = start.elapsed();

    if status.as_u16() >= 400 {
        let req_body_str = request_body_bytes.as_ref().and_then(|b| parse_and_scrub(b));

        let resp_content_type = response
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        let (response, resp_body_bytes) = if should_capture_body(resp_content_type.as_deref()) {
            match buffer_response_body(response).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("sentry: failed to buffer response body: {e}");
                    return Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "failed to process response".to_string(),
                    ));
                }
            }
        } else {
            (response, None)
        };

        let resp_body_str = resp_body_bytes.as_ref().and_then(|b| parse_and_scrub(b));

        add_request_breadcrumb(&method, &path, query.as_deref(), req_body_str.as_deref());
        add_response_breadcrumb(
            status.as_u16(),
            duration.as_millis() as u64,
            resp_body_str.as_deref(),
        );

        sentry::configure_scope(|scope| {
            scope.set_tag("http.status_code", status.as_str());
        });

        Ok(response)
    } else {
        add_lightweight_breadcrumb(&method, &path, status.as_u16(), duration.as_millis() as u64);
        Ok(response)
    }
}

// ─── Body buffering ───────────────────────────────────────────────────────────

fn should_capture_body(content_type: Option<&str>) -> bool {
    matches!(content_type, Some(ct) if ct.contains("json") || ct.contains("text"))
}

async fn buffer_request_body(
    req: Request,
) -> Result<(Request, Option<Bytes>), Box<dyn std::error::Error>> {
    let (parts, body) = req.into_parts();
    let bytes = body.collect().await?.to_bytes();
    let stored = truncate_bytes(&bytes);
    let new_req = Request::from_parts(parts, Body::from(bytes));
    Ok((new_req, stored))
}

async fn buffer_response_body(
    res: Response,
) -> Result<(Response, Option<Bytes>), Box<dyn std::error::Error>> {
    let (parts, body) = res.into_parts();
    let bytes = body.collect().await?.to_bytes();
    let stored = truncate_bytes(&bytes);
    let new_res = Response::from_parts(parts, Body::from(bytes));
    Ok((new_res, stored))
}

fn truncate_bytes(bytes: &Bytes) -> Option<Bytes> {
    if bytes.is_empty() {
        None
    } else if bytes.len() > BODY_CAPTURE_LIMIT {
        Some(bytes.slice(0..BODY_CAPTURE_LIMIT))
    } else {
        Some(bytes.clone())
    }
}

// ─── Scrubbing ────────────────────────────────────────────────────────────────

fn parse_and_scrub(bytes: &Bytes) -> Option<String> {
    if bytes.is_empty() {
        return None;
    }
    let s = String::from_utf8(bytes.to_vec()).ok()?;
    if is_sensitive(&s) {
        if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&s) {
            scrub_json(&mut json);
            return serde_json::to_string(&json).ok();
        }
    }
    Some(s)
}

fn is_sensitive(body: &str) -> bool {
    let lower = body.to_lowercase();
    SENSITIVE_FIELDS.iter().any(|f| lower.contains(f))
}

fn scrub_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            let keys: Vec<String> = map
                .keys()
                .filter(|k| {
                    let kl = k.to_lowercase();
                    SENSITIVE_FIELDS.iter().any(|f| kl.contains(f))
                })
                .cloned()
                .collect();
            for k in keys {
                map.insert(k, serde_json::json!("[REDACTED]"));
            }
            for v in map.values_mut() {
                scrub_json(v);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                scrub_json(v);
            }
        }
        _ => {}
    }
}

// ─── Breadcrumbs ─────────────────────────────────────────────────────────────

fn add_lightweight_breadcrumb(method: &http::Method, path: &str, status: u16, duration_ms: u64) {
    let mut data = BTreeMap::new();
    data.insert("method".to_string(), method.to_string().into());
    data.insert("status_code".to_string(), (status as i64).into());
    data.insert("duration_ms".to_string(), (duration_ms as i64).into());
    sentry::add_breadcrumb(sentry::Breadcrumb {
        ty: "http".into(),
        category: Some("http.request".into()),
        message: Some(format!("{method} {path} {status} ({duration_ms}ms)")),
        level: Level::Info,
        data,
        ..Default::default()
    });
}

fn add_request_breadcrumb(
    method: &http::Method,
    path: &str,
    query: Option<&str>,
    body: Option<&str>,
) {
    let mut data = BTreeMap::new();
    data.insert("method".to_string(), method.to_string().into());
    data.insert("path".to_string(), path.to_string().into());
    if let Some(q) = query {
        data.insert("query".to_string(), q.to_string().into());
    }
    if let Some(b) = body {
        data.insert("body".to_string(), b.to_string().into());
    }
    sentry::add_breadcrumb(sentry::Breadcrumb {
        ty: "http".into(),
        category: Some("http.request".into()),
        message: Some(format!("{method} {path}")),
        level: Level::Info,
        data,
        ..Default::default()
    });
}

fn add_response_breadcrumb(status: u16, duration_ms: u64, body: Option<&str>) {
    let level = if status >= 500 {
        Level::Error
    } else {
        Level::Warning
    };
    let mut data = BTreeMap::new();
    data.insert("status_code".to_string(), (status as i64).into());
    data.insert("duration_ms".to_string(), (duration_ms as i64).into());
    if let Some(b) = body {
        data.insert("body".to_string(), b.to_string().into());
    }
    sentry::add_breadcrumb(sentry::Breadcrumb {
        ty: "http".into(),
        category: Some("http.response".into()),
        message: Some(format!("HTTP {status} ({duration_ms}ms)")),
        level,
        data,
        ..Default::default()
    });
}

// ─── Breadcrumb macro ─────────────────────────────────────────────────────────

#[macro_export]
macro_rules! breadcrumb {
    ($category:expr, $operation:expr, $target:expr, $success:expr, $details:expr) => {
        sentry::add_breadcrumb(sentry::Breadcrumb {
            ty: "default".into(),
            category: Some($category.into()),
            message: Some(format!("{} {}: {}", $category, $operation, $target)),
            level: if $success {
                sentry::Level::Info
            } else {
                sentry::Level::Error
            },
            data: {
                let mut map = std::collections::BTreeMap::new();
                map.insert("operation".to_string(), ($operation).to_string().into());
                map.insert("target".to_string(), ($target).to_string().into());
                map.insert("success".to_string(), $success.into());
                map.insert("details".to_string(), ($details).to_string().into());
                map
            },
            ..Default::default()
        });
    };
    ($category:expr, $operation:expr, $target:expr, $success:expr) => {
        sentry::add_breadcrumb(sentry::Breadcrumb {
            ty: "default".into(),
            category: Some($category.into()),
            message: Some(format!("{} {}: {}", $category, $operation, $target)),
            level: if $success {
                sentry::Level::Info
            } else {
                sentry::Level::Error
            },
            data: {
                let mut map = std::collections::BTreeMap::new();
                map.insert("operation".to_string(), ($operation).to_string().into());
                map.insert("target".to_string(), ($target).to_string().into());
                map.insert("success".to_string(), $success.into());
                map
            },
            ..Default::default()
        });
    };
}
