pub mod mirror;
pub mod phash_backfill;
pub mod scan_hetzner;
pub mod scan_storj;

use anyhow::{Context, Result};
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::process::Command;

/// RAII guard that resets an AtomicBool to false on drop.
/// Ensures the job-running flag is cleared even if the job panics.
pub struct JobGuard(pub Arc<AtomicBool>);

impl Drop for JobGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Extract video_id from an S3 key — returns None if not an .mp4 file.
/// video_id = full path without .mp4 extension, e.g. "user123/abc"
pub fn video_id_from_key(key: &str) -> Option<String> {
    key.strip_suffix(".mp4").map(|s| s.to_string())
}

/// Derive thumbnail key from mp4 key using strip_suffix (safe for paths with ".mp4" in dirs).
pub fn thumb_key_from_mp4_key(key: &str) -> Option<String> {
    key.strip_suffix(".mp4")
        .map(|stem| format!("{stem}-thumbnail.png"))
}

/// Upload a local file to Storj via uplink CLI.
pub async fn uplink_cp(src: &Path, dst: &str, access_grant: &str) -> Result<()> {
    let output = Command::new("uplink")
        .args([
            "cp",
            "--interactive=false",
            "--analytics=false",
            "--progress=false",
            "--access",
            access_grant,
            src.to_str().context("non-UTF8 path")?,
            dst,
        ])
        .output()
        .await
        .context("failed to spawn uplink")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("uplink cp to {dst} failed: {stderr}");
    }
    Ok(())
}

/// Log progress every 1000 items.
pub fn log_progress(processed: usize, label: &str) {
    if processed % 1000 == 0 && processed > 0 {
        tracing::info!(processed, "{label}: processed {processed} items");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_id_from_mp4_key_strips_suffix() {
        assert_eq!(
            video_id_from_key("user123/abc.mp4"),
            Some("user123/abc".to_string())
        );
    }

    #[test]
    fn video_id_from_key_rejects_non_mp4() {
        assert_eq!(video_id_from_key("user123/abc.mov"), None);
        assert_eq!(video_id_from_key("user123/abc_thumbnail.png"), None);
        assert_eq!(video_id_from_key("user123/abc-thumbnail.png"), None);
    }

    #[test]
    fn video_id_from_key_handles_nested_paths() {
        assert_eq!(
            video_id_from_key("a/b/c/video.mp4"),
            Some("a/b/c/video".to_string())
        );
    }

    #[test]
    fn thumb_key_from_mp4_key_uses_strip_suffix() {
        assert_eq!(
            thumb_key_from_mp4_key("user/abc.mp4"),
            Some("user/abc-thumbnail.png".to_string())
        );
    }

    #[test]
    fn thumb_key_does_not_mutate_folder_with_mp4_in_name() {
        assert_eq!(
            thumb_key_from_mp4_key("video.mp4.bak/file.mp4"),
            Some("video.mp4.bak/file-thumbnail.png".to_string())
        );
    }
}
