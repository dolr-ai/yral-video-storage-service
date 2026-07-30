-- V1__baseline.sql — the pre-refinery schema, VERBATIM.
--
-- Concatenation of the three hand-rolled SCHEMA_SQL constants that were
-- replayed on every boot before refinery was adopted:
--   1. src/db.rs                      (video_index, mirror_jobs)
--   2. src/media_index/schema.rs      (master, hashes, feed, jobs, chain)
--   3. src/videogen/request_store.rs  (videogen_requests)
--
-- DO NOT TIDY THIS FILE. Its only job is to reproduce exactly what production
-- already has, warts included, so that a fresh database and an adopted one end
-- up structurally identical. Improvements belong in a later migration where
-- they are reviewable as a diff.
--
-- Every existing database has already executed all of this, so it is stamped
-- as applied rather than run there (see src/migrations.rs::run_migrations).
-- refinery runs this inside a transaction via batch_execute, which is what the
-- pg_advisory_xact_lock below and the DO $$ blocks require.

-- ===========================================================================
-- 1/3 — from src/db.rs
-- ===========================================================================
-- Create tables first so that the migration ALTER statements have something to target.
CREATE TABLE IF NOT EXISTS video_index (
    video_id    TEXT PRIMARY KEY,
    storj_key   TEXT,
    hetzner_key TEXT,
    phash       TEXT,
    phash_kind  TEXT,
    phash_version TEXT,
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
ALTER TABLE video_index ADD COLUMN IF NOT EXISTS phash_kind TEXT;
ALTER TABLE video_index ADD COLUMN IF NOT EXISTS phash_version TEXT;
UPDATE video_index
SET phash_kind = COALESCE(phash_kind, 'phash'),
    phash_version = COALESCE(phash_version, 'legacy_hex_8x8_v0')
WHERE phash IS NOT NULL
  AND (phash_kind IS NULL OR phash_version IS NULL);
CREATE INDEX IF NOT EXISTS idx_phash_versioned_val
    ON video_index (phash_kind, phash_version, phash)
    WHERE phash IS NOT NULL;
-- Canonical video-key index for the chain-audit join. video_index.video_id is
-- the full path principal-slash-uid; the chain sends the bare uid, so the
-- canonical key strips the principal prefix (up to last slash), then dashes,
-- then lowercases. IMMUTABLE exprs, so indexable. Lives here (not in the
-- media_index schema) because video_index is created by this schema.
-- New name: drop the stale dash-only index, build fresh under a new name so
-- CREATE-IF-NOT-EXISTS actually applies the new expression.
DROP INDEX IF EXISTS idx_video_index_video_id_norm;
CREATE INDEX IF NOT EXISTS idx_video_index_video_key
    ON video_index (lower(replace(regexp_replace(video_id, '^.*/', ''), '-', '')));
-- Drop stale trigger from old single-table schema.
DROP TRIGGER IF EXISTS video_index_updated_at ON video_index;

CREATE TABLE IF NOT EXISTS mirror_jobs (
    video_id      TEXT PRIMARY KEY REFERENCES video_index(video_id),
    is_temp       BOOLEAN NOT NULL DEFAULT FALSE,
    retry_count   INTEGER NOT NULL DEFAULT 0,
    status        TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending','phash_computed','mirrored','failed','done')),
    error_message TEXT,
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Migration: ensure columns exist if mirror_jobs was created by an older schema.
ALTER TABLE mirror_jobs ADD COLUMN IF NOT EXISTS is_temp BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE mirror_jobs ADD COLUMN IF NOT EXISTS retry_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE mirror_jobs ADD COLUMN IF NOT EXISTS error_message TEXT;
ALTER TABLE mirror_jobs ADD COLUMN IF NOT EXISTS updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW();

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


-- ===========================================================================
-- 2/3 — from src/media_index/schema.rs
-- ===========================================================================
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

-- Functional indexes on the CANONICAL video key. Our video_id is the full
-- storage path "<principal>/<uid>" (both dashed and undashed uuid forms exist),
-- but the chain sends the bare "<uid>". Canonical = strip everything up to the
-- last '/' (drop the principal prefix), then strip dashes + lowercase. The
-- chain-audit join compares video_uid <-> video_id on this form. Without these
-- indexes the joins seq-scan the full master table and time out on prod.
-- `lower`/`replace`/`regexp_replace` are IMMUTABLE, so indexable.
-- NEW index names (…_video_key). The prior …_video_id_norm indexes used a
-- dash-only expression; CREATE IF NOT EXISTS matches by NAME, so reusing the
-- name would keep the stale expression. Drop the old ones (one-time; no-op
-- afterwards) and build fresh under new names.
DROP INDEX IF EXISTS idx_master_video_id_norm;
CREATE INDEX IF NOT EXISTS idx_master_video_key
    ON all_servable_videos_on_yral (lower(replace(regexp_replace(video_id, '^.*/', ''), '-', '')));
DROP INDEX IF EXISTS idx_hashes_video_id_norm;
CREATE INDEX IF NOT EXISTS idx_hashes_video_key
    ON servable_video_hashes (lower(replace(regexp_replace(video_id, '^.*/', ''), '-', '')));
CREATE INDEX IF NOT EXISTS idx_yral_posts_video_uid_norm
    ON yral_posts (lower(replace(video_uid, '-', '')));

-- ===========================================================================
-- 3/3 — from src/videogen/request_store.rs
-- ===========================================================================
-- Two instances can boot concurrently; CREATE ... IF NOT EXISTS is not atomic
-- against a concurrent creator (it raises a duplicate pg_type key). Serialize the
-- whole schema application, same as media_index::schema.
SELECT pg_advisory_xact_lock(904648332137142901);

CREATE TABLE IF NOT EXISTS videogen_requests (
    counter        BIGSERIAL PRIMARY KEY,
    principal      TEXT NOT NULL,
    request_id     TEXT NOT NULL,
    model_id       TEXT NOT NULL,
    prompt         TEXT NOT NULL,
    status         TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending','complete','failed')),
    video_id       TEXT,
    bucket_url     TEXT,
    failure_reason TEXT,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Serves the in-progress lookup and the per-principal staleness sweep, both of
-- which filter (principal, status) and order by created_at.
CREATE INDEX IF NOT EXISTS idx_videogen_requests_principal_status
    ON videogen_requests (principal, status, created_at DESC);

-- Correlating a Vast request id back to its row (debugging) must not seq scan.
CREATE INDEX IF NOT EXISTS idx_videogen_requests_request_id
    ON videogen_requests (request_id);

-- updated_at is maintained by trigger, not by each UPDATE, so a future write
-- cannot forget it.
CREATE OR REPLACE FUNCTION videogen_requests_touch_updated_at()
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
        WHERE tgname = 'videogen_requests_touch_updated_at'
          AND tgrelid = 'videogen_requests'::regclass
    ) THEN
        CREATE TRIGGER videogen_requests_touch_updated_at
            BEFORE UPDATE ON videogen_requests
            FOR EACH ROW EXECUTE FUNCTION videogen_requests_touch_updated_at();
    END IF;
END;
$$;
