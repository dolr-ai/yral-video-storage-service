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
use std::sync::Arc;
use tokio::{signal, sync::Notify};
use tower::ServiceBuilder;
use tower_http::cors::{Any, CorsLayer};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

pub(crate) mod consts;
mod routes;
mod s3_client;
pub(crate) mod sentry_utils;

fn main() {
    // Initialize Sentry
    let _guard = sentry::init((
        "https://9ce8dcaeb2b87f8603bbd6f5b7e8ac2a@apm.yral.com/16",
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

    // Initialize S3 client
    let s3_client = s3_client::S3Client::new().await;

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
                .with_state(s3_client.clone())
                .layer(middleware::from_fn(authorize)),
        )
        .route(
            "/duplicate_raw/upload",
            post(routes::duplicate::handler_raw_upload_initial)
                .with_state(s3_client.clone())
                .layer(DefaultBodyLimit::max(500 * 1024 * 1024)), // 500MB limit for raw video upload
        )
        .route(
            "/duplicate_raw/finalize",
            post(routes::duplicate::handler_raw_finalize).with_state(s3_client.clone()),
        )
        // NOTE: This will be removed as the upload happens in the very end of the pipeline and nsfw flag is passed into duplicate
        .route(
            "/move-to-nsfw",
            post(routes::move2nsfw::handler)
                .with_state(s3_client.clone())
                .layer(middleware::from_fn(authorize)),
        )
        .route(
            "/hls/duplicate",
            post(routes::duplicate_hls::handler)
                .with_state(s3_client.clone())
                .layer(DefaultBodyLimit::max(100 * 1024 * 1024)) // 100MB limit for HLS files
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

    tokio::spawn(async move {
        if let Err(err) = signal::ctrl_c().await {
            tracing::error!("Failed to listen for shutdown signal: {err:#}");
        }
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

/// A dead simple authorization check based on a shared secret
async fn authorize(
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Result<impl IntoResponse, StatusCode> {
    let auth = headers.get(AUTHORIZATION).ok_or(StatusCode::UNAUTHORIZED)?;
    let auth = auth.to_str().map_err(|_| StatusCode::BAD_REQUEST)?;

    if auth != SERVICE_SECRET_TOKEN.as_str() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(next.run(request).await)
}
