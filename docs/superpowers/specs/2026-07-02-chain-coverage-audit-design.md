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
| `created_at`     | timestamptz | from `Post.created_at`                   |
| `snapshot_run_id`| bigint      | run that last wrote this row             |
| `fetched_at`     | timestamptz | wall-clock of the write                  |

**`yral_users`** — distinct creators, derived from the walk.

| column              | type        | notes                              |
|---------------------|-------------|------------------------------------|
| `creator_principal` | text PK     |                                    |
| `post_count`        | bigint      | count of posts seen for creator    |
| `first_seen`        | timestamptz | earliest `created_at` among posts  |
| `last_seen`         | timestamptz | latest `created_at` among posts    |

Both are upserted → the snapshot is idempotent and re-runnable. `video_uid` is
**not** unique (a video can back multiple posts / re-posts); reconciliation
operates on the distinct set of `video_uid`s.

### Snapshot job — `chain-snapshot`

New `src/jobs/chain_snapshot.rs`. Async, tracked via the existing run infra with
a new `job_kind` (e.g. `chain_snapshot`). Loop:

1. `last = None`
2. `fetch_posts(limit = PAGE, last_uuid_processed = last)`
3. Upsert each returned post into `yral_posts` (with `snapshot_run_id`).
4. `last = result.last_post_id_fetched`; repeat while `last` is `Some(non-empty)`.
5. After the walk, recompute `yral_users` from `yral_posts`
   (`INSERT … SELECT … GROUP BY creator_principal … ON CONFLICT DO UPDATE`).

Page size is a tunable constant, throttled to be gentle on the canister. Progress
(pages done, rows upserted, cursor) is recorded on the run row so `chain-status`
can report it.

### Reconcile audit — `chain-audit` (read-only)

A SQL reconciliation joining the distinct `video_uid`s in `yral_posts` against
`all_servable_videos_on_yral` (master) and `servable_video_hashes` (canonical
pHash). Each `video_uid` falls into exactly one category:

| cat | meaning                                         | fixable by     |
|-----|-------------------------------------------------|----------------|
| A   | in master **and** has canonical pHash           | — (clean ✓)    |
| B   | in master, **no** canonical pHash               | media-phash    |
| C   | not in master, but present in `video_index`     | media-import → phash |
| D   | not in master **and** not in `video_index`      | none (object missing from buckets) |

**Status filter.** Only posts whose `status` is one of `Uploaded`,
`ReadyToView`, `Transcoding`, `CheckingExplicitness` count as coverage gaps.
`Draft`, `Deleted`, `BannedForExplicitness`, `BannedDueToUserReporting` are
reported as a separate informational line (we do not expect a pHash for them).

Output: counts per category (A/B/C/D + excluded-by-status), a sample of
`video_uid`s per non-clean category, and the "worst creators" — principals with
the most category-B/C/D videos, via join to `yral_users`.

### Remediation — `chain-audit --remediate` (off by default)

Read-only unless `--remediate` is passed. When passed:

- **B** → clear any backoff / mark eligible so the media-phash worker hashes it.
- **C** → enqueue into media-import, then it flows to media-phash.
- **D** → cannot be auto-fixed (the object is not in either bucket); emit the
  full list for manual follow-up (targeted bucket scan or upstream re-check).

Remediation reuses the existing pipeline entry points; it does not introduce a
new write path to master or to the hash table.

## Data flow

```
gxhc3 fetch_posts (cursor)
        │  posts: video_uid, creator_principal, status, created_at
        ▼
   yral_posts  ──group by creator──▶  yral_users
        │ (distinct video_uid, status-filtered)
        ▼
   LEFT JOIN all_servable_videos_on_yral   (master)
   LEFT JOIN servable_video_hashes         (canonical pHash)
   LEFT JOIN video_index                   (bucket scan, for cat C vs D)
        ▼
   category A/B/C/D  ──▶ report
                     └─(--remediate)─▶ media-import / media-phash
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

- cursor pagination terminates when `last_post_id_fetched` is `None`/empty.
- `yral_posts` upsert is idempotent (same post twice → one row, updated run id).
- `yral_users` aggregation matches distinct creators + counts.
- category SQL assigns A/B/C/D correctly against seeded master / hashes /
  video_index fixtures.
- status filter excludes Draft/Deleted/Banned from gap categories.
- `--remediate` enqueues B and C, leaves D untouched, and is a no-op without the
  flag.

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
