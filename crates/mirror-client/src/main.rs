use mirror_client::{MirrorClient, MirrorError};

const USAGE: &str = "Usage: mirror-client <command> [--limit N]

Commands:
  audit           Print index statistics
  duplicates      List videos with identical perceptual hashes
  scan-storj      Scan Storj bucket into index
  scan-hetzner    Scan Hetzner bucket into index
  phash           Compute missing perceptual hashes
  mirror          Copy pending videos from Hetzner → Storj
  run-pipeline    Run full pipeline: scan-hetzner → phash → mirror (requires --prefix)
  cancel          Cancel all running background jobs
  status          Show which jobs are currently running

Options:
  --limit N       Stop after processing N items (scan/phash/mirror/run-pipeline)
  --prefix PREFIX Filter by object key prefix, e.g. publisher-id/  (scan/run-pipeline)

Environment:
  MIRROR_SERVICE_URL    Base URL of the service (required)
  SERVICE_SECRET_TOKEN  Shared HMAC signing secret (required)";

fn parse_limit(args: &[String]) -> Option<u64> {
    args.windows(2)
        .find(|w| w[0] == "--limit")
        .and_then(|w| w[1].parse().ok())
}

fn parse_prefix(args: &[String]) -> Option<&str> {
    args.windows(2)
        .find(|w| w[0] == "--prefix")
        .map(|w| w[1].as_str())
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("");
    let limit = parse_limit(&args);
    let prefix = parse_prefix(&args);

    let url = std::env::var("MIRROR_SERVICE_URL").expect("MIRROR_SERVICE_URL must be set");
    let secret = std::env::var("SERVICE_SECRET_TOKEN").expect("SERVICE_SECRET_TOKEN must be set");

    let client = MirrorClient::new(url, secret);

    let result = match cmd {
        "audit" => match client.audit().await {
            Ok(r) => {
                println!("total:            {}", r.total);
                println!("phash_computed:   {}", r.phash_computed);
                println!("mirrored:         {}", r.mirrored);
                println!("missing_storj:    {}", r.missing_storj);
                println!("missing_hetzner:  {}", r.missing_hetzner);
                println!("cleanup_pending:  {}", r.cleanup_pending);
                println!("failed:           {}", r.failed);
                println!("error_count:      {}", r.error_count);
                if !r.duplicate_phashes.is_empty() {
                    println!("\nduplicate phashes ({}):", r.duplicate_phashes.len());
                    for d in &r.duplicate_phashes {
                        let ids: Vec<&str> = d.videos.iter().map(|v| v.video_id.as_str()).collect();
                        println!("  {} → {:?}", d.phash, ids);
                    }
                }
                Ok(())
            }
            Err(e) => Err(e),
        },
        "duplicates" => match client.duplicates().await {
            Ok(r) => {
                println!("duplicate groups:  {}", r.total_groups);
                println!("duplicate videos:  {}", r.total_duplicate_videos);
                for g in &r.groups {
                    println!("\n  phash: {} ({} videos)", g.phash, g.count);
                    for v in &g.videos {
                        println!("    video_id:    {}", v.video_id);
                        println!("    storj_key:   {}", v.storj_key.as_deref().unwrap_or("—"));
                        println!(
                            "    hetzner_key: {}",
                            v.hetzner_key.as_deref().unwrap_or("—")
                        );
                    }
                }
                Ok(())
            }
            Err(e) => Err(e),
        },
        "scan-storj" => client
            .scan_storj(limit, prefix)
            .await
            .map(|_| println!("scan-storj accepted")),
        "scan-hetzner" => client
            .scan_hetzner(limit, prefix)
            .await
            .map(|_| println!("scan-hetzner accepted")),
        "phash" => client
            .phash_backfill(limit)
            .await
            .map(|_| println!("phash accepted")),
        "mirror" => client
            .mirror(limit)
            .await
            .map(|_| println!("mirror accepted")),
        "run-pipeline" => {
            let Some(pfx) = prefix else {
                eprintln!("error: run-pipeline requires --prefix");
                std::process::exit(1);
            };
            run_pipeline(&client, limit, pfx).await
        }
        "cancel" => match client.cancel_all().await {
            Ok(r) => {
                println!("{}", r.message);
                if !r.jobs_running_at_cancel.is_empty() {
                    println!("jobs running at cancel: {:?}", r.jobs_running_at_cancel);
                } else {
                    println!("no jobs were running");
                }
                Ok(())
            }
            Err(e) => Err(e),
        },
        "status" => match client.job_status().await {
            Ok(s) => {
                println!(
                    "scan-storj:   {}",
                    if s.scan_storj { "running" } else { "idle" }
                );
                println!(
                    "scan-hetzner: {}",
                    if s.scan_hetzner { "running" } else { "idle" }
                );
                println!("phash:        {}", if s.phash { "running" } else { "idle" });
                println!(
                    "mirror:       {}",
                    if s.mirror { "running" } else { "idle" }
                );
                Ok(())
            }
            Err(e) => Err(e),
        },
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(1);
        }
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

/// Run the full pipeline: scan-hetzner → phash → mirror, polling between steps.
async fn run_pipeline(
    client: &MirrorClient,
    limit: Option<u64>,
    prefix: &str,
) -> Result<(), MirrorError> {
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

    // Step 1: scan-hetzner
    println!("[1/3] Starting scan-hetzner (prefix: {prefix})");
    client.scan_hetzner(limit, Some(prefix)).await?;
    println!("       scan-hetzner accepted, waiting for completion...");
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        let s = client.job_status().await?;
        if !s.scan_hetzner {
            break;
        }
        print!(".");
        use std::io::Write;
        std::io::stdout().flush().ok();
    }
    println!("\n       scan-hetzner complete");

    // Step 2: phash
    println!("[2/3] Starting phash");
    client.phash_backfill(limit).await?;
    println!("       phash accepted, waiting for completion...");
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        let s = client.job_status().await?;
        if !s.phash {
            break;
        }
        print!(".");
        use std::io::Write;
        std::io::stdout().flush().ok();
    }
    println!("\n       phash complete");

    // Step 3: mirror
    println!("[3/3] Starting mirror");
    client.mirror(limit).await?;
    println!("       mirror accepted, waiting for completion...");
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        let s = client.job_status().await?;
        if !s.mirror {
            break;
        }
        print!(".");
        use std::io::Write;
        std::io::stdout().flush().ok();
    }
    println!("\n       mirror complete");

    // Final audit
    println!("\nPipeline finished. Running audit...");
    match client.audit().await {
        Ok(r) => {
            println!("total:            {}", r.total);
            println!("phash_computed:   {}", r.phash_computed);
            println!("mirrored:         {}", r.mirrored);
            println!("missing_storj:    {}", r.missing_storj);
            println!("failed:           {}", r.failed);
            println!("error_count:      {}", r.error_count);
        }
        Err(e) => eprintln!("audit failed: {e}"),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_limit_returns_value() {
        assert_eq!(
            parse_limit(&args(&["mirror-client", "mirror", "--limit", "10"])),
            Some(10)
        );
    }

    #[test]
    fn parse_limit_returns_none_when_absent() {
        assert_eq!(parse_limit(&args(&["mirror-client", "mirror"])), None);
    }

    #[test]
    fn parse_limit_returns_none_for_invalid_value() {
        assert_eq!(
            parse_limit(&args(&["mirror-client", "mirror", "--limit", "abc"])),
            None
        );
    }

    #[test]
    fn parse_limit_requires_value_after_flag() {
        assert_eq!(parse_limit(&args(&["mirror-client", "--limit"])), None);
    }
}
