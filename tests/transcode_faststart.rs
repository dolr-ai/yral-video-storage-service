use bytes::Bytes;
use std::path::Path;
use std::process::Stdio;
use storj_interface::transcode::{
    ensure_faststart_mp4, mp4_moov_position, FaststartOutcome, MoovPosition,
};
use tempfile::tempdir;
use tokio::process::Command;

#[test]
fn mp4_moov_position_reports_front_index() {
    let mp4 = fake_mp4([b"ftyp", b"moov", b"mdat"]);

    assert_eq!(
        mp4_moov_position(&mp4).expect("parse mp4 boxes"),
        MoovPosition::BeforeMdat
    );
}

#[test]
fn mp4_moov_position_reports_back_index() {
    let mp4 = fake_mp4([b"ftyp", b"mdat", b"moov"]);

    assert_eq!(
        mp4_moov_position(&mp4).expect("parse mp4 boxes"),
        MoovPosition::AfterMdat
    );
}

#[test]
fn mp4_moov_position_rejects_overflowing_box_size() {
    let mut mp4 = fake_mp4([b"ftyp"]);
    mp4.extend_from_slice(&1u32.to_be_bytes());
    mp4.extend_from_slice(b"free");
    mp4.extend_from_slice(&u64::MAX.to_be_bytes());

    assert!(mp4_moov_position(&mp4).is_err());
}

#[tokio::test]
async fn ensure_faststart_mp4_passes_through_front_index_video() {
    let input = Bytes::from(fake_mp4([b"ftyp", b"moov", b"mdat"]));

    let result = ensure_faststart_mp4(input.clone())
        .await
        .expect("front-index mp4 should not require ffmpeg");

    assert_eq!(result.outcome, FaststartOutcome::AlreadyFaststart);
    assert_eq!(result.data, input);
}

#[tokio::test]
#[ignore = "requires ffmpeg installed locally"]
async fn ensure_faststart_mp4_remuxes_back_index_video() {
    let temp_dir = tempdir().expect("temp dir");
    let input_path = temp_dir.path().join("input.mp4");

    create_test_video(&input_path)
        .await
        .expect("create test video");

    let input_data = tokio::fs::read(&input_path).await.expect("read input mp4");
    assert_eq!(
        mp4_moov_position(&input_data).expect("parse input mp4"),
        MoovPosition::AfterMdat
    );

    let result = ensure_faststart_mp4(Bytes::from(input_data))
        .await
        .expect("remux mp4");

    assert_eq!(result.outcome, FaststartOutcome::Remuxed);
    assert_eq!(
        mp4_moov_position(&result.data).expect("parse remuxed mp4"),
        MoovPosition::BeforeMdat
    );
}

fn fake_mp4<const N: usize>(boxes: [&[u8; 4]; N]) -> Vec<u8> {
    let mut data = Vec::new();
    for box_type in boxes {
        data.extend_from_slice(&8u32.to_be_bytes());
        data.extend_from_slice(box_type);
    }
    data
}

async fn create_test_video(output_path: &Path) -> Result<(), String> {
    let output = Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=red:s=16x16:d=1:r=1",
            "-t",
            "1",
            "-pix_fmt",
            "yuv420p",
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
