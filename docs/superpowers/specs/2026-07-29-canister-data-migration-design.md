# Migrating Post & Profile Data Off Canisters Into Postgres — Design

**Date:** 2026-07-29
**Repo context:** `yral-video-storage-service` (branch: `main`)
**Related work:** [chain-coverage-audit](2026-07-02-chain-coverage-audit-design.md) (built `yral_posts`), [upload-storage-merge](2026-06-24-upload-storage-merge-design.md) (brought the upload routes in), [move-profile-image-endpoint](2026-07-13-move-profile-image-endpoint-design.md) (brought profile-image in)
**Primary goal:** Make Postgres the system of record for the post and profile data this service produces, confine every remaining IC canister call to one deletable module, and lay the read API that lets the other repos migrate.

---

## Summary

This service currently writes user-visible content into the `user_post_service` and `user_info_service` canisters and owns no durable copy of it. Every post it creates lives only on-chain; the only local trace is `yral_posts`, a read-only snapshot built by walking the chain for audit purposes.

This spec inverts that. A new `posts` / `post_likes` / `users` schema becomes the record of what this service creates. Writes commit to Postgres and to a **transactional outbox** in one transaction; a leased worker drains the outbox into the canister so existing canister readers keep working unchanged. A read API mirroring the canister's query surface ships alongside, so `yral-mobile`, `off-chain-agent`, and the web app can migrate at their own pace. When they have, deleting the canister dependency is deleting one module, one worker, and one Cargo dependency.

The canister is not the goal of the migration — it is a *replication target* we keep alive until its last reader is gone.

**Non-goals.** The `user_info_service` social graph (registration, follows, followers, profile details, session type) stays on-chain; it is not this service's data and no part of this spec claims it. Feed ranking, hot-or-not, tournaments, and the individual-user-canister decommission are all out of scope. **Rebuilding chain-coverage-audit on a post-chain reference corpus is explicitly carved out** — see "Consequence for chain-coverage-audit".

---

## Current behavior (verified)

### Canister surface used by this repo

Six calls across five files, plus one compile guard.

| Call | Site | Kind |
|---|---|---|
| `user_post_service.add_post_v_1` | [update_video_metadata.rs:125](../../../src/routes/upload/update_video_metadata.rs#L125) | write |
| `user_post_service.get_individual_post_details_by_id` | [mark_post_as_published.rs:52](../../../src/routes/upload/mark_post_as_published.rs#L52) | read (ownership check) |
| `user_post_service.update_post_status` | [mark_post_as_published.rs:73](../../../src/routes/upload/mark_post_as_published.rs#L73) | write |
| `user_post_service.fetch_posts` | [chain_snapshot.rs:33](../../../src/jobs/chain_snapshot.rs#L33) | read (full-corpus walk) |
| `user_info_service.get_user_profile_details_v_6` | [get_upload_url.rs:67](../../../src/routes/upload/get_upload_url.rs#L67) | read (principal exists) |
| `user_info_service.update_profile_details` | [profile_image.rs:98](../../../src/routes/user/profile_image.rs#L98) | write (**as the user**) |

Reached indirectly: [draft_client.rs](../../../src/routes/upload/draft_client.rs) (videogen drafts → `update_metadata_impl` → `add_post_v_1`), [generate.rs:1099](../../../src/routes/videogen/generate.rs#L1099) (`reserve_upload_destination` → `get_upload_url_core` → `get_user_profile_details_v_6`), [chain.rs:36](../../../src/routes/chain.rs#L36) (`/chain/snapshot` handler).

`AppState.ic_agent` is built once in [main.rs:263](../../../src/main.rs#L263) from optional `BACKEND_ADMIN_IDENTITY`; `deploy/docker-compose.ha.yml:135` supplies it. `IC_URL` defaults to `https://ic0.app`.

### What is *not* canister code

`ic-agent`, `candid`, and `k256` are also used for **delegation-chain verification** — [identity_auth.rs](../../../src/routes/identity_auth.rs) builds a `DelegatedIdentity` via `DelegatedIdentity::new` (which validates signatures, unlike `new_unchecked`) and derives the sender principal. That is local cryptography with no network call, and it is the authentication scheme for every public route in this service. **It stays.** `Principal` also remains the user identifier in every table and payload. Only `yral-canisters-client` — the generated canister client — is on the deletion path.

### Candid shapes being replaced

```
Post = { id: text; video_uid: text; creator_principal: principal;
         status: PostStatus; description: text; hashtags: vec text;
         share_count: nat64; likes: vec principal;
         view_stats: { total_view_count: nat64;
                       average_watch_percentage: nat8;
                       threshold_view_count: nat64 };
         created_at: SystemTime }

PostStatus = variant { Draft; Uploaded; Transcoding; CheckingExplicitness;
                       ReadyToView; Deleted; BannedForExplicitness;
                       BannedDueToUserReporting }

PostStatusFromFrontend = variant { Draft; Published }
  -> Draft maps to PostStatus::Draft, Published maps to PostStatus::Uploaded
     (yral-backend-canister/src/lib/shared_utils/.../args.rs:54)
```

**Which status variants are actually reachable.** Only four have a live writer:

| Variant | Written by |
|---|---|
| `Draft` | this service (`add_post_v_1` with `PostStatusFromFrontend::Draft`) |
| `Uploaded` | this service (`add_post_v_1` Published, and `mark_post_as_published`) |
| `Deleted` | off-chain-agent `delete_post` |
| `BannedDueToUserReporting` | off-chain-agent `offchain_service.rs:586` |

`Transcoding`, `CheckingExplicitness`, `ReadyToView`, and `BannedForExplicitness` have **no live writer** — searching `yral-backend-canister` finds them only in the legacy `top_posts` scoring structs and tests, never on a `Post.status` write path. They nonetheless **exist in stored data**, having arrived through `sync_post_from_individual_canister` during the individual-user-canister migration, which copies a whole `Post` including its status. The chain-audit's `EXPECTED_STATUSES` already treats three of them as coverage-expected ([chain_repo.rs:104](../../../src/media_index/chain_repo.rs#L104)).

Consequence: the schema must accept all eight, the backfill must carry all eight through, and the reconcile ownership rule (below) is well-defined because the four legacy variants are never written by anyone after backfill.

Query semantics we must reproduce, read from the canister source:

- `get_posts_of_this_user_profile_with_pagination_cursor(creator, limit, offset)` — newest first (`sort_by_creation_time` sorts descending), `limit` clamped to 100, excludes `Deleted` / `BannedDueToUserReporting` / `Draft`. **Known canister bug:** it applies `skip(offset).take(limit)` *before* the status filter, so a page can under-fill. Our SQL filters first; pages will be correctly sized. This is an intentional, documented behavioral improvement, not a parity break.
- `get_draft_posts_of_this_user_profile_with_pagination(offset, limit)` — guarded by `is_not_anonymous`, and the creator is `caller()`, **not an argument**. Drafts are private to their owner. Any HTTP replacement must derive the principal from a verified identity and must not accept it as a parameter.
- `get_individual_post_details_by_id_for_user(post_id, principal)` returns `liked_by_me`, taking the viewer as a plain query argument — i.e. unauthenticated. See "Privacy delta" below.

### Who else touches this data

Complete sweep of the local repo set (`grep` for `user_post_service` / `user_info_service` across `off-chain-agent`, `yral-mobile`, `hot-or-not-web-leptos-ssr`, `yral-metadata`, `yral-auth-v2`, `yral-backend-cloudflare-workers`, `videogen`, `marketing-analytics-server`, `yral-mixpanel-offchain`):

**yral-mobile** — writes already come to us over HTTP (`storage-interface.prakash.yral.com`): `/get-upload-url`, `/update-video-metadata`, `/mark-post-as-published`, videogen drafts, and `/api/v1/user/profile-image`. Reads still go **direct to the canister** through the `rust-agent-uniffi` FFI layer, with no HTTP fallback:
- `get_individual_post_details_by_id`
- `get_posts_of_this_user_profile_with_pagination` (via `..._cursor`)
- `get_draft_posts_of_this_user_profile_with_pagination`

`user_info_service` is likewise called direct from the client for follow/unfollow, `getUserProfileDetailsV7`, `getUsersProfileDetails`, followers/following, `updateProfileDetailsV2`, `acceptNewUserRegistrationV2`, delete.

**off-chain-agent** — a co-writer of post state:
- `posts/delete_post.rs:73` — `delete_post`, as the user
- `offchain_service.rs:517,586` — `get_individual_post_details_by_id` then `update_post_status(BannedDueToUserReporting)`
- `events/event.rs:272,299,325,375` — `update_post_add_view_details` (the view-stat writer)
- `canister/delete/mod.rs:282,326` — account deletion: enumerate a user's posts, delete each
- `bin/backfill_video_counts.rs:161` — `fetch_posts` walk
- `canister/health.rs:81` — `get_version` liveness probe

**hot-or-not-web-leptos-ssr** — one site, `ssr/src/utils/src/profile.rs:58-107` (profile post pagination).

**Nobody calls** `update_post_increment_share_count` or `update_post_toggle_like_status_by_caller` from any repo in the working set. Treat both as dormant surfaces: model the data, do not build endpoints for them yet.

**Conclusion:** post state is multi-writer. This service cannot unilaterally become the source of truth, and any plan that deletes the canister write before mobile adoption drains would make new posts invisible to installed apps. Dual-write is not a preference here, it is a constraint.

---

## Design

### Placement

```
src/posts/
  mod.rs        - re-exports, init_schema
  types.rs      - PostStatus enum (ours, no candid), PostRecord, NewPost, cursors
  repo.rs       - all SQL. insert, status transition, reads, like set/unset
  outbox.rs     - enqueue (caller's tx), claim, mark_sent, mark_dead
  api.rs        - axum handlers + utoipa annotations
  ic_sync.rs    - THE ONLY FILE IMPORTING yral_canisters_client
src/jobs/post_outbox_worker.rs  - leased drain loop
migrations/                     - versioned SQL (see "Schema management")
```

The isolation rule is the whole point of the layout: `repo.rs` and `api.rs` must never mention a canister type. `ic_sync.rs` translates `PostRecord` → `PostDetailsFromFrontendV1` at the boundary and owns every `yral_canisters_client` import in the crate. Phase 4 is `rm src/posts/ic_sync.rs src/jobs/post_outbox_worker.rs` plus a Cargo line.

### Schema management — adopt versioned migrations

The repo currently applies DDL as three `&str` constants replayed on every boot ([db.rs:3](../../../src/db.rs#L3), [media_index/schema.rs:3](../../../src/media_index/schema.rs#L3), `videogen/request_store.rs`), written defensively as `CREATE TABLE IF NOT EXISTS` + `ALTER TABLE ... ADD COLUMN IF NOT EXISTS`, serialized by `pg_advisory_xact_lock`. That was right for tables nobody outside the service reads. It stops being right the moment this database holds user-visible content of record:

- It cannot express an ordered or destructive change. Renaming a column, backfilling it, and dropping the old one is three steps that must happen once, in order — `IF NOT EXISTS` cannot encode "once".
- Drift is invisible. There is no record of which statements a given database has actually seen, so a partially-applied change is undetectable.
- Reviewers diff a Rust string literal instead of a migration.
- The `idx_master_video_key` comment block in `media_index/schema.rs` is already an artifact of the workaround: because `CREATE INDEX IF NOT EXISTS` matches on *name*, a changed index expression had to be shipped under a new name with a manual `DROP INDEX` beside it.

**Recommendation:** introduce [`refinery`](https://crates.io/crates/refinery) with the `tokio-postgres` driver. It embeds `migrations/V{n}__{name}.sql` at compile time, records applied versions in `refinery_schema_history`, and takes its own lock — no new runtime dependency, no ORM, no change to the query layer.

Adoption path, deliberately non-disruptive:
1. `V1__baseline.sql` = the current three schema constants verbatim. Every existing database has already run all of it, so mark V1 applied without executing it (`refinery`'s `set_migrations`/baseline path, or a one-shot INSERT into the history table gated on the tables already existing).
2. `V2__posts.sql` onward = everything in this spec. New databases get V1 then V2; production gets V2 only.
3. Existing `init_schema` functions stay callable for the docker-based tests until the test harness moves over, then are deleted.

If adopting refinery is judged out of scope for this change, the fallback is to keep the constant-string pattern for `posts` — but then the migration debt is explicitly accepted and recorded, not silently inherited.

### Tables

```sql
CREATE TABLE posts (
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
    CONSTRAINT posts_status_valid CHECK (status IN (
        'Draft','Uploaded','Transcoding','CheckingExplicitness',
        'ReadyToView','Deleted','BannedForExplicitness','BannedDueToUserReporting')),
    CONSTRAINT posts_origin_valid CHECK (origin IN ('upload','videogen','chain_reconcile','offchain_agent')),
    CONSTRAINT posts_watch_pct_range CHECK (average_watch_percentage BETWEEN 0 AND 100),
    -- Single source of truth for deletion: `status` is the canister-mirrored fact,
    -- `deleted_at` is only its timestamp, and may be unknown (see below).
    CONSTRAINT posts_deleted_consistent CHECK (deleted_at IS NULL OR status = 'Deleted')
);

-- Serves the profile-pagination read EXACTLY: the status predicate is in the index
-- predicate, so no per-row recheck, and (created_at, post_id) DESC is the keyset order.
CREATE INDEX idx_posts_creator_visible ON posts (creator_principal, created_at DESC, post_id DESC)
    WHERE status NOT IN ('Draft','Deleted','BannedDueToUserReporting');

-- Serves the drafts read. Separate index because its predicate is the complement.
CREATE INDEX idx_posts_creator_drafts ON posts (creator_principal, created_at DESC, post_id DESC)
    WHERE status = 'Draft';

CREATE INDEX idx_posts_video_uid ON posts (video_uid);
-- Canonical video key, matching the existing convention in media_index/schema.rs:213
-- and db.rs:43 so posts joins the master/hash tables without a seq scan.
CREATE INDEX idx_posts_video_key
    ON posts (lower(replace(regexp_replace(video_uid, '^.*/', ''), '-', '')));
```

`status` is TEXT with a CHECK rather than a Postgres ENUM: adding a variant to an ENUM is a migration with locking implications, and the values are already a closed set defined elsewhere (the candid variant names, stored verbatim). `average_watch_percentage` is SMALLINT because the source is `nat8`.

**Deletion modelling.** `status = 'Deleted'` is the fact — it mirrors the canister variant and is what every read filters on and what the reconciler writes. `deleted_at` carries only *when*, and is deliberately allowed to be NULL on a deleted row: **the chain does not record a deletion timestamp**, so every post the backfill imports as `Deleted` has an unknowable one. A bidirectional constraint (`(deleted_at IS NULL) = (status <> 'Deleted')`) would have forced the backfill to fabricate a timestamp, which is worse than admitting the value is unknown. The one-directional constraint still forbids the incoherent case — a `deleted_at` on a post that is not deleted.

Read paths filter on `status`, never on `deleted_at` — which is why the partial indexes key on the status predicate and the earlier `WHERE deleted_at IS NULL` formulation is gone. Rows are never hard-deleted.

**Index note.** The two partial indexes are complementary and jointly cover every creator-scoped read. `BannedForExplicitness` is deliberately *included* in `idx_posts_creator_visible` — the canister's own profile filter checks only `Deleted` / `BannedDueToUserReporting` / `Draft`, so a `BannedForExplicitness` post is visible there today. Matching that filter exactly matters more than the variant's name suggesting it should be hidden. (It has no live writer either way; see the reachability table.)

**`updated_at` maintenance.** `posts.updated_at` and `users.updated_at` are maintained by `BEFORE UPDATE` triggers reusing the repo's existing [`update_updated_at()`](../../../src/db.rs#L75) function, created with the same `pg_trigger`-guarded `DO $$` block as [`media_job_failures_touch_updated_at`](../../../src/media_index/schema.rs#L159). A column that defaults on insert and is then never touched is a trap: it reads like a freshness signal and silently is not one.

```sql
CREATE TABLE post_likes (
    post_id    TEXT NOT NULL REFERENCES posts(post_id) ON DELETE CASCADE,
    principal  TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (post_id, principal)
);
CREATE INDEX idx_post_likes_principal ON post_likes (principal);
```

`posts.like_count` is a denormalized counter maintained by an `AFTER INSERT OR DELETE ON post_likes` trigger, following the `media_job_failures_touch_updated_at` trigger pattern already in the repo. Denormalizing is worth it: `like_count` is read on every post render, `post_likes` will be the largest table here, and the trigger makes the counter impossible to forget.

**Volume, and why the backfill needs a bulk path.** `Post.likes` is an unbounded `HashSet<Principal>` in the canister — no cap, no pagination. Against ~583k posts in the master corpus, `post_likes` plausibly lands in the tens of millions of rows, making it by an order of magnitude the largest table this service owns. Two consequences the backfill must handle:

- **The per-row trigger is the wrong tool for bulk load.** Firing `UPDATE posts SET like_count = like_count + 1` once per like row during a corpus-wide import is millions of redundant row updates and the resulting bloat. The backfill instead runs with the trigger disabled (`ALTER TABLE post_likes DISABLE TRIGGER ...`), bulk-inserts via `COPY`, then recomputes every counter in a single `UPDATE posts SET like_count = (SELECT count(*) ...)` pass and re-enables. Steady-state writes keep the trigger.

  **This is only safe because the backfill runs before dual-write is enabled** (rollout step 1 precedes step 3), so nothing else is writing `post_likes` while the trigger is off. `DISABLE TRIGGER` is table-wide, not session-scoped — a concurrent writer during that window would silently skip its counter update and leave a permanently wrong `like_count`. If the rollout steps are ever reordered, this breaks silently. Guard it: the backfill should refuse to disable the trigger when `POSTS_DUAL_WRITE` is on.

- **Build `post_likes` indexes after the load, not before.** `idx_post_likes_principal` and the PK maintained incrementally across a multi-million-row `COPY` is substantially slower than one bulk build afterwards. Same for the FK: it is checked per row on insert, so `post_likes` is created without it, loaded, then the constraint added with a single validating pass (or added `NOT VALID` and validated separately if the lock window matters).
- **`fetch_posts` page size becomes a memory hazard.** Each `Post` in a page carries its entire likes set, so a page of 100 posts is not a bounded payload — one viral post can dominate it. The walk should reduce `PAGE` (currently 100, [chain_snapshot.rs:58](../../../src/jobs/chain_snapshot.rs#L58)) for the backfill pass and treat a page-decode failure as a signal to halve it, rather than assuming the existing constant transfers.

A pre-flight measurement — walk a few hundred pages and record the like-set size distribution — should run before the full backfill, so the row estimate is a number rather than a guess. If it comes back far larger than expected, storing only `like_count` and deferring per-principal rows to Phase 3 is a legitimate reduction in scope.

```sql
CREATE TABLE post_outbox (
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
    CONSTRAINT post_outbox_status_valid CHECK (status IN ('pending','in_flight','sent','dead'))
);
-- "Unsent" = anything that has not reached the canister: pending, in_flight, OR dead.
-- The per-post ordering guard keys on exactly this set, so `sent` is the only status
-- that stops blocking a successor. See "Ordering" for why `dead` must be in here.
CREATE INDEX idx_post_outbox_ready  ON post_outbox (next_attempt_at, seq) WHERE status = 'pending';
CREATE INDEX idx_post_outbox_unsent ON post_outbox (post_id, seq)         WHERE status <> 'sent';
CREATE INDEX idx_post_outbox_reap   ON post_outbox (claimed_at)           WHERE status = 'in_flight';
```

```sql
CREATE TABLE users (
    principal           TEXT PRIMARY KEY,
    profile_picture_url TEXT,
    first_seen_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

`users` deliberately omits `bio` and `website_url`. `user_info_service` owns them and nothing here writes them; mirroring fields we never write manufactures drift with no consumer. The row is created on first profile-image upload and on first post. It is a projection of what this service knows, not a claim on user registration.

```sql
CREATE TABLE post_events (
    cursor     BIGSERIAL PRIMARY KEY,
    post_id    TEXT NOT NULL,
    event_kind TEXT NOT NULL,
    actor      TEXT,
    payload    JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_post_events_post ON post_events (post_id, cursor);
```

`event_kind` is one of `created`, `status_changed`, `synced_to_canister`, `reconciled`. `actor` is the principal that caused the change when one is attributable (the verified sender on a user-initiated write), and `NULL` for machine-initiated changes (reconciler, outbox). `payload` carries the before/after of whatever changed.

An append-only log of every state change, mirroring the existing [`media_feed_events`](../../../src/media_index/schema.rs#L64) pattern. Three reasons it earns its place rather than being YAGNI'd:

1. **It is what replaces the canister as an event source.** The chain was an audit log by construction. Postgres is not. Losing that on cutover is a real regression in a system where "why is this post banned" is a question people ask.
2. **Downstream consumers can tail a cursor** instead of polling — the same shape `media_feed_events` already serves, so the pattern and its consumers exist.
3. **It makes the reconciler debuggable.** When local and chain state disagree, the log says which side moved.

Rows are written in the same transaction as the mutation. Retention is a follow-up; start unbounded and revisit at volume.

Unlike `media_feed_events`, `post_events` does **not** get the [append-guard trigger](../../../src/media_index/schema.rs#L127) that forces inserts through a helper function. That guard exists because feed events must be appended with a cursor allocated under a specific locking discipline; `post_events` has no such invariant — it is a plain append. Noted because the divergence from the neighbouring table is deliberate, not an oversight.

**Why `post_events` and `post_outbox` carry no foreign key to `posts` while `post_likes` does.** The two log-shaped tables must outlive the rows they describe — an audit trail that vanishes with its subject is not an audit trail, and an outbox row must remain deliverable regardless of what happens to local state. `post_likes` is different: it is *part of* the post's current state, meaningless without it, so referential integrity is the right guarantee there. The asymmetry is intentional.

### Write path — transactional outbox

Every post mutation commits the row, the outbox entry, and the event log atomically:

```
BEGIN
  INSERT INTO users (principal) ON CONFLICT DO NOTHING   -- creator projection row
  INSERT INTO posts (...)                                -- or UPDATE for a status change
  INSERT INTO post_events (...)
  INSERT INTO post_outbox (post_id, op, payload)
COMMIT
  -> 2xx to the client
  -> tokio::spawn immediate best-effort drain of this post's pending ops
[worker] periodically drains anything the inline kick missed
```

The inline kick after commit is what keeps this honest. Mobile still reads the canister, so an outbox that only drains on a timer means a user's freshly uploaded post is invisible in their profile for up to one worker interval. With the kick, canister latency in the healthy path is what it is today; the worker is strictly a retry mechanism for the unhealthy path.

**The inline kick is not leased; the periodic worker is.** The kick runs on whichever node served the request, which may not be the lease holder. That is safe and intended: the claim is a single atomic `UPDATE ... SKIP LOCKED` (below), so any number of concurrent drainers is correct — the lease exists to stop N nodes *polling* the table on a timer, not to make draining exclusive. Stated explicitly because "exactly one node drains" and "every node kicks after its own commits" read as contradictory otherwise.

**What the transaction does and does not cover.** It covers the three table writes and nothing else. In particular `finalize_via_http` — the Storj finalize hop — runs **before** the transaction opens ([update_video_metadata.rs:98](../../../src/routes/upload/update_video_metadata.rs#L98)), so a Postgres rollback leaves a finalized object with no post row. This is not a regression: today the identical window exists between finalize and a failed `add_post_v_1`. The orphan is self-healing because the media pipeline discovers objects by scanning storage, independently of whether a post exists. Reordering finalize to sit inside the transaction is not possible (it is a network call, and putting it there recreates exactly the long-transaction problem the outbox claim design avoids). So the honest statement is: **post state is atomic; post state and storage state are eventually consistent, as they already are.**

**`created_at` skew during dual-write.** The canister stamps IC time inside `add_post`; we stamp `now()` at insert. The two differ by the outbox delivery latency — normally milliseconds, but unbounded when the outbox is backed up. Since profile pagination orders by `created_at`, a client reading our API and a client reading the canister can briefly disagree about the order of two posts created close together. Harmless in itself, but it has two consequences that are easy to miss:

- **The backfill's `created_at` must come from the chain, never re-stamped.** Otherwise every historical post is re-dated to the migration timestamp and profile ordering collapses to backfill-walk order.
- **The drift metric must not compare `created_at`** — see "What drift compares" under Reconciliation. Posts we author differ from the chain on this column permanently and by design.

**Why post writes can be outboxed at all.** `add_post_v_1` and `update_post_status` are called with `state.ic_agent` — the **shared admin identity** from `BACKEND_ADMIN_IDENTITY`, not the caller's ([update_video_metadata.rs:122](../../../src/routes/upload/update_video_metadata.rs#L122), [mark_post_as_published.rs:50](../../../src/routes/upload/mark_post_as_published.rs#L50)). A worker can therefore replay them at any later time using an identity the service already holds, with no credential stored in the outbox row. This is the entire asymmetry with `/profile-image`, which signs as the *user* and so cannot be deferred. It is also why Phase 3 will not be able to outbox off-chain-agent's `delete_post` — that call uses a per-request `user_ic_agent` (`off-chain-agent/src/posts/delete_post.rs:70`).

Corollary: `BACKEND_ADMIN_IDENTITY` is currently **optional** ([main.rs:265](../../../src/main.rs#L265)) — absent it, the agent is anonymous. Today that surfaces as an immediate `Unauthorized` on every publish; under dual-write it becomes a silent queue of dead-lettered rows. **Startup must refuse `POSTS_DUAL_WRITE=true` when `BACKEND_ADMIN_IDENTITY` is unset.**

**What this changes for callers.** Two canister errors become synchronous and better:
- `DuplicatePostId` → the `posts` PK rejects it inside the transaction → **409** instead of a canister round-trip.
- `Unauthorized` on the *caller* → already checked locally by `verified_sender` before any of this.

**Canister error classification — not every failure is retryable.** `add_post_v_1` returns `Result_ = variant { Ok; Err : UserPostServiceError }` where `UserPostServiceError = { DuplicatePostId; Unauthorized; CallError(RejectionCode, text); PostNotFound }`. Treating all of them as transient would burn twelve backoff attempts (≈ a day) on failures that will never succeed, and delay the alert that matters:

| Outcome | Worker action |
|---|---|
| `Ok` | `sent` |
| `Err(DuplicatePostId)` | `sent` — already applied; this is the replay path |
| `Err(Unauthorized)` | **`dead` immediately** + Sentry. The admin identity is not authorized; retrying cannot fix it and every subsequent post will fail the same way. |
| `Err(PostNotFound)` | `dead` immediately — not meaningful for `add_post`; indicates a contract mismatch worth a human. |
| `Err(CallError(..))` | retry with backoff — this is the genuinely transient class |
| transport `AgentError` | retry with backoff |

An `Unauthorized` dead-letter should page, not merely log: it means *no* post is reaching the canister.

**`update_post_status` returns `()`, not a Result.** Per the candid (`update_post_status : (text, PostStatus) -> ()`), the canister reports nothing about whether the status change applied — a call against an unknown `post_id` is indistinguishable from success. Two consequences: the worker can only classify transport-level outcomes for this op, and there is no way to *verify* a status change landed other than reading the post back. This is a pre-existing property of the canister API (today's `mark_post_as_published` has exactly the same blindness), not something the outbox introduces — but it is why the reconciler, not the outbox, is the mechanism that ultimately proves status convergence.

**Outbox payloads.** `payload` carries the op's *intent at enqueue time*, not a pointer to current state — that is what makes replay deterministic and ordering meaningful. For `add_post` it is the serialized `PostDetailsFromFrontendV1` (id, video_uid, creator_principal, status, hashtags, description); for `update_post_status` it is `{ post_id, status }`. `ic_sync` deserializes and calls; it never re-reads `posts`, because a row mutated after enqueue must not retroactively change what an earlier queued op sends.

**Claiming — two phases, never a transaction across the network.** The obvious `SELECT ... FOR UPDATE SKIP LOCKED` → canister call → `UPDATE ... SET status='sent'` in one transaction is wrong here: it holds a Postgres transaction open for the duration of an `ic0.app` round-trip. That pins a pgbouncer server connection for seconds under load (the HA stack runs pgbouncer in front of Patroni), blocks vacuum on a high-churn table, and turns any IC slowdown into database pressure. Instead:

```sql
-- Phase 1: claim. Own transaction, commits immediately.
UPDATE post_outbox SET status = 'in_flight', claimed_by = $1, claimed_at = now()
WHERE status = 'pending'          -- redundant given the row lock below, kept as a guard
  AND seq IN (
    SELECT o.seq FROM post_outbox o
    WHERE o.status = 'pending' AND o.next_attempt_at <= now()
      AND NOT EXISTS (
          SELECT 1 FROM post_outbox p
          WHERE p.post_id = o.post_id
            AND p.status <> 'sent'          -- pending, in_flight, OR dead
            AND p.seq < o.seq)
    ORDER BY o.seq
    FOR UPDATE OF o SKIP LOCKED
    LIMIT $2
)
RETURNING seq, post_id, op, payload, attempts;
```

Then, outside any transaction, call the canister per claimed row. Phase 2 is a short transaction marking the row `sent` (`sent_at = now()`) or bouncing it back to `pending` with an incremented `attempts` and a new `next_attempt_at`.

`FOR UPDATE OF o` is qualified deliberately — an unqualified `FOR UPDATE` would try to lock every table in the query's scope.

**Ordering.** `add_post` must reach the canister before `update_post_status` for the same post. The `NOT EXISTS` guard admits only the lowest-`seq` **unsent** op per post — and *unsent* means `status <> 'sent'`, i.e. `pending`, `in_flight`, **and `dead`**. The set is defined by exclusion for a reason: two separate near-misses are easy here.

- If the guard checked only `pending`, a claimed-but-unsent `add_post` would stop blocking its own `update_post_status`, and the two could hit the canister out of order.
- If it checked `('pending','in_flight')` — the natural "still in progress" reading — then a **dead** `add_post` would stop blocking too, and the status change would ship for a post the canister never received. That is precisely the outcome the dead-letter rule exists to prevent, so `dead` must block, and `sent` is the only status that releases a successor.

**Stale claims.** A worker that dies mid-flight leaves rows `in_flight` forever, and — because of the ordering guard — permanently blocks every later op for those posts. A reaper returns rows with `claimed_at < now() - POST_OUTBOX_CLAIM_TTL` (default 5 min, comfortably longer than any IC call) to `pending` without incrementing `attempts`, since nothing is known about whether the call landed. Idempotency is what makes that safe.

**Idempotency.** Replaying `add_post_v_1` returns `DuplicatePostId`, which the worker treats as success — the post is on the canister, which is the only thing the op asserts. `update_post_status` is naturally idempotent. Both properties are load-bearing: the two-phase claim means a crash between the canister call and the phase-2 commit will re-send, and the reaper deliberately re-queues work that may already have been applied.

**Failure.** Exponential backoff on `next_attempt_at` (base 30s, cap 1h, jittered). Past `POST_OUTBOX_MAX_ATTEMPTS` (default 12, ≈ a day of retries) the row goes `status='dead'`, is reported to Sentry once, and is surfaced by the stats endpoint. Dead rows are never retried automatically — a human decides, because by then the failure is structural. A `dead` row blocks every later op for its post (see Ordering); that is intentional, and it is what makes the dead-letter alert urgent rather than informational.

**Recovering a dead row.** Because dead blocks and is never auto-retried, a dead row stalls its post *permanently* until someone acts — so there has to be a way to act. `POST /posts/outbox/{seq}/requeue` (operator-only, behind the `authorize` layer) resets `status='pending'`, `attempts=0`, `next_attempt_at=now()`. That is the whole recovery mechanism: fix the underlying cause, requeue, let ordering resume. Without it, "a human decides" is not a decision anyone can carry out.

**Retention.** `sent` rows are pure history and would otherwise grow without bound on a table whose whole purpose is churn. A daily sweep deletes `status='sent' AND sent_at < now() - POST_OUTBOX_RETENTION` (default 7 days) — long enough to debug an incident, short enough that the partial indexes stay small. `dead` rows are never auto-deleted.

**Leasing.** The worker reuses the `sweep_lease` / `NODE_NAME` election already built for `SweepWorker` ([main.rs:315](../../../src/main.rs#L315)) so exactly one node *polls* across the HA cluster (draining itself is safe from any node — see the inline-kick note above), and a restart re-adopts its own lease without waiting out a TTL. Ships behind `RUN_POST_OUTBOX_WORKER`, default off, matching the `RUN_SWEEP_WORKER` convention.

### Rewritten handlers

**`update_metadata_impl`** ([update_video_metadata.rs:83](../../../src/routes/upload/update_video_metadata.rs#L83)) — identity verification, `post_details` injection, and the storj finalize hop are unchanged. `upload_video_canister` is replaced by a `posts::repo::create_post` call in a transaction that also enqueues `add_post`. The analytics event and push notification keep firing on the same conditions, keyed off `PostStatusFromFrontend::Published` exactly as today. The `canister_id` field in the analytics payload ([events.rs:58](../../../src/routes/upload/events.rs#L58)) currently carries `USER_INFO_SERVICE_ID` — a constant, unrelated to the post. It stays as-is; renaming an analytics field is a downstream schema change and does not belong in this spec.

**`mark_post_as_published`** ([mark_post_as_published.rs:41](../../../src/routes/upload/mark_post_as_published.rs#L41)) — the ownership check reads local `posts` instead of `get_individual_post_details_by_id`, then transitions `Draft → Uploaded` and enqueues `update_post_status`. **This creates a hard ordering dependency: the backfill must complete before this handler can be flipped over,** or a user publishing a pre-migration draft gets a 404. Until the backfill is verified, the handler falls back to the canister read when the post is absent locally, behind `POSTS_READ_LOCAL` (see Rollout).

**Videogen draft path** ([draft_client.rs](../../../src/routes/upload/draft_client.rs)) — unchanged; it goes through `update_metadata_impl` and inherits the new behavior. `draft_post_details` keeps producing a `PostDetailsFromFrontendV1`; only its consumer changes.

**`/get-upload-url`** — **accepts an optional delegated identity; the canister call becomes the legacy fallback.** (Revised 2026-07-29 — this was originally deferred as a `user_info_service` concern. It is not: the fix needs no `user_info_service` data at all.)

The endpoint is unauthenticated today: `GetUploadUrlReq` is `{ publisher_user_id }` with no identity ([get_upload_url.rs:24](../../../src/routes/upload/get_upload_url.rs#L24)), and `get_user_profile_details_v_6` is the only thing stopping anyone minting upload URLs for arbitrary principals. It does that job poorly — it proves the principal *exists*, never that the caller *is* that principal.

A verified delegated identity proves both, so requiring one is simultaneously stricter and canister-free. It ships additively: `delegated_identity_wire` is optional; present → verify the chain, assert `sender == publisher_user_id`, skip the canister; absent → today's behavior unchanged. A present-but-invalid identity is a 401 and must **never** fall through to the legacy path, or the check is bypassable by sending garbage.

The videogen internal caller (`reserve_upload_destination` → `get_upload_url_core`) passes a verified principal directly: `/generate` already rejects anonymous senders and asserts `identity_principal == user_id` ([generate.rs:442](../../../src/routes/videogen/generate.rs#L442), [:504](../../../src/routes/videogen/generate.rs#L504)), so the canister round-trip it pays today buys nothing.

Residual exposure until the legacy arm is deleted is bounded and unchanged from today — an attacker minting a URL can park bytes under someone else's prefix, but publishing requires `/update-video-metadata`, which is chain-verified. The legacy arm is removed in Phase 2 once mobile sends the identity, at which point **`user_info_service` is gone from the post path entirely** and only `/profile-image` still touches it.

**`/profile-image`** ([profile_image.rs](../../../src/routes/user/profile_image.rs)) — **keeps its inline canister write, and must.** The write runs as the *user*, signed by their delegated identity. Outboxing it would mean persisting a user credential in a database row so a worker could replay it later — an unacceptable expansion of what a leaked database backup is worth. The handler additionally upserts `users.profile_picture_url` locally. If the canister write fails the request still fails, exactly as today.

### Read API

Shapes mirror the uniffi models (`UPSPostDetailsForFrontend`, `UPSResult3`) so mobile changes transport, not domain types.

```
GET  /api/v1/posts/{post_id}
       -> 200 PostDetails | 404
       optional ?viewer=<principal> for liked_by_me

POST /api/v1/posts/by-creator
       body { creator_principal, limit, cursor? }
       -> 200 { posts: [PostDetails], next_cursor: string|null }
       excludes Draft / Deleted / BannedDueToUserReporting, newest first

POST /api/v1/posts/drafts
       body { delegated_identity_wire, limit, cursor? }
       -> 200 { posts: [PostDetails], next_cursor: string|null } | 401
       creator derived from the verified sender; NEVER a parameter
```

`by-creator` is POST rather than GET only to stay consistent with `drafts`, which must carry an identity in the body per this repo's convention. If a plain `GET /api/v1/users/{principal}/posts` is preferred for cacheability, that is a fine variation — the drafts endpoint is the one with a hard constraint.

**`PostDetails` — the concrete shape.** Mirrors candid `PostDetailsForFrontend` field-for-field so the mobile mapping is mechanical:

```json
{
  "id": "string",
  "video_uid": "string",
  "creator_principal": "string",
  "created_by_user_principal_id": "string",
  "description": "string",
  "hashtags": ["string"],
  "status": "Uploaded",
  "created_at": "2026-07-29T10:14:00Z",
  "like_count": 0,
  "total_view_count": 0,
  "liked_by_me": false
}
```

Two encoding deltas Phase 2 must handle:

- **`created_at` is RFC3339**, not candid's `SystemTime { secs_since_epoch, nanos_since_epoch }`. The uniffi layer currently hands mobile the two-field struct.
- **`creator_principal` and `created_by_user_principal_id` are principal *text***, not the candid principal type. They carry the same value, as they do on the canister; the duplicate field is kept purely so the mobile model does not need editing.

`status` is included even though `PostDetailsForFrontend` omits it — clients currently infer visibility from which query returned the post, and having it explicit is what lets a single endpoint serve both the drafts and the public views. `share_count` and the two secondary view-stat fields are deliberately not serialized: nothing reads them today, and adding a field later is cheaper than removing one.

**Pagination.** Keyset on `(created_at DESC, post_id DESC)`, cursor = base64 of the last row's tuple. The canister used numeric offsets, which are O(n) and skip or duplicate rows when the underlying list changes between pages. Keyset is stable under concurrent inserts and index-driven — via `idx_posts_creator_visible` for `by-creator` and `idx_posts_creator_drafts` for `drafts`. `limit` clamped to 100, matching the canister.

**Deleted posts return 404 — a deliberate delta.** The canister's `get_individual_post_details_by_id` calls `get_post(post_id)` and returns the post *whatever its status*, so a `Deleted` or `BannedDueToUserReporting` post is still fetchable by id today. Our single-post read returns 404 for both. That is the right behavior — a moderation-removed post should not be retrievable by anyone who kept the id — but it **is** a contract change, and Phase 2 must confirm no mobile flow depends on resolving a deleted post (a stale feed entry or a deep link is the plausible case). If one does, the fix is a tombstone response (404 body carrying the status), not restoring the full details.

**Privacy delta.** The canister's `get_individual_post_details_by_id_for_user` takes the viewer principal as a plain query argument, so anyone can already ask "did X like Y". Copying that verbatim ports a leak into a service where it is cheap to fix. The recommendation: `?viewer=` is accepted for parity during migration, but `liked_by_me` is only populated when the request carries a verified identity; with a bare `?viewer=` it returns `null`. Mobile sends its identity on this call already.

**`post_likes` must never be exposed.** Only `like_count` and `liked_by_me` are serialized. The table carries `idx_post_likes_principal`, which exists to make "unlike" and `liked_by_me` cheap — but that same index makes "everything principal X has ever liked" a fast query. No endpoint, present or future, may return a like list or a per-principal like history without an explicit product decision. Worth a comment in the migration next to the index.

**Access control and abuse.** `by-creator` and the single-post read are unauthenticated, matching the canister's open query calls, and that parity is intentional — the data is public. But moving them from a replicated IC query layer to a single Postgres instance changes the cost profile: these become the first high-fanout public read endpoints this service hosts, in front of a database that is also running the phash pipeline. Two mitigations belong in the implementation: `limit` clamped server-side to 100 (already specified), and a per-IP rate limit on the read endpoints via the existing `tower` middleware stack. Neither is exotic; both are easy to forget until the first scraper arrives.

**Connection pooling.** There are ~20 `db::connect()` sites, each opening a fresh connection per job or per request ([chain.rs](../../../src/routes/chain.rs), [mirror.rs](../../../src/routes/mirror.rs), every job). Batch jobs can absorb that; a read API on the mobile hot path cannot. pgbouncer is in front of Postgres in the HA compose, which helps, but every request still pays a TCP connect and auth round-trip to pgbouncer. **Recommendation:** add `deadpool-postgres` (thin wrapper over the `tokio_postgres` already in use, pgbouncer-compatible in transaction mode), hold the pool in `AppState`, and use it for the new API and the outbox worker. Migrating the existing 20 sites is a separate mechanical change and not required by this spec.

### Backfill and reconciliation

`yral_posts` cannot seed `posts`: the snapshot only kept `post_id`, `video_uid`, `creator_principal`, `created_at`, `status` — no description, hashtags, likes, or view stats. A fresh walk is needed for full fidelity.

**Backfill** extends the existing `chain_snapshot` job ([chain_snapshot.rs:76](../../../src/jobs/chain_snapshot.rs#L76)) to write both destinations from the same `fetch_posts` page: `yral_posts` exactly as today (the audit contract is unchanged), and `posts` + `post_likes` with every field. It reuses the `media_job_runs` tracking, the `MAX_ITERS` backstop, the cancellation token, and the partial-run rule that only a **complete** walk may mark rows stale.

Backfill inserts must not enqueue outbox rows — these posts are already on the canister, and re-sending 583k `add_post_v_1` calls would be catastrophic. That is enforced structurally, not by discipline: `repo::create_post` (which enqueues) and `repo::import_post` (which does not) are **separate functions**, and the reconciler has no access to `outbox::enqueue`. A boolean parameter on one shared function would be a single typo away from the catastrophe.

**Reconciliation** is the same job in steady state, and this is where the ownership rules matter. Once `posts` is authoritative for what we author, a naive re-import would clobber local truth with a stale chain read. The rule:

| Column | Owner during Phase 1 | Reconciler behavior |
|---|---|---|
| `description`, `hashtags`, `created_at`, `video_uid`, `creator_principal` | this service | **never overwritten** |
| `status` | shared — see rule below | conditional |
| `share_count`, view stats | canister / off-chain-agent | always overwritten (cheap column write) |
| `post_likes` | canister | **diffed only when `like_count` disagrees** — see below |
| rows absent locally | canister | inserted with `origin='chain_reconcile'` |

**Why `post_likes` cannot simply be "overwritten".** The other canister-owned fields are scalar column writes. Likes are a *set*: reconciling them means diffing the chain's `HashSet<Principal>` against our rows, per post. Doing that unconditionally would re-diff tens of millions of rows on every reconcile pass over the full corpus — operationally impossible, and the naive reading of "always overwritten" in the table above.

The cheap discriminator is already in hand: the chain page carries `likes`, so `likes.len()` compared against our stored `like_count` is an O(1) check per post. So:

> Reconcile compares `likes.len()` to `posts.like_count`. Equal → skip the post's likes entirely. Unequal → diff that post's set and apply the delta (`INSERT ... ON CONFLICT DO NOTHING` for additions, `DELETE ... WHERE principal = ANY($removed)` for removals), then let the trigger correct `like_count`.

Cardinality equality does not strictly prove set equality — a simultaneous like and unlike between passes is invisible. That residual is acceptable: likes are not a correctness-critical quantity, the next real change re-syncs the post, and the alternative costs a full-corpus set diff per run. State the limitation rather than pretending the check is exact.

**Reconcile writes `post_events` only on actual change.** A `reconciled` event per *examined* post would add one row per post per pass — the log would outgrow the data it describes within days. Only a row that actually moved gets an event.

**The `status` rule, stated precisely.** We write `Draft` and `Uploaded`. off-chain-agent writes `Deleted` and `BannedDueToUserReporting`. The remaining four variants have no live writer (see "Which status variants are actually reachable"). So:

> The reconciler overwrites local `status` **only** when the chain value is `Deleted` or `BannedDueToUserReporting` and the local value is not. Every other chain value is ignored.

This is total over the eight variants — there is no undefined case — and it is deliberately asymmetric: a ban or delete must always win, because those are moderation actions, while a chain `Draft`/`Uploaded` that disagrees with ours is by definition stale (we are the only writer of those, so ours is newer). The four legacy variants can only appear on rows that predate the backfill, where local and chain already agree.

Because `status` and `deleted_at` are constrained to agree, a reconciler writing `status='Deleted'` must set `deleted_at` in the same statement.

**What "drift" compares.** Drift between `posts` and the chain is counted per run and joins the existing `/chain/audit` output — but it must compare **only the canister-owned columns**: `status`, `share_count`, the view stats, and `like_count`. Comparing service-owned columns would make the metric useless by construction, and `created_at` is the trap: the canister stamps IC time inside `add_post` while we stamp `now()` at insert, so **every post authored during dual-write differs on `created_at` permanently and by design**. A drift counter that included it would read 100% from the first post onward and never recover.

Rows the reconciler declines to change are not drift. Drift is specifically: a canister-owned column whose chain value differs from ours *after* the ownership rules have been applied. A rising count means an ownership boundary is wrong — which is the only thing the metric is for.

### Consequence for chain-coverage-audit

The [chain-coverage-audit](2026-07-02-chain-coverage-audit-design.md) asks one question: *of everything the chain says should be servable, what is missing from our master table and hash index?* Its reference corpus — the definition of "should exist" — is `yral_posts`, which is a projection of the chain. Phase 4 deletes the chain as a data source, and with it the audit's entire premise. This is not a cleanup item; it is a design question this spec deliberately does not answer.

What is true regardless:

- Through Phases 1–3, the audit is **unaffected**. `chain_snapshot` keeps writing `yral_posts` exactly as today; the new `posts` writes are additive. `EXPECTED_STATUSES` and the canonical-join-key logic ([chain_repo.rs:104](../../../src/media_index/chain_repo.rs#L104)) are untouched.
- At Phase 4, `posts` is the only remaining answer to "what should exist", and the audit becomes an internal consistency check — `posts` vs `all_servable_videos_on_yral` vs `servable_video_hashes` — rather than an external one. That is a genuinely weaker guarantee: today the chain is an independent witness, and afterwards nothing is. A corpus-wide bug in our own write path becomes invisible to an audit that uses our own write path as its reference.
- The honest mitigations are storage-side (enumerate the bucket, which is genuinely independent) or `post_events` replay. Choosing between them, and deciding whether the weaker guarantee is acceptable, needs its own spec.

**Therefore:** Phase 4 must not delete `chain_snapshot` / `chain_audit` / `yral_posts` / `yral_users` until that follow-up exists. They are listed in the Phase 4 table with that dependency stated. A final full chain snapshot should be taken and retained before the canister is decommissioned, as a frozen witness — it is the last chance to capture one.

### Error handling

- Postgres failure on a write → the transaction rolls back, the client gets 5xx, and no post state is partially applied. An already-finalized Storj object may be left behind; see "What the transaction does and does not cover" — that window is pre-existing and self-healing.
- Outbox drain failure → classified per the table in "Canister error classification": transient errors back off and retry, terminal ones dead-letter on the first attempt. Never user-visible either way.
- Canister rejection that is *semantic and benign* (`DuplicatePostId`) → treated as applied.
- `Unauthorized` from the canister → dead-letter **and page**. It is not a per-post failure; it means the admin identity is rejected and nothing is reaching the canister.
- Reconciler failure → the run is marked `failed` in `media_job_runs` with the error, exactly as today.
- Every canister error type stays inside `ic_sync.rs`. `AppError::CanisterError` ([types.rs:45](../../../src/routes/upload/types.rs#L45)) remains only for the two handlers still making direct calls, and is deleted in Phase 4.

### Observability

- `GET /posts/outbox/stats` → `{ pending, in_flight, dead, oldest_pending_age_secs, sent_last_hour }`, and `POST /posts/outbox/{seq}/requeue` to revive a dead row. Both are operator endpoints and sit **behind the existing `authorize` layer**, unlike the public post reads — they expose queue depth and failure detail, and one of them mutates delivery state. (Router comments in [main.rs:356](../../../src/main.rs#L356) mark which routes are deliberately public; these are not.)
- Sentry on first dead-letter per post, and on a reconcile drift count above a threshold.
- Structured `tracing` on every outbox transition with `post_id` and `seq`.
- `oldest_pending_age_secs` is the single number that says whether the canister replica is healthy. It is the alert to wire first.

---

## Testing

Following the repo pattern — pure functions unit-tested inline, stateful behavior against a docker Postgres via `media_index::test_support::test_client`.

**Pure:**
- `PostStatusFromFrontend` → `PostStatus` mapping, including `Published → Uploaded`
- cursor encode/decode round-trip; malformed cursor rejected
- `PostRecord` → `PostDetailsFromFrontendV1` translation in `ic_sync`

**Repo / schema:**
- create post writes `posts` + `post_events` + `post_outbox` in one transaction; a forced failure leaves none of them
- duplicate `post_id` → the typed duplicate error, mapped to 409
- `like_count` trigger tracks insert and delete of `post_likes`
- status CHECK rejects an unknown variant
- `posts_deleted_consistent` rejects a non-null `deleted_at` on a non-`Deleted` status, and **accepts** `status='Deleted'` with a null `deleted_at` (the backfill case)
- `updated_at` advances on UPDATE and is not merely the insert default
- keyset pagination is stable when rows are inserted mid-iteration
- `by-creator` excludes Draft / Deleted / BannedDueToUserReporting, and **includes** `BannedForExplicitness` (canister-filter parity)
- `EXPLAIN` asserts both creator-scoped reads use their partial index and perform no status recheck

**Outbox:**
- ordering: with `add_post` seq 1 and `update_post_status` seq 2 for one post, a claim returns only seq 1; **while seq 1 is `in_flight`, seq 2 is still not claimable**; after seq 1 is marked sent, seq 2 becomes claimable
- two concurrent claimers never receive the same row, and the claim transaction commits without waiting on any canister call (assert the claim path issues no network I/O)
- the reaper returns an `in_flight` row past the TTL to `pending` without incrementing `attempts`, and the row is then claimable again
- a reaped row that had in fact been delivered replays cleanly — stubbed sync returns `DuplicatePostId`, row marks sent
- `DuplicatePostId` from a stubbed sync marks the row sent, not failed
- **error classification**: `Unauthorized` and `PostNotFound` go straight to `dead` on the first attempt; `CallError` and transport errors retry
- `payload` is what was enqueued, not current state — mutate the `posts` row after enqueue and assert the sent payload is unchanged
- attempts past the max transition to `dead` and stop being claimed
- **a `dead` row blocks every later op for its post** — enqueue seq 1 and seq 2, force seq 1 dead, assert seq 2 is never claimable (this is the guard that `status <> 'sent'` exists for; a `('pending','in_flight')` predicate passes every other outbox test and fails only this one)
- `requeue` on a dead row restores it to `pending` with `attempts` reset, and its successor becomes claimable once it sends
- backoff schedule increases and is capped
- retention sweep deletes `sent` rows past the window and never touches `dead` ones

**API:**
- drafts endpoint rejects a forged delegation chain with 401 (mirrors the existing `upload_forged_identity_is_401` test)
- drafts endpoint returns only the verified sender's drafts even if another principal appears in the body
- `liked_by_me` is null without a verified identity
- 404 for unknown, `Deleted`, and `BannedDueToUserReporting` posts — the deliberate delta from the canister, which returns them
- no response body on any endpoint contains a like list or a principal other than the post's creator

**Reconcile:**
- a chain post absent locally is inserted with `origin='chain_reconcile'`
- a locally-authored post's `description`/`hashtags`/`created_at` survive a reconcile pass carrying different values
- a chain `BannedDueToUserReporting` overwrites a local `Uploaded`, and sets `deleted_at` consistently when the value is `Deleted`
- a chain `Draft` does **not** overwrite a local `Uploaded` (the asymmetry rule)
- each of the four legacy variants arriving from the chain leaves local `status` untouched
- likes are **skipped** when `likes.len() == like_count`, and diffed when it differs — assert no `post_likes` write occurs in the equal case
- a like diff applies additions and removals and leaves `like_count` correct via the trigger
- a reconcile pass that changes nothing writes **zero** `post_events` rows
- **drift is zero for a post whose only difference from the chain is `created_at`** — the dual-write case, which must not be counted
- a partial (cancelled or limited) walk performs no destructive step, matching `limited_snapshot_stops_early_and_is_partial`

**Backfill:** run against a mock `PostPageSource` and assert `posts`, `post_likes`, and view stats all populate; `created_at` is the chain value and not `now()`; `like_count` after the bulk recompute equals `post_likes` cardinality; and **no outbox rows are produced**. Separately, assert the backfill refuses to disable the `like_count` trigger when `POSTS_DUAL_WRITE` is on.

**Rollout flags:** `POSTS_READ_LOCAL=true` with `POSTS_DUAL_WRITE=false` is rejected at startup; so is `POSTS_DUAL_WRITE=true` with `BACKEND_ADMIN_IDENTITY` unset.

**Wire contract:** a golden-file test pins the `PostDetails` JSON — RFC3339 `created_at`, principal-as-text, both principal fields present. It is the artifact Phase 2 codes against, so it should fail loudly on an accidental rename.

---

## Rollout

Everything ships dark and is enabled by flag, in this order.

**Flag semantics, stated exactly** — both code paths coexist for the duration of the rollout:

| `POSTS_DUAL_WRITE` | Write path in `update_metadata_impl` / `mark_post_as_published` |
|---|---|
| `false` (ship default) | Today's behavior verbatim: inline `add_post_v_1` / `update_post_status`, no Postgres write, no outbox row. |
| `true` | Transaction writes `posts` + `post_events` + `post_outbox`; **no inline canister call** — the outbox owns delivery. |

| `POSTS_READ_LOCAL` | `mark_post_as_published` ownership check |
|---|---|
| `false` (ship default) | `get_individual_post_details_by_id` against the canister. |
| `true` | Local `posts` lookup, falling back to the canister read on a local miss until the backfill is verified, then hard-failing 404. |

The flags are independent, but `POSTS_DUAL_WRITE=true` with `POSTS_READ_LOCAL=false` is the only meaningful intermediate — the reverse (reading local state we are not writing) is incoherent and should be rejected at startup with a clear error rather than silently misbehaving.

**`RUN_POST_OUTBOX_WORKER` must be enabled before `POSTS_DUAL_WRITE`, not with it.** Once dual-write is on, the outbox is the *only* path to the canister. The inline kick alone would deliver the happy path, but nothing would ever retry a failure — a single transient IC error would strand a post permanently. Enable the worker on all nodes (the lease elects one) and confirm it is draining an empty queue before flipping the write flag. A startup check cannot enforce this, because the worker flag is per-node and the write flag is global; it is a runbook step and should be written down as one.

1. **Schema + backfill.** Migrations applied; `chain_snapshot` extended to dual-write. Run the like-set sizing pre-flight, then the full walk on production. Verify: `posts` count matches non-stale `yral_posts` count; spot-check fields the snapshot never carried; `like_count` recomputation matches `post_likes` cardinality; drift = 0.
2. **Outbox worker on.** `RUN_POST_OUTBOX_WORKER=true` on every node. Queue is empty; confirm the lease is held by exactly one and the drain loop is idle-healthy.
3. **Outbox writes on.** `POSTS_DUAL_WRITE=true`. Canister readers are unaffected because the outbox lands the same call — but delivery is now asynchronous, so this is the step that needs watching: `oldest_pending_age_secs` and the dead-letter count, for several days, before proceeding.
4. **Local reads on.** `POSTS_READ_LOCAL=true`. Reversible instantly.
5. **Read API public.** Endpoints registered and documented in swagger; mobile can begin integrating.

Rollback at any step is the flag. Flipping `POSTS_DUAL_WRITE` back to `false` leaves pending outbox rows behind; the worker keeps draining them (it is gated on `RUN_POST_OUTBOX_WORKER`, not on the write flag) so they still reach the canister. Do not disable both at once with a non-empty outbox — drain first, or those posts exist only in Postgres while every reader is still on the canister.

## Phases beyond this spec

| Phase | Repo | Work |
|---|---|---|
| 1 | this repo | Everything above. Canister still written, via the outbox. Nothing deleted. |
| 2 | yral-mobile | Replace the three `rust-agent-uniffi` post reads with HTTP calls, including the error mapping below. **Also send `delegated_identity_wire` on `/get-upload-url`** (the app already holds one for `/update-video-metadata`) — that retires the last `user_info_service` call on the post path. Ship, wait for adoption. Old installs keep reading the canister and keep working — that is what the outbox buys. |
| 3 | off-chain-agent | Route `delete_post`, ban, `update_post_add_view_details`, and account-deletion enumeration to this service's API instead of the canister. Needs write endpoints this spec does not define. Note `delete_post` signs as the **user**, not an admin identity, so it cannot be outboxed the way our post writes can — its endpoint must carry the delegated identity and act synchronously. `backfill_video_counts` and `canister/health.rs` retire with it. |
| 3b | hot-or-not-web-leptos-ssr | One call site, `ssr/src/utils/src/profile.rs`. |
| 4 | this repo | Delete `ic_sync.rs`, `post_outbox_worker.rs`, `post_outbox`, `tests/canister_symbols_guard.rs`, `AppState.ic_agent`, `BACKEND_ADMIN_IDENTITY`, `AppError::CanisterError`, and the `yral-canisters-client` dependency. Keep `ic-agent`, `candid`, `k256` for identity verification. |
| 4b | this repo | Retire the chain-audit stack — `chain_snapshot`, `LivePostSource`, `chain_audit`, the `/chain/*` routes, `yral_posts`, `yral_users`, `EXPECTED_STATUSES`, and the `idx_*_video_key` indexes that exist only for its join. **Blocked on** the follow-up spec described in "Consequence for chain-coverage-audit". Retain one final frozen snapshot before the canister is decommissioned. |

Phase 4 cannot start until Phase 2's adoption curve flattens — that is a product decision about how long to support installed app versions, not an engineering one. Phase 3 needs write endpoints (delete, ban, view-stat ingestion) that this spec deliberately does not design, because their shape depends on how off-chain-agent wants to call them; that is a joint design with that repo's owner.

**Phase 2 error mapping.** Mobile's `UPSResult3` surfaces the canister's `GetPostsOfUserProfileError` variants, and the Kotlin layer branches on them. Our API does not have those failure modes — keyset pagination has no invalid bounds and no end-of-list error. The mapping mobile must implement:

| Canister | HTTP equivalent |
|---|---|
| `Ok(posts)` with a full page | `200` + `next_cursor` non-null |
| `Err(ReachedEndOfItemsList)` | `200` with `posts: []` and `next_cursor: null` — **not** an error |
| `Err(InvalidBoundsPassed)` | unreachable; a malformed cursor is `400` |
| `Err(ExceededMaxNumberOfItemsAllowedInOneRequest)` | unreachable; `limit` is clamped server-side to 100 rather than rejected |

The important one is the first: `ReachedEndOfItemsList` is currently an *error* branch in mobile, and it becomes an ordinary empty page. Any client treating an empty page as a failure will regress.

---

## Longer-term recommendations

Moving this data off-chain makes Postgres the durability story for user content. Three things that are currently sized for a derived index, not for a system of record:

**Point-in-time recovery.** The HA stack is solid — Patroni with streaming replication and a leader-following `db-backup` sidecar. But the backup is a daily `pg_dump -Fc`, so the worst-case RPO is 24 hours. That is acceptable for a phash index that can be recomputed; it is not acceptable for the only copy of a user's posts. Patroni already manages WAL; adding continuous archiving to object storage (`pgbackrest` or `wal-g`, same Hetzner bucket) brings RPO to minutes. **This should land before or with Phase 3,** which is the point at which the chain stops being an implicit backup.

**TLS to Postgres.** Every connection uses `NoTls` ([db.rs:130](../../../src/db.rs#L130)). Inside the compose network behind the firewall container that is defensible, but it is defensible by topology alone, with nothing in the code preventing a `DATABASE_URL` that points off-box. Make the trust boundary explicit: document it, and gate on `tokio-postgres-rustls` when the host is not local.

**Idempotency keys on write endpoints.** Mobile retries on flaky networks. A retried `/update-video-metadata` today produces a second `add_post_v_1`; after this change it produces a 409, which is better but still an error the client must interpret. An `Idempotency-Key` header recorded alongside the post would make the retry return the original 200. Small, and it removes a class of support tickets.

Two smaller notes: `post_events` should get a retention policy once volume is known — start unbounded, revisit. And when Phase 4 removes `yral-canisters-client`, check whether `candid` can be narrowed to its principal types rather than the full crate; the dependency is currently pulled in whole for `Principal` and a handful of error conversions.

**Capacity.** This lands on a database that already holds ~585k master rows and ~1.17M hash rows and runs the phash pipeline. `posts` adds roughly one row per master row; `post_likes` is the unknown and potentially adds tens of millions (see the sizing pre-flight). Disk headroom on the Patroni volumes, and the effect of the bulk load on replication lag, should be checked before the backfill rather than discovered during it. The backfill is resumable via `media_job_runs` and can be run in slices.

---

## Out of scope (YAGNI)

- `update_post_increment_share_count` and `update_post_toggle_like_status_by_caller` endpoints — no repo in the working set calls either. Model the data, skip the handlers.
- Write endpoints for off-chain-agent (delete, ban, view-stat ingestion). Phase 3 needs them; their shape is a joint design with that repo.
- The `user_info_service` social graph. Not our data.
- Rebuilding chain-coverage-audit without a chain. Carved out to its own spec; Phase 4b is blocked on it.
- Migrating the ~20 existing `db::connect()` call sites to the pool.
- Retention/archival for `post_events`.
- Deleting `/get-upload-url`'s legacy unauthenticated arm. Accepting an identity is now **in** scope (see § `/get-upload-url`); removing the fallback is Phase 2, gated on mobile adoption.
- The analytics `canister_id` field rename.

---

## Open questions

1. **Does `post_likes` earn its place in Phase 1?** Contingent on the sizing pre-flight. If the corpus carries tens of millions of likes, deferring per-principal rows and shipping only `like_count` is the better trade — `liked_by_me` then stays on the canister until Phase 3, which is where the like *writer* migrates anyway.
2. **Is refinery adopted now, or is the DDL-constant pattern extended once more?** The migration debt is real either way; this spec recommends paying it here, while the new schema is the only thing that needs converting.
3. **How long must Phase 2 adoption run before Phase 4?** A product call on installed-app support, not an engineering one — but it determines how long the outbox is load-bearing.
4. **Does the weaker post-Phase-4 audit guarantee need a storage-side independent witness before the canister is decommissioned?** See "Consequence for chain-coverage-audit".
