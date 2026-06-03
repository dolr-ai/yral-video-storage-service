use std::{fmt, sync::Arc};

use chrono::{DateTime, Duration, Utc};
use tokio::sync::Mutex;
use tokio_postgres::{types::Json as PgJson, Client};

use crate::videogen::{
    identity_crypto::EncryptedDelegatedIdentity, rate_limiter::RateLimiterRequestKey,
    types::VideogenContextState,
};

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum ContextStoreError {
    #[error("context store unavailable: {0}")]
    Unavailable(String),
    #[error("context state rejected update: {0}")]
    InvalidState(String),
}

#[derive(Clone, PartialEq, Eq)]
pub struct PendingVideogenContext {
    pub request_key: RateLimiterRequestKey,
    pub operation_id: String,
    pub request_fingerprint: String,
    pub request_fingerprint_version: i32,
    pub provider: String,
    pub model_id: String,
    pub prompt: String,
    pub upload_handling: String,
    pub encrypted_identity: EncryptedDelegatedIdentity,
    pub dedupe_expires_at: DateTime<Utc>,
    pub generation_expires_at: DateTime<Utc>,
}

/// Internal client handle — either an owned one-shot client (routes) or a
/// shared cached client (reconciler).
enum ClientHandle {
    Owned(Client),
    Shared(Arc<Mutex<Option<Client>>>),
}

impl fmt::Debug for PendingVideogenContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingVideogenContext")
            .field("request_key", &self.request_key)
            .field("operation_id", &self.operation_id)
            .field("request_fingerprint", &self.request_fingerprint)
            .field(
                "request_fingerprint_version",
                &self.request_fingerprint_version,
            )
            .field("provider", &self.provider)
            .field("model_id", &self.model_id)
            .field("prompt", &"<redacted>")
            .field("upload_handling", &self.upload_handling)
            .field("encrypted_identity", &"<redacted>")
            .field("dedupe_expires_at", &self.dedupe_expires_at)
            .field("generation_expires_at", &self.generation_expires_at)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DedupeMatch {
    pub operation_id: String,
    pub provider: String,
    pub request_key: RateLimiterRequestKey,
}

pub struct PostgresVideogenContextStore {
    handle: ClientHandle,
}

impl PostgresVideogenContextStore {
    /// Create a store wrapping an owned (one-shot) Postgres client.
    /// Used by HTTP route handlers that connect once per request.
    pub fn new(client: Client) -> Self {
        Self {
            handle: ClientHandle::Owned(client),
        }
    }

    /// Create a store sharing a cached Postgres client held in a mutex.
    /// Used by the reconciler to reuse one connection across a full cycle.
    pub fn from_shared(shared: Arc<Mutex<Option<Client>>>) -> Self {
        Self {
            handle: ClientHandle::Shared(shared),
        }
    }

    /// Acquire a reference to the underlying Postgres client.
    ///
    /// For `Owned` handles this is a direct borrow.
    /// For `Shared` handles this acquires the mutex and unwraps the `Option`
    /// (the reconciler ensures it is `Some` before constructing the store).
    ///
    /// Returns `Err` only if the shared client has been vacated, which should
    /// not happen in normal operation.
    async fn client(&self) -> Result<ClientRef<'_>, ContextStoreError> {
        match &self.handle {
            ClientHandle::Owned(c) => Ok(ClientRef::Borrowed(c)),
            ClientHandle::Shared(arc) => {
                let guard = arc.lock().await;
                if guard.is_none() {
                    return Err(ContextStoreError::Unavailable(
                        "shared db client not initialised".to_string(),
                    ));
                }
                Ok(ClientRef::Guarded(guard))
            }
        }
    }
}

/// Temporary borrow of a Postgres client, either from an owned value or a mutex guard.
enum ClientRef<'a> {
    Borrowed(&'a Client),
    Guarded(tokio::sync::MutexGuard<'a, Option<Client>>),
}

impl std::ops::Deref for ClientRef<'_> {
    type Target = Client;

    fn deref(&self) -> &Self::Target {
        match self {
            ClientRef::Borrowed(c) => c,
            ClientRef::Guarded(g) => g.as_ref().unwrap(),
        }
    }
}

impl PostgresVideogenContextStore {
    pub async fn create_context(
        &self,
        context: PendingVideogenContext,
    ) -> Result<(), ContextStoreError> {
        let c = self.client().await?;
        let counter = context.request_key.counter as i64;
        let state = VideogenContextState::ContextCreated.as_str();
        c.execute(
            "INSERT INTO videogen_completion_contexts (
                    principal, counter, operation_id, request_fingerprint,
                    request_fingerprint_version, provider, model_id, prompt, upload_handling,
                    encrypted_delegated_identity, identity_nonce, encryption_key_id, state,
                    dedupe_expires_at, generation_expires_at
                ) VALUES (
                    $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15
                )",
            &[
                &context.request_key.principal,
                &counter,
                &context.operation_id,
                &context.request_fingerprint,
                &context.request_fingerprint_version,
                &context.provider,
                &context.model_id,
                &context.prompt,
                &context.upload_handling,
                &context.encrypted_identity.ciphertext,
                &context.encrypted_identity.nonce,
                &context.encrypted_identity.encryption_key_id,
                &state,
                &context.dedupe_expires_at,
                &context.generation_expires_at,
            ],
        )
        .await
        .map_err(|error| ContextStoreError::Unavailable(error.to_string()))?;
        Ok(())
    }

    pub async fn find_dedupe(
        &self,
        principal: &str,
        fingerprint: &str,
        window_secs: u64,
    ) -> Result<Option<DedupeMatch>, ContextStoreError> {
        let c = self.client().await?;
        let created_after = Utc::now() - Duration::seconds(window_secs as i64);
        let row = c
            .query_opt(
                "SELECT operation_id, provider, principal, counter
                 FROM videogen_completion_contexts
                 WHERE principal = $1
                   AND request_fingerprint = $2
                   AND created_at >= $3
                   AND state IN (
                     'context_created','submitted','uploaded','draft_creating',
                     'draft_created','complete'
                   )
                 ORDER BY created_at DESC
                 LIMIT 1",
                &[&principal, &fingerprint, &created_after],
            )
            .await
            .map_err(|error| ContextStoreError::Unavailable(error.to_string()))?;

        Ok(row.map(|row| DedupeMatch {
            operation_id: row.get(0),
            provider: row.get(1),
            request_key: RateLimiterRequestKey {
                principal: row.get(2),
                counter: row.get::<_, i64>(3) as u64,
            },
        }))
    }

    pub async fn mark_submitted(
        &self,
        request_key: &RateLimiterRequestKey,
        request_id: &str,
        _accepted_at: DateTime<Utc>,
    ) -> Result<(), ContextStoreError> {
        let c = self.client().await?;
        let counter = request_key.counter as i64;
        let rows = c
            .execute(
                "UPDATE videogen_completion_contexts
                 SET request_id = $3,
                     state = 'submitted',
                     vast_submit_attempts = vast_submit_attempts + 1,
                     updated_at = NOW()
                 WHERE principal = $1
                   AND counter = $2
                   AND state = 'context_created'
                   AND request_id IS NULL",
                &[&request_key.principal, &counter, &request_id],
            )
            .await
            .map_err(|error| ContextStoreError::Unavailable(error.to_string()))?;
        if rows == 0 {
            return Err(ContextStoreError::InvalidState(
                "context was not in context_created".to_string(),
            ));
        }
        Ok(())
    }

    pub async fn store_request_id(
        &self,
        request_key: &RateLimiterRequestKey,
        request_id: &str,
    ) -> Result<(), ContextStoreError> {
        let c = self.client().await?;
        let counter = request_key.counter as i64;
        let rows = c
            .execute(
                "UPDATE videogen_completion_contexts
                 SET request_id = $3,
                     updated_at = NOW()
                 WHERE principal = $1
                   AND counter = $2
                   AND state = 'context_created'
                   AND request_id IS NULL",
                &[&request_key.principal, &counter, &request_id],
            )
            .await
            .map_err(|error| ContextStoreError::Unavailable(error.to_string()))?;
        if rows == 0 {
            return Err(ContextStoreError::InvalidState(
                "context cannot store request_id".to_string(),
            ));
        }
        Ok(())
    }

    pub async fn set_upload_destination(
        &self,
        request_key: &RateLimiterRequestKey,
        destination: &crate::videogen::upload_destination::UploadDestination,
    ) -> Result<(), ContextStoreError> {
        let c = self.client().await?;
        let counter = request_key.counter as i64;
        let upload_destination = PgJson(destination);
        c.execute(
            "UPDATE videogen_completion_contexts
                 SET upload_destination = $3,
                     draft_video_id = $4,
                     object_key = $5,
                     upload_destination_expires_at = $6,
                     updated_at = NOW()
                 WHERE principal = $1 AND counter = $2",
            &[
                &request_key.principal,
                &counter,
                &upload_destination,
                &destination.video_id,
                &destination.object_key,
                &destination.expires_at,
            ],
        )
        .await
        .map_err(|error| ContextStoreError::Unavailable(error.to_string()))?;
        Ok(())
    }

    pub async fn mark_submit_failed(
        &self,
        request_key: &RateLimiterRequestKey,
        reason: &str,
    ) -> Result<(), ContextStoreError> {
        let c = self.client().await?;
        let counter = request_key.counter as i64;
        c.execute(
            "UPDATE videogen_completion_contexts
                 SET state = 'submit_failed',
                     last_error = $3,
                     updated_at = NOW()
                 WHERE principal = $1
                   AND counter = $2
                   AND state IN ('context_created','submitted')",
            &[&request_key.principal, &counter, &reason],
        )
        .await
        .map_err(|error| ContextStoreError::Unavailable(error.to_string()))?;
        Ok(())
    }

    pub async fn redact_identity(
        &self,
        request_key: &RateLimiterRequestKey,
    ) -> Result<(), ContextStoreError> {
        let c = self.client().await?;
        let counter = request_key.counter as i64;
        c.execute(
            "UPDATE videogen_completion_contexts
                 SET encrypted_delegated_identity = NULL,
                     identity_nonce = NULL,
                     encryption_key_id = NULL,
                     updated_at = NOW()
                 WHERE principal = $1 AND counter = $2",
            &[&request_key.principal, &counter],
        )
        .await
        .map_err(|error| ContextStoreError::Unavailable(error.to_string()))?;
        Ok(())
    }

    /// Atomically claim a submitted row for completion processing.
    /// Returns Some(row) if the claim succeeded (state was 'submitted' with matching request_id),
    /// or None if the row was already claimed by another handler.
    pub async fn claim_for_completion(
        &self,
        request_key: &RateLimiterRequestKey,
        request_id: &str,
    ) -> Result<Option<CompletionContextRow>, ContextStoreError> {
        let c = self.client().await?;
        let counter = request_key.counter as i64;
        let row = c
            .query_opt(
                "UPDATE videogen_completion_contexts
                 SET state = 'uploaded',
                     updated_at = NOW()
                 WHERE principal = $1
                   AND counter = $2
                   AND request_id = $3
                   AND state = 'submitted'
                 RETURNING principal, counter, request_id, state, object_key, draft_video_id",
                &[&request_key.principal, &counter, &request_id],
            )
            .await
            .map_err(|error| ContextStoreError::Unavailable(error.to_string()))?;

        Ok(row.map(|row| CompletionContextRow {
            request_key: RateLimiterRequestKey {
                principal: row.get(0),
                counter: row.get::<_, i64>(1) as u64,
            },
            request_id: row.get(2),
            state: row.get(3),
            object_key: row.get(4),
            video_id: row.get(5),
        }))
    }

    pub async fn mark_draft_creating(
        &self,
        request_key: &RateLimiterRequestKey,
    ) -> Result<(), ContextStoreError> {
        let c = self.client().await?;
        let counter = request_key.counter as i64;
        let rows = c
            .execute(
                "UPDATE videogen_completion_contexts
                 SET state = 'draft_creating',
                     updated_at = NOW()
                 WHERE principal = $1
                   AND counter = $2
                   AND state = 'uploaded'",
                &[&request_key.principal, &counter],
            )
            .await
            .map_err(|error| ContextStoreError::Unavailable(error.to_string()))?;
        if rows == 0 {
            return Err(ContextStoreError::InvalidState(
                "context was not in uploaded".to_string(),
            ));
        }
        Ok(())
    }

    pub async fn mark_draft_created(
        &self,
        request_key: &RateLimiterRequestKey,
    ) -> Result<(), ContextStoreError> {
        let c = self.client().await?;
        let counter = request_key.counter as i64;
        let rows = c
            .execute(
                "UPDATE videogen_completion_contexts
                 SET state = 'draft_created',
                     updated_at = NOW()
                 WHERE principal = $1
                   AND counter = $2
                   AND state = 'draft_creating'",
                &[&request_key.principal, &counter],
            )
            .await
            .map_err(|error| ContextStoreError::Unavailable(error.to_string()))?;
        if rows == 0 {
            return Err(ContextStoreError::InvalidState(
                "context was not in draft_creating".to_string(),
            ));
        }
        Ok(())
    }

    pub async fn mark_complete(
        &self,
        request_key: &RateLimiterRequestKey,
        bucket_url: &str,
    ) -> Result<(), ContextStoreError> {
        let c = self.client().await?;
        let counter = request_key.counter as i64;
        let rows = c
            .execute(
                "UPDATE videogen_completion_contexts
                 SET state = 'complete',
                     bucket_url = $3,
                     encrypted_delegated_identity = NULL,
                     identity_nonce = NULL,
                     encryption_key_id = NULL,
                     updated_at = NOW()
                 WHERE principal = $1
                   AND counter = $2
                   AND state = 'draft_created'",
                &[&request_key.principal, &counter, &bucket_url],
            )
            .await
            .map_err(|error| ContextStoreError::Unavailable(error.to_string()))?;
        if rows == 0 {
            return Err(ContextStoreError::InvalidState(
                "context was not in draft_created".to_string(),
            ));
        }
        Ok(())
    }

    pub async fn mark_generation_failed(
        &self,
        request_key: &RateLimiterRequestKey,
        reason: &str,
    ) -> Result<(), ContextStoreError> {
        let c = self.client().await?;
        let counter = request_key.counter as i64;
        c.execute(
            "UPDATE videogen_completion_contexts
                 SET state = 'failed',
                     last_error = $3,
                     encrypted_delegated_identity = NULL,
                     identity_nonce = NULL,
                     encryption_key_id = NULL,
                     updated_at = NOW()
                 WHERE principal = $1
                   AND counter = $2
                   AND state IN ('context_created', 'submitted')",
            &[&request_key.principal, &counter, &reason],
        )
        .await
        .map_err(|error| ContextStoreError::Unavailable(error.to_string()))?;
        Ok(())
    }

    pub async fn get_context_state(
        &self,
        request_key: &RateLimiterRequestKey,
    ) -> Result<Option<ContextStateRow>, ContextStoreError> {
        let c = self.client().await?;
        let counter = request_key.counter as i64;
        let row = c
            .query_opt(
                "SELECT state, request_id, principal, object_key, draft_video_id
                 FROM videogen_completion_contexts
                 WHERE principal = $1 AND counter = $2",
                &[&request_key.principal, &counter],
            )
            .await
            .map_err(|error| ContextStoreError::Unavailable(error.to_string()))?;

        Ok(row.map(|row| ContextStateRow {
            state: row.get(0),
            request_id: row.get(1),
            principal: row.get(2),
            object_key: row.get(3),
            video_id: row.get(4),
        }))
    }
}

// ---------------------------------------------------------------------------
// Stale row types for reconciliation
// ---------------------------------------------------------------------------

/// A minimal stale row for states that need rate-limiter cleanup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleRow {
    pub request_key: RateLimiterRequestKey,
    pub operation_id: String,
    pub upload_destination: Option<crate::videogen::upload_destination::UploadDestination>,
}

/// Stale row for `uploaded` state — needs draft creation attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleUploadedRow {
    pub request_key: RateLimiterRequestKey,
    pub operation_id: String,
    pub video_id: Option<String>,
    pub object_key: Option<String>,
    pub bucket_url: Option<String>,
}

/// Stale row for `draft_creating` state — may need retry or failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleDraftCreatingRow {
    pub request_key: RateLimiterRequestKey,
    pub operation_id: String,
    pub draft_attempts: i32,
    pub video_id: Option<String>,
    pub object_key: Option<String>,
    pub bucket_url: Option<String>,
}

/// Stale row for `draft_created` state — needs completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleDraftCreatedRow {
    pub request_key: RateLimiterRequestKey,
    pub operation_id: String,
    pub bucket_url: Option<String>,
}

// ---------------------------------------------------------------------------
// Batch-query methods for reconciliation
// ---------------------------------------------------------------------------

impl PostgresVideogenContextStore {
    pub async fn fetch_stale_context_created(
        &self,
        before: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<StaleRow>, ContextStoreError> {
        let c = self.client().await?;
        let rows = c
            .query(
                "SELECT principal, counter, operation_id, upload_destination
                 FROM videogen_completion_contexts
                 WHERE state = 'context_created'
                   AND updated_at < $1
                 ORDER BY updated_at ASC
                 LIMIT $2",
                &[&before, &limit],
            )
            .await
            .map_err(|e| ContextStoreError::Unavailable(e.to_string()))?;

        rows.into_iter()
            .map(|row| {
                let upload_destination: Option<
                    crate::videogen::upload_destination::UploadDestination,
                > = row
                    .try_get::<_, Option<PgJson<crate::videogen::upload_destination::UploadDestination>>>(3)
                    .ok()
                    .flatten()
                    .map(|j| j.0);
                Ok(StaleRow {
                    request_key: RateLimiterRequestKey {
                        principal: row.get(0),
                        counter: row.get::<_, i64>(1) as u64,
                    },
                    operation_id: row.get(2),
                    upload_destination,
                })
            })
            .collect()
    }

    pub async fn fetch_stale_submitted(
        &self,
        before: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<StaleRow>, ContextStoreError> {
        let c = self.client().await?;
        let rows = c
            .query(
                "SELECT principal, counter, operation_id, upload_destination
                 FROM videogen_completion_contexts
                 WHERE state = 'submitted'
                   AND updated_at < $1
                 ORDER BY updated_at ASC
                 LIMIT $2",
                &[&before, &limit],
            )
            .await
            .map_err(|e| ContextStoreError::Unavailable(e.to_string()))?;

        rows.into_iter()
            .map(|row| {
                let upload_destination: Option<
                    crate::videogen::upload_destination::UploadDestination,
                > = row
                    .try_get::<_, Option<PgJson<crate::videogen::upload_destination::UploadDestination>>>(3)
                    .ok()
                    .flatten()
                    .map(|j| j.0);
                Ok(StaleRow {
                    request_key: RateLimiterRequestKey {
                        principal: row.get(0),
                        counter: row.get::<_, i64>(1) as u64,
                    },
                    operation_id: row.get(2),
                    upload_destination,
                })
            })
            .collect()
    }

    pub async fn fetch_stale_uploaded(
        &self,
        before: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<StaleUploadedRow>, ContextStoreError> {
        let c = self.client().await?;
        let rows = c
            .query(
                "SELECT principal, counter, operation_id, draft_video_id, object_key, bucket_url
                 FROM videogen_completion_contexts
                 WHERE state = 'uploaded'
                   AND updated_at < $1
                 ORDER BY updated_at ASC
                 LIMIT $2",
                &[&before, &limit],
            )
            .await
            .map_err(|e| ContextStoreError::Unavailable(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(|row| StaleUploadedRow {
                request_key: RateLimiterRequestKey {
                    principal: row.get(0),
                    counter: row.get::<_, i64>(1) as u64,
                },
                operation_id: row.get(2),
                video_id: row.get(3),
                object_key: row.get(4),
                bucket_url: row.get(5),
            })
            .collect())
    }

    pub async fn fetch_stale_draft_creating(
        &self,
        before: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<StaleDraftCreatingRow>, ContextStoreError> {
        let c = self.client().await?;
        let rows = c
            .query(
                "SELECT principal, counter, operation_id, draft_attempts, draft_video_id, object_key, bucket_url
                 FROM videogen_completion_contexts
                 WHERE state = 'draft_creating'
                   AND updated_at < $1
                 ORDER BY updated_at ASC
                 LIMIT $2",
                &[&before, &limit],
            )
            .await
            .map_err(|e| ContextStoreError::Unavailable(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(|row| StaleDraftCreatingRow {
                request_key: RateLimiterRequestKey {
                    principal: row.get(0),
                    counter: row.get::<_, i64>(1) as u64,
                },
                operation_id: row.get(2),
                draft_attempts: row.get(3),
                video_id: row.get(4),
                object_key: row.get(5),
                bucket_url: row.get(6),
            })
            .collect())
    }

    pub async fn fetch_stale_draft_created(
        &self,
        before: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<StaleDraftCreatedRow>, ContextStoreError> {
        let c = self.client().await?;
        let rows = c
            .query(
                "SELECT principal, counter, operation_id, bucket_url
                 FROM videogen_completion_contexts
                 WHERE state = 'draft_created'
                   AND updated_at < $1
                 ORDER BY updated_at ASC
                 LIMIT $2",
                &[&before, &limit],
            )
            .await
            .map_err(|e| ContextStoreError::Unavailable(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(|row| StaleDraftCreatedRow {
                request_key: RateLimiterRequestKey {
                    principal: row.get(0),
                    counter: row.get::<_, i64>(1) as u64,
                },
                operation_id: row.get(2),
                bucket_url: row.get(3),
            })
            .collect())
    }

    /// Atomic conditional transition to `stale_failed`.
    /// Returns `true` if the row was updated, `false` if it had already moved on.
    pub async fn mark_stale_failed(
        &self,
        request_key: &RateLimiterRequestKey,
        from_states: &[&str],
    ) -> Result<bool, ContextStoreError> {
        let c = self.client().await?;
        let counter = request_key.counter as i64;
        let row = c
            .query_opt(
                "UPDATE videogen_completion_contexts
                 SET state = 'stale_failed', updated_at = NOW()
                 WHERE principal = $1 AND counter = $2 AND state = ANY($3)
                 RETURNING principal",
                &[&request_key.principal, &counter, &from_states],
            )
            .await
            .map_err(|e| ContextStoreError::Unavailable(e.to_string()))?;
        Ok(row.is_some())
    }

    /// Record a reconciliation error on the row WITHOUT touching `updated_at`
    /// (so stale detection keeps working), and increment `reconciliation_attempts`
    /// to allow operational queries to detect stuck rows.
    pub async fn record_reconciliation_error(
        &self,
        request_key: &RateLimiterRequestKey,
        error: &str,
    ) -> Result<(), ContextStoreError> {
        let c = self.client().await?;
        let counter = request_key.counter as i64;
        c.execute(
            "UPDATE videogen_completion_contexts
                 SET last_reconciliation_error = $3,
                     reconciliation_attempts = reconciliation_attempts + 1
                 WHERE principal = $1 AND counter = $2",
            &[&request_key.principal, &counter, &error],
        )
        .await
        .map_err(|e| ContextStoreError::Unavailable(e.to_string()))?;
        Ok(())
    }

    pub async fn increment_draft_attempts(
        &self,
        request_key: &RateLimiterRequestKey,
    ) -> Result<(), ContextStoreError> {
        let c = self.client().await?;
        let counter = request_key.counter as i64;
        c.execute(
            "UPDATE videogen_completion_contexts
                 SET draft_attempts = draft_attempts + 1,
                     updated_at = NOW()
                 WHERE principal = $1 AND counter = $2",
            &[&request_key.principal, &counter],
        )
        .await
        .map_err(|e| ContextStoreError::Unavailable(e.to_string()))?;
        Ok(())
    }

    pub async fn mark_draft_failed(
        &self,
        request_key: &RateLimiterRequestKey,
        reason: &str,
    ) -> Result<(), ContextStoreError> {
        let c = self.client().await?;
        let counter = request_key.counter as i64;
        c.execute(
            "UPDATE videogen_completion_contexts
                 SET state = 'draft_failed',
                     last_error = $3,
                     updated_at = NOW()
                 WHERE principal = $1 AND counter = $2
                   AND state IN ('draft_creating', 'uploaded')",
            &[&request_key.principal, &counter, &reason],
        )
        .await
        .map_err(|e| ContextStoreError::Unavailable(e.to_string()))?;
        Ok(())
    }

    pub async fn mark_complete_with_bucket_url(
        &self,
        request_key: &RateLimiterRequestKey,
        bucket_url: &str,
    ) -> Result<(), ContextStoreError> {
        let c = self.client().await?;
        let counter = request_key.counter as i64;
        c.execute(
            "UPDATE videogen_completion_contexts
                 SET state = 'complete',
                     bucket_url = $3,
                     encrypted_delegated_identity = NULL,
                     identity_nonce = NULL,
                     encryption_key_id = NULL,
                     updated_at = NOW()
                 WHERE principal = $1 AND counter = $2
                   AND state = 'draft_created'",
            &[&request_key.principal, &counter, &bucket_url],
        )
        .await
        .map_err(|e| ContextStoreError::Unavailable(e.to_string()))?;
        Ok(())
    }
}

/// A row returned from atomic claim or query operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionContextRow {
    pub request_key: RateLimiterRequestKey,
    pub request_id: String,
    pub state: String,
    pub object_key: Option<String>,
    pub video_id: Option<String>,
}

/// A lightweight row for idempotency checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextStateRow {
    pub state: String,
    pub request_id: Option<String>,
    pub principal: String,
    pub object_key: Option<String>,
    pub video_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::PendingVideogenContext;
    use crate::videogen::{
        identity_crypto::EncryptedDelegatedIdentity, rate_limiter::RateLimiterRequestKey,
    };

    #[test]
    fn pending_context_debug_redacts_prompt_and_identity() {
        let context = PendingVideogenContext {
            request_key: RateLimiterRequestKey {
                principal: "aaaaa-aa".to_string(),
                counter: 1,
            },
            operation_id: "aaaaa-aa_1".to_string(),
            request_fingerprint: "fingerprint".to_string(),
            request_fingerprint_version: 1,
            provider: "Ltx2".to_string(),
            model_id: "ltx2".to_string(),
            prompt: "private user prompt".to_string(),
            upload_handling: "ServerDraft".to_string(),
            encrypted_identity: EncryptedDelegatedIdentity {
                encryption_key_id: "key-v1".to_string(),
                nonce: b"secret nonce".to_vec(),
                ciphertext: b"secret ciphertext".to_vec(),
            },
            dedupe_expires_at: "2026-05-27T11:00:00Z".parse().unwrap(),
            generation_expires_at: "2026-05-27T11:30:00Z".parse().unwrap(),
        };

        let debug = format!("{context:?}");

        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("private user prompt"));
        assert!(!debug.contains("secret ciphertext"));
        assert!(!debug.contains("secret nonce"));
    }
}
