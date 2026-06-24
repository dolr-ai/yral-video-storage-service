# Resumable, Batched video_index Import — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `import_current_video_index` complete reliably at 700k–1M rows — resumable (skip-existing), memory-safe (paged), and fast (batched commits) — without changing the route, client, schema, or `media_phash`.

**Architecture:** Replace the load-all `fetch_legacy_rows` with a paged, `video_id`-cursored, skip-existing anti-join scan. Replace the per-row commit loop with an optimistic one-transaction-per-page commit, falling back to per-row transactions only when a page hits a genuine SQL error. Extract the existing per-row import work into a reusable `import_one_row_txn` so both paths share it.

**Tech Stack:** Rust, tokio-postgres, `tokio_util::sync::CancellationToken`, `once_cell::Lazy<AtomicI64>`, testcontainers.

**Spec:** `docs/superpowers/specs/2026-06-24-resumable-batched-import-design.md`

---

## File Structure

- `src/consts.rs` — add `MEDIA_IMPORT_BATCH_SIZE` (`Lazy<AtomicI64>`, default 500), modeled on the existing `SCAN_PAGE_SIZE`.
- `src/jobs/media_imports.rs` — the whole change:
  - add paged skip-existing scan `fetch_missing_legacy_rows_after`
  - extract `import_one_row_txn` + `RowCounts` from the current inline loop body
  - rewrite `import_current_video_index_inner` into the paged batched loop with per-row fallback
  - delete the obsolete `fetch_legacy_rows`
  - tests added to its `#[cfg(test)] mod tests`

No changes to `src/routes/media.rs`, `crate::jobs::media_phash`, `crates/mirror-client`, or any schema.

Reference (existing, mirror its shape): `crate::media_index::repo::videos_missing_canonical_phash` — same anti-join + `Option<&str>` cursor pattern.

---

## Task 1: `MEDIA_IMPORT_BATCH_SIZE` constant

**Files:**
- Modify: `src/consts.rs` (near `SCAN_PAGE_SIZE`, ~line 120)

- [ ] **Step 1: Add the constant**

Mirror `SCAN_PAGE_SIZE` exactly:
```rust
pub static MEDIA_IMPORT_BATCH_SIZE: Lazy<AtomicI64> = Lazy::new(|| {
    let val = std::env::var("MEDIA_IMPORT_BATCH_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(500);
    AtomicI64::new(val)
});
```

- [ ] **Step 2: Verify it compiles**

Run: `SDKROOT=$(xcrun --show-sdk-path) cargo build -p storj-interface --lib`
Expected: clean compile. (No unit test — it's an env-driven static, same as `SCAN_PAGE_SIZE` which is untested.)

- [ ] **Step 3: Commit**

```bash
git add src/consts.rs
git commit -m "feat: add MEDIA_IMPORT_BATCH_SIZE config (default 500)"
```

---

## Task 2: Paged skip-existing scan

**Files:**
- Modify: `src/jobs/media_imports.rs` (add fn near `fetch_legacy_rows`; add tests)

- [ ] **Step 1: Write the failing tests** (in `#[cfg(test)] mod tests`)

```rust
#[tokio::test]
async fn scan_returns_only_rows_missing_from_master() {
    let (_pg, client) = test_client().await;
    init_test_schema(&client).await;
    // three legacy rows
    for vid in ["v-a", "v-b", "v-c"] {
        client.execute(
            "INSERT INTO video_index (video_id, storj_key) VALUES ($1, $2)",
            &[&vid, &format!("creator/{vid}.mp4")],
        ).await.unwrap();
    }
    // v-b already in master
    let tx = client.transaction().await.unwrap();
    crate::media_index::upsert_servable_video_txn(&tx, servable_input("v-b")).await.unwrap();
    tx.commit().await.unwrap();

    let rows = super::fetch_missing_legacy_rows_after(&client, None, 100).await.unwrap();
    let ids: Vec<_> = rows.iter().map(|r| r.video_id.as_str()).collect();
    assert_eq!(ids, vec!["v-a", "v-c"], "v-b is in master, must be skipped");
}

#[tokio::test]
async fn scan_pages_by_video_id_cursor_and_respects_limit() {
    let (_pg, client) = test_client().await;
    init_test_schema(&client).await;
    for vid in ["v-a", "v-b", "v-c"] {
        client.execute(
            "INSERT INTO video_index (video_id, storj_key) VALUES ($1, $2)",
            &[&vid, &format!("creator/{vid}.mp4")],
        ).await.unwrap();
    }
    // limit 2, no cursor -> first two by video_id
    let page1 = super::fetch_missing_legacy_rows_after(&client, None, 2).await.unwrap();
    assert_eq!(page1.iter().map(|r| r.video_id.as_str()).collect::<Vec<_>>(), vec!["v-a", "v-b"]);
    // cursor past v-b -> only v-c
    let page2 = super::fetch_missing_legacy_rows_after(&client, Some("v-b"), 2).await.unwrap();
    assert_eq!(page2.iter().map(|r| r.video_id.as_str()).collect::<Vec<_>>(), vec!["v-c"]);
    // cursor past last -> empty
    let page3 = super::fetch_missing_legacy_rows_after(&client, Some("v-c"), 2).await.unwrap();
    assert!(page3.is_empty());
}
```

Add a small test helper if not present:
```rust
fn servable_input(video_id: &str) -> crate::media_index::ServableVideoInput<'_> {
    crate::media_index::ServableVideoInput {
        video_id, publisher_user_id: None, post_id: None,
        source_kind: "legacy_video_index", source_ref: Some(video_id),
        servable_status: "servable", nsfw_state: None,
        storage_provider: Some("storj"), bucket: None, object_key: Some(video_id),
        canonical_url: None, thumbnail_key: None, duration_ms: None, width: None,
        height: None, fps: None, container: None, video_codec: None, audio_codec: None,
        moov_atom_front: None, canonical_encoding_version: None, discovered_from: "test",
    }
}
```

- [ ] **Step 2: Run, verify fail**

Run: `cargo test -p storj-interface --lib media_imports::tests::scan_ -- --test-threads=1`
Expected: FAIL — `fetch_missing_legacy_rows_after` not defined.

- [ ] **Step 3: Implement the scan** (add to `media_imports.rs`)

```rust
/// Paged, skip-existing scan of legacy `video_index`. Returns up to `limit`
/// rows with `video_id > after` (or from the start when `after` is None) that
/// are NOT yet present in `all_servable_videos_on_yral`. Both columns are PKs,
/// so the anti-join + cursor range are index-driven. Mirrors the convention of
/// `media_index::videos_missing_canonical_phash`.
async fn fetch_missing_legacy_rows_after(
    client: &Client,
    after: Option<&str>,
    limit: i64,
) -> Result<Vec<LegacyVideoIndexRow>, tokio_postgres::Error> {
    let rows = client
        .query(
            "SELECT v.video_id, v.storj_key, v.hetzner_key, v.phash, v.phash_kind, v.phash_version
             FROM video_index v
             WHERE ($1::TEXT IS NULL OR v.video_id > $1)
               AND NOT EXISTS (
                 SELECT 1 FROM all_servable_videos_on_yral m WHERE m.video_id = v.video_id
               )
             ORDER BY v.video_id
             LIMIT $2",
            &[&after, &limit],
        )
        .await?;

    Ok(rows
        .into_iter()
        .map(|row| LegacyVideoIndexRow {
            video_id: row.get(0),
            storj_key: row.get(1),
            hetzner_key: row.get(2),
            phash: row.get(3),
            phash_kind: row.get(4),
            phash_version: row.get(5),
        })
        .collect())
}
```

- [ ] **Step 4: Run, verify pass**

Run: `cargo test -p storj-interface --lib media_imports::tests::scan_ -- --test-threads=1`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add src/jobs/media_imports.rs
git commit -m "feat: paged skip-existing scan for video_index import"
```

---

## Task 3: Extract `import_one_row_txn` (behavior-preserving refactor)

**Files:**
- Modify: `src/jobs/media_imports.rs`

Pull the per-row body (current lines ~97–184) into one function callable inside any transaction, returning per-row counter deltas. The existing `import_current_video_index_inner` keeps working by calling it — no behavior change yet.

- [ ] **Step 1: Add `RowCounts` + `import_one_row_txn`**

```rust
#[derive(Debug, Default, Clone, Copy)]
struct RowCounts {
    imported_media_rows: i64,
    hash_rows_upserted: i64,
    hash_feed_events_appended: i64,
    row_failures: i64,
}

/// Import a single legacy row within an existing transaction. Keyless rows are
/// recorded as a failure (NOT an error) and return `row_failures = 1`. Returns
/// the per-row counter deltas. Any returned `Err` is a real SQL error that
/// poisons `tx` and must trigger the caller's rollback + per-row fallback.
async fn import_one_row_txn(
    tx: &Transaction<'_>,
    row: &LegacyVideoIndexRow,
    job_run_id: Uuid,
) -> Result<RowCounts, tokio_postgres::Error> {
    let mut counts = RowCounts::default();

    let Some(storage) = canonical_storage(row) else {
        record_row_failure_txn(
            tx, job_run_id, &row.video_id, "storage_selection",
            "legacy video_index row has neither storj_key nor hetzner_key",
        ).await?;
        counts.row_failures = 1;
        return Ok(counts);
    };

    let media_outcome = import_media_row_txn(tx, row, storage).await?;
    if matches!(media_outcome.media,
        crate::media_index::UpsertOutcome::Inserted | crate::media_index::UpsertOutcome::Changed) {
        crate::media_index::append_feed_event_txn(tx, crate::media_index::FeedEventInput {
            event_kind: crate::media_index::FeedEventKind::MediaVisibilityChanged,
            video_id: &row.video_id, hash_kind: None, hash_version: None,
            input_media_version: None, payload: media_feed_payload(row, storage),
        }).await?;
        counts.imported_media_rows = 1;
    }

    if let (Some(phash), Some(phash_kind), Some(phash_version)) =
        (row.phash.as_deref(), row.phash_kind.as_deref(), row.phash_version.as_deref())
    {
        let provenance = hash_provenance(row);
        let outcome = crate::media_index::upsert_hash_record_txn(tx, crate::media_index::HashRecordInput {
            video_id: &row.video_id, hash_kind: phash_kind, hash_version: phash_version,
            input_media_version: INPUT_MEDIA_VERSION, hash_value: phash,
            hash_bit_length: phash.len() as i32, num_frames: 0, hash_size: 0,
            computed_from_provider: provenance.map(|s| s.provider),
            computed_from_bucket: provenance.and_then(|s| s.bucket),
            computed_from_key: provenance.map(|s| s.object_key),
            metadata: Some(json!({"source": SOURCE_KIND})),
        }).await?;

        if matches!(outcome,
            crate::media_index::UpsertOutcome::Inserted | crate::media_index::UpsertOutcome::Changed) {
            crate::media_index::append_feed_event_txn(tx, crate::media_index::FeedEventInput {
                event_kind: crate::media_index::FeedEventKind::HashUpserted,
                video_id: &row.video_id, hash_kind: Some(phash_kind),
                hash_version: Some(phash_version), input_media_version: Some(INPUT_MEDIA_VERSION),
                payload: json!({
                    "video_id": row.video_id, "hash_kind": phash_kind,
                    "hash_version": phash_version, "input_media_version": INPUT_MEDIA_VERSION,
                    "hash_value": phash, "source": SOURCE_KIND
                }),
            }).await?;
            counts.hash_rows_upserted = 1;
            counts.hash_feed_events_appended = 1;
        }
    }

    Ok(counts)
}
```

- [ ] **Step 2: Make the current loop delegate to it** (temporary — replaced in Task 4)

In `import_current_video_index_inner`, replace the inline per-row body (the `let Some(storage) = ...` block through the hash block, but NOT the `tx`/`commit`) with:
```rust
let counts = import_one_row_txn(&tx, &row, job_run_id).await?;
tx.commit().await?;
summary.imported_media_rows += counts.imported_media_rows;
summary.hash_rows_upserted += counts.hash_rows_upserted;
summary.hash_feed_events_appended += counts.hash_feed_events_appended;
summary.row_failures += counts.row_failures;
```
(Keep the `cancel` check, `scanned_rows += 1`, and `fetch_legacy_rows` call unchanged for now.)

- [ ] **Step 3: Run the existing suite — must stay green (behavior-preserving)**

Run: `cargo test -p storj-interface --lib media_imports -- --test-threads=1`
Expected: PASS — all existing tests (import, cancellation, idempotency, failures, scan from Task 2) unchanged.

- [ ] **Step 4: Commit**

```bash
git add src/jobs/media_imports.rs
git commit -m "refactor: extract import_one_row_txn for reuse in batched import"
```

---

## Task 4: Paged batched import loop with per-row fallback

**Files:**
- Modify: `src/jobs/media_imports.rs` (rewrite `import_current_video_index_inner`; delete `fetch_legacy_rows`)

- [ ] **Step 1: Write the failing tests** (in `#[cfg(test)] mod tests`)

```rust
// (a) full import works batched; counts + feed events correct (replaces/augments existing happy-path test)
// (b) per-row fallback isolation — handled structurally; assert keyless row in a batch is recorded
//     and the rest of the batch commits.
#[tokio::test]
async fn keyless_row_in_a_batch_is_recorded_and_rest_of_batch_commits() {
    let (_pg, mut client) = test_client().await;
    init_test_schema(&client).await;
    // one good, one keyless (no storj/hetzner), one good
    client.execute("INSERT INTO video_index (video_id, storj_key) VALUES ('g1','creator/g1.mp4')", &[]).await.unwrap();
    client.execute("INSERT INTO video_index (video_id) VALUES ('bad1')", &[]).await.unwrap();
    client.execute("INSERT INTO video_index (video_id, storj_key) VALUES ('g2','creator/g2.mp4')", &[]).await.unwrap();

    let cancel = tokio_util::sync::CancellationToken::new();
    let summary = super::import_current_video_index(&mut client, "t", None, &cancel).await.unwrap();

    assert_eq!(summary.scanned_rows, 3);
    assert_eq!(summary.imported_media_rows, 2);
    assert_eq!(summary.row_failures, 1);
    // good rows in master
    let n: i64 = client.query_one("SELECT count(*) FROM all_servable_videos_on_yral", &[]).await.unwrap().get(0);
    assert_eq!(n, 2);
    // failure recorded
    let f: i64 = client.query_one("SELECT count(*) FROM media_job_failures WHERE video_id='bad1'", &[]).await.unwrap().get(0);
    assert_eq!(f, 1);
}

// (c) forward progress over an all-keyless page — must terminate, not loop
#[tokio::test]
async fn import_makes_progress_over_keyless_rows_and_terminates() {
    use std::sync::atomic::Ordering;
    // Force tiny batches so multiple pages of keyless rows are exercised.
    // Swap+restore the prior value (don't hardcode the default).
    let prev_batch = crate::consts::MEDIA_IMPORT_BATCH_SIZE.swap(2, Ordering::Relaxed);
    let (_pg, mut client) = test_client().await;
    init_test_schema(&client).await;
    for vid in ["k1","k2","k3","k4","k5"] {
        client.execute("INSERT INTO video_index (video_id) VALUES ($1)", &[&vid]).await.unwrap();
    }
    let cancel = tokio_util::sync::CancellationToken::new();
    let summary = super::import_current_video_index(&mut client, "t", None, &cancel).await.unwrap();
    assert_eq!(summary.scanned_rows, 5);
    assert_eq!(summary.row_failures, 5);
    assert_eq!(summary.imported_media_rows, 0);
    let status: String = client.query_one(
        "SELECT status FROM media_job_runs WHERE id=$1::TEXT::UUID", &[&summary.job_run_id.to_string()],
    ).await.unwrap().get(0);
    assert_eq!(status, "succeeded_with_failures");
    crate::consts::MEDIA_IMPORT_BATCH_SIZE.store(prev_batch, Ordering::Relaxed);
}

// (d) resume after partial — second run imports only what's missing, no new feed events for done rows
#[tokio::test]
async fn second_run_imports_only_missing_rows() {
    let (_pg, mut client) = test_client().await;
    init_test_schema(&client).await;
    client.execute("INSERT INTO video_index (video_id, storj_key) VALUES ('r1','creator/r1.mp4')", &[]).await.unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    super::import_current_video_index(&mut client, "t", None, &cancel).await.unwrap();
    let events_after_first: i64 = client.query_one("SELECT count(*) FROM media_feed_events", &[]).await.unwrap().get(0);
    // add a new legacy row, re-run
    client.execute("INSERT INTO video_index (video_id, storj_key) VALUES ('r2','creator/r2.mp4')", &[]).await.unwrap();
    let summary = super::import_current_video_index(&mut client, "t", None, &cancel).await.unwrap();
    assert_eq!(summary.scanned_rows, 1, "only the new row r2 is scanned");
    assert_eq!(summary.imported_media_rows, 1);
    let events_after_second: i64 = client.query_one("SELECT count(*) FROM media_feed_events", &[]).await.unwrap().get(0);
    assert!(events_after_second > events_after_first, "only r2's events added; r1 not re-emitted");
}
```

Keep the existing `import_honors_cancellation_and_finalizes_cancelled` and idempotency tests — they must still pass.

- [ ] **Step 2: Run, verify the new tests fail**

Run: `cargo test -p storj-interface --lib media_imports -- --test-threads=1`
Expected: at least `second_run_imports_only_missing_rows` FAILS — the post-Task-3 loop still uses `fetch_legacy_rows` (no skip-existing), so the re-run re-scans the already-imported row and `scanned_rows == 1` does not hold. (The keyless/forward-progress test may already pass on counts before the rewrite; the resume test is the one that reliably drives this task.)

- [ ] **Step 3: Rewrite `import_current_video_index_inner`**

```rust
async fn import_current_video_index_inner(
    client: &mut Client,
    job_run_id: Uuid,
    limit: Option<i64>,
    cancel: &CancellationToken,
) -> Result<ImportSummary, ImportError> {
    let mut summary = ImportSummary {
        job_run_id, scanned_rows: 0, imported_media_rows: 0,
        hash_rows_upserted: 0, hash_feed_events_appended: 0, row_failures: 0,
    };
    let batch_size = crate::consts::MEDIA_IMPORT_BATCH_SIZE
        .load(std::sync::atomic::Ordering::Relaxed);
    let mut after: Option<String> = None;
    let mut cancelled = false;

    loop {
        if cancel.is_cancelled() { cancelled = true; break; }

        // global limit: cap rows fetched across the whole run
        let remaining = limit.map(|l| l - summary.scanned_rows);
        if remaining == Some(0) { break; }
        let page_limit = match remaining {
            Some(r) => r.min(batch_size),
            None => batch_size,
        };

        let page = fetch_missing_legacy_rows_after(client, after.as_deref(), page_limit).await?;
        if page.is_empty() { break; }
        after = page.last().map(|r| r.video_id.clone());

        // optimistic batch: one tx for the whole page
        let mut page_counts = RowCounts::default();
        let tx = client.transaction().await?;
        let mut batch_ok = true;
        for row in &page {
            match import_one_row_txn(&tx, row, job_run_id).await {
                Ok(c) => {
                    page_counts.imported_media_rows += c.imported_media_rows;
                    page_counts.hash_rows_upserted += c.hash_rows_upserted;
                    page_counts.hash_feed_events_appended += c.hash_feed_events_appended;
                    page_counts.row_failures += c.row_failures;
                }
                Err(_) => { batch_ok = false; break; }
            }
        }

        if batch_ok {
            tx.commit().await?;
            summary.imported_media_rows += page_counts.imported_media_rows;
            summary.hash_rows_upserted += page_counts.hash_rows_upserted;
            summary.hash_feed_events_appended += page_counts.hash_feed_events_appended;
            summary.row_failures += page_counts.row_failures;
        } else {
            // a real SQL error poisoned the batch tx; roll back and reprocess
            // this page row-by-row to isolate the offending row.
            drop(tx);
            for row in &page {
                let row_tx = client.transaction().await?;
                match import_one_row_txn(&row_tx, row, job_run_id).await {
                    Ok(c) => {
                        row_tx.commit().await?;
                        summary.imported_media_rows += c.imported_media_rows;
                        summary.hash_rows_upserted += c.hash_rows_upserted;
                        summary.hash_feed_events_appended += c.hash_feed_events_appended;
                        summary.row_failures += c.row_failures;
                    }
                    Err(e) => {
                        drop(row_tx);
                        let fail_tx = client.transaction().await?;
                        record_row_failure_txn(
                            &fail_tx, job_run_id, &row.video_id, "import_error", &e.to_string(),
                        ).await?;
                        fail_tx.commit().await?;
                        summary.row_failures += 1;
                    }
                }
            }
        }

        summary.scanned_rows += page.len() as i64;
    }

    let status = if cancelled {
        "cancelled"
    } else if summary.row_failures == 0 {
        "succeeded"
    } else {
        "succeeded_with_failures"
    };
    complete_job_run(client, &summary, status).await?;
    Ok(summary)
}
```

Then **delete `fetch_legacy_rows`** (now unused) to avoid a dead-code warning.

- [ ] **Step 4: Run the full media_imports suite**

Run: `cargo test -p storj-interface --lib media_imports -- --test-threads=1`
Expected: PASS — new tests (keyless-in-batch, forward-progress, resume) + existing (cancellation, idempotency, the original happy-path/feed/failure tests). If an existing test asserted an exact `scanned_rows` that included already-present rows, update it to the skip-existing semantics (scanned = missing rows processed).

- [ ] **Step 5: Commit**

```bash
git add src/jobs/media_imports.rs
git commit -m "feat: resumable batched video_index import (skip-existing + per-row fallback)"
```

---

## Task 5: Final verification

**Files:** none

- [ ] **Step 1: Format + lint**

Run: `cargo fmt && cargo clippy --all-targets --all-features 2>&1 | grep -A2 '^warning:' | grep -E '\--> src' | grep -v 'videogen/generate.rs\|videogen/complete.rs'`
Expected: empty (no new warnings beyond the known pre-existing videogen lints).

- [ ] **Step 2: Focused + regression suites**

Run:
```bash
cargo test -p storj-interface --lib media_imports -- --test-threads=1
cargo test media_index -- --test-threads=1
cargo build
```
Expected: all PASS, binary builds.

- [ ] **Step 3: Commit any fmt-only changes**

```bash
git add -A && git commit -m "style: cargo fmt" || true
```

---

## After implementation (operational — not a code task)

Ship this branch (PR + merge + deploy). The deploy will kill any in-flight old import; then re-run `mirror-client media-import` against prod — it resumes via skip-existing (cheap index sweep over the ~158k already done) and completes the remaining ~700k–1M in batched commits. Keep the prod deploy freeze only during the final run if desired; with resume, an interrupted run now continues cheaply.
