pub mod audit;
pub mod run;
pub mod seed;
pub mod verify;

pub use audit::execute_audit;
pub use run::execute_run;
pub use seed::execute_seed_test_data;
pub use verify::execute_verify;

use anyhow::{Context, Result};
use backfill_thumbnails::{ManifestEntry, Scope};
use serde::Serialize;
use std::path::{Path, PathBuf};
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

#[derive(Debug, Default, Serialize)]
pub struct CommandSummary {
    pub run_id: String,
    pub command: String,
    pub scope: String,
    pub bucket: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub cutoff: String,
    pub execute: bool,
    pub total_objects: usize,
    pub candidate_videos: usize,
    pub existing_staged_objects: usize,
    pub planned_process: usize,
    pub skip_remote_exists: usize,
    pub skip_manifest_done: usize,
    pub completed: usize,
    pub failed: usize,
    pub dry_run: usize,
    pub verified_pass: usize,
    pub verified_fail: usize,
    pub verified_skip: usize,
    pub seeded: usize,
}

pub(crate) fn state_dir_for_scope_and_bucket(
    manifest_dir: &str,
    scope: Scope,
    bucket: &str,
) -> PathBuf {
    PathBuf::from(manifest_dir)
        .join(scope.as_str())
        .join(sanitize_path_component(bucket))
}

fn sanitize_path_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' | '_' => ch,
            _ => '_',
        })
        .collect()
}

pub(super) async fn load_manifest_entries(path: &Path) -> Result<Vec<ManifestEntry>> {
    if !fs::try_exists(path).await.unwrap_or(false) {
        return Ok(Vec::new());
    }

    let content = fs::read(path)
        .await
        .with_context(|| format!("read manifest {}", path.display()))?;
    parse_manifest_entries(&content).with_context(|| format!("parse manifest {}", path.display()))
}

pub(super) async fn append_jsonl<T: Serialize>(
    path: &Path,
    value: &T,
    lock: &Mutex<()>,
) -> Result<()> {
    let _guard = lock.lock().await;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create parent dir {}", parent.display()))?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .with_context(|| format!("open append file {}", path.display()))?;
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    file.write_all(&line).await?;
    file.flush().await?;
    file.sync_data().await?;
    Ok(())
}

pub(super) async fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create parent dir {}", parent.display()))?;
    }
    let json = serde_json::to_vec_pretty(value)?;
    fs::write(path, json)
        .await
        .with_context(|| format!("write json file {}", path.display()))
}

fn parse_manifest_entries(bytes: &[u8]) -> Result<Vec<ManifestEntry>> {
    let ended_with_newline = bytes.last() == Some(&b'\n');
    let lines = bytes.split(|byte| *byte == b'\n').collect::<Vec<_>>();
    let mut entries = Vec::new();

    for (index, line) in lines.iter().enumerate() {
        if line.is_empty() {
            continue;
        }

        let is_last_line = index + 1 == lines.len();
        match serde_json::from_slice::<ManifestEntry>(line) {
            Ok(entry) => entries.push(entry),
            Err(err) if is_last_line && !ended_with_newline => {
                tracing::warn!("ignoring torn final manifest line: {err}");
            }
            Err(err) => {
                return Err(err).with_context(|| format!("invalid manifest line {}", index + 1));
            }
        }
    }

    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::{load_manifest_entries, state_dir_for_scope_and_bucket};
    use backfill_thumbnails::{BackendKind, ManifestEntry, ManifestStatus, Scope};
    use tempfile::tempdir;
    use tokio::fs;

    #[test]
    fn state_dir_is_bucket_scoped() {
        let dir = state_dir_for_scope_and_bucket("artifacts/backfill", Scope::Storj, "bucket-a");
        assert!(dir.ends_with("artifacts/backfill/storj/bucket-a"));
    }

    #[tokio::test]
    async fn load_manifest_entries_ignores_a_torn_last_line() {
        let temp = tempdir().expect("tempdir");
        let manifest_path = temp.path().join("manifest.jsonl");
        let complete = serde_json::to_string(&ManifestEntry {
            backend: BackendKind::Storj,
            bucket: "bucket-a".to_string(),
            source_video_key: "publisher/video-1.mp4".to_string(),
            staged_thumbnail_key: "publisher/video-1-thumbnail.png".to_string(),
            status: ManifestStatus::Completed,
        })
        .expect("serialize manifest entry");
        let torn_line = "{\"backend\":\"Storj\",\"bucket\":\"bucket-a\"";
        fs::write(&manifest_path, format!("{complete}\n{torn_line}"))
            .await
            .expect("write manifest");

        let entries = load_manifest_entries(&manifest_path)
            .await
            .expect("load manifest");

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].bucket, "bucket-a");
    }

    #[tokio::test]
    async fn load_manifest_entries_rejects_invalid_complete_lines() {
        let temp = tempdir().expect("tempdir");
        let manifest_path = temp.path().join("manifest.jsonl");
        fs::write(&manifest_path, b"{\"invalid\":true}\n")
            .await
            .expect("write manifest");

        let error = load_manifest_entries(&manifest_path)
            .await
            .expect_err("expected invalid manifest line to fail");

        assert!(format!("{error:#}").contains("manifest"));
    }
}
