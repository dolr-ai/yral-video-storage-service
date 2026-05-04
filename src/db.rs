use tokio_postgres::{Client, NoTls};

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS video_index (
    video_id      TEXT PRIMARY KEY,
    hetzner_key   TEXT,
    storj_key     TEXT,
    phash         TEXT,
    is_temp       BOOLEAN NOT NULL DEFAULT FALSE,
    retry_count   INTEGER NOT NULL DEFAULT 0,
    status        TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending','phash_computed','mirrored','failed','done')),
    error_message TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_phash_null ON video_index (video_id, hetzner_key)
    WHERE phash IS NULL AND hetzner_key IS NOT NULL AND status != 'failed';

CREATE INDEX IF NOT EXISTS idx_missing_storj ON video_index (video_id, hetzner_key)
    WHERE storj_key IS NULL AND is_temp = FALSE
      AND hetzner_key IS NOT NULL AND status = 'phash_computed';

CREATE INDEX IF NOT EXISTS idx_temp_cleanup ON video_index (video_id)
    WHERE is_temp = TRUE AND status = 'mirrored';

CREATE INDEX IF NOT EXISTS idx_status ON video_index (status);

CREATE INDEX IF NOT EXISTS idx_phash_val ON video_index (phash)
    WHERE phash IS NOT NULL;

CREATE OR REPLACE FUNCTION update_updated_at()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN NEW.updated_at = NOW(); RETURN NEW; END;
$$;

DROP TRIGGER IF EXISTS video_index_updated_at ON video_index;
CREATE TRIGGER video_index_updated_at
    BEFORE UPDATE ON video_index
    FOR EACH ROW EXECUTE FUNCTION update_updated_at();
";

pub struct VideoRow {
    pub video_id: String,
    pub hetzner_key: String,
}

pub struct CleanupRow {
    pub video_id: String,
    pub hetzner_key: String,
    pub storj_key: String,
}

pub struct AuditStats {
    pub total: i64,
    pub phash_computed: i64,
    pub mirrored: i64,
    pub missing_storj: i64,
    pub missing_hetzner: i64,
    pub cleanup_pending: i64,
    pub failed: i64,
    pub error_count: i64,
}

pub struct DuplicatePhash {
    pub phash: String,
    pub video_ids: Vec<String>,
}

pub async fn connect(url: &str) -> Result<Client, tokio_postgres::Error> {
    let (client, connection) = tokio_postgres::connect(url, NoTls).await?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            tracing::error!(error = %e, "postgres connection closed");
        }
    });
    Ok(client)
}

pub async fn init_schema(client: &Client) -> Result<(), tokio_postgres::Error> {
    client.batch_execute(SCHEMA_SQL).await
}

// --- Job 0: Scan Storj ---

pub async fn upsert_storj_key(
    client: &Client,
    video_id: &str,
    storj_key: &str,
) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            "INSERT INTO video_index (video_id, storj_key)
             VALUES ($1, $2)
             ON CONFLICT (video_id) DO UPDATE
             SET storj_key = EXCLUDED.storj_key,
                 error_message = NULL",
            &[&video_id, &storj_key],
        )
        .await?;
    Ok(())
}

// --- Job 1: Scan Hetzner ---

pub async fn upsert_hetzner_key(
    client: &Client,
    video_id: &str,
    hetzner_key: &str,
    is_temp: bool,
) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            "INSERT INTO video_index (video_id, hetzner_key, is_temp)
             VALUES ($1, $2, $3)
             ON CONFLICT (video_id) DO UPDATE
             SET hetzner_key = EXCLUDED.hetzner_key,
                 is_temp = EXCLUDED.is_temp,
                 error_message = NULL",
            &[&video_id, &hetzner_key, &is_temp],
        )
        .await?;
    Ok(())
}

// --- Job 2: Phash Backfill ---

pub async fn fetch_pending_phash_batch(
    client: &Client,
    limit: i64,
) -> Result<Vec<VideoRow>, tokio_postgres::Error> {
    let rows = client
        .query(
            "SELECT video_id, hetzner_key FROM video_index
             WHERE phash IS NULL
               AND hetzner_key IS NOT NULL
               AND status = 'pending'
             LIMIT $1",
            &[&limit],
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| VideoRow {
            video_id: r.get(0),
            hetzner_key: r.get(1),
        })
        .collect())
}

pub async fn update_phash_success(
    client: &Client,
    video_id: &str,
    phash: &str,
) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            "UPDATE video_index
             SET phash = $1, status = 'phash_computed', error_message = NULL
             WHERE video_id = $2 AND status = 'pending'",
            &[&phash, &video_id],
        )
        .await?;
    Ok(())
}

/// Atomically increments retry_count and sets status to 'failed' when threshold reached.
/// Returns the new retry_count.
pub async fn update_phash_failure(
    client: &Client,
    video_id: &str,
    error: &str,
    max_retries: i32,
) -> Result<i32, tokio_postgres::Error> {
    let row = client
        .query_one(
            "UPDATE video_index
             SET retry_count = retry_count + 1,
                 status = CASE WHEN retry_count + 1 >= $3
                               THEN 'failed' ELSE 'pending' END,
                 error_message = $2
             WHERE video_id = $1
             RETURNING retry_count",
            &[&video_id, &error, &max_retries],
        )
        .await?;
    Ok(row.get(0))
}

// --- Job 3: Mirror ---

pub async fn fetch_pending_mirror_batch(
    client: &Client,
    limit: i64,
) -> Result<Vec<VideoRow>, tokio_postgres::Error> {
    let rows = client
        .query(
            "SELECT video_id, hetzner_key FROM video_index
             WHERE storj_key IS NULL
               AND hetzner_key IS NOT NULL
               AND is_temp = FALSE
               AND status = 'phash_computed'
             LIMIT $1",
            &[&limit],
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| VideoRow {
            video_id: r.get(0),
            hetzner_key: r.get(1),
        })
        .collect())
}

pub async fn update_mirror_success(
    client: &Client,
    video_id: &str,
    storj_key: &str,
) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            "UPDATE video_index
             SET storj_key = $1, status = 'mirrored', error_message = NULL
             WHERE video_id = $2 AND status = 'phash_computed'",
            &[&storj_key, &video_id],
        )
        .await?;
    Ok(())
}

pub async fn update_error(
    client: &Client,
    video_id: &str,
    error: &str,
) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            "UPDATE video_index SET error_message = $1 WHERE video_id = $2",
            &[&error, &video_id],
        )
        .await?;
    Ok(())
}

// --- Job 4: Cleanup ---

pub async fn fetch_pending_cleanup_batch(
    client: &Client,
    limit: i64,
) -> Result<Vec<CleanupRow>, tokio_postgres::Error> {
    let rows = client
        .query(
            "SELECT video_id, hetzner_key, storj_key FROM video_index
             WHERE is_temp = TRUE AND status = 'mirrored'
               AND hetzner_key IS NOT NULL AND storj_key IS NOT NULL
             LIMIT $1",
            &[&limit],
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| CleanupRow {
            video_id: r.get(0),
            hetzner_key: r.get(1),
            storj_key: r.get(2),
        })
        .collect())
}

pub async fn update_cleanup_done(
    client: &Client,
    video_id: &str,
) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            "UPDATE video_index
             SET hetzner_key = NULL, is_temp = FALSE,
                 status = 'done', error_message = NULL
             WHERE video_id = $1",
            &[&video_id],
        )
        .await?;
    Ok(())
}

// --- Audit ---

pub async fn get_audit_stats(client: &Client) -> Result<AuditStats, tokio_postgres::Error> {
    let row = client
        .query_one(
            "SELECT
                COUNT(*),
                COUNT(phash),
                COUNT(*) FILTER (WHERE status IN ('mirrored','done')),
                COUNT(*) FILTER (WHERE storj_key IS NULL AND is_temp = FALSE),
                COUNT(*) FILTER (WHERE hetzner_key IS NULL AND is_temp = FALSE AND status != 'done'),
                COUNT(*) FILTER (WHERE is_temp = TRUE AND status = 'mirrored'),
                COUNT(*) FILTER (WHERE status = 'failed'),
                COUNT(*) FILTER (WHERE error_message IS NOT NULL)
             FROM video_index",
            &[],
        )
        .await?;

    Ok(AuditStats {
        total: row.get(0),
        phash_computed: row.get(1),
        mirrored: row.get(2),
        missing_storj: row.get(3),
        missing_hetzner: row.get(4),
        cleanup_pending: row.get(5),
        failed: row.get(6),
        error_count: row.get(7),
    })
}

pub async fn get_duplicate_phashes(
    client: &Client,
) -> Result<Vec<DuplicatePhash>, tokio_postgres::Error> {
    let rows = client
        .query(
            "SELECT phash, array_agg(video_id) FROM video_index
             WHERE phash IS NOT NULL
             GROUP BY phash HAVING COUNT(*) > 1
             LIMIT 100",
            &[],
        )
        .await?;

    Ok(rows
        .into_iter()
        .map(|r| DuplicatePhash {
            phash: r.get(0),
            video_ids: r.get(1),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct PgContainer {
        name: String,
    }

    impl PgContainer {
        async fn spawn() -> (Self, String) {
            let port = TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let name = format!(
                "mirror-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            );
            Command::new("docker")
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
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let url = format!("postgres://test:test@127.0.0.1:{port}/test");
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

    #[tokio::test]
    async fn schema_init_is_idempotent() {
        let (pg, url) = PgContainer::spawn().await;
        let client = connect(&url).await.unwrap();
        init_schema(&client).await.unwrap();
        init_schema(&client).await.unwrap(); // second call must not error
        drop(pg);
    }

    #[tokio::test]
    async fn upsert_storj_key_overwrites_existing() {
        let (pg, url) = PgContainer::spawn().await;
        let client = connect(&url).await.unwrap();
        init_schema(&client).await.unwrap();

        upsert_storj_key(&client, "user/abc", "user/abc.mp4")
            .await
            .unwrap();
        upsert_storj_key(&client, "user/abc", "user/abc-v2.mp4")
            .await
            .unwrap();

        let row = client
            .query_one(
                "SELECT storj_key FROM video_index WHERE video_id = $1",
                &[&"user/abc"],
            )
            .await
            .unwrap();
        let storj_key: String = row.get(0);
        assert_eq!(storj_key, "user/abc-v2.mp4");
        drop(pg);
    }

    #[tokio::test]
    async fn phash_failure_increments_retry_count_atomically() {
        let (pg, url) = PgContainer::spawn().await;
        let client = connect(&url).await.unwrap();
        init_schema(&client).await.unwrap();

        upsert_hetzner_key(&client, "user/vid", "user/vid.mp4", false)
            .await
            .unwrap();
        let r1 = update_phash_failure(&client, "user/vid", "err", 5)
            .await
            .unwrap();
        let r2 = update_phash_failure(&client, "user/vid", "err", 5)
            .await
            .unwrap();
        assert_eq!(r1, 1);
        assert_eq!(r2, 2);
        drop(pg);
    }

    #[tokio::test]
    async fn phash_failure_marks_failed_at_max_retries() {
        let (pg, url) = PgContainer::spawn().await;
        let client = connect(&url).await.unwrap();
        init_schema(&client).await.unwrap();

        upsert_hetzner_key(&client, "user/vid2", "user/vid2.mp4", false)
            .await
            .unwrap();
        for _ in 0..5 {
            update_phash_failure(&client, "user/vid2", "err", 5)
                .await
                .unwrap();
        }

        let stats = get_audit_stats(&client).await.unwrap();
        assert_eq!(stats.failed, 1);
        drop(pg);
    }

    #[tokio::test]
    async fn audit_stats_counts_correctly() {
        let (pg, url) = PgContainer::spawn().await;
        let client = connect(&url).await.unwrap();
        init_schema(&client).await.unwrap();

        upsert_hetzner_key(&client, "user/a", "user/a.mp4", false)
            .await
            .unwrap();
        upsert_storj_key(&client, "user/b", "user/b.mp4")
            .await
            .unwrap();

        let stats = get_audit_stats(&client).await.unwrap();
        assert_eq!(stats.total, 2);
        assert_eq!(stats.missing_storj, 1);
        drop(pg);
    }
}
