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
