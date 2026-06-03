use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use ic_agent::Agent;
use tokio::sync::Mutex;

use crate::{
    db,
    videogen::{
        config::VideogenConfig,
        context::{
            ContextStoreError, PostgresVideogenContextStore, StaleDraftCreatedRow,
            StaleDraftCreatingRow, StaleRow, StaleUploadedRow,
        },
        draft::{DraftCreationRequest, LoggingDraftServiceClient},
        rate_limiter::RateLimiterRequestKey,
    },
};

// ---------------------------------------------------------------------------
// Config snapshot for the reconciler
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ReconcileConfig {
    pub batch_size: i64,
    pub context_created_timeout_secs: u64,
    pub ltx_generation_timeout_secs: u64,
    pub draft_create_timeout_secs: u64,
    pub draft_created_complete_timeout_secs: u64,
    pub draft_create_max_attempts: u32,
}

impl ReconcileConfig {
    pub fn from_videogen_config(cfg: &VideogenConfig) -> Self {
        Self {
            batch_size: cfg.reconciliation_batch_size as i64,
            context_created_timeout_secs: cfg.context_created_timeout_secs,
            ltx_generation_timeout_secs: cfg.ltx_generation_timeout_secs,
            draft_create_timeout_secs: cfg.draft_create_timeout_secs,
            draft_created_complete_timeout_secs: cfg.draft_created_complete_timeout_secs,
            draft_create_max_attempts: cfg.draft_create_max_attempts,
        }
    }
}

// ---------------------------------------------------------------------------
// Dependency injection trait
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
pub trait ReconcileDeps: Send + Sync {
    async fn fetch_stale_context_created(
        &self,
        before: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<StaleRow>, ContextStoreError>;
    async fn fetch_stale_submitted(
        &self,
        before: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<StaleRow>, ContextStoreError>;
    async fn fetch_stale_uploaded(
        &self,
        before: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<StaleUploadedRow>, ContextStoreError>;
    async fn fetch_stale_draft_creating(
        &self,
        before: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<StaleDraftCreatingRow>, ContextStoreError>;
    async fn fetch_stale_draft_created(
        &self,
        before: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<StaleDraftCreatedRow>, ContextStoreError>;

    async fn mark_stale_failed(
        &self,
        request_key: &RateLimiterRequestKey,
        from_states: &[&str],
    ) -> Result<bool, ContextStoreError>;
    async fn record_reconciliation_error(
        &self,
        request_key: &RateLimiterRequestKey,
        error: &str,
    ) -> Result<(), ContextStoreError>;

    /// Mark the canister entry as Failed. If this returns Err the row should
    /// NOT be terminalized — the canister may be temporarily unavailable.
    async fn mark_rate_limit_failed(
        &self,
        request_key: &RateLimiterRequestKey,
        reason: &str,
    ) -> Result<(), String>;

    /// Decrement the VIDEOGEN counter on the canister. Best-effort.
    async fn decrement_rate_limit(&self, request_key: &RateLimiterRequestKey)
        -> Result<(), String>;

    /// Mark the canister entry as Complete. Best-effort.
    async fn mark_rate_limit_complete(
        &self,
        request_key: &RateLimiterRequestKey,
        bucket_url: &str,
    ) -> Result<(), String>;

    /// Release the reserved upload slot/URL. Best-effort.
    async fn release_upload_destination(
        &self,
        request_key: &RateLimiterRequestKey,
        destination: Option<&crate::videogen::upload_destination::UploadDestination>,
    ) -> Result<(), String>;

    /// Redact the encrypted identity from the row. Best-effort.
    async fn redact_identity(
        &self,
        request_key: &RateLimiterRequestKey,
    ) -> Result<(), ContextStoreError>;

    /// Count contexts grouped by state for gauge metrics. Best-effort.
    async fn count_contexts_by_state(&self) -> Result<Vec<(String, i64)>, String>;

    /// Attempt to create a draft for an already-uploaded video. Best-effort.
    async fn create_draft_for_upload(&self, row: &StaleDraftCreatingRow) -> Result<(), String>;

    async fn increment_draft_attempts(
        &self,
        request_key: &RateLimiterRequestKey,
    ) -> Result<(), ContextStoreError>;
    async fn mark_draft_creating(
        &self,
        request_key: &RateLimiterRequestKey,
    ) -> Result<(), ContextStoreError>;
    async fn mark_draft_failed(
        &self,
        request_key: &RateLimiterRequestKey,
        reason: &str,
    ) -> Result<(), ContextStoreError>;
    async fn mark_complete_with_bucket_url(
        &self,
        request_key: &RateLimiterRequestKey,
        bucket_url: &str,
    ) -> Result<(), ContextStoreError>;
}

// ---------------------------------------------------------------------------
// Per-state reconciliation helpers
// ---------------------------------------------------------------------------

/// Shared cleanup sequence for `context_created` and `submitted` stale rows.
///
/// Invariant: NEVER terminalize Postgres before `mark_rate_limit_failed` succeeds.
async fn handle_stale_failed_row<D: ReconcileDeps>(deps: &D, row: &StaleRow, from_state: &str) {
    let key = &row.request_key;

    // Step 1 — must succeed before we can terminalize.
    if let Err(error) = deps
        .mark_rate_limit_failed(key, &format!("stale {from_state}"))
        .await
    {
        tracing::warn!(
            principal = %key.principal,
            counter = key.counter,
            state = from_state,
            error = %error,
            "reconcile: rate-limiter unavailable, skipping row"
        );
        let _ = deps.record_reconciliation_error(key, &error).await;
        return; // do NOT call mark_stale_failed
    }

    // Step 2 — best-effort counter decrement.
    if let Err(error) = deps.decrement_rate_limit(key).await {
        tracing::warn!(
            principal = %key.principal,
            counter = key.counter,
            "reconcile: decrement_rate_limit failed (best-effort): {error}"
        );
    }

    // Step 3 — best-effort release of upload slot.
    if let Err(error) = deps
        .release_upload_destination(key, row.upload_destination.as_ref())
        .await
    {
        tracing::warn!(
            principal = %key.principal,
            counter = key.counter,
            "reconcile: release_upload_destination failed (best-effort): {error}"
        );
    }

    // Step 4 — best-effort identity redaction.
    if let Err(error) = deps.redact_identity(key).await {
        tracing::warn!(
            principal = %key.principal,
            counter = key.counter,
            "reconcile: redact_identity failed (best-effort): {error}"
        );
    }

    // Step 5 — atomically terminalize the Postgres row.
    match deps.mark_stale_failed(key, &[from_state]).await {
        Ok(true) => {
            metrics::counter!(crate::videogen::metrics::RECONCILIATION_ACTIONS_TOTAL).increment(1);
            tracing::info!(
                principal = %key.principal,
                counter = key.counter,
                from_state = from_state,
                "reconcile: row transitioned to stale_failed"
            );
        }
        Ok(false) => {
            tracing::info!(
                principal = %key.principal,
                counter = key.counter,
                "reconcile: row already moved on, skipping stale_failed transition"
            );
        }
        Err(error) => {
            tracing::error!(
                principal = %key.principal,
                counter = key.counter,
                error = %error,
                "reconcile: failed to mark_stale_failed"
            );
        }
    }
}

async fn reconcile_context_created<D: ReconcileDeps>(deps: &D, config: &ReconcileConfig) {
    let before = Utc::now() - Duration::seconds(config.context_created_timeout_secs as i64);
    let rows = match deps
        .fetch_stale_context_created(before, config.batch_size)
        .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::error!("reconcile: fetch_stale_context_created failed: {error}");
            return;
        }
    };

    for row in &rows {
        handle_stale_failed_row(deps, row, "context_created").await;
    }
}

async fn reconcile_submitted<D: ReconcileDeps>(deps: &D, config: &ReconcileConfig) {
    let before = Utc::now() - Duration::seconds(config.ltx_generation_timeout_secs as i64);
    let rows = match deps.fetch_stale_submitted(before, config.batch_size).await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::error!("reconcile: fetch_stale_submitted failed: {error}");
            return;
        }
    };

    for row in &rows {
        handle_stale_failed_row(deps, row, "submitted").await;
    }
}

async fn reconcile_uploaded<D: ReconcileDeps>(deps: &D, config: &ReconcileConfig) {
    let before = Utc::now() - Duration::seconds(config.draft_create_timeout_secs as i64);
    let rows = match deps.fetch_stale_uploaded(before, config.batch_size).await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::error!("reconcile: fetch_stale_uploaded failed: {error}");
            return;
        }
    };

    for row in &rows {
        let key = &row.request_key;
        if let Err(error) = deps.mark_draft_creating(key).await {
            tracing::warn!(
                principal = %key.principal,
                counter = key.counter,
                error = %error,
                "reconcile: mark_draft_creating failed"
            );
            let _ = deps
                .record_reconciliation_error(key, &error.to_string())
                .await;
        } else {
            metrics::counter!(crate::videogen::metrics::RECONCILIATION_ACTIONS_TOTAL).increment(1);
            tracing::info!(
                principal = %key.principal,
                counter = key.counter,
                "reconcile: uploaded row moved to draft_creating"
            );
        }
    }
}

async fn reconcile_draft_creating<D: ReconcileDeps>(deps: &D, config: &ReconcileConfig) {
    let before = Utc::now() - Duration::seconds(config.draft_create_timeout_secs as i64);
    let rows = match deps
        .fetch_stale_draft_creating(before, config.batch_size)
        .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::error!("reconcile: fetch_stale_draft_creating failed: {error}");
            return;
        }
    };

    for row in &rows {
        let key = &row.request_key;

        if row.draft_attempts >= config.draft_create_max_attempts as i32 {
            // Max attempts reached — terminalize canister FIRST, then Postgres.
            // Invariant: NEVER terminalize Postgres before canister succeeds.
            let reason = "Draft creation failed after video upload";
            if let Err(error) = deps.mark_rate_limit_failed(key, reason).await {
                tracing::warn!(
                    principal = %key.principal,
                    counter = key.counter,
                    error = %error,
                    "reconcile: rate-limiter unavailable for draft_failed, skipping row"
                );
                let _ = deps.record_reconciliation_error(key, &error).await;
                // Do NOT call mark_draft_failed — canister may be temporarily unavailable.
            } else if let Err(error) = deps.mark_draft_failed(key, reason).await {
                tracing::error!(
                    principal = %key.principal,
                    counter = key.counter,
                    error = %error,
                    "reconcile: mark_draft_failed failed"
                );
            } else {
                metrics::counter!(crate::videogen::metrics::RECONCILIATION_ACTIONS_TOTAL)
                    .increment(1);
            }
        } else {
            // Retry draft creation.
            if let Err(error) = deps.create_draft_for_upload(row).await {
                tracing::warn!(
                    principal = %key.principal,
                    counter = key.counter,
                    error = %error,
                    "reconcile: create_draft_for_upload failed"
                );
                let _ = deps.record_reconciliation_error(key, &error).await;
            } else {
                metrics::counter!(crate::videogen::metrics::RECONCILIATION_ACTIONS_TOTAL)
                    .increment(1);
            }
            if let Err(error) = deps.increment_draft_attempts(key).await {
                tracing::warn!(
                    principal = %key.principal,
                    counter = key.counter,
                    error = %error,
                    "reconcile: increment_draft_attempts failed"
                );
            }
        }
    }
}

async fn reconcile_draft_created<D: ReconcileDeps>(deps: &D, config: &ReconcileConfig) {
    let before = Utc::now() - Duration::seconds(config.draft_created_complete_timeout_secs as i64);
    let rows = match deps
        .fetch_stale_draft_created(before, config.batch_size)
        .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::error!("reconcile: fetch_stale_draft_created failed: {error}");
            return;
        }
    };

    for row in &rows {
        let key = &row.request_key;
        let bucket_url = match &row.bucket_url {
            Some(url) => url.clone(),
            None => {
                tracing::warn!(
                    principal = %key.principal,
                    counter = key.counter,
                    "reconcile: draft_created row missing bucket_url, skipping"
                );
                continue;
            }
        };

        // Retry RateLimiter Complete.
        if let Err(error) = deps.mark_rate_limit_complete(key, &bucket_url).await {
            tracing::warn!(
                principal = %key.principal,
                counter = key.counter,
                error = %error,
                "reconcile: mark_rate_limit_complete failed (best-effort)"
            );
            let _ = deps.record_reconciliation_error(key, &error).await;
        }

        if let Err(error) = deps.mark_complete_with_bucket_url(key, &bucket_url).await {
            tracing::error!(
                principal = %key.principal,
                counter = key.counter,
                error = %error,
                "reconcile: mark_complete_with_bucket_url failed"
            );
        } else {
            metrics::counter!(crate::videogen::metrics::RECONCILIATION_ACTIONS_TOTAL).increment(1);
            tracing::info!(
                principal = %key.principal,
                counter = key.counter,
                "reconcile: draft_created row transitioned to complete"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Main reconciliation entry point
// ---------------------------------------------------------------------------

pub async fn run_reconciliation_cycle<D: ReconcileDeps>(deps: &D, config: &ReconcileConfig) {
    if let Ok(counts) = deps.count_contexts_by_state().await {
        for (state, count) in counts {
            metrics::gauge!(
                crate::videogen::metrics::CONTEXTS_BY_STATE,
                "state" => state
            )
            .set(count as f64);
        }
    }
    reconcile_context_created(deps, config).await;
    reconcile_submitted(deps, config).await;
    reconcile_uploaded(deps, config).await;
    reconcile_draft_creating(deps, config).await;
    reconcile_draft_created(deps, config).await;
}

// ---------------------------------------------------------------------------
// Runtime implementation
// ---------------------------------------------------------------------------

/// A concrete `ReconcileDeps` implementation backed by Postgres and IC canister
/// stubs (the canister calls are best-effort no-ops until the IC wiring is done).
///
/// The `db_client` is lazily connected and reused across all operations within a
/// reconciliation cycle, avoiding a new TCP connection per DB call.
pub struct RuntimeReconcileDeps {
    pub db_url: String,
    #[allow(dead_code)] // TODO: wire canister calls (Task 7 stub)
    pub ic_agent: Agent,
    /// Lazily-initialised, cycle-wide Postgres connection.
    db_client: Arc<Mutex<Option<tokio_postgres::Client>>>,
}

impl RuntimeReconcileDeps {
    pub fn new(db_url: String, ic_agent: Agent) -> Self {
        Self {
            db_url,
            ic_agent,
            db_client: Arc::new(Mutex::new(None)),
        }
    }

    /// Returns a store wrapping the shared client, reconnecting if the connection
    /// has been closed (e.g. after a Postgres restart between cycles).
    async fn context_store(&self) -> Result<PostgresVideogenContextStore, ContextStoreError> {
        let mut guard = self.db_client.lock().await;
        let needs_reconnect = guard.as_ref().map(|c| c.is_closed()).unwrap_or(true);
        if needs_reconnect {
            let client = db::connect(&self.db_url)
                .await
                .map_err(|e| ContextStoreError::Unavailable(e.to_string()))?;
            *guard = Some(client);
        }
        // SAFETY: we just ensured the Option is Some above.
        Ok(PostgresVideogenContextStore::from_shared(
            self.db_client.clone(),
        ))
    }
}

#[async_trait::async_trait]
impl ReconcileDeps for RuntimeReconcileDeps {
    async fn fetch_stale_context_created(
        &self,
        before: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<StaleRow>, ContextStoreError> {
        self.context_store()
            .await?
            .fetch_stale_context_created(before, limit)
            .await
    }

    async fn fetch_stale_submitted(
        &self,
        before: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<StaleRow>, ContextStoreError> {
        self.context_store()
            .await?
            .fetch_stale_submitted(before, limit)
            .await
    }

    async fn fetch_stale_uploaded(
        &self,
        before: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<StaleUploadedRow>, ContextStoreError> {
        self.context_store()
            .await?
            .fetch_stale_uploaded(before, limit)
            .await
    }

    async fn fetch_stale_draft_creating(
        &self,
        before: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<StaleDraftCreatingRow>, ContextStoreError> {
        self.context_store()
            .await?
            .fetch_stale_draft_creating(before, limit)
            .await
    }

    async fn fetch_stale_draft_created(
        &self,
        before: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<StaleDraftCreatedRow>, ContextStoreError> {
        self.context_store()
            .await?
            .fetch_stale_draft_created(before, limit)
            .await
    }

    async fn mark_stale_failed(
        &self,
        request_key: &RateLimiterRequestKey,
        from_states: &[&str],
    ) -> Result<bool, ContextStoreError> {
        self.context_store()
            .await?
            .mark_stale_failed(request_key, from_states)
            .await
    }

    async fn record_reconciliation_error(
        &self,
        request_key: &RateLimiterRequestKey,
        error: &str,
    ) -> Result<(), ContextStoreError> {
        self.context_store()
            .await?
            .record_reconciliation_error(request_key, error)
            .await
    }

    async fn mark_rate_limit_failed(
        &self,
        _request_key: &RateLimiterRequestKey,
        _reason: &str,
    ) -> Result<(), String> {
        // TODO: wire actual IC canister call (uses self.ic_agent)
        tracing::info!("reconcile: mark_rate_limit_failed stub");
        Ok(())
    }

    async fn decrement_rate_limit(
        &self,
        _request_key: &RateLimiterRequestKey,
    ) -> Result<(), String> {
        // TODO: wire actual IC canister call
        tracing::info!("reconcile: decrement_rate_limit stub");
        Ok(())
    }

    async fn mark_rate_limit_complete(
        &self,
        _request_key: &RateLimiterRequestKey,
        _bucket_url: &str,
    ) -> Result<(), String> {
        // TODO: wire actual IC canister call
        tracing::info!("reconcile: mark_rate_limit_complete stub");
        Ok(())
    }

    async fn release_upload_destination(
        &self,
        request_key: &RateLimiterRequestKey,
        destination: Option<&crate::videogen::upload_destination::UploadDestination>,
    ) -> Result<(), String> {
        use crate::videogen::upload_destination::{
            ReleaseUploadDestinationRequest, UploadDestinationReleaseClient,
        };
        let Some(dest) = destination else {
            tracing::info!(
                principal = %request_key.principal,
                counter = request_key.counter,
                mode = "no_destination",
                "reconcile: release_upload_destination skipped (no destination)"
            );
            return Ok(());
        };
        UploadDestinationReleaseClient::from_env()
            .release(ReleaseUploadDestinationRequest {
                request_key: request_key.clone(),
                video_id: dest.video_id.clone(),
                object_key: dest.object_key.clone(),
            })
            .await
    }

    async fn redact_identity(
        &self,
        request_key: &RateLimiterRequestKey,
    ) -> Result<(), ContextStoreError> {
        self.context_store()
            .await?
            .redact_identity(request_key)
            .await
    }

    async fn count_contexts_by_state(&self) -> Result<Vec<(String, i64)>, String> {
        self.context_store()
            .await
            .map_err(|e| e.to_string())?
            .count_contexts_by_state()
            .await
            .map_err(|e| e.to_string())
    }

    async fn create_draft_for_upload(&self, row: &StaleDraftCreatingRow) -> Result<(), String> {
        use crate::videogen::draft::DraftServiceClient;
        let video_id = row
            .video_id
            .clone()
            .ok_or_else(|| "missing video_id on stale draft_creating row".to_string())?;
        let object_key = row
            .object_key
            .clone()
            .ok_or_else(|| "missing object_key on stale draft_creating row".to_string())?;
        let req = DraftCreationRequest {
            request_id: row.operation_id.clone(),
            request_key: row.request_key.clone(),
            user_principal: row.request_key.principal.clone(),
            video_id,
            object_key,
        };
        LoggingDraftServiceClient
            .create_draft(req)
            .await
            .map_err(|e| e.to_string())
    }

    async fn increment_draft_attempts(
        &self,
        request_key: &RateLimiterRequestKey,
    ) -> Result<(), ContextStoreError> {
        self.context_store()
            .await?
            .increment_draft_attempts(request_key)
            .await
    }

    async fn mark_draft_creating(
        &self,
        request_key: &RateLimiterRequestKey,
    ) -> Result<(), ContextStoreError> {
        self.context_store()
            .await?
            .mark_draft_creating(request_key)
            .await
    }

    async fn mark_draft_failed(
        &self,
        request_key: &RateLimiterRequestKey,
        reason: &str,
    ) -> Result<(), ContextStoreError> {
        self.context_store()
            .await?
            .mark_draft_failed(request_key, reason)
            .await
    }

    async fn mark_complete_with_bucket_url(
        &self,
        request_key: &RateLimiterRequestKey,
        bucket_url: &str,
    ) -> Result<(), ContextStoreError> {
        self.context_store()
            .await?
            .mark_complete_with_bucket_url(request_key, bucket_url)
            .await
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    // -----------------------------------------------------------------------
    // Fake deps
    // -----------------------------------------------------------------------

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        FetchStaleContextCreated,
        FetchStaleSubmitted,
        FetchStaleUploaded,
        FetchStaleDraftCreating,
        FetchStaleDraftCreated,
        MarkStaleFailed(String),
        RecordReconciliationError(String),
        MarkRateLimitFailed(String),
        DecrementRateLimit,
        MarkRateLimitComplete,
        ReleaseUploadDestination,
        RedactIdentity,
        CreateDraftForUpload,
        IncrementDraftAttempts,
        MarkDraftCreating,
        MarkDraftFailed,
        MarkCompleteWithBucketUrl,
    }

    type CallLog = Arc<Mutex<Vec<Call>>>;

    #[derive(Default)]
    struct FakeReconcileDeps {
        calls: CallLog,
        stale_context_created: Vec<StaleRow>,
        stale_submitted: Vec<StaleRow>,
        stale_uploaded: Vec<StaleUploadedRow>,
        stale_draft_creating: Vec<StaleDraftCreatingRow>,
        stale_draft_created: Vec<StaleDraftCreatedRow>,
        /// If set, mark_rate_limit_failed returns this error string.
        rate_limit_failed_error: Option<String>,
    }

    impl FakeReconcileDeps {
        fn push(&self, call: Call) {
            self.calls.lock().unwrap().push(call);
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl ReconcileDeps for FakeReconcileDeps {
        async fn fetch_stale_context_created(
            &self,
            _before: DateTime<Utc>,
            limit: i64,
        ) -> Result<Vec<StaleRow>, ContextStoreError> {
            self.push(Call::FetchStaleContextCreated);
            Ok(self
                .stale_context_created
                .iter()
                .take(limit as usize)
                .cloned()
                .collect())
        }

        async fn fetch_stale_submitted(
            &self,
            _before: DateTime<Utc>,
            limit: i64,
        ) -> Result<Vec<StaleRow>, ContextStoreError> {
            self.push(Call::FetchStaleSubmitted);
            Ok(self
                .stale_submitted
                .iter()
                .take(limit as usize)
                .cloned()
                .collect())
        }

        async fn fetch_stale_uploaded(
            &self,
            _before: DateTime<Utc>,
            limit: i64,
        ) -> Result<Vec<StaleUploadedRow>, ContextStoreError> {
            self.push(Call::FetchStaleUploaded);
            Ok(self
                .stale_uploaded
                .iter()
                .take(limit as usize)
                .cloned()
                .collect())
        }

        async fn fetch_stale_draft_creating(
            &self,
            _before: DateTime<Utc>,
            limit: i64,
        ) -> Result<Vec<StaleDraftCreatingRow>, ContextStoreError> {
            self.push(Call::FetchStaleDraftCreating);
            Ok(self
                .stale_draft_creating
                .iter()
                .take(limit as usize)
                .cloned()
                .collect())
        }

        async fn fetch_stale_draft_created(
            &self,
            _before: DateTime<Utc>,
            limit: i64,
        ) -> Result<Vec<StaleDraftCreatedRow>, ContextStoreError> {
            self.push(Call::FetchStaleDraftCreated);
            Ok(self
                .stale_draft_created
                .iter()
                .take(limit as usize)
                .cloned()
                .collect())
        }

        async fn mark_stale_failed(
            &self,
            request_key: &RateLimiterRequestKey,
            from_states: &[&str],
        ) -> Result<bool, ContextStoreError> {
            self.push(Call::MarkStaleFailed(from_states.join(",")));
            let _ = request_key;
            Ok(true)
        }

        async fn record_reconciliation_error(
            &self,
            _request_key: &RateLimiterRequestKey,
            error: &str,
        ) -> Result<(), ContextStoreError> {
            self.push(Call::RecordReconciliationError(error.to_string()));
            Ok(())
        }

        async fn mark_rate_limit_failed(
            &self,
            _request_key: &RateLimiterRequestKey,
            reason: &str,
        ) -> Result<(), String> {
            self.push(Call::MarkRateLimitFailed(reason.to_string()));
            if let Some(error) = &self.rate_limit_failed_error {
                return Err(error.clone());
            }
            Ok(())
        }

        async fn decrement_rate_limit(
            &self,
            _request_key: &RateLimiterRequestKey,
        ) -> Result<(), String> {
            self.push(Call::DecrementRateLimit);
            Ok(())
        }

        async fn mark_rate_limit_complete(
            &self,
            _request_key: &RateLimiterRequestKey,
            _bucket_url: &str,
        ) -> Result<(), String> {
            self.push(Call::MarkRateLimitComplete);
            Ok(())
        }

        async fn release_upload_destination(
            &self,
            _request_key: &RateLimiterRequestKey,
            _destination: Option<&crate::videogen::upload_destination::UploadDestination>,
        ) -> Result<(), String> {
            self.push(Call::ReleaseUploadDestination);
            Ok(())
        }

        async fn redact_identity(
            &self,
            _request_key: &RateLimiterRequestKey,
        ) -> Result<(), ContextStoreError> {
            self.push(Call::RedactIdentity);
            Ok(())
        }

        async fn count_contexts_by_state(&self) -> Result<Vec<(String, i64)>, String> {
            Ok(vec![])
        }

        async fn create_draft_for_upload(
            &self,
            _row: &StaleDraftCreatingRow,
        ) -> Result<(), String> {
            self.push(Call::CreateDraftForUpload);
            Ok(())
        }

        async fn increment_draft_attempts(
            &self,
            _request_key: &RateLimiterRequestKey,
        ) -> Result<(), ContextStoreError> {
            self.push(Call::IncrementDraftAttempts);
            Ok(())
        }

        async fn mark_draft_creating(
            &self,
            _request_key: &RateLimiterRequestKey,
        ) -> Result<(), ContextStoreError> {
            self.push(Call::MarkDraftCreating);
            Ok(())
        }

        async fn mark_draft_failed(
            &self,
            _request_key: &RateLimiterRequestKey,
            _reason: &str,
        ) -> Result<(), ContextStoreError> {
            self.push(Call::MarkDraftFailed);
            Ok(())
        }

        async fn mark_complete_with_bucket_url(
            &self,
            _request_key: &RateLimiterRequestKey,
            _bucket_url: &str,
        ) -> Result<(), ContextStoreError> {
            self.push(Call::MarkCompleteWithBucketUrl);
            Ok(())
        }
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn config() -> ReconcileConfig {
        ReconcileConfig {
            batch_size: 100,
            context_created_timeout_secs: 120,
            ltx_generation_timeout_secs: 1800,
            draft_create_timeout_secs: 600,
            draft_created_complete_timeout_secs: 120,
            draft_create_max_attempts: 3,
        }
    }

    fn key(counter: u64) -> RateLimiterRequestKey {
        RateLimiterRequestKey {
            principal: "aaaaa-aa".to_string(),
            counter,
        }
    }

    fn stale_row(counter: u64) -> StaleRow {
        StaleRow {
            request_key: key(counter),
            operation_id: format!("aaaaa-aa_{counter}"),
            upload_destination: None,
        }
    }

    fn stale_uploaded_row(counter: u64) -> StaleUploadedRow {
        StaleUploadedRow {
            request_key: key(counter),
            operation_id: format!("aaaaa-aa_{counter}"),
            video_id: Some(format!("video-{counter}")),
            object_key: Some(format!("generated/video-{counter}.mp4")),
            bucket_url: None,
        }
    }

    fn stale_draft_creating_row(counter: u64, attempts: i32) -> StaleDraftCreatingRow {
        StaleDraftCreatingRow {
            request_key: key(counter),
            operation_id: format!("aaaaa-aa_{counter}"),
            draft_attempts: attempts,
            video_id: Some(format!("video-{counter}")),
            object_key: Some(format!("generated/video-{counter}.mp4")),
            bucket_url: None,
        }
    }

    fn stale_draft_created_row(counter: u64, bucket_url: Option<&str>) -> StaleDraftCreatedRow {
        StaleDraftCreatedRow {
            request_key: key(counter),
            operation_id: format!("aaaaa-aa_{counter}"),
            bucket_url: bucket_url.map(|u| u.to_string()),
        }
    }

    // -----------------------------------------------------------------------
    // Test 1: context_created stale → full cleanup sequence
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn context_created_stale_calls_rate_limiter_decrement_redact_and_stale_failed() {
        let deps = FakeReconcileDeps {
            stale_context_created: vec![stale_row(1)],
            ..Default::default()
        };

        reconcile_context_created(&deps, &config()).await;

        let calls = deps.calls();
        assert!(calls.contains(&Call::FetchStaleContextCreated));
        assert!(calls.contains(&Call::MarkRateLimitFailed(
            "stale context_created".to_string()
        )));
        assert!(calls.contains(&Call::DecrementRateLimit));
        assert!(calls.contains(&Call::ReleaseUploadDestination));
        assert!(calls.contains(&Call::RedactIdentity));
        assert!(calls.contains(&Call::MarkStaleFailed("context_created".to_string())));
    }

    // -----------------------------------------------------------------------
    // Test 2: mark_rate_limit_failed errors → record error, NO mark_stale_failed
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn rate_limit_failed_error_records_error_and_skips_stale_failed() {
        let deps = FakeReconcileDeps {
            stale_context_created: vec![stale_row(2)],
            rate_limit_failed_error: Some("canister unavailable".to_string()),
            ..Default::default()
        };

        reconcile_context_created(&deps, &config()).await;

        let calls = deps.calls();
        assert!(calls.contains(&Call::RecordReconciliationError(
            "canister unavailable".to_string()
        )));
        // mark_stale_failed must NOT be called
        assert!(!calls.iter().any(|c| matches!(c, Call::MarkStaleFailed(_))));
        // updated_at must not be touched — we verify by absence of MarkStaleFailed
    }

    // -----------------------------------------------------------------------
    // Test 3: submitted stale → same RateLimiter+redact sequence
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn submitted_stale_calls_rate_limiter_decrement_redact_and_stale_failed() {
        let deps = FakeReconcileDeps {
            stale_submitted: vec![stale_row(3)],
            ..Default::default()
        };

        reconcile_submitted(&deps, &config()).await;

        let calls = deps.calls();
        assert!(calls.contains(&Call::FetchStaleSubmitted));
        assert!(calls.contains(&Call::MarkRateLimitFailed("stale submitted".to_string())));
        assert!(calls.contains(&Call::DecrementRateLimit));
        assert!(calls.contains(&Call::ReleaseUploadDestination));
        assert!(calls.contains(&Call::RedactIdentity));
        assert!(calls.contains(&Call::MarkStaleFailed("submitted".to_string())));
    }

    // -----------------------------------------------------------------------
    // Test 4: uploaded stale → mark_draft_creating called
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn uploaded_stale_triggers_mark_draft_creating() {
        let deps = FakeReconcileDeps {
            stale_uploaded: vec![stale_uploaded_row(4)],
            ..Default::default()
        };

        reconcile_uploaded(&deps, &config()).await;

        let calls = deps.calls();
        assert!(calls.contains(&Call::FetchStaleUploaded));
        assert!(calls.contains(&Call::MarkDraftCreating));
    }

    // -----------------------------------------------------------------------
    // Test 5: draft_creating with attempts < max → create_draft + increment
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn draft_creating_below_max_attempts_retries_creation_and_increments() {
        let deps = FakeReconcileDeps {
            stale_draft_creating: vec![stale_draft_creating_row(5, 1)],
            ..Default::default()
        };

        reconcile_draft_creating(&deps, &config()).await;

        let calls = deps.calls();
        assert!(calls.contains(&Call::FetchStaleDraftCreating));
        assert!(calls.contains(&Call::CreateDraftForUpload));
        assert!(calls.contains(&Call::IncrementDraftAttempts));
        assert!(!calls.contains(&Call::MarkDraftFailed));
    }

    // -----------------------------------------------------------------------
    // Test 6: draft_creating with attempts >= max → mark_draft_failed + rate_limit_failed
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn draft_creating_at_max_attempts_marks_draft_failed_and_rate_limit_failed() {
        let max = config().draft_create_max_attempts as i32;
        let deps = FakeReconcileDeps {
            stale_draft_creating: vec![stale_draft_creating_row(6, max)],
            ..Default::default()
        };

        reconcile_draft_creating(&deps, &config()).await;

        let calls = deps.calls();
        assert!(calls.contains(&Call::MarkDraftFailed));
        assert!(calls
            .iter()
            .any(|c| matches!(c, Call::MarkRateLimitFailed(_))));
        assert!(!calls.contains(&Call::CreateDraftForUpload));
    }

    // -----------------------------------------------------------------------
    // Test 7: draft_created stale → mark_rate_limit_complete + mark_complete
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn draft_created_stale_completes_with_bucket_url() {
        let deps = FakeReconcileDeps {
            stale_draft_created: vec![stale_draft_created_row(
                7,
                Some("https://bucket.example.test/video-7.mp4"),
            )],
            ..Default::default()
        };

        reconcile_draft_created(&deps, &config()).await;

        let calls = deps.calls();
        assert!(calls.contains(&Call::FetchStaleDraftCreated));
        assert!(calls.contains(&Call::MarkRateLimitComplete));
        assert!(calls.contains(&Call::MarkCompleteWithBucketUrl));
    }

    // -----------------------------------------------------------------------
    // Test 8: batch_size = 1 limits rows processed even with 3 stale rows
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn batch_size_one_processes_only_one_row() {
        let rows = vec![stale_row(10), stale_row(11), stale_row(12)];
        let deps = FakeReconcileDeps {
            stale_context_created: rows,
            ..Default::default()
        };

        let cfg = ReconcileConfig {
            batch_size: 1,
            ..config()
        };
        reconcile_context_created(&deps, &cfg).await;

        // Only one MarkStaleFailed should appear (one row processed).
        let calls = deps.calls();
        let stale_failed_count = calls
            .iter()
            .filter(|c| matches!(c, Call::MarkStaleFailed(_)))
            .count();
        assert_eq!(stale_failed_count, 1);
    }
}
