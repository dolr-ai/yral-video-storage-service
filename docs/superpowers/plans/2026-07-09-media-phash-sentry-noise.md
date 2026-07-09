# Media pHash Sentry Noise Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prevent handled S3 retry exhaustion and per-video pHash failures from creating Sentry error issues.

**Architecture:** Keep the global tracing-to-Sentry mapping unchanged and assign severity at the handling boundary. Storage helpers return errors after warning-level retry logs; the pHash job records row failures and logs them as warnings, while errors escaping the job continue through existing Sentry paths.

**Tech Stack:** Rust, Tokio, `tracing`, `tracing-subscriber`, Cargo tests

---

### Task 1: Verify Per-Row Failure Severity

**Files:**
- Modify: `src/jobs/media_phash.rs`
- Test: `src/jobs/media_phash.rs`

- [ ] **Step 1: Retain the tracing capture test for `media_phash: row failed`**

The test must assert that the message is emitted at `WARN` and never at `ERROR`.

- [ ] **Step 2: Remove the warning implementation temporarily and run the test**

Run:

```bash
cargo test -p storj-interface --lib row_failure_logs_warning_not_error -- --test-threads=1
```

Expected: FAIL because the existing implementation emits `ERROR`.

- [ ] **Step 3: Restore the minimal warning implementation**

Route the row-failure log through `log_row_failure`, using `tracing::warn!`.

- [ ] **Step 4: Run the focused test**

Run the command from Step 2. Expected: PASS.

### Task 2: Verify S3 Retry-Exhaustion Severity

**Files:**
- Modify: `src/s3_client.rs`
- Test: `src/s3_client.rs`

- [ ] **Step 1: Retain the tracing capture test for retry exhaustion**

The test must assert four attempts, the returned error, a `WARN` event, and no
matching `ERROR` event.

- [ ] **Step 2: Remove the warning implementation temporarily and run the test**

Run:

```bash
cargo test -p storj-interface --lib retry_exhaustion_logs_warning_not_error -- --test-threads=1
```

Expected: FAIL because retry exhaustion emits `ERROR`.

- [ ] **Step 3: Restore the shared warning helper**

Use `log_s3_retry_exhausted` in both retry helpers and streamed downloads.

- [ ] **Step 4: Run the focused test**

Run the command from Step 2. Expected: PASS.

### Task 3: Verify the Complete Change

**Files:**
- Verify: `src/jobs/media_phash.rs`
- Verify: `src/s3_client.rs`

- [ ] **Step 1: Run both focused tests together**

```bash
cargo test -p storj-interface --lib logs_warning_not_error -- --test-threads=1
```

- [ ] **Step 2: Compile all library tests**

```bash
cargo test -p storj-interface --lib --no-run
```

- [ ] **Step 3: Check the package**

```bash
cargo check -p storj-interface
```

- [ ] **Step 4: Inspect the final diff**

Confirm that only logging severity and its regression tests changed; retry,
backoff, persistence, and global Sentry configuration remain untouched.
