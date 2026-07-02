#![recursion_limit = "256"]
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
use ic_agent::{identity::Secp256k1Identity, Agent};
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
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

pub(crate) mod consts;
mod db;
mod jobs;
mod media_index;
mod routes;
mod s3_client;
pub(crate) mod sentry_utils;
mod storj_s3_client;
mod thumbnail;
mod videogen;

#[derive(Clone)]
pub(crate) struct AppState {
    pub s3_client: s3_client::S3Client,
    pub storj_client: storj_s3_client::StorjS3Client,
    pub db_url: String,
    /// Token for cancelling running background jobs without shutting down the server.
    /// Wrapped in Mutex so it can be swapped for a fresh token after cancel_all().
    pub job_cancel: Arc<Mutex<CancellationToken>>,
    pub job_scan_storj_running: Arc<AtomicBool>,
    pub job_scan_hetzner_running: Arc<AtomicBool>,
    pub job_phash_running: Arc<AtomicBool>,
    pub job_mirror_running: Arc<AtomicBool>,
    pub job_pipeline_running: Arc<AtomicBool>,
    pub job_media_import_running: Arc<AtomicBool>,
    /// Separate cancellation token for the two media jobs (import + pHash).
    /// Cancelling this does NOT affect the mirror jobs and vice-versa.
    pub media_job_cancel: Arc<Mutex<CancellationToken>>,
    pub job_media_phash_running: Arc<AtomicBool>,
    pub job_chain_snapshot_running: Arc<AtomicBool>,
    pub ic_agent: Agent,
    /// Optional best-effort upload side-effect clients (offchain events + push
    /// notifications). Each is independently `None` when its token is unset; the
    /// core publish flow works regardless.
    pub upload: routes::upload::UploadState,
}

#[derive(OpenApi)]
#[openapi(
    info(title = "Storj Interface API", version = "0.1.0"),
    paths(
        health,
        routes::duplicate::handler,
        routes::duplicate::handler_raw_upload_initial,
        routes::duplicate::handler_raw_finalize,
        routes::move2nsfw::handler,
        routes::duplicate_hls::handler,
        routes::mirror::scan_storj,
        routes::mirror::scan_hetzner,
        routes::mirror::phash_backfill,
        routes::mirror::mirror,
        routes::mirror::run_pipeline,
        routes::mirror::audit,
        routes::mirror::duplicates,
        routes::mirror::video_duplicates,
        routes::mirror::failed_jobs,
        routes::mirror::retry_failed,
        routes::mirror::cancel_all,
        routes::mirror::status,
        routes::mirror::get_config,
        routes::mirror::update_config,
        routes::media::import_video_index,
        routes::media::missing_phash_audit,
        routes::media::feed_events,
        routes::media::run_phash,
        routes::media::cancel_media_jobs,
        routes::media::media_jobs_status,
        routes::media::media_jobs_runs,
        routes::media::media_jobs_failures,
        routes::media::media_sweep_status,
        routes::videogen::drafts::get_in_progress_drafts,
        routes::videogen::generate::generate_video,
        routes::videogen::providers::get_providers,
        routes::videogen::providers::get_providers_all,
        routes::videogen::complete::complete_video,
        routes::videogen::upload_refresh::refresh_upload_url,
        // routes::videogen::get_in_progress_by_principal,
        // routes::videogen::get_all_status_by_principal,
    ),
    components(schemas(
        storj_interface::duplicate::Args,
        storj_interface::move2nsfw::Args,
        routes::duplicate::RawFinalizeBody,
        routes::mirror::VideoEntry,
        routes::mirror::AuditResponse,
        routes::mirror::DuplicateEntry,
        routes::mirror::DuplicatesResponse,
        routes::mirror::DuplicateGroup,
        routes::mirror::FailedJobEntry,
        routes::mirror::FailedJobsResponse,
        routes::mirror::RetryResponse,
        routes::mirror::JobStatus,
        routes::mirror::ConfigResponse,
        routes::mirror::ConfigUpdate,
        routes::media::CoverageStatsResponse,
        routes::media::FeedEvent,
        routes::media::FeedResponse,
        routes::media::MediaJobsStatus,
        routes::media::MediaCancelResponse,
        routes::media::JobRunView,
        routes::media::JobRunsResponse,
        routes::media::FailureGroupView,
        routes::media::FailuresResponse,
        routes::videogen::InProgressDraftsRequest,
        routes::videogen::InProgressDraftItem,
        routes::videogen::InProgressDraftsResponse,
        routes::videogen::GenerateVideoRequest,
        routes::videogen::GenerateVideoRequestBody,
        routes::videogen::GenerateResponse,
        routes::videogen::GenerateTokenType,
        routes::videogen::ImageInput,
        routes::videogen::ImageSource,
        routes::videogen::VideoGenError,
        routes::videogen::VideoUploadHandling,
        routes::videogen::ProvidersResponse,
        routes::videogen::ProviderItem,
        routes::videogen::ProviderCost,
        routes::videogen::CompleteVideoRequest,
        routes::videogen::CompletionStatus,
        routes::videogen::CompletionError,
        routes::videogen::CompletionRequestKey,
        routes::videogen::UploadRefreshRequest,
        routes::videogen::UploadRefreshResponse,
        routes::videogen::RefreshError,
        // routes::videogen::AllStatusItem,
        // routes::videogen::AllStatusResponse,
    )),
    tags(
        (name = "videos", description = "Video management endpoints"),
        (name = "mirror", description = "Mirror job management"),
        (name = "media", description = "Media ownership and feed endpoints"),
        (name = "videogen", description = "Video generation status"),
    )
)]
struct ApiDoc;

fn main() {
    let _ = rustls::crypto::ring::default_provider().install_default();

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
    media_index::init_schema(&db_client)
        .await
        .context("Failed to init media index schema")?;
    drop(db_client); // jobs create their own connections

    let storj_client = storj_s3_client::StorjS3Client::new().await;
    let ic_agent = {
        let mut builder = Agent::builder().with_url(consts::IC_URL.as_str());
        if let Ok(pem) = std::env::var("BACKEND_ADMIN_IDENTITY") {
            let identity =
                Secp256k1Identity::from_pem(stringreader::StringReader::new(pem.as_str()))
                    .context("Failed to parse BACKEND_ADMIN_IDENTITY")?;
            builder = builder.with_identity(identity);
        }
        builder.build().context("Failed to build IC agent")?
    };
    let upload = routes::upload::UploadState::from_env();
    if upload.events_service.is_none() {
        tracing::warn!(
            "OFFCHAIN_EVENTS_API_TOKEN not set — upload analytics events disabled \
             (publishing still works)"
        );
    }
    if upload.notification_client.is_none() {
        tracing::warn!(
            "YRAL_METADATA_NOTIFICATION_SERVICE_API_TOKEN not set — push notifications \
             disabled (publishing still works)"
        );
    }

    let cancel = CancellationToken::new();
    let job_cancel = CancellationToken::new();

    // Server shutdown also cancels running jobs
    let _job_cancel_on_shutdown = cancel.clone().drop_guard();

    let app_state = AppState {
        s3_client,
        storj_client,
        db_url: consts::DATABASE_URL.clone(),
        job_cancel: Arc::new(Mutex::new(job_cancel)),
        job_scan_storj_running: Arc::new(AtomicBool::new(false)),
        job_scan_hetzner_running: Arc::new(AtomicBool::new(false)),
        job_phash_running: Arc::new(AtomicBool::new(false)),
        job_mirror_running: Arc::new(AtomicBool::new(false)),
        job_pipeline_running: Arc::new(AtomicBool::new(false)),
        job_media_import_running: Arc::new(AtomicBool::new(false)),
        media_job_cancel: Arc::new(Mutex::new(CancellationToken::new())),
        job_media_phash_running: Arc::new(AtomicBool::new(false)),
        job_chain_snapshot_running: Arc::new(AtomicBool::new(false)),
        ic_agent,
        upload,
    };

    // Steady-state sweep worker (leased; single-runner across boxes). Ships disabled
    // via RUN_SWEEP_WORKER; enabling on all 3 boxes is safe because the DB lease elects
    // exactly one. `me` = NODE_NAME (stable per box across redeploys) so a restart
    // re-adopts its own lease without waiting a TTL.
    if consts::run_sweep_worker() {
        let me = std::env::var("NODE_NAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| format!("sweep-{}", uuid::Uuid::new_v4()));
        let worker = jobs::worker::SweepWorker {
            s3: app_state.s3_client.clone(),
            storj: app_state.storj_client.clone(),
            db_url: app_state.db_url.clone(),
            drain_flag: app_state.job_media_phash_running.clone(),
            import_flag: app_state.job_media_import_running.clone(),
            media_cancel: app_state.media_job_cancel.clone(),
            me,
            drain_interval: std::time::Duration::from_secs(consts::drain_interval_secs()),
            discovery_interval: std::time::Duration::from_secs(consts::discovery_interval_secs()),
            lease_ttl: std::time::Duration::from_secs(consts::sweep_lease_ttl_secs()),
        };
        tokio::spawn(worker.run(cancel.clone()));
    }

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
        // Upload-service routes (merged in). PUBLIC — no `authorize` layer; auth is the
        // in-body chain-verified delegated identity. Small JSON bodies → default 2MB limit.
        .route(
            "/get-upload-url",
            post(routes::upload::get_upload_url::get_upload_url).with_state(app_state.clone()),
        )
        .route(
            "/update-video-metadata",
            post(routes::upload::update_video_metadata::update_video_metadata)
                .with_state(app_state.clone()),
        )
        .route(
            "/mark-post-as-published",
            post(routes::upload::mark_post_as_published::mark_post_as_published)
                .with_state(app_state.clone()),
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
            "/mirror/jobs/run-pipeline",
            post(routes::mirror::run_pipeline)
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
            "/mirror/duplicates",
            get(routes::mirror::duplicates)
                .with_state(app_state.clone())
                .layer(middleware::from_fn(authorize)),
        )
        .route(
            "/mirror/duplicates/{video_id}",
            get(routes::mirror::video_duplicates)
                .with_state(app_state.clone())
                .layer(middleware::from_fn(authorize)),
        )
        .route(
            "/mirror/jobs/failed",
            get(routes::mirror::failed_jobs)
                .with_state(app_state.clone())
                .layer(middleware::from_fn(authorize)),
        )
        .route(
            "/mirror/jobs/retry-failed",
            post(routes::mirror::retry_failed)
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
        .route(
            "/mirror/config",
            get(routes::mirror::get_config)
                .post(routes::mirror::update_config)
                .layer(middleware::from_fn(authorize)),
        )
        .route(
            "/media/import/video-index",
            post(routes::media::import_video_index)
                .with_state(app_state.clone())
                .layer(middleware::from_fn(authorize)),
        )
        .route(
            "/media/audit/missing-phash",
            get(routes::media::missing_phash_audit)
                .with_state(app_state.clone())
                .layer(middleware::from_fn(authorize)),
        )
        .route(
            "/media/feed/events",
            get(routes::media::feed_events)
                .with_state(app_state.clone())
                .layer(middleware::from_fn(authorize)),
        )
        .route(
            "/media/phash/run",
            post(routes::media::run_phash)
                .with_state(app_state.clone())
                .layer(middleware::from_fn(authorize)),
        )
        .route(
            "/media/jobs/cancel",
            post(routes::media::cancel_media_jobs)
                .with_state(app_state.clone())
                .layer(middleware::from_fn(authorize)),
        )
        .route(
            "/media/jobs/status",
            get(routes::media::media_jobs_status)
                .with_state(app_state.clone())
                .layer(middleware::from_fn(authorize)),
        )
        .route(
            "/media/jobs/runs",
            get(routes::media::media_jobs_runs)
                .with_state(app_state.clone())
                .layer(middleware::from_fn(authorize)),
        )
        .route(
            "/media/jobs/failures",
            get(routes::media::media_jobs_failures)
                .with_state(app_state.clone())
                .layer(middleware::from_fn(authorize)),
        )
        .route(
            "/media/sweep/status",
            get(routes::media::media_sweep_status)
                .with_state(app_state.clone())
                .layer(middleware::from_fn(authorize)),
        )
        .route(
            "/chain/snapshot",
            post(routes::chain::chain_snapshot_start)
                .with_state(app_state.clone())
                .layer(middleware::from_fn(authorize)),
        )
        .route(
            "/chain/snapshot/status",
            get(routes::chain::chain_snapshot_status)
                .with_state(app_state.clone())
                .layer(middleware::from_fn(authorize)),
        )
        .route(
            "/chain/audit",
            get(routes::chain::chain_audit)
                .with_state(app_state.clone())
                .layer(middleware::from_fn(authorize)),
        )
        .route(
            "/chain/diagnose",
            get(routes::chain::chain_diagnose)
                .with_state(app_state.clone())
                .layer(middleware::from_fn(authorize)),
        )
        .route(
            "/api/v2/videogen/drafts/in-progress",
            post(routes::videogen::get_in_progress_drafts).with_state(app_state.clone()),
        )
        .route(
            "/api/v2/videogen/generate",
            post(routes::videogen::generate_video)
                .with_state(app_state.clone())
                .layer(DefaultBodyLimit::max(10 * 1024 * 1024)),
        )
        .route(
            "/api/v2/videogen/complete",
            post(routes::videogen::complete_video)
                .with_state(app_state.clone())
                .layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            "/api/v2/videogen/upload-url/refresh",
            post(routes::videogen::refresh_upload_url)
                .with_state(app_state.clone())
                .layer(DefaultBodyLimit::max(64 * 1024)),
        )
        // .route(
        //     "/api/v2/videogen/in-progress/{principal}",
        //     get(routes::videogen::get_in_progress_by_principal).with_state(app_state.clone()),
        // )
        // .route(
        //     "/api/v2/videogen/status/{principal}/all",
        //     get(routes::videogen::get_all_status_by_principal).with_state(app_state.clone()),
        // )
        .route(
            "/api/v2/videogen/providers",
            get(routes::videogen::get_providers),
        )
        .route(
            "/api/v2/videogen/providers-all",
            get(routes::videogen::get_providers_all),
        )
        .route("/health", get(health))
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", ApiDoc::openapi()))
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
#[utoipa::path(
    get,
    path = "/health",
    tag = "misc",
    responses((status = 200, description = "Server is alive"))
)]
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
