use tokio_postgres::{Client, NoTls};

const SCHEMA_SQL: &str = "
-- Create tables first so that the migration ALTER statements have something to target.
CREATE TABLE IF NOT EXISTS video_index (
    video_id    TEXT PRIMARY KEY,
    storj_key   TEXT,
    hetzner_key TEXT,
    phash       TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_phash_val ON video_index (phash)
    WHERE phash IS NOT NULL;

-- Migration: columns moved from video_index to mirror_jobs table.
-- IF EXISTS makes these no-ops on a fresh DB.
ALTER TABLE video_index DROP COLUMN IF EXISTS is_temp;
ALTER TABLE video_index DROP COLUMN IF EXISTS retry_count;
ALTER TABLE video_index DROP COLUMN IF EXISTS status;
ALTER TABLE video_index DROP COLUMN IF EXISTS error_message;
ALTER TABLE video_index DROP COLUMN IF EXISTS updated_at;

CREATE TABLE IF NOT EXISTS mirror_jobs (
    video_id      TEXT PRIMARY KEY REFERENCES video_index(video_id),
    is_temp       BOOLEAN NOT NULL DEFAULT FALSE,
    retry_count   INTEGER NOT NULL DEFAULT 0,
    status        TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending','phash_computed','mirrored','failed','done')),
    error_message TEXT,
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_status ON mirror_jobs (status);

CREATE INDEX IF NOT EXISTS idx_phash_pending ON mirror_jobs (video_id)
    WHERE status = 'pending';

CREATE INDEX IF NOT EXISTS idx_mirror_pending ON mirror_jobs (video_id)
    WHERE is_temp = FALSE AND status = 'phash_computed';

CREATE INDEX IF NOT EXISTS idx_temp_cleanup ON mirror_jobs (video_id)
    WHERE is_temp = TRUE AND status = 'mirrored';

CREATE OR REPLACE FUNCTION update_updated_at()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN NEW.updated_at = NOW(); RETURN NEW; END;
$$;

DROP TRIGGER IF EXISTS mirror_jobs_updated_at ON mirror_jobs;
CREATE TRIGGER mirror_jobs_updated_at
    BEFORE UPDATE ON mirror_jobs
    FOR EACH ROW EXECUTE FUNCTION update_updated_at();
";

pub struct VideoRow {
    pub video_id: String,
    pub hetzner_key: String,
}

#[allow(dead_code)]
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

pub struct DuplicateVideo {
    pub video_id: String,
    pub storj_key: Option<String>,
    pub hetzner_key: Option<String>,
}

pub struct DuplicatePhash {
    pub phash: String,
    pub videos: Vec<DuplicateVideo>,
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
             SET storj_key = EXCLUDED.storj_key",
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
    // Atomic: upsert video_index, then upsert mirror_jobs.
    // CTE ensures both run in one statement.
    client
        .execute(
            "WITH vi AS (
                INSERT INTO video_index (video_id, hetzner_key)
                VALUES ($1, $2)
                ON CONFLICT (video_id) DO UPDATE
                SET hetzner_key = EXCLUDED.hetzner_key
             )
             INSERT INTO mirror_jobs (video_id, is_temp)
             VALUES ($1, $3)
             ON CONFLICT (video_id) DO UPDATE
             SET is_temp = EXCLUDED.is_temp",
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
            "SELECT vi.video_id, vi.hetzner_key
             FROM mirror_jobs mj
             JOIN video_index vi ON vi.video_id = mj.video_id
             WHERE mj.status = 'pending'
               AND vi.hetzner_key IS NOT NULL
               AND vi.phash IS NULL
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
            "WITH _ AS (
                UPDATE video_index SET phash = $1 WHERE video_id = $2
             )
             UPDATE mirror_jobs
             SET status = 'phash_computed', error_message = NULL
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
            "UPDATE mirror_jobs
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
            "SELECT vi.video_id, vi.hetzner_key
             FROM mirror_jobs mj
             JOIN video_index vi ON vi.video_id = mj.video_id
             WHERE mj.status = 'phash_computed'
               AND mj.is_temp = FALSE
               AND vi.storj_key IS NULL
               AND vi.hetzner_key IS NOT NULL
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
            "WITH _ AS (
                UPDATE video_index SET storj_key = $1 WHERE video_id = $2
             )
             UPDATE mirror_jobs
             SET status = 'mirrored', error_message = NULL
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
            "UPDATE mirror_jobs SET error_message = $1 WHERE video_id = $2",
            &[&error, &video_id],
        )
        .await?;
    Ok(())
}

// --- Job 4: Cleanup ---

#[allow(dead_code)]
pub async fn fetch_pending_cleanup_batch(
    client: &Client,
    limit: i64,
) -> Result<Vec<CleanupRow>, tokio_postgres::Error> {
    let rows = client
        .query(
            "SELECT vi.video_id, vi.hetzner_key, vi.storj_key
             FROM mirror_jobs mj
             JOIN video_index vi ON vi.video_id = mj.video_id
             WHERE mj.is_temp = TRUE AND mj.status = 'mirrored'
               AND vi.hetzner_key IS NOT NULL AND vi.storj_key IS NOT NULL
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

#[allow(dead_code)]
pub async fn update_cleanup_done(
    client: &Client,
    video_id: &str,
) -> Result<(), tokio_postgres::Error> {
    client
        .execute(
            "WITH _ AS (
                UPDATE video_index SET hetzner_key = NULL WHERE video_id = $1
             )
             UPDATE mirror_jobs
             SET is_temp = FALSE, status = 'done', error_message = NULL
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
                COUNT(vi.phash),
                COUNT(*) FILTER (WHERE mj.status IN ('mirrored','done')),
                COUNT(*) FILTER (WHERE vi.storj_key IS NULL AND mj.is_temp = FALSE),
                COUNT(*) FILTER (WHERE vi.hetzner_key IS NULL AND mj.is_temp = FALSE AND mj.status != 'done'),
                COUNT(*) FILTER (WHERE mj.is_temp = TRUE AND mj.status = 'mirrored'),
                COUNT(*) FILTER (WHERE mj.status = 'failed'),
                COUNT(*) FILTER (WHERE mj.error_message IS NOT NULL)
             FROM video_index vi
             LEFT JOIN mirror_jobs mj ON vi.video_id = mj.video_id",
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
            "SELECT vi.phash,
                    array_agg(vi.video_id   ORDER BY vi.video_id) AS video_ids,
                    array_agg(vi.storj_key  ORDER BY vi.video_id) AS storj_keys,
                    array_agg(vi.hetzner_key ORDER BY vi.video_id) AS hetzner_keys
             FROM video_index vi
             WHERE vi.phash IS NOT NULL
             GROUP BY vi.phash
             HAVING COUNT(*) > 1
             ORDER BY COUNT(*) DESC
             LIMIT 100",
            &[],
        )
        .await?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let phash: String = r.get(0);
            let video_ids: Vec<String> = r.get(1);
            let storj_keys: Vec<Option<String>> = r.get(2);
            let hetzner_keys: Vec<Option<String>> = r.get(3);
            let videos = video_ids
                .into_iter()
                .zip(storj_keys)
                .zip(hetzner_keys)
                .map(|((video_id, storj_key), hetzner_key)| DuplicateVideo {
                    video_id,
                    storj_key,
                    hetzner_key,
                })
                .collect();
            DuplicatePhash { phash, videos }
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
            let url = format!("postgres://test:test@127.0.0.1:{port}/test");
            for _ in 0..20 {
                if tokio_postgres::connect(&url, tokio_postgres::NoTls)
                    .await
                    .is_ok()
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
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

    #[tokio::test]
    async fn schema_init_is_idempotent() {
        let (pg, url) = PgContainer::spawn().await;
        let client = connect(&url).await.unwrap();
        init_schema(&client).await.unwrap();
        init_schema(&client).await.unwrap();
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
    async fn upsert_hetzner_key_creates_mirror_job() {
        let (pg, url) = PgContainer::spawn().await;
        let client = connect(&url).await.unwrap();
        init_schema(&client).await.unwrap();

        upsert_hetzner_key(&client, "user/vid", "user/vid.mp4", false)
            .await
            .unwrap();

        let row = client
            .query_one(
                "SELECT vi.hetzner_key, mj.status, mj.is_temp
                 FROM video_index vi JOIN mirror_jobs mj ON vi.video_id = mj.video_id
                 WHERE vi.video_id = $1",
                &[&"user/vid"],
            )
            .await
            .unwrap();
        assert_eq!(row.get::<_, String>(0), "user/vid.mp4");
        assert_eq!(row.get::<_, String>(1), "pending");
        assert!(!row.get::<_, bool>(2));
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

    #[tokio::test]
    async fn phash_success_updates_both_tables() {
        let (pg, url) = PgContainer::spawn().await;
        let client = connect(&url).await.unwrap();
        init_schema(&client).await.unwrap();

        upsert_hetzner_key(&client, "user/vid", "user/vid.mp4", false)
            .await
            .unwrap();
        update_phash_success(&client, "user/vid", "aabbccdd")
            .await
            .unwrap();

        let row = client
            .query_one(
                "SELECT vi.phash, mj.status FROM video_index vi
                 JOIN mirror_jobs mj ON vi.video_id = mj.video_id
                 WHERE vi.video_id = $1",
                &[&"user/vid"],
            )
            .await
            .unwrap();
        assert_eq!(row.get::<_, String>(0), "aabbccdd");
        assert_eq!(row.get::<_, String>(1), "phash_computed");
        drop(pg);
    }

    #[tokio::test]
    async fn mirror_success_updates_both_tables() {
        let (pg, url) = PgContainer::spawn().await;
        let client = connect(&url).await.unwrap();
        init_schema(&client).await.unwrap();

        upsert_hetzner_key(&client, "user/vid", "user/vid.mp4", false)
            .await
            .unwrap();
        update_phash_success(&client, "user/vid", "aabbccdd")
            .await
            .unwrap();
        update_mirror_success(&client, "user/vid", "user/vid.mp4")
            .await
            .unwrap();

        let row = client
            .query_one(
                "SELECT vi.storj_key, mj.status FROM video_index vi
                 JOIN mirror_jobs mj ON vi.video_id = mj.video_id
                 WHERE vi.video_id = $1",
                &[&"user/vid"],
            )
            .await
            .unwrap();
        assert_eq!(row.get::<_, String>(0), "user/vid.mp4");
        assert_eq!(row.get::<_, String>(1), "mirrored");
        drop(pg);
    }

    #[tokio::test]
    async fn get_duplicate_phashes_returns_keys() {
        let (pg, url) = PgContainer::spawn().await;
        let client = connect(&url).await.unwrap();
        init_schema(&client).await.unwrap();

        upsert_storj_key(&client, "user/a", "user/a.mp4")
            .await
            .unwrap();
        upsert_storj_key(&client, "user/b", "user/b.mp4")
            .await
            .unwrap();
        client
            .execute(
                "UPDATE video_index SET phash = 'deadbeef' WHERE video_id = ANY($1)",
                &[&vec!["user/a", "user/b"]],
            )
            .await
            .unwrap();

        let dups = get_duplicate_phashes(&client).await.unwrap();
        assert_eq!(dups.len(), 1);
        assert_eq!(dups[0].phash, "deadbeef");
        assert_eq!(dups[0].videos.len(), 2);
        assert!(dups[0].videos.iter().any(|v| v.video_id == "user/a"));
        drop(pg);
    }
}
