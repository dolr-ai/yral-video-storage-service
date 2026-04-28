use super::{append_jsonl, write_json, CommandSummary};
use anyhow::{Context, Result};
use backfill_thumbnails::{staged_thumbnail_key, CliOptions};
use image::ImageFormat;
use serde::Serialize;
use std::path::Path;
use std::sync::Arc;
use storj_interface::thumbnail::extract_thumbnail;
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;

use crate::backend::Backend;

#[derive(Debug, Serialize)]
struct VerifyRecord {
    video_key: String,
    staged_thumbnail_key: String,
    status: &'static str,
    detail: String,
}

pub async fn execute_verify(
    cli: &CliOptions,
    backend: &Backend,
    run_id: &str,
    summary_path: &Path,
    verify_path: &Path,
    append_lock: Arc<Mutex<()>>,
) -> Result<CommandSummary> {
    let objects = backend.list_objects(cli.prefix.as_deref()).await?;
    let cutoff = cli
        .cutoff_before
        .context("verify command requires cutoff_before")?;
    let remote_staged_keys = objects
        .iter()
        .filter(|object| {
            object.key.ends_with("-thumbnail.png") && !object.key.ends_with("-bak-thumbnail.png")
        })
        .map(|object| object.key.clone())
        .collect::<std::collections::HashSet<_>>();

    let mut candidates = objects
        .iter()
        .filter(|object| object.key.ends_with(".mp4"))
        .filter(|object| object.last_modified <= cutoff)
        .filter(|object| {
            cli.prefix
                .as_deref()
                .map(|prefix| object.key.starts_with(prefix))
                .unwrap_or(true)
        })
        .filter_map(|object| {
            let staged = staged_thumbnail_key(&object.key)?;
            remote_staged_keys
                .contains(&staged)
                .then_some((object.key.clone(), staged))
        })
        .collect::<Vec<_>>();

    if let Some(sample) = cli.verify_sample {
        candidates.truncate(sample);
    }

    let backend = Arc::new(backend.clone());
    let download_sem = Arc::new(Semaphore::new(cli.download_concurrency.max(1)));
    let ffmpeg_sem = Arc::new(Semaphore::new(cli.ffmpeg_concurrency.max(1)));
    let mut join_set = JoinSet::new();
    let mut summary = CommandSummary {
        run_id: run_id.to_string(),
        command: cli.command.as_str().to_string(),
        scope: cli.scope.as_str().to_string(),
        bucket: backend.bucket_name().to_string(),
        cutoff: cutoff.to_rfc3339(),
        execute: false,
        total_objects: objects.len(),
        candidate_videos: candidates.len(),
        existing_staged_objects: remote_staged_keys.len(),
        ..Default::default()
    };

    for (video_key, staged_thumbnail_key) in candidates {
        let backend = backend.clone();
        let download_sem = download_sem.clone();
        let ffmpeg_sem = ffmpeg_sem.clone();
        let verify_path = verify_path.to_path_buf();
        let append_lock = append_lock.clone();

        join_set.spawn(async move {
            let result = async {
                let _permit = download_sem.acquire_owned().await?;
                let video_data = backend.download_object(&video_key).await?;
                let staged_thumbnail = backend.download_object(&staged_thumbnail_key).await?;
                drop(_permit);

                let _permit = ffmpeg_sem.acquire_owned().await?;
                let expected_thumbnail = extract_thumbnail(&video_data)
                    .await
                    .context("extract expected first-frame thumbnail")?;
                drop(_permit);

                compare_png_pixels(&expected_thumbnail, &staged_thumbnail)
            }
            .await;

            let (status, detail) = match result {
                Ok(true) => (
                    "PASS",
                    "staged thumbnail matches decoded first-frame pixels".to_string(),
                ),
                Ok(false) => (
                    "FAIL",
                    "staged thumbnail differs from decoded first-frame pixels".to_string(),
                ),
                Err(err) => ("SKIP", format!("{err:#}")),
            };

            let record = VerifyRecord {
                video_key: video_key.clone(),
                staged_thumbnail_key: staged_thumbnail_key.clone(),
                status,
                detail: detail.clone(),
            };
            append_jsonl(verify_path.as_path(), &record, &append_lock).await?;

            Ok::<_, anyhow::Error>((status.to_string(), detail))
        });
    }

    while let Some(result) = join_set.join_next().await {
        let (status, _) = result??;
        match status.as_str() {
            "PASS" => summary.verified_pass += 1,
            "FAIL" => summary.verified_fail += 1,
            _ => summary.verified_skip += 1,
        }
    }

    write_json(summary_path, &summary).await?;
    Ok(summary)
}

fn compare_png_pixels(expected_png: &[u8], actual_png: &[u8]) -> Result<bool> {
    let expected = image::load_from_memory_with_format(expected_png, ImageFormat::Png)
        .context("decode expected png")?
        .to_rgba8();
    let actual = image::load_from_memory_with_format(actual_png, ImageFormat::Png)
        .context("decode staged png")?
        .to_rgba8();

    Ok(expected.dimensions() == actual.dimensions() && expected.as_raw() == actual.as_raw())
}

#[cfg(test)]
mod tests {
    use super::compare_png_pixels;
    use image::{DynamicImage, ImageFormat, RgbImage, RgbaImage};
    use std::io::Cursor;

    #[test]
    fn compare_png_pixels_accepts_identical_pixels_with_different_encodings() {
        let expected = encode_rgb_png(12, 34, 56);
        let actual = encode_rgba_png(12, 34, 56, 255);

        assert_ne!(expected, actual);
        assert!(compare_png_pixels(&expected, &actual).expect("compare images"));
    }

    #[test]
    fn compare_png_pixels_rejects_different_pixels() {
        let expected = encode_rgb_png(12, 34, 56);
        let actual = encode_rgb_png(99, 88, 77);

        assert!(!compare_png_pixels(&expected, &actual).expect("compare images"));
    }

    fn encode_rgb_png(red: u8, green: u8, blue: u8) -> Vec<u8> {
        let image =
            DynamicImage::ImageRgb8(RgbImage::from_pixel(1, 1, image::Rgb([red, green, blue])));
        encode_png(image)
    }

    fn encode_rgba_png(red: u8, green: u8, blue: u8, alpha: u8) -> Vec<u8> {
        let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(
            1,
            1,
            image::Rgba([red, green, blue, alpha]),
        ));
        encode_png(image)
    }

    fn encode_png(image: DynamicImage) -> Vec<u8> {
        let mut buffer = Cursor::new(Vec::new());
        image
            .write_to(&mut buffer, ImageFormat::Png)
            .expect("encode png");
        buffer.into_inner()
    }
}
