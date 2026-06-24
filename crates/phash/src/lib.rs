use anyhow::Context;
use image::DynamicImage;
use image_hasher::{HasherConfig, ImageHash};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub type PHashError = anyhow::Error;

pub const HASH_KIND: &str = "phash";
pub const HASH_VERSION: &str = "offchain_binary_10x8_v1";
pub const EXPECTED_BINARY_HASH_LEN: usize = 640;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoMetadata {
    pub duration_seconds: f64,
    pub frame_count: usize,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoHashResult {
    pub hash: String,
    pub metadata: VideoMetadata,
    pub hash_kind: &'static str,
    pub hash_version: &'static str,
}

#[derive(Debug, Clone)]
pub struct PHasher {
    num_frames: usize,
    hash_size: u32,
}

impl Default for PHasher {
    fn default() -> Self {
        Self::new()
    }
}

impl PHasher {
    pub fn new() -> Self {
        Self {
            num_frames: 10,
            hash_size: 8,
        }
    }

    pub fn compute_hash(&self, path: &Path) -> Result<String, PHashError> {
        self.hash_video(path)
    }

    /// Compute off-chain-compatible pHash for a video file.
    ///
    /// Returns ten 8x8 pHashes concatenated as a 640-character binary string.
    pub fn hash_video(&self, path: impl AsRef<Path>) -> Result<String, PHashError> {
        Ok(self.hash_video_with_metadata(path)?.hash)
    }

    pub fn hash_video_with_metadata(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<VideoHashResult, PHashError> {
        let (frames, metadata) = self
            .extract_frames_with_metadata(path.as_ref())
            .context("Failed to extract frames")?;

        if frames.is_empty() {
            log::warn!("No frames extracted from video: {:?}", path.as_ref());
            anyhow::bail!("No frames extracted from video");
        }

        let frame_hashes: Vec<String> = frames
            .iter()
            .map(|frame| {
                self.compute_image_hash(frame)
                    .map(|hash| self.hash_to_binary_string(&hash))
            })
            .collect::<Result<Vec<_>, PHashError>>()?;

        let mut hashes = Vec::with_capacity(self.num_frames);
        for i in 0..self.num_frames {
            hashes.push(frame_hashes[i % frame_hashes.len()].clone());
        }

        if frame_hashes.len() < self.num_frames {
            log::debug!(
                "Video has {} frames, repeating cyclically to fill {} slots",
                frame_hashes.len(),
                self.num_frames
            );
        }

        Ok(VideoHashResult {
            hash: hashes.join(""),
            metadata,
            hash_kind: HASH_KIND,
            hash_version: HASH_VERSION,
        })
    }

    fn extract_frames_with_metadata(
        &self,
        video_path: &Path,
    ) -> Result<(Vec<DynamicImage>, VideoMetadata), PHashError> {
        ffmpeg_next::init().context("ffmpeg init")?;

        let mut ictx =
            ffmpeg_next::format::input(video_path).context("Failed to open video file")?;

        let (stream_index, total_frames, mut decoder, mut metadata) = {
            let stream = ictx
                .streams()
                .best(ffmpeg_next::media::Type::Video)
                .context("No video stream found")?;
            let stream_index = stream.index();
            let total_frames = stream.frames() as usize;

            let context_decoder =
                ffmpeg_next::codec::context::Context::from_parameters(stream.parameters())
                    .context("Failed to create codec context")?;
            let decoder = context_decoder
                .decoder()
                .video()
                .context("Failed to create video decoder")?;

            let metadata = VideoMetadata {
                duration_seconds: duration_seconds(stream.duration(), stream.time_base(), &ictx),
                frame_count: total_frames,
                width: decoder.width(),
                height: decoder.height(),
                fps: rational_to_f64(stream.avg_frame_rate()),
            };

            (stream_index, total_frames, decoder, metadata)
        };

        let frame_interval = if total_frames > 1 {
            (total_frames - 1) as f64 / (self.num_frames - 1) as f64
        } else {
            0.0
        };

        // Preserve off-chain_binary_10x8_v1 behavior: when FFmpeg does not
        // report frame count, all target indices collapse to frame 0.
        let target_indices: Vec<usize> = (0..self.num_frames)
            .map(|i| (i as f64 * frame_interval).round() as usize)
            .collect();

        let mut frames = Vec::new();
        let mut frame_count = 0usize;
        let mut decoded_frame = ffmpeg_next::util::frame::video::Video::empty();

        for (stream, packet) in ictx.packets() {
            if stream.index() == stream_index {
                if let Err(e) = decoder.send_packet(&packet) {
                    log::warn!("Skipping corrupt packet at frame {}: {}", frame_count, e);
                    continue;
                }

                while decoder.receive_frame(&mut decoded_frame).is_ok() {
                    if target_indices.contains(&frame_count) {
                        match self.convert_to_rgb(&decoded_frame) {
                            Ok(rgb_frame) => {
                                frames.push(rgb_frame);
                                if frames.len() >= self.num_frames {
                                    metadata.frame_count =
                                        metadata_frame_count(total_frames, frame_count);
                                    return Ok((frames, metadata));
                                }
                            }
                            Err(e) => {
                                log::warn!(
                                    "Failed to convert frame {} to RGB, skipping: {}",
                                    frame_count,
                                    e
                                );
                            }
                        }
                    }
                    frame_count += 1;
                }
            }
        }

        if let Err(e) = decoder.send_eof() {
            log::warn!("Failed to send EOF to decoder: {}", e);
        }

        while decoder.receive_frame(&mut decoded_frame).is_ok() {
            if target_indices.contains(&frame_count) {
                match self.convert_to_rgb(&decoded_frame) {
                    Ok(rgb_frame) => {
                        frames.push(rgb_frame);
                        if frames.len() >= self.num_frames {
                            break;
                        }
                    }
                    Err(e) => {
                        log::warn!(
                            "Failed to convert frame {} to RGB during flush, skipping: {}",
                            frame_count,
                            e
                        );
                    }
                }
            }
            frame_count += 1;
        }

        metadata.frame_count = metadata_frame_count(total_frames, frame_count);
        Ok((frames, metadata))
    }

    fn convert_to_rgb(
        &self,
        frame: &ffmpeg_next::util::frame::video::Video,
    ) -> Result<DynamicImage, PHashError> {
        let width = frame.width();
        let height = frame.height();

        let mut scaler = ffmpeg_next::software::scaling::context::Context::get(
            frame.format(),
            width,
            height,
            ffmpeg_next::format::Pixel::RGB24,
            width,
            height,
            ffmpeg_next::software::scaling::flag::Flags::BILINEAR,
        )
        .context("Failed to create scaler")?;

        let mut rgb_frame = ffmpeg_next::util::frame::video::Video::empty();
        scaler
            .run(frame, &mut rgb_frame)
            .context("Failed to scale frame")?;

        let data = rgb_frame.data(0);
        let stride = rgb_frame.stride(0);
        let img_data = copy_rgb24_frame_data(data, stride, width, height)?;

        let img = image::RgbImage::from_raw(width, height, img_data)
            .context("Failed to create image from raw data")?;

        Ok(DynamicImage::ImageRgb8(img))
    }

    fn compute_image_hash(&self, img: &DynamicImage) -> Result<ImageHash, PHashError> {
        let hasher = HasherConfig::new()
            .hash_size(self.hash_size, self.hash_size)
            .to_hasher();

        let gray = img.to_luma8();
        Ok(hasher.hash_image(&gray))
    }

    fn hash_to_binary_string(&self, hash: &ImageHash) -> String {
        let bytes = hash.as_bytes();
        let mut binary = String::with_capacity(bytes.len() * 8);

        for byte in bytes {
            for i in (0..8).rev() {
                binary.push(if (byte >> i) & 1 == 1 { '1' } else { '0' });
            }
        }

        binary
    }
}

fn duration_seconds(
    stream_duration: i64,
    time_base: ffmpeg_next::Rational,
    ictx: &ffmpeg_next::format::context::Input,
) -> f64 {
    if stream_duration > 0 {
        stream_duration as f64 * rational_to_f64(time_base)
    } else if ictx.duration() > 0 {
        ictx.duration() as f64 / ffmpeg_next::ffi::AV_TIME_BASE as f64
    } else {
        0.0
    }
}

fn rational_to_f64(value: ffmpeg_next::Rational) -> f64 {
    if value.denominator() == 0 {
        0.0
    } else {
        value.numerator() as f64 / value.denominator() as f64
    }
}

fn metadata_frame_count(total_frames: usize, decoded_frames: usize) -> usize {
    if total_frames > 0 {
        total_frames
    } else {
        decoded_frames
    }
}

fn copy_rgb24_frame_data(
    data: &[u8],
    stride: usize,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, PHashError> {
    let row_bytes = (width as usize)
        .checked_mul(3)
        .context("RGB24 row byte count overflow")?;
    anyhow::ensure!(
        stride >= row_bytes,
        "RGB24 stride {stride} is shorter than row width {row_bytes}"
    );

    let capacity = row_bytes
        .checked_mul(height as usize)
        .context("RGB24 image byte count overflow")?;
    let mut img_data = Vec::with_capacity(capacity);

    for y in 0..height as usize {
        let row_start = y.checked_mul(stride).context("RGB24 row offset overflow")?;
        let row_end = row_start
            .checked_add(row_bytes)
            .context("RGB24 row end overflow")?;
        anyhow::ensure!(
            row_end <= data.len(),
            "RGB24 frame data too short for row {y}: need {row_end} bytes, have {}",
            data.len()
        );
        img_data.extend_from_slice(&data[row_start..row_end]);
    }

    Ok(img_data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phasher_default_num_frames() {
        let p = PHasher::new();
        assert_eq!(p.num_frames, 10);
    }

    #[test]
    fn phasher_default_hash_size() {
        let p = PHasher::new();
        assert_eq!(p.hash_size, 8);
    }

    #[test]
    fn copy_rgb24_frame_data_rejects_short_stride() {
        let data = vec![0; 6];
        let result = copy_rgb24_frame_data(&data, 2, 1, 2);

        assert!(result.is_err());
    }

    #[test]
    fn copy_rgb24_frame_data_rejects_short_data() {
        let data = vec![0; 5];
        let result = copy_rgb24_frame_data(&data, 3, 1, 2);

        assert!(result.is_err());
    }

    #[test]
    fn copy_rgb24_frame_data_copies_valid_rows_without_padding() {
        let data = vec![1, 2, 3, 99, 4, 5, 6, 88];
        let result = copy_rgb24_frame_data(&data, 4, 1, 2).expect("valid RGB24 frame data");

        assert_eq!(result, vec![1, 2, 3, 4, 5, 6]);
    }
}
