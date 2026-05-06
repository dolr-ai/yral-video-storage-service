use anyhow::Context;
use axum::{
    extract::{DefaultBodyLimit, Request},
    http::{HeaderMap, Method},
    middleware::{self, Next},
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use consts::{
    ACCESS_GRANT_NSFW, ACCESS_GRANT_SFW, HETZNER_S3_ACCESS_KEY, HETZNER_S3_BUCKET,
    HETZNER_S3_ENDPOINT, HETZNER_S3_REGION, HETZNER_S3_SECRET_KEY, SERVICE_SECRET_TOKEN,
    YRAL_VIDEOS,
};
use once_cell::sync::Lazy;
use reqwest::{header::AUTHORIZATION, StatusCode};
use sentry_tower::{NewSentryLayer, SentryHttpLayer};
use std::sync::{atomic::AtomicBool, Arc, Mutex};
use tokio::{signal, sync::Notify};
use tokio_util::sync::CancellationToken;
use tower::ServiceBuilder;
use tower_http::cors::{Any, CorsLayer};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

pub(crate) mod consts;
mod db;
mod jobs;
mod routes;
mod s3_client;
pub(crate) mod sentry_utils;
mod storj_s3_client;
mod thumbnail;

#[derive(Clone)]
pub(crate) struct AppState {
    pub s3_client: s3_client::S3Client,
    pub storj_client: storj_s3_client::StorjS3Client,
    pub db_url: String,
    pub cancel: CancellationToken,
    /// Token for cancelling running background jobs without shutting down the server.
    /// Wrapped in Mutex so it can be swapped for a fresh token after cancel_all().
    pub job_cancel: Arc<Mutex<CancellationToken>>,
    pub job_scan_storj_running: Arc<AtomicBool>,
    pub job_scan_hetzner_running: Arc<AtomicBool>,
    pub job_phash_running: Arc<AtomicBool>,
    pub job_mirror_running: Arc<AtomicBool>,
}

fn main() {
    // Initialize Sentry
    let _guard = sentry::init((
        "https://9c27a9c734fcc4481e858a089f2c8fee@sentry.prakash.yral.com/7",
        sentry::ClientOptions {
            release: sentry::release_name!(),
            environment: Some(
                std::env::var("ENVIRONMENT")
                    .unwrap_or_else(|_| "production".to_string())
                    .into(),
            ),
            send_default_pii: true,
            traces_sample_rate: 0.1,
            attach_stacktrace: true,
            ..Default::default()
        },
    ));

    // Configure sentry-tracing: ERROR -> Sentry Event, WARN -> Breadcrumb, rest -> Ignore
    let sentry_layer = sentry_tracing::layer().event_filter(|metadata| match *metadata.level() {
        tracing::Level::ERROR => sentry_tracing::EventFilter::Event,
        tracing::Level::WARN => sentry_tracing::EventFilter::Breadcrumb,
        _ => sentry_tracing::EventFilter::Ignore,
    });

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                format!(
                    "{}=info,tower_http=warn,axum::rejection=warn,hyper=warn,reqwest=warn",
                    env!("CARGO_CRATE_NAME")
                )
                .into()
            }),
        )
        .with(tracing_subscriber::fmt::layer())
        .with(sentry_layer)
        .init();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            if let Err(err) = run_server().await {
                tracing::error!("Server error: {err:#}");
                sentry::capture_error(&*err);
            }
        });
}

async fn run_server() -> anyhow::Result<()> {
    // Force loading of Storj configuration
    Lazy::force(&ACCESS_GRANT_SFW);
    Lazy::force(&ACCESS_GRANT_NSFW);
    Lazy::force(&YRAL_VIDEOS);
    Lazy::force(&SERVICE_SECRET_TOKEN);

    // Force loading of Hetzner S3 configuration
    Lazy::force(&HETZNER_S3_ENDPOINT);
    Lazy::force(&HETZNER_S3_BUCKET);
    Lazy::force(&HETZNER_S3_ACCESS_KEY);
    Lazy::force(&HETZNER_S3_SECRET_KEY);
    Lazy::force(&HETZNER_S3_REGION);

    // Force new lazy consts
    let _ = &*consts::DATABASE_URL;
    let _ = &*consts::STORJ_EU1_GATEWAY_ACCESS_KEY;
    let _ = &*consts::STORJ_EU1_GATEWAY_SECRET_KEY;
    let _ = &*consts::MIRROR_ACCESS_GRANT;

    // Initialize S3 client
    let s3_client = s3_client::S3Client::new().await;

    // Init DB schema at startup
    let db_client = db::connect(consts::DATABASE_URL.as_str())
        .await
        .context("Failed to connect to postgres")?;
    db::init_schema(&db_client)
        .await
        .context("Failed to init DB schema")?;
    drop(db_client); // jobs create their own connections

    let storj_client = storj_s3_client::StorjS3Client::new().await;
    let cancel = CancellationToken::new();
    let job_cancel = CancellationToken::new();

    // Server shutdown also cancels running jobs
    let _job_cancel_on_shutdown = cancel.clone().drop_guard();

    let app_state = AppState {
        s3_client,
        storj_client,
        db_url: consts::DATABASE_URL.clone(),
        cancel: cancel.clone(),
        job_cancel: Arc::new(Mutex::new(job_cancel)),
        job_scan_storj_running: Arc::new(AtomicBool::new(false)),
        job_scan_hetzner_running: Arc::new(AtomicBool::new(false)),
        job_phash_running: Arc::new(AtomicBool::new(false)),
        job_mirror_running: Arc::new(AtomicBool::new(false)),
    };

    // Configure CORS to allow cross-origin requests
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers(Any);

    // Sentry middleware for request tracking
    let sentry_layer = ServiceBuilder::new()
        .layer(NewSentryLayer::new_from_top())
        .layer(SentryHttpLayer::default());

    let app = Router::new()
        .route(
            "/duplicate",
            post(routes::duplicate::handler)
                .with_state(app_state.clone())
                .layer(middleware::from_fn(authorize)),
        )
        .route(
            "/duplicate_raw/upload",
            post(routes::duplicate::handler_raw_upload_initial)
                .with_state(app_state.clone())
                .layer(DefaultBodyLimit::max(500 * 1024 * 1024)), // 500MB limit for raw video upload
        )
        .route(
            "/duplicate_raw/finalize",
            post(routes::duplicate::handler_raw_finalize).with_state(app_state.clone()),
        )
        // NOTE: This will be removed as the upload happens in the very end of the pipeline and nsfw flag is passed into duplicate
        .route(
            "/move-to-nsfw",
            post(routes::move2nsfw::handler)
                .with_state(app_state.clone())
                .layer(middleware::from_fn(authorize)),
        )
        .route(
            "/hls/duplicate",
            post(routes::duplicate_hls::handler)
                .with_state(app_state.clone())
                .layer(DefaultBodyLimit::max(100 * 1024 * 1024)) // 100MB limit for HLS files
                .layer(middleware::from_fn(authorize)),
        )
        .route(
            "/mirror/jobs/scan-storj",
            post(routes::mirror::scan_storj)
                .with_state(app_state.clone())
                .layer(middleware::from_fn(authorize)),
        )
        .route(
            "/mirror/jobs/scan-hetzner",
            post(routes::mirror::scan_hetzner)
                .with_state(app_state.clone())
                .layer(middleware::from_fn(authorize)),
        )
        .route(
            "/mirror/jobs/phash",
            post(routes::mirror::phash_backfill)
                .with_state(app_state.clone())
                .layer(middleware::from_fn(authorize)),
        )
        .route(
            "/mirror/jobs/mirror",
            post(routes::mirror::mirror)
                .with_state(app_state.clone())
                .layer(middleware::from_fn(authorize)),
        )
        .route(
            "/mirror/audit",
            get(routes::mirror::audit)
                .with_state(app_state.clone())
                .layer(middleware::from_fn(authorize)),
        )
        .route(
            "/mirror/jobs/cancel",
            post(routes::mirror::cancel_all)
                .with_state(app_state.clone())
                .layer(middleware::from_fn(authorize)),
        )
        .route(
            "/mirror/jobs/status",
            get(routes::mirror::status)
                .with_state(app_state.clone())
                .layer(middleware::from_fn(authorize)),
        )
        .route("/health", get(health))
        .layer(middleware::from_fn(sentry_utils::sentry_request_logger))
        .layer(cors)
        .layer(sentry_layer);

    let addr = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();

    let server = axum::serve(addr, app);

    let notify = Arc::new(Notify::new());
    let notify_clone = notify.clone();
    let cancel_clone = cancel.clone();

    tokio::spawn(async move {
        if let Err(err) = signal::ctrl_c().await {
            tracing::error!("Failed to listen for shutdown signal: {err:#}");
        }
        cancel_clone.cancel();
        notify_clone.notify_one();
    });

    tracing::info!("Starting to listen on http://localhost:3000");

    server
        .with_graceful_shutdown(async move {
            notify.notified().await;
            tracing::info!("Shutting down gracefully...");
        })
        .await
        .context("Server error")
}

/// Simple path to check that the server is running
async fn health() -> &'static str {
    "alive"
}

/// HMAC-SHA256 request signing. Accepts credentials via headers OR query params.
///
/// Headers:
///   X-Timestamp: <unix_seconds>
///   Authorization: HMAC-SHA256 <hex_sig>
///
/// Query params (for signed URLs):
///   ?timestamp=<unix_seconds>&sig=<hex_sig>
///
/// Signature covers: HMAC-SHA256(SECRET, "METHOD\nPATH\nTIMESTAMP")
/// Requests outside a 5-minute window are rejected.
async fn authorize(
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Result<impl IntoResponse, StatusCode> {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    use std::time::{SystemTime, UNIX_EPOCH};

    // Extract (timestamp, sig_hex) from headers, falling back to query params.
    let (ts_str, sig_hex): (String, String) = if let (Some(ts_hdr), Some(auth_hdr)) =
        (headers.get("x-timestamp"), headers.get(AUTHORIZATION))
    {
        let ts = ts_hdr
            .to_str()
            .map_err(|_| StatusCode::BAD_REQUEST)?
            .to_string();
        let auth = auth_hdr.to_str().map_err(|_| StatusCode::BAD_REQUEST)?;
        let sig = auth
            .strip_prefix("HMAC-SHA256 ")
            .ok_or(StatusCode::UNAUTHORIZED)?
            .to_string();
        (ts, sig)
    } else {
        let query = request.uri().query().unwrap_or("");
        let mut ts = None;
        let mut sig = None;
        for pair in query.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                match k {
                    "timestamp" => ts = Some(v.to_string()),
                    "sig" => sig = Some(v.to_string()),
                    _ => {}
                }
            }
        }
        match (ts, sig) {
            (Some(t), Some(s)) => (t, s),
            _ => return Err(StatusCode::UNAUTHORIZED),
        }
    };

    let ts: i64 = ts_str.parse().map_err(|_| StatusCode::BAD_REQUEST)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    if (now - ts).abs() > 300 {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let sig_bytes = hex::decode(&sig_hex).map_err(|_| StatusCode::BAD_REQUEST)?;

    let message = format!(
        "{}\n{}\n{}",
        request.method().as_str(),
        request.uri().path(),
        ts_str
    );

    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(SERVICE_SECRET_TOKEN.as_bytes())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    mac.update(message.as_bytes());

    // constant-time comparison via subtle crate (prevents timing attacks)
    mac.verify_slice(&sig_bytes)
        .map_err(|_| StatusCode::UNAUTHORIZED)?;

    Ok(next.run(request).await)
}
