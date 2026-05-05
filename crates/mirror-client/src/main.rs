use mirror_client::MirrorClient;

const USAGE: &str = "Usage: mirror-client <command> [--limit N]

Commands:
  audit           Print index statistics
  scan-storj      Scan Storj bucket into index
  scan-hetzner    Scan Hetzner bucket into index
  phash           Compute missing perceptual hashes
  mirror          Copy pending videos from Hetzner → Storj

Options:
  --limit N       Stop after processing N items (scan/phash/mirror commands only)

Environment:
  MIRROR_SERVICE_URL    Base URL of the service (required)
  SERVICE_SECRET_TOKEN  Shared HMAC signing secret (required)";

fn parse_limit(args: &[String]) -> Option<u64> {
    args.windows(2)
        .find(|w| w[0] == "--limit")
        .and_then(|w| w[1].parse().ok())
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("");
    let limit = parse_limit(&args);

    let url = std::env::var("MIRROR_SERVICE_URL")
        .expect("MIRROR_SERVICE_URL must be set");
    let secret = std::env::var("SERVICE_SECRET_TOKEN")
        .expect("SERVICE_SECRET_TOKEN must be set");

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
                        println!("  {} → {:?}", d.phash, d.video_ids);
                    }
                }
                Ok(())
            }
            Err(e) => Err(e),
        },
        "scan-storj" => client.scan_storj(limit).await.map(|_| println!("scan-storj accepted")),
        "scan-hetzner" => client.scan_hetzner(limit).await.map(|_| println!("scan-hetzner accepted")),
        "phash" => client.phash_backfill(limit).await.map(|_| println!("phash accepted")),
        "mirror" => client.mirror(limit).await.map(|_| println!("mirror accepted")),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_limit_returns_value() {
        assert_eq!(parse_limit(&args(&["mirror-client", "mirror", "--limit", "10"])), Some(10));
    }

    #[test]
    fn parse_limit_returns_none_when_absent() {
        assert_eq!(parse_limit(&args(&["mirror-client", "mirror"])), None);
    }

    #[test]
    fn parse_limit_returns_none_for_invalid_value() {
        assert_eq!(parse_limit(&args(&["mirror-client", "mirror", "--limit", "abc"])), None);
    }

    #[test]
    fn parse_limit_requires_value_after_flag() {
        assert_eq!(parse_limit(&args(&["mirror-client", "--limit"])), None);
    }
}
