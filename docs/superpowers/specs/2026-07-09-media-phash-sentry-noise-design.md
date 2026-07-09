# Media pHash Sentry Noise Design

## Goal

Stop expected, handled per-video failures from creating Sentry error issues while
preserving alerts for failures that escape the job boundary.

## Logging Policy

Severity is owned by the caller that understands whether a failure is handled:

- S3 retry attempts and final retry exhaustion log at `WARN` and return the error.
- `media_phash` per-row download or decode failures log at `WARN`.
- Per-row failures remain persisted in `media_job_failures`, retain exponential
  backoff, and leave the job run as `succeeded_with_failures`.
- Unhandled database errors, failed worker passes, and panics remain Sentry
  events through their existing error reporting paths.

Warnings remain Sentry breadcrumbs under the existing tracing configuration, so
an eventual systemic event retains the preceding storage context without
creating an issue for each video or retry layer.

## Scope

Change only the log severity for:

- `S3 operation failed after all retries`
- `media_phash: row failed`

Do not change retry counts, backoff, failure persistence, job status, or Sentry's
global `ERROR`/`WARN` mapping.

## Verification

Focused tracing tests capture emitted events and assert that both messages are
present at `WARN` and absent at `ERROR`. Existing job persistence tests continue
to verify failure recording and retry behavior. Run the focused tests, compile
all library tests, and run `cargo check`.
