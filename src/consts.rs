use candid::Principal;
use once_cell::sync::Lazy;
use std::sync::atomic::{AtomicI64, AtomicUsize};

pub const ENVIRONMENT: &str = "ENVIRONMENT";
pub const MODERATION_MODE: &str = "MODERATION_MODE";
pub const MODERATION_TIMEOUT_MS: &str = "MODERATION_TIMEOUT_MS";
pub const MODERATION_SERVICE_URL: &str = "MODERATION_SERVICE_URL";
pub const VIDEOGEN_GENERATE_DEDUPE_WINDOW_SECS: &str = "VIDEOGEN_GENERATE_DEDUPE_WINDOW_SECS";
pub const VIDEOGEN_VAST_SUBMIT_TIMEOUT_SECS: &str = "VIDEOGEN_VAST_SUBMIT_TIMEOUT_SECS";
pub const VIDEOGEN_UPLOAD_DESTINATION_TIMEOUT_SECS: &str =
    "VIDEOGEN_UPLOAD_DESTINATION_TIMEOUT_SECS";
pub const VIDEOGEN_VAST_IMAGE_STAGE_TIMEOUT_SECS: &str = "VIDEOGEN_VAST_IMAGE_STAGE_TIMEOUT_SECS";
pub const VIDEOGEN_LTX_GENERATION_TIMEOUT_SECS: &str = "VIDEOGEN_LTX_GENERATION_TIMEOUT_SECS";
pub const VIDEOGEN_VAST_UPLOAD_EXPIRY_REFRESH_MARGIN_SECS: &str =
    "VIDEOGEN_VAST_UPLOAD_EXPIRY_REFRESH_MARGIN_SECS";
pub const VIDEOGEN_UPLOAD_URL_TTL_SECS: &str = "VIDEOGEN_UPLOAD_URL_TTL_SECS";
pub const VIDEOGEN_COMPLETION_HMAC_SKEW_SECS: &str = "VIDEOGEN_COMPLETION_HMAC_SKEW_SECS";
pub const VIDEOGEN_COMPLETION_HMAC_KEYS: &str = "VIDEOGEN_COMPLETION_HMAC_KEYS";
pub const VIDEOGEN_SERVICE_AUTH_TOKEN: &str = "VIDEOGEN_SERVICE_AUTH_TOKEN";
pub const VIDEOGEN_VAST_SUBMIT_TRANSPORT: &str = "VIDEOGEN_VAST_SUBMIT_TRANSPORT";
pub const VIDEOGEN_RABBITMQ_AMQPS_URLS: &str = "VIDEOGEN_RABBITMQ_AMQPS_URLS";
pub const VIDEOGEN_RABBITMQ_PUBLISHER_PASSWORD: &str = "VIDEOGEN_RABBITMQ_PUBLISHER_PASSWORD";
pub const VIDEOGEN_RABBITMQ_EXCHANGE: &str = "VIDEOGEN_RABBITMQ_EXCHANGE";
pub const VIDEOGEN_RABBITMQ_ROUTING_KEY: &str = "VIDEOGEN_RABBITMQ_ROUTING_KEY";
pub const VIDEOGEN_RABBITMQ_PUBLISH_TIMEOUT_SECS: &str = "VIDEOGEN_RABBITMQ_PUBLISH_TIMEOUT_SECS";
pub const VIDEOGEN_RABBITMQ_CONNECTION_NAME: &str = "VIDEOGEN_RABBITMQ_CONNECTION_NAME";
pub const VIDEOGEN_RABBITMQ_TLS_CA_CERT_PEM_B64: &str = "VIDEOGEN_RABBITMQ_TLS_CA_CERT_PEM_B64";
pub const VIDEOGEN_UPLOAD_DESTINATION_RELEASE_URL: &str = "VIDEOGEN_UPLOAD_DESTINATION_RELEASE_URL";

// Videogen upload service URL env var names
pub const VIDEOGEN_UPLOAD_SERVICE_URL_ENV: &str = "VIDEOGEN_UPLOAD_SERVICE_URL";
pub const VIDEOGEN_LEGACY_UPLOAD_SERVICE_URL_ENV: &str = "YRAL_UPLOAD_SERVICE";
pub const VIDEOGEN_UPLOAD_SERVICE_DEFAULT_URL: &str = "https://upload.yral.com";

// Storj configuration
pub static YRAL_VIDEOS: Lazy<String> = Lazy::new(|| {
    const FALLBACK: &str = "yral-videos";
    std::env::var("SFW_BUCKET")
        .inspect_err(|err| tracing::warn!("Using fallback for SFW_BUCKET because {err}"))
        .unwrap_or_else(|_| FALLBACK.into())
});
pub static ACCESS_GRANT_SFW: Lazy<String> = Lazy::new(|| {
    std::env::var("STORJ_ACCESS_GRANT_SFW")
        .expect("Access grant to be present: STORJ_ACCESS_GRANT_SFW")
});

pub static YRAL_NSFW_VIDEOS: Lazy<String> = Lazy::new(|| {
    const FALLBACK: &str = "yral-nsfw-videos";
    std::env::var("NSFW_BUCKET")
        .inspect_err(|err| tracing::warn!("Using fallback for NSFW_BUCKET because {err}"))
        .unwrap_or_else(|_| FALLBACK.into())
});
pub static ACCESS_GRANT_NSFW: Lazy<String> = Lazy::new(|| {
    std::env::var("STORJ_ACCESS_GRANT_NSFW")
        .expect("Access grant to be present: STORJ_ACCESS_GRANT_NSFW")
});

// Hetzner S3 configuration (for SFW videos)
pub static HETZNER_S3_ENDPOINT: Lazy<String> = Lazy::new(|| {
    std::env::var("HETZNER_S3_ENDPOINT")
        .expect("Hetzner S3 endpoint to be present: HETZNER_S3_ENDPOINT")
});
pub static HETZNER_S3_BUCKET: Lazy<String> = Lazy::new(|| {
    std::env::var("HETZNER_S3_BUCKET").expect("Hetzner S3 bucket to be present: HETZNER_S3_BUCKET")
});
pub static HETZNER_S3_ACCESS_KEY: Lazy<String> = Lazy::new(|| {
    std::env::var("HETZNER_S3_ACCESS_KEY")
        .expect("Hetzner S3 access key to be present: HETZNER_S3_ACCESS_KEY")
});
pub static HETZNER_S3_SECRET_KEY: Lazy<String> = Lazy::new(|| {
    std::env::var("HETZNER_S3_SECRET_KEY")
        .expect("Hetzner S3 secret key to be present: HETZNER_S3_SECRET_KEY")
});
pub static HETZNER_S3_REGION: Lazy<String> =
    Lazy::new(|| std::env::var("HETZNER_S3_REGION").unwrap_or_else(|_| "eu-central".to_string()));

pub static SERVICE_SECRET_TOKEN: Lazy<String> =
    Lazy::new(|| std::env::var("SERVICE_SECRET_TOKEN").expect("A shared secret to be present"));

// Mirror access grant — scoped to yral-sfw on EU1 satellite (separate from ACCESS_GRANT_SFW
// which targets yral-videos and is used by existing upload routes)
pub static MIRROR_ACCESS_GRANT: Lazy<String> = Lazy::new(|| {
    std::env::var("STORJ_MIRROR_ACCESS_GRANT")
        .expect("STORJ_MIRROR_ACCESS_GRANT required for mirror job uploads")
});

// Storj S3 gateway credentials (for listing/verifying, not uploads)
pub static STORJ_EU1_GATEWAY_ACCESS_KEY: Lazy<String> = Lazy::new(|| {
    std::env::var("STORJ_EU1_GATEWAY_ACCESS_KEY").expect("STORJ_EU1_GATEWAY_ACCESS_KEY required")
});
pub static STORJ_EU1_GATEWAY_SECRET_KEY: Lazy<String> = Lazy::new(|| {
    std::env::var("STORJ_EU1_GATEWAY_SECRET_KEY").expect("STORJ_EU1_GATEWAY_SECRET_KEY required")
});
pub static STORJ_SFW_BUCKET: Lazy<String> =
    Lazy::new(|| std::env::var("STORJ_SFW_BUCKET").unwrap_or_else(|_| "yral-sfw".to_string()));

// Database
pub static DATABASE_URL: Lazy<String> =
    Lazy::new(|| std::env::var("DATABASE_URL").expect("DATABASE_URL required"));

// Job tuning
pub static PHASH_CONCURRENCY: Lazy<AtomicUsize> = Lazy::new(|| {
    let val = std::env::var("PHASH_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    AtomicUsize::new(val)
});
pub static MIRROR_CONCURRENCY: Lazy<AtomicUsize> = Lazy::new(|| {
    let val = std::env::var("MIRROR_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    AtomicUsize::new(val)
});
pub static SCAN_PAGE_SIZE: Lazy<AtomicI64> = Lazy::new(|| {
    let val = std::env::var("SCAN_PAGE_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2500);
    AtomicI64::new(val)
});
pub static MAX_PHASH_RETRIES: Lazy<i32> = Lazy::new(|| {
    std::env::var("MAX_PHASH_RETRIES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5)
});
pub static TEMP_KEY_PREFIX: Lazy<String> =
    Lazy::new(|| std::env::var("TEMP_KEY_PREFIX").unwrap_or_else(|_| "_pending/".to_string()));

// IC canister IDs
pub static RATE_LIMITS_CANISTER_ID: Lazy<Principal> = Lazy::new(|| {
    "h2jgv-ayaaa-aaaas-qbh4a-cai"
        .parse()
        .expect("Rate limits canister ID to be valid")
});

// IC network URL
pub static IC_URL: Lazy<String> =
    Lazy::new(|| std::env::var("IC_URL").unwrap_or_else(|_| "https://ic0.app".to_string()));
