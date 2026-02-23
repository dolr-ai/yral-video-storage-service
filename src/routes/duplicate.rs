use axum::{body::Body, extract::State, response::IntoResponse, Json};
use futures_util::StreamExt;
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
/// Returns the PNG thumbnail bytes
async fn extract_thumbnail(video_data: &[u8]) -> Result<Vec<u8>, Error> {
    let mut child = Command::new("ffmpeg")
        .args([
            "-i",
            "pipe:0", // Read from stdin
            "-ss",
            "00:00:01", // Seek to 1 second (or first frame if shorter)
            "-vframes",
            "1", // Extract 1 frame
            "-f",
            "image2pipe", // Output as image pipe
            "-vcodec",
            "png",    // PNG format
            "pipe:1", // Write to stdout
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let mut stdin = child.stdin.take().expect("Stdin pipe to be opened");
    let video_data_clone = video_data.to_vec();

    // Write video data to stdin in a separate task to avoid deadlock
    let write_task = tokio::spawn(async move {
        stdin.write_all(&video_data_clone).await.ok();
        drop(stdin);
    });

    let output = child.wait_with_output().await?;
    write_task.await.ok();

    if !output.status.success() || output.stdout.is_empty() {
        // Try again with frame 0 for very short videos
        let mut child = Command::new("ffmpeg")
            .args([
                "-i",
                "pipe:0",
                "-vframes",
                "1",
                "-f",
                "image2pipe",
                "-vcodec",
                "png",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;

        let mut stdin = child.stdin.take().expect("Stdin pipe to be opened");
        let video_data_clone = video_data.to_vec();

        let write_task = tokio::spawn(async move {
            stdin.write_all(&video_data_clone).await.ok();
            drop(stdin);
        });

        let output = child.wait_with_output().await?;
        write_task.await.ok();

        if output.stdout.is_empty() {
            return Err(Error::Io(std::io::Error::other(
                "Failed to extract thumbnail from video",
            )));
        }
        return Ok(output.stdout);
    }

    Ok(output.stdout)
}

/// Upload a thumbnail to Storj
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

    let mut child = Command::new("uplink")
        .args([
            "cp",
            "--interactive=false",
            "--analytics=false",
            "--progress=false",
            "--access",
            grant,
            "-",
            dest.as_str(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    let mut pipe = child.stdin.take().expect("Stdin pipe to be opened for us");
    pipe.write_all(thumbnail_data).await?;
    pipe.flush().await?;
    drop(pipe);

    let status = child.wait().await?;
    if !status.success() {
        return Err(Error::Io(std::io::Error::other(format!(
            "uplink command failed with status: {status}"
        ))));
    }

    Ok(())
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

    let mut child = Command::new("uplink")
        .args([
            "cp",
            "--interactive=false",
            "--analytics=false",
            "--progress=false",
            "--expires",
            expires,
            "--access",
            grant,
            "-",
            dest.as_str(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    let mut pipe = child.stdin.take().expect("Stdin pipe to be opened for us");
    pipe.write_all(thumbnail_data).await?;
    pipe.flush().await?;
    drop(pipe);

    let status = child.wait().await?;
    if !status.success() {
        return Err(Error::Io(std::io::Error::other(format!(
            "uplink command failed with status: {status}"
        ))));
    }

    Ok(())
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
    mut stream: impl futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
    is_nsfw: bool,
) -> Result<(), Error> {
    let (bucket, grant) = if is_nsfw {
        (YRAL_NSFW_VIDEOS.as_str(), ACCESS_GRANT_NSFW.as_str())
    } else {
        (YRAL_VIDEOS.as_str(), ACCESS_GRANT_SFW.as_str())
    };
    let dest = format!("sj://{bucket}/{publisher_user_id}/{video_id}.mp4");
    let key = format!("{publisher_user_id}/{video_id}.mp4");

    breadcrumb!("storj", "upload_video", key, true, "starting upload");

    let metadata_str = serde_json::to_string(metadata)
        .expect("serialization to go through as we are guaranteed utf-8");

    let mut child = Command::new("uplink")
        .args([
            "cp",
            "--interactive=false",
            "--analytics=false",
            "--progress=false",
            format!("--metadata={metadata_str}").as_str(),
            "--access",
            grant,
            "-",
            dest.as_str(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()?;

    let mut pipe = child.stdin.take().expect("Stdin pipe to be opened for us");

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        pipe.write_all(&chunk).await?;
    }

    drop(pipe);
    let status = child.wait().await?;
    if !status.success() {
        breadcrumb!(
            "storj",
            "upload_video",
            key,
            false,
            format!("status: {}", status)
        );
        return Err(Error::Io(std::io::Error::other(format!(
            "uplink command failed with status: {status}"
        ))));
    }

    breadcrumb!("storj", "upload_video", key, true, "completed");
    Ok(())
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

    // Extract thumbnail from video
    breadcrumb!(
        "ffmpeg",
        "extract_thumbnail",
        video_id,
        true,
        "starting extraction"
    );
    let thumbnail_data = match extract_thumbnail(&body).await {
        Ok(data) => {
            breadcrumb!(
                "ffmpeg",
                "extract_thumbnail",
                video_id,
                true,
                format!("extracted {} bytes", data.len())
            );
            data
        }
        Err(e) => {
            breadcrumb!(
                "ffmpeg",
                "extract_thumbnail",
                video_id,
                false,
                format!("{}", e)
            );
            return Err(e);
        }
    };

    if !is_nsfw {
        // For SFW videos, upload to both Storj and S3
        let body_clone = body.clone();
        let thumbnail_clone = thumbnail_data.clone();

        // Create streams from the collected bytes
        let storj_stream = futures_util::stream::once(async move { Ok(body) });
        let s3_stream = futures_util::stream::once(async move { Ok(body_clone) });

        let storj_video_upload = upload_to_storj(
            &publisher_user_id,
            &video_id,
            &metadata,
            Box::pin(storj_stream),
            is_nsfw,
        );
        let storj_thumbnail_upload =
            upload_thumbnail_to_storj(&publisher_user_id, &video_id, &thumbnail_data, is_nsfw);
        let s3_video_upload = upload_to_s3(
            &s3_client,
            &publisher_user_id,
            &video_id,
            &metadata,
            s3_stream,
        );
        let s3_thumbnail_upload =
            upload_thumbnail_to_s3(&s3_client, &publisher_user_id, &video_id, thumbnail_clone);

        tokio::try_join!(
            storj_video_upload,
            storj_thumbnail_upload,
            s3_video_upload,
            s3_thumbnail_upload
        )?;
    } else {
        // For NSFW videos, only upload to Storj
        let storj_stream = futures_util::stream::once(async move { Ok::<_, reqwest::Error>(body) });

        let storj_video_upload = upload_to_storj(
            &publisher_user_id,
            &video_id,
            &metadata,
            Box::pin(storj_stream),
            is_nsfw,
        );
        let storj_thumbnail_upload =
            upload_thumbnail_to_storj(&publisher_user_id, &video_id, &thumbnail_data, is_nsfw);

        tokio::try_join!(storj_video_upload, storj_thumbnail_upload)?;
    }

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

    // Extract thumbnail from video
    let thumbnail_data = extract_thumbnail(&body_data).await?;

    let mut pending_metadata = BTreeMap::new();
    pending_metadata.insert("_pending".to_string(), "true".to_string());
    pending_metadata.insert("_uploaded_at".to_string(), chrono::Utc::now().to_rfc3339());

    let expires = format!("+{}h", PENDING_UPLOAD_TTL_HOURS);

    if !params.is_nsfw {
        // For SFW videos, upload to both Storj (with TTL) and S3 (without TTL)
        let body_clone = body_data.clone();
        let thumbnail_clone = thumbnail_data.clone();

        let storj_video_upload = upload_to_storj_with_ttl(
            &params.publisher_user_id,
            &params.video_id,
            &pending_metadata,
            &body_data,
            &expires,
            params.is_nsfw,
        );

        let storj_thumbnail_upload = upload_thumbnail_to_storj_with_ttl(
            &params.publisher_user_id,
            &params.video_id,
            &thumbnail_data,
            &expires,
            params.is_nsfw,
        );

        let s3_stream = futures_util::stream::once(async move { Ok(body_clone) });
        let s3_video_upload = upload_to_s3(
            &s3_client,
            &params.publisher_user_id,
            &params.video_id,
            &pending_metadata,
            s3_stream,
        );

        let s3_thumbnail_upload = upload_thumbnail_to_s3(
            &s3_client,
            &params.publisher_user_id,
            &params.video_id,
            thumbnail_clone,
        );

        tokio::try_join!(
            storj_video_upload,
            storj_thumbnail_upload,
            s3_video_upload,
            s3_thumbnail_upload
        )?;
    } else {
        let storj_video_upload = upload_to_storj_with_ttl(
            &params.publisher_user_id,
            &params.video_id,
            &pending_metadata,
            &body_data,
            &expires,
            params.is_nsfw,
        );

        let storj_thumbnail_upload = upload_thumbnail_to_storj_with_ttl(
            &params.publisher_user_id,
            &params.video_id,
            &thumbnail_data,
            &expires,
            params.is_nsfw,
        );

        tokio::try_join!(storj_video_upload, storj_thumbnail_upload)?;
    }

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

    // Download video to temporary file with retry logic
    let temp_video_file = format!(
        "/tmp/storj-finalize-{}-{}.mp4",
        params.publisher_user_id, params.video_id
    );
    let temp_thumbnail_file = format!(
        "/tmp/storj-finalize-{}-{}_thumbnail.png",
        params.publisher_user_id, params.video_id
    );

    let max_retries = 3;
    let retry_delay = std::time::Duration::from_secs(2);

    // Download video
    let mut last_error = String::new();
    for attempt in 1..=max_retries {
        let download_child = Command::new("uplink")
            .args([
                "cp",
                "--interactive=false",
                "--analytics=false",
                "--progress=false",
                "--access",
                grant,
                src_video_path.as_str(),
                temp_video_file.as_str(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;

        let output = download_child.wait_with_output().await?;

        if output.status.success() {
            break;
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        last_error = stderr.to_string();
        tracing::warn!(
            "Uplink video download attempt {}/{} failed: {}",
            attempt,
            max_retries,
            stderr
        );

        if attempt < max_retries {
            tokio::time::sleep(retry_delay).await;
        }
    }

    // Check if video file exists after retries
    if !tokio::fs::try_exists(&temp_video_file)
        .await
        .unwrap_or(false)
    {
        return Err(Error::Io(std::io::Error::other(format!(
            "Failed to download video from Storj after {} retries. Last error: {}",
            max_retries, last_error
        ))));
    }

    // Download thumbnail
    let mut last_error = String::new();
    for attempt in 1..=max_retries {
        let download_child = Command::new("uplink")
            .args([
                "cp",
                "--interactive=false",
                "--analytics=false",
                "--progress=false",
                "--access",
                grant,
                src_thumbnail_path.as_str(),
                temp_thumbnail_file.as_str(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;

        let output = download_child.wait_with_output().await?;

        if output.status.success() {
            break;
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        last_error = stderr.to_string();
        tracing::warn!(
            "Uplink thumbnail download attempt {}/{} failed: {}",
            attempt,
            max_retries,
            stderr
        );

        if attempt < max_retries {
            tokio::time::sleep(retry_delay).await;
        }
    }

    // Check if thumbnail file exists after retries
    if !tokio::fs::try_exists(&temp_thumbnail_file)
        .await
        .unwrap_or(false)
    {
        return Err(Error::Io(std::io::Error::other(format!(
            "Failed to download thumbnail from Storj after {} retries. Last error: {}",
            max_retries, last_error
        ))));
    }

    // Read the file data
    let file_data = tokio::fs::read(&temp_video_file).await?;
    let thumbnail_data = tokio::fs::read(&temp_thumbnail_file).await?;

    // Re-upload with final metadata (no TTL)
    if !params.is_nsfw {
        // For SFW videos, upload to both Storj and S3
        let file_data_clone = file_data.clone();
        let thumbnail_clone = thumbnail_data.clone();

        let storj_stream =
            futures_util::stream::once(async move { Ok::<_, reqwest::Error>(file_data.into()) });
        let s3_stream =
            futures_util::stream::once(
                async move { Ok::<_, reqwest::Error>(file_data_clone.into()) },
            );

        let storj_video_upload = upload_to_storj(
            &params.publisher_user_id,
            &params.video_id,
            &metadata,
            Box::pin(storj_stream),
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
            thumbnail_clone,
        );

        tokio::try_join!(
            storj_video_upload,
            storj_thumbnail_upload,
            s3_video_upload,
            s3_thumbnail_upload
        )?;
    } else {
        // For NSFW videos, only upload to Storj
        let storj_stream =
            futures_util::stream::once(async move { Ok::<_, reqwest::Error>(file_data.into()) });

        let storj_video_upload = upload_to_storj(
            &params.publisher_user_id,
            &params.video_id,
            &metadata,
            Box::pin(storj_stream),
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

    let metadata_str = serde_json::to_string(metadata)
        .expect("serialization to go through as we are guaranteed utf-8");

    let mut child = Command::new("uplink")
        .args([
            "cp",
            "--interactive=false",
            "--analytics=false",
            "--progress=false",
            format!("--metadata={metadata_str}").as_str(),
            "--expires",
            expires,
            "--access",
            grant,
            "-",
            dest.as_str(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    let mut pipe = child.stdin.take().expect("Stdin pipe to be opened for us");
    pipe.write_all(body_data).await?;
    pipe.flush().await?;
    drop(pipe);

    let status = child.wait().await?;
    if !status.success() {
        return Err(Error::Io(std::io::Error::other(format!(
            "uplink command failed with status: {status}"
        ))));
    }

    Ok(())
}
