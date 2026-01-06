use axum::{extract::State, response::IntoResponse, Json};
use reqwest::StatusCode;
use serde_json::json;
use std::process::Stdio;
use storj_interface::move2nsfw::Args;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::breadcrumb;
use crate::consts::{ACCESS_GRANT_NSFW, YRAL_NSFW_VIDEOS};
use crate::s3_client::S3Client;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("S3 operation failed: {0}")]
    S3(String),
}

impl IntoResponse for Error {
    fn into_response(self) -> axum::response::Response {
        println!("err: {self}");
        let (status, message) = match self {
            Error::Io(_) => (
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

pub async fn handler(
    State(s3_client): State<S3Client>,
    Json(request): Json<Args>,
) -> Result<impl IntoResponse, Error> {
    // Download video from S3 (SFW storage)
    let s3_video_key = format!("{}/{}.mp4", request.publisher_user_id, request.video_id);
    let s3_thumbnail_key = format!(
        "{}/{}_thumbnail.png",
        request.publisher_user_id, request.video_id
    );

    println!("Moving video and thumbnail from S3 to Storj NSFW bucket: {s3_video_key}");

    // Download video from S3
    breadcrumb!(
        "s3",
        "download_video",
        s3_video_key,
        true,
        "starting download"
    );
    let video_data = s3_client.download_video(&s3_video_key).await.map_err(|e| {
        eprintln!("S3 video download error for {s3_video_key}: {e}");
        breadcrumb!("s3", "download_video", s3_video_key, false, e.clone());
        Error::S3(e)
    })?;
    breadcrumb!(
        "s3",
        "download_video",
        s3_video_key,
        true,
        format!("downloaded {} bytes", video_data.len())
    );

    // Download thumbnail from S3 (optional - may not exist for older videos)
    breadcrumb!(
        "s3",
        "download_thumbnail",
        s3_thumbnail_key,
        true,
        "starting download"
    );
    let thumbnail_data = match s3_client.download_thumbnail(&s3_thumbnail_key).await {
        Ok(data) => {
            breadcrumb!(
                "s3",
                "download_thumbnail",
                s3_thumbnail_key,
                true,
                format!("downloaded {} bytes", data.len())
            );
            Some(data)
        }
        Err(e) => {
            eprintln!("S3 thumbnail download failed (may not exist): {s3_thumbnail_key}: {e}");
            breadcrumb!(
                "s3",
                "download_thumbnail",
                s3_thumbnail_key,
                false,
                "not found (ok for old videos)"
            );
            None
        }
    };

    // Upload video to Storj NSFW bucket
    let storj_video_key = format!("{}/{}.mp4", request.publisher_user_id, request.video_id);
    let video_dest = format!(
        "sj://{}/{}/{}.mp4",
        YRAL_NSFW_VIDEOS.as_str(),
        request.publisher_user_id,
        request.video_id
    );

    breadcrumb!(
        "storj",
        "upload_video_nsfw",
        storj_video_key,
        true,
        "starting upload"
    );

    let mut child = Command::new("uplink")
        .args([
            "cp",
            "--interactive=false",
            "--analytics=false",
            "--progress=false",
            "--access",
            ACCESS_GRANT_NSFW.as_str(),
            "-",
            video_dest.as_str(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()?;

    let mut pipe = child.stdin.take().expect("Stdin pipe to be opened for us");
    pipe.write_all(&video_data).await?;
    pipe.flush().await?;
    drop(pipe);

    let status = child.wait().await?;

    if !status.success() {
        breadcrumb!(
            "storj",
            "upload_video_nsfw",
            storj_video_key,
            false,
            format!("status: {}", status)
        );
        return Ok((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "message": "Failed to upload video to Storj NSFW bucket. Check server logs."
            })),
        ));
    }
    breadcrumb!(
        "storj",
        "upload_video_nsfw",
        storj_video_key,
        true,
        "completed"
    );

    // Upload thumbnail to Storj NSFW bucket (if it exists)
    if let Some(ref thumbnail_data) = thumbnail_data {
        let storj_thumbnail_key = format!(
            "{}/{}_thumbnail.png",
            request.publisher_user_id, request.video_id
        );
        let thumbnail_dest = format!(
            "sj://{}/{}/{}_thumbnail.png",
            YRAL_NSFW_VIDEOS.as_str(),
            request.publisher_user_id,
            request.video_id
        );

        breadcrumb!(
            "storj",
            "upload_thumbnail_nsfw",
            storj_thumbnail_key,
            true,
            "starting upload"
        );

        let mut child = Command::new("uplink")
            .args([
                "cp",
                "--interactive=false",
                "--analytics=false",
                "--progress=false",
                "--access",
                ACCESS_GRANT_NSFW.as_str(),
                "-",
                thumbnail_dest.as_str(),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()?;

        let mut pipe = child.stdin.take().expect("Stdin pipe to be opened for us");
        pipe.write_all(thumbnail_data).await?;
        pipe.flush().await?;
        drop(pipe);

        let status = child.wait().await?;

        if !status.success() {
            eprintln!("Failed to upload thumbnail to Storj NSFW bucket, continuing anyway");
            breadcrumb!(
                "storj",
                "upload_thumbnail_nsfw",
                storj_thumbnail_key,
                false,
                "failed but continuing"
            );
        } else {
            breadcrumb!(
                "storj",
                "upload_thumbnail_nsfw",
                storj_thumbnail_key,
                true,
                "completed"
            );
        }
    }

    // Delete video from S3 after successful move
    breadcrumb!("s3", "delete_video", s3_video_key, true, "starting delete");
    s3_client.delete_video(&s3_video_key).await.map_err(|e| {
        eprintln!("S3 video delete error for {s3_video_key}: {e:?}");
        breadcrumb!("s3", "delete_video", s3_video_key, false, format!("{e:?}"));
        Error::S3(format!("{e:?}"))
    })?;
    breadcrumb!("s3", "delete_video", s3_video_key, true, "completed");

    // Delete thumbnail from S3 if it existed
    if thumbnail_data.is_some() {
        breadcrumb!(
            "s3",
            "delete_thumbnail",
            s3_thumbnail_key,
            true,
            "starting delete"
        );
        if let Err(e) = s3_client.delete_thumbnail(&s3_thumbnail_key).await {
            eprintln!("S3 thumbnail delete error (non-fatal): {s3_thumbnail_key}: {e:?}");
            breadcrumb!(
                "s3",
                "delete_thumbnail",
                s3_thumbnail_key,
                false,
                "failed (non-fatal)"
            );
        } else {
            breadcrumb!(
                "s3",
                "delete_thumbnail",
                s3_thumbnail_key,
                true,
                "completed"
            );
        }
    }

    Ok((
        StatusCode::OK,
        Json(json!({
            "message": "moved"
        })),
    ))
}
