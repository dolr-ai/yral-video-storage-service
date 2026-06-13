# Videogen RabbitMQ Publisher Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the current synchronous Prakash-to-Vast HTTP submit step with a RabbitMQ publish-confirm enqueue path, then finish the Prakash-side runtime callbacks that currently remain stubbed.

**Architecture:** Keep `/api/v2/videogen/generate` as the mobile-facing orchestration endpoint. It still validates identity, moderates, checks RateLimiter, persists the completion context, stages image input, reserves upload destination, generates `request_id`, and returns immediately after durable job submission. The submit backend becomes RabbitMQ AMQPS publish-confirm to `videogen.jobs` with routing key `ltx.generate`; Vast consumes from `videogen.ltx.generate` and later calls the existing Prakash completion endpoint.

**Tech Stack:** Rust, Axum, Tokio, Postgres, IC RateLimiter canister, RabbitMQ quorum queue over AMQPS, `lapin`/Tokio AMQP client, reqwest for moderation/upload-service HTTP, existing HMAC completion auth.

---

## Current State

This repo already has most of the lean videogen flow:

- `/api/v2/videogen/generate` exists.
- Moderation is routed through generic `MODERATION_SERVICE_URL`.
- RateLimiter create/check is implemented for generate.
- Completion context persistence exists.
- Upload destination reservation exists.
- Image staging exists.
- `/api/v2/videogen/complete` exists.
- `/api/v2/videogen/upload-url/refresh` exists.
- Reconciliation exists.
- Provider endpoints exist.

The remaining gaps for production are:

- Generate still submits directly to Vast HTTP via `VIDEOGEN_VAST_GENERATE_URL` and `VAST_API_KEY`.
- `Cargo.toml` has no AMQP/RabbitMQ dependency.
- Runtime completion RateLimiter calls are stubs in `src/routes/videogen/complete.rs`.
- Runtime reconciliation RateLimiter calls are stubs in `src/videogen/reconcile.rs`.
- Runtime upload-destination release is a no-op in generate, completion, and reconciliation.
- Draft creation is a logging stub in `src/videogen/draft.rs`.
- `src/videogen/rate_limiter.rs` exists, but shared runtime helper functions still live partly inside route modules.
- Completion does not yet decrypt the stored delegated identity for the draft metadata call.

## RabbitMQ Contract

The broker is already deployed and verified:

- vhost: `/videogen`
- exchange: `videogen.jobs`
- routing key: `ltx.generate`
- queue: `videogen.ltx.generate`
- DLQ: `videogen.ltx.generate.dlq`
- publisher user: `prakash_videogen_publisher`
- consumer user: `vast_ltx_consumer`
- admin user: `rabbitmq_admin`

Prakash publishes the same logical payload as the current `VastSubmitRequest`:

```json
{
  "request_id": "<uuid-v4>",
  "request_key": { "principal": "...", "counter": 123 },
  "user_principal": "...",
  "model_id": "ltx2",
  "workflow_json": {},
  "input": {},
  "callback_url": "https://prakash.example/api/v2/videogen/complete",
  "upload_url_refresh_url": "https://prakash.example/api/v2/videogen/upload-url/refresh",
  "upload_destination": {
    "video_id": "...",
    "object_key": "...",
    "upload_url": "...",
    "expires_at": "2026-05-27T12:00:00Z"
  }
}
```

AMQP message requirements:

- `exchange`: `videogen.jobs`
- `routing_key`: `ltx.generate`
- `content_type`: `application/json`
- `delivery_mode`: persistent
- `message_id`: `request_id`
- `correlation_id`: `request_id`
- publisher confirm is required before Prakash marks the context `submitted`
- unroutable messages must be treated as submit failure
- publish timeout maps to submit failure and triggers existing `fail_after_rate_limit`

## File Structure

- `Cargo.toml`
  - Add AMQP client dependencies.
- `src/consts.rs`
  - Add RabbitMQ env var names.
- `src/videogen/config.rs`
  - Add RabbitMQ submit transport config and publish timeouts.
- `src/videogen/rabbitmq.rs`
  - New RabbitMQ publisher config, message builder, publisher client, and tests.
- `src/videogen/vast.rs`
  - Keep `VastSubmitRequest` and `VastSubmitAccepted`.
  - Keep legacy HTTP client for rollback.
- `src/videogen/mod.rs`
  - Export `rabbitmq`.
- `src/routes/videogen/generate.rs`
  - Switch runtime submit to HTTP or RabbitMQ based on config.
  - Treat publish-confirm success as accepted state.
- `src/routes/videogen/complete.rs`
  - Replace RateLimiter stubs with real canister calls.
  - Decrypt delegated identity for draft creation.
- `src/videogen/reconcile.rs`
  - Replace RateLimiter stubs with real canister calls.
  - Use real draft client for uploaded/draft_creating recovery.
- `src/videogen/draft.rs`
  - Replace logging-only draft client with upload-service metadata client.
- `src/videogen/rate_limiter.rs`
  - Move shared canister request-key conversion and update helpers here.
- Tests stay colocated with modules unless an integration broker smoke test is added.

---

### Task 1: Add RabbitMQ Config And Dependencies

**Files:**
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/Cargo.toml`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/Cargo.lock`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/consts.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/config.rs`

- [ ] **Step 1: Write failing config tests**

Add tests in `src/videogen/config.rs`:

```rust
#[test]
fn default_submit_transport_is_http_for_rollback() {
    let cfg = VideogenConfig::test_defaults();
    assert_eq!(cfg.vast_submit_transport, VastSubmitTransport::Http);
}

#[test]
fn parses_rabbitmq_submit_transport() {
    assert_eq!(
        VastSubmitTransport::parse("rabbitmq").unwrap(),
        VastSubmitTransport::RabbitMq
    );
    assert_eq!(
        VastSubmitTransport::parse("amqp").unwrap(),
        VastSubmitTransport::RabbitMq
    );
}

#[test]
fn rabbitmq_publish_confirm_timeout_defaults_to_submit_timeout() {
    let cfg = VideogenConfig::test_defaults();
    assert_eq!(cfg.rabbitmq_publish_timeout_secs, cfg.vast_submit_timeout_secs);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test videogen::config
```

Expected: compile failure because `VastSubmitTransport` and RabbitMQ config fields do not exist.

- [ ] **Step 3: Add dependencies**

Add AMQP dependencies to `Cargo.toml`.

Start by checking the current crate compatibility rather than blindly pinning old integration crates. As of this plan, `tokio-amqp 2.0.0` exists and `lapin` has current versions with Tokio and rustls feature support. Prefer the smallest dependency set that compiles with this repo:

Option A, preferred if supported by the selected `lapin` version:

```toml
lapin = { version = "4", default-features = false, features = ["tokio", "rustls--aws_lc_rs"] }
```

Option B, if the selected `lapin` version still requires `tokio-amqp` integration:

```toml
lapin = { version = "2.5", default-features = false, features = ["rustls"] }
tokio-amqp = "2.0"
```

If Cargo reports feature drift for `lapin`, inspect the crate feature error and keep these requirements unchanged:

- Tokio-compatible runtime.
- AMQPS support.
- Rustls or an equivalent TLS stack.
- No OpenSSL dependency unless the repo already accepts it.

- [ ] **Step 4: Add env constants**

Add to `src/consts.rs`:

```rust
pub const VIDEOGEN_VAST_SUBMIT_TRANSPORT: &str = "VIDEOGEN_VAST_SUBMIT_TRANSPORT";
pub const VIDEOGEN_RABBITMQ_AMQPS_URLS: &str = "VIDEOGEN_RABBITMQ_AMQPS_URLS";
pub const VIDEOGEN_RABBITMQ_EXCHANGE: &str = "VIDEOGEN_RABBITMQ_EXCHANGE";
pub const VIDEOGEN_RABBITMQ_ROUTING_KEY: &str = "VIDEOGEN_RABBITMQ_ROUTING_KEY";
pub const VIDEOGEN_RABBITMQ_PUBLISH_TIMEOUT_SECS: &str =
    "VIDEOGEN_RABBITMQ_PUBLISH_TIMEOUT_SECS";
pub const VIDEOGEN_RABBITMQ_CONNECTION_NAME: &str = "VIDEOGEN_RABBITMQ_CONNECTION_NAME";
pub const VIDEOGEN_RABBITMQ_TLS_CA_CERT_PEM_B64: &str =
    "VIDEOGEN_RABBITMQ_TLS_CA_CERT_PEM_B64";
```

- [ ] **Step 5: Implement config fields**

Add:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VastSubmitTransport {
    Http,
    RabbitMq,
}
```

Parse:

- `http` -> `Http`
- `rabbitmq` -> `RabbitMq`
- `amqp` -> `RabbitMq`

Add to `VideogenConfig`:

```rust
pub vast_submit_transport: VastSubmitTransport,
pub rabbitmq_amqps_urls: Vec<String>,
pub rabbitmq_exchange: String,
pub rabbitmq_routing_key: String,
pub rabbitmq_publish_timeout_secs: u64,
pub rabbitmq_connection_name: String,
pub rabbitmq_tls_ca_cert_pem_b64: Option<String>,
```

Defaults:

- `VIDEOGEN_VAST_SUBMIT_TRANSPORT`: `http`
- `VIDEOGEN_RABBITMQ_AMQPS_URLS`: no default; required only when transport is `rabbitmq`
- `VIDEOGEN_RABBITMQ_EXCHANGE`: `videogen.jobs`
- `VIDEOGEN_RABBITMQ_ROUTING_KEY`: `ltx.generate`
- `VIDEOGEN_RABBITMQ_PUBLISH_TIMEOUT_SECS`: default to `VIDEOGEN_VAST_SUBMIT_TIMEOUT_SECS`
- `VIDEOGEN_RABBITMQ_CONNECTION_NAME`: `yral-video-storage-service-videogen-publisher`
- `VIDEOGEN_RABBITMQ_TLS_CA_CERT_PEM_B64`: optional base64 PEM CA certificate for private/self-signed broker certificates

When `VIDEOGEN_VAST_SUBMIT_TRANSPORT=rabbitmq`, reject empty `VIDEOGEN_RABBITMQ_AMQPS_URLS` at config load.

- [ ] **Step 6: Run tests**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test videogen::config
```

Expected: config tests pass.

- [ ] **Step 7: Commit**

```bash
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service add Cargo.toml Cargo.lock src/consts.rs src/videogen/config.rs
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service commit -m "feat: add videogen rabbitmq config"
```

---

### Task 2: Add RabbitMQ Publisher Module

**Files:**
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/rabbitmq.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/mod.rs`

- [ ] **Step 1: Write failing publisher envelope tests**

Create `src/videogen/rabbitmq.rs` with tests first:

```rust
#[test]
fn publish_envelope_uses_request_id_for_message_ids() {
    let request = sample_vast_submit_request();
    let envelope = RabbitMqPublishEnvelope::from_request(&request).unwrap();

    assert_eq!(envelope.message_id.as_deref(), Some(request.request_id.as_str()));
    assert_eq!(envelope.correlation_id.as_deref(), Some(request.request_id.as_str()));
    assert_eq!(envelope.content_type.as_deref(), Some("application/json"));
    assert!(envelope.persistent);
}

#[test]
fn publish_envelope_body_is_vast_submit_request_json() {
    let request = sample_vast_submit_request();
    let envelope = RabbitMqPublishEnvelope::from_request(&request).unwrap();
    let decoded: serde_json::Value = serde_json::from_slice(&envelope.body).unwrap();

    assert_eq!(decoded["request_id"], request.request_id);
    assert_eq!(decoded["request_key"]["principal"], request.request_key.principal);
    assert_eq!(
        decoded["upload_destination"]["video_id"],
        request.upload_destination.video_id
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test videogen::rabbitmq
```

Expected: compile failure because module does not exist.

- [ ] **Step 3: Implement envelope builder**

Implement:

```rust
pub struct RabbitMqPublishConfig {
    pub amqps_urls: Vec<String>,
    pub exchange: String,
    pub routing_key: String,
    pub connection_name: String,
    pub publish_timeout_secs: u64,
    pub tls_ca_cert_pem_b64: Option<String>,
}

pub struct RabbitMqPublishEnvelope {
    pub body: Vec<u8>,
    pub message_id: Option<String>,
    pub correlation_id: Option<String>,
    pub content_type: Option<String>,
    pub persistent: bool,
}

impl RabbitMqPublishEnvelope {
    pub fn from_request(request: &VastSubmitRequest) -> Result<Self, RabbitMqPublishError> {
        Ok(Self {
            body: serde_json::to_vec(request)?,
            message_id: Some(request.request_id.clone()),
            correlation_id: Some(request.request_id.clone()),
            content_type: Some("application/json".to_string()),
            persistent: true,
        })
    }
}
```

Error type:

```rust
#[derive(Debug, thiserror::Error)]
pub enum RabbitMqPublishError {
    #[error("failed to serialize RabbitMQ message: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("RabbitMQ connection failed: {0}")]
    Connect(String),
    #[error("RabbitMQ channel failed: {0}")]
    Channel(String),
    #[error("RabbitMQ publish failed: {0}")]
    Publish(String),
    #[error("RabbitMQ publish timed out")]
    Timeout,
    #[error("RabbitMQ publish was not confirmed")]
    NotConfirmed,
}
```

- [ ] **Step 4: Implement AMQPS publisher**

Implement `RabbitMqPublisher`:

```rust
pub struct RabbitMqPublisher {
    config: RabbitMqPublishConfig,
}
```

Behavior:

1. Iterate `amqps_urls` in order.
2. Connect with configured `connection_name`.
3. Create channel.
4. Enable publisher confirms.
5. Register handling for basic-return/unroutable messages before publish.
6. Publish to `exchange` and `routing_key` with mandatory publish.
7. Use persistent delivery mode and JSON metadata from the envelope.
8. Wait for publisher confirm.
9. Return `VastSubmitAccepted { request_id, status: "queued", accepted_at: Utc::now() }`.
10. If one URL fails, try the next URL.
11. If all URLs fail, return the last error.

Implementation notes:

- Do not log the AMQPS URL because it contains credentials.
- Do log broker host without password if needed.
- Treat unroutable returns and negative confirms as submit failure.
- Do not publish to the queue directly; publish to `videogen.jobs`.
- If `tls_ca_cert_pem_b64` is set, use it to trust the broker certificate. If the AMQP crate requires a custom TLS connector for this, keep the connector code contained inside `rabbitmq.rs`.
- Never add a "danger accept invalid certs" mode.

- [ ] **Step 5: Export module**

Add to `src/videogen/mod.rs`:

```rust
pub mod rabbitmq;
```

- [ ] **Step 6: Run tests**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test videogen::rabbitmq
```

Expected: RabbitMQ module tests pass without a live broker.

- [ ] **Step 7: Commit**

```bash
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service add src/videogen/rabbitmq.rs src/videogen/mod.rs
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service commit -m "feat: add videogen rabbitmq publisher"
```

---

### Task 3: Wire Generate Submission To RabbitMQ

**Files:**
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/routes/videogen/generate.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/vast.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/config.rs`

- [ ] **Step 1: Write failing runtime submit selection tests**

Add tests in `src/routes/videogen/generate.rs` around the existing `FakeDeps` submit dependency boundary.

First extend the existing test helper. Today it has:

- `struct FakeDeps`
- `fn request() -> GenerateRequest`
- `fn config() -> GenerateConfig`
- `vast: Option<VastSubmitAccepted>`

Change `FakeDeps` so it can return success or failure:

```rust
vast: Option<Result<VastSubmitAccepted, VastSubmitError>>,
```

In `submit_vast`, keep the existing default behavior that echoes the generated `request.request_id` when `vast` is `None`. Only use `vast: Some(Err(...))` for failure tests or `vast: Some(Ok(...))` for explicit mismatch tests.

Then add:

```rust
#[tokio::test]
async fn rabbitmq_submit_success_marks_context_submitted_and_returns_operation_id() {
    let deps = FakeDeps::with_calls();

    let response = generate_with_dependencies(
        request(),
        &deps,
        config(),
    )
    .await
    .unwrap();

    assert_eq!(response.operation_id, "aaaaa-aa_17");
    assert!(deps.calls().contains(&Call::VastSubmit));
    assert!(deps.calls().contains(&Call::ContextSubmitted));
}
```

Add a failure test:

```rust
#[tokio::test]
async fn rabbitmq_submit_failure_rolls_back_rate_limiter_and_redacts_identity() {
    let deps = FakeDeps {
        vast: Some(Err(VastSubmitError::RequestFailed(
            "RabbitMQ publish timed out".to_string(),
        ))),
        ..FakeDeps::with_calls()
    };

    let err = generate_with_dependencies(
        request(),
        &deps,
        config(),
    )
    .await
    .unwrap_err();

    assert_eq!(err.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        deps.calls(),
        vec![
            Call::DedupeLookup,
            Call::Moderate,
            Call::RateLimiterCreate,
            Call::ContextCreate,
            Call::StageImage,
            Call::WorkflowJson,
            Call::ReserveUpload,
            Call::SaveUpload,
            Call::RequestIdStored,
            Call::VastSubmit,
            Call::ContextSubmitFailed,
            Call::RateLimiterFailed,
            Call::RateLimiterDecrement,
            Call::ReleaseUpload,
            Call::RedactIdentity,
        ]
    );
}
```

- [ ] **Step 2: Run tests to verify current behavior**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test routes::videogen::generate
```

Expected: new RabbitMQ-specific tests fail until runtime selection exists.

- [ ] **Step 3: Implement runtime backend selection**

In `RuntimeGenerateDeps::submit_vast`, switch on `self.config.vast_submit_transport`:

```rust
match self.config.vast_submit_transport {
    VastSubmitTransport::Http => submit_vast_http(...).await,
    VastSubmitTransport::RabbitMq => submit_vast_rabbitmq(...).await,
}
```

HTTP branch:

- Keep the existing `VIDEOGEN_VAST_GENERATE_URL` + `VAST_API_KEY` behavior for rollback.
- Keep exact accepted-response verification.

RabbitMQ branch:

- Build `RabbitMqPublishConfig` from `VideogenConfig`.
- Publish `VastSubmitRequest`.
- Map publisher-confirm success to `VastSubmitAccepted { request_id, status: "queued", accepted_at }`.
- Map publish timeout or confirm failure to `VastSubmitError::RequestFailed(...)`.

Do not change `generate_inner` rollback behavior; RabbitMQ failures must use the existing `fail_after_rate_limit` path.

- [ ] **Step 4: Add submit transport metrics labels**

For `videogen_vast_submit_total`, use labels:

- `transport=http`
- `transport=rabbitmq`
- `result=attempt`

If the current metrics usage cannot label cleanly without broad churn, add a new counter:

```rust
pub const SUBMIT_TRANSPORT_TOTAL: &str = "videogen_submit_transport_total";
```

- [ ] **Step 5: Run targeted tests**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test routes::videogen::generate
cargo test videogen::vast
cargo test videogen::rabbitmq
```

Expected: all targeted tests pass.

- [ ] **Step 6: Commit**

```bash
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service add src/routes/videogen/generate.rs src/videogen/vast.rs src/videogen/config.rs src/videogen/metrics.rs
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service commit -m "feat: enqueue videogen jobs through rabbitmq"
```

---

### Task 4: Add RabbitMQ Broker Smoke Test

**Files:**
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/tests/videogen_rabbitmq_smoke.rs`
- Create: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/scripts/smoke-videogen-rabbitmq.sh`

- [ ] **Step 1: Write ignored integration test**

Create an ignored test that only runs when `VIDEOGEN_RABBITMQ_AMQPS_URLS` is set:

```rust
#[tokio::test]
#[ignore = "requires deployed RabbitMQ broker"]
async fn publishes_to_videogen_exchange_with_confirm() {
    let urls = std::env::var("VIDEOGEN_RABBITMQ_AMQPS_URLS")
        .expect("VIDEOGEN_RABBITMQ_AMQPS_URLS required");

    let config = RabbitMqPublishConfig {
        amqps_urls: urls.split(',').map(|s| s.trim().to_string()).collect(),
        exchange: "videogen.jobs".to_string(),
        routing_key: "ltx.generate".to_string(),
        connection_name: "yral-video-storage-service-smoke-test".to_string(),
        publish_timeout_secs: 10,
        tls_ca_cert_pem_b64: std::env::var("VIDEOGEN_RABBITMQ_TLS_CA_CERT_PEM_B64").ok(),
    };

    let accepted = RabbitMqPublisher::new(config)
        .publish(sample_vast_submit_request())
        .await
        .unwrap();

    assert_eq!(accepted.status, "queued");
}
```

- [ ] **Step 2: Run normal tests**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test --test videogen_rabbitmq_smoke
```

Expected: test is ignored by default.

- [ ] **Step 3: Pre-check broker TLS identity**

Before using direct IP AMQPS URLs, verify the broker certificate contains the IP address as a SAN, or use a DNS hostname that matches the certificate.

Run:

```bash
openssl s_client -connect 94.130.13.115:5671 -servername 94.130.13.115 </dev/null 2>/dev/null \
  | openssl x509 -noout -subject -ext subjectAltName
```

Expected: certificate SANs include `IP Address:94.130.13.115`, or you have a DNS hostname in the SAN and will use that hostname in `VIDEOGEN_RABBITMQ_AMQPS_URLS`.

If the broker uses a private/self-signed CA, also export that CA as `VIDEOGEN_RABBITMQ_TLS_CA_CERT_PEM_B64` for the smoke test.

- [ ] **Step 4: Run broker smoke test manually**

Use a publisher URL with the real publisher password:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
VIDEOGEN_RABBITMQ_AMQPS_URLS='amqps://prakash_videogen_publisher:<password>@94.130.13.115:5671/%2Fvideogen' \
cargo test --test videogen_rabbitmq_smoke -- --ignored
```

Expected: publish confirm succeeds.

If TLS verification fails, do not disable TLS verification in code. Fix the broker certificate trust path or use hostnames/IP SANs that match the RabbitMQ certificate.

- [ ] **Step 5: Verify message visibility**

From a broker node:

```bash
ssh -i ~/.ssh/yral_onboarding_deploy deploy@94.130.13.115 \
  'docker exec rabbitmq-rabbitmq-1 rabbitmqctl list_queues -p /videogen name messages_ready messages_unacknowledged'
```

Expected: if no Vast consumer is running, `videogen.ltx.generate` `messages_ready` increases by one after the smoke publish. Purge the smoke message manually after the test if needed.

- [ ] **Step 6: Commit**

Create `scripts/smoke-videogen-rabbitmq.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail

: "${VIDEOGEN_RABBITMQ_AMQPS_URLS:?VIDEOGEN_RABBITMQ_AMQPS_URLS is required}"

cargo test --test videogen_rabbitmq_smoke -- --ignored
```

```bash
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service add tests/videogen_rabbitmq_smoke.rs scripts/smoke-videogen-rabbitmq.sh
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service commit -m "test: add videogen rabbitmq smoke test"
```

---

### Task 5: Implement Upload Destination Release Handling

**Files:**
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/upload_destination.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/routes/videogen/generate.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/routes/videogen/complete.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/reconcile.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/consts.rs`

- [ ] **Step 1: Write failing tests for explicit release behavior**

Add tests proving release is not an untracked runtime stub:

```rust
#[test]
fn no_release_endpoint_mode_is_explicit() {
    let client = UploadDestinationReleaseClient::disabled();
    assert_eq!(client.mode(), UploadDestinationReleaseMode::DisabledNoEndpoint);
}

#[test]
fn release_endpoint_payload_uses_video_id_and_object_key() {
    let request = ReleaseUploadDestinationRequest {
        request_key: request_key(),
        video_id: "video-17".to_string(),
        object_key: "generated/video-17.mp4".to_string(),
    };
    let body = request.to_json_body();

    assert_eq!(body["video_id"], "video-17");
    assert_eq!(body["object_key"], "generated/video-17.mp4");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test videogen::upload_destination
```

Expected: compile failure because release client types do not exist.

- [ ] **Step 3: Add explicit release client**

Add env constant:

```rust
pub const VIDEOGEN_UPLOAD_DESTINATION_RELEASE_URL: &str =
    "VIDEOGEN_UPLOAD_DESTINATION_RELEASE_URL";
```

Implement:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadDestinationReleaseMode {
    Endpoint,
    DisabledNoEndpoint,
}
```

Behavior:

- If `VIDEOGEN_UPLOAD_DESTINATION_RELEASE_URL` is set, POST JSON to it.
- If it is absent, return `Ok(())` but emit a structured log/metric with `mode=disabled_no_endpoint`.
- This makes "release when possible" explicit and prevents hidden production stubs.
- Never fail the user-facing request only because release cleanup failed; release remains best-effort.

Payload:

```json
{
  "request_key": { "principal": "...", "counter": 123 },
  "video_id": "...",
  "object_key": "..."
}
```

- [ ] **Step 4: Wire generate runtime release**

Replace `RuntimeGenerateDeps::release_upload_destination` no-op with the release client.

Use the `UploadDestination` already available in the generate failure path.

- [ ] **Step 5: Wire completion runtime release**

Replace `RuntimeCompletionDeps::release_upload_destination` no-op with the release client.

Prefer extending the trait to avoid an extra DB round trip. Change:

```rust
async fn release_upload_destination(
    &self,
    request_key: &RateLimiterRequestKey,
) -> Result<(), String>;
```

to:

```rust
async fn release_upload_destination(
    &self,
    request_key: &RateLimiterRequestKey,
    destination: &UploadDestination,
) -> Result<(), String>;
```

Then update `handle_failure_completion` so it loads the context state once, keeps the stored `upload_destination` from that row, and passes it into release. If `ContextStateRow` does not expose `upload_destination` today, extend that row and query result before calling release. Do not perform a second DB lookup only for cleanup metadata.

- [ ] **Step 6: Wire reconciliation runtime release**

Replace `RuntimeReconcileDeps::release_upload_destination` no-op with the release client.

Use stored context upload destination metadata from stale rows. If a stale row does not currently expose `video_id`/`object_key`, extend the stale-row query result instead of skipping release silently.

- [ ] **Step 7: Run tests**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test videogen::upload_destination
cargo test routes::videogen::generate
cargo test routes::videogen::complete
cargo test videogen::reconcile
```

Expected: tests pass and no runtime `release_upload_destination stub` string remains.

- [ ] **Step 8: Commit**

```bash
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service add src/videogen/upload_destination.rs src/routes/videogen/generate.rs src/routes/videogen/complete.rs src/videogen/reconcile.rs src/consts.rs
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service commit -m "feat: handle videogen upload destination release"
```

---

### Task 6: Move Shared RateLimiter Runtime Helpers Into `videogen/rate_limiter.rs`

**Files:**
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/rate_limiter.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/routes/videogen/generate.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/routes/videogen/complete.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/reconcile.rs`

- [ ] **Step 1: Write failing helper tests**

In `src/videogen/rate_limiter.rs`, add tests:

```rust
#[test]
fn canister_request_key_rejects_invalid_principal() {
    let key = RateLimiterRequestKey {
        principal: "not-a-principal".to_string(),
        counter: 7,
    };

    assert!(to_canister_request_key(&key).is_err());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test videogen::rate_limiter
```

Expected: compile failure because helper does not exist.

- [ ] **Step 3: Move helper out of generate route**

Move the private `canister_request_key` logic from `src/routes/videogen/generate.rs` into `src/videogen/rate_limiter.rs`:

```rust
pub fn to_canister_request_key(
    request_key: &RateLimiterRequestKey,
) -> Result<CanisterVideoGenRequestKey, RateLimiterError> {
    Ok(CanisterVideoGenRequestKey {
        principal: Principal::from_text(&request_key.principal)
            .map_err(|error| RateLimiterError::Rejected(error.to_string()))?,
        counter: request_key.counter,
    })
}
```

Also add small async helper functions if duplication remains reasonable:

- `mark_failed(rate_limits, request_key, reason)`
- `mark_complete(rate_limits, request_key, bucket_url)`
- `decrement_counter(rate_limits, request_key, property)`

Keep helper code small. Do not introduce a large service abstraction unless repeated code becomes hard to read.

- [ ] **Step 4: Update generate to use shared helper**

Replace local helper usage in `src/routes/videogen/generate.rs`.

- [ ] **Step 5: Run tests**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test videogen::rate_limiter
cargo test routes::videogen::generate
```

Expected: helper and generate tests pass.

- [ ] **Step 6: Commit**

```bash
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service add src/videogen/rate_limiter.rs src/routes/videogen/generate.rs
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service commit -m "refactor: share videogen rate limiter helpers"
```

---

### Task 7: Replace Completion RateLimiter Stubs

**Files:**
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/routes/videogen/complete.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/rate_limiter.rs`

- [ ] **Step 1: Add tests for runtime completion side effects**

Existing `CompletionDeps` tests prove the generic handler calls dependency methods. Add tests for the runtime helper functions in `rate_limiter.rs` instead of trying to hit the real IC canister.

Add a test that validates conversion and status construction:

```rust
#[test]
fn complete_status_uses_bucket_url_payload() {
    let status = complete_status("https://bucket.example/video.mp4");
    match status {
        VideoGenRequestStatus::Complete(url) => assert_eq!(url, "https://bucket.example/video.mp4"),
        _ => panic!("expected complete status"),
    }
}
```

- [ ] **Step 2: Implement real completion RateLimiter calls**

In `RuntimeCompletionDeps`:

- `mark_rate_limit_complete(request_key)` must call `update_video_generation_status(key, Complete(bucket_url))`.
- The trait currently lacks `bucket_url` in `mark_rate_limit_complete`; change it to:

```rust
async fn mark_rate_limit_complete(
    &self,
    request_key: &RateLimiterRequestKey,
    bucket_url: &str,
) -> Result<(), String>;
```

Update `handle_success_completion` to pass `bucket_url`.

- `mark_rate_limit_failed(request_key, reason)` must call `update_video_generation_status(key, Failed(reason))`.
- `decrement_rate_limit(request_key)` must call `decrement_video_generation_counter_v_1(key, "VIDEOGEN")`.

Use the same canister and property as generate:

```rust
let rate_limits = RateLimits(*RATE_LIMITS_CANISTER_ID, &self.ic_agent);
```

- [ ] **Step 3: Adjust fake deps in tests**

Update fake `CompletionDeps` implementations for the new `mark_rate_limit_complete` signature.

- [ ] **Step 4: Run tests**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test routes::videogen::complete
cargo test videogen::rate_limiter
```

Expected: completion tests pass and no runtime RateLimiter stubs remain in `complete.rs`.

- [ ] **Step 5: Commit**

```bash
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service add src/routes/videogen/complete.rs src/videogen/rate_limiter.rs
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service commit -m "feat: update rate limiter on videogen completion"
```

---

### Task 8: Replace Reconciliation RateLimiter Stubs

**Files:**
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/reconcile.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/rate_limiter.rs`

- [ ] **Step 1: Add tests that runtime code no longer contains stub markers**

Add a lightweight test in `src/videogen/reconcile.rs`:

```rust
#[test]
fn runtime_reconcile_deps_uses_real_rate_limiter_paths() {
    let source = include_str!("reconcile.rs");
    assert!(!source.contains("mark_rate_limit_failed stub"));
    assert!(!source.contains("decrement_rate_limit stub"));
    assert!(!source.contains("mark_rate_limit_complete stub"));
}
```

This is not a substitute for behavior tests; it prevents accidental production no-ops.

- [ ] **Step 2: Run test to verify it fails**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test videogen::reconcile::tests::runtime_reconcile_deps_uses_real_rate_limiter_paths
```

Expected: fails because stub markers exist.

- [ ] **Step 3: Implement real reconciler RateLimiter calls**

In `RuntimeReconcileDeps`:

- `mark_rate_limit_failed` calls canister `Failed(reason)`.
- `decrement_rate_limit` calls `decrement_video_generation_counter_v_1(key, "VIDEOGEN")`.
- `mark_rate_limit_complete` calls canister `Complete(bucket_url)`.

Preserve the spec invariant already implemented in reconciliation:

- If `mark_rate_limit_failed` returns error, do not terminalize Postgres.
- Record reconciliation error.
- Do not bump retry budget for canister-unavailable skips.

- [ ] **Step 4: Run reconciliation tests**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test videogen::reconcile
```

Expected: reconciliation tests pass and runtime stubs are gone.

- [ ] **Step 5: Commit**

```bash
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service add src/videogen/reconcile.rs src/videogen/rate_limiter.rs
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service commit -m "feat: update rate limiter during videogen reconciliation"
```

---

### Task 9: Implement Real Draft Metadata Client

**Files:**
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/draft.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/routes/videogen/complete.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/context.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/videogen/reconcile.rs`
- Modify: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/src/consts.rs`

This task is independently blockable. Tasks 1-8 can be implemented and tested without the upload metadata contract, and RabbitMQ publishing can remain behind `VIDEOGEN_VAST_SUBMIT_TRANSPORT=rabbitmq` in staging. Production traffic must not switch to the migrated flow until this task is complete or the team explicitly accepts draft creation staying disabled.

- [ ] **Step 1: Confirm upload metadata contract before code**

Use the current off-chain implementation as the reference contract:

Path:

```text
POST {VIDEOGEN_UPLOAD_SERVICE_URL}/update-video-metadata
```

Body:

```json
{
  "delegated_identity_wire": {},
  "meta": {},
  "post_details": {
    "id": "<video_id>",
    "video_uid": "<video_id>",
    "creator_principal": "<principal>",
    "status": "Draft",
    "hashtags": [],
    "description": ""
  }
}
```

Before implementation, confirm with the upload-service owner:

- `/update-video-metadata` accepts a video that was already uploaded by Vast to the reserved `video_id`.
- The endpoint is idempotent on `video_id` or an idempotency key.
- Passing `Idempotency-Key: <request_id>` is accepted or ignored harmlessly.
- No second media upload is required.

If this contract is not confirmed, stop this task and leave `LoggingDraftServiceClient` in place. RabbitMQ publishing can still ship behind config, but production traffic must not be switched until draft metadata is real.

- [ ] **Step 2: Write failing draft client tests**

Use a mock HTTP server or a trait boundary. Tests must prove:

- `UploadMetadataDraftServiceClient` posts to `/update-video-metadata`.
- Body contains `delegated_identity_wire`.
- Body contains `post_details.status = "Draft"`.
- Body uses the existing `video_id`; it does not call `/get-upload-url`.
- Debug output does not print delegated identity.

- [ ] **Step 3: Extend `DraftCreationRequest`**

Add:

```rust
pub delegated_identity: Option<yral_types::delegated_identity::DelegatedIdentityWire>,
```

Reason: completion must create a user draft, and upload metadata requires delegated identity.

- [ ] **Step 4: Return encrypted identity from context claim**

Update `CompletionContextRow` and `claim_for_completion` to include:

```rust
pub encrypted_identity: Option<EncryptedDelegatedIdentity>,
```

The SQL `RETURNING` clause must include:

- `encryption_key_id`
- `encrypted_identity_nonce`
- `encrypted_identity_ciphertext`

Map nulls to `None`.

- [ ] **Step 5: Decrypt identity in completion**

Add a `CompletionDeps` method:

```rust
fn decrypt_delegated_identity(
    &self,
    encrypted: &EncryptedDelegatedIdentity,
) -> Result<yral_types::delegated_identity::DelegatedIdentityWire, String>;
```

Runtime implementation:

- parse `VIDEOGEN_IDENTITY_ENCRYPTION_KEYS`
- use `encrypted.encryption_key_id`
- decrypt bytes
- deserialize `DelegatedIdentityWire`

Failure behavior:

- return `503`
- do not create draft
- leave state recoverable according to existing draft retry/reconciliation policy

- [ ] **Step 6: Implement upload metadata draft client**

Replace `LoggingDraftServiceClient` with:

```rust
pub struct UploadMetadataDraftServiceClient {
    base_url: String,
    http: reqwest::Client,
}
```

Request:

```rust
let update_metadata_url = format!("{}/update-video-metadata", base_url.trim_end_matches('/'));
let body = json!({
    "delegated_identity_wire": identity,
    "meta": {},
    "post_details": {
        "id": request.video_id,
        "video_uid": request.video_id,
        "creator_principal": request.user_principal,
        "status": "Draft",
        "hashtags": Vec::<String>::new(),
        "description": ""
    }
});
```

Headers:

- `Content-Type: application/json`
- `Idempotency-Key: request.request_id`

Map non-2xx to `DraftServiceError::Rejected` with sanitized status/message.

- [ ] **Step 7: Use real client in completion and reconciliation**

In `RuntimeCompletionDeps::create_draft`, call `UploadMetadataDraftServiceClient`.

In `RuntimeReconcileDeps::create_draft_for_upload`, use the same real client once stale rows include enough encrypted identity context. If stale rows do not include encrypted identity today, extend `StaleDraftCreatingRow`/`StaleUploadedRow` query results.

- [ ] **Step 8: Run tests**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test videogen::draft
cargo test routes::videogen::complete
cargo test videogen::reconcile
```

Expected: draft client, completion, and reconciliation tests pass.

- [ ] **Step 9: Commit**

```bash
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service add src/videogen/draft.rs src/routes/videogen/complete.rs src/videogen/context.rs src/videogen/reconcile.rs src/consts.rs
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service commit -m "feat: create videogen drafts from completion"
```

---

### Task 10: End-To-End Generate Path Verification

**Files:**
- Modify if needed: `/Users/prk-jr/Desktop/work/dolr/yral-video-storage-service/docs/superpowers/plans/2026-06-03-videogen-rabbitmq-publisher.md`

- [ ] **Step 1: Run full targeted test suite**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo test videogen
cargo test routes::videogen
```

Expected: all videogen tests pass.

- [ ] **Step 2: Run compile check**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo check
```

Expected: compile succeeds.

- [ ] **Step 3: Run formatting/lint baseline**

Run:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: no formatting or clippy failures. If the repo has pre-existing clippy failures unrelated to this work, document them exactly and do not hide new failures.

- [ ] **Step 4: Run RabbitMQ smoke publish**

Run the ignored smoke test with a publisher URL:

```bash
cd /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service
VIDEOGEN_RABBITMQ_AMQPS_URLS='amqps://prakash_videogen_publisher:<password>@94.130.13.115:5671/%2Fvideogen' \
cargo test --test videogen_rabbitmq_smoke -- --ignored
```

Expected: publish confirm succeeds.

- [ ] **Step 5: Verify mobile-facing behavior remains unchanged**

Check that the generate response still has:

```json
{
  "operation_id": "<principal>_<counter>",
  "provider": "Ltx2",
  "request_key": {
    "principal": "...",
    "counter": 123
  }
}
```

No mobile DTO change should be required.

- [ ] **Step 6: Commit final verification docs if needed**

Only commit docs if the plan or runbook changed:

```bash
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service add docs/superpowers/plans/2026-06-03-videogen-rabbitmq-publisher.md
git -C /Users/prk-jr/Desktop/work/dolr/yral-video-storage-service commit -m "docs: add videogen rabbitmq implementation plan"
```

---

## Required Environment For RabbitMQ Mode

For staging/production RabbitMQ submit mode:

```text
VIDEOGEN_VAST_SUBMIT_TRANSPORT=rabbitmq
VIDEOGEN_RABBITMQ_AMQPS_URLS=amqps://prakash_videogen_publisher:<password>@94.130.13.115:5671/%2Fvideogen,amqps://prakash_videogen_publisher:<password>@88.99.151.102:5671/%2Fvideogen,amqps://prakash_videogen_publisher:<password>@138.201.129.173:5671/%2Fvideogen
VIDEOGEN_RABBITMQ_EXCHANGE=videogen.jobs
VIDEOGEN_RABBITMQ_ROUTING_KEY=ltx.generate
VIDEOGEN_RABBITMQ_PUBLISH_TIMEOUT_SECS=10
VIDEOGEN_RABBITMQ_CONNECTION_NAME=yral-video-storage-service-videogen-publisher
VIDEOGEN_RABBITMQ_TLS_CA_CERT_PEM_B64=<optional base64 PEM CA if broker cert is private/self-signed>
```

TLS note:

- Do not disable TLS verification in code.
- If direct IP URLs are used, the RabbitMQ certificate must include those IPs as SANs.
- If the certificate uses DNS SANs, use DNS hostnames in `VIDEOGEN_RABBITMQ_AMQPS_URLS`.
- If the broker certificate is private/self-signed, set `VIDEOGEN_RABBITMQ_TLS_CA_CERT_PEM_B64` or install the CA in the runtime trust store.

Existing required envs still apply:

```text
MODERATION_MODE=remote
MODERATION_SERVICE_URL=<moderation service URL>
VIDEOGEN_UPLOAD_SERVICE_URL=<upload service URL>
VIDEOGEN_IDENTITY_ENCRYPTION_KEYS=<key-id:base64-32-byte-key>
VIDEOGEN_IDENTITY_ACTIVE_KEY_ID=<key-id>
VIDEOGEN_COMPLETION_HMAC_KEYS=<key-id:secret>
PRAKASH_PUBLIC_BASE_URL=<public callback base URL>
```

Legacy HTTP rollback mode:

```text
VIDEOGEN_VAST_SUBMIT_TRANSPORT=http
VIDEOGEN_VAST_GENERATE_URL=<legacy Vast HTTP submit URL>
VAST_API_KEY=<legacy Vast API key>
```

---

## Rollout Gates

- RabbitMQ broker remains verified:
  - 3 running nodes.
  - quorum queue has 3 voters.
  - `/videogen` vhost exists.
- RabbitMQ publish smoke test passes from the Prakash runtime environment.
- Vast worker can consume and ack from `videogen.ltx.generate`.
- Vast worker handles duplicate `request_id` idempotently.
- Vast worker signs completion callbacks with a key in `VIDEOGEN_COMPLETION_HMAC_KEYS`.
- Upload metadata draft service contract is confirmed and tested.
- Completion RateLimiter runtime stubs are removed.
- Reconciliation RateLimiter runtime stubs are removed.
- `LoggingDraftServiceClient` is not used in production mode.

## Rollback

Keep HTTP submit mode until RabbitMQ and Vast consumer are proven in staging:

```text
VIDEOGEN_VAST_SUBMIT_TRANSPORT=http
```

Rollback must move the submit transport and the Vast callback behavior together. Do not publish to RabbitMQ while Vast still calls an old off-chain callback URL.
