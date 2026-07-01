use mirror_client::{MirrorClient, MirrorError};

const USAGE: &str = "Usage: mirror-client <command> [--limit N]

Commands:
  audit                Print index statistics
  duplicates           List videos with identical perceptual hashes
  video-duplicates     Show duplicate group for a specific video (requires --video VIDEO_ID)
  failed               List all permanently failed videos
  retry-failed         Reset failed videos to retry on next mirror run
  scan-storj           Scan Storj bucket into index
  scan-hetzner         Scan Hetzner bucket into index
  phash                Compute missing perceptual hashes
  mirror               Copy pending videos from Hetzner → Storj
  run-pipeline         Run full pipeline locally: scan-hetzner → phash → mirror
  run-pipeline-async   Trigger full pipeline on server and return immediately; use status/audit to track
  cancel               Cancel all running background jobs
  status               Show which jobs are currently running
  config               Show dynamic configuration
  config-set           Update configuration (use --phash, --mirror, --page)

  media-import         Trigger video-index import into media master table [--limit N]
  media-phash          Trigger pHash computation for media master entries [--limit N] [--shard I --of N]
  media-cancel         Cancel any running media import / phash jobs
  media-status         Show running status of media import and phash jobs
  media-audit          Show pHash coverage stats for servable videos
  media-sweep          Show steady-state sweep worker lease (owner/heartbeat/last discovery)
  media-feed           Stream events from the media feed [--after CURSOR] [--limit N]
  media-runs           List recent job runs [--job-kind KIND] [--limit N]
  media-failures       List failure groups [--job-kind KIND] [--limit N]

Options:
  --limit N           Stop after processing N items (scan/phash/mirror/run-pipeline/media-*)
  --prefix PREFIX     Filter by object key prefix, e.g. publisher-id/  (scan/run-pipeline)
  --full-scan         Scan entire S3 bucket and reset failed jobs instead of resuming (scan/run-pipeline)
  --video VIDEO_ID    Video ID for video-duplicates command
  --after CURSOR      Cursor (i64) for media-feed pagination
  --job-kind KIND     Job kind filter for media-runs / media-failures (e.g. media_phash)
  --shard I --of N    Process only shard I of N (0 <= I < N) for media-phash; both required together

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

fn parse_full_scan(args: &[String]) -> Option<bool> {
    if args.iter().any(|arg| arg == "--full-scan") {
        Some(true)
    } else {
        None
    }
}

fn parse_after(args: &[String]) -> Option<i64> {
    args.windows(2)
        .find(|w| w[0] == "--after")
        .and_then(|w| w[1].parse().ok())
}

fn parse_job_kind(args: &[String]) -> Option<String> {
    args.windows(2)
        .find(|w| w[0] == "--job-kind")
        .map(|w| w[1].clone())
}

fn parse_shard_arg(args: &[String]) -> Option<i64> {
    args.windows(2)
        .find(|w| w[0] == "--shard")
        .and_then(|w| w[1].parse().ok())
}

fn parse_of(args: &[String]) -> Option<i64> {
    args.windows(2)
        .find(|w| w[0] == "--of")
        .and_then(|w| w[1].parse().ok())
}

/// Resolve `--shard`/`--of` into the `(of, idx)` tuple the client expects.
///
/// `Ok(None)` when neither flag is set. Both must be present together with
/// `of >= 1` and `0 <= shard < of`; otherwise `Err(msg)`.
fn resolve_shard(args: &[String]) -> Result<Option<(i64, i64)>, String> {
    match (parse_shard_arg(args), parse_of(args)) {
        (None, None) => Ok(None),
        (Some(idx), Some(of)) if of >= 1 && (0..of).contains(&idx) => Ok(Some((of, idx))),
        (Some(_), Some(_)) => Err("--shard must satisfy 0 <= shard < of, and of >= 1".to_string()),
        _ => Err("--shard and --of must be provided together".to_string()),
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("");
    let limit = parse_limit(&args);
    let prefix = parse_prefix(&args);
    let full_scan = parse_full_scan(&args);

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
                if !r.status_breakdown.is_empty() {
                    println!("\nstatus breakdown:");
                    for (status, count) in &r.status_breakdown {
                        println!("  {:<16} {}", status, count);
                    }
                }
                if !r.duplicate_phashes.is_empty() {
                    println!("\nduplicate phashes ({}):", r.duplicate_phashes.len());
                    for d in &r.duplicate_phashes {
                        let ids: Vec<&str> = d.videos.iter().map(|v| v.video_id.as_str()).collect();
                        println!(
                            "  {}/{} {} → {:?}",
                            d.hash_kind, d.hash_version, d.phash, ids
                        );
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
                    println!(
                        "\n  phash: {}/{} {} ({} videos)",
                        g.hash_kind, g.hash_version, g.phash, g.count
                    );
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
        "failed" => match client.failed_jobs().await {
            Ok(r) => {
                println!("failed: {}", r.count);
                for j in &r.jobs {
                    println!(
                        "  {} — {}",
                        j.video_id,
                        j.error_message.as_deref().unwrap_or("(no message)")
                    );
                }
                Ok(())
            }
            Err(e) => Err(e),
        },
        "retry-failed" => match client.retry_failed().await {
            Ok(r) => {
                println!("reset {} failed jobs → phash_computed", r.reset_count);
                Ok(())
            }
            Err(e) => Err(e),
        },
        "video-duplicates" => {
            let video_id = args
                .windows(2)
                .find(|w| w[0] == "--video")
                .map(|w| w[1].as_str());
            let Some(vid) = video_id else {
                eprintln!("error: video-duplicates requires --video VIDEO_ID");
                std::process::exit(1);
            };
            match client.video_duplicates(vid).await {
                Ok(Some(g)) => {
                    println!("phash: {} {}", g.hash_version, g.phash);
                    println!("kind: {}", g.hash_kind);
                    println!("count: {}", g.count);
                    for v in &g.videos {
                        println!("  {}", v.video_id);
                    }
                    Ok(())
                }
                Ok(None) => {
                    println!("no duplicates found for {vid}");
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
        "scan-storj" => client
            .scan_storj(limit, prefix, full_scan)
            .await
            .map(|_| println!("scan-storj accepted")),
        "scan-hetzner" => client
            .scan_hetzner(limit, prefix, full_scan)
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
        "run-pipeline" => run_pipeline(&client, limit, prefix, full_scan).await,
        "run-pipeline-async" => client
            .trigger_pipeline(limit, prefix, full_scan)
            .await
            .map(|_| {
                println!(
                    "run-pipeline accepted by server. Use 'status' or 'audit' to check progress."
                )
            }),
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
                println!(
                    "pipeline:     {}",
                    if s.pipeline { "running" } else { "idle" }
                );
                Ok(())
            }
            Err(e) => Err(e),
        },
        "config" => match client.config_get().await {
            Ok(c) => {
                println!("phash_concurrency:  {}", c.phash_concurrency);
                println!("mirror_concurrency: {}", c.mirror_concurrency);
                println!("scan_page_size:     {}", c.scan_page_size);
                Ok(())
            }
            Err(e) => Err(e),
        },
        "config-set" => {
            let p_phash = args
                .windows(2)
                .find(|w| w[0] == "--phash")
                .and_then(|w| w[1].parse().ok());
            let p_mirror = args
                .windows(2)
                .find(|w| w[0] == "--mirror")
                .and_then(|w| w[1].parse().ok());
            let p_page = args
                .windows(2)
                .find(|w| w[0] == "--page")
                .and_then(|w| w[1].parse().ok());
            let update = mirror_client::ConfigUpdate {
                phash_concurrency: p_phash,
                mirror_concurrency: p_mirror,
                scan_page_size: p_page,
            };
            match client.config_update(&update).await {
                Ok(c) => {
                    println!("updated:");
                    println!("phash_concurrency:  {}", c.phash_concurrency);
                    println!("mirror_concurrency: {}", c.mirror_concurrency);
                    println!("scan_page_size:     {}", c.scan_page_size);
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
        "media-import" => client
            .media_import(limit)
            .await
            .map(|_| println!("media-import accepted")),
        "media-phash" => match resolve_shard(&args) {
            Ok(shard) => client
                .media_phash(limit, shard)
                .await
                .map(|_| println!("media-phash accepted")),
            Err(msg) => {
                eprintln!("error: {msg}");
                std::process::exit(1);
            }
        },
        "media-cancel" => match client.media_cancel().await {
            Ok(v) => {
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                Ok(())
            }
            Err(e) => Err(e),
        },
        "media-status" => match client.media_status().await {
            Ok(s) => {
                println!(
                    "import: {}",
                    if s.import_running { "running" } else { "idle" }
                );
                println!(
                    "phash:  {}",
                    if s.phash_running { "running" } else { "idle" }
                );
                Ok(())
            }
            Err(e) => Err(e),
        },
        "media-audit" => match client.media_audit().await {
            Ok(r) => {
                println!("total_servable:         {}", r.total_servable);
                println!("with_canonical_phash:   {}", r.with_canonical_phash);
                println!("missing_canonical_phash:{}", r.missing_canonical_phash);
                Ok(())
            }
            Err(e) => Err(e),
        },
        "media-sweep" => match client.media_sweep().await {
            Ok(s) => {
                println!(
                    "owner:             {}",
                    s.owner.as_deref().unwrap_or("<none>")
                );
                println!(
                    "heartbeat:         {}",
                    s.heartbeat.as_deref().unwrap_or("<none>")
                );
                println!(
                    "last_discovery_at: {}",
                    s.last_discovery_at.as_deref().unwrap_or("<never>")
                );
                Ok(())
            }
            Err(e) => Err(e),
        },
        "media-feed" => {
            let after = parse_after(&args);
            match client.media_feed(after, limit).await {
                Ok(r) => {
                    println!("{:#?}", r);
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
        "media-runs" => {
            let job_kind = parse_job_kind(&args);
            match client.media_runs(job_kind.as_deref(), limit).await {
                Ok(r) => {
                    if r.runs.is_empty() {
                        println!("no runs found");
                    }
                    for run in &r.runs {
                        let totals_str = run
                            .totals
                            .as_ref()
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "—".to_string());
                        let finished = run.finished_at.as_deref().unwrap_or("—");
                        let err = run
                            .error_message
                            .as_deref()
                            .map(|e| format!("  error: {e}"))
                            .unwrap_or_default();
                        println!(
                            "{:<20}  {:<12}  totals: {:<30}  started: {}  finished: {}{}",
                            run.job_kind, run.status, totals_str, run.started_at, finished, err
                        );
                    }
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
        "media-failures" => {
            let job_kind = parse_job_kind(&args);
            match client.media_failures(job_kind.as_deref(), limit).await {
                Ok(r) => {
                    if r.failures.is_empty() {
                        println!("no failures found");
                    }
                    for group in &r.failures {
                        println!("{:<30}  count: {}", group.phase, group.count);
                        for sample in &group.samples {
                            println!("    {sample}");
                        }
                    }
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
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
    prefix: Option<&str>,
    full_scan: Option<bool>,
) -> Result<(), MirrorError> {
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

    // Step 1: scan-hetzner
    println!("[1/3] Starting scan-hetzner (prefix: {:?})", prefix);
    client.scan_hetzner(limit, prefix, full_scan).await?;
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
            if !r.status_breakdown.is_empty() {
                println!("\nstatus breakdown:");
                for (status, count) in &r.status_breakdown {
                    println!("  {:<16} {}", status, count);
                }
            }
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
    fn parses_job_kind_flag() {
        let a = args(&["bin", "media-runs", "--job-kind", "media_phash"]);
        assert_eq!(parse_job_kind(&a), Some("media_phash".to_string()));
        let none: Vec<String> = args(&["bin", "media-runs"]);
        assert_eq!(parse_job_kind(&none), None);
    }

    #[test]
    fn resolve_shard_none_when_both_absent() {
        assert_eq!(resolve_shard(&args(&["bin", "media-phash"])), Ok(None));
    }

    #[test]
    fn resolve_shard_valid_pair_returns_of_idx() {
        let a = args(&["bin", "media-phash", "--shard", "1", "--of", "3"]);
        assert_eq!(resolve_shard(&a), Ok(Some((3, 1))));
    }

    #[test]
    fn resolve_shard_rejects_partial_pair() {
        assert!(resolve_shard(&args(&["bin", "media-phash", "--shard", "0"])).is_err());
        assert!(resolve_shard(&args(&["bin", "media-phash", "--of", "3"])).is_err());
    }

    #[test]
    fn resolve_shard_rejects_out_of_range() {
        assert!(resolve_shard(&args(&["bin", "p", "--shard", "3", "--of", "3"])).is_err());
        assert!(resolve_shard(&args(&["bin", "p", "--shard", "-1", "--of", "3"])).is_err());
        assert!(resolve_shard(&args(&["bin", "p", "--shard", "0", "--of", "0"])).is_err());
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

    #[test]
    fn parse_full_scan_returns_true_when_present() {
        assert_eq!(
            parse_full_scan(&args(&["mirror-client", "scan-hetzner", "--full-scan"])),
            Some(true)
        );
    }

    #[test]
    fn parse_full_scan_returns_none_when_absent() {
        assert_eq!(
            parse_full_scan(&args(&["mirror-client", "scan-hetzner"])),
            None
        );
    }

    #[test]
    fn parses_after_flag() {
        let a = args(&["bin", "media-feed", "--after", "42"]);
        assert_eq!(parse_after(&a), Some(42));
        let none = args(&["bin", "media-feed"]);
        assert_eq!(parse_after(&none), None);
    }
}
