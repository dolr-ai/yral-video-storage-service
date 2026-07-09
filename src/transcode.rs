use bytes::Bytes;
use std::io;
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoovPosition {
    BeforeMdat,
    AfterMdat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaststartOutcome {
    AlreadyFaststart,
    Remuxed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaststartResult {
    pub data: Bytes,
    pub outcome: FaststartOutcome,
}

pub async fn ensure_faststart_mp4(video_data: Bytes) -> io::Result<FaststartResult> {
    if matches!(mp4_moov_position(&video_data), Ok(MoovPosition::BeforeMdat)) {
        return Ok(FaststartResult {
            data: video_data,
            outcome: FaststartOutcome::AlreadyFaststart,
        });
    }

    remux_faststart(video_data).await
}

pub fn mp4_moov_position(data: &[u8]) -> io::Result<MoovPosition> {
    let mut offset = 0usize;
    let mut moov_offset = None;
    let mut mdat_offset = None;

    while offset + 8 <= data.len() {
        let size32 = u32::from_be_bytes(
            data[offset..offset + 4]
                .try_into()
                .expect("slice length checked"),
        );
        let box_type = &data[offset + 4..offset + 8];

        let (box_size, header_size) = match size32 {
            0 => (data.len() - offset, 8usize),
            1 => {
                if offset + 16 > data.len() {
                    return Err(invalid_data("truncated MP4 large-size box"));
                }
                let size64 = u64::from_be_bytes(
                    data[offset + 8..offset + 16]
                        .try_into()
                        .expect("slice length checked"),
                );
                let size = usize::try_from(size64)
                    .map_err(|_| invalid_data("MP4 box size does not fit in memory"))?;
                (size, 16usize)
            }
            size => (
                usize::try_from(size).expect("u32 always fits usize on supported targets"),
                8usize,
            ),
        };

        let next_offset = offset
            .checked_add(box_size)
            .ok_or_else(|| invalid_data("MP4 box offset overflow"))?;

        if box_size < header_size || next_offset > data.len() {
            return Err(invalid_data("invalid MP4 top-level box size"));
        }

        match box_type {
            b"moov" => moov_offset = Some(offset),
            b"mdat" => mdat_offset = Some(offset),
            _ => {}
        }

        if let (Some(moov), Some(mdat)) = (moov_offset, mdat_offset) {
            return if moov < mdat {
                Ok(MoovPosition::BeforeMdat)
            } else {
                Ok(MoovPosition::AfterMdat)
            };
        }

        offset = next_offset;
    }

    Err(invalid_data("MP4 is missing top-level moov or mdat box"))
}

async fn remux_faststart(video_data: Bytes) -> io::Result<FaststartResult> {
    let temp_dir = tempfile::tempdir()?;
    let input_path = temp_dir.path().join("input.mp4");
    let output_path = temp_dir.path().join("output.mp4");

    tokio::fs::write(&input_path, &video_data).await?;
    run_ffmpeg_faststart(&input_path, &output_path).await?;

    let output_data = Bytes::from(tokio::fs::read(&output_path).await?);
    match mp4_moov_position(&output_data)? {
        MoovPosition::BeforeMdat => Ok(FaststartResult {
            data: output_data,
            outcome: FaststartOutcome::Remuxed,
        }),
        MoovPosition::AfterMdat => Err(invalid_data(
            "ffmpeg faststart output still has moov after mdat",
        )),
    }
}

async fn run_ffmpeg_faststart(input: &Path, output: &Path) -> io::Result<()> {
    let output_proc = Command::new("ffmpeg")
        .arg("-y")
        .arg("-v")
        .arg("error")
        .arg("-i")
        .arg(input)
        .arg("-c")
        .arg("copy")
        .arg("-movflags")
        .arg("+faststart")
        .arg(output)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?
        .wait_with_output()
        .await?;

    if output_proc.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output_proc.stderr);
        Err(io::Error::other(format!(
            "ffmpeg faststart remux failed: {stderr}"
        )))
    }
}

fn invalid_data(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
