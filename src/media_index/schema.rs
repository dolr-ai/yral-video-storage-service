use tokio_postgres::Client;

const SCHEMA_SQL: &str = r#"
SELECT pg_advisory_xact_lock(904648332137142900);

CREATE TABLE IF NOT EXISTS all_servable_videos_on_yral (
    video_id TEXT PRIMARY KEY,
    publisher_user_id TEXT,
    post_id TEXT,
    source_kind TEXT NOT NULL,
    source_ref TEXT,
    servable_status TEXT NOT NULL,
    nsfw_state TEXT,
    storage_provider TEXT,
    bucket TEXT,
    object_key TEXT,
    canonical_url TEXT,
    thumbnail_key TEXT,
    duration_ms BIGINT,
    width INTEGER,
    height INTEGER,
    fps DOUBLE PRECISION,
    container TEXT,
    video_codec TEXT,
    audio_codec TEXT,
    moov_atom_front BOOLEAN,
    canonical_encoding_version TEXT,
    discovered_from TEXT NOT NULL,
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS servable_video_sources (
    id BIGSERIAL PRIMARY KEY,
    video_id TEXT NOT NULL REFERENCES all_servable_videos_on_yral(video_id),
    source_kind TEXT NOT NULL,
    source_ref TEXT NOT NULL,
    raw_payload JSONB,
    imported_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (video_id, source_kind, source_ref)
);

CREATE TABLE IF NOT EXISTS servable_video_hashes (
    video_id TEXT NOT NULL REFERENCES all_servable_videos_on_yral(video_id),
    hash_kind TEXT NOT NULL,
    hash_version TEXT NOT NULL,
    input_media_version TEXT NOT NULL,
    hash_value TEXT NOT NULL,
    hash_bit_length INTEGER NOT NULL,
    num_frames INTEGER NOT NULL,
    hash_size INTEGER NOT NULL,
    computed_from_provider TEXT,
    computed_from_bucket TEXT,
    computed_from_key TEXT,
    computed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    metadata JSONB,
    PRIMARY KEY (video_id, hash_kind, hash_version, input_media_version)
);

CREATE INDEX IF NOT EXISTS idx_servable_video_hash_exact
    ON servable_video_hashes (hash_kind, hash_version, hash_value);

CREATE TABLE IF NOT EXISTS media_feed_events (
    cursor BIGSERIAL PRIMARY KEY,
    event_kind TEXT NOT NULL,
    video_id TEXT NOT NULL REFERENCES all_servable_videos_on_yral(video_id),
    hash_kind TEXT,
    hash_version TEXT,
    input_media_version TEXT,
    payload JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_media_feed_events_hash_cursor
    ON media_feed_events (hash_kind, hash_version, cursor);

CREATE INDEX IF NOT EXISTS idx_media_feed_events_video
    ON media_feed_events (video_id);

CREATE TABLE IF NOT EXISTS media_job_runs (
    id UUID PRIMARY KEY,
    job_kind TEXT NOT NULL,
    status TEXT NOT NULL,
    requested_by TEXT NOT NULL,
    started_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at TIMESTAMPTZ,
    cursor JSONB,
    totals JSONB,
    error_message TEXT
);

ALTER TABLE media_job_runs ADD COLUMN IF NOT EXISTS cursor JSONB;
ALTER TABLE media_job_runs ADD COLUMN IF NOT EXISTS totals JSONB;
ALTER TABLE media_job_runs ADD COLUMN IF NOT EXISTS error_message TEXT;

CREATE TABLE IF NOT EXISTS media_job_failures (
    id BIGSERIAL PRIMARY KEY,
    job_run_id UUID REFERENCES media_job_runs(id) ON DELETE SET NULL,
    job_kind TEXT NOT NULL,
    item_key TEXT NOT NULL,
    video_id TEXT,
    phase TEXT NOT NULL,
    source_ref TEXT,
    retry_count INTEGER NOT NULL DEFAULT 0,
    last_error TEXT NOT NULL,
    next_retry_at TIMESTAMPTZ,
    status TEXT NOT NULL DEFAULT 'pending_retry',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (job_kind, item_key, phase)
);

ALTER TABLE media_job_failures ADD COLUMN IF NOT EXISTS job_kind TEXT;
ALTER TABLE media_job_failures ADD COLUMN IF NOT EXISTS item_key TEXT;
ALTER TABLE media_job_failures ADD COLUMN IF NOT EXISTS video_id TEXT;
ALTER TABLE media_job_failures ADD COLUMN IF NOT EXISTS phase TEXT;
ALTER TABLE media_job_failures ADD COLUMN IF NOT EXISTS source_ref TEXT;
ALTER TABLE media_job_failures ADD COLUMN IF NOT EXISTS status TEXT NOT NULL DEFAULT 'pending_retry';

CREATE INDEX IF NOT EXISTS idx_media_job_failures_retry
    ON media_job_failures (status, next_retry_at);

CREATE INDEX IF NOT EXISTS idx_media_job_failures_video
    ON media_job_failures (video_id);

CREATE OR REPLACE FUNCTION media_feed_events_require_append_helper()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF current_setting('media_index.feed_event_append_locked', true) IS DISTINCT FROM 'on' THEN
        RAISE EXCEPTION 'media_feed_events inserts must use append_feed_event_txn';
    END IF;

    RETURN NEW;
END;
$$;

CREATE OR REPLACE FUNCTION media_job_failures_touch_updated_at()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    NEW.updated_at = NOW();
    RETURN NEW;
END;
$$;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM pg_trigger
        WHERE tgname = 'media_feed_events_require_append_helper'
          AND tgrelid = 'media_feed_events'::regclass
    ) THEN
        CREATE TRIGGER media_feed_events_require_append_helper
            BEFORE INSERT ON media_feed_events
            FOR EACH ROW EXECUTE FUNCTION media_feed_events_require_append_helper();
    END IF;

    IF NOT EXISTS (
        SELECT 1
        FROM pg_trigger
        WHERE tgname = 'media_job_failures_touch_updated_at'
          AND tgrelid = 'media_job_failures'::regclass
    ) THEN
        CREATE TRIGGER media_job_failures_touch_updated_at
            BEFORE UPDATE ON media_job_failures
            FOR EACH ROW EXECUTE FUNCTION media_job_failures_touch_updated_at();
    END IF;
END;
$$;

CREATE TABLE IF NOT EXISTS sweep_lease (
    id                SMALLINT PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    owner             TEXT NOT NULL,
    heartbeat         TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_discovery_at TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS yral_posts (
    post_id TEXT PRIMARY KEY,
    video_uid TEXT NOT NULL,
    creator_principal TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    status TEXT NOT NULL,
    snapshot_run_id UUID,
    fetched_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    stale BOOLEAN NOT NULL DEFAULT FALSE
);
CREATE INDEX IF NOT EXISTS idx_yral_posts_video_uid ON yral_posts (video_uid);
CREATE INDEX IF NOT EXISTS idx_yral_posts_creator ON yral_posts (creator_principal);
CREATE INDEX IF NOT EXISTS idx_yral_posts_status ON yral_posts (status);
CREATE INDEX IF NOT EXISTS idx_yral_posts_created_at ON yral_posts (created_at);

CREATE TABLE IF NOT EXISTS yral_users (
    creator_principal TEXT PRIMARY KEY,
    post_count BIGINT NOT NULL,
    first_seen TIMESTAMPTZ,
    last_seen TIMESTAMPTZ
);

-- Functional indexes on the CANONICAL video key (lowercased, dashes stripped).
-- The chain-audit join compares video_uid <-> video_id on this normalized form
-- because storj keys (hence video_id) and chain video_uid appear both dashed and
-- undashed. Without these, the audit/diagnostic joins seq-scan the full master
-- table and time out on prod. `lower`+`replace` are IMMUTABLE, so indexable.
CREATE INDEX IF NOT EXISTS idx_master_video_id_norm
    ON all_servable_videos_on_yral (lower(replace(video_id, '-', '')));
CREATE INDEX IF NOT EXISTS idx_hashes_video_id_norm
    ON servable_video_hashes (lower(replace(video_id, '-', '')));
CREATE INDEX IF NOT EXISTS idx_video_index_video_id_norm
    ON video_index (lower(replace(video_id, '-', '')));
CREATE INDEX IF NOT EXISTS idx_yral_posts_video_uid_norm
    ON yral_posts (lower(replace(video_uid, '-', '')));
"#;

pub async fn init_schema(client: &Client) -> Result<(), tokio_postgres::Error> {
    client.batch_execute("BEGIN").await?;
    match client.batch_execute(SCHEMA_SQL).await {
        Ok(()) => client.batch_execute("COMMIT").await,
        Err(err) => {
            let _ = client.batch_execute("ROLLBACK").await;
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::media_index::test_support::test_client;

    #[test]
    fn schema_sql_uses_concurrency_safe_trigger_creation() {
        assert!(super::SCHEMA_SQL.contains("pg_advisory_xact_lock"));
        assert!(super::SCHEMA_SQL.contains("DO $$"));
        assert!(super::SCHEMA_SQL.contains("IF NOT EXISTS"));
        assert!(super::SCHEMA_SQL.contains("CREATE TRIGGER"));
        assert!(!super::SCHEMA_SQL.contains("DROP TRIGGER"));
    }

    #[test]
    fn schema_sql_uses_resumable_job_failure_contract() {
        assert!(super::SCHEMA_SQL.contains("cursor JSONB"));
        assert!(super::SCHEMA_SQL.contains("totals JSONB"));
        assert!(super::SCHEMA_SQL.contains("error_message TEXT"));
        assert!(super::SCHEMA_SQL.contains("job_kind TEXT NOT NULL"));
        assert!(super::SCHEMA_SQL.contains("item_key TEXT NOT NULL"));
        assert!(super::SCHEMA_SQL.contains("phase TEXT NOT NULL"));
        assert!(super::SCHEMA_SQL.contains("status TEXT NOT NULL DEFAULT 'pending_retry'"));
        assert!(super::SCHEMA_SQL.contains("UNIQUE (job_kind, item_key, phase)"));
        assert!(super::SCHEMA_SQL.contains("idx_media_job_failures_retry"));
        assert!(super::SCHEMA_SQL.contains("idx_media_job_failures_video"));
    }

    #[test]
    fn schema_sql_touches_job_failure_updated_at_on_update() {
        assert!(super::SCHEMA_SQL.contains("media_job_failures_touch_updated_at"));
        assert!(super::SCHEMA_SQL.contains("NEW.updated_at = NOW()"));
        assert!(super::SCHEMA_SQL.contains("BEFORE UPDATE ON media_job_failures"));
    }

    #[test]
    fn schema_sql_has_idempotent_column_migrations_for_existing_tables() {
        assert!(super::SCHEMA_SQL
            .contains("ALTER TABLE media_job_runs ADD COLUMN IF NOT EXISTS cursor JSONB"));
        assert!(super::SCHEMA_SQL
            .contains("ALTER TABLE media_job_runs ADD COLUMN IF NOT EXISTS totals JSONB"));
        assert!(super::SCHEMA_SQL
            .contains("ALTER TABLE media_job_runs ADD COLUMN IF NOT EXISTS error_message TEXT"));
        assert!(super::SCHEMA_SQL
            .contains("ALTER TABLE media_job_failures ADD COLUMN IF NOT EXISTS job_kind TEXT"));
        assert!(super::SCHEMA_SQL
            .contains("ALTER TABLE media_job_failures ADD COLUMN IF NOT EXISTS item_key TEXT"));
        assert!(super::SCHEMA_SQL
            .contains("ALTER TABLE media_job_failures ADD COLUMN IF NOT EXISTS phase TEXT"));
        assert!(super::SCHEMA_SQL
            .contains("ALTER TABLE media_job_failures ADD COLUMN IF NOT EXISTS status TEXT"));
    }

    #[tokio::test]
    async fn schema_initialization_creates_all_first_slice_tables() {
        let (_pg, client) = test_client().await;

        super::init_schema(&client).await.unwrap();
        super::init_schema(&client).await.unwrap();

        let rows = client
            .query(
                "SELECT table_name
                 FROM information_schema.tables
                 WHERE table_schema = 'public'
                   AND table_name = ANY($1)
                 ORDER BY table_name",
                &[&vec![
                    "all_servable_videos_on_yral",
                    "servable_video_sources",
                    "servable_video_hashes",
                    "media_feed_events",
                    "media_job_runs",
                    "media_job_failures",
                ]],
            )
            .await
            .unwrap();
        let table_names: Vec<String> = rows.into_iter().map(|row| row.get(0)).collect();

        assert_eq!(
            table_names,
            vec![
                "all_servable_videos_on_yral",
                "media_feed_events",
                "media_job_failures",
                "media_job_runs",
                "servable_video_hashes",
                "servable_video_sources",
            ]
        );
    }

    #[tokio::test]
    async fn schema_initialization_can_run_concurrently() {
        let (pg, url) = crate::media_index::test_support::PgContainer::spawn().await;
        let client_a = crate::media_index::test_support::connect_test_client(&url).await;
        let client_b = crate::media_index::test_support::connect_test_client(&url).await;

        let (result_a, result_b) =
            tokio::join!(super::init_schema(&client_a), super::init_schema(&client_b));

        result_a.unwrap();
        result_b.unwrap();
        drop(pg);
    }

    #[tokio::test]
    async fn sweep_lease_table_created_by_schema() {
        let (_pg, client) = test_client().await;
        crate::media_index::init_schema(&client).await.unwrap();
        let row = client
            .query_one(
                "SELECT to_regclass('public.sweep_lease') IS NOT NULL AS exists",
                &[],
            )
            .await
            .unwrap();
        assert!(row.get::<_, bool>("exists"), "sweep_lease must exist");
    }
}
