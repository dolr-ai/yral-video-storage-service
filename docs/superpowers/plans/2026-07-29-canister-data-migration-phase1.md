# Canister Data Migration — Phase 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Postgres the system of record for the post data this service produces, with the canister kept in sync by a transactional outbox, and ship a read API the other repos can migrate to.

**Architecture:** New `src/posts/` module owning `posts` / `post_likes` / `post_events` / `post_outbox` / `users`. Writes commit locally and enqueue an outbox row in one transaction; a leased worker drains the outbox into `user_post_service`. Every `yral_canisters_client` import in the module is confined to `ic_sync.rs` so Phase 4 is a file deletion. Everything ships behind flags, default off.

**Tech Stack:** Rust, axum 0.8, tokio-postgres 0.7 (raw SQL, no ORM), refinery (new — versioned migrations), utoipa, docker-based Postgres tests.

**Spec:** [`docs/superpowers/specs/2026-07-29-canister-data-migration-design.md`](../specs/2026-07-29-canister-data-migration-design.md)

---

## How this plan relates to the spec

**The spec is normative.** For DDL, the outbox claim query, the canister error classification, the `PostDetails` JSON shape, and the reconcile ownership rules, this plan **cites the spec section by name and does not reproduce it**. Copying those into the plan would create two sources of truth that drift the first time either is edited — and the spec has already survived five review passes.

When a task says *"per spec § X"*, open that section and implement exactly what it says. If you believe the spec is wrong, stop and say so; do not silently deviate.

**Sequencing constraints the spec imposes** (violating these breaks things silently, not loudly):

1. `RUN_POST_OUTBOX_WORKER` is enabled **before** `POSTS_DUAL_WRITE` — spec § Rollout. Otherwise nothing retries a failed delivery and a transient IC error strands a post forever.
2. `mark_post_as_published` cannot read locally until backfill lands — spec § Rewritten handlers.

> A third constraint — "backfill must precede dual-write because `DISABLE TRIGGER` is table-wide" — was **removed 2026-07-29**. Task 0 measured the like corpus at ~17k rows, so there is no bulk-load path and no trigger-disable window. Backfill and dual-write ordering is now a preference (backfill first still makes `mark_post_as_published` simpler), not a silent-corruption hazard. Spec § Volume.

Build order below is *not* rollout order. Build A→F; roll out per spec § Rollout.

## Open question — RESOLVED 2026-07-29

**Task 0 is done. `post_likes` stays in Phase 1; Tasks 7 and 19 both survive.**

Measured over 30,000 posts: 890 likes total, mean 0.03/post, p95 = 0, 98.6% of posts have zero likes, worst single post 39, worst 100-post page 40 principals. Projection **~17,000 rows** — three orders of magnitude below the spec's original guess of "tens of millions".

Consequences already applied to this plan and the spec:

- Task 17 loses its bulk-load path (no `COPY`, no trigger disable, no build-indexes-after, no reduced `PAGE`).
- Sequencing constraint #1 is deleted — it existed solely to protect the trigger-disable window.
- Task 19's cardinality gate **stays**, but its justification changed: it saves a write *per post* across a 583k-post walk, not per *like*. Spec § "Why `post_likes` cannot simply be overwritten".

Carry forward as a product question, not an engineering one: nothing calls the like *writer* and 98.6% of posts have no likes. If liking is being retired, this table and `liked_by_me` could go away entirely.

---

## File structure

| File | Responsibility |
|---|---|
| `migrations/V1__baseline.sql` | Existing schema, verbatim. Marked applied on prod, executed on fresh DBs. |
| `migrations/V2__posts.sql` | Everything new. Per spec § Tables. |
| `src/posts/mod.rs` | Re-exports and the `#[cfg(test)]` schema tests. Owns no schema itself — migrations do (Task 1). |
| `src/posts/types.rs` | `PostStatus` (ours, no candid), `PostRecord`, `NewPost`, `Cursor`. No SQL, no HTTP. |
| `src/posts/repo.rs` | All post SQL. `create_post` (enqueues), `import_post` (does not), reads. |
| `src/posts/outbox.rs` | `enqueue`, `claim`, `mark_sent`, `mark_failed`, `reap`, `sweep_retention`, `requeue`. |
| `src/posts/ic_sync.rs` | **Only** file importing `yral_canisters_client`. Translates + calls + classifies. |
| `src/posts/api.rs` | axum handlers, utoipa annotations, cursor codec. |
| `src/jobs/post_outbox_worker.rs` | Leased drain loop. Mirrors `jobs/worker.rs`. |
| `src/posts/reconcile.rs` | Ownership rules, like diffing, drift counting. |

`repo.rs` and `api.rs` must never name a canister type. That invariant is enforced by Task 22.

**Test harness.** Every Postgres-backed test uses the existing helper, not a new one:

```rust
// `mut` is required — run_migrations takes &mut Client, and a transaction needs it too.
let (_pg, mut client) = crate::media_index::test_support::test_client().await;
crate::migrations::run_migrations(&mut client).await.unwrap();
```

`test_support` is `#[cfg(test)] pub(crate)` in `src/media_index/mod.rs:29` — crate-visible, so `src/posts/*` can use it directly. It spawns a real `postgres:16-alpine` container per call via `docker run` and tears it down on drop, so **Docker must be running** and tests are slow by design. Bind the returned `PgContainer` for the whole test (`let (_pg, ...)`, never `let (_, ...)`) or the container is dropped immediately and the client dies mid-test — an existing footgun, noted at `src/media_index/chain_repo.rs:453`.

---

## Task 0: Like-corpus sizing pre-flight — ✅ DONE 2026-07-29

Answered spec Open Question 1 and retracted the bulk-load path. See "Open question — RESOLVED" above for the numbers and consequences.

- [x] **Step 1: Write the sizing binary** — `src/bin/likes_sizing.rs`. Walks `fetch_posts` via the existing `LivePostSource` / `walk_step`, reports mean, p50/p95/p99, max-per-post, max-per-page, and projects rows against the 583k corpus.
- [x] **Step 2: Run against prod** — `PAGES=300 PAGE_SIZE=100 cargo run --bin likes_sizing`. No `BACKEND_ADMIN_IDENTITY` needed: `fetch_posts` is an unguarded candid `query`, so an anonymous agent suffices. Read-only.
- [x] **Step 3: Record and decide** — numbers written into spec § Volume; Open Question 1 marked answered; bulk-load machinery struck from spec and from Task 17.
- [x] **Step 4: Commit**

> **Delete `src/bin/likes_sizing.rs` before the branch merges.** It is a throwaway whose only output is a number now recorded in the spec. Leaving it turns a one-off measurement into a permanent binary that compiles on every build and will rot the first time `PostPageSource` changes shape.

---

## Milestone A — Migrations

## Task 1: Adopt refinery with the existing schema as baseline

Spec § "Schema management — adopt versioned migrations". Open Question 2 — if the answer is "not now", skip this task and put `V2` content into a new `SCHEMA_SQL` constant in `src/posts/schema.rs`, then continue.

**Files:**
- Modify: `Cargo.toml`
- Create: `migrations/V1__baseline.sql`, `src/migrations.rs`
- Modify: `src/lib.rs`, `src/main.rs:247-259`

- [ ] **Step 1: Add the dependency**

```toml
refinery = { version = "0.8", features = ["tokio-postgres"] }
```

- [ ] **Step 2: Write the failing test**

`src/migrations.rs`, `#[tokio::test]`: on a fresh container, `run_migrations` creates `refinery_schema_history` **and** every table the old `init_schema` pair created. Assert on `information_schema.tables` for `video_index`, `mirror_jobs`, `all_servable_videos_on_yral`, `servable_video_hashes`, `media_feed_events`, `media_job_runs`, `media_job_failures`, `sweep_lease`, `yral_posts`, `yral_users`, `videogen_requests`.

- [ ] **Step 3: Run it, confirm it fails**

Run: `cargo test --lib migrations:: -- --nocapture`
Expected: FAIL — `run_migrations` not defined.

- [ ] **Step 4: Write `V1__baseline.sql`**

Concatenate, in this order: `db.rs` `SCHEMA_SQL`, `media_index/schema.rs` `SCHEMA_SQL`, `videogen/request_store.rs` schema. Verbatim — do not tidy them. The point of a baseline is that it reproduces what production already has, warts included.

- [ ] **Step 5: Implement `run_migrations` with the baseline-adopt path**

```rust
pub async fn run_migrations(client: &mut tokio_postgres::Client) -> anyhow::Result<()> {
    // A database that already has the pre-refinery schema must NOT re-run V1.
    // Detect it by a table only the old schema created, and stamp V1 as applied.
    let adopted: bool = client
        .query_one(
            "SELECT to_regclass('public.all_servable_videos_on_yral') IS NOT NULL
                AND to_regclass('public.refinery_schema_history') IS NULL AS adopt",
            &[],
        )
        .await?
        .get("adopt");
    if adopted {
        // FakeVersion(1), NOT Fake: stamp ONLY the baseline as applied, never
        // whatever else happens to be pending. See the note below — this is the
        // difference between a safe adoption and silently skipping V2 on prod.
        embedded::migrations::runner()
            .set_target(refinery::Target::FakeVersion(1))
            .run_async(client)
            .await?;
    }
    // Runs everything still pending. On an adopted DB that is V2 onward; on a
    // fresh DB it is V1 onward. Both correct, in either build order.
    embedded::migrations::runner().run_async(client).await?;
    Ok(())
}
```

> **Why `FakeVersion(1)` and not `Fake`.** `Target::Fake` stamps *every* pending migration as applied without executing it. That would be correct only if V1 were the sole pending migration at the moment production first runs this code — which imposes a deploy-ordering constraint (ship Task 1 to prod, *then* write Task 2) that directly contradicts this plan's "build A→F" instruction. An executor working straight through would write V2 first, and prod's first migration run would then mark V2 applied while creating none of the posts tables. Silent, and only discovered when a query hits a missing relation.
>
> `FakeVersion(1)` bounds the stamp to the baseline, so adoption is safe no matter how many migrations exist by the time it runs, and build order and deploy order stop being coupled. **Verify the exact API against the refinery version you pin** (`Target::FakeVersion` exists in 0.8; confirm the variant name and that `set_target` is on the runner) — if it differs, find the bounded equivalent, do not fall back to unbounded `Fake`.

- [ ] **Step 5b: Prove the bound with a test**

```rust
#[tokio::test]
async fn adoption_stamps_only_the_baseline_and_still_runs_v2() {
    let (_pg, mut c) = test_client().await;
    db::init_schema(&c).await.unwrap();              // pre-refinery prod shape
    media_index::init_schema(&c).await.unwrap();
    run_migrations(&mut c).await.unwrap();
    // V1 faked, V2 actually executed
    assert!(table_exists(&c, "posts").await, "V2 must run, not be stamped");
    let applied: i64 = count(&c, "SELECT count(*) FROM refinery_schema_history").await;
    assert_eq!(applied, 2);
}
```

This is the single test that distinguishes a correct adoption from a catastrophic one; without it, `Fake` and `FakeVersion(1)` are indistinguishable on a fresh database.

- [ ] **Step 6: Run the test, confirm it passes**

Run: `cargo test --lib migrations:: -- --nocapture`
Expected: PASS.

- [ ] **Step 7: Add an adoption test**

Second test: run the old `db::init_schema` + `media_index::init_schema` on a fresh container, *then* `run_migrations`. Assert `refinery_schema_history` has V1 and no error occurred — this is the production path and it is the one that can corrupt a live database.

- [ ] **Step 8: Wire into main, keeping the old calls for now**

`main.rs`: call `run_migrations` before the existing `init_schema` calls. Both are idempotent, so they coexist during transition.

> **Loose end, tracked deliberately.** Leaving two schema systems live is the exact drift the spec argues against. They coexist only because the docker tests call `init_schema` directly in dozens of places, and converting those is mechanical churn that would bury this plan's real changes. **Retiring them is Task 23 Step 4** — do not consider this plan finished until that step is done, or "temporary" becomes permanent.

- [ ] **Step 9: Commit**

```bash
git add Cargo.toml Cargo.lock migrations/ src/migrations.rs src/lib.rs src/main.rs
git commit -m "feat: adopt refinery migrations with existing schema as baseline"
```

---

## Task 2: `V2__posts.sql`

**Files:**
- Create: `migrations/V2__posts.sql`, `src/posts/mod.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Write the failing test**

`src/posts/mod.rs` `#[tokio::test]`: after migrations, all five tables exist; and these constraint probes behave correctly (each is a spec decision worth pinning):

```rust
// posts_deleted_consistent: one-directional (spec § Deletion modelling)
assert!(insert_post(&c, "p1", "Uploaded", Some(now)).await.is_err());   // deleted_at on non-deleted
assert!(insert_post(&c, "p2", "Deleted", None).await.is_ok());          // deleted, timestamp unknown
// posts_status_valid
assert!(insert_post(&c, "p3", "NotAStatus", None).await.is_err());
// posts_watch_pct_range
assert!(set_watch_pct(&c, "p2", 101).await.is_err());
```

- [ ] **Step 2: Run it, confirm it fails**

Run: `cargo test --lib posts::tests::schema -- --nocapture`
Expected: FAIL — relation "posts" does not exist.

- [ ] **Step 3: Write the migration**

Transcribe spec § Tables exactly — `posts`, `post_likes`, `post_outbox`, `users`, `post_events`, all CHECK constraints, all indexes.

Then add, per spec:
- the `like_count` `AFTER INSERT OR DELETE ON post_likes` trigger
- `BEFORE UPDATE` triggers on `posts` and `users` reusing `update_updated_at()` (spec § "`updated_at` maintenance"), created inside a `pg_trigger`-guarded `DO $$` block copying the shape at `src/media_index/schema.rs:159`

Three details that are easy to get wrong and are already decided in the spec — re-read rather than infer:
- `idx_post_outbox_unsent` is `WHERE status <> 'sent'`, **not** `IN ('pending','in_flight')`. Spec § Ordering explains why; Task 12 has the test that catches it.
- `idx_posts_creator_visible` includes `BannedForExplicitness`. Spec § Index note.
- `post_events` and `post_outbox` have **no** FK to `posts`; `post_likes` does. Spec § "Why ... carry no foreign key".

- [ ] **Step 4: Run the test, confirm it passes**

Run: `cargo test --lib posts::tests::schema -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add migrations/V2__posts.sql src/posts/mod.rs src/lib.rs
git commit -m "feat: add posts schema migration"
```

---

## Task 2b: Connection pool in `AppState`

Spec § "Connection pooling" — in scope for the new API and the outbox worker; migrating the ~20 existing `db::connect()` sites is explicitly *not*.

**Files:**
- Modify: `Cargo.toml`, `src/db.rs`, `src/main.rs:293-309`

- [ ] **Step 1: Add the dependency**

```toml
deadpool-postgres = "0.14"
```

- [ ] **Step 2: Write the failing test**

```rust
#[tokio::test]
async fn pool_hands_out_working_clients_and_reuses_them() {
    let pool = build_pool(&url, 8).unwrap();
    let a = pool.get().await.unwrap();
    let one: i32 = a.query_one("SELECT 1", &[]).await.unwrap().get(0);
    assert_eq!(one, 1);
    drop(a);
    // second acquire must not open a new backend connection
    let before = backend_pid(&pool).await;
    let after  = backend_pid(&pool).await;
    assert_eq!(before, after, "pool must reuse the connection, not reconnect");
}
```

- [ ] **Step 3: Run, confirm fail**

Run: `cargo test --lib db::pool -- --nocapture`
Expected: FAIL — `build_pool` not defined.

- [ ] **Step 4: Implement**

`build_pool(url: &str, max_size: usize) -> anyhow::Result<Pool>` in `src/db.rs`, `NoTls` to match the rest of the repo (spec § TLS to Postgres is a separate infrastructure track). Add `pool: deadpool_postgres::Pool` to `AppState`; size from `POSTS_POOL_MAX_SIZE`, default 16.

pgbouncer sits in front in the HA deploy and runs transaction pooling, so **do not** use session-level features (prepared-statement caching across acquires, `SET` that must persist, `LISTEN`). Keep every acquire self-contained.

- [ ] **Step 5: Run, confirm pass**

Run: `cargo test --lib db::pool -- --nocapture`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/db.rs src/main.rs
git commit -m "feat: add connection pool for posts API and outbox worker"
```

> **Signature convention for every function in Tasks 5-22.** Do **not** take `&deadpool_postgres::Client` concretely — that makes the function uncallable from a transaction, and half the tests in this plan need transactions. Follow the pattern already in this repo at [`repo.rs:26-27`](../../../src/media_index/repo.rs#L26):
>
> ```rust
> async fn thing(client: &(impl GenericClient + Sync), ...) -> Result<..>
> ```
>
> `tokio_postgres::GenericClient` is implemented by `Client`, `Transaction`, and (via `Deref`) the pooled client, so one signature serves the API handler, the worker, the reconciler, and the tests. Where a function *must* be transactional — anything enqueuing an outbox row — take `&Transaction<'_>` explicitly instead, which is what makes atomicity unforgeable at the type level (Task 5).
>
> The existing jobs keep their per-run `db::connect()` connections — out of scope.

---

## Milestone B — Types and repository

## Task 3: `PostStatus` and the frontend mapping

**Files:**
- Create: `src/posts/types.rs`

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn published_maps_to_uploaded() {
    assert_eq!(PostStatus::from_frontend(FrontendStatus::Published), PostStatus::Uploaded);
    assert_eq!(PostStatus::from_frontend(FrontendStatus::Draft), PostStatus::Draft);
}

#[test]
fn status_roundtrips_through_db_text() {
    for s in PostStatus::ALL {
        assert_eq!(PostStatus::from_db(s.as_db()).unwrap(), s);
    }
}

#[test]
fn unknown_db_status_is_an_error_not_a_panic() {
    assert!(PostStatus::from_db("Whatever").is_err());
}
```

- [ ] **Step 2: Run, confirm fail**

Run: `cargo test --lib posts::types -- --nocapture`
Expected: FAIL — unresolved `PostStatus`.

- [ ] **Step 3: Implement**

Eight-variant enum, no candid derive. `as_db()` returns the exact candid variant name (spec stores them verbatim). `ALL` const for exhaustive tests. `from_frontend` per spec § Candid shapes — `Published → Uploaded`, confirmed against `yral-backend-canister/.../args.rs:54`.

Also here: `visible_in_profile()` returning `!matches!(self, Draft | Deleted | BannedDueToUserReporting)` — one definition, used by both the read filter and the index predicate assertion, so they cannot diverge.

- [ ] **Step 4: Run, confirm pass. Commit.**

```bash
git add src/posts/types.rs && git commit -m "feat: add PostStatus type and frontend mapping"
```

---

## Task 4: Cursor codec

**Files:**
- Modify: `src/posts/types.rs`

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn cursor_roundtrips() { /* (DateTime<Utc>, String) -> encode -> decode -> equal */ }
#[test] fn malformed_cursor_rejected() { assert!(Cursor::decode("!!!").is_err()); }
#[test] fn cursor_is_opaque_base64() { assert!(!Cursor::new(t, "p1").encode().contains("p1")); }
```

- [ ] **Step 2-4: Run (fail) → implement → run (pass)**

Base64 of `<rfc3339>|<post_id>`. Keyset tuple is `(created_at DESC, post_id DESC)` per spec § Pagination. Opaque so the shape can change without a client release.

- [ ] **Step 5: Commit**

```bash
git add src/posts/types.rs && git commit -m "feat: add keyset cursor codec"
```

---

## Task 5: `repo::create_post` — the write transaction

**Files:**
- Create: `src/posts/repo.rs`

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn create_post_writes_all_four_rows() {
    // users (projection), posts, post_events, post_outbox — spec § Write path
}
#[tokio::test] async fn create_post_is_atomic() {
    // force failure mid-transaction; assert zero rows in all four tables
}
#[tokio::test] async fn duplicate_post_id_is_a_typed_error() {
    // second create_post with same id -> RepoError::DuplicatePostId (mapped to 409 in Task 17)
}
#[tokio::test] async fn create_post_enqueues_exactly_one_add_post_op() { }
```

- [ ] **Step 2: Run, confirm fail**

Run: `cargo test --lib posts::repo -- --nocapture`

- [ ] **Step 3: Implement**

One `Transaction`, statements in spec § Write path order: `users` upsert `ON CONFLICT DO NOTHING`, `posts` insert, `post_events` (`event_kind='created'`, `actor` = verified sender), `post_outbox` via `outbox::enqueue(&tx, ..)`.

`enqueue` takes `&Transaction` — it is not callable outside one. That is deliberate: the outbox row and the state change must be atomic or the pattern is pointless.

Map the PK violation (SQLSTATE `23505`) to `RepoError::DuplicatePostId` rather than leaking `tokio_postgres::Error`.

- [ ] **Step 4: Run, confirm pass. Commit.**

```bash
git add src/posts/repo.rs && git commit -m "feat: add create_post write transaction with outbox enqueue"
```

---

## Task 6: `repo::import_post` — the backfill path that cannot enqueue

Spec § Backfill: structural separation, not a boolean.

**Files:**
- Modify: `src/posts/repo.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn import_post_never_enqueues() {
    import_post(&mut c, chain_post_fixture()).await.unwrap();
    let n: i64 = count(&c, "SELECT count(*) FROM post_outbox").await;
    assert_eq!(n, 0, "backfill must never enqueue — 583k replayed add_post calls");
}

#[tokio::test]
async fn import_post_preserves_chain_created_at() {
    // spec § created_at skew: must be the chain value, never now()
}
```

- [ ] **Step 2-4: Run (fail) → implement → run (pass)**

Separate function. Does **not** import `outbox`. Sets `origin='chain_reconcile'`. `created_at` from the argument, never `now()`.

- [ ] **Step 5: Commit**

```bash
git add src/posts/repo.rs && git commit -m "feat: add import_post backfill path with no outbox access"
```

---

## Task 7: `post_likes` write helpers

**Skip if Task 0 said to defer `post_likes`.**

**Files:**
- Modify: `src/posts/repo.rs`

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn like_count_trigger_tracks_inserts_and_deletes() { }
#[tokio::test] async fn apply_like_diff_adds_and_removes() { }
#[tokio::test] async fn like_is_idempotent() { /* double insert -> ON CONFLICT DO NOTHING, count 1 */ }
```

- [ ] **Step 2-4: Run (fail) → implement → run (pass)**

`apply_like_diff(tx, post_id, added: &[String], removed: &[String])`. Additions `ON CONFLICT DO NOTHING`; removals `DELETE ... WHERE principal = ANY($2)`. Never recompute `like_count` by hand — the trigger owns it.

- [ ] **Step 5: Commit**

```bash
git add src/posts/repo.rs && git commit -m "feat: add post_likes diff helpers"
```

---

## Task 8: Read queries

**Files:**
- Modify: `src/posts/repo.rs`

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn by_creator_excludes_draft_deleted_banned_reporting() { }
#[tokio::test] async fn by_creator_includes_banned_for_explicitness() {
    // spec § Index note — canister-filter parity, deliberately NOT hidden
}
#[tokio::test] async fn by_creator_is_newest_first() { }
#[tokio::test] async fn keyset_pagination_is_stable_under_concurrent_insert() {
    // page 1, insert a newer post, page 2 -> no duplicate, no skip
}
#[tokio::test] async fn drafts_returns_only_that_creators_drafts() { }
#[tokio::test] async fn get_by_id_returns_none_for_deleted_and_banned_reporting() {
    // spec § "Deleted posts return 404 — a deliberate delta"
}
#[tokio::test] async fn liked_by_me_is_none_without_a_viewer() { }
#[tokio::test] async fn explain_uses_partial_indexes() {
    // EXPLAIN both creator reads; assert the index names appear
}
```

- [ ] **Step 2-4: Run (fail) → implement → run (pass)**

Filter predicates must match the index predicates **textually** or the planner will not use the partial indexes — that is what the `EXPLAIN` test guards. Derive the profile filter from `PostStatus::visible_in_profile()` (Task 3) so there is one definition.

- [ ] **Step 5: Commit**

```bash
git add src/posts/repo.rs && git commit -m "feat: add post read queries with keyset pagination"
```

---

## Milestone C — Outbox and canister sync

## Task 9: `outbox::enqueue` and the claim query

**Files:**
- Create: `src/posts/outbox.rs`

- [ ] **Step 1: Write the failing tests — ordering is the whole point**

```rust
#[tokio::test] async fn claim_returns_lowest_seq_per_post() { }

#[tokio::test] async fn in_flight_op_still_blocks_its_successor() {
    // claim seq1 (-> in_flight), claim again -> seq2 NOT returned
}

#[tokio::test] async fn dead_op_blocks_its_successor() {
    // THE test. A ('pending','in_flight') predicate passes every other test here
    // and fails only this one. Spec § Ordering.
    enqueue(add_post, "p1").await; enqueue(update_status, "p1").await;
    force_dead(seq1).await;
    assert!(claim(1).await.unwrap().is_empty(),
        "a dead add_post must block its update_post_status");
}

#[tokio::test] async fn sent_op_releases_its_successor() { }
#[tokio::test] async fn concurrent_claimers_never_get_the_same_row() { }
#[tokio::test] async fn claim_commits_without_any_network_call() { }

#[tokio::test]
async fn payload_is_intent_at_enqueue_not_current_state() {
    // spec § Outbox payloads. Enqueue, then mutate the posts row, then claim:
    // the claimed payload must be the original. ic_sync never re-reads posts,
    // or a later edit would retroactively change what an earlier op sends.
}
```

- [ ] **Step 2: Run, confirm fail**

Run: `cargo test --lib posts::outbox -- --nocapture`

- [ ] **Step 3: Implement**

Claim query verbatim from spec § Claiming — including `status <> 'sent'` in the `NOT EXISTS`, `FOR UPDATE OF o SKIP LOCKED`, and the outer `WHERE status = 'pending'` guard. Claim is its own short transaction that commits before any canister call.

- [ ] **Step 4: Run, confirm pass. Commit.**

```bash
git add src/posts/outbox.rs && git commit -m "feat: add outbox enqueue and ordered claim"
```

---

## Task 10: Terminal transitions, reaper, retention, requeue

**Files:**
- Modify: `src/posts/outbox.rs`

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn mark_sent_sets_sent_at() { }
#[tokio::test] async fn mark_failed_backs_off_exponentially_and_caps() { }
#[tokio::test] async fn attempts_past_max_go_dead() { }
#[tokio::test] async fn reaper_returns_stale_in_flight_without_incrementing_attempts() {
    // spec § Stale claims — nothing is known about whether the call landed
}
#[tokio::test] async fn retention_deletes_sent_and_never_dead() { }
#[tokio::test] async fn requeue_revives_a_dead_row_and_unblocks_its_successor() {
    // spec § Recovering a dead row
}
#[tokio::test] async fn going_dead_reports_to_sentry_exactly_once() {
    // spec § Observability — "Sentry on first dead-letter per post"; a retry loop
    // that reports every attempt buries the signal it exists to raise
}
```

- [ ] **Step 2-4: Run (fail) → implement → run (pass)**

Backoff base 30s, cap 1h, jittered. `POST_OUTBOX_MAX_ATTEMPTS` default 12. `POST_OUTBOX_CLAIM_TTL` default 5min. `POST_OUTBOX_RETENTION` default 7 days. All per spec § Failure / Stale claims / Retention.

Observability per spec § Observability: `tracing` span on every status transition carrying `post_id` and `seq`; Sentry on the `pending → dead` edge only. An `Unauthorized` classification pages rather than logs — it means *nothing* is reaching the canister, not that one post failed (spec § Error handling).

- [ ] **Step 5: Commit**

```bash
git add src/posts/outbox.rs && git commit -m "feat: add outbox terminal transitions, reaper, retention, requeue"
```

---

## Task 11: `ic_sync` — the only canister-aware file

**Files:**
- Create: `src/posts/ic_sync.rs`

- [ ] **Step 1: Write the failing tests**

Behind a `CanisterSync` trait so the worker is testable without an IC connection.

```rust
#[test] fn record_translates_to_post_details_v1() { }

#[test] fn error_classification_matches_spec() {
    // spec § Canister error classification — the exact table
    assert_eq!(classify(Ok(Result_::Ok)),                    Outcome::Sent);
    assert_eq!(classify(Ok(Err_(DuplicatePostId))),          Outcome::Sent);
    assert_eq!(classify(Ok(Err_(Unauthorized))),             Outcome::DeadNow);
    assert_eq!(classify(Ok(Err_(PostNotFound))),             Outcome::DeadNow);
    assert_eq!(classify(Ok(Err_(CallError(..)))),            Outcome::Retry);
    assert_eq!(classify(Err(AgentError::..)),                Outcome::Retry);
}
```

- [ ] **Step 2-4: Run (fail) → implement → run (pass)**

`classify` is pure and separate from the call, which is what makes the table testable without a network. Note in a comment that `update_post_status` returns `()` — only transport outcomes are classifiable for it (spec § "`update_post_status` returns `()`").

- [ ] **Step 5: Commit**

```bash
git add src/posts/ic_sync.rs && git commit -m "feat: add ic_sync with canister error classification"
```

---

## Task 12: Outbox worker and the inline kick

**Files:**
- Create: `src/jobs/post_outbox_worker.rs`
- Modify: `src/main.rs`, `src/consts.rs`

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn drain_pass_sends_claimed_rows_via_stub_sync() { }
#[tokio::test] async fn dead_now_outcome_skips_retries_entirely() { }
#[tokio::test] async fn worker_without_lease_does_nothing() { }
#[tokio::test] async fn inline_kick_drains_without_holding_the_lease() {
    // spec § "The inline kick is not leased; the periodic worker is"
}
```

- [ ] **Step 2-4: Run (fail) → implement → run (pass)**

Mirror `src/jobs/worker.rs`: `run_one_pass` free function taking the lease + injected closures (testable), plus a `run` loop with `catch_unwind` and `tokio::select!` on the cancellation token — copy that panic-containment, it exists because a fire-and-forget task dying silently is very hard to notice.

Lease via `media_index::acquire_or_renew_lease`. Consts: `run_post_outbox_worker()`, `post_outbox_drain_interval_secs()` (default 30), `post_outbox_claim_ttl_secs()` (300), `post_outbox_max_attempts()` (12), `post_outbox_retention_days()` (7) — following the `run_sweep_worker()` shape at `src/consts.rs:154`.

- [ ] **Step 5: Commit**

```bash
git add src/jobs/post_outbox_worker.rs src/jobs/mod.rs src/main.rs src/consts.rs
git commit -m "feat: add leased outbox drain worker"
```

---

## Milestone D — Handlers behind flags

## Task 13: Flags and startup validation

**Files:**
- Modify: `src/consts.rs`, `src/main.rs`

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn read_local_without_dual_write_is_rejected() { }
#[test] fn dual_write_without_backend_admin_identity_is_rejected() {
    // spec § "Corollary: BACKEND_ADMIN_IDENTITY is currently optional"
}
#[test] fn both_off_is_valid() { }
```

- [ ] **Step 2-4: Run (fail) → implement → run (pass)**

`validate_flags()` returning `anyhow::Result<()>`, called in `main` before the server binds. Both rejections are spec § Rollout requirements; the second one prevents a silent queue of dead letters.

- [ ] **Step 5: Commit**

```bash
git add src/consts.rs src/main.rs && git commit -m "feat: add posts rollout flags with startup validation"
```

---

## Task 14: `update_metadata_impl` dual-write

**Files:**
- Modify: `src/routes/upload/update_video_metadata.rs:83-188`

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn dual_write_off_uses_the_canister_path_unchanged() { }
#[tokio::test] async fn dual_write_on_writes_postgres_and_enqueues_no_inline_call() { }
#[tokio::test] async fn duplicate_post_id_returns_409() { }

#[tokio::test]
async fn analytics_and_notification_still_fire_under_dual_write() {
    // `upload_video_canister` is being deleted, and the analytics event + push
    // notification currently live INSIDE its Result_::Ok arm
    // (update_video_metadata.rs:126-163). Removing the function without
    // rehoming them silently drops both — no test fails, no error logs.
    // Assert with stub EventService/NotificationClient that a Published post
    // fires both, and a Draft fires only the notification, matching today.
}
```

Keep the existing `authorize_publisher` / `inject_post_details` unit tests passing untouched — auth behavior does not change.

- [ ] **Step 2-4: Run (fail) → implement → run (pass)**

Branch on `POSTS_DUAL_WRITE`. Flag off = today's `upload_video_canister` verbatim. Flag on = `repo::create_post` + kick, **no inline canister call** (spec § Flag semantics).

Unchanged either way: identity verification, `inject_post_details`, the `finalize_via_http` hop (spec § "What the transaction does and does not cover" — it stays outside the transaction), and the analytics/notification side effects keyed off `Published`.

- [ ] **Step 5: Commit**

```bash
git add src/routes/upload/update_video_metadata.rs
git commit -m "feat: dual-write posts to postgres behind POSTS_DUAL_WRITE"
```

---

## Task 15: `mark_post_as_published` local ownership check

**Files:**
- Modify: `src/routes/upload/mark_post_as_published.rs:41-107`

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn read_local_off_uses_canister_lookup() { }
#[tokio::test] async fn read_local_on_uses_posts_table() { }
#[tokio::test] async fn read_local_on_falls_back_to_canister_on_local_miss() { }
#[tokio::test] async fn non_owner_is_rejected_401_from_either_source() { }
```

- [ ] **Step 2-4: Run (fail) → implement → run (pass)**

Per spec § Rewritten handlers and the `POSTS_READ_LOCAL` table. The fallback exists because the backfill may not have landed; it is removed in Phase 2 cleanup, not here.

- [ ] **Step 5: Commit**

```bash
git add src/routes/upload/mark_post_as_published.rs
git commit -m "feat: read post ownership locally behind POSTS_READ_LOCAL"
```

---

## Task 16: `/profile-image` users upsert

**Files:**
- Modify: `src/routes/user/profile_image.rs:137-215`

- [ ] **Step 1: Write the failing test**

Upload upserts `users.profile_picture_url`; delete clears it to `''`. The canister write stays inline and unchanged — spec § `/profile-image`, it signs as the user and cannot be outboxed.

- [ ] **Step 2-4: Run (fail) → implement → run (pass)**
- [ ] **Step 5: Commit**

```bash
git add src/routes/user/profile_image.rs
git commit -m "feat: mirror profile_picture_url into local users table"
```

---

## Task 16b: Remove the `/get-upload-url` canister call via an optional identity

Spec § `/get-upload-url` — the amended decision. The endpoint's `get_user_profile_details_v_6` call is the last `user_info_service` read on the post path. It exists to stop anyone minting an upload URL for an arbitrary principal, and it does that job badly: it proves the principal *exists*, never that the caller *is* that principal. A verified delegated identity proves both, so this is a security improvement that also deletes a canister call.

Done additively so no client breaks: identity present → verify and skip the canister; identity absent → today's behavior exactly.

**Files:**
- Modify: `src/routes/upload/get_upload_url.rs:23-100`
- Modify: `src/routes/videogen/generate.rs:1087-1110`

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn verified_identity_authorizes_without_a_canister_call() {
    // signed wire whose sender == publisher_user_id -> Caller::Verified, no agent used
}

#[test]
fn identity_whose_sender_differs_from_publisher_is_401() {
    // THE point of the change: proving existence was never proving ownership
    let (wire, _sender) = signed_wire_with_sender();
    let req = GetUploadUrlReq { publisher_user_id: other_principal(), delegated_identity_wire: Some(wire) };
    assert!(matches!(authorize_caller(&req), Err(AppError::Unauthorized(_))));
}

#[test]
fn forged_chain_is_401_not_a_fallback_to_the_canister() {
    // a present-but-invalid identity must FAIL, never silently degrade to the
    // unauthenticated path — that would make the check bypassable by sending garbage
}

#[test]
fn absent_identity_still_takes_the_canister_path() { }

#[tokio::test]
async fn videogen_internal_path_makes_no_canister_call() {
    // /generate already chain-verified the sender (generate.rs:442 rejects anonymous,
    // :504-514 asserts identity_principal == user_id), so reserve_upload_destination
    // passes Caller::Verified and must not re-check
}
```

- [ ] **Step 2: Run, confirm fail**

Run: `cargo test --lib routes::upload::get_upload_url -- --nocapture`
Expected: FAIL — `delegated_identity_wire` is not a field of `GetUploadUrlReq`.

- [ ] **Step 3: Implement**

```rust
pub enum Caller<'a> {
    /// Chain-verified principal. No existence check needed — the delegation chain
    /// proves the caller controls it, which is strictly stronger than "it exists".
    Verified(Principal),
    /// Legacy unauthenticated body. Falls back to the user_info_service check.
    Unverified(&'a str),
}
```

Add `delegated_identity_wire: Option<DelegatedIdentityWire>` to `GetUploadUrlReq` (serde default, so existing bodies deserialize unchanged). `authorize_caller` is pure and unit-testable:

- `Some(wire)` → `verify_delegated_identity` (the chain-checking one at `routes/identity_auth.rs:24`, never `new_unchecked`); sender must equal `publisher_user_id` or 401; invalid chain → 401, **never** fall through to the legacy path.
- `None` → `Caller::Unverified`.

`get_upload_url_core` takes `Caller` and only touches `ic_agent` in the `Unverified` arm. Change videogen's call site to `Caller::Verified(principal)` — it holds an already-verified principal and currently pays a canister round-trip per generation for nothing.

- [ ] **Step 4: Run, confirm pass**

Run: `cargo test --lib routes::upload::get_upload_url routes::videogen -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/routes/upload/get_upload_url.rs src/routes/videogen/generate.rs
git commit -m "feat: authorize get-upload-url by delegated identity, skipping the canister check"
```

> **Handoff to Phase 2.** The legacy `Unverified` arm is the only remaining `user_info_service` call in this service. Mobile sends `delegated_identity_wire` on `/get-upload-url` (it already holds one for `/update-video-metadata`), adoption drains, then the arm and `Caller::Unverified` are deleted — at which point `user_info_service` is gone from the post path entirely. Record this in the Phase 2 mobile ticket alongside the three read migrations.

---

## Milestone E — Backfill and reconcile

## Task 17: Extend `chain_snapshot` to import posts

**Files:**
- Modify: `src/jobs/chain_snapshot.rs:76-187`
- Create: `src/posts/reconcile.rs`

- [ ] **Step 1: Write the failing tests**

Against the existing `MockSource` — reuse it, it already exists at `chain_snapshot.rs:291`.

```rust
#[tokio::test] async fn backfill_populates_posts_likes_and_view_stats() { }
#[tokio::test] async fn backfill_uses_chain_created_at_not_now() { }
#[tokio::test] async fn backfill_produces_zero_outbox_rows() { }
#[tokio::test] async fn backfill_still_writes_yral_posts_unchanged() {
    // the audit contract must not move — spec § Consequence for chain-coverage-audit
}
#[tokio::test] async fn backfill_keeps_like_count_consistent_with_post_likes() {
    // trigger stays armed through the backfill, so this is just the trigger
    // doing its job — no recompute pass to verify
}
```

- [ ] **Step 2-4: Run (fail) → implement → run (pass)**

Write both destinations from one page. Preserve: `media_job_runs` tracking, `MAX_ITERS`, cancellation, and the partial-run rule that only a complete walk marks stale.

Insert `post_likes` normally with the `like_count` trigger armed — no `COPY`, no trigger disable, no deferred index build, and `PAGE` stays at 100. The bulk-load path this task originally specified was retracted after Task 0 measured the corpus at ~17k like rows (spec § Volume).

- [ ] **Step 5: Commit**

```bash
git add src/jobs/chain_snapshot.rs src/posts/reconcile.rs
git commit -m "feat: extend chain snapshot to backfill posts and likes"
```

---

## Task 18: Reconcile ownership rules

**Files:**
- Modify: `src/posts/reconcile.rs`

- [ ] **Step 1: Write the failing tests — one per rule in the spec table**

```rust
#[tokio::test] async fn service_owned_columns_are_never_overwritten() { }
#[tokio::test] async fn chain_banned_overwrites_local_uploaded() { }
#[tokio::test] async fn chain_deleted_sets_deleted_at_in_the_same_statement() { }
#[tokio::test] async fn chain_draft_does_not_overwrite_local_uploaded() { }
#[tokio::test] async fn legacy_variants_leave_local_status_untouched() {
    // all four of them — spec § "The status rule, stated precisely"
}
#[tokio::test] async fn absent_rows_are_inserted_as_chain_reconcile() { }
#[tokio::test] async fn no_change_writes_zero_post_events() { }
```

- [ ] **Step 2-4: Run (fail) → implement → run (pass)**

Implement the rule as written: overwrite `status` **only** when the chain value is `Deleted` or `BannedDueToUserReporting` and local is not. It is total over all eight variants — assert that with an exhaustive loop over `PostStatus::ALL`, not six hand-written cases.

- [ ] **Step 5: Commit**

```bash
git add src/posts/reconcile.rs && git commit -m "feat: add reconcile ownership rules"
```

---

## Task 19: Like reconciliation via cardinality gate

**Skip if Task 0 deferred `post_likes`.**

**Files:**
- Modify: `src/posts/reconcile.rs`

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn equal_cardinality_skips_the_post_entirely() {
    // assert ZERO post_likes statements issued — this is the whole optimization
}
#[tokio::test] async fn unequal_cardinality_diffs_and_fixes_like_count() { }
```

- [ ] **Step 2-4: Run (fail) → implement → run (pass)**

Per spec § "Why `post_likes` cannot simply be overwritten": compare `likes.len()` to `like_count`, diff only on mismatch. Document the known limitation in a comment — a simultaneous like and unlike between passes is invisible, and that is accepted.

- [ ] **Step 5: Commit**

```bash
git add src/posts/reconcile.rs && git commit -m "feat: reconcile likes behind a cardinality gate"
```

---

## Task 20: Drift metric

**Files:**
- Modify: `src/posts/reconcile.rs`, `src/routes/chain.rs`

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn drift_counts_canister_owned_column_differences() { }
#[tokio::test] async fn drift_ignores_created_at() {
    // spec § "What drift compares" — dual-write posts differ here permanently
    // and by design; counting it makes the metric read 100% forever
}
#[tokio::test] async fn drift_ignores_description_and_hashtags() { }
```

- [ ] **Step 2-4: Run (fail) → implement → run (pass)**

Compare only `status`, `share_count`, view stats, `like_count`. Surface in the existing `/chain/audit` response.

- [ ] **Step 5: Commit**

```bash
git add src/posts/reconcile.rs src/routes/chain.rs
git commit -m "feat: add posts-vs-chain drift metric scoped to canister-owned columns"
```

---

## Milestone F — Read API

## Task 21: Endpoints, auth, rate limit, swagger

**Files:**
- Create: `src/posts/api.rs`
- Modify: `src/main.rs`, `src/routes/mod.rs`

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn drafts_rejects_a_forged_delegation_chain_with_401() {
    // mirror the existing upload_forged_identity_is_401 test
}
#[tokio::test] async fn drafts_ignores_a_principal_in_the_body_and_uses_the_verified_sender() {
    // spec: creator is derived, NEVER a parameter
}
#[tokio::test] async fn liked_by_me_is_null_without_a_verified_identity() { }
#[tokio::test] async fn deleted_and_banned_posts_are_404() { }
#[tokio::test] async fn no_response_ever_contains_a_like_list() {
    // spec § "post_likes must never be exposed"
}
#[test] fn post_details_json_matches_the_golden_file() {
    // RFC3339 created_at, principal-as-text, both principal fields present.
    // This is the artifact Phase 2 codes against — it must fail loudly on a rename.
}
#[test] fn limit_is_clamped_to_100_not_rejected() { }
```

- [ ] **Step 2-4: Run (fail) → implement → run (pass)**

Three routes per spec § Read API. Public (no `authorize` layer) matching the canister's open queries; drafts authenticated via the in-body delegated identity using `routes::identity_auth::verify_delegated_identity`. `PostDetails` exactly per spec § "`PostDetails` — the concrete shape".

Add the per-IP rate limit on the two public reads (spec § "Access control and abuse") — these are the first high-fanout public reads in front of a database also running the phash pipeline.

> **Dependency decision, not an implementation detail.** `tower-http` is already a dependency but has no rate limiter; `tower` has `RateLimit`, which is global rather than per-IP and therefore not what the spec asks for. The straightforward option is `tower_governor` (`tower-governor = "0.4"`), keyed on the peer IP with a `SmartIpKeyExtractor` so it reads `X-Forwarded-For` — required here, because Caddy and HAProxy sit in front (`deploy/Caddyfile`, `deploy/haproxy`) and every request otherwise arrives from the proxy's address, making a naive limiter either useless or a global outage. **Confirm the crate choice before implementing**; if a new dependency is unwelcome, the fallback is enforcing the limit at Caddy instead, which is arguably the better layer anyway.

Register in `ApiDoc` under a new `posts` tag.

- [ ] **Step 5: Commit**

```bash
git add src/posts/api.rs src/main.rs src/routes/mod.rs
git commit -m "feat: add public post read API with keyset pagination"
```

---

## Task 22: Operator endpoints and the isolation guard

**Files:**
- Modify: `src/posts/api.rs`, `src/main.rs`
- Create: `tests/posts_isolation_guard.rs`

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn outbox_stats_reports_all_five_counters() { }
#[tokio::test] async fn requeue_revives_a_dead_row() { }
#[tokio::test] async fn operator_endpoints_require_authorization() { }
```

Plus the guard that makes Phase 4 a deletion — a source-level assertion, the same spirit as the existing `tests/canister_symbols_guard.rs`:

```rust
#[test]
fn only_ic_sync_imports_the_canister_client() {
    for f in ["types.rs", "repo.rs", "outbox.rs", "api.rs", "reconcile.rs"] {
        let src = std::fs::read_to_string(format!("src/posts/{f}")).unwrap();
        assert!(!src.contains("yral_canisters_client"),
            "src/posts/{f} imports the canister client; Phase 4 deletion depends on \
             ic_sync.rs being the only file that does");
    }
}
```

- [ ] **Step 2-4: Run (fail) → implement → run (pass)**

Both endpoints behind `middleware::from_fn(authorize)` — spec § Observability. `requeue` is the only recovery path for a dead-blocked post, so it is not optional.

- [ ] **Step 5: Commit**

```bash
git add src/posts/api.rs src/main.rs tests/posts_isolation_guard.rs
git commit -m "feat: add outbox operator endpoints and canister isolation guard"
```

---

## Task 23: Full verification

- [ ] **Step 1: Whole suite**

Run: `cargo test --all`
Expected: PASS. Docker must be running — the Postgres-backed tests spawn containers.

- [ ] **Step 2: Lint and format**

Run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 3: Confirm defaults are off**

Grep the diff: `RUN_POST_OUTBOX_WORKER`, `POSTS_DUAL_WRITE`, `POSTS_READ_LOCAL` all default `false`. Nothing in this plan changes production behavior until a flag is flipped.

- [ ] **Step 4: Retire the duplicate schema path**

The loose end opened in Task 1 Step 8. Replace every test-side `db::init_schema` / `media_index::init_schema` / `videogen::request_store::init_schema` call with `migrations::run_migrations`, then delete the three `SCHEMA_SQL` constants and their functions. Mechanical, but do it here rather than "later" — two live schema systems is the drift the spec argues against, and the only reason they coexisted was to keep this plan's diffs readable.

Run: `cargo test --all && grep -rn "init_schema" src/ tests/`
Expected: PASS, and the grep returns nothing outside `migrations.rs`.

- [ ] **Step 5: Re-read spec § Rollout**

The build order in this plan is not the rollout order. Deploy dark, then follow the spec's five steps, in order.

- [ ] **Step 6: Commit**

```bash
git commit -am "refactor: retire pre-refinery init_schema paths"
```

---

## Files this plan deliberately does not touch

- **The `Caller::Unverified` arm of `/get-upload-url`** — Task 16b makes the canister call skippable but does **not** delete it, because clients that send no identity must keep working. Removing the arm is a Phase 2 cleanup gated on mobile adoption. Do not delete it early; that breaks every installed app.
- **`users` as an existence check** — do not "fix" the legacy arm by looking the principal up in the local `users` table. Registration lives in `user_info_service`, so a real new user has no row here and would be rejected. Identity or canister; there is no third option.
- **`src/routes/upload/draft_client.rs`** — goes through `update_metadata_impl` and inherits Task 14 for free. It keeps producing a `PostDetailsFromFrontendV1`; only the consumer changes.
- **`src/jobs/chain_snapshot.rs`'s `yral_posts` writes** — the chain-audit contract must not move. Task 17 adds a destination; it changes nothing existing.

## Not in this plan

Per spec § Out of scope — Phase 3 write endpoints for off-chain-agent, the `user_info_service` graph, rebuilding chain-coverage-audit (blocks Phase 4b), migrating the ~20 existing `db::connect()` sites to a pool, `post_events` retention, requiring an identity on `/get-upload-url`, and the analytics `canister_id` rename.

Two spec recommendations are deliberately excluded because they are infrastructure, not code, and land on their own track: WAL archiving for PITR (spec says before or with Phase 3) and Postgres TLS.
