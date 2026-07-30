-- V2__posts.sql — Postgres becomes the system of record for post data.
--
-- Design: docs/superpowers/specs/2026-07-29-canister-data-migration-design.md
-- § Tables. Decisions that look arbitrary are argued there; the load-bearing
-- ones are restated inline because they are the ones easy to "fix" wrongly.

-- ===========================================================================
-- posts — the record of what this service creates
-- ===========================================================================
CREATE TABLE IF NOT EXISTS posts (
    post_id                  TEXT PRIMARY KEY,
    video_uid                TEXT NOT NULL,
    creator_principal        TEXT NOT NULL,
    status                   TEXT NOT NULL,
    description              TEXT NOT NULL DEFAULT '',
    hashtags                 TEXT[] NOT NULL DEFAULT '{}',
    share_count              BIGINT NOT NULL DEFAULT 0,
    total_view_count         BIGINT NOT NULL DEFAULT 0,
    average_watch_percentage SMALLINT NOT NULL DEFAULT 0,
    threshold_view_count     BIGINT NOT NULL DEFAULT 0,
    like_count               BIGINT NOT NULL DEFAULT 0,
    origin                   TEXT NOT NULL,
    created_at               TIMESTAMPTZ NOT NULL,
    updated_at               TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    deleted_at               TIMESTAMPTZ,
    -- All eight candid PostStatus variants. Four have no live writer but exist
    -- in stored data via sync_post_from_individual_canister, so all eight must
    -- round-trip. TEXT + CHECK rather than an ENUM: adding an ENUM variant is a
    -- migration with locking implications, and the value set is defined
    -- elsewhere (the candid variant names, stored verbatim).
    CONSTRAINT posts_status_valid CHECK (status IN (
        'Draft','Uploaded','Transcoding','CheckingExplicitness',
        'ReadyToView','Deleted','BannedForExplicitness','BannedDueToUserReporting')),
    CONSTRAINT posts_origin_valid CHECK (origin IN ('upload','videogen','chain_reconcile','offchain_agent')),
    -- SMALLINT because the canister source is nat8.
    CONSTRAINT posts_watch_pct_range CHECK (average_watch_percentage BETWEEN 0 AND 100),
    -- ONE-directional on purpose. `status` is the fact; `deleted_at` is only its
    -- timestamp and may be unknown, because the chain records no deletion time —
    -- every backfilled Deleted row has an unknowable one. A bidirectional check
    -- would force the backfill to fabricate a value. This still forbids the
    -- incoherent case: a deleted_at on a post that is not deleted.
    CONSTRAINT posts_deleted_consistent CHECK (deleted_at IS NULL OR status = 'Deleted')
);

-- Serves profile pagination exactly: the status predicate lives in the index
-- predicate (no per-row recheck) and (created_at, post_id) DESC is the keyset
-- order. BannedForExplicitness is deliberately NOT excluded — the canister's
-- own profile filter checks only Deleted / BannedDueToUserReporting / Draft, so
-- such a post is visible there today. Matching that filter exactly matters more
-- than the variant's name suggesting it should be hidden.
CREATE INDEX IF NOT EXISTS idx_posts_creator_visible
    ON posts (creator_principal, created_at DESC, post_id DESC)
    WHERE status NOT IN ('Draft','Deleted','BannedDueToUserReporting');

-- Serves the drafts read; separate index because its predicate is the complement.
CREATE INDEX IF NOT EXISTS idx_posts_creator_drafts
    ON posts (creator_principal, created_at DESC, post_id DESC)
    WHERE status = 'Draft';

CREATE INDEX IF NOT EXISTS idx_posts_video_uid ON posts (video_uid);

-- Canonical video key, matching the convention already used by
-- idx_master_video_key and idx_video_index_video_key, so posts joins the
-- master/hash tables without a seq scan. Chain video_uid arrives both dashed
-- and undashed, and our video_id carries a "<principal>/" prefix; this strips
-- the prefix, then dashes, then lowercases. All IMMUTABLE, hence indexable.
CREATE INDEX IF NOT EXISTS idx_posts_video_key
    ON posts (lower(replace(regexp_replace(video_uid, '^.*/', ''), '-', '')));

-- ===========================================================================
-- post_likes — part of a post's current state, hence the FK
-- ===========================================================================
CREATE TABLE IF NOT EXISTS post_likes (
    post_id    TEXT NOT NULL REFERENCES posts(post_id) ON DELETE CASCADE,
    principal  TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (post_id, principal)
);

-- Makes "unlike" and liked_by_me cheap. It also makes "everything principal X
-- liked" cheap — NO endpoint may expose that. Only like_count and liked_by_me
-- are ever serialized.
CREATE INDEX IF NOT EXISTS idx_post_likes_principal ON post_likes (principal);

-- ===========================================================================
-- post_outbox — replication intent, must outlive local state (no FK)
-- ===========================================================================
CREATE TABLE IF NOT EXISTS post_outbox (
    seq             BIGSERIAL PRIMARY KEY,
    post_id         TEXT NOT NULL,
    op              TEXT NOT NULL,
    payload         JSONB NOT NULL,
    status          TEXT NOT NULL DEFAULT 'pending',
    attempts        INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    claimed_by      TEXT,
    claimed_at      TIMESTAMPTZ,
    last_error      TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    sent_at         TIMESTAMPTZ,
    CONSTRAINT post_outbox_op_valid CHECK (op IN ('add_post','update_post_status')),
    -- 'in_flight' is required by the two-phase claim: the claim transaction must
    -- commit before the canister call, so a claimed row needs a durable state
    -- that is neither pending nor terminal.
    CONSTRAINT post_outbox_status_valid CHECK (status IN ('pending','in_flight','sent','dead'))
);

CREATE INDEX IF NOT EXISTS idx_post_outbox_ready
    ON post_outbox (next_attempt_at, seq) WHERE status = 'pending';

-- "Unsent" = has not reached the canister: pending, in_flight, OR dead.
-- `sent` is the ONLY status that stops blocking a later op for the same post.
-- Do not "tidy" this to IN ('pending','in_flight'): that would let a dead
-- add_post release its update_post_status, shipping a status change for a post
-- the canister never received — the exact outcome dead-lettering exists to
-- prevent. The ordering guard in outbox::claim keys on this same predicate.
CREATE INDEX IF NOT EXISTS idx_post_outbox_unsent
    ON post_outbox (post_id, seq) WHERE status <> 'sent';

CREATE INDEX IF NOT EXISTS idx_post_outbox_reap
    ON post_outbox (claimed_at) WHERE status = 'in_flight';

-- ===========================================================================
-- users — a projection of what THIS service knows, not user registration
-- ===========================================================================
-- Deliberately omits bio and website_url: user_info_service owns them and
-- nothing here writes them, so mirroring would manufacture drift with no
-- consumer. Rows appear on first profile-image upload and on first post.
CREATE TABLE IF NOT EXISTS users (
    principal           TEXT PRIMARY KEY,
    profile_picture_url TEXT,
    first_seen_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- ===========================================================================
-- post_events — append-only audit log, must outlive its subject (no FK)
-- ===========================================================================
-- The chain was an audit log by construction; Postgres is not. This replaces
-- that. Unlike media_feed_events it gets no append-guard trigger: that guard
-- exists because feed cursors need a specific locking discipline, and this is
-- a plain append.
CREATE TABLE IF NOT EXISTS post_events (
    cursor     BIGSERIAL PRIMARY KEY,
    post_id    TEXT NOT NULL,
    event_kind TEXT NOT NULL,
    actor      TEXT,
    payload    JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_post_events_post ON post_events (post_id, cursor);

-- ===========================================================================
-- Triggers
-- ===========================================================================

-- like_count is denormalized because it is read on every post render. The
-- trigger makes it impossible to forget. Measured corpus is ~17k like rows, so
-- per-row firing costs nothing and there is no bulk-load path to work around.
CREATE OR REPLACE FUNCTION posts_sync_like_count()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        UPDATE posts SET like_count = like_count + 1 WHERE post_id = NEW.post_id;
    ELSIF TG_OP = 'DELETE' THEN
        UPDATE posts SET like_count = GREATEST(like_count - 1, 0) WHERE post_id = OLD.post_id;
    END IF;
    RETURN NULL; -- AFTER trigger; return value is ignored
END;
$$;

-- Guarded creation, matching the existing media_index/schema.rs pattern:
-- CREATE TRIGGER has no IF NOT EXISTS, and DROP+CREATE would race a concurrent
-- migration on another node.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_trigger
        WHERE tgname = 'post_likes_sync_like_count'
          AND tgrelid = 'post_likes'::regclass
    ) THEN
        CREATE TRIGGER post_likes_sync_like_count
            AFTER INSERT OR DELETE ON post_likes
            FOR EACH ROW EXECUTE FUNCTION posts_sync_like_count();
    END IF;

    -- updated_at on both tables. A column that defaults on insert and is then
    -- never touched reads like a freshness signal and silently is not one.
    -- update_updated_at() already exists (created by V1, from db.rs).
    IF NOT EXISTS (
        SELECT 1 FROM pg_trigger
        WHERE tgname = 'posts_updated_at' AND tgrelid = 'posts'::regclass
    ) THEN
        CREATE TRIGGER posts_updated_at
            BEFORE UPDATE ON posts
            FOR EACH ROW EXECUTE FUNCTION update_updated_at();
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_trigger
        WHERE tgname = 'users_updated_at' AND tgrelid = 'users'::regclass
    ) THEN
        CREATE TRIGGER users_updated_at
            BEFORE UPDATE ON users
            FOR EACH ROW EXECUTE FUNCTION update_updated_at();
    END IF;
END;
$$;
