# Videogen pHash Key Fix + Exponential Backoff Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Register completed videogen videos with the correct prefixed Storj key so they hash, and replace the flat 24h failure quarantine with exponential backoff.

**Architecture:** `resolve_source` derives the bucket-relative key from the completion's `bucket_url` (authoritative). Failed downloads populate the existing `retry_count`/`next_retry_at` columns with exponential backoff; both the drain selection and the eligibility gate exclude rows whose `next_retry_at` is in the future via one shared SQL predicate. A successful hash clears the video's failure row.

**Tech Stack:** Rust, axum, tokio-postgres, Postgres (Storj S3 via gateway). Tests use ephemeral `PgContainer` — run DB tests with `--test-threads=1`.

**Spec:** `docs/superpowers/specs/2026-06-30-videogen-phash-key-and-backoff-design.md`

**Test command (all DB tests):**
`SDKROOT=$(xcrun --show-sdk-path) cargo test -p storj-interface --lib <filter> -- --test-threads=1`

---

## File Structure

- `src/jobs/ingest.rs` — `resolve_source(bucket_url)` derives key from URL; `on_video_ingested` drops `object_key`. (Task 1)
- `src/routes/videogen/complete.rs` — `CompletionDeps::register_ingested` drops `object_key` (default + impl + call site). (Task 1)
- `src/jobs/media_phash.rs` — failure upsert sets `retry_count`/`next_retry_at` (Task 2); `persist_one` Ok branch clears failure row (Task 4).
- `src/media_index/repo.rs` — shared `NOT_BACKING_OFF` predicate; `any_eligible_for_hash` drops window param; `videos_missing_canonical_phash` adds the predicate. (Task 3)
- `src/jobs/worker.rs` + `src/main.rs` — drop `failed_within` from the call chain (signature ripple from Task 3). (Task 3)

---

## Task 1: Core key fix — derive Storj key from `bucket_url`

**Files:**
- Modify: `src/jobs/ingest.rs` (`resolve_source`, `on_video_ingested`, tests)
- Modify: `src/routes/videogen/complete.rs` (`CompletionDeps::register_ingested` default `:135`, call site `:248`, `RuntimeCompletionDeps` impl `:554`)

- [ ] **Step 1: Rewrite the `resolve_source` tests** (in `src/jobs/ingest.rs` `#[cfg(test)] mod tests`)

Replace `resolves_videogen_storj_sfw` and `rejects_unknown_host`, and add edge cases:

```rust
#[test]
fn resolve_source_takes_key_from_bucket_url_not_object_key() {
    // bucket_url is the real download URL: {base}/yral-sfw/{principal}/{uuid}.mp4.
    // The bare `object_key` request field (no prefix) is the bug — must be ignored.
    let src = resolve_source(
        "https://link.storjshare.io/raw/tok/yral-sfw/km5ld-principal/5a08-uuid.mp4",
    )
    .unwrap();
    assert_eq!(src.storage_provider, "storj");
    assert_eq!(src.bucket, "yral-sfw");
    assert_eq!(src.object_key, "km5ld-principal/5a08-uuid.mp4");
}

#[test]
fn resolve_source_strips_query_and_fragment() {
    let src =
        resolve_source("https://link.storjshare.io/raw/tok/yral-sfw/p/u.mp4?download=1#x")
            .unwrap();
    assert_eq!(src.object_key, "p/u.mp4");
}

#[test]
fn resolve_source_rejects_missing_marker() {
    assert!(resolve_source("https://unknown.example/p/u.mp4").is_err());
}

#[test]
fn resolve_source_rejects_empty_key_tail() {
    // bucket base with no object path → cannot derive a key.
    assert!(resolve_source("https://link.storjshare.io/raw/tok/yral-sfw/").is_err());
}
```

- [ ] **Step 2: Run tests, verify they fail to compile** (signature mismatch — `resolve_source` still takes 2 args)

Run: `SDKROOT=$(xcrun --show-sdk-path) cargo test -p storj-interface --lib ingest::tests::resolve_source -- --test-threads=1`
Expected: compile error (wrong arg count / unused).

- [ ] **Step 3: Rewrite `resolve_source`** (`src/jobs/ingest.rs:35`)

```rust
/// Derive the storage triple from the videogen completion's `bucket_url`.
///
/// `bucket_url` is the real Storj download URL,
/// `{share_base}/yral-sfw/{user_principal}/{video_id}.mp4`, so the
/// bucket-relative object key is everything after the first `/yral-sfw/`.
/// The completion also carries a bare `object_key` request field WITHOUT the
/// principal prefix — that is the bug this replaces; we ignore it and take the
/// key from the URL, which is authoritative.
pub fn resolve_source(bucket_url: &str) -> Result<VideoSource, ResolveError> {
    const MARKER: &str = "/yral-sfw/";
    let tail = bucket_url
        .split_once(MARKER)
        .map(|(_, rest)| rest)
        .ok_or_else(|| ResolveError::UnknownSource(bucket_url.to_string()))?;
    // Strip any query string / fragment from the raw URL.
    let key = tail
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .trim_matches('/');
    if key.is_empty() {
        return Err(ResolveError::UnknownSource(bucket_url.to_string()));
    }
    Ok(VideoSource {
        storage_provider: "storj",
        bucket: "yral-sfw".into(),
        object_key: key.to_string(),
    })
}
```

- [ ] **Step 4: Update `on_video_ingested`** (`src/jobs/ingest.rs:99`) — drop `object_key` param

```rust
pub async fn on_video_ingested(db_url: &str, video_id: &str, bucket_url: &str) {
    let src = match resolve_source(bucket_url) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                video_id,
                %e,
                "ingest: unresolved source, skipping inline register (sweep will catch it)"
            );
            return;
        }
    };
    // ...rest unchanged...
```

- [ ] **Step 5: Update `registers_video_into_missing_set` test** (`src/jobs/ingest.rs:138`)

```rust
    let src = resolve_source(
        "https://link.storjshare.io/raw/x/yral-sfw/principal/vid-1.mp4",
    )
    .unwrap();
    register_master_row(&mut client, "vid-1", &src)
        .await
        .unwrap();
    // ...
    assert_eq!(r.object_key.as_deref(), Some("principal/vid-1.mp4"));
```

- [ ] **Step 6: Update the trait + call site in `complete.rs`** — drop `object_key`

`src/routes/videogen/complete.rs:135` (default method):
```rust
    async fn register_ingested(&self, _video_id: &str, _bucket_url: &str) {}
```
`:248` (call site in `handle_success_completion`):
```rust
    deps.register_ingested(video_id, bucket_url).await;
```
`:554` (`RuntimeCompletionDeps` impl):
```rust
    async fn register_ingested(&self, video_id: &str, bucket_url: &str) {
        crate::jobs::ingest::on_video_ingested(&self.db_url, video_id, bucket_url).await;
    }
```
Also update any test mock impl of `register_ingested` in `complete.rs` tests to the new signature.

- [ ] **Step 7: Run the full ingest + complete tests**

Run: `SDKROOT=$(xcrun --show-sdk-path) cargo test -p storj-interface --lib -- ingest:: videogen::complete:: --test-threads=1`
Expected: PASS. Then `cargo clippy -p storj-interface --lib` — no new warnings (no unused `object_key`).

- [ ] **Step 8: Commit**

```bash
git add src/jobs/ingest.rs src/routes/videogen/complete.rs
git commit -m "fix: derive videogen Storj key from bucket_url (prefixed path), not bare object_key"
```

---

## Task 2: Exponential backoff on failure upsert

**Files:**
- Modify: `src/jobs/media_phash.rs` (failure upsert, ~`:424`)
- Test: `src/media_index/repo.rs` or `src/jobs/media_phash.rs` tests

- [ ] **Step 1: Write the failing test** (add to `src/media_index/repo.rs` tests, or a `media_phash` test that calls the failure upsert)

Insert two failures for the same `(job_kind,item_key,phase)` and assert backoff:

```rust
#[tokio::test]
async fn failure_upsert_sets_exponential_backoff() {
    let (_pg, client) = test_client_owned().await; // an owned Client + schema
    crate::media_index::init_schema(&client).await.unwrap();

    // First failure: retry_count=1, next_retry_at ≈ now()+5m.
    insert_phash_failure(&client, "vid-b", "boom").await;
    let (rc1, secs1) = read_backoff(&client, "vid-b").await;
    assert_eq!(rc1, 1);
    assert!((250.0..=350.0).contains(&secs1), "≈5m, got {secs1}s");

    // Second failure (conflict): retry_count=2, next_retry_at ≈ now()+10m.
    insert_phash_failure(&client, "vid-b", "boom again").await;
    let (rc2, secs2) = read_backoff(&client, "vid-b").await;
    assert_eq!(rc2, 2);
    assert!((550.0..=650.0).contains(&secs2), "≈10m, got {secs2}s");
}
```

`read_backoff` helper:
```rust
async fn read_backoff(client: &Client, video_id: &str) -> (i32, f64) {
    let row = client
        .query_one(
            "SELECT retry_count, EXTRACT(EPOCH FROM (next_retry_at - now()))::float8
             FROM media_job_failures WHERE video_id = $1",
            &[&video_id],
        )
        .await
        .unwrap();
    (row.get(0), row.get(1))
}
```
`insert_phash_failure` wraps the production failure-upsert fn in a transaction (reuse the fn touched in Step 3; export it `pub(crate)` if needed).

- [ ] **Step 2: Run, verify fail**

Run: `SDKROOT=$(xcrun --show-sdk-path) cargo test -p storj-interface --lib failure_upsert_sets_exponential_backoff -- --test-threads=1`
Expected: FAIL — `retry_count` is 0 / `next_retry_at` is NULL (current upsert sets neither).

- [ ] **Step 3: Update the failure upsert** (`src/jobs/media_phash.rs:424-452`)

```rust
    tx.execute(
        "INSERT INTO media_job_failures (
            job_run_id, job_kind, item_key, video_id, phase, source_ref,
            last_error, status, retry_count, next_retry_at
         )
         VALUES ($1::TEXT::UUID, $2, $3, $4, $5, $6, $7, 'pending_retry',
                 1, now() + interval '5 minutes')
         ON CONFLICT (job_kind, item_key, phase) DO UPDATE
         SET job_run_id    = EXCLUDED.job_run_id,
             video_id      = EXCLUDED.video_id,
             source_ref    = EXCLUDED.source_ref,
             last_error    = EXCLUDED.last_error,
             status        = EXCLUDED.status,
             retry_count   = media_job_failures.retry_count + 1,
             next_retry_at = now() + LEAST(
                 interval '5 minutes' * power(2, media_job_failures.retry_count),
                 interval '24 hours'
             )",
        &[
            &job_run_id.to_string(),
            &JOB_KIND,
            &video_id,
            &video_id,
            &phase,
            &video_id,
            &error,
        ],
    )
    .await?;
```
(Args unchanged — 7 params. On INSERT: `retry_count=1`, `+5m` = `5m·2^0`. On UPDATE: uses the *pre-increment* `retry_count` as the exponent → `+10m, +20m, …`, `LEAST(_, 24h)`.)

- [ ] **Step 4: Run, verify pass**

Run: same as Step 2. Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/jobs/media_phash.rs src/media_index/repo.rs
git commit -m "feat: exponential backoff (retry_count/next_retry_at) on phash download failure"
```

---

## Task 3: Backoff-aware eligibility + drain selection (shared predicate)

**Files:**
- Modify: `src/media_index/repo.rs` (`NOT_BACKING_OFF` const, `videos_missing_canonical_phash:404`, `any_eligible_for_hash:515`, tests)
- Modify: `src/jobs/worker.rs` (`run_one_pass:136`, `SweepWorker` field `:182` + `run_pass:321`)
- Modify: `src/main.rs:302` (drop `failed_within`)

- [ ] **Step 1: Write/replace the failing tests** (`src/media_index/repo.rs`)

Replace `any_eligible_for_hash_quarantines_recent_failures` with a `next_retry_at` version, and add a selection test:

```rust
#[tokio::test]
async fn eligibility_and_selection_respect_next_retry_at() {
    let (_pg, client) = test_client_owned().await;
    crate::media_index::init_schema(&client).await.unwrap();
    seed_missing_video(&client, "vid-due").await;   // missing canonical phash, no failure

    // No failure → eligible + selected.
    assert!(super::any_eligible_for_hash(&client, HASH_KIND, HASH_VERSION, imv).await.unwrap());
    assert!(missing_contains(&client, "vid-due").await);

    // Failure backing off (next_retry_at in the FUTURE) → not eligible, not selected.
    set_failure(&client, "vid-due", "now() + interval '1 hour'").await;
    assert!(!super::any_eligible_for_hash(&client, HASH_KIND, HASH_VERSION, imv).await.unwrap());
    assert!(!missing_contains(&client, "vid-due").await);

    // next_retry_at in the PAST (due) → eligible + selected again.
    set_failure(&client, "vid-due", "now() - interval '1 minute'").await;
    assert!(super::any_eligible_for_hash(&client, HASH_KIND, HASH_VERSION, imv).await.unwrap());
    assert!(missing_contains(&client, "vid-due").await);
}
```
Helpers: `set_failure` UPSERTs a `media_job_failures` row with the given `next_retry_at` SQL; `missing_contains` calls `videos_missing_canonical_phash(.., None, Some(100), None)` and checks for the id. Note the new `any_eligible_for_hash` has **no** `failed_within` arg.

- [ ] **Step 2: Run, verify fail to compile** (`any_eligible_for_hash` still takes the window arg)

Run: `SDKROOT=$(xcrun --show-sdk-path) cargo test -p storj-interface --lib eligibility_and_selection -- --test-threads=1`
Expected: compile error (arg count).

- [ ] **Step 3: Add the shared predicate + update both queries** (`src/media_index/repo.rs`)

Near the top of the repo module:
```rust
/// Shared SQL: video `v` has no failure row currently backing off
/// (`next_retry_at` in the future). Used by BOTH the drain selection and the
/// eligibility gate so they cannot diverge. Requires the outer query to alias
/// `all_servable_videos_on_yral` as `v`.
const NOT_BACKING_OFF: &str = "NOT EXISTS (\
    SELECT 1 FROM media_job_failures f \
    WHERE f.video_id = v.video_id AND f.next_retry_at > now())";
```

`videos_missing_canonical_phash` — interpolate the predicate into `base`:
```rust
    let base = format!(
        "SELECT v.video_id, v.storage_provider, v.object_key, v.servable_status, v.bucket
         FROM all_servable_videos_on_yral v
         LEFT JOIN servable_video_hashes h
            ON h.video_id = v.video_id
           AND h.hash_kind = $1 AND h.hash_version = $2 AND h.input_media_version = $3
         WHERE h.video_id IS NULL
           AND ($4::TEXT IS NULL OR v.video_id > $4)
           AND ($5::BIGINT IS NULL
                OR (((hashtext(v.video_id)::bigint % $5) + $5) % $5) = $6)
           AND {NOT_BACKING_OFF}
         ORDER BY v.video_id"
    );
```
(`base` becomes a `String`; the `LIMIT $7` branch uses `format!("{base} LIMIT $7")`, the no-limit branch passes `&base`. The predicate contains no params, so numbering is unaffected.)

`any_eligible_for_hash` — drop `failed_within`, use the predicate:
```rust
pub async fn any_eligible_for_hash(
    client: &Client,
    hash_kind: &str,
    hash_version: &str,
    input_media_version: &str,
) -> Result<bool, tokio_postgres::Error> {
    let sql = format!(
        "SELECT EXISTS (
           SELECT 1
           FROM all_servable_videos_on_yral v
           LEFT JOIN servable_video_hashes h
             ON h.video_id = v.video_id
            AND h.hash_kind = $1 AND h.hash_version = $2 AND h.input_media_version = $3
           WHERE h.video_id IS NULL
             AND {NOT_BACKING_OFF}
           LIMIT 1
         ) AS eligible"
    );
    let row = client
        .query_one(&sql, &[&hash_kind, &hash_version, &input_media_version])
        .await?;
    Ok(row.get("eligible"))
}
```

- [ ] **Step 4: Update the worker call chain** (drop `failed_within`)

`src/jobs/worker.rs:136`:
```rust
    let eligible = crate::media_index::any_eligible_for_hash(
        &client,
        phash::HASH_KIND,
        phash::HASH_VERSION,
        crate::jobs::media_phash::INPUT_MEDIA_VERSION,
    )
    .await?;
```
Remove the `failed_within: std::time::Duration` parameter from `run_one_pass` (`:122`), the `failed_within` field from `SweepWorker` (`:182`), and the `self.failed_within` argument in `run_pass` (`:321`).

`src/main.rs:302`: remove the `failed_within: std::time::Duration::from_secs(consts::discovery_interval_secs()),` line from the `SweepWorker` construction.

- [ ] **Step 5: Run repo + worker tests + build**

Run: `SDKROOT=$(xcrun --show-sdk-path) cargo test -p storj-interface --lib -- repo:: worker:: --test-threads=1`
Then: `SDKROOT=$(xcrun --show-sdk-path) cargo build -p storj-interface` (bin compiles — main.rs change).
Expected: PASS / clean build.

- [ ] **Step 6: Commit**

```bash
git add src/media_index/repo.rs src/jobs/worker.rs src/main.rs
git commit -m "feat: gate drain selection + eligibility on next_retry_at backoff (shared predicate); drop failed_within"
```

---

## Task 4: Clear failure row on hash success

**Files:**
- Modify: `src/jobs/media_phash.rs` (`persist_one` Ok branch, before `tx.commit()` ~`:318`)
- Test: `src/jobs/media_phash.rs` tests

- [ ] **Step 1: Write the failing test** (`src/jobs/media_phash.rs` tests)

```rust
#[tokio::test]
async fn successful_hash_clears_prior_failure_row() {
    let (_pg, mut client) = crate::media_index::test_support::test_client().await;
    crate::media_index::init_schema(&client).await.unwrap();
    // seed a servable row + a prior failure for it
    seed_servable_storj_row(&client, "video-ok").await;
    insert_phash_failure(&client, "video-ok", "earlier transient").await;

    let row = make_missing_row("video-ok"); // MissingHashRow for video-ok
    let mut summary = PHashSummary::default();
    persist_one(&mut client, Uuid::new_v4(), &row, Ok(make_hash_result("ab12")), &mut summary)
        .await
        .unwrap();

    let n: i64 = client
        .query_one(
            "SELECT count(*) FROM media_job_failures WHERE video_id = 'video-ok'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(n, 0, "failure row cleared on success");
}
```

- [ ] **Step 2: Run, verify fail**

Run: `SDKROOT=$(xcrun --show-sdk-path) cargo test -p storj-interface --lib successful_hash_clears_prior_failure_row -- --test-threads=1`
Expected: FAIL — failure row still present (count 1).

- [ ] **Step 3: Add the DELETE in the Ok branch** (`src/jobs/media_phash.rs`, in `persist_one` `Ok(vhr) =>` arm, immediately before `tx.commit().await?;`)

```rust
            // Clean slate: a successful hash clears any prior failure backoff so
            // retry_count resets if this video ever fails again later.
            tx.execute(
                "DELETE FROM media_job_failures WHERE job_kind = $1 AND video_id = $2",
                &[&JOB_KIND, &row.video_id],
            )
            .await?;

            tx.commit().await?;
```

- [ ] **Step 4: Run, verify pass**

Run: same as Step 2. Expected: PASS. Then run the existing `persist_*` tests to confirm no regression:
`SDKROOT=$(xcrun --show-sdk-path) cargo test -p storj-interface --lib persist -- --test-threads=1`

- [ ] **Step 5: Commit**

```bash
git add src/jobs/media_phash.rs
git commit -m "feat: clear media_job_failures row on successful phash (reset backoff)"
```

---

## Task 5: Full build + lib suite green

- [ ] **Step 1: Whole lib suite serialized**

Run: `SDKROOT=$(xcrun --show-sdk-path) cargo test -p storj-interface --lib -- --test-threads=1`
Expected: all pass. Investigate any failure (do NOT assume flake without reading the error).

- [ ] **Step 2: clippy + fmt**

Run: `cargo fmt && SDKROOT=$(xcrun --show-sdk-path) cargo clippy -p storj-interface --all-targets 2>&1 | grep -E "^warning|^error" | head`
Expected: no new warnings in touched files.

- [ ] **Step 3: Push branch**

```bash
git push -u origin prakash/videogen-phash-key-fix
```

---

## Task 6: Independent code review (before preview)

- [ ] Dispatch `superpowers:requesting-code-review` (or a fresh reviewer subagent) over the branch diff vs `main`. Address CRITICAL/HIGH findings; re-review.

---

## Task 7: Rollout (preview first, then prod) — operational, not code

> Execute these manually with the operator (Prakash); not part of the automated build.

- [ ] **Preview:** deploy branch to the preview app. Run a real videogen → tail `storj-interface` logs for `marked complete` + absence of `ingest:` warn; confirm `media-audit` `with_canonical_phash` ticks up and `media-runs` shows `hash_rows_upserted >= 1`. Force a failure (e.g. a bogus key) → confirm `next_retry_at` ≈ now()+5m (not 24h) and it is NOT re-attempted next tick.
- [ ] **Merge → deploy to the 3 prod servers.**
- [ ] **Immediately pre-deploy:** re-seed `UPDATE sweep_lease SET last_discovery_at = now() WHERE id = 1` (keep discovery suppressed).
- [ ] **Fix the stuck `5a08…` row:** `UPDATE all_servable_videos_on_yral SET object_key = '<user_principal>/5a087732-1242-4ce4-b809-e6e89132f0d2.mp4' WHERE video_id = '5a087732-1242-4ce4-b809-e6e89132f0d2';` then `DELETE FROM media_job_failures WHERE video_id = '5a087732-1242-4ce4-b809-e6e89132f0d2';`
- [ ] **Tame the deploy burst (optional):** for the 9 dead rows (`next_retry_at IS NULL`), `UPDATE media_job_failures SET next_retry_at = now() + interval '24 hours' WHERE next_retry_at IS NULL;` — or accept the short backoff ramp.
- [ ] **Prod smoke test:** run a videogen → confirm register → hash green end to end.

---

## Notes for the implementer

- DB tests MUST run with `--test-threads=1` (concurrent `PgContainer` startup is flaky — `docker run` exit 125 is the symptom, not a code bug).
- The `media_job_failures` `BEFORE UPDATE` trigger sets `updated_at=now()` only; it does NOT touch `next_retry_at`, so the backoff UPDATE is safe.
- `power()` returns `double precision`; `interval * double precision` and `LEAST(interval, interval)` are valid Postgres.
- Do not add NSFW handling — out of scope.
