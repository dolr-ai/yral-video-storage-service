use anyhow::{bail, Context, Result};
use backfill_thumbnails::{BackendKind, ObjectInfo, Scope};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::Deserialize;
use std::process::Stdio;
use storj_interface::s3_client::S3Client;
use tempfile::NamedTempFile;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

#[derive(Clone)]
pub enum Backend {
    Storj(StorjBackend),
    Hetzner(HetznerBackend),
}

#[derive(Clone)]
pub struct StorjBackend {
    bucket: String,
    access_grant: String,
}

#[derive(Clone)]
pub struct HetznerBackend {
    client: S3Client,
}

impl Backend {
    pub async fn from_scope(scope: Scope, bucket_override: Option<String>) -> Result<Self> {
        match scope.backend() {
            BackendKind::Storj => Ok(Self::Storj(StorjBackend {
                bucket: resolve_storj_bucket(bucket_override)?,
                access_grant: resolve_storj_access_grant()?,
            })),
            BackendKind::Hetzner => Ok(Self::Hetzner(HetznerBackend {
                client: S3Client::new_with_bucket(bucket_override).await,
            })),
        }
    }

    pub fn kind(&self) -> BackendKind {
        match self {
            Self::Storj(_) => BackendKind::Storj,
            Self::Hetzner(_) => BackendKind::Hetzner,
        }
    }

    pub fn bucket_name(&self) -> &str {
        match self {
            Self::Storj(backend) => backend.bucket.as_str(),
            Self::Hetzner(backend) => backend.client.bucket(),
        }
    }

    pub async fn list_objects(&self, prefix: Option<&str>) -> Result<Vec<ObjectInfo>> {
        match self {
            Self::Storj(backend) => backend.list_objects(prefix).await,
            Self::Hetzner(backend) => backend.list_objects(prefix).await,
        }
    }

    pub async fn download_object(&self, key: &str) -> Result<Vec<u8>> {
        match self {
            Self::Storj(backend) => backend.download_object(key).await,
            Self::Hetzner(backend) => backend.download_object(key).await,
        }
    }

    pub async fn upload_png(&self, key: &str, data: Vec<u8>) -> Result<()> {
        match self {
            Self::Storj(backend) => backend.upload_bytes(key, data).await,
            Self::Hetzner(backend) => backend.upload_png(key, data).await,
        }
    }

    pub async fn upload_video(&self, key: &str, data: Vec<u8>) -> Result<()> {
        match self {
            Self::Storj(backend) => backend.upload_bytes(key, data).await,
            Self::Hetzner(backend) => backend.upload_video(key, data).await,
        }
    }
}

impl StorjBackend {
    async fn list_objects(&self, prefix: Option<&str>) -> Result<Vec<ObjectInfo>> {
        let target = storj_target(&self.bucket, prefix);
        let output = Command::new("uplink")
            .args([
                "ls",
                "-r",
                "--utc",
                "-o",
                "json",
                "--analytics=false",
                "--interactive=false",
                "--access",
                self.access_grant.as_str(),
                target.as_str(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to spawn uplink ls")?
            .wait_with_output()
            .await
            .context("failed to wait for uplink ls")?;

        if !output.status.success() {
            bail!(
                "uplink ls failed for {}: {}",
                target,
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(parse_uplink_ls_json_output(
            String::from_utf8_lossy(&output.stdout).as_ref(),
        ))
    }

    async fn download_object(&self, key: &str) -> Result<Vec<u8>> {
        let temp = NamedTempFile::new().context("create temp file for storj download")?;
        let temp_path = temp.path().to_path_buf();
        let src = format!("sj://{}/{}", self.bucket, key);

        let output = Command::new("uplink")
            .args([
                "cp",
                "--analytics=false",
                "--interactive=false",
                "--progress=false",
                "--access",
                self.access_grant.as_str(),
                src.as_str(),
                temp_path
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("invalid temp path"))?,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to spawn uplink cp download")?
            .wait_with_output()
            .await
            .context("failed to wait for uplink cp download")?;

        if !output.status.success() {
            bail!(
                "uplink download failed for {}: {}",
                src,
                String::from_utf8_lossy(&output.stderr)
            );
        }

        tokio::fs::read(temp_path)
            .await
            .context("read downloaded storj object")
    }

    async fn upload_bytes(&self, key: &str, data: Vec<u8>) -> Result<()> {
        let dest = format!("sj://{}/{}", self.bucket, key);
        let mut child = Command::new("uplink")
            .args([
                "cp",
                "--analytics=false",
                "--interactive=false",
                "--progress=false",
                "--access",
                self.access_grant.as_str(),
                "-",
                dest.as_str(),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to spawn uplink cp upload")?;

        let mut stdin = child
            .stdin
            .take()
            .context("uplink upload stdin unavailable")?;
        stdin
            .write_all(&data)
            .await
            .context("write uplink upload stdin")?;
        stdin.flush().await.context("flush uplink upload stdin")?;
        drop(stdin);

        let output = child
            .wait_with_output()
            .await
            .context("failed waiting for uplink upload")?;
        if !output.status.success() {
            bail!(
                "uplink upload failed for {}: {}",
                dest,
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(())
    }
}

impl HetznerBackend {
    async fn list_objects(&self, prefix: Option<&str>) -> Result<Vec<ObjectInfo>> {
        let objects = self
            .client
            .list_objects(prefix)
            .await
            .map_err(|e| anyhow::anyhow!("S3 list_objects (prefix={prefix:?}): {e}"))?;
        Ok(objects
            .into_iter()
            .filter_map(|object| {
                Some(ObjectInfo {
                    key: object.key,
                    last_modified: object.last_modified?,
                })
            })
            .collect())
    }

    async fn download_object(&self, key: &str) -> Result<Vec<u8>> {
        self.client
            .download_object(key)
            .await
            .map_err(|e| anyhow::anyhow!("S3 download_object (key={key}): {e}"))
    }

    async fn upload_png(&self, key: &str, data: Vec<u8>) -> Result<()> {
        self.client
            .upload_png_object(key, data)
            .await
            .map_err(|e| anyhow::anyhow!("S3 upload_png_object (key={key}): {e}"))
    }

    async fn upload_video(&self, key: &str, data: Vec<u8>) -> Result<()> {
        self.client
            .upload_video(key, &data, &std::collections::HashMap::new())
            .await
            .map_err(Into::into)
    }
}

fn resolve_storj_bucket(bucket_override: Option<String>) -> Result<String> {
    bucket_override.ok_or_else(|| anyhow::anyhow!("--bucket is required for storj scope"))
}

fn resolve_storj_access_grant() -> Result<String> {
    std::env::var("STORJ_ACCESS_GRANT").context("STORJ_ACCESS_GRANT must be set")
}

fn storj_target(bucket: &str, prefix: Option<&str>) -> String {
    match prefix {
        Some(prefix) if !prefix.is_empty() => format!("sj://{bucket}/{prefix}"),
        _ => format!("sj://{bucket}"),
    }
}

#[derive(Debug, Deserialize)]
struct UplinkListRecord {
    kind: String,
    created: String,
    key: String,
}

fn parse_uplink_ls_json_output(stdout: &str) -> Vec<ObjectInfo> {
    stdout
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            match parse_uplink_ls_json_line(trimmed) {
                Ok(Some(info)) => Some(info),
                Ok(None) => None, // expected non-OBJ entry (e.g. directory prefix)
                Err(reason) => {
                    tracing::warn!(
                        line = trimmed,
                        reason,
                        "skipping unparseable uplink ls line — object may be missing from listing"
                    );
                    None
                }
            }
        })
        .collect()
}

fn parse_uplink_ls_json_line(line: &str) -> Result<Option<ObjectInfo>, &'static str> {
    let record = serde_json::from_str::<UplinkListRecord>(line).map_err(|_| "invalid JSON")?;
    if record.kind != "OBJ" {
        return Ok(None);
    }

    let last_modified = parse_uplink_timestamp(&record.created).ok_or("unparseable timestamp")?;

    Ok(Some(ObjectInfo {
        key: record.key,
        last_modified,
    }))
}

fn parse_uplink_timestamp(created: &str) -> Option<DateTime<Utc>> {
    // "2026-04-21 12:34:56" — standard uplink ls -o json output
    if let Ok(dt) = NaiveDateTime::parse_from_str(created, "%Y-%m-%d %H:%M:%S") {
        return Some(DateTime::<Utc>::from_naive_utc_and_offset(dt, Utc));
    }
    // "2026-04-21 12:34:56 UTC" — uplink ls --utc -o json may append " UTC"
    let stripped = created.strip_suffix(" UTC").unwrap_or(created);
    if let Ok(dt) = NaiveDateTime::parse_from_str(stripped, "%Y-%m-%d %H:%M:%S") {
        return Some(DateTime::<Utc>::from_naive_utc_and_offset(dt, Utc));
    }
    // RFC 3339 fallback: "2026-04-21T12:34:56Z"
    if let Ok(dt) = DateTime::parse_from_rfc3339(created) {
        return Some(dt.with_timezone(&Utc));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{parse_uplink_ls_json_output, parse_uplink_timestamp};

    #[test]
    fn parses_standard_uplink_ls_json_output() {
        let objects = parse_uplink_ls_json_output(
            "{\"kind\":\"OBJ\",\"created\":\"2026-04-21 12:34:56\",\"size\":123,\"key\":\"publisher/video-1.mp4\"}\n{\"kind\":\"OBJ\",\"created\":\"2026-04-21 12:34:57\",\"size\":456,\"key\":\"publisher/video-1-thumbnail.png\"}\n",
        );

        assert_eq!(objects.len(), 2);
        assert_eq!(objects[0].key, "publisher/video-1.mp4");
        assert_eq!(objects[1].key, "publisher/video-1-thumbnail.png");
    }

    #[test]
    fn parses_utc_suffix_timestamp_from_uplink_utc_flag() {
        let objects = parse_uplink_ls_json_output(
            "{\"kind\":\"OBJ\",\"created\":\"2026-04-21 12:34:56 UTC\",\"size\":123,\"key\":\"publisher/video-1.mp4\"}\n",
        );

        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].key, "publisher/video-1.mp4");
    }

    #[test]
    fn parses_rfc3339_timestamp_fallback() {
        let ts = parse_uplink_timestamp("2026-04-21T12:34:56Z");
        assert!(ts.is_some());
    }

    #[test]
    fn ignores_non_object_json_lines() {
        let objects = parse_uplink_ls_json_output(
            "{\"kind\":\"PRE\",\"created\":\"2026-04-21 12:34:56\",\"key\":\"publisher/\"}\n",
        );
        assert!(objects.is_empty());
    }
}
