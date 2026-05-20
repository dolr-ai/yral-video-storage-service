use axum::extract::{Multipart, State};
use axum::response::IntoResponse;
use axum::Json;
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::breadcrumb;
use crate::consts::{ACCESS_GRANT_NSFW, MIRROR_ACCESS_GRANT, STORJ_SFW_BUCKET, YRAL_NSFW_VIDEOS};
use crate::s3_client::S3Client;
use crate::AppState;

// TTL for pending uploads (in hours)
const PENDING_UPLOAD_TTL_HOURS: u32 = 1;

/// Extract a thumbnail from video data using ffmpeg
/// Uses a temp file so ffmpeg can seek (pipe input fails for videos with moov atom at end)
pub async fn extract_thumbnail(video_data: &[u8]) -> Result<Vec<u8>, Error> {
    crate::thumbnail::extract_thumbnail(video_data)
        .await
        .map_err(Error::Io)
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
        (STORJ_SFW_BUCKET.as_str(), MIRROR_ACCESS_GRANT.as_str())
    };
    let dest_under = format!("sj://{bucket}/{publisher_user_id}/{video_id}_thumbnail.png");
    let dest_dash = format!("sj://{bucket}/{publisher_user_id}/{video_id}-thumbnail.png");

    let mut child_under = Command::new("uplink")
        .args([
            "cp",
            "--interactive=false",
            "--analytics=false",
            "--progress=false",
            "--access",
            grant,
            "-",
            dest_under.as_str(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut child_dash = Command::new("uplink")
        .args([
            "cp",
            "--interactive=false",
            "--analytics=false",
            "--progress=false",
            "--access",
            grant,
            "-",
            dest_dash.as_str(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut pipe_under = child_under
        .stdin
        .take()
        .expect("Stdin pipe to be opened for us");
    let mut pipe_dash = child_dash
        .stdin
        .take()
        .expect("Stdin pipe to be opened for us");

    tokio::try_join!(
        async move {
            pipe_under.write_all(thumbnail_data).await?;
            pipe_under.flush().await
        },
        async move {
            pipe_dash.write_all(thumbnail_data).await?;
            pipe_dash.flush().await
        }
    )?;

    let (out_under, out_dash) = tokio::join!(
        child_under.wait_with_output(),
        child_dash.wait_with_output(),
    );

    for (out, dest) in [
        (out_under.map_err(Error::Io)?, &dest_under),
        (out_dash.map_err(Error::Io)?, &dest_dash),
    ] {
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(Error::Io(std::io::Error::other(format!(
                "uplink command failed for {dest} with status: {} — {stderr}",
                out.status
            ))));
        }
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
        (STORJ_SFW_BUCKET.as_str(), MIRROR_ACCESS_GRANT.as_str())
    };
    let dest_under = format!("sj://{bucket}/{publisher_user_id}/{video_id}_thumbnail.png");
    let dest_dash = format!("sj://{bucket}/{publisher_user_id}/{video_id}-thumbnail.png");

    let mut child_under = Command::new("uplink")
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
            dest_under.as_str(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut child_dash = Command::new("uplink")
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
            dest_dash.as_str(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut pipe_under = child_under
        .stdin
        .take()
        .expect("Stdin pipe to be opened for us");
    let mut pipe_dash = child_dash
        .stdin
        .take()
        .expect("Stdin pipe to be opened for us");

    tokio::try_join!(
        async move {
            pipe_under.write_all(thumbnail_data).await?;
            pipe_under.flush().await
        },
        async move {
            pipe_dash.write_all(thumbnail_data).await?;
            pipe_dash.flush().await
        }
    )?;

    let (out_under, out_dash) = tokio::join!(
        child_under.wait_with_output(),
        child_dash.wait_with_output(),
    );

    for (out, dest) in [
        (out_under.map_err(Error::Io)?, &dest_under),
        (out_dash.map_err(Error::Io)?, &dest_dash),
    ] {
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(Error::Io(std::io::Error::other(format!(
                "uplink command failed for {dest} with status: {} — {stderr}",
                out.status
            ))));
        }
    }

    Ok(())
}

/// Upload a thumbnail to S3
async fn upload_thumbnail_to_s3(
    s3_client: &S3Client,
    publisher_user_id: &str,
    video_id: &str,
    thumbnail_data: &[u8],
) -> Result<(), Error> {
    let key_under = format!("{publisher_user_id}/{video_id}_thumbnail.png");
    let key_dash = format!("{publisher_user_id}/{video_id}-thumbnail.png");

    let (r_under, r_dash) = tokio::join!(
        s3_client.upload_thumbnail(&key_under, thumbnail_data.to_vec()),
        s3_client.upload_thumbnail(&key_dash, thumbnail_data.to_vec()),
    );

    for r in [r_under, r_dash] {
        r.map_err(|e| {
            tracing::warn!("S3 thumbnail upload error for {publisher_user_id}/{video_id}: {e:?}");
            Error::S3(format!("{e:?}"))
        })?;
    }

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
        (STORJ_SFW_BUCKET.as_str(), MIRROR_ACCESS_GRANT.as_str())
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
    pipe.write_all(video_data).await?;
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
    video_data: &[u8],
) -> Result<(), Error> {
    let key = format!("{publisher_user_id}/{video_id}.mp4");

    breadcrumb!("s3", "upload_video", key, true, "starting upload");

    let mut s3_metadata = HashMap::new();
    for (k, v) in metadata.iter() {
        s3_metadata.insert(k.clone(), v.clone());
    }

    s3_client
        .upload_video(&key, video_data, &s3_metadata)
        .await
        .map_err(|e| {
            tracing::warn!("S3 upload error for {publisher_user_id}/{video_id}: {e:?}");
            breadcrumb!("s3", "upload_video", key, false, format!("{e:?}"));
            Error::S3(format!("{e:?}"))
        })?;

    breadcrumb!("s3", "upload_video", key, true, "completed");
    Ok(())
}

#[derive(Deserialize, utoipa::IntoParams)]
pub struct RawUploadInitialParams {
    publisher_user_id: String,
    video_id: String,
    is_nsfw: bool,
}

#[derive(Deserialize, utoipa::IntoParams)]
pub struct RawFinalizeParams {
    publisher_user_id: String,
    video_id: String,
    is_nsfw: bool,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct RawFinalizeBody {
    #[serde(default)]
    metadata: BTreeMap<String, String>,
}

#[utoipa::path(
    post,
    path = "/duplicate_raw/upload",
    tag = "videos",
    params(RawUploadInitialParams),
    request_body(content = Vec<u8>, content_type = "multipart/form-data", description = "Video file in 'file' field (max 500MB)"),
    responses(
        (status = 200, description = "Video uploaded to staging area"),
        (status = 500, description = "Internal server error"),
    )
)]
#[tracing::instrument(skip_all)]
pub async fn handler_raw_upload_initial(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<RawUploadInitialParams>,
    mut multipart: Multipart,
) -> Result<impl IntoResponse, Error> {
    let s3_client = &state.s3_client;
    // Extract the "file" field from the multipart upload
    let mut file_data = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| Error::Io(std::io::Error::other(format!("multipart field error: {e}"))))?
    {
        if field.name() == Some("file") {
            file_data = Some(field.bytes().await.map_err(|e| {
                Error::Io(std::io::Error::other(format!("multipart read error: {e}")))
            })?);
            break;
        }
    }
    let body_data = file_data.ok_or_else(|| {
        Error::Io(std::io::Error::other(
            "missing 'file' field in multipart upload",
        ))
    })?;

    // Extract thumbnail from video
    let thumbnail_data = extract_thumbnail(&body_data).await?;

    let mut pending_metadata = BTreeMap::new();
    pending_metadata.insert("_pending".to_string(), "true".to_string());
    pending_metadata.insert("_uploaded_at".to_string(), chrono::Utc::now().to_rfc3339());

    let expires = format!("+{}h", PENDING_UPLOAD_TTL_HOURS);

    if !params.is_nsfw {
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
        let s3_video_upload = upload_to_s3(
            s3_client,
            &params.publisher_user_id,
            &params.video_id,
            &pending_metadata,
            &body_data,
        );
        let s3_thumbnail_upload = upload_thumbnail_to_s3(
            s3_client,
            &params.publisher_user_id,
            &params.video_id,
            &thumbnail_data,
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

#[utoipa::path(
    post,
    path = "/duplicate_raw/finalize",
    tag = "videos",
    params(RawFinalizeParams),
    request_body = RawFinalizeBody,
    responses(
        (status = 200, description = "Video finalized and pushed to Storj"),
        (status = 500, description = "Internal server error"),
    )
)]
#[tracing::instrument(skip_all)]
pub async fn handler_raw_finalize(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<RawFinalizeParams>,
    Json(body): Json<RawFinalizeBody>,
) -> Result<impl IntoResponse, Error> {
    let s3_client = &state.s3_client;
    let metadata = body.metadata;

    let (bucket, grant) = if params.is_nsfw {
        (YRAL_NSFW_VIDEOS.as_str(), ACCESS_GRANT_NSFW.as_str())
    } else {
        (STORJ_SFW_BUCKET.as_str(), MIRROR_ACCESS_GRANT.as_str())
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
            s3_client,
            &params.publisher_user_id,
            &params.video_id,
            &metadata,
            &file_data,
        );
        let s3_thumbnail_upload = upload_thumbnail_to_s3(
            s3_client,
            &params.publisher_user_id,
            &params.video_id,
            &thumbnail_data,
        );

        tokio::try_join!(
            storj_video_upload,
            storj_thumbnail_upload,
            s3_video_upload,
            s3_thumbnail_upload
        )?;
    } else {
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
        (STORJ_SFW_BUCKET.as_str(), MIRROR_ACCESS_GRANT.as_str())
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

#[cfg(test)]
mod tests {
    use super::extract_thumbnail;
    use std::path::Path;
    use std::process::Stdio;
    use tempfile::tempdir;
    use tokio::fs;
    use tokio::process::Command;

    #[tokio::test]
    #[ignore = "requires ffmpeg installed locally"]
    async fn extract_thumbnail_uses_the_first_video_frame() {
        let temp_dir = tempdir().expect("temp dir");
        let video_path = temp_dir.path().join("two-frame.mp4");

        create_two_frame_video(&video_path)
            .await
            .expect("create test video");

        let video_data = fs::read(&video_path).await.expect("read test video");
        let thumbnail = extract_thumbnail(&video_data)
            .await
            .expect("extract thumbnail");

        let rgb = decode_png_to_rgb24(temp_dir.path(), &thumbnail)
            .await
            .expect("decode thumbnail");

        let (avg_r, avg_g, avg_b) = average_rgb(&rgb);
        assert!(
            avg_r > 200 && avg_g < 40 && avg_b < 40,
            "expected a red first frame, got average rgb ({avg_r}, {avg_g}, {avg_b})"
        );
    }

    async fn create_two_frame_video(output_path: &Path) -> Result<(), String> {
        let output = Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=16x16:d=1:r=1",
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:s=16x16:d=1:r=1",
                "-filter_complex",
                "[0:v][1:v]concat=n=2:v=1:a=0,format=yuv420p",
                output_path
                    .to_str()
                    .expect("test video path should be valid utf-8"),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| format!("failed to spawn ffmpeg: {err}"))?
            .wait_with_output()
            .await
            .map_err(|err| format!("failed to wait for ffmpeg: {err}"))?;

        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).into_owned())
        }
    }

    async fn decode_png_to_rgb24(temp_dir: &Path, png_data: &[u8]) -> Result<Vec<u8>, String> {
        let png_path = temp_dir.join("thumbnail.png");
        let rgb_path = temp_dir.join("thumbnail.rgb");

        fs::write(&png_path, png_data)
            .await
            .map_err(|err| format!("failed to write thumbnail: {err}"))?;

        let output = Command::new("ffmpeg")
            .args([
                "-y",
                "-i",
                png_path
                    .to_str()
                    .expect("test png path should be valid utf-8"),
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgb24",
                rgb_path
                    .to_str()
                    .expect("test rgb path should be valid utf-8"),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| format!("failed to spawn ffmpeg: {err}"))?
            .wait_with_output()
            .await
            .map_err(|err| format!("failed to wait for ffmpeg: {err}"))?;

        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).into_owned());
        }

        fs::read(&rgb_path)
            .await
            .map_err(|err| format!("failed to read rgb output: {err}"))
    }

    fn average_rgb(rgb: &[u8]) -> (u8, u8, u8) {
        assert_eq!(rgb.len() % 3, 0, "rgb24 data should be 3 bytes per pixel");

        let pixel_count = (rgb.len() / 3) as u32;
        let (sum_r, sum_g, sum_b) =
            rgb.chunks_exact(3)
                .fold((0u32, 0u32, 0u32), |(r, g, b), pixel| {
                    (
                        r + pixel[0] as u32,
                        g + pixel[1] as u32,
                        b + pixel[2] as u32,
                    )
                });

        (
            (sum_r / pixel_count) as u8,
            (sum_g / pixel_count) as u8,
            (sum_b / pixel_count) as u8,
        )
    }
}

/// Integration tests for dual-name thumbnail uploads.
///
/// Storj:
///   STORJ_ACCESS_GRANT=... TEST_BUCKET=... \
///   cargo test -p storj-interface --bin storj-interface -- --ignored storj_both_thumbnail_names
///
/// Hetzner S3:
///   HETZNER_S3_ENDPOINT=... HETZNER_S3_BUCKET=... \
///   HETZNER_S3_ACCESS_KEY=... HETZNER_S3_SECRET_KEY=... HETZNER_S3_REGION=... \
///   cargo test -p storj-interface --bin storj-interface -- --ignored hetzner_both_thumbnail_names
#[cfg(test)]
mod integration_tests {
    use super::{upload_thumbnail_to_s3, upload_thumbnail_to_storj};
    use crate::s3_client::S3Client;
    use tokio::process::Command;

    /// Generate a real 16×16 red PNG using ffmpeg (stdout).
    async fn generate_test_png() -> Vec<u8> {
        let out = Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=16x16:d=1:r=1",
                "-vframes",
                "1",
                "-f",
                "image2pipe",
                "-vcodec",
                "png",
                "-",
            ])
            .output()
            .await
            .expect("ffmpeg must be installed");
        assert!(
            out.status.success(),
            "ffmpeg failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    // # Storj
    // export $(grep -v '^#' .env | xargs)
    // cargo test -p storj-interface --bin storj-interface -- --ignored storj_both_thumbnail_names
    #[tokio::test]
    #[ignore = "integration: requires STORJ_ACCESS_GRANT and TEST_BUCKET env vars"]
    async fn storj_both_thumbnail_names_uploaded() {
        let grant = std::env::var("STORJ_ACCESS_GRANT").expect("STORJ_ACCESS_GRANT must be set");
        let bucket = std::env::var("TEST_BUCKET").expect("TEST_BUCKET must be set");

        // SAFETY: this test runs alone (--ignored) so no other thread races on env.
        unsafe {
            std::env::set_var("STORJ_ACCESS_GRANT_SFW", &grant);
            std::env::set_var("SFW_BUCKET", &bucket);
        }

        let thumbnail = generate_test_png().await;
        let video_id = format!("test-storj-{}", chrono::Utc::now().timestamp_millis());
        let publisher = "integration-test";

        upload_thumbnail_to_storj(publisher, &video_id, &thumbnail, false)
            .await
            .expect("Storj upload failed");

        for suffix in ["_thumbnail.png", "-thumbnail.png"] {
            let path = format!("sj://{bucket}/{publisher}/{video_id}{suffix}");
            let out = Command::new("uplink")
                .args([
                    "cp",
                    "--interactive=false",
                    "--analytics=false",
                    "--progress=false",
                    "--access",
                    &grant,
                    &path,
                    "-",
                ])
                .output()
                .await
                .expect("uplink cp");
            assert!(
                out.status.success(),
                "{path} not found in Storj: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(
                out.stdout, thumbnail,
                "{path}: downloaded bytes differ from uploaded PNG"
            );
        }

        // Clean up
        for suffix in ["_thumbnail.png", "-thumbnail.png"] {
            Command::new("uplink")
                .args([
                    "rm",
                    "--access",
                    &grant,
                    &format!("sj://{bucket}/{publisher}/{video_id}{suffix}"),
                ])
                .status()
                .await
                .ok();
        }
    }

    // # Hetzner S3
    // export $(grep -v '^#' .env | xargs)
    // cargo test -p storj-interface --bin storj-interface -- --ignored hetzner_both_thumbnail_names
    #[tokio::test]
    #[ignore = "integration: requires HETZNER_S3_ENDPOINT, HETZNER_S3_BUCKET, HETZNER_S3_ACCESS_KEY, HETZNER_S3_SECRET_KEY, HETZNER_S3_REGION env vars"]
    async fn hetzner_both_thumbnail_names_uploaded() {
        let video_id = format!("test-hetzner-{}", chrono::Utc::now().timestamp_millis());
        let publisher = "integration-test";

        let s3_client = S3Client::new().await;
        let thumbnail = generate_test_png().await;

        upload_thumbnail_to_s3(&s3_client, publisher, &video_id, &thumbnail)
            .await
            .expect("S3 upload failed");

        for suffix in ["_thumbnail.png", "-thumbnail.png"] {
            let key = format!("{publisher}/{video_id}{suffix}");
            let downloaded = s3_client
                .download_thumbnail(&key)
                .await
                .unwrap_or_else(|e| panic!("{key} not found in Hetzner S3: {e}"));
            assert_eq!(
                downloaded, thumbnail,
                "{key}: downloaded bytes differ from uploaded PNG"
            );
        }

        // Clean up
        for suffix in ["_thumbnail.png", "-thumbnail.png"] {
            let key = format!("{publisher}/{video_id}{suffix}");
            s3_client.delete_thumbnail(&key).await.ok();
        }
    }
}
