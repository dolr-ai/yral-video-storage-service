use axum::{body::Body, extract::Request, middleware::Next, response::Response};
use sentry::{Breadcrumb, Level};
use std::collections::BTreeMap;
use std::time::Instant;

/// Macro for adding breadcrumbs to Sentry
///
/// Usage:
/// ```
/// breadcrumb!("storj", "upload_video", "user/video.mp4", true, "completed");
/// breadcrumb!("s3", "download", "key/path", false, "timeout error");
/// breadcrumb!("http", "download", "https://example.com", true);  // without details
/// ```
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

/// Middleware for logging HTTP requests and responses to Sentry
pub async fn sentry_request_logger(request: Request<Body>, next: Next) -> Response {
    let start = Instant::now();
    let method = request.method().clone();
    let uri = request.uri().clone();
    let path = uri.path().to_string();

    // Add request breadcrumb
    sentry::add_breadcrumb(Breadcrumb {
        ty: "http".into(),
        category: Some("http.request".into()),
        message: Some(format!("{} {}", method, path)),
        level: Level::Info,
        data: {
            let mut map = BTreeMap::new();
            map.insert("method".to_string(), method.to_string().into());
            map.insert("path".to_string(), path.clone().into());
            if let Some(query) = uri.query() {
                map.insert("query".to_string(), query.into());
            }
            map
        },
        ..Default::default()
    });

    // Set request context in Sentry scope
    sentry::configure_scope(|scope| {
        scope.set_tag("http.method", method.as_str());
        scope.set_tag("http.path", &path);
    });

    // Execute the request
    let response = next.run(request).await;

    let duration = start.elapsed();
    let status = response.status();

    // Add response breadcrumb
    let level = if status.is_server_error() {
        Level::Error
    } else if status.is_client_error() {
        Level::Warning
    } else {
        Level::Info
    };

    sentry::add_breadcrumb(Breadcrumb {
        ty: "http".into(),
        category: Some("http.response".into()),
        message: Some(format!(
            "{} {} -> {} ({}ms)",
            method,
            path,
            status.as_u16(),
            duration.as_millis()
        )),
        level,
        data: {
            let mut map = BTreeMap::new();
            map.insert("status_code".to_string(), (status.as_u16() as i64).into());
            map.insert(
                "duration_ms".to_string(),
                (duration.as_millis() as i64).into(),
            );
            map
        },
        ..Default::default()
    });

    // Update scope with response info
    sentry::configure_scope(|scope| {
        scope.set_tag("http.status_code", status.as_str());
        scope.set_extra("response.duration_ms", (duration.as_millis() as i64).into());
    });

    response
}
