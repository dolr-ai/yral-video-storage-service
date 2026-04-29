//! Mirror Hetzner S3 objects into Storj SFW bucket.
//!
//! Usage:
//!   backfill-mirror audit    -- list what Hetzner has that Storj SFW is missing
//!   backfill-mirror run      -- copy missing objects (add --execute to actually write)
//!
//! Required env vars:
//!   HETZNER_S3_ENDPOINT, HETZNER_S3_BUCKET, HETZNER_S3_ACCESS_KEY, HETZNER_S3_SECRET_KEY
//!   STORJ_ACCESS_GRANT_SFW   (SFW access grant)
//!   SFW_BUCKET               (optional, defaults to "yral-videos")

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use std::collections::HashSet;
use std::process::Stdio;
use std::sync::Arc;
use storj_interface::s3_client::S3Client;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

#[derive(Parser)]
#[command(
    name = "backfill-mirror",
    about = "Mirror Hetzner S3 objects to Storj SFW bucket",
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: SubCommand,
}

#[derive(Subcommand)]
enum SubCommand {
    /// Show how many objects are in Hetzner but missing from Storj SFW
    Audit(CommonArgs),
    /// Copy objects from Hetzner to Storj SFW (dry-run by default, add --execute to write)
    Run(RunArgs),
}

#[derive(Args, Clone)]
struct CommonArgs {
    /// Only process keys matching this prefix
    #[arg(long)]
    prefix: Option<String>,
    /// Override Storj SFW bucket (default: $SFW_BUCKET or "yral-videos")
    #[arg(long)]
    storj_bucket: Option<String>,
}

#[derive(Args)]
struct RunArgs {
    #[command(flatten)]
    common: CommonArgs,
    /// Max concurrent Hetzner downloads
    #[arg(long, default_value_t = 8)]
    download_concurrency: usize,
    /// Max concurrent Storj uploads
    #[arg(long, default_value_t = 8)]
    upload_concurrency: usize,
    /// Actually perform copies (default is dry-run)
    #[arg(long)]
    execute: bool,
}

#[derive(Debug, Serialize)]
struct AuditSummary {
    hetzner_objects: usize,
    storj_sfw_objects: usize,
    missing_from_storj: usize,
}

#[derive(Debug, Serialize)]
struct RunSummary {
    mode: &'static str,
    hetzner_objects: usize,
    storj_sfw_objects: usize,
    missing_from_storj: usize,
    copied: usize,
    failed: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();

    match cli.command {
        SubCommand::Audit(args) => audit(args).await,
        SubCommand::Run(args) => run(args).await,
    }
}

async fn audit(args: CommonArgs) -> Result<()> {
    let access_grant = resolve_access_grant()?;
    let storj_bucket = resolve_storj_bucket(args.storj_bucket.as_deref());

    let s3 = S3Client::new().await;

    eprintln!("Listing Hetzner objects...");
    let hetzner_keys = list_hetzner_keys(&s3, args.prefix.as_deref()).await?;

    eprintln!("Listing Storj SFW objects...");
    let storj_keys = list_storj_keys(&access_grant, &storj_bucket, args.prefix.as_deref()).await?;

    let missing = hetzner_keys
        .iter()
        .filter(|k| !storj_keys.contains(*k))
        .count();

    let summary = AuditSummary {
        hetzner_objects: hetzner_keys.len(),
        storj_sfw_objects: storj_keys.len(),
        missing_from_storj: missing,
    };

    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

async fn run(args: RunArgs) -> Result<()> {
    let access_grant = resolve_access_grant()?;
    let storj_bucket = resolve_storj_bucket(args.common.storj_bucket.as_deref());

    let s3 = S3Client::new().await;

    eprintln!("Listing Hetzner objects...");
    let hetzner_keys = list_hetzner_keys(&s3, args.common.prefix.as_deref()).await?;

    eprintln!("Listing Storj SFW objects...");
    let storj_keys = list_storj_keys(&access_grant, &storj_bucket, args.common.prefix.as_deref()).await?;

    let missing: Vec<String> = hetzner_keys
        .iter()
        .filter(|k| !storj_keys.contains(*k))
        .cloned()
        .collect();

    let total_missing = missing.len();

    let summary = if !args.execute {
        println!(
            "Dry-run: {} Hetzner objects, {} Storj SFW objects, {} missing (use --execute to copy)",
            hetzner_keys.len(),
            storj_keys.len(),
            total_missing
        );
        RunSummary {
            mode: "dry-run",
            hetzner_objects: hetzner_keys.len(),
            storj_sfw_objects: storj_keys.len(),
            missing_from_storj: total_missing,
            copied: 0,
            failed: 0,
        }
    } else {
        eprintln!(
            "Copying {} missing objects from Hetzner to Storj SFW...",
            total_missing
        );

        let s3 = Arc::new(s3);
        let access_grant = Arc::new(access_grant);
        let storj_bucket = Arc::new(storj_bucket);

        let download_sem = Arc::new(Semaphore::new(args.download_concurrency.max(1)));
        let upload_sem = Arc::new(Semaphore::new(args.upload_concurrency.max(1)));

        let mut join_set: JoinSet<Result<bool>> = JoinSet::new();

        for key in missing {
            let s3 = s3.clone();
            let access_grant = access_grant.clone();
            let storj_bucket = storj_bucket.clone();
            let download_sem = download_sem.clone();
            let upload_sem = upload_sem.clone();

            join_set.spawn(async move {
                let _dl = download_sem.acquire_owned().await?;
                let data = s3
                    .download_object(&key)
                    .await
                    .map_err(|e| anyhow::anyhow!("download {key} from Hetzner: {e}"))?;
                drop(_dl);

                let _ul = upload_sem.acquire_owned().await?;
                upload_to_storj(&access_grant, &storj_bucket, &key, data)
                    .await
                    .with_context(|| format!("upload {key} to Storj SFW"))?;
                drop(_ul);

                tracing::info!(key = %key, "copied");
                Ok(true)
            });
        }

        let mut copied = 0usize;
        let mut failed = 0usize;

        while let Some(result) = join_set.join_next().await {
            match result {
                Ok(Ok(_)) => copied += 1,
                Ok(Err(e)) => {
                    tracing::error!(error = %e, "copy failed");
                    failed += 1;
                }
                Err(e) => {
                    tracing::error!(error = %e, "task panicked");
                    failed += 1;
                }
            }

            if (copied + failed) % 100 == 0 {
                eprintln!("  progress: {}/{} ({} failed)", copied + failed, total_missing, failed);
            }
        }

        RunSummary {
            mode: "execute",
            hetzner_objects: hetzner_keys.len(),
            storj_sfw_objects: storj_keys.len(),
            missing_from_storj: total_missing,
            copied,
            failed,
        }
    };

    println!("{}", serde_json::to_string_pretty(&summary)?);

    if summary.failed > 0 {
        bail!("{} objects failed to copy", summary.failed);
    }

    Ok(())
}

async fn list_hetzner_keys(s3: &S3Client, prefix: Option<&str>) -> Result<HashSet<String>> {
    let objects = s3
        .list_objects(prefix)
        .await
        .map_err(anyhow::Error::msg)?;
    Ok(objects.into_iter().map(|o| o.key).collect())
}

async fn list_storj_keys(
    access_grant: &str,
    bucket: &str,
    prefix: Option<&str>,
) -> Result<HashSet<String>> {
    let target = match prefix {
        Some(p) if !p.is_empty() => format!("sj://{bucket}/{p}"),
        _ => format!("sj://{bucket}"),
    };

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
            access_grant,
            &target,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn uplink ls")?
        .wait_with_output()
        .await
        .context("wait for uplink ls")?;

    if !output.status.success() {
        bail!(
            "uplink ls failed for {}: {}",
            target,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_uplink_ls_keys(&stdout))
}

fn parse_uplink_ls_keys(stdout: &str) -> HashSet<String> {
    stdout
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            let obj: serde_json::Value = serde_json::from_str(trimmed).ok()?;
            if obj["kind"].as_str()? != "OBJ" {
                return None;
            }
            obj["key"].as_str().map(String::from)
        })
        .collect()
}

async fn upload_to_storj(
    access_grant: &str,
    bucket: &str,
    key: &str,
    data: Vec<u8>,
) -> Result<()> {
    let dest = format!("sj://{bucket}/{key}");

    let mut child = Command::new("uplink")
        .args([
            "cp",
            "--interactive=false",
            "--analytics=false",
            "--progress=false",
            "--access",
            access_grant,
            "-",
            &dest,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn uplink cp for {dest}"))?;

    let mut pipe = child.stdin.take().context("uplink stdin unavailable")?;
    pipe.write_all(&data).await.context("write data to uplink")?;
    pipe.flush().await.context("flush uplink stdin")?;
    drop(pipe);

    let output = child
        .wait_with_output()
        .await
        .context("wait for uplink cp")?;
    if !output.status.success() {
        bail!(
            "uplink cp failed for {}: {}",
            dest,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(())
}

fn resolve_access_grant() -> Result<String> {
    std::env::var("STORJ_ACCESS_GRANT_SFW")
        .context("STORJ_ACCESS_GRANT_SFW must be set")
}

fn resolve_storj_bucket(override_val: Option<&str>) -> String {
    override_val
        .map(String::from)
        .or_else(|| std::env::var("SFW_BUCKET").ok())
        .unwrap_or_else(|| "yral-videos".to_string())
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::parse_uplink_ls_keys;

    #[test]
    fn parses_obj_lines_and_ignores_prefixes() {
        let stdout = r#"{"kind":"OBJ","created":"2026-04-21 12:34:56","size":1000,"key":"publisher/video-1.mp4"}
{"kind":"PRE","created":"2026-04-21 12:34:57","key":"publisher/"}
{"kind":"OBJ","created":"2026-04-21 12:34:58","size":500,"key":"publisher/video-1_thumbnail.png"}
"#;
        let keys = parse_uplink_ls_keys(stdout);
        assert_eq!(keys.len(), 2);
        assert!(keys.contains("publisher/video-1.mp4"));
        assert!(keys.contains("publisher/video-1_thumbnail.png"));
    }

    #[test]
    fn handles_empty_output() {
        let keys = parse_uplink_ls_keys("");
        assert!(keys.is_empty());
    }
}
