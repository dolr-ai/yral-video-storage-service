# Chain Coverage Audit — Design

**Date:** 2026-07-02
**Status:** Draft (pending spec review + user review)
**Branch:** `prakash/chain-coverage-audit`

## Problem

We need to confirm that every video the chain knows about has been migrated
cleanly into our storage pipeline and has a canonical perceptual hash (pHash).

Today our pHash coverage is measured against *our own* view of the world
(`video_index` from bucket scans → `all_servable_videos_on_yral` master →
`servable_video_hashes` canonical hashes). We have no independent check against
the **chain's** record of what videos exist. A video could be published on chain
yet never scanned, never imported, or never hashed, and our internal metrics
would not reveal it.

This design adds a read-only reconciliation that pulls the chain's full post
record, stages it locally, and compares it against our master + pHash tables to
surface any coverage gaps — with optional, explicitly-flagged remediation.

## Canister facts (verified)

- `ivkka-7qaaa-aaaas-qbg3q-cai` = **user_info_service**. It has **no
  enumeration method** — every call takes a `principal` as input
  (`get_user_profile_details_*`, `get_users_profile_details`, etc.). We cannot
  list all users from it.
- `gxhc3-pqaaa-aaaas-qbh3q-cai` = **user_post_service**. It exposes
  `fetch_posts(FetchPostsArgs { limit: nat64, last_uuid_processed: opt text }) ->
  FetchPostsResult { posts: vec Post, last_post_id_fetched: opt text }` — a
  **global paginated cursor over ALL posts**.
- `Post` carries: `id: text`, `video_uid: text`, `creator_principal: principal`,
  `status: PostStatus`, `created_at: SystemTime`, plus like/share/view fields we
  ignore.
- `PostStatus` variants: `Uploaded`, `ReadyToView`, `Transcoding`,
  `CheckingExplicitness`, `Draft`, `Deleted`, `BannedForExplicitness`,
  `BannedDueToUserReporting`.

**Consequence:** `fetch_posts` alone yields the entire corpus — every
`video_uid` *and* its `creator_principal`. We do not need (and cannot use)
user_info_service enumeration. The principal list is derived as a byproduct of
the post walk.

## Join key: chain `video_uid` == internal `video_id`

Our internal tables (`video_index`, `all_servable_videos_on_yral`,
`servable_video_hashes`) are all keyed on `video_id TEXT`, which is the **bare
video uuid** — the object key is `<principal>/<uuid>.mp4` and `video_id` is the
`<uuid>` portion (confirmed: video_index rows have `storj_key='creator/g1.mp4'`
with `video_id='g1'`; schema.rs:6-7). The chain's `Post.video_uid` is that same
bare uuid. So the reconciliation join key is a **direct string equality**
`yral_posts.video_uid = <table>.video_id` — no transform.

**This equality is the correctness lynchpin; if it is wrong, every video falls
into category D and the audit is meaningless.** Therefore `chain-audit` begins
with a *validation gate*: sample K (e.g. 200) `video_uid`s from `yral_posts` and
report how many match a `video_id` in master or `video_index`. If the match rate
is implausibly low (e.g. < 50% for videos in expected statuses), the audit halts
with a loud warning rather than reporting a bogus wall of category-D "gaps." A
normalization step is added here only if this gate reveals a real format skew.

## Architecture

The heavy work lives in the **service**, which already holds an `ic_agent`
handle, the Postgres pool, and the media-ownership repos. `mirror-client` stays a
thin HMAC CLI that triggers and polls — identical to the existing `media-import`
/ `media-phash` / `media-audit` pattern.

```
mirror-client chain-snapshot   --HMAC-->  service async job: walk fetch_posts,
mirror-client chain-status                upsert yral_posts + yral_users
mirror-client chain-audit                 service: reconcile SQL -> report
mirror-client chain-audit --remediate     service: enqueue fixable gaps
```

### New tables (prod DB, beside master)

**`yral_posts`** — the chain's record, one row per post.

| column           | type        | notes                                    |
|------------------|-------------|------------------------------------------|
| `post_id`        | text PK     | `Post.id`                                |
| `video_uid`      | text        | `Post.video_uid`; indexed               |
| `creator_principal` | text     | `Post.creator_principal`; indexed        |
| `status`         | text        | `PostStatus` variant name; indexed       |
| `created_at`     | timestamptz | from `Post.created_at` (see conversion note) |
| `snapshot_run_id`| uuid        | run that last wrote this row; matches `media_job_runs.id` (UUID) |
| `fetched_at`     | timestamptz | wall-clock of the write                  |
| `stale`          | boolean     | default false; set true when a completed run no longer sees the post (hard-deleted on chain); excluded from audit |

`Post.created_at` is an IC `SystemTime` (nanoseconds since epoch); convert
`nanos → timestamptz` (divide to secs + nanos remainder) on the way in.

**`yral_users`** — distinct creators, derived from the walk.

| column              | type        | notes                              |
|---------------------|-------------|------------------------------------|
| `creator_principal` | text PK     |                                    |
| `post_count`        | bigint      | count of posts seen for creator    |
| `first_seen`        | timestamptz | earliest `created_at` among posts  |
| `last_seen`         | timestamptz | latest `created_at` among posts    |

Both are upserted → the snapshot is idempotent and re-runnable. `video_uid` is
**not** unique (a video can back multiple posts / re-posts, possibly by different
principals); reconciliation operates on the distinct set of `video_uid`s — see
the status-aggregation rule below. `yral_users.post_count` counts all posts
regardless of status; it is descriptive, **not** a coverage metric.

### Snapshot job — `chain-snapshot`

New `src/jobs/chain_snapshot.rs`. Async, tracked via the existing run infra with
a new `job_kind` (e.g. `chain_snapshot`); `snapshot_run_id` = the run's UUID.

**Single-flight:** guarded exactly like the media-phash job (a running-flag
`compare_exchange` / advisory lock; a second `chain-snapshot` while one is live
returns 409). Overlapping walks must not interleave upserts on the same tables.

Loop:

1. `last = None`; `iters = 0`.
2. `fetch_posts(limit = PAGE, last_uuid_processed = last)`.
3. Upsert each returned post into `yral_posts` (stamping `snapshot_run_id`).
4. Advance and **terminate** on ANY of:
   - `posts.is_empty()`, or
   - `last_post_id_fetched` is `None`/empty, or
   - the returned cursor equals the previous `last` (cursor failed to advance), or
   - `posts.len() < PAGE` (short/final page).
   Otherwise `last = last_post_id_fetched`, `iters += 1`, and loop while
   `iters < MAX_ITERS` (a hard backstop against a mis-behaving cursor).
   `fetch_posts` cursor semantics are **unverified** (no binding exists yet), so
   these belt-and-suspenders conditions are required — the existing import loop
   terminates on an empty page, not a null cursor (media_imports.rs:231-233).
5. After a *complete* walk only: **stale-row handling first** — rows in
   `yral_posts` whose `snapshot_run_id` != the current run were not seen this
   pass → the post was hard-deleted on chain. Mark them stale (a
   `stale = true` flag, cheaper and safer than deleting) so the audit excludes
   them and they don't linger as phantom gaps. This runs **only** when the walk
   completed cleanly — a partial/aborted run must never tombstone live rows.
6. Then recompute `yral_users` from `yral_posts` **excluding stale rows**
   (`INSERT … SELECT … WHERE NOT stale GROUP BY creator_principal … ON CONFLICT
   DO UPDATE`), so hard-deleted posts don't inflate `post_count`/first/last-seen.

Page size is a tunable constant, throttled to be gentle on the canister. Progress
(pages done, rows upserted, current cursor, completed?) is recorded on the run
row so `chain-status` can report it — including whether the last run finished or
died partway.

### Reconcile audit — `chain-audit` (read-only)

A SQL reconciliation joining the coverage-expected distinct `video_uid`s in
`yral_posts` against `all_servable_videos_on_yral` (master, incl.
`servable_status`), `servable_video_hashes` (canonical pHash), and `video_index`
(bucket scan). Each `video_uid` falls into exactly one category:

| cat | meaning                                              | fixable by     |
|-----|------------------------------------------------------|----------------|
| A   | in master, **servable**, **and** has canonical pHash | — (clean ✓)    |
| B   | in master, servable, **no** canonical pHash          | media-phash    |
| C   | not in master, but present in `video_index`          | media-import → phash |
| D   | not in master **and** not in `video_index`           | none (object missing from buckets) |
| E   | in master but **not servable** (dead/unservable object) | none (manual — object gone/broken) |

Category A is gated on `servable_status = 'servable'` (the exact servable value
is read from the schema at implementation time). A master row that exists but is
non-servable (dead/missing object) is **not** counted clean — it becomes
category E rather than silently passing as A/B. Categories are mutually
exclusive and exhaustive over the expected `video_uid` set.

**Status filter + aggregation.** A `video_uid` can back multiple posts with
different statuses. Rule: a `video_uid` is **coverage-expected** if *any* of its
posts is in `Uploaded`, `ReadyToView`, `Transcoding`, or `CheckingExplicitness`.
A `video_uid` is excluded (informational only) *only* when *all* of its posts are
`Draft` / `Deleted` / `BannedForExplicitness` / `BannedDueToUserReporting` — we
don't expect a pHash for those. Rows flagged `stale` (post hard-deleted on chain)
are dropped from the input set before any of this. Only coverage-expected,
non-stale `video_uid`s are categorized A–E.

Output: counts per category (A/B/C/D/E + excluded-by-status), a sample of
`video_uid`s per non-clean category, and the "worst creators" — principals ranked
by number of non-clean (B/C/D/E) videos. Charging rule: a non-clean `video_uid`
is charged to **every** `creator_principal` that authored a coverage-expected
post for it (a video with mixed creators counts against each), so no gap is
hidden by picking one principal. Also report snapshot freshness
(`snapshot_run_id` / newest `fetched_at` / whether that run completed).

### Remediation — `chain-audit --remediate` (off by default)

Read-only unless `--remediate` is passed. When passed:

- **B** → clear any backoff (`DELETE FROM media_job_failures` for the row, as the
  phash path already does; media_phash.rs:322) so the media-phash worker re-hashes.
- **C** → trigger a media-import **run**. Note: the existing entry point
  (`import_current_video_index`, media_imports.rs:351-357) is a *bulk* scan of all
  `video_index` rows missing from master — there is no per-`video_uid` enqueue.
  So C remediation kicks a full import run that necessarily picks up these rows;
  they then flow to media-phash. (A targeted per-video import path is out of scope
  unless a later need appears.)
- **D** and **E** → cannot be auto-fixed (object absent from buckets / dead in
  master); emit the full list for manual follow-up (targeted bucket scan or
  upstream re-check).

Remediation reuses the existing pipeline entry points; it introduces **no** new
write path to master or to the hash table.

## Data flow

```
gxhc3 fetch_posts (cursor)
        │  posts: video_uid, creator_principal, status, created_at
        ▼
   yral_posts  ──group by creator──▶  yral_users
        │ (distinct video_uid, coverage-expected, non-stale)
        ▼
   LEFT JOIN all_servable_videos_on_yral   (master + servable_status)
   LEFT JOIN servable_video_hashes         (canonical pHash)
   LEFT JOIN video_index                   (bucket scan, for cat C vs D)
        ▼
   category A/B/C/D/E  ──▶ report
                       └─(--remediate)─▶ media-import (C) / media-phash (B)
```

## Error handling

- **Canister errors / timeouts mid-walk:** the run records the last successful
  cursor; a re-run resumes cleanly because upserts are idempotent (re-processing
  a page is harmless).
- **Malformed `Post` (missing video_uid):** skip + count in the run's
  `skipped` tally; never abort the walk.
- **Partial snapshot:** `chain-audit` operates on whatever is in `yral_posts`;
  it reports the `snapshot_run_id` / `fetched_at` freshness so a stale or partial
  snapshot is visible in the output.
- **No Patroni / HA changes.** All work is ordinary reads + upserts on the
  primary via the existing pool.

## Testing

Unit tests (Postgres container, `--test-threads=1` per the known CI flake):

- cursor pagination terminates on every guard: empty page, null/empty cursor,
  **non-advancing cursor**, and short page — including the case where the cursor
  is non-null but the page is empty/short (the C2 termination risk).
- `yral_posts` upsert is idempotent (same post twice → one row, updated run id).
- `yral_users` aggregation matches distinct creators + counts.
- join-key validation gate: a seeded set of matching `video_uid`/`video_id`
  passes; a deliberately-skewed set trips the low-match-rate halt.
- category SQL assigns A/B/C/D/**E** correctly against seeded master / hashes /
  video_index fixtures — including a **non-servable master row → E** (not A/B).
- **mixed-status `video_uid`**: expected if ANY post is in an expected status;
  excluded only if ALL posts are Draft/Deleted/Banned.
- **stale handling**: a post present in a prior run but absent in a completed new
  run is marked stale and dropped from the audit; a *partial* run does NOT
  tombstone.
- worst-creator charging: a mixed-creator non-clean video counts against each
  creator.
- `--remediate` remediates B (clears failure row) and C (import run), leaves
  D/E untouched, and is a no-op without the flag.
- single-flight: a second `chain-snapshot` while one runs is rejected (409).

Manual: dry-run `chain-snapshot` + `chain-audit` against **preview** first;
inspect category counts; only then run against prod. Remediation run gated behind
explicit user go.

## Safety & rollout

- Read-only by default; `--remediate` is the only write path and is opt-in.
- Throttled canister walk; idempotent upserts; feature branch
  (`prakash/chain-coverage-audit`), not main.
- New tables only — no schema change to master/hashes/video_index.
- No Patroni changes.

## Out of scope (YAGNI)

- Enriching `yral_users` with profile details from user_info_service.
- Continuous/scheduled coverage monitoring (this is an on-demand audit).
- Reverse audit (videos in master that the chain does *not* know about).
- Automated recovery of category-D videos (object genuinely missing).
