use std::fmt;

use chrono::{DateTime, Duration, Utc};
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
    client: Client,
}

impl PostgresVideogenContextStore {
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    pub async fn create_context(
        &self,
        context: PendingVideogenContext,
    ) -> Result<(), ContextStoreError> {
        let counter = context.request_key.counter as i64;
        let state = VideogenContextState::ContextCreated.as_str();
        self.client
            .execute(
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
        let created_after = Utc::now() - Duration::seconds(window_secs as i64);
        let row = self
            .client
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
        let counter = request_key.counter as i64;
        let rows = self
            .client
            .execute(
                "UPDATE videogen_completion_contexts
                 SET request_id = $3,
                     state = 'submitted',
                     vast_submit_attempts = vast_submit_attempts + 1,
                     updated_at = NOW()
                 WHERE principal = $1
                   AND counter = $2
                   AND request_id = $3
                   AND state = 'context_created'",
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
        let counter = request_key.counter as i64;
        let rows = self
            .client
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
        let counter = request_key.counter as i64;
        let upload_destination = PgJson(destination);
        self.client
            .execute(
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
        let counter = request_key.counter as i64;
        self.client
            .execute(
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
        let counter = request_key.counter as i64;
        self.client
            .execute(
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
