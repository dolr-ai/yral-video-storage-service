# Sharded Distributed pHash Backfill — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run the pHash backfill across all 3 prakash boxes, partitioned by a shard predicate so they do disjoint work, at concurrency 4 — triggered by an SSH fleet script, monitored via the (same-PR) observability endpoints.

**Architecture:** Add an optional `shard (of, idx)` filter to the missing-hash scan (`((hashtext(video_id)::bigint % of) + of) % of = idx` — total + uniform). Thread it through `media_phash::run` and the `POST /media/phash/run` route (`?shard&of`). Deploy the app on all 3 boxes (`RUN_APP` + creds + `PHASH_CONCURRENCY=4`). A `phash-fleet.sh` SSHes to chosen boxes and triggers one shard each.

**Tech Stack:** Rust, tokio-postgres, axum/utoipa, reqwest (client), bash/SSH, GitHub Actions deploy.

**Spec:** `docs/superpowers/specs/2026-06-24-sharded-phash-backfill-design.md`. **Branch:** `prakash/media-jobs-observability` (same PR as observability).

---

## Task 1: Shard predicate in the missing-hash scan

**Files:** `src/media_index/repo.rs` (fn + its in-file test callers).

- [ ] **Step 1: Failing test** (in `repo.rs` tests) — disjoint + total partition, incl. negative-hash rows:
```rust
#[tokio::test]
async fn missing_phash_scan_shards_are_disjoint_and_total() {
    let (_pg, client) = test_client().await;
    crate::media_index::init_schema(&client).await.unwrap();
    // Seed 30 servable rows, none hashed. video_ids chosen to spread across buckets;
    // include ones whose hashtext is negative (plain ints stringified is fine — hashtext varies in sign).
    for i in 0..30u32 {
        let vid = format!("shard-vid-{i:04}");
        let tx = client.transaction().await.unwrap();
        crate::media_index::upsert_servable_video_txn(&tx, servable_input(&vid)).await.unwrap();
        tx.commit().await.unwrap();
    }
    let of = 3i64;
    let mut all = std::collections::BTreeSet::new();
    let mut total = 0;
    for idx in 0..of {
        let rows = super::videos_missing_canonical_phash(
            &client, "phash", "offchain_binary_10x8_v1", "current_stored_object_v1",
            None, None, Some((of, idx)),
        ).await.unwrap();
        for r in &rows {
            assert!(all.insert(r.video_id.clone()), "video {} appeared in two shards", r.video_id);
        }
        total += rows.len();
    }
    assert_eq!(total, 30, "every row lands in exactly one shard (disjoint + total)");
}
```
`servable_input` does NOT exist yet — add this helper to the `repo.rs` tests module (builds a minimal `ServableVideoInput`; all 22 fields, storj-backed):
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

- [ ] **Step 2: Run → fail** — `cargo test -p storj-interface --lib repo::tests::missing_phash_scan_shards -- --test-threads=1`. Expected: arity error (fn takes 6 args).

- [ ] **Step 3: Implement.** Add `shard: Option<(i64, i64)>` as the last param. Bind `of`/`idx` as `Option<i64>` ($5/$6) with a NULL-guard clause (mirrors the `after` pattern — no dynamic SQL for the shard); keep `LIMIT` as the conditional `$7` append:
```rust
pub async fn videos_missing_canonical_phash(
    client: &Client,
    hash_kind: &str,
    hash_version: &str,
    input_media_version: &str,
    after: Option<&str>,
    limit: Option<i64>,
    shard: Option<(i64, i64)>,
) -> Result<Vec<MissingHashRow>, tokio_postgres::Error> {
    let (shard_of, shard_idx): (Option<i64>, Option<i64>) = match shard {
        Some((of, idx)) => (Some(of), Some(idx)),
        None => (None, None),
    };
    let base = "SELECT v.video_id, v.storage_provider, v.object_key, v.servable_status, v.bucket
                FROM all_servable_videos_on_yral v
                LEFT JOIN servable_video_hashes h
                   ON h.video_id = v.video_id
                  AND h.hash_kind = $1 AND h.hash_version = $2 AND h.input_media_version = $3
                WHERE h.video_id IS NULL
                  AND ($4::TEXT IS NULL OR v.video_id > $4)
                  AND ($5::BIGINT IS NULL
                       OR (((hashtext(v.video_id)::bigint % $5) + $5) % $5) = $6)
                ORDER BY v.video_id";
    let rows = if let Some(limit) = limit {
        client.query(&format!("{base} LIMIT $7"),
            &[&hash_kind, &hash_version, &input_media_version, &after, &shard_of, &shard_idx, &limit]).await?
    } else {
        client.query(base,
            &[&hash_kind, &hash_version, &input_media_version, &after, &shard_of, &shard_idx]).await?
    };
    Ok(rows.into_iter().map(|row| MissingHashRow {
        video_id: row.get(0), storage_provider: row.get(1), object_key: row.get(2),
        servable_status: row.get(3), bucket: row.get(4),
    }).collect())
}
```
Then fix the **6 existing callers** so it compiles: the 1 prod caller in `media_phash.rs::run_inner` (pass `None` for now — Task 2 wires the real value) and the **5 test callers** in `media_phash.rs` (lines ~882/900/981/996/1010 — add `, None`).

- [ ] **Step 4: Run → pass** — `cargo test -p storj-interface --lib repo::tests::missing_phash_scan_shards media_phash -- --test-threads=1` (the shard test + all media_phash tests).

- [ ] **Step 5: Commit** — `feat: shard predicate on missing-phash scan (total hashtext modulo)`

---

## Task 2: Thread shard through media_phash run

**Files:** `src/jobs/media_phash.rs`.

- [ ] **Step 1: Implement** (no new behavior test — the shard SQL is tested in Task 1; this is plumbing through the untested orchestration shell):
  - `run(s3, storj, db_url, cancel, limit, requested_by, shard: Option<(i64,i64)>)` — add `shard` as the last param; pass to `run_inner`.
  - `run_inner(..., limit, shard: Option<(i64,i64)>)` — add param; pass it to the `videos_missing_canonical_phash(..., after, batch_limit, shard)` call (replace the `None` left in Task 1).
  - Update the `run` caller in `src/routes/media.rs` (the `run_phash` handler's `tokio::spawn`) — for now pass `None`; Task 3 wires the real shard. (Keep it compiling.)
- [ ] **Step 2: Build + tests** — `SDKROOT=$(xcrun --show-sdk-path) cargo build -p storj-interface --lib && cargo test -p storj-interface --lib media_phash -- --test-threads=1`.
- [ ] **Step 3: Commit** — `feat: thread shard param through media_phash run`

---

## Task 3: Route `?shard&of` + client `--shard/--of`

**Files:** `src/routes/media.rs`, `crates/mirror-client/src/{lib,main}.rs`.

- [ ] **Step 1: Failing test** (route validation — pure helper). Add a `parse_shard(shard: Option<i64>, of: Option<i64>) -> Result<Option<(i64,i64)>, &'static str>` in `media.rs` and test it:
```rust
#[test]
fn parse_shard_validates() {
    assert_eq!(super::parse_shard(None, None), Ok(None));
    assert_eq!(super::parse_shard(Some(0), Some(3)), Ok(Some((3,0))));   // (of, idx)
    assert!(super::parse_shard(Some(1), None).is_err());                  // only one provided
    assert!(super::parse_shard(Some(3), Some(3)).is_err());               // shard >= of
    assert!(super::parse_shard(Some(0), Some(0)).is_err());               // of < 1
    assert!(super::parse_shard(Some(-1), Some(3)).is_err());              // shard < 0
}
```
- [ ] **Step 2: Run → fail.**
- [ ] **Step 3: Implement.**
  - `PhashParams`: add `pub shard: Option<i64>`, `pub of: Option<i64>`.
  - `parse_shard(shard, of)`: `None,None → Ok(None)`; both Some with `of>=1 && 0<=shard<of → Ok(Some((of,shard)))`; else `Err`. (Returns `(of, idx)` to match the repo param order.)
  - `run_phash` handler: call `parse_shard(params.shard, params.of)`; on `Err` return `StatusCode::BAD_REQUEST` BEFORE the running-flag CAS; pass the `Option<(i64,i64)>` into `media_phash::run(...)`. Add `shard`/`of` to the `#[utoipa::path]` `params`.
  - client lib: change `media_phash(&self, limit, shard: Option<u64>, of: Option<u64>)` — build a signed POST to `/media/phash/run` appending `?limit=&shard=&of=` (don't reuse `post_job` since it doesn't take shard/of; write a focused 202/409 POST mirroring `post_job`'s status handling, or extend `post_job` with extra query pairs).
  - client bin: `parse_of` (mirror `parse_limit`); `media-phash` arm reads `--shard`/`--of` and passes through; USAGE updated.
- [ ] **Step 4: Build + tests** — `SDKROOT=$(xcrun --show-sdk-path) cargo build && cargo test routes::media -- --test-threads=1 && cargo test -p mirror-client`.
- [ ] **Step 5: Commit** — `feat: media-phash shard/of params (route + client)`

---

## Task 4: Deploy the app on all 3 boxes

**Files:** `.github/workflows/deploy-prakash-servers.yml`.

- [ ] **Step 1: Edit the deploy step.**
  - Remove the `RUN_APP` gate so it's `true` for all nodes (currently `[[ "${NODE_NAME}" == "server_1" ]] && RUN_APP=true`). Set `RUN_APP=true` for server_1/2/3.
  - The `APP_VARS` block is built `if [[ "${RUN_APP}" == "true" ]]` — with RUN_APP true everywhere it now applies to all nodes. **Add `export PHASH_CONCURRENCY='4';`** to that `APP_VARS` block (mandatory — default is 10).
  - Do NOT add `DATABASE_URL` (comes from `docker-compose.ha.yml` via the always-exported `APP_DB_PASSWORD`).
  - Update the `deployment-summary` step's `**App:** server_1 only` line → `**App:** all nodes (server_1 public; server_2/3 internal for backfill)`.
  - Leave the health check on server_1 (public). Optionally add a localhost health check per node.

- [ ] **Step 2: Self-review the diff** — confirm: RUN_APP true for all 3; APP_VARS (incl. PHASH_CONCURRENCY=4) exported for all 3; no `DATABASE_URL` added; `HOST_PORT=3005` unchanged; server_1 public behavior unchanged. (No automated test for the workflow; correctness is by review + the deploy's own health check + the post-deploy write check below.)

- [ ] **Step 3: Commit** — `ci: run storj-interface app on all 3 prakash nodes (PHASH_CONCURRENCY=4) for sharded backfill`

---

## Task 5: Fleet trigger script

**Files:** `scripts/phash-fleet.sh` (new, `chmod +x`).

- [ ] **Step 1: Write the script** — per spec §4. Takes a server→shard map + `--of N` (or positional `ip=idx` pairs). For each server: SSH (deploy key) → inline HMAC-sign → signed `GET /media/jobs/status` (skip if `phash_running`) → signed `POST /media/phash/run?shard=<idx>&of=<N>&requested_by=phash-shard-<idx>-of-<N>` to `localhost:3005`. Echo each server's result. `set -euo pipefail`; require `SERVICE_SECRET_TOKEN` env; default SSH key `~/.ssh/yral_onboarding_deploy` (overridable). Enforce one `--of` across all entries.
- [ ] **Step 2: Lint** — `bash -n scripts/phash-fleet.sh` (syntax) + `shellcheck scripts/phash-fleet.sh` if available. No live run in CI (SSH/secrets). Include a header comment with a usage example + a "dry-run" note (`echo` the curl instead of running, via a `DRY_RUN=1` guard).
- [ ] **Step 3: Commit** — `feat: phash-fleet.sh — SSH-triggered sharded backfill across boxes`

---

## Task 6: README + final verification

**Files:** `crates/mirror-client/README.md`.

- [ ] **Step 1: README** — add `--shard i --of n` to the `media-phash` row/options; add a short "Distributed backfill" subsection pointing at `scripts/phash-fleet.sh` (one command to trigger the fleet; monitor via `media-audit`/`media-runs`).
- [ ] **Step 2: Final verification:**
```bash
cargo fmt --check
SDKROOT=$(xcrun --show-sdk-path) cargo clippy --all-targets --all-features 2>&1 | grep -A2 '^warning:' | grep -E '\--> (src|crates)' | grep -vE 'videogen/generate.rs|videogen/complete.rs'   # expect empty
cargo test -p storj-interface --lib media_index media_phash -- --test-threads=1
cargo test routes::media -- --test-threads=1
cargo test -p mirror-client
SDKROOT=$(xcrun --show-sdk-path) cargo build
```
- [ ] **Step 3: Commit** any fmt-only changes.

---

## After merge (operational)
1. Deploy (one deploy for observability + sharding). App now on all 3 boxes at `PHASH_CONCURRENCY=4`.
2. **Post-deploy write check:** on each box, signed `media-status` on `localhost:3005` returns 200, and a tiny `media-phash --limit 1 --shard <i> --of 3` writes a row (confirms each box reaches the primary). 
3. Launch: `SERVICE_SECRET_TOKEN=... scripts/phash-fleet.sh <server-map> --of 3`. Monitor: `media-audit` (coverage), `media-runs` (per-shard live totals via the shard-labeled `requested_by`), `media-failures` (grouped reasons).
