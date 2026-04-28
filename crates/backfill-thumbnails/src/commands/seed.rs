use super::{write_json, CommandSummary};
use anyhow::{bail, Context, Result};
use backfill_thumbnails::CliOptions;
use chrono::Utc;
use std::path::Path;
use storj_interface::thumbnail::extract_thumbnail_with_seek;
use tempfile::tempdir;
use tokio::fs;
use tokio::process::Command;

use crate::backend::Backend;

pub async fn execute_seed_test_data(
    cli: &CliOptions,
    backend: &Backend,
    run_id: &str,
    summary_path: &Path,
) -> Result<CommandSummary> {
    let video_data = create_seed_video_bytes().await?;
    let old_thumbnail = extract_thumbnail_with_seek(&video_data, Some("00:00:01"))
        .await
        .context("generate old-style thumbnail from second frame")?;

    let folder = cli
        .prefix
        .as_deref()
        .unwrap_or("test-user")
        .trim_end_matches('/');

    let mut seeded = 0usize;
    for index in 1..=cli.seed_count {
        let video_id = format!("seed-video-{}-{index}", Utc::now().timestamp());
        let video_key = format!("{folder}/{video_id}.mp4");
        let thumbnail_key = format!("{folder}/{video_id}_thumbnail.png");

        backend.upload_video(&video_key, video_data.clone()).await?;
        backend
            .upload_png(&thumbnail_key, old_thumbnail.clone())
            .await?;
        seeded += 1;
    }

    let summary = CommandSummary {
        run_id: run_id.to_string(),
        command: cli.command.as_str().to_string(),
        scope: cli.scope.as_str().to_string(),
        bucket: backend.bucket_name().to_string(),
        execute: true,
        seeded,
        ..Default::default()
    };
    write_json(summary_path, &summary).await?;
    Ok(summary)
}

async fn create_seed_video_bytes() -> Result<Vec<u8>> {
    let temp_dir = tempdir().context("create temp dir for seed video")?;
    let video_path = temp_dir.path().join("seed.mp4");

    let output = Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=blue:s=16x16:d=1:r=1",
            "-f",
            "lavfi",
            "-i",
            "color=c=red:s=16x16:d=1:r=1",
            "-f",
            "lavfi",
            "-i",
            "color=c=green:s=16x16:d=1:r=1",
            "-filter_complex",
            "[0:v][1:v][2:v]concat=n=3:v=1:a=0,format=yuv420p",
            video_path
                .to_str()
                .context("seed video path must be valid utf-8")?,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("spawn ffmpeg for seed video")?
        .wait_with_output()
        .await
        .context("wait for ffmpeg seed video")?;

    if !output.status.success() {
        bail!(
            "ffmpeg failed creating seed video: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fs::read(video_path)
        .await
        .context("read generated seed video")
}
