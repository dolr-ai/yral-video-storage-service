use anyhow::{bail, Context, Result};
use backfill_thumbnails::{BackendKind, ObjectInfo, Scope};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::Deserialize;
use std::process::Stdio;
use storj_interface::consts::{YRAL_NSFW_VIDEOS, YRAL_VIDEOS};
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
                bucket: resolve_storj_bucket(scope, bucket_override)?,
                access_grant: resolve_storj_access_grant(scope)?,
            })),
            BackendKind::Hetzner => Ok(Self::Hetzner(HetznerBackend {
                client: S3Client::new_with_bucket(resolve_hetzner_bucket(scope, bucket_override)?)
                    .await,
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
            .map_err(anyhow::Error::msg)?;
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
            .map_err(anyhow::Error::msg)
    }

    async fn upload_png(&self, key: &str, data: Vec<u8>) -> Result<()> {
        self.client
            .upload_png_object(key, data)
            .await
            .map_err(anyhow::Error::msg)
    }

    async fn upload_video(&self, key: &str, data: Vec<u8>) -> Result<()> {
        self.client
            .upload_video(key, &data, &std::collections::HashMap::new())
            .await
            .map_err(Into::into)
    }
}

fn resolve_storj_bucket(scope: Scope, bucket_override: Option<String>) -> Result<String> {
    if let Some(bucket) = bucket_override {
        return Ok(bucket);
    }

    match scope {
        Scope::TestSfwStorj => std::env::var("TEST_BUCKET")
            .context("TEST_BUCKET must be set for test-sfw-storj when --bucket is not provided"),
        Scope::ProdSfwStorj => Ok(YRAL_VIDEOS.clone()),
        Scope::ProdNsfwStorj => Ok(YRAL_NSFW_VIDEOS.clone()),
        Scope::TestSfwHetzner | Scope::ProdSfwHetzner => {
            bail!("internal error: non-storj scope passed to storj bucket resolver")
        }
    }
}

fn resolve_storj_access_grant(scope: Scope) -> Result<String> {
    match scope {
        Scope::TestSfwStorj | Scope::ProdSfwStorj | Scope::ProdNsfwStorj => {
            std::env::var("STORJ_ACCESS_GRANT").context("STORJ_ACCESS_GRANT must be set")
        }
        Scope::TestSfwHetzner | Scope::ProdSfwHetzner => {
            bail!("internal error: non-storj scope passed to storj access-grant resolver")
        }
    }
}

fn resolve_hetzner_bucket(scope: Scope, bucket_override: Option<String>) -> Result<Option<String>> {
    match scope {
        Scope::TestSfwHetzner => bucket_override
            .map(Some)
            .ok_or_else(|| anyhow::anyhow!("--bucket is required for test-sfw-hetzner")),
        Scope::ProdSfwHetzner => Ok(bucket_override),
        Scope::TestSfwStorj | Scope::ProdSfwStorj | Scope::ProdNsfwStorj => {
            bail!("internal error: non-hetzner scope passed to hetzner bucket resolver")
        }
    }
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
    let record = serde_json::from_str::<UplinkListRecord>(line)
        .map_err(|_| "invalid JSON")?;
    if record.kind != "OBJ" {
        return Ok(None);
    }

    let last_modified = NaiveDateTime::parse_from_str(&record.created, "%Y-%m-%d %H:%M:%S")
        .map(|value| DateTime::<Utc>::from_naive_utc_and_offset(value, Utc))
        .map_err(|_| "unparseable timestamp")?;

    Ok(Some(ObjectInfo {
        key: record.key,
        last_modified,
    }))
}

#[cfg(test)]
mod tests {
    use super::{parse_uplink_ls_json_output, resolve_hetzner_bucket};
    use backfill_thumbnails::Scope;

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
    fn ignores_non_object_json_lines() {
        let objects = parse_uplink_ls_json_output(
            "{\"kind\":\"PRE\",\"created\":\"2026-04-21 12:34:56\",\"key\":\"publisher/\"}\n",
        );
        assert!(objects.is_empty());
    }

    #[test]
    fn rejects_test_hetzner_scope_without_explicit_bucket() {
        let error = resolve_hetzner_bucket(Scope::TestSfwHetzner, None)
            .expect_err("expected missing test bucket to fail");

        assert!(format!("{error:#}").contains("--bucket"));
    }
}
