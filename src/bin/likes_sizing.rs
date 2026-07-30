//! Task 0 pre-flight (see `docs/superpowers/plans/2026-07-29-canister-data-migration-phase1.md`).
//!
//! Walks `user_post_service.fetch_posts` and reports the distribution of
//! `Post.likes` set sizes, so the `post_likes` row estimate in the design spec
//! stops being a guess. Spec § "Volume, and why the backfill needs a bulk path".
//!
//! `fetch_posts` is an unguarded candid `query`, so this runs with an anonymous
//! agent — no `BACKEND_ADMIN_IDENTITY` required. Read-only; writes nothing.
//!
//! Throwaway: delete once the number is recorded in the spec.
//!
//! Usage:
//!   cargo run --bin likes_sizing                  # 300 pages of 100
//!   PAGES=50 PAGE_SIZE=25 cargo run --bin likes_sizing

use ic_agent::Agent;
use storj_interface::jobs::chain_snapshot::{walk_step, LivePostSource, PostPageSource};

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Nearest-rank percentile over a sorted slice. Empty slice -> 0.
fn percentile(sorted: &[usize], p: f64) -> usize {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let max_pages = env_usize("PAGES", 300);
    let page_size = env_usize("PAGE_SIZE", 100) as u64;
    let ic_url = std::env::var("IC_URL").unwrap_or_else(|_| "https://ic0.app".to_string());

    let agent = Agent::builder().with_url(&ic_url).build()?;
    let source = LivePostSource(&agent);

    let mut like_counts: Vec<usize> = Vec::new();
    let mut total_likes: usize = 0;
    let mut max_post: (usize, String) = (0, String::new());
    let mut max_page_likes: (usize, usize) = (0, 0); // (likes_in_page, page_index)
    let mut cursor: Option<String> = None;
    let mut pages = 0usize;
    let mut empty_video_uid = 0usize;

    println!("walking up to {max_pages} pages of {page_size} from {ic_url}\n");

    while pages < max_pages {
        let res = match source.fetch(page_size, cursor.clone()).await {
            Ok(r) => r,
            Err(e) => {
                // A decode/transport failure here is itself a finding: it is the
                // page-size hazard the spec warns about (a page carries whole
                // like-sets, so one viral post can blow up the payload).
                eprintln!("page {pages} failed at page_size={page_size}: {e}");
                eprintln!("retry with a smaller PAGE_SIZE; partial results below");
                break;
            }
        };
        pages += 1;

        let mut page_likes = 0usize;
        for p in &res.posts {
            if p.video_uid.is_empty() {
                empty_video_uid += 1;
            }
            let n = p.likes.len();
            like_counts.push(n);
            total_likes += n;
            page_likes += n;
            if n > max_post.0 {
                max_post = (n, p.id.clone());
            }
        }
        if page_likes > max_page_likes.0 {
            max_page_likes = (page_likes, pages);
        }

        if pages.is_multiple_of(25) {
            println!(
                "  {pages} pages, {} posts, {total_likes} likes so far",
                like_counts.len()
            );
        }

        let (stop, next) = walk_step(&cursor, &res, page_size);
        if stop {
            println!("\nreached end of corpus after {pages} pages");
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cursor = next;
    }

    let posts = like_counts.len();
    if posts == 0 {
        anyhow::bail!("no posts sampled — nothing to report");
    }
    like_counts.sort_unstable();
    let mean = total_likes as f64 / posts as f64;

    // Master-table corpus size, per spec § Capacity.
    const CORPUS: f64 = 583_000.0;

    println!("\n=== like-set distribution (sample) ===");
    println!("pages walked          {pages}");
    println!("posts sampled         {posts}");
    println!("posts w/ empty uid    {empty_video_uid}");
    println!("total likes           {total_likes}");
    println!("mean likes/post       {mean:.2}");
    println!(
        "p50 / p95 / p99       {} / {} / {}",
        percentile(&like_counts, 50.0),
        percentile(&like_counts, 95.0),
        percentile(&like_counts, 99.0)
    );
    println!("max likes on a post   {} (post {})", max_post.0, max_post.1);
    println!(
        "max likes in a page   {} (page {})",
        max_page_likes.0, max_page_likes.1
    );
    println!(
        "zero-like posts       {}",
        like_counts.iter().filter(|&&n| n == 0).count()
    );

    let projected = mean * CORPUS;
    println!("\n=== projection ===");
    println!("post_likes rows ≈ {mean:.2} × {CORPUS:.0} = {projected:.0}");
    println!(
        "\nDECISION (plan Task 0 Step 3): {}",
        if projected > 50_000_000.0 {
            "EXCEEDS 50M — re-scope. Drop Tasks 7 and 19, keep like_count only."
        } else {
            "under 50M — keep per-principal post_likes in Phase 1."
        }
    );
    println!(
        "\nNOTE: sample is the head of the corpus (cursor order), not uniform.\n\
              Treat the projection as an order of magnitude, not a precise count."
    );

    Ok(())
}
