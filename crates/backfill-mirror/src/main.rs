//! Mirror Hetzner S3 objects into Storj SFW bucket.
//!
//! Commands:
//!   audit    -- show full bidirectional diff (what each side is missing)
//!   run      -- copy objects Hetzner has that Storj SFW is missing (add --execute to write)
//!   cleanup  -- delete objects Storj SFW has that Hetzner no longer has (add --execute to delete)
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
    /// Show bidirectional diff: what each side has that the other is missing
    Audit(CommonArgs),
    /// Copy objects from Hetzner to Storj SFW that Storj is missing (dry-run by default)
    Run(MutateArgs),
    /// Delete Storj SFW objects that have no matching object in Hetzner (dry-run by default)
    Cleanup(MutateArgs),
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
struct MutateArgs {
    #[command(flatten)]
    common: CommonArgs,
    /// Max concurrent operations
    #[arg(long, default_value_t = 8)]
    concurrency: usize,
    /// Actually perform writes/deletes (default is dry-run)
    #[arg(long)]
    execute: bool,
}

#[derive(Debug, Serialize)]
struct AuditSummary {
    hetzner_objects: usize,
    storj_sfw_objects: usize,
    /// In Hetzner but missing from Storj SFW
    missing_from_storj: usize,
    /// In Storj SFW but not in Hetzner (stale — likely NSFW-moved videos)
    stale_in_storj: usize,
}

#[derive(Debug, Serialize)]
struct RunSummary {
    mode: &'static str,
    to_copy: usize,
    copied: usize,
    failed: usize,
}

#[derive(Debug, Serialize)]
struct CleanupSummary {
    mode: &'static str,
    stale_in_storj: usize,
    deleted: usize,
    failed: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();

    match cli.command {
        SubCommand::Audit(args) => audit(args).await,
        SubCommand::Run(args) => run(args).await,
        SubCommand::Cleanup(args) => cleanup(args).await,
    }
}

async fn audit(args: CommonArgs) -> Result<()> {
    let (hetzner_keys, storj_keys) = list_both(&args).await?;

    let missing_from_storj = hetzner_keys
        .iter()
        .filter(|k| !storj_keys.contains(*k))
        .count();
    let stale_in_storj = storj_keys
        .iter()
        .filter(|k| !hetzner_keys.contains(*k))
        .count();

    let summary = AuditSummary {
        hetzner_objects: hetzner_keys.len(),
        storj_sfw_objects: storj_keys.len(),
        missing_from_storj,
        stale_in_storj,
    };

    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

async fn run(args: MutateArgs) -> Result<()> {
    let (hetzner_keys, storj_keys) = list_both(&args.common).await?;

    let to_copy: Vec<String> = hetzner_keys
        .iter()
        .filter(|k| !storj_keys.contains(*k))
        .cloned()
        .collect();

    if !args.execute {
        let summary = RunSummary {
            mode: "dry-run",
            to_copy: to_copy.len(),
            copied: 0,
            failed: 0,
        };
        println!("{}", serde_json::to_string_pretty(&summary)?);
        return Ok(());
    }

    eprintln!(
        "Copying {} objects from Hetzner to Storj SFW...",
        to_copy.len()
    );

    let access_grant = Arc::new(resolve_access_grant()?);
    let storj_bucket = Arc::new(resolve_storj_bucket(args.common.storj_bucket.as_deref()));
    let s3 = Arc::new(S3Client::new().await);
    let sem = Arc::new(Semaphore::new(args.concurrency.max(1)));
    let total = to_copy.len();

    let mut join_set: JoinSet<Result<()>> = JoinSet::new();
    for key in to_copy {
        let s3 = s3.clone();
        let access_grant = access_grant.clone();
        let storj_bucket = storj_bucket.clone();
        let sem = sem.clone();

        join_set.spawn(async move {
            let _permit = sem.acquire_owned().await?;
            let data = s3
                .download_object(&key)
                .await
                .map_err(|e| anyhow::anyhow!("download {key} from Hetzner: {e}"))?;
            upload_to_storj(&access_grant, &storj_bucket, &key, data)
                .await
                .with_context(|| format!("upload {key} to Storj SFW"))?;
            tracing::info!(key = %key, "copied");
            Ok(())
        });
    }

    let (mut copied, mut failed) = (0usize, 0usize);
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(Ok(())) => copied += 1,
            Ok(Err(e)) => {
                tracing::error!(error = %e, "copy failed");
                failed += 1;
            }
            Err(e) => {
                tracing::error!(error = %e, "task panicked");
                failed += 1;
            }
        }
        if (copied + failed) % 500 == 0 {
            eprintln!("  progress: {}/{total} ({failed} failed)", copied + failed);
        }
    }

    let summary = RunSummary {
        mode: "execute",
        to_copy: total,
        copied,
        failed,
    };
    println!("{}", serde_json::to_string_pretty(&summary)?);

    if failed > 0 {
        bail!("{failed} objects failed to copy");
    }
    Ok(())
}

async fn cleanup(args: MutateArgs) -> Result<()> {
    let (hetzner_keys, storj_keys) = list_both(&args.common).await?;

    let stale: Vec<String> = storj_keys
        .iter()
        .filter(|k| !hetzner_keys.contains(*k))
        .cloned()
        .collect();

    if !args.execute {
        let summary = CleanupSummary {
            mode: "dry-run",
            stale_in_storj: stale.len(),
            deleted: 0,
            failed: 0,
        };
        println!("{}", serde_json::to_string_pretty(&summary)?);
        return Ok(());
    }

    eprintln!("Deleting {} stale objects from Storj SFW...", stale.len());

    let access_grant = Arc::new(resolve_access_grant()?);
    let storj_bucket = Arc::new(resolve_storj_bucket(args.common.storj_bucket.as_deref()));
    let sem = Arc::new(Semaphore::new(args.concurrency.max(1)));
    let total = stale.len();

    let mut join_set: JoinSet<Result<()>> = JoinSet::new();
    for key in stale {
        let access_grant = access_grant.clone();
        let storj_bucket = storj_bucket.clone();
        let sem = sem.clone();

        join_set.spawn(async move {
            let _permit = sem.acquire_owned().await?;
            delete_from_storj(&access_grant, &storj_bucket, &key)
                .await
                .with_context(|| format!("delete {key} from Storj SFW"))?;
            tracing::info!(key = %key, "deleted");
            Ok(())
        });
    }

    let (mut deleted, mut failed) = (0usize, 0usize);
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(Ok(())) => deleted += 1,
            Ok(Err(e)) => {
                tracing::error!(error = %e, "delete failed");
                failed += 1;
            }
            Err(e) => {
                tracing::error!(error = %e, "task panicked");
                failed += 1;
            }
        }
        if (deleted + failed) % 500 == 0 {
            eprintln!("  progress: {}/{total} ({failed} failed)", deleted + failed);
        }
    }

    let summary = CleanupSummary {
        mode: "execute",
        stale_in_storj: total,
        deleted,
        failed,
    };
    println!("{}", serde_json::to_string_pretty(&summary)?);

    if failed > 0 {
        bail!("{failed} objects failed to delete");
    }
    Ok(())
}

async fn list_both(args: &CommonArgs) -> Result<(HashSet<String>, HashSet<String>)> {
    let access_grant = resolve_access_grant()?;
    let storj_bucket = resolve_storj_bucket(args.storj_bucket.as_deref());
    let s3 = S3Client::new().await;

    eprintln!("Listing Hetzner objects...");
    let hetzner_keys = list_hetzner_keys(&s3, args.prefix.as_deref()).await?;

    eprintln!("Listing Storj SFW objects...");
    let storj_keys = list_storj_keys(&access_grant, &storj_bucket, args.prefix.as_deref()).await?;

    Ok((hetzner_keys, storj_keys))
}

async fn list_hetzner_keys(s3: &S3Client, prefix: Option<&str>) -> Result<HashSet<String>> {
    let objects = s3.list_objects(prefix).await.map_err(anyhow::Error::msg)?;
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

async fn upload_to_storj(access_grant: &str, bucket: &str, key: &str, data: Vec<u8>) -> Result<()> {
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
    pipe.write_all(&data)
        .await
        .context("write data to uplink")?;
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

async fn delete_from_storj(access_grant: &str, bucket: &str, key: &str) -> Result<()> {
    let path = format!("sj://{bucket}/{key}");

    let output = Command::new("uplink")
        .args([
            "rm",
            "--interactive=false",
            "--analytics=false",
            "--access",
            access_grant,
            &path,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn uplink rm for {path}"))?
        .wait_with_output()
        .await
        .context("wait for uplink rm")?;

    if !output.status.success() {
        bail!(
            "uplink rm failed for {}: {}",
            path,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

fn resolve_access_grant() -> Result<String> {
    std::env::var("STORJ_ACCESS_GRANT_SFW").context("STORJ_ACCESS_GRANT_SFW must be set")
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
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
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
