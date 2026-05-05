use once_cell::sync::Lazy;

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

pub static SERVICE_SECRET_TOKEN: Lazy<String> = Lazy::new(|| {
    format!(
        "Bearer {}",
        std::env::var("SERVICE_SECRET_TOKEN").expect("A shared secret to be present")
    )
});

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
pub static PHASH_CONCURRENCY: Lazy<usize> = Lazy::new(|| {
    std::env::var("PHASH_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4)
});
pub static MIRROR_CONCURRENCY: Lazy<usize> = Lazy::new(|| {
    std::env::var("MIRROR_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8)
});
pub static SCAN_PAGE_SIZE: Lazy<i64> = Lazy::new(|| {
    std::env::var("SCAN_PAGE_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000)
});
pub static MAX_PHASH_RETRIES: Lazy<i32> = Lazy::new(|| {
    std::env::var("MAX_PHASH_RETRIES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5)
});
pub static TEMP_KEY_PREFIX: Lazy<String> =
    Lazy::new(|| std::env::var("TEMP_KEY_PREFIX").unwrap_or_else(|_| "_pending/".to_string()));
