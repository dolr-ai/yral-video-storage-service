# Backfill Thumbnails — Production Execution Workflow

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to execute this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run the `backfill-thumbnails` operator against all production buckets to generate `<video_id>-thumbnail.png` first-frame staged thumbnails for historical videos, with audit/verify gates between every phase.

**Architecture:** Five scopes are processed in order: test-sfw-storj → test-sfw-hetzner → prod-sfw-storj → prod-sfw-hetzner → prod-nsfw-storj. Each scope requires an audit gate before execution and a verify gate after. Production scopes require explicit human approval before any write. The `--execute` flag is the only write trigger; omitting it is always a safe dry-run.

**Tech Stack:** `cargo run -p backfill-thumbnails`, `uplink` CLI (Storj), Hetzner S3 client. All artifacts land under `artifacts/backfill/`.

> **Naming note:** Staged thumbnails are `<video_id>-thumbnail.png` (dash, no `bak`). Existing production thumbnails are `<video_id>_thumbnail.png` (underscore). These are distinct keys; the tool never touches the underscore variant.

> **STOP rule:** Do not execute any production scope until Task 5 (approval gate) is explicitly cleared.

---

## Scope Reference

| Scope | Backend | Bucket env / flag | Approval required |
|-------|---------|-------------------|-------------------|
| `test-sfw-storj` | Storj | `--bucket <test-bucket>` | No |
| `test-sfw-hetzner` | Hetzner | `--bucket <test-bucket>` | No |
| `prod-sfw-storj` | Storj | resolved from env | **Yes — Saikat** |
| `prod-sfw-hetzner` | Hetzner | resolved from env | **Yes — Saikat** |
| `prod-nsfw-storj` | Storj | resolved from env | **Yes — Saikat** |

---

## Rollback Strategy

The tool never deletes or overwrites objects. "Rollback" means staged `-thumbnail.png` files exist but are ignored by the read path until explicitly wired up.

If staged files must be removed after a bad run:

```sh
# List staged files created by this run (Storj example):
uplink ls -r --access $STORJ_ACCESS_GRANT_SFW sj://your-bucket/ | grep -- "-thumbnail.png"

# Delete individually — never use wildcards on production buckets without approval
uplink rm --access $STORJ_ACCESS_GRANT_SFW sj://your-bucket/<video_id>-thumbnail.png
```

The `artifacts/backfill/<scope>/<bucket>/manifest.jsonl` is the audit trail. Keep it even if files are removed.

---

## Task 1: Complete Test Validation — `test-sfw-storj`

**Files touched (local only):**
- `artifacts/backfill/test-sfw-storj/<bucket>/manifest.jsonl`
- `artifacts/backfill/test-sfw-storj/<bucket>/runs/<run-id>-*/`

- [x] **Step 1: Seed test data (if bucket is empty)**

  Bucket `test-duplicate` was pre-populated. No seed needed.

- [x] **Step 2: Audit — confirm candidate count and 0 remaining**

  Candidate videos before cutoff: **9**. Existing staged: **0**. Remaining: **9**.

- [x] **Step 3: Dry-run — preview what will be processed**

  Run ID: `20260424T080041Z`. Mode: dry-run. Dry-run candidates: **9**. No uploads.

- [x] **Step 4: Execute — generate staged thumbnails**

  Run ID: `20260424T082322Z`. Completed: **9**. Failed: **0**.

- [x] **Step 5: Verify — confirm staged thumbnails match first frames**

  Run ID: `20260424T082554Z`. PASS: **9**. FAIL: **0**. SKIP: **0**.

- [x] **Step 6: Final audit — confirm 0 remaining**

  Run ID: `20260424T082714Z`. Existing staged: **9**. Remaining: **0**. ✓

- [x] **Step 7: Commit artifacts and mark task done**

  Artifacts committed. Evidence: manifest at `artifacts/backfill/test-sfw-storj/test-duplicate/manifest.jsonl`, runs under `artifacts/backfill/test-sfw-storj/test-duplicate/runs/`.

---

## Task 2: Test Validation — `test-sfw-hetzner`

> **DEFERRED** — Hetzner test validation will be done in a separate pass after all Storj scopes are complete.

> If Hetzner test bucket access is not yet available, skip to Task 5 and mark that SFW production sign-off is **blocked on Hetzner test**.

- [ ] **Step 1: Seed test data**

```sh
cargo run -p backfill-thumbnails -- \
  seed-test-data \
  --scope test-sfw-hetzner \
  --bucket <your-hetzner-test-bucket> \
  --manifest-dir artifacts/backfill
```

- [ ] **Step 2: Audit**

```sh
cargo run -p backfill-thumbnails -- \
  audit \
  --scope test-sfw-hetzner \
  --bucket <your-hetzner-test-bucket> \
  --cutoff-before 2026-04-25T03:31:00Z \
  --manifest-dir artifacts/backfill
```

Record candidate count: `______`

- [ ] **Step 3: Dry-run**

```sh
cargo run -p backfill-thumbnails -- \
  run \
  --scope test-sfw-hetzner \
  --bucket <your-hetzner-test-bucket> \
  --cutoff-before 2026-04-25T03:31:00Z \
  --manifest-dir artifacts/backfill
```

- [ ] **Step 4: Execute**

```sh
cargo run -p backfill-thumbnails -- \
  run \
  --scope test-sfw-hetzner \
  --bucket <your-hetzner-test-bucket> \
  --cutoff-before 2026-04-25T03:31:00Z \
  --manifest-dir artifacts/backfill \
  --execute
```

Expected: `Completed` equals candidate count. `Failed: 0`.

- [ ] **Step 5: Verify**

```sh
cargo run -p backfill-thumbnails -- \
  verify \
  --scope test-sfw-hetzner \
  --bucket <your-hetzner-test-bucket> \
  --cutoff-before 2026-04-25T03:31:00Z \
  --manifest-dir artifacts/backfill \
  --verify-sample 10
```

Expected: `FAIL: 0`.

- [ ] **Step 6: Final audit — confirm 0 remaining**

```sh
cargo run -p backfill-thumbnails -- \
  audit \
  --scope test-sfw-hetzner \
  --bucket <your-hetzner-test-bucket> \
  --cutoff-before 2026-04-25T03:31:00Z \
  --manifest-dir artifacts/backfill
```

Expected: `Remaining videos to backfill: 0`.

- [ ] **Step 7: Commit**

```sh
git add artifacts/backfill/test-sfw-hetzner/
git commit -m "chore: test-sfw-hetzner backfill complete — <N> videos staged"
```

---

## Task 3: Size Production Work With Audits (Read-Only)

> Safe to run any time. No writes.

- [ ] **Step 1: Audit prod-sfw-storj**

```sh
cargo run -p backfill-thumbnails -- \
  audit \
  --scope prod-sfw-storj \
  --cutoff-before <CONFIRM-EXACT-PRODUCTION-CUTOFF> \
  --manifest-dir artifacts/backfill
```

Record: Total objects, Candidate videos, Remaining. Note if any `-thumbnail.png` already exist.

- [ ] **Step 2: Audit prod-nsfw-storj**

```sh
cargo run -p backfill-thumbnails -- \
  audit \
  --scope prod-nsfw-storj \
  --cutoff-before <CONFIRM-EXACT-PRODUCTION-CUTOFF> \
  --manifest-dir artifacts/backfill
```

- [ ] **Step 3: Audit prod-sfw-hetzner**

```sh
cargo run -p backfill-thumbnails -- \
  audit \
  --scope prod-sfw-hetzner \
  --cutoff-before <CONFIRM-EXACT-PRODUCTION-CUTOFF> \
  --manifest-dir artifacts/backfill
```

- [ ] **Step 4: Record totals for approval discussion**

| Scope | Candidates | Already staged | Remaining |
|-------|-----------|----------------|-----------|
| prod-sfw-storj | | | |
| prod-sfw-hetzner | | | |
| prod-nsfw-storj | | | |

---

## Task 4: Production Dry-Runs (Read-Only)

> Safe to run any time. Dry-run never writes.

- [ ] **Step 1: Dry-run prod-sfw-storj**

```sh
cargo run -p backfill-thumbnails -- \
  run \
  --scope prod-sfw-storj \
  --cutoff-before <CONFIRM-EXACT-PRODUCTION-CUTOFF> \
  --manifest-dir artifacts/backfill
```

Review `Planned process`, `Skip remote exists`, `Skip manifest done`.

- [ ] **Step 2: Dry-run prod-nsfw-storj**

```sh
cargo run -p backfill-thumbnails -- \
  run \
  --scope prod-nsfw-storj \
  --cutoff-before <CONFIRM-EXACT-PRODUCTION-CUTOFF> \
  --manifest-dir artifacts/backfill
```

- [ ] **Step 3: Dry-run prod-sfw-hetzner**

```sh
cargo run -p backfill-thumbnails -- \
  run \
  --scope prod-sfw-hetzner \
  --cutoff-before <CONFIRM-EXACT-PRODUCTION-CUTOFF> \
  --manifest-dir artifacts/backfill
```

- [ ] **Step 4: Commit dry-run summaries**

```sh
git add artifacts/backfill/prod-*/
git commit -m "chore: production backfill dry-run summaries"
```

---

## Task 5: Production Approval Gate

> **STOP. Do not proceed to Task 6 without explicit approval from Saikat.**

- [ ] Share Task 3 audit table (candidate counts, remaining per scope)
- [ ] Share Task 1 and Task 2 verify outputs (`PASS: N, FAIL: 0`)
- [ ] Share dry-run planned process counts from Task 4
- [ ] Confirm the exact production cutoff timestamp (replace all `<CONFIRM-EXACT-PRODUCTION-CUTOFF>` placeholders)
- [ ] Confirm the rollout order (default: prod-sfw-storj → prod-sfw-hetzner → prod-nsfw-storj)
- [ ] Get explicit written sign-off

Record approval here:
```
Approved by: ___________
Date/time:   ___________
Cutoff used: ___________
Scopes approved: ___________
```

---

## Task 6: Execute `prod-sfw-storj`

- [ ] **Step 1: Execute (full bucket, or by prefix if count is very large)**

Full bucket:
```sh
cargo run -p backfill-thumbnails -- \
  run \
  --scope prod-sfw-storj \
  --cutoff-before <APPROVED-CUTOFF> \
  --manifest-dir artifacts/backfill \
  --execute
```

Or scoped to a publisher prefix (repeat per prefix):
```sh
cargo run -p backfill-thumbnails -- \
  run \
  --scope prod-sfw-storj \
  --cutoff-before <APPROVED-CUTOFF> \
  --manifest-dir artifacts/backfill \
  --prefix <publisher-id>/ \
  --execute
```

Expected: `Failed: 0`. If failures occur, check `artifacts/backfill/prod-sfw-storj/*/failures.jsonl` and re-run — the manifest will skip already-completed items.

- [ ] **Step 2: Verify a sample**

```sh
cargo run -p backfill-thumbnails -- \
  verify \
  --scope prod-sfw-storj \
  --cutoff-before <APPROVED-CUTOFF> \
  --manifest-dir artifacts/backfill \
  --verify-sample 50
```

Expected: `FAIL: 0`.

- [ ] **Step 3: Final audit**

```sh
cargo run -p backfill-thumbnails -- \
  audit \
  --scope prod-sfw-storj \
  --cutoff-before <APPROVED-CUTOFF> \
  --manifest-dir artifacts/backfill
```

Expected: `Remaining videos to backfill: 0`.

- [ ] **Step 4: Commit**

```sh
git add artifacts/backfill/prod-sfw-storj/
git commit -m "chore: prod-sfw-storj backfill complete — <N> videos staged"
```

---

## Task 7: Execute `prod-sfw-hetzner`

- [ ] **Step 1: Execute**

```sh
cargo run -p backfill-thumbnails -- \
  run \
  --scope prod-sfw-hetzner \
  --cutoff-before <APPROVED-CUTOFF> \
  --manifest-dir artifacts/backfill \
  --execute
```

- [ ] **Step 2: Verify a sample**

```sh
cargo run -p backfill-thumbnails -- \
  verify \
  --scope prod-sfw-hetzner \
  --cutoff-before <APPROVED-CUTOFF> \
  --manifest-dir artifacts/backfill \
  --verify-sample 50
```

Expected: `FAIL: 0`.

- [ ] **Step 3: Final audit**

```sh
cargo run -p backfill-thumbnails -- \
  audit \
  --scope prod-sfw-hetzner \
  --cutoff-before <APPROVED-CUTOFF> \
  --manifest-dir artifacts/backfill
```

Expected: `Remaining videos to backfill: 0`.

- [ ] **Step 4: Commit**

```sh
git add artifacts/backfill/prod-sfw-hetzner/
git commit -m "chore: prod-sfw-hetzner backfill complete — <N> videos staged"
```

---

## Task 8: Execute `prod-nsfw-storj`

- [ ] **Step 1: Execute**

```sh
cargo run -p backfill-thumbnails -- \
  run \
  --scope prod-nsfw-storj \
  --cutoff-before <APPROVED-CUTOFF> \
  --manifest-dir artifacts/backfill \
  --execute
```

- [ ] **Step 2: Verify a sample**

```sh
cargo run -p backfill-thumbnails -- \
  verify \
  --scope prod-nsfw-storj \
  --cutoff-before <APPROVED-CUTOFF> \
  --manifest-dir artifacts/backfill \
  --verify-sample 50
```

Expected: `FAIL: 0`.

- [ ] **Step 3: Final audit**

```sh
cargo run -p backfill-thumbnails -- \
  audit \
  --scope prod-nsfw-storj \
  --cutoff-before <APPROVED-CUTOFF> \
  --manifest-dir artifacts/backfill
```

Expected: `Remaining videos to backfill: 0`.

- [ ] **Step 4: Commit**

```sh
git add artifacts/backfill/prod-nsfw-storj/
git commit -m "chore: prod-nsfw-storj backfill complete — <N> videos staged"
```

---

## Task 9: Post-Rollout Sign-Off

- [ ] All three production scopes show `Remaining: 0` in audit
- [ ] All verify samples show `FAIL: 0`
- [ ] Manifests archived under `artifacts/backfill/` and committed
- [ ] Share final summary table with Saikat

| Scope | Completed | Verified sample | Remaining |
|-------|-----------|-----------------|-----------|
| prod-sfw-storj | | PASS: N, FAIL: 0 | 0 |
| prod-sfw-hetzner | | PASS: N, FAIL: 0 | 0 |
| prod-nsfw-storj | | PASS: N, FAIL: 0 | 0 |

---

## Handling Failures Mid-Run

If a run reports `Failed > 0`:

1. Check `artifacts/backfill/<scope>/<bucket>/runs/<run-id>-run/failures.jsonl` for the error messages
2. Fix the root cause (network, ffmpeg, permissions)
3. Re-run the same `run --execute` command — the manifest ensures completed items are skipped automatically
4. Re-run until `Failed: 0`
5. Then proceed to verify and audit

Do **not** delete the manifest to "reset" — that would cause duplicate work for already-completed items.

---

## Known Safe Assumptions

The following failure scenarios were assessed and are **not risks** for this deployment:

| Scenario | Resolution |
|---|---|
| Naming collision with prod service | Production writes `_thumbnail.png` (underscore). Backfill writes `-thumbnail.png` (dash). Distinct keys, no overlap. |
| OOM on large video downloads | Videos are max ~1 minute / ~20MB. `download_concurrency=8` = ~160MB peak, well within any operator machine. |
| Concurrent operator runs | Runs are executed one scope at a time. No parallel sessions. |
| Hetzner S3 pagination | `S3Client::list_objects` already loops via `continuation_token`. All objects are listed regardless of bucket size. |

## Remaining Operational Risks

- **Wrong production cutoff** — Mitigated: the cutoff is now printed on every `run`, `verify`, and `audit` terminal summary so it cannot be missed. Still confirm the exact value before executing. Replace all `<CONFIRM-EXACT-PRODUCTION-CUTOFF>` placeholders in this plan at Task 5.
- **`uplink ls` unexpected output** — Mitigated: non-JSON non-empty lines now emit a `WARN` log. Run with `RUST_LOG=warn` to surface them. Pin the `uplink` CLI version on the operator machine and do not upgrade mid-run.
- **`verify` completeness gap** — Mitigated: the verify terminal summary now prints an explicit note to run `audit` afterward. Always confirm `Remaining: 0` via audit before signing off.

## Notes

- The operator always places `-thumbnail.png` in the **same folder** as the source video (no separate thumbnails directory)
- `audit` checks the real remote state; it does not trust the manifest for `Remaining` count
- Old `_thumbnail.png` objects are never touched by this tool
- Production cutoff must be confirmed before Task 5 — do not guess or reuse the test cutoff
