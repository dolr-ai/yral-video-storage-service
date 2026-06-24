# Media Jobs Observability + Smooth pHash Progress — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the media backfill observable over HTTP — runs (live progress), failures (grouped reasons) — and make pHash progress climb continuously, so "is it stuck?" is a glance, not a guess.

**Architecture:** Two read helpers in `media_index::repo` (recent runs, failure summary) exposed via two HMAC routes + `mirror-client` commands. Both jobs flush `totals` per page so a running job's row is live. `media_phash` switches from collect-whole-page-then-persist to persist-as-each-completes (with per-row cancel) and records a typed failure phase (`phash_download`/`phash_decode`).

**Tech Stack:** Rust, tokio-postgres, chrono, axum/utoipa, reqwest (client), testcontainers.

**Spec:** `docs/superpowers/specs/2026-06-24-media-jobs-observability-design.md`

---

## File Structure
- `src/media_index/repo.rs` — `recent_job_runs`, `failure_summary` (+ `JobRunRow`, `FailureGroup`), `update_job_run_totals`.
- `src/media_index/mod.rs` — re-exports.
- `src/jobs/media_imports.rs` — per-page `update_job_run_totals` flush.
- `src/jobs/media_phash.rs` — stream-persist + per-row cancel + per-page flush + typed phase split (`persist_one` error type changes).
- `src/routes/media.rs` — `runs` + `failures` handlers + response structs + tests.
- `src/main.rs` — 2 routes + ApiDoc.
- `crates/mirror-client/src/{lib,main}.rs` — `media-runs` + `media-failures`.

Patterns to mirror: `videos_missing_canonical_phash` (repo query + `Option<&str>` filter), `media_jobs_status`/`feed_events` (handlers, HMAC, `to_rfc3339()`), `media_audit()` (client GET).

---

## Task 1: Read helpers — recent runs + failure summary

**Files:** `src/media_index/repo.rs` (+ `mod.rs`), tests in `repo.rs`.

- [ ] **Step 1: Failing tests**
```rust
#[tokio::test]
async fn recent_job_runs_returns_newest_first_with_totals() {
    let (_pg, client) = test_client().await;
    crate::media_index::init_schema(&client).await.unwrap();
    for (i, kind) in ["legacy_video_index_import","media_phash","media_phash"].iter().enumerate() {
        client.execute(
            "INSERT INTO media_job_runs (id, job_kind, status, requested_by, started_at, totals)
             VALUES (gen_random_uuid(), $1, 'succeeded', 't', now() + ($2 || ' seconds')::interval, '{\"scanned_rows\":7}'::jsonb)",
            &[kind, &i.to_string()],
        ).await.unwrap();
    }
    let all = super::recent_job_runs(&client, None, 10).await.unwrap();
    assert_eq!(all.len(), 3);
    assert!(all[0].started_at >= all[1].started_at, "newest first");
    assert_eq!(all[0].totals.as_ref().unwrap()["scanned_rows"], 7);
    let phash = super::recent_job_runs(&client, Some("media_phash"), 10).await.unwrap();
    assert_eq!(phash.len(), 2);
}

#[tokio::test]
async fn failure_summary_groups_by_phase_with_distinct_samples() {
    let (_pg, client) = test_client().await;
    crate::media_index::init_schema(&client).await.unwrap();
    // 3 phash_download failures (2 distinct errors), 1 phash_decode
    let rows = [
        ("v1","phash_download","storj download a.mp4: 404"),
        ("v2","phash_download","storj download b.mp4: 404"),
        ("v3","phash_download","storj download a.mp4: 404"),
        ("v4","phash_decode","phash: no video stream"),
    ];
    for (vid, phase, err) in rows {
        client.execute(
            "INSERT INTO media_job_failures (job_kind, item_key, video_id, phase, last_error)
             VALUES ('media_phash', $1, $1, $2, $3)",
            &[&vid, &phase, &err],
        ).await.unwrap();
    }
    let groups = super::failure_summary(&client, Some("media_phash"), 10).await.unwrap();
    let dl = groups.iter().find(|g| g.phase == "phash_download").unwrap();
    assert_eq!(dl.count, 3);
    assert!(dl.samples.len() <= 5 && dl.samples.len() >= 2);
    // samples are distinct
    let mut s = dl.samples.clone(); s.sort(); s.dedup();
    assert_eq!(s.len(), dl.samples.len(), "samples must be distinct");
    assert!(groups.iter().any(|g| g.phase == "phash_decode" && g.count == 1));
}
```

- [ ] **Step 2: Run → fail** (`cargo test -p storj-interface --lib media_index::repo::tests::recent_job_runs -p storj-interface --lib`… simply `cargo test -p storj-interface --lib repo::tests::recent_job_runs repo::tests::failure_summary -- --test-threads=1`). Expected: undefined fns.

- [ ] **Step 3: Implement** (add to `repo.rs`; `chrono::{DateTime,Utc}` already used here):
```rust
#[derive(Debug, Clone)]
pub struct JobRunRow {
    pub job_kind: String,
    pub status: String,
    pub requested_by: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    pub totals: Option<serde_json::Value>,
    pub error_message: Option<String>,
}

pub async fn recent_job_runs(
    client: &Client,
    job_kind: Option<&str>,
    limit: i64,
) -> Result<Vec<JobRunRow>, tokio_postgres::Error> {
    let rows = client.query(
        "SELECT job_kind, status, requested_by, started_at, finished_at, totals, error_message
         FROM media_job_runs
         WHERE ($1::TEXT IS NULL OR job_kind = $1)
         ORDER BY started_at DESC
         LIMIT $2",
        &[&job_kind, &limit],
    ).await?;
    Ok(rows.into_iter().map(|r| JobRunRow {
        job_kind: r.get(0), status: r.get(1), requested_by: r.get(2),
        started_at: r.get(3), finished_at: r.get(4), totals: r.get(5), error_message: r.get(6),
    }).collect())
}

#[derive(Debug, Clone)]
pub struct FailureGroup { pub phase: String, pub count: i64, pub samples: Vec<String> }

pub async fn failure_summary(
    client: &Client,
    job_kind: Option<&str>,
    limit: i64,
) -> Result<Vec<FailureGroup>, tokio_postgres::Error> {
    // Query 1: counts per phase.
    let count_rows = client.query(
        "SELECT phase, COUNT(*) FROM media_job_failures
         WHERE ($1::TEXT IS NULL OR job_kind = $1)
         GROUP BY phase ORDER BY COUNT(*) DESC LIMIT $2",
        &[&job_kind, &limit],
    ).await?;
    let mut out = Vec::with_capacity(count_rows.len());
    for cr in count_rows {
        let phase: String = cr.get(0);
        let count: i64 = cr.get(1);
        // Query 2: recent rows for this phase; dedup to <=5 distinct in Rust
        // (avoids the invalid `SELECT DISTINCT ... ORDER BY created_at` form).
        let sample_rows = client.query(
            "SELECT left(last_error, 200) FROM media_job_failures
             WHERE phase = $1 AND ($2::TEXT IS NULL OR job_kind = $2)
             ORDER BY created_at DESC LIMIT 50",
            &[&phase, &job_kind],
        ).await?;
        let mut samples: Vec<String> = Vec::new();
        for sr in sample_rows {
            let s: String = sr.get(0);
            if !samples.contains(&s) { samples.push(s); }
            if samples.len() == 5 { break; }
        }
        out.push(FailureGroup { phase, count, samples });
    }
    Ok(out)
}
```
Re-export both fns + structs in `src/media_index/mod.rs`.

- [ ] **Step 4: Run → pass.** `cargo test -p storj-interface --lib repo::tests::recent_job_runs repo::tests::failure_summary -- --test-threads=1`

- [ ] **Step 5: Commit** — `feat: media_index read helpers for job runs + failure summary`

---

## Task 2: Live per-run progress flush

**Files:** `src/media_index/repo.rs` (helper), `src/jobs/media_imports.rs`, `src/jobs/media_phash.rs`.

- [ ] **Step 1: Failing test** (in `repo.rs`):
```rust
#[tokio::test]
async fn update_job_run_totals_reflects_progress_before_completion() {
    let (_pg, client) = test_client().await;
    crate::media_index::init_schema(&client).await.unwrap();
    let id = uuid::Uuid::new_v4();
    client.execute(
        "INSERT INTO media_job_runs (id, job_kind, status, requested_by) VALUES ($1::TEXT::UUID,'media_phash','running','t')",
        &[&id.to_string()],
    ).await.unwrap();
    super::update_job_run_totals(&client, id, &serde_json::json!({"scanned_rows": 42})).await.unwrap();
    let got: serde_json::Value = client.query_one(
        "SELECT totals FROM media_job_runs WHERE id=$1::TEXT::UUID", &[&id.to_string()],
    ).await.unwrap().get(0);
    assert_eq!(got["scanned_rows"], 42);
    // still running (not completed)
    let status: String = client.query_one("SELECT status FROM media_job_runs WHERE id=$1::TEXT::UUID", &[&id.to_string()]).await.unwrap().get(0);
    assert_eq!(status, "running");
}
```

- [ ] **Step 2: Run → fail.**

- [ ] **Step 3: Implement helper** (repo.rs) + re-export:
```rust
pub async fn update_job_run_totals(
    client: &Client,
    job_run_id: uuid::Uuid,
    totals: &serde_json::Value,
) -> Result<(), tokio_postgres::Error> {
    client.execute(
        "UPDATE media_job_runs SET totals = $2 WHERE id = $1::TEXT::UUID",
        &[&job_run_id.to_string(), totals],
    ).await?;
    Ok(())
}
```

- [ ] **Step 4: Wire per-page flush in both jobs.**
  - `media_imports.rs`: it already has `summary_totals(&ImportSummary) -> Value` (used by `complete_job_run`). After each page commits (end of the page loop body, before the next iteration), call `let _ = crate::media_index::update_job_run_totals(client, job_run_id, &summary_totals(&summary)).await;` (best-effort: a failed progress flush must not abort the import — use `let _ =`).
  - `media_phash.rs`: build the same totals JSON its `complete_job_run` uses (reuse/extract that serialization), and after each page's rows are persisted call `let _ = crate::media_index::update_job_run_totals(client, job_run_id, &<totals>).await;` (best-effort).

- [ ] **Step 5: Run** `cargo test -p storj-interface --lib repo::tests::update_job_run_totals media_imports media_phash -- --test-threads=1` → all pass (existing job tests unaffected; the flush is best-effort and additive).

- [ ] **Step 6: Commit** — `feat: flush live per-page totals to media_job_runs`

---

## Task 3: media_phash stream-persist + per-row cancel + typed phase split

**Files:** `src/jobs/media_phash.rs`.

- [ ] **Step 1: Failing test** — phase split (update the EXISTING failure test, ~`media_phash.rs:806`, plus add a download-phase test):
```rust
#[tokio::test]
async fn persist_records_decode_failures_under_phash_decode() {
    let (_pg, mut client) = test_client().await;
    init_test_schema(&client).await;
    let row = make_row("vid-x");
    let mut summary = PHashSummary { job_run_id: make_run_id(), scanned_rows:0, hash_rows_upserted:0, hash_feed_events_appended:0, row_failures:0 };
    insert_job_run(&client, summary.job_run_id, "t").await.unwrap();
    super::persist_one(&mut client, summary.job_run_id, &row,
        Err(("phash_decode", "phash: no video stream".to_string())), &mut summary).await.unwrap();
    let phase: String = client.query_one(
        "SELECT phase FROM media_job_failures WHERE video_id='vid-x'", &[]).await.unwrap().get(0);
    assert_eq!(phase, "phash_decode");
}
```
(Also update any existing test that constructed `Err("...".to_string())` / asserted `phase == "phash_compute"` to the new `(phase, msg)` tuple + new phase.)

- [ ] **Step 2: Run → fail** (type mismatch: `persist_one` takes `Result<_, String>`).

- [ ] **Step 3: Implement.**
  - Change `persist_one`'s `hash_result` param to `Result<VideoHashResult, (&'static str, String)>`. In the `Err((phase, err)) =>` arm, replace the hardcoded `"phash_compute"` with the passed `phase`:
    ```rust
    Err((phase, err)) => {
        tracing::error!(video_id = %row.video_id, phase, error = %err, "media_phash: row failed");
        record_row_failure_txn(&tx, job_run_id, &row.video_id, phase, &err).await?;
        tx.commit().await?;
        summary.row_failures += 1;
    }
    ```
  - In the download-and-hash closure, make each error carry its phase: tempfile/clone/download → `"phash_download"`; `spawn_blocking` panic + `phash:` decode → `"phash_decode"`. i.e. the inner async returns `Result<VideoHashResult, (&'static str, String)>`:
    ```rust
    let tmp = NamedTempFile::new().map_err(|e| ("phash_download", format!("tempfile: {e}")))?;
    // ... try_clone => ("phash_download", format!("file clone: {e}"))
    // ... downloads => ("phash_download", format!("hetzner download {key}: {e}")) / storj / missing object_key / unknown provider
    // ... spawn_blocking => ("phash_decode", format!("spawn_blocking panic: {e}"))
    // ... .and_then(|r| r.map_err(|e| ("phash_decode", format!("phash: {e}"))))
    ```
  - Replace the collect-then-persist block with stream-persist + per-row cancel:
    ```rust
    let mut stream = futures::stream::iter(rows)
        .map(|row| { /* unchanged download+hash closure, now returning (phase,err) */ })
        .buffer_unordered(concurrency);
    while let Some((row, hash_result)) = stream.next().await {
        if cancel.is_cancelled() { cancelled = true; break; }
        summary.scanned_rows += 1;
        persist_one(client, job_run_id, &row, hash_result, &mut summary).await?;
        crate::jobs::log_progress(summary.scanned_rows as usize, JOB_KIND);
    }
    drop(stream);
    if cancelled { break; }
    // per-page totals flush (Task 2) goes here
    ```
    (`futures::StreamExt` is already imported.)

- [ ] **Step 4: Run** `cargo test -p storj-interface --lib media_phash -- --test-threads=1` → all pass (existing persist/cancel/idempotency tests + new phase tests).

- [ ] **Step 5: Commit** — `feat: media_phash stream-persist, per-row cancel, typed failure phase`

---

## Task 4: Routes — /media/jobs/runs + /media/jobs/failures

**Files:** `src/routes/media.rs`, `src/main.rs`.

- [ ] **Step 1: Failing test** (in `media.rs` tests — exercise the response mapping via a pure helper like the existing `media_jobs_status_body`, or test the repo call + JSON shape). Minimal: assert the handler maps `JobRunRow`→`JobRunView` with RFC3339 timestamps. Prefer a small pure mapper `job_run_view(JobRunRow) -> JobRunView` unit-tested without HTTP.
```rust
#[test]
fn job_run_view_serializes_timestamps_rfc3339() {
    let row = crate::media_index::JobRunRow {
        job_kind: "media_phash".into(), status: "running".into(), requested_by: "t".into(),
        started_at: chrono::Utc::now(), finished_at: None,
        totals: Some(serde_json::json!({"scanned_rows": 9})), error_message: None,
    };
    let v = super::job_run_view(row);
    assert_eq!(v.status, "running");
    assert!(v.started_at.contains('T')); // rfc3339
    assert!(v.finished_at.is_none());
}
```

- [ ] **Step 2: Run → fail.**

- [ ] **Step 3: Implement** structs + mapper + handlers:
```rust
#[derive(Serialize, ToSchema)]
pub struct JobRunView {
    pub job_kind: String, pub status: String, pub requested_by: String,
    pub started_at: String, pub finished_at: Option<String>,
    pub totals: Option<serde_json::Value>, pub error_message: Option<String>,
}
#[derive(Serialize, ToSchema)]
pub struct JobRunsResponse { pub runs: Vec<JobRunView> }
#[derive(Serialize, ToSchema)]
pub struct FailureGroupView { pub phase: String, pub count: i64, pub samples: Vec<String> }
#[derive(Serialize, ToSchema)]
pub struct FailuresResponse { pub failures: Vec<FailureGroupView> }

pub fn job_run_view(r: crate::media_index::JobRunRow) -> JobRunView {
    JobRunView {
        job_kind: r.job_kind, status: r.status, requested_by: r.requested_by,
        started_at: r.started_at.to_rfc3339(),
        finished_at: r.finished_at.map(|t| t.to_rfc3339()),
        totals: r.totals, error_message: r.error_message,
    }
}
```
- `media_jobs_runs(State, Query{job_kind:Option<String>, limit:Option<i64>})` — `let limit = params.limit.unwrap_or(20).clamp(1,100);` connect, `recent_job_runs(&client, params.job_kind.as_deref(), limit)`, map, `Json(JobRunsResponse{runs})`. `#[utoipa::path(get, path="/media/jobs/runs", tag="media", ...)]` 200/401/500.
- `media_jobs_failures(State, Query{job_kind, limit})` — same shape; `failure_summary(...)` → `FailuresResponse`. Path `/media/jobs/failures`.
- Error mapping: `map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)`.
- `main.rs`: register both routes with `.with_state(...).layer(middleware::from_fn(authorize))`; add the 2 paths + 4 schemas to `ApiDoc`.

- [ ] **Step 4: Run** `cargo build && cargo test -p storj-interface --lib routes::media -- --test-threads=1`.

- [ ] **Step 5: Commit** — `feat: /media/jobs/runs and /media/jobs/failures endpoints`

---

## Task 5: mirror-client commands

**Files:** `crates/mirror-client/src/lib.rs`, `src/main.rs`.

- [ ] **Step 1: Failing test** — arg parsing for `--job-kind` (mirror `parse_after`):
```rust
#[test]
fn parses_job_kind_flag() {
    let args = vec!["bin".into(),"media-runs".into(),"--job-kind".into(),"media_phash".into()];
    assert_eq!(parse_job_kind(&args), Some("media_phash".to_string()));
}
```
- [ ] **Step 2: Run → fail.**
- [ ] **Step 3: Implement.**
  - lib: `JobRunView`/`JobRunsResponse`/`FailureGroupView`/`FailuresResponse` (`Deserialize`), methods `media_runs(job_kind: Option<&str>, limit: Option<u64>)` + `media_failures(job_kind, limit)` — signed GETs modeled on `media_audit()`, appending `?job_kind=&limit=` query params.
  - bin: `parse_job_kind` helper; `media-runs`/`media-failures` dispatch arms (print runs as `job_kind status scanned/failed started_at`; failures as `phase  count` + indented samples); USAGE entries.
- [ ] **Step 4: Run** `cargo test -p mirror-client && cargo build -p mirror-client`.
- [ ] **Step 5: Commit** — `feat: mirror-client media-runs + media-failures`

---

## Task 6: Final verification

- [ ] `cargo fmt && cargo clippy --all-targets --all-features 2>&1 | grep -A2 '^warning:' | grep -E '\--> src' | grep -vE 'videogen/generate.rs|videogen/complete.rs'` → empty.
- [ ] `cargo test -p storj-interface --lib media_index media_imports media_phash routes::media -- --test-threads=1` → pass.
- [ ] `cargo test -p mirror-client` → pass.
- [ ] `cargo build` → clean.
- [ ] Commit any fmt-only changes.

---

## After implementation (operational)
Ship the branch (PR → merge → deploy). The deploy interrupts the running pHash backfill (resets in-memory config); re-run `media-phash` after, now with `media-runs` (live progress/ETA + stuck detection) and `media-failures` (grouped reasons) for monitoring, at a CPU-safe `config-set --phash N`.
