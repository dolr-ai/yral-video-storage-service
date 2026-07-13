//! Profile-image storage on Hetzner Object Storage (bucket `yral-profile`).
//!
//! Ported from `off-chain-agent/src/utils/s3.rs` with three changes:
//! - config comes from `PROFILE_S3_*` env (bucket/prefix/public-url), creds/endpoint/region
//!   reuse the service's existing `HETZNER_S3_*`;
//! - decode is bounded by `image::Limits` (guards decompression bombs);
//! - upload deletes the user's prior `profile-*` objects first (bounds orphan growth).

use aws_config::{BehaviorVersion, Region};
use aws_sdk_s3::{config::Credentials, primitives::ByteStream, types::ObjectCannedAcl, Client};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use image::{DynamicImage, ImageFormat, ImageReader};
use std::env;
use std::io::Cursor;
use std::time::{SystemTime, UNIX_EPOCH};

/// Where profile images live + how their keys/URLs are formed.
pub struct ProfileS3Config {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub key_prefix: String,
    pub public_url_base: String,
}

impl ProfileS3Config {
    /// Bucket/prefix/URL from `PROFILE_S3_*`; endpoint/region from the shared `HETZNER_S3_*`.
    pub fn from_env() -> Self {
        Self {
            endpoint: env::var("HETZNER_S3_ENDPOINT")
                .unwrap_or_else(|_| "https://hel1.your-objectstorage.com".to_string()),
            region: env::var("HETZNER_S3_REGION").unwrap_or_else(|_| "hel1".to_string()),
            bucket: env::var("PROFILE_S3_BUCKET").unwrap_or_else(|_| "yral-profile".to_string()),
            key_prefix: env::var("PROFILE_S3_KEY_PREFIX").unwrap_or_else(|_| "users/".to_string()),
            public_url_base: env::var("PROFILE_S3_PUBLIC_URL_BASE")
                .unwrap_or_else(|_| "https://yral-profile.hel1.your-objectstorage.com".to_string()),
        }
    }

    /// Object key for a user's current image at `ts`.
    pub fn object_key(&self, principal: &str, ts: u64) -> String {
        format!("{}{}/profile-{}.jpg", self.key_prefix, principal, ts)
    }

    /// List/delete prefix scoping to exactly one user's profile images.
    pub fn user_prefix(&self, principal: &str) -> String {
        format!("{}{}/profile-", self.key_prefix, principal)
    }

    #[cfg(test)]
    pub fn test_defaults() -> Self {
        Self {
            endpoint: "https://hel1.your-objectstorage.com".to_string(),
            region: "hel1".to_string(),
            bucket: "yral-profile".to_string(),
            key_prefix: "users/".to_string(),
            public_url_base: "https://yral-profile.hel1.your-objectstorage.com".to_string(),
        }
    }
}

/// S3 client for Hetzner using the shared `HETZNER_S3_ACCESS_KEY`/`HETZNER_S3_SECRET_KEY`.
pub async fn create_client(cfg: &ProfileS3Config) -> Result<Client, String> {
    let access_key = env::var("HETZNER_S3_ACCESS_KEY")
        .map_err(|_| "Missing HETZNER_S3_ACCESS_KEY environment variable".to_string())?;
    let secret_key = env::var("HETZNER_S3_SECRET_KEY")
        .map_err(|_| "Missing HETZNER_S3_SECRET_KEY environment variable".to_string())?;
    let credentials = Credentials::new(access_key, secret_key, None, None, "hetzner-s3");
    let aws_config = aws_config::defaults(BehaviorVersion::latest())
        .region(Region::new(cfg.region.clone()))
        .credentials_provider(credentials)
        .endpoint_url(&cfg.endpoint)
        .load()
        .await;
    Ok(Client::new(&aws_config))
}

/// Decode (bounded), resize so the longest side is <= 1000px (Lanczos3, aspect
/// preserved), drop alpha, and encode JPEG q85. Ported from off-chain-agent.
pub fn process_image(image_bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut reader = ImageReader::new(Cursor::new(image_bytes))
        .with_guessed_format()
        .map_err(|e| format!("Failed to read image: {e}"))?;

    // Guard against decompression bombs: cap decoded dimensions.
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    reader.limits(limits);

    let img = reader
        .decode()
        .map_err(|e| format!("Failed to decode image: {e}"))?;

    const MAX_SIZE: u32 = 1000;
    let (width, height) = (img.width(), img.height());
    let processed = if width > MAX_SIZE || height > MAX_SIZE {
        let ratio = (MAX_SIZE as f32 / width.max(height) as f32).min(1.0);
        let new_width = (width as f32 * ratio) as u32;
        let new_height = (height as f32 * ratio) as u32;
        img.resize(new_width, new_height, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };

    let rgb = DynamicImage::ImageRgb8(processed.to_rgb8());
    let mut output = Vec::new();
    rgb.write_to(&mut Cursor::new(&mut output), ImageFormat::Jpeg)
        .map_err(|e| format!("Failed to encode image as JPEG: {e}"))?;
    Ok(output)
}

/// Process + upload a base64 image for `principal`, returning its public URL.
/// Deletes the user's prior images first (best-effort) so storage stays bounded.
pub async fn upload_profile_image(
    cfg: &ProfileS3Config,
    client: &Client,
    image_data_base64: &str,
    principal: &str,
) -> Result<String, String> {
    let image_bytes = BASE64
        .decode(image_data_base64)
        .map_err(|e| format!("Failed to decode base64 image: {e}"))?;
    let processed = process_image(&image_bytes)?;

    // F4: remove prior objects before writing the new (timestamped) key. Best-effort:
    // a failure here must not fail the upload.
    if let Err(e) = delete_profile_images(cfg, client, principal).await {
        tracing::warn!("profile-image: failed to delete prior images for {principal}: {e}");
    }

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("clock error: {e}"))?
        .as_secs();
    let key = cfg.object_key(principal, ts);

    client
        .put_object()
        .bucket(&cfg.bucket)
        .key(&key)
        .body(ByteStream::from(processed))
        .content_type("image/jpeg")
        .acl(ObjectCannedAcl::PublicRead)
        .send()
        .await
        .map_err(|e| format!("Failed to upload image to S3: {e}"))?;

    Ok(format!("{}/{}", cfg.public_url_base, key))
}

/// Delete all of a user's `profile-*` objects.
pub async fn delete_profile_images(
    cfg: &ProfileS3Config,
    client: &Client,
    principal: &str,
) -> Result<(), String> {
    let prefix = cfg.user_prefix(principal);
    let list = client
        .list_objects_v2()
        .bucket(&cfg.bucket)
        .prefix(&prefix)
        .send()
        .await
        .map_err(|e| format!("Failed to list objects from S3: {e}"))?;

    for object in list.contents() {
        if let Some(key) = object.key() {
            client
                .delete_object()
                .bucket(&cfg.bucket)
                .key(key)
                .send()
                .await
                .map_err(|e| format!("Failed to delete image from S3: {e}"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, RgbImage, RgbaImage};

    #[test]
    fn object_key_uses_prefix_principal_timestamp() {
        let cfg = ProfileS3Config::test_defaults();
        assert_eq!(
            cfg.object_key("aaaaa-aa", 1_700_000_000),
            "users/aaaaa-aa/profile-1700000000.jpg"
        );
    }

    #[test]
    fn user_prefix_targets_only_that_user() {
        let cfg = ProfileS3Config::test_defaults();
        assert_eq!(cfg.user_prefix("aaaaa-aa"), "users/aaaaa-aa/profile-");
    }

    fn encode(img: DynamicImage, fmt: ImageFormat) -> Vec<u8> {
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), fmt).unwrap();
        buf
    }

    #[test]
    fn process_png_input_yields_jpeg() {
        let png = encode(
            DynamicImage::ImageRgba8(RgbaImage::new(64, 64)),
            ImageFormat::Png,
        );
        let out = process_image(&png).expect("process png");
        assert_eq!(
            image::guess_format(&out).unwrap(),
            ImageFormat::Jpeg,
            "output must be JPEG"
        );
    }

    #[test]
    fn process_resizes_large_images_within_1000px() {
        // 1200x300 JPEG in → longest side must be clamped to 1000.
        let big = encode(
            DynamicImage::ImageRgb8(RgbImage::new(1200, 300)),
            ImageFormat::Jpeg,
        );
        let out = process_image(&big).expect("process big");
        let decoded = image::load_from_memory(&out).unwrap();
        assert!(decoded.width() <= 1000 && decoded.height() <= 1000);
        assert_eq!(decoded.width(), 1000, "longest side scaled to 1000");
    }
}
