use axum::{body::Body, extract::State, response::IntoResponse, Json};
use http_body_util::BodyExt;
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::process::Stdio;
use storj_interface::duplicate::Args;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::breadcrumb;
use crate::consts::{ACCESS_GRANT_NSFW, ACCESS_GRANT_SFW, YRAL_NSFW_VIDEOS, YRAL_VIDEOS};
use crate::s3_client::S3Client;

// TTL for pending uploads (in hours)
const PENDING_UPLOAD_TTL_HOURS: u32 = 1;

/// Extract a thumbnail from video data using ffmpeg
/// Uses a temp file so ffmpeg can seek (pipe input fails for videos with moov atom at end)
async fn extract_thumbnail(video_data: &[u8]) -> Result<Vec<u8>, Error> {
    use std::io::Write;

    // Create temp files — NamedTempFile auto-cleans on drop
    let mut temp_input = tempfile::Builder::new()
        .suffix(".mp4")
        .tempfile()
        .map_err(Error::Io)?;
    temp_input.write_all(video_data).map_err(Error::Io)?;
    let input_path = temp_input.path().to_owned();

    let temp_output = tempfile::Builder::new()
        .suffix(".png")
        .tempfile()
        .map_err(Error::Io)?;
    let output_path = temp_output.path().to_owned();

    // Try extracting at 1 second first
    let output = Command::new("ffmpeg")
        .args([
            "-y",
            "-i",
            input_path.to_str().unwrap(),
            "-ss",
            "00:00:01",
            "-vframes",
            "1",
            "-f",
            "image2",
            output_path.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?
        .wait_with_output()
        .await?;

    // If that failed, try frame 0 for very short videos
    if !output.status.success() || !tokio::fs::try_exists(&output_path).await.unwrap_or(false) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!("ffmpeg thumbnail at 1s failed, retrying at frame 0: {stderr}");

        let output = Command::new("ffmpeg")
            .args([
                "-y",
                "-i",
                input_path.to_str().unwrap(),
                "-vframes",
                "1",
                "-f",
                "image2",
                output_path.to_str().unwrap(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?
            .wait_with_output()
            .await?;

        if !output.status.success() || !tokio::fs::try_exists(&output_path).await.unwrap_or(false) {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::error!("ffmpeg thumbnail extraction failed: {stderr}");
            return Err(Error::Io(std::io::Error::other(
                "Failed to extract thumbnail from video",
            )));
        }
    }

    let thumbnail = tokio::fs::read(&output_path).await?;

    Ok(thumbnail)
}

/// Upload a thumbnail to Storj
const UPLINK_MAX_RETRIES: u32 = 3;
const UPLINK_BASE_DELAY_MS: u64 = 500;

/// Run an uplink command with exponential backoff retries.
/// `build_command` is called on each attempt to create a fresh Command.
/// For stdin-based uploads, `stdin_data` provides the bytes to pipe in.
async fn retry_uplink_op<F>(
    operation_name: &str,
    key: &str,
    stdin_data: Option<&[u8]>,
    build_command: F,
) -> Result<(), Error>
where
    F: Fn() -> Command,
{
    let mut last_err = String::new();
    for attempt in 0..=UPLINK_MAX_RETRIES {
        let mut child = build_command()
            .stdin(if stdin_data.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;

        if let Some(data) = stdin_data {
            let mut pipe = child.stdin.take().expect("Stdin pipe to be opened");
            pipe.write_all(data).await?;
            pipe.flush().await?;
            drop(pipe);
        }

        let output = child.wait_with_output().await?;
        if output.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        if attempt == UPLINK_MAX_RETRIES {
            tracing::error!(
                operation = operation_name,
                key = key,
                attempts = attempt + 1,
                error = %stderr,
                "Uplink operation failed after all retries"
            );
            last_err = stderr.to_string();
            break;
        }

        let delay_ms = UPLINK_BASE_DELAY_MS * 2u64.pow(attempt);
        let jitter = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos()
            % 250) as u64;
        let total_delay = std::time::Duration::from_millis(delay_ms + jitter);
        tracing::warn!(
            operation = operation_name,
            key = key,
            attempt = attempt + 1,
            max_retries = UPLINK_MAX_RETRIES,
            delay_ms = total_delay.as_millis() as u64,
            error = %stderr,
            "Uplink operation failed, retrying"
        );
        tokio::time::sleep(total_delay).await;
    }
    Err(Error::Io(std::io::Error::other(format!(
        "uplink {operation_name} failed after {} retries: {last_err}",
        UPLINK_MAX_RETRIES + 1
    ))))
}

async fn upload_thumbnail_to_storj(
    publisher_user_id: &str,
    video_id: &str,
    thumbnail_data: &[u8],
    is_nsfw: bool,
) -> Result<(), Error> {
    let (bucket, grant) = if is_nsfw {
        (YRAL_NSFW_VIDEOS.as_str(), ACCESS_GRANT_NSFW.as_str())
    } else {
        (YRAL_VIDEOS.as_str(), ACCESS_GRANT_SFW.as_str())
    };
    let dest = format!("sj://{bucket}/{publisher_user_id}/{video_id}_thumbnail.png");
    let grant = grant.to_string();
    let key = format!("{publisher_user_id}/{video_id}_thumbnail.png");

    retry_uplink_op("upload_thumbnail", &key, Some(thumbnail_data), || {
        let mut cmd = Command::new("uplink");
        cmd.args([
            "cp",
            "--interactive=false",
            "--analytics=false",
            "--progress=false",
            "--access",
            &grant,
            "-",
            &dest,
        ]);
        cmd
    })
    .await
}

/// Upload a thumbnail to Storj with TTL
async fn upload_thumbnail_to_storj_with_ttl(
    publisher_user_id: &str,
    video_id: &str,
    thumbnail_data: &[u8],
    expires: &str,
    is_nsfw: bool,
) -> Result<(), Error> {
    let (bucket, grant) = if is_nsfw {
        (YRAL_NSFW_VIDEOS.as_str(), ACCESS_GRANT_NSFW.as_str())
    } else {
        (YRAL_VIDEOS.as_str(), ACCESS_GRANT_SFW.as_str())
    };
    let dest = format!("sj://{bucket}/{publisher_user_id}/{video_id}_thumbnail.png");
    let grant = grant.to_string();
    let expires = expires.to_string();
    let key = format!("{publisher_user_id}/{video_id}_thumbnail.png");

    retry_uplink_op("upload_thumbnail_ttl", &key, Some(thumbnail_data), || {
        let mut cmd = Command::new("uplink");
        cmd.args([
            "cp",
            "--interactive=false",
            "--analytics=false",
            "--progress=false",
            "--expires",
            &expires,
            "--access",
            &grant,
            "-",
            &dest,
        ]);
        cmd
    })
    .await
}

/// Upload a thumbnail to S3
async fn upload_thumbnail_to_s3(
    s3_client: &S3Client,
    publisher_user_id: &str,
    video_id: &str,
    thumbnail_data: Vec<u8>,
) -> Result<(), Error> {
    let key = format!("{publisher_user_id}/{video_id}_thumbnail.png");

    s3_client
        .upload_thumbnail(&key, thumbnail_data)
        .await
        .map_err(|e| {
            tracing::warn!("S3 thumbnail upload error for {publisher_user_id}/{video_id}: {e:?}");
            Error::S3(format!("{e:?}"))
        })?;

    Ok(())
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error(transparent)]
    Network(#[from] reqwest::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Hyper(#[from] axum::Error),

    #[error("Cloudflare returned non-ok status ({0}) when fetching the video")]
    Clouflare(StatusCode),

    #[error("S3 operation failed: {0}")]
    S3(String),
}

impl IntoResponse for Error {
    fn into_response(self) -> axum::response::Response {
        tracing::error!("request error: {self}");
        let (status, message) = match self {
            Error::Network(_) | Error::Io(_) | Error::Hyper(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error. Check server logs.",
            ),
            Error::Clouflare(StatusCode::NOT_FOUND) => (
                StatusCode::NOT_FOUND,
                "The video doesn't exist on cloudflare",
            ),
            Error::Clouflare(_) => (
                StatusCode::BAD_REQUEST,
                "The video couldn't fetched from cloudflare. Check server logs.",
            ),
            Error::S3(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "S3 storage operation failed. Check server logs.",
            ),
        };

        (
            status,
            Json(json!({
                "message": message
            })),
        )
            .into_response()
    }
}

async fn upload_to_storj(
    publisher_user_id: &str,
    video_id: &str,
    metadata: &BTreeMap<String, String>,
    video_data: &[u8],
    is_nsfw: bool,
) -> Result<(), Error> {
    let (bucket, grant) = if is_nsfw {
        (YRAL_NSFW_VIDEOS.as_str(), ACCESS_GRANT_NSFW.as_str())
    } else {
        (YRAL_VIDEOS.as_str(), ACCESS_GRANT_SFW.as_str())
    };
    let dest = format!("sj://{bucket}/{publisher_user_id}/{video_id}.mp4");
    let key = format!("{publisher_user_id}/{video_id}.mp4");
    let grant = grant.to_string();

    breadcrumb!("storj", "upload_video", key, true, "starting upload");

    let metadata_str = serde_json::to_string(metadata)
        .expect("serialization to go through as we are guaranteed utf-8");

    let result = retry_uplink_op("upload_video", &key, Some(video_data), || {
        let mut cmd = Command::new("uplink");
        cmd.args([
            "cp",
            "--interactive=false",
            "--analytics=false",
            "--progress=false",
            &format!("--metadata={metadata_str}"),
            "--access",
            &grant,
            "-",
            &dest,
        ]);
        cmd
    })
    .await;

    if result.is_err() {
        breadcrumb!("storj", "upload_video", key, false, "failed after retries");
    } else {
        breadcrumb!("storj", "upload_video", key, true, "completed");
    }
    result
}

async fn upload_to_s3(
    s3_client: &S3Client,
    publisher_user_id: &str,
    video_id: &str,
    metadata: &BTreeMap<String, String>,
    stream: impl futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>>,
) -> Result<(), Error> {
    let key = format!("{publisher_user_id}/{video_id}.mp4");

    breadcrumb!("s3", "upload_video", key, true, "starting upload");

    // Convert metadata to HashMap for S3
    let mut s3_metadata = HashMap::new();
    for (k, v) in metadata.iter() {
        s3_metadata.insert(k.clone(), v.clone());
    }

    s3_client
        .upload_video_stream(&key, stream, &s3_metadata)
        .await
        .map_err(|e| {
            tracing::warn!("S3 upload error for {publisher_user_id}/{video_id}: {e:?}");
            breadcrumb!("s3", "upload_video", key, false, format!("{e:?}"));
            Error::S3(format!("{e:?}"))
        })?;

    breadcrumb!("s3", "upload_video", key, true, "completed");
    Ok(())
}

#[tracing::instrument(skip_all)]
pub async fn handler(
    State(s3_client): State<S3Client>,
    Json(Args {
        publisher_user_id,
        video_id,
        is_nsfw,
        metadata,
    }): Json<Args>,
) -> Result<impl IntoResponse, Error> {
    let source = format!(
        "https://customer-2p3jflss4r4hmpnz.cloudflarestream.com/{video_id}/downloads/default.mp4",
    );

    // Download video from Cloudflare
    breadcrumb!("http", "download", source, true, "starting download");
    let req = reqwest::get(&source).await?;
    let status = req.status();

    if status != StatusCode::OK {
        breadcrumb!(
            "http",
            "download",
            source,
            false,
            format!("status: {}", status)
        );
        return Err(Error::Clouflare(status));
    }

    // Collect all bytes into memory to extract thumbnail and upload
    let body = req.bytes().await?;
    breadcrumb!(
        "http",
        "download",
        source,
        true,
        format!("downloaded {} bytes", body.len())
    );

    // Keep a copy for background thumbnail extraction
    let body_for_thumb = body.clone();

    // Critical path: upload videos to storage backends
    if !is_nsfw {
        // For SFW videos, upload to both Storj and S3
        let body_for_s3 = body.clone();
        let s3_stream = futures_util::stream::once(async move { Ok(body_for_s3) });

        let storj_video_upload =
            upload_to_storj(&publisher_user_id, &video_id, &metadata, &body, is_nsfw);
        let s3_video_upload = upload_to_s3(
            &s3_client,
            &publisher_user_id,
            &video_id,
            &metadata,
            s3_stream,
        );

        tokio::try_join!(storj_video_upload, s3_video_upload)?;
    } else {
        // For NSFW videos, only upload to Storj
        upload_to_storj(&publisher_user_id, &video_id, &metadata, &body, is_nsfw).await?;
    }

    // Background: extract thumbnail and upload it (don't block the response)
    let publisher = publisher_user_id.clone();
    let vid = video_id.clone();

    tokio::spawn(async move {
        let thumbnail_data = match extract_thumbnail(&body_for_thumb).await {
            Ok(data) => data,
            Err(e) => {
                tracing::error!(
                    "Background thumbnail extraction failed for {publisher}/{vid}: {e}"
                );
                return;
            }
        };

        if !is_nsfw {
            let storj_thumb = upload_thumbnail_to_storj(&publisher, &vid, &thumbnail_data, is_nsfw);
            let s3_thumb =
                upload_thumbnail_to_s3(&s3_client, &publisher, &vid, thumbnail_data.clone());
            if let Err(e) = tokio::try_join!(storj_thumb, s3_thumb) {
                tracing::error!("Background thumbnail upload failed for {publisher}/{vid}: {e}");
            }
        } else if let Err(e) =
            upload_thumbnail_to_storj(&publisher, &vid, &thumbnail_data, is_nsfw).await
        {
            tracing::error!("Background thumbnail upload failed for {publisher}/{vid}: {e}");
        }
    });

    Ok(())
}

#[derive(Deserialize)]
pub struct RawUploadInitialParams {
    publisher_user_id: String,
    video_id: String,
    is_nsfw: bool,
}

#[derive(Deserialize)]
pub struct RawFinalizeParams {
    publisher_user_id: String,
    video_id: String,
    is_nsfw: bool,
}

#[derive(Deserialize)]
pub struct RawFinalizeBody {
    #[serde(default)]
    metadata: BTreeMap<String, String>,
}

#[tracing::instrument(skip_all)]
pub async fn handler_raw_upload_initial(
    State(s3_client): State<S3Client>,
    axum::extract::Query(params): axum::extract::Query<RawUploadInitialParams>,
    body: Body,
) -> Result<impl IntoResponse, Error> {
    // Collect the body data
    let body_data = body.collect().await.map_err(Error::Hyper)?.to_bytes();

    let mut pending_metadata = BTreeMap::new();
    pending_metadata.insert("_pending".to_string(), "true".to_string());
    pending_metadata.insert("_uploaded_at".to_string(), chrono::Utc::now().to_rfc3339());

    let expires = format!("+{}h", PENDING_UPLOAD_TTL_HOURS);

    // Background ALL uploads — return 200 immediately after receiving the body.
    // Finalize handler retries Storj downloads, so it naturally waits if upload isn't done yet.
    let publisher = params.publisher_user_id.clone();
    let video_id = params.video_id.clone();
    let is_nsfw = params.is_nsfw;
    let metadata = pending_metadata.clone();

    tokio::spawn(async move {
        // Upload video to Storj (with TTL) — if this fails after all retries, skip the rest
        if let Err(e) = upload_to_storj_with_ttl(
            &publisher, &video_id, &metadata, &body_data, &expires, is_nsfw,
        )
        .await
        {
            tracing::error!("Background Storj video upload failed for {publisher}/{video_id}: {e}");
            return;
        }

        // Extract thumbnail
        let thumbnail_data = match extract_thumbnail(&body_data).await {
            Ok(data) => Some(data),
            Err(e) => {
                tracing::error!(
                    "Background thumbnail extraction failed for {publisher}/{video_id}: {e}"
                );
                None
            }
        };

        // Upload thumbnail to Storj if extracted
        if let Some(ref thumb) = thumbnail_data {
            if let Err(e) =
                upload_thumbnail_to_storj_with_ttl(&publisher, &video_id, thumb, &expires, is_nsfw)
                    .await
            {
                tracing::error!(
                    "Background Storj thumbnail upload failed for {publisher}/{video_id}: {e}"
                );
            }
        }

        // S3 uploads for SFW videos (finalize re-uploads, so failure here is non-fatal)
        if !is_nsfw {
            let s3_stream =
                futures_util::stream::once(async { Ok::<_, reqwest::Error>(body_data.clone()) });
            if let Err(e) =
                upload_to_s3(&s3_client, &publisher, &video_id, &metadata, s3_stream).await
            {
                tracing::error!(
                    "Background S3 video upload failed for {publisher}/{video_id}: {e}"
                );
            }

            if let Some(thumb) = thumbnail_data {
                if let Err(e) =
                    upload_thumbnail_to_s3(&s3_client, &publisher, &video_id, thumb).await
                {
                    tracing::error!(
                        "Background S3 thumbnail upload failed for {publisher}/{video_id}: {e}"
                    );
                }
            }
        }
    });

    Ok(Json(json!({
        "status": "pending",
        "expires_in_hours": PENDING_UPLOAD_TTL_HOURS,
        "message": "Video uploaded successfully. Call /duplicate_raw/finalize to complete the upload."
    })))
}

#[tracing::instrument(skip_all)]
pub async fn handler_raw_finalize(
    State(s3_client): State<S3Client>,
    axum::extract::Query(params): axum::extract::Query<RawFinalizeParams>,
    Json(body): Json<RawFinalizeBody>,
) -> Result<impl IntoResponse, Error> {
    let metadata = body.metadata;

    let (bucket, grant) = if params.is_nsfw {
        (YRAL_NSFW_VIDEOS.as_str(), ACCESS_GRANT_NSFW.as_str())
    } else {
        (YRAL_VIDEOS.as_str(), ACCESS_GRANT_SFW.as_str())
    };

    let src_video_path = format!(
        "sj://{}/{}/{}.mp4",
        bucket, params.publisher_user_id, params.video_id
    );
    let src_thumbnail_path = format!(
        "sj://{}/{}/{}_thumbnail.png",
        bucket, params.publisher_user_id, params.video_id
    );

    // Download video and thumbnail from Storj with retries
    let temp_video_file = format!(
        "/tmp/storj-finalize-{}-{}.mp4",
        params.publisher_user_id, params.video_id
    );
    let temp_thumbnail_file = format!(
        "/tmp/storj-finalize-{}-{}_thumbnail.png",
        params.publisher_user_id, params.video_id
    );

    let grant = grant.to_string();
    let video_key = format!("{}/{}.mp4", params.publisher_user_id, params.video_id);
    let thumb_key = format!(
        "{}/{}_thumbnail.png",
        params.publisher_user_id, params.video_id
    );

    retry_uplink_op("download_video", &video_key, None, || {
        let mut cmd = Command::new("uplink");
        cmd.args([
            "cp",
            "--interactive=false",
            "--analytics=false",
            "--progress=false",
            "--access",
            &grant,
            &src_video_path,
            &temp_video_file,
        ]);
        cmd
    })
    .await?;

    retry_uplink_op("download_thumbnail", &thumb_key, None, || {
        let mut cmd = Command::new("uplink");
        cmd.args([
            "cp",
            "--interactive=false",
            "--analytics=false",
            "--progress=false",
            "--access",
            &grant,
            &src_thumbnail_path,
            &temp_thumbnail_file,
        ]);
        cmd
    })
    .await?;

    // Read the file data
    let file_data = tokio::fs::read(&temp_video_file).await?;
    let thumbnail_data = tokio::fs::read(&temp_thumbnail_file).await?;

    // Re-upload with final metadata (no TTL)
    if !params.is_nsfw {
        // For SFW videos, upload to both Storj and S3
        let file_data_for_s3 = file_data.clone();
        let s3_stream =
            futures_util::stream::once(
                async move { Ok::<_, reqwest::Error>(file_data_for_s3.into()) },
            );

        let storj_video_upload = upload_to_storj(
            &params.publisher_user_id,
            &params.video_id,
            &metadata,
            &file_data,
            params.is_nsfw,
        );

        let storj_thumbnail_upload = upload_thumbnail_to_storj(
            &params.publisher_user_id,
            &params.video_id,
            &thumbnail_data,
            params.is_nsfw,
        );

        let s3_video_upload = upload_to_s3(
            &s3_client,
            &params.publisher_user_id,
            &params.video_id,
            &metadata,
            s3_stream,
        );

        let s3_thumbnail_upload = upload_thumbnail_to_s3(
            &s3_client,
            &params.publisher_user_id,
            &params.video_id,
            thumbnail_data.clone(),
        );

        tokio::try_join!(
            storj_video_upload,
            storj_thumbnail_upload,
            s3_video_upload,
            s3_thumbnail_upload
        )?;
    } else {
        // For NSFW videos, only upload to Storj
        let storj_video_upload = upload_to_storj(
            &params.publisher_user_id,
            &params.video_id,
            &metadata,
            &file_data,
            params.is_nsfw,
        );

        let storj_thumbnail_upload = upload_thumbnail_to_storj(
            &params.publisher_user_id,
            &params.video_id,
            &thumbnail_data,
            params.is_nsfw,
        );

        tokio::try_join!(storj_video_upload, storj_thumbnail_upload)?;
    }

    // Clean up temp files
    tokio::fs::remove_file(&temp_video_file).await.ok();
    tokio::fs::remove_file(&temp_thumbnail_file).await.ok();

    Ok(Json(json!({
        "status": "completed",
        "message": "Video finalized successfully with metadata."
    })))
}

async fn upload_to_storj_with_ttl(
    publisher_user_id: &str,
    video_id: &str,
    metadata: &BTreeMap<String, String>,
    body_data: &[u8],
    expires: &str,
    is_nsfw: bool,
) -> Result<(), Error> {
    let (bucket, grant) = if is_nsfw {
        (YRAL_NSFW_VIDEOS.as_str(), ACCESS_GRANT_NSFW.as_str())
    } else {
        (YRAL_VIDEOS.as_str(), ACCESS_GRANT_SFW.as_str())
    };
    let dest = format!("sj://{bucket}/{publisher_user_id}/{video_id}.mp4");
    let grant = grant.to_string();
    let expires = expires.to_string();
    let key = format!("{publisher_user_id}/{video_id}.mp4");

    let metadata_str = serde_json::to_string(metadata)
        .expect("serialization to go through as we are guaranteed utf-8");

    retry_uplink_op("upload_video_ttl", &key, Some(body_data), || {
        let mut cmd = Command::new("uplink");
        cmd.args([
            "cp",
            "--interactive=false",
            "--analytics=false",
            "--progress=false",
            &format!("--metadata={metadata_str}"),
            "--expires",
            &expires,
            "--access",
            &grant,
            "-",
            &dest,
        ]);
        cmd
    })
    .await
}
