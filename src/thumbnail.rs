use std::path::Path;
use std::process::Stdio;

use tokio::process::Command;

pub async fn extract_thumbnail(video_data: &[u8]) -> std::io::Result<Vec<u8>> {
    extract_thumbnail_with_seek(video_data, None).await
}

pub async fn extract_thumbnail_with_seek(
    video_data: &[u8],
    seek: Option<&str>,
) -> std::io::Result<Vec<u8>> {
    let temp_dir = tempfile::tempdir()?;
    let input_path = temp_dir.path().join("input.mp4");
    let output_path = temp_dir.path().join("thumbnail.png");

    tokio::fs::write(&input_path, video_data).await?;
    extract_thumbnail_from_video_path(&input_path, &output_path, seek).await?;

    tokio::fs::read(&output_path).await
}

pub async fn extract_thumbnail_from_video_path(
    input: &Path,
    output: &Path,
    seek: Option<&str>,
) -> std::io::Result<()> {
    let input = input.to_str().ok_or_else(|| io_err("Invalid input path"))?;
    let output = output
        .to_str()
        .ok_or_else(|| io_err("Invalid output path"))?;

    let mut args = vec!["-y"];
    if let Some(seek) = seek {
        args.extend(["-ss", seek]);
    }
    args.extend(["-i", input, "-vframes", "1", "-f", "image2", output]);

    // kill_on_drop is intentionally NOT set: sending SIGKILL on future cancellation triggers
    // SIGCHLD delivery, which at burst rates can stall Tokio's I/O driver and freeze the
    // entire runtime (including timers and signal handlers). Orphaned ffmpeg processes are
    // acceptable — they complete within seconds and are cleaned up by the OS when the runner
    // exits. The ffmpeg semaphore already caps concurrent invocations.
    let output_proc = Command::new("ffmpeg")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?
        .wait_with_output()
        .await?;

    if !output_proc.status.success() || !tokio::fs::try_exists(output).await.unwrap_or(false) {
        let stderr = String::from_utf8_lossy(&output_proc.stderr);
        tracing::error!("ffmpeg failed: {stderr}");
        return Err(io_err("Thumbnail extraction failed"));
    }

    Ok(())
}

fn io_err(msg: &str) -> std::io::Error {
    std::io::Error::other(msg)
}

#[cfg(test)]
mod tests {
    use super::{extract_thumbnail, extract_thumbnail_with_seek};
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

    #[tokio::test]
    #[ignore = "requires ffmpeg installed locally"]
    async fn extract_thumbnail_with_seek_uses_the_requested_frame() {
        let temp_dir = tempdir().expect("temp dir");
        let video_path = temp_dir.path().join("two-frame.mp4");

        create_two_frame_video(&video_path)
            .await
            .expect("create test video");

        let video_data = fs::read(&video_path).await.expect("read test video");
        let thumbnail = extract_thumbnail_with_seek(&video_data, Some("00:00:01"))
            .await
            .expect("extract thumbnail");

        let rgb = decode_png_to_rgb24(temp_dir.path(), &thumbnail)
            .await
            .expect("decode thumbnail");

        let (avg_r, avg_g, avg_b) = average_rgb(&rgb);
        assert!(
            avg_b > 200 && avg_r < 40 && avg_g < 40,
            "expected a blue second frame, got average rgb ({avg_r}, {avg_g}, {avg_b})"
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
