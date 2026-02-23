use aws_config::BehaviorVersion;
use aws_sdk_s3::config::{Credentials, Region};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::{Client, Config};
use bytes::Bytes;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::time::Duration;

use crate::consts::{
    HETZNER_S3_ACCESS_KEY, HETZNER_S3_BUCKET, HETZNER_S3_ENDPOINT, HETZNER_S3_REGION,
    HETZNER_S3_SECRET_KEY,
};

const MAX_RETRIES: u32 = 3;
const BASE_DELAY_MS: u64 = 500;

async fn retry_s3_op<F, Fut, T>(
    operation_name: &str,
    key: &str,
    f: F,
) -> Result<T, aws_sdk_s3::Error>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T, aws_sdk_s3::Error>>,
{
    let mut last_err = None;
    for attempt in 0..=MAX_RETRIES {
        match f().await {
            Ok(val) => return Ok(val),
            Err(e) => {
                if attempt == MAX_RETRIES {
                    tracing::error!(
                        operation = operation_name,
                        key = key,
                        attempts = attempt + 1,
                        error = %e,
                        "S3 operation failed after all retries"
                    );
                    last_err = Some(e);
                    break;
                }
                let delay_ms = BASE_DELAY_MS * 2u64.pow(attempt);
                let jitter = (std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .subsec_nanos()
                    % 250) as u64;
                let total_delay = Duration::from_millis(delay_ms + jitter);
                tracing::warn!(
                    operation = operation_name,
                    key = key,
                    attempt = attempt + 1,
                    max_retries = MAX_RETRIES,
                    delay_ms = total_delay.as_millis() as u64,
                    error = %e,
                    "S3 operation failed, retrying"
                );
                tokio::time::sleep(total_delay).await;
            }
        }
    }
    Err(last_err.unwrap())
}

async fn retry_s3_op_string<F, Fut, T>(operation_name: &str, key: &str, f: F) -> Result<T, String>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    let mut last_err = String::new();
    for attempt in 0..=MAX_RETRIES {
        match f().await {
            Ok(val) => return Ok(val),
            Err(e) => {
                if attempt == MAX_RETRIES {
                    tracing::error!(
                        operation = operation_name,
                        key = key,
                        attempts = attempt + 1,
                        error = %e,
                        "S3 operation failed after all retries"
                    );
                    last_err = e;
                    break;
                }
                let delay_ms = BASE_DELAY_MS * 2u64.pow(attempt);
                let jitter = (std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .subsec_nanos()
                    % 250) as u64;
                let total_delay = Duration::from_millis(delay_ms + jitter);
                tracing::warn!(
                    operation = operation_name,
                    key = key,
                    attempt = attempt + 1,
                    max_retries = MAX_RETRIES,
                    delay_ms = total_delay.as_millis() as u64,
                    error = %e,
                    "S3 operation failed, retrying"
                );
                tokio::time::sleep(total_delay).await;
            }
        }
    }
    Err(last_err)
}

#[derive(Clone)]
pub struct S3Client {
    client: Client,
    bucket: String,
}

impl S3Client {
    pub async fn new() -> Self {
        let creds = Credentials::new(
            HETZNER_S3_ACCESS_KEY.as_str(),
            HETZNER_S3_SECRET_KEY.as_str(),
            None,
            None,
            "hetzner_s3",
        );

        let config = Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(HETZNER_S3_REGION.clone()))
            .endpoint_url(HETZNER_S3_ENDPOINT.as_str())
            .credentials_provider(creds)
            .force_path_style(true)
            .build();

        let client = Client::from_conf(config);

        Self {
            client,
            bucket: HETZNER_S3_BUCKET.clone(),
        }
    }

    pub async fn upload_video_stream(
        &self,
        key: &str,
        stream: impl futures_util::Stream<Item = Result<Bytes, reqwest::Error>>,
        metadata: &HashMap<String, String>,
    ) -> Result<(), aws_sdk_s3::Error> {
        let chunks: Vec<Bytes> = stream
            .filter_map(|chunk| async move { chunk.ok() })
            .collect()
            .await;

        let body_bytes = chunks.concat();
        let metadata = metadata.clone();

        retry_s3_op("upload_video_stream", key, || {
            let body = ByteStream::from(body_bytes.clone());
            let mut request = self
                .client
                .put_object()
                .bucket(&self.bucket)
                .key(key)
                .body(body)
                .content_type("video/mp4");

            for (k, v) in &metadata {
                request = request.metadata(k, v);
            }

            async move {
                request.send().await?;
                Ok(())
            }
        })
        .await
    }

    pub async fn upload_hls_segment(
        &self,
        key: &str,
        data: Bytes,
        metadata: &HashMap<String, String>,
    ) -> Result<(), aws_sdk_s3::Error> {
        let content_type = if key.ends_with(".m3u8") {
            "application/vnd.apple.mpegurl"
        } else if key.ends_with(".ts") {
            "video/mp2t"
        } else {
            "application/octet-stream"
        };

        let metadata = metadata.clone();

        retry_s3_op("upload_hls_segment", key, || {
            let mut request = self
                .client
                .put_object()
                .bucket(&self.bucket)
                .key(key)
                .body(ByteStream::from(data.clone()))
                .content_type(content_type);

            for (k, v) in &metadata {
                request = request.metadata(k, v);
            }

            async move {
                request.send().await?;
                Ok(())
            }
        })
        .await
    }

    pub async fn download_video(&self, key: &str) -> Result<Vec<u8>, String> {
        retry_s3_op_string("download_video", key, || async {
            let resp = self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await
                .map_err(|e| e.to_string())?;

            let data = resp.body.collect().await.map_err(|e| e.to_string())?;
            Ok(data.into_bytes().to_vec())
        })
        .await
    }

    pub async fn delete_video(&self, key: &str) -> Result<(), aws_sdk_s3::Error> {
        retry_s3_op("delete_video", key, || async {
            self.client
                .delete_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await?;
            Ok(())
        })
        .await
    }

    pub async fn upload_thumbnail(
        &self,
        key: &str,
        data: Vec<u8>,
    ) -> Result<(), aws_sdk_s3::Error> {
        retry_s3_op("upload_thumbnail", key, || {
            let body = ByteStream::from(data.clone());
            async move {
                self.client
                    .put_object()
                    .bucket(&self.bucket)
                    .key(key)
                    .body(body)
                    .content_type("image/png")
                    .send()
                    .await?;
                Ok(())
            }
        })
        .await
    }

    pub async fn download_thumbnail(&self, key: &str) -> Result<Vec<u8>, String> {
        retry_s3_op_string("download_thumbnail", key, || async {
            let resp = self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await
                .map_err(|e| e.to_string())?;

            let data = resp.body.collect().await.map_err(|e| e.to_string())?;
            Ok(data.into_bytes().to_vec())
        })
        .await
    }

    pub async fn delete_thumbnail(&self, key: &str) -> Result<(), aws_sdk_s3::Error> {
        retry_s3_op("delete_thumbnail", key, || async {
            self.client
                .delete_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await?;
            Ok(())
        })
        .await
    }
}
