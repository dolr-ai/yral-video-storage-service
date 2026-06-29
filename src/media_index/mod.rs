#[allow(dead_code)]
pub mod feed;
#[allow(dead_code)]
pub mod repo;
pub mod schema;
#[allow(dead_code)]
pub mod types;

#[allow(unused_imports)]
pub use feed::{append_feed_event_txn, list_feed_events_after, FeedReadError};
#[allow(unused_imports)]
pub use repo::{
    acquire_or_renew_lease, canonical_phash_coverage, failure_summary, find_exact_duplicates,
    get_last_discovery_at, read_lease, recent_job_runs, release_lease, set_last_discovery_at,
    update_job_run_totals, upsert_hash_record_txn, upsert_servable_video_txn,
    videos_missing_canonical_phash, CoverageStats, FailureGroup, JobRunRow, LeaseRow,
    MissingHashRow,
};
pub use schema::init_schema;
pub use types::*;

#[cfg(test)]
#[allow(unused_imports)]
pub use repo::upsert_servable_video;

#[cfg(test)]
pub(crate) mod test_support {
    use std::net::TcpListener;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio_postgres::{Client, NoTls};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    pub(crate) struct PgContainer {
        name: String,
    }

    impl PgContainer {
        pub(crate) async fn spawn() -> (Self, String) {
            let port = TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let name = format!(
                "media-index-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            );
            let status = Command::new("docker")
                .args([
                    "run",
                    "--rm",
                    "--detach",
                    "--name",
                    &name,
                    "-e",
                    "POSTGRES_PASSWORD=test",
                    "-e",
                    "POSTGRES_USER=test",
                    "-e",
                    "POSTGRES_DB=test",
                    "-p",
                    &format!("{port}:5432"),
                    "postgres:16-alpine",
                ])
                .status()
                .expect("docker run");
            if !status.success() {
                panic!(
                    "failed to start postgres test container with docker run (status: {status}); is Docker running?"
                );
            }
            let url = format!("postgres://test:test@127.0.0.1:{port}/test");
            let mut connected = false;
            for _ in 0..20 {
                if tokio_postgres::connect(&url, NoTls).await.is_ok() {
                    connected = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            if !connected {
                panic!("postgres test container did not accept connections at {url}");
            }
            (Self { name }, url)
        }
    }

    impl Drop for PgContainer {
        fn drop(&mut self) {
            Command::new("docker")
                .args(["rm", "-f", &self.name])
                .status()
                .ok();
        }
    }

    pub(crate) async fn test_client() -> (PgContainer, Client) {
        let (pg, url) = PgContainer::spawn().await;
        let client = connect_test_client(&url).await;
        (pg, client)
    }

    pub(crate) async fn connect_test_client(url: &str) -> Client {
        let (client, connection) = tokio_postgres::connect(url, NoTls).await.unwrap();
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::error!(error = %e, "postgres test connection closed");
            }
        });
        client
    }
}
