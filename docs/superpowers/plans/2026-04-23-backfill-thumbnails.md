# Thumbnail Backfill Migration Plan

**Goal:** Generate staged first-frame thumbnails as `<video_id>-thumbnail.png` for historical videos, starting with test buckets, then later rolling out to all required buckets only after explicit human approval. Existing `<video_id>_thumbnail.png` objects must remain untouched throughout this plan.

**Current rollout context:** The new first-frame thumbnail generation was rolled out on **April 22, 2026**. This backfill exists only for objects created before the final rollout cutoff. For test buckets, use `2026-04-22` as the default cutoff. Before any production run, confirm the exact production cutoff timestamp and use that absolute value in the command.

**Architecture:** Implement the operator as a separate Rust workspace crate at `crates/backfill-thumbnails`, with its binary entrypoint at `crates/backfill-thumbnails/src/main.rs`. It should support four modes: `seed-test-data`, `run`, `verify`, and `audit`. The binary must use bounded concurrency and backend-specific adapters:
- Storj via `uplink` subprocesses
- Hetzner via the existing Rust S3 client path

**Production-safety constraints (non-negotiable):**
- Never overwrite existing `_thumbnail.png` objects in this plan
- Never delete any thumbnail or video object in this plan
- Only create staged thumbnails named `<video_id>-thumbnail.png`
- Dry-run is the default; `--execute` is required to write
- Test buckets first; production buckets require explicit approval from Saikat before any write run
- A stopped or partial run must be resumable without duplicating completed work
- Each backend is processed independently; one backend failing must not force duplicate work on another backend
- Every write run must emit a durable manifest and a summary report
- Verification and audit modes must be read-only

---

## Storage Matrix

| Scope | Backend | Bucket / Destination | Write Key | Notes |
|------|---------|----------------------|-----------|-------|
| `test-sfw-storj` | Storj | Test Storj bucket | `<video_id>-thumbnail.png` | Required before any production Storj write |
| `test-sfw-hetzner` | Hetzner S3 | Test Hetzner bucket | `<video_id>-thumbnail.png` | Required before any production SFW Hetzner write |
| `prod-sfw-storj` | Storj | `SFW_BUCKET` (`yral-videos`) | `<video_id>-thumbnail.png` | Approval-gated |
| `prod-sfw-hetzner` | Hetzner S3 | `HETZNER_S3_BUCKET` | `<video_id>-thumbnail.png` | Approval-gated |
| `prod-nsfw-storj` | Storj | `NSFW_BUCKET` (`yral-nsfw-videos`) | `<video_id>-thumbnail.png` | Approval-gated |

**Important:** SFW production is not considered validated until both Storj and Hetzner test paths have passed. If Hetzner test access is not yet available, Storj-only testing is useful but does **not** complete the sign-off for SFW production rollout.

---

## File Map

| File | Action | Purpose |
|------|--------|---------|
| `crates/backfill-thumbnails/Cargo.toml` | Create | Separate crate manifest for the backfill operator |
| `crates/backfill-thumbnails/src/main.rs` | Create | Operator CLI entrypoint for seeding, running, verifying, and auditing |
| `src/thumbnail.rs` | Create | Shared thumbnail extraction helper used by the service and the operator |
| `src/s3_client.rs` | Extend | Reuse existing Hetzner S3 client for list/head/download/upload to staged thumbnail keys |
| `src/consts.rs` | Optional update | Add test-bucket env accessors only if needed; prefer CLI flags where possible |
| `scripts/verify-thumbnail-fix.sh` | Keep for now | Legacy helper only; not part of the authoritative backfill path |
| `scripts/test-thumbnail-e2e.sh` | Keep for now | Legacy helper only; useful as reference while migrating to Rust operator |

---

## Implementation Discipline

Implementation for this plan should happen on a dedicated git branch, not on `main`, and not in a worktree.

- Create and switch to a dedicated implementation branch before touching the code path for this plan
- Keep this plan file updated in the same branch as implementation progresses
- Mark each checklist item complete immediately after evidence exists; do not batch-update the plan at the end
- If a step is blocked, add a short note under that step rather than silently skipping it
- When useful, record evidence inline under the completed step:
  - command used
  - manifest or artifact directory
  - verification output location
  - commit hash

This plan document is the live execution tracker, not just a pre-implementation spec.

---

## Binary Design

The Rust binary should be the only authoritative operator interface for the backfill workflow. Avoid adding new shell scripts for the main workflow.

### Commands / Modes

- `seed-test-data`
  Creates deterministic test videos and intentionally wrong old-style thumbnails in test buckets only.
- `run`
  Discovers eligible videos, skips anything already staged, generates `-thumbnail.png`, and uploads it.
- `verify`
  Read-only mode that checks whether staged `-thumbnail.png` matches the first frame of the corresponding video.
- `audit`
  Read-only mode that reports candidate counts, staged counts, verified counts, and failed counts per backend.

### Required CLI flags

- `--scope <scope>`
  Example values: `test-sfw-storj`, `test-sfw-hetzner`, `prod-sfw-storj`, `prod-sfw-hetzner`, `prod-nsfw-storj`
- `--cutoff-before <timestamp-or-date>`
  Absolute cutoff such as `2026-04-22` or `2026-04-22T14:35:00Z`
- `--prefix <prefix>`
  Optional prefix filter for narrow runs
- `--manifest-dir <path>`
  Directory for durable state, logs, and summaries
- `--execute`
  Required for writes; omitted means dry-run
- `--download-concurrency <n>`
- `--ffmpeg-concurrency <n>`
- `--upload-concurrency <n>`
- `--verify-sample <n>`
  Optional for sampled verification

### Output layout

Each run writes to a dedicated directory such as:

```text
artifacts/backfill/<run-id>/
  manifest.jsonl
  summary.json
  failures.jsonl
  verify.jsonl
  audit.json
```

---

## Concurrency And Resume Model

This needs to be production-grade, not just fast.

### Work discovery

- List candidate `.mp4` objects and existing `-thumbnail.png` objects per backend
- Run discovery for independent backends in parallel
- Filter candidates by:
  - cutoff
  - optional prefix
  - remote staged file already exists
  - manifest already shows a successful prior completion for the same backend and staged key

### Worker pipeline

For each backend, process work through a bounded pipeline:

1. Download video to a unique temp directory
2. Run `ffmpeg` to extract the first frame
3. Sanity-check the generated PNG
4. Upload `<video_id>-thumbnail.png`
5. Append a durable manifest row

Recommended defaults:
- `download_concurrency = 8`
- `ffmpeg_concurrency = max(1, available_parallelism / 2)`
- `upload_concurrency = 8`

All limits must be configurable because the safe values for a laptop, CI runner, and production runner differ.

### Resume guarantees

The binary must avoid duplicate work in interrupted runs:

- Remote staged object exists for that backend/key:
  mark as `SKIP_REMOTE_EXISTS`
- Manifest already records a completed upload for that backend/key:
  mark as `SKIP_MANIFEST_DONE`
- Failed items remain retryable on the next run
- Partial temp files are local only and must never be treated as completion markers

The remote staged object is the source of truth for completion. The manifest is the operator log and audit trail.

---

## Backend Strategy

### Storj backend

Use `uplink` subprocesses because that matches the current production Storj path in this repo.

Required operations:
- list objects
- download video object
- upload staged thumbnail object
- detect whether staged thumbnail already exists

Do not route Storj through the S3 gateway for this plan.

### Hetzner backend

Reuse and extend the existing Rust S3 client rather than creating a second unrelated S3 path.

Add or expose methods for:
- `list_objects_v2`
- `head_object`
- `get_object`
- `put_object` to an arbitrary staged thumbnail key

### Shared abstraction

Create a backend trait used by the binary so the processing pipeline is shared while the storage operations remain backend-specific.

Each manifest row must include:
- backend kind
- bucket
- source video key
- staged thumbnail key
- status
- bytes uploaded
- attempts
- started timestamp
- completed timestamp
- error text if any

---

## Verification Strategy

Verification is read-only and must work for both test and later production review.

### `seed-test-data`

Implement seeded test data in Rust, not shell:

- Create tiny deterministic videos via `ffmpeg`
- Example pattern:
  - first frame = blue
  - frame 1 = red
  - frame 2 = green
- Intentionally create the wrong historical thumbnail from the 1-second frame
- Upload:
  - original video
  - old thumbnail as `<video_id>_thumbnail.png`

This gives a known before/after state:
- old thumbnail = red
- staged backfill thumbnail = blue

### `verify`

For each sampled or requested item:

1. Download the source video
2. Download the staged `-thumbnail.png`
3. Extract the first frame locally
4. Compare the staged thumbnail to the first frame using a stable comparison method

Preferred comparison:
- normalize both images through `ffmpeg`
- compare via PSNR or byte-level equality after normalization

The verify report must clearly state:
- `PASS`
- `FAIL`
- `SKIP`

It should also include backend and object key for each row.

---

## Audit Strategy

`audit` must be read-only and per-backend.

For each backend, report:
- total `.mp4` candidates before the cutoff
- total existing staged `-thumbnail.png`
- total completed in manifest
- total verification passes
- total failures
- remaining work

For SFW rollout, the audit should also make it easy to compare Storj and Hetzner counts side by side so inconsistencies are visible before production sign-off.

---

## Branch-Switch Gate

After test-bucket validation passes and before any wider rollout:

- [ ] Switch to the branch that reflects the current live consumer/read path
- [ ] Check whether any code path or operational process actually requires staged `-thumbnail.png` files
- [ ] Record whether staged files are:
  - only validation artifacts for now, or
  - needed temporarily by another branch / workflow
- [ ] Do **not** delete anything as part of this check

This gate exists because staged files are a safety mechanism, but we should still verify whether they remain necessary before expanding the rollout.

---

## Task 0: Create Implementation Branch And Tracking Loop

- [x] Create and switch to a dedicated implementation branch from the current base branch
  Evidence: working branch is `backfill-thumbnails`
- [x] Confirm the branch name is recorded in the run notes or commit history
  Evidence: branch recorded here as `backfill-thumbnails`
- [x] Keep this plan file open and update it as work proceeds
- [x] Mark each step complete only after the corresponding code, command, or verification evidence exists

Deliverable: implementation starts in an isolated branch and progress is tracked live in this plan.
Evidence: this plan has been updated alongside implementation on branch `backfill-thumbnails`

---

## Task 1: Finalize Operator Contract

- [x] Define the `backfill_thumbnails` CLI surface in `crates/backfill-thumbnails/src/main.rs`
- [x] Lock the staged output naming to `<video_id>-thumbnail.png`
- [x] Lock dry-run as the default; only `--execute` enables writes
- [x] Lock absolute cutoff handling for all non-test runs
- [x] Define one manifest directory layout for all run modes

Deliverable: operator usage and modes are stable before implementation expands.
Evidence: implemented in `crates/backfill-thumbnails/src/lib.rs` and `crates/backfill-thumbnails/src/main.rs`
Evidence: verified by `cargo test -p backfill-thumbnails`

---

## Task 2: Implement Backend Adapters

- [x] Create a shared backend abstraction for list/download/upload operations
- [x] Implement Storj backend using `uplink`
- [x] Extend `src/s3_client.rs` for Hetzner list/head/download/upload support
- [x] Make backend target selection explicit via `--scope`
- [x] Ensure every staged upload writes to the same directory as the source video

Deliverable: one shared processing pipeline can run against Storj or Hetzner without branching business logic everywhere.
Evidence: implemented in `crates/backfill-thumbnails/src/backend.rs`
Evidence: Hetzner helper extensions added in `src/s3_client.rs`
Evidence: verified by `cargo test -p backfill-thumbnails`

---

## Task 3: Implement High-Concurrency `run`

- [x] Build single-pass discovery for videos and staged thumbnails
- [x] Filter candidates by cutoff, prefix, remote staged existence, and manifest success
- [x] Build bounded worker pools for download, `ffmpeg`, and upload stages
- [x] Use unique temp directories per work item
- [x] Fail one object independently without aborting unrelated objects
- [x] Append success and failure rows durably as the run progresses
- [x] Exit non-zero if any work item fails in execute mode

Deliverable: resumable, bounded, backend-aware staged backfill execution.
Evidence: planning and execution pipeline implemented in `crates/backfill-thumbnails/src/lib.rs` and `crates/backfill-thumbnails/src/main.rs`
Evidence: shared first-frame extraction moved to `src/thumbnail.rs`
Evidence: verified by `cargo test --workspace`

---

## Task 4: Implement Read-Only `seed-test-data`, `verify`, and `audit`

- [x] `seed-test-data`: create deterministic old-style thumbnails in test buckets only
- [x] `verify`: confirm staged thumbnails match the first frame
- [x] `audit`: report per-backend totals and remaining work
- [x] Keep these modes read-only except for `seed-test-data`

Deliverable: the operator fully owns test setup, validation, and reporting.
Evidence: implemented in `crates/backfill-thumbnails/src/main.rs`
Evidence: first-frame and seeked thumbnail behavior verified by:
`cargo test -p storj-interface --lib thumbnail::tests::extract_thumbnail_with_seek_uses_the_requested_frame -- --ignored`
Evidence: workspace verification completed with `cargo test --workspace`

---

## Task 4A: Harden The Operator From Review Findings

- [x] Require an explicit `--bucket` for `test-sfw-hetzner` so test scope cannot silently fall through to the production Hetzner bucket
- [x] Scope operator state and manifest resume behavior by resolved bucket as well as backend
- [x] Make manifest loading tolerant of a torn final JSONL line while still failing on invalid complete lines
- [x] Make `verify` compare decoded image pixels rather than raw PNG bytes

Deliverable: review findings have been converted into concrete safety and correctness hardening before any bucket runs.
Evidence: implemented in `crates/backfill-thumbnails/src/lib.rs`, `crates/backfill-thumbnails/src/backend.rs`, and `crates/backfill-thumbnails/src/main.rs`
Evidence: verified by `cargo test -p backfill-thumbnails`
Evidence: verified by `cargo test --workspace`

---

## Task 4B: Move The Operator CLI To `clap`

- [x] Replace the hand-rolled argument parser with a `clap`-based CLI while preserving the existing command contract
- [x] Make top-level and subcommand `--help` output first-class operator behavior
- [x] Keep `rayon` out of the crate and document that concurrency stays on Tokio semaphores plus `ffmpeg` subprocess parallelism
- [x] Update the crate-local README so the documented CLI matches the implementation

Deliverable: the operator has a typed `clap` CLI with generated help text and aligned documentation.
Evidence: implemented in `crates/backfill-thumbnails/src/lib.rs` and `crates/backfill-thumbnails/src/main.rs`
Evidence: documented in `crates/backfill-thumbnails/README.md`

---

## Task 4C: Print Human Summaries To The Terminal

- [x] Print a compact terminal summary for `seed-test-data`
- [x] Print a compact terminal summary for `run`
- [x] Print a compact terminal summary for `verify`
- [x] Print a compact terminal summary for `audit`
- [x] Keep JSON artifact output as the durable source of truth

Deliverable: every operator mode gives immediate human-readable feedback in the terminal without removing artifact files.
Evidence: implemented in `crates/backfill-thumbnails/src/main.rs`
Evidence: documented in `crates/backfill-thumbnails/README.md`

---

## Task 4D: Stabilize Storj Listing And Cutoff Handling

- [x] Switch Storj object discovery from brittle tabular `uplink ls` parsing to `uplink ls --utc -o json`
- [x] Parse Storj listing timestamps in UTC so cutoff comparisons align with operator input
- [x] Add regression tests for the Storj JSON listing format

Deliverable: Storj-backed `run`, `verify`, and `audit` commands see real bucket contents and apply cutoffs against UTC timestamps.
Evidence: implemented in `crates/backfill-thumbnails/src/backend.rs`
Evidence: verified by `cargo test -p backfill-thumbnails`

---

## Task 4E: Clarify Terminal Summary Labels

- [x] Make `run` summaries explicitly call out staged `-thumbnail.png` counts
- [x] Make `audit` summaries explicitly distinguish total objects, candidate videos, and remaining videos to backfill
- [x] Keep README wording aligned with the terminal output

Deliverable: operator summaries are clearer about what counts videos versus legacy or staged thumbnail objects.
Evidence: implemented in `crates/backfill-thumbnails/src/main.rs`
Evidence: documented in `crates/backfill-thumbnails/README.md`

---

## Task 5: Validate On Test Buckets First

> **This is the only allowed write stage before any production approval.**

- [x] Run `seed-test-data` for `test-sfw-storj`
  Evidence: `cargo run -p backfill-thumbnails -- seed-test-data --scope test-sfw-storj --bucket test-duplicate --manifest-dir artifacts/backfill`
  Evidence: summary at `artifacts/backfill/test-sfw-storj/test-duplicate/runs/20260424T033024Z-seed-test-data/summary.json` reports `"seeded": 3`
  Evidence: remote `uplink ls` shows 3 seeded `.mp4` objects and 3 legacy `_thumbnail.png` objects under `test-user/` in bucket `test-duplicate`
- [ ] Run `run` dry-run for `test-sfw-storj`
- [ ] Run `run --execute` for `test-sfw-storj`
- [ ] Run `verify` for `test-sfw-storj`
- [ ] Run `audit` for `test-sfw-storj`

- [ ] If Hetzner test access is available, repeat for `test-sfw-hetzner`
- [ ] If Hetzner test access is not yet available, stop short of SFW production sign-off and request/arrange that access before widening rollout

Deliverable: staged thumbnails are proven on test buckets before broader use.

---

## Task 6: Branch Switch And Staged-File Relevance Check

- [ ] Switch to the relevant branch and inspect the live read path
- [ ] Confirm whether staged files are still required by any rollout or validation workflow
- [ ] Document the outcome in the run notes
- [ ] Keep the no-delete rule in force regardless of outcome

Deliverable: we know whether `-thumbnail.png` remains operationally necessary before the rollout expands.

---

## Task 7: Production Approval Gate

> **STOP. Do not write to any production bucket until Saikat explicitly approves.**

- [ ] Share test-bucket `verify` and `audit` outputs
- [ ] Share manifest summaries and failure counts
- [ ] Confirm the exact production cutoff timestamp
- [ ] Confirm which scopes are approved for rollout

Deliverable: explicit human approval and a locked production scope.

---

## Task 8: Production Dry-Run Then Execute

Only after Task 7:

- [ ] Run `run` dry-run for each approved production scope
- [ ] Review candidate counts and staged-skip counts
- [ ] Run `verify --verify-sample <n>` on a small read-only sample if needed
- [ ] Run `run --execute` for the approved production scope
- [ ] Run `audit` immediately after completion

Suggested rollout order:
1. `prod-sfw-storj`
2. `prod-sfw-hetzner`
3. `prod-nsfw-storj`

Adjust only if the human approver asks for a different order.

---

## Explicit Non-Goals

- No overwrite of `_thumbnail.png`
- No delete / cleanup of any thumbnails
- No assumption that Storj and Hetzner can be treated as one completion marker
- No shell-script-based primary operator flow
- No production write before test-bucket validation and explicit approval

---

## Notes For Future Follow-Up

Once this staged backfill is complete and validated everywhere, a separate plan can decide:
- whether `_thumbnail.png` should later be replaced from staged objects
- whether staged objects remain useful
- whether any cleanup is worth doing later

Those decisions are deliberately out of scope for this plan.
