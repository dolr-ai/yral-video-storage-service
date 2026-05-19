use anyhow::{Context, Result};
use image::DynamicImage;
use image_hasher::{HasherConfig, ImageHash};
use std::path::Path;

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

    /// Compute perceptual hash for a video file.
    /// Returns concatenated hex-encoded hashes — one per sampled frame.
    /// Output length = num_frames × (hash_size² / 4) hex chars (deterministic).
    pub fn compute_hash(&self, path: &Path) -> Result<String> {
        use ffmpeg_next as ffmpeg;

        ffmpeg::init().context("ffmpeg init")?;

        let mut ictx = ffmpeg::format::input(path)
            .with_context(|| format!("open video: {}", path.display()))?;

        let video_stream_index = ictx
            .streams()
            .best(ffmpeg::media::Type::Video)
            .context("no video stream")?
            .index();

        let stream = ictx.stream(video_stream_index).context("stream")?;

        let mut decoder = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
            .context("codec context")?
            .decoder()
            .video()
            .context("video decoder")?;

        // AV_NOPTS_VALUE = i64::MIN — stream duration absent for fragmented/truncated files
        let duration_ts = if stream.duration() == i64::MIN {
            // Fall back to format-context duration (in AV_TIME_BASE = 1_000_000 units)
            let fmt_duration = ictx.duration();
            if fmt_duration <= 0 {
                anyhow::bail!("Cannot determine video duration for {}", path.display());
            }
            // Convert from AV_TIME_BASE to stream time_base units
            let tb = stream.time_base();
            (fmt_duration as f64 * tb.denominator() as f64
                / (ffmpeg_next::ffi::AV_TIME_BASE as f64 * tb.numerator() as f64))
                as i64
        } else {
            stream.duration()
        };

        // Compute evenly-spaced timestamps (in stream time_base units)
        let timestamps: Vec<i64> = (0..self.num_frames)
            .map(|i| {
                let frac = if self.num_frames > 1 {
                    i as f64 / (self.num_frames - 1) as f64
                } else {
                    0.5
                };
                (frac * duration_ts as f64) as i64
            })
            .collect();

        let hasher = HasherConfig::new()
            .hash_size(self.hash_size, self.hash_size)
            .to_hasher();

        let mut result = String::new();

        for &ts in &timestamps {
            ictx.seek(ts, ..ts).context("seek")?;
            decoder.flush();

            let mut found = false;
            'packet_loop: for (stream, packet) in ictx.packets() {
                if stream.index() != video_stream_index {
                    continue;
                }
                decoder.send_packet(&packet).context("send packet")?;
                let mut frame = ffmpeg::frame::Video::empty();
                if decoder.receive_frame(&mut frame).is_ok() {
                    let image = frame_to_image(&frame)?;
                    let hash: ImageHash = hasher.hash_image(&image);
                    result.push_str(&bytes_to_hex(hash.as_bytes()));
                    found = true;
                    break 'packet_loop;
                }
            }

            if !found {
                log::warn!("No frame found at timestamp {ts}, using zero hash");
                // hash_size bits = hash_size * hash_size bits = hash_size^2 / 8 bytes
                let zero_bytes = vec![0u8; (self.hash_size * self.hash_size / 8) as usize];
                result.push_str(&bytes_to_hex(&zero_bytes));
            }
        }

        if result.is_empty() {
            anyhow::bail!("Could not extract any frames from {}", path.display());
        }

        Ok(result)
    }
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn frame_to_image(frame: &ffmpeg_next::frame::Video) -> Result<DynamicImage> {
    use ffmpeg_next::format::Pixel;
    use ffmpeg_next::software::scaling::{context::Context as ScalingContext, flag::Flags};

    let width = frame.width();
    let height = frame.height();

    let mut scaler = ScalingContext::get(
        frame.format(),
        width,
        height,
        Pixel::RGB24,
        width,
        height,
        Flags::BILINEAR,
    )
    .context("scaler")?;

    let mut rgb_frame = ffmpeg_next::frame::Video::empty();
    scaler.run(frame, &mut rgb_frame).context("scale frame")?;

    let data = rgb_frame.data(0);
    let stride = rgb_frame.stride(0);

    let mut buf = Vec::with_capacity((width * height * 3) as usize);
    for row in 0..height as usize {
        buf.extend_from_slice(&data[row * stride..row * stride + width as usize * 3]);
    }

    let img = image::RgbImage::from_raw(width, height, buf).context("RgbImage from raw")?;
    Ok(DynamicImage::ImageRgb8(img))
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
}
