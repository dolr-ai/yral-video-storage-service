use crate::videogen::{
    identity_crypto::EncryptedDelegatedIdentity, rate_limiter::RateLimiterRequestKey,
};
use chrono::{DateTime, Utc};
use std::fmt;

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
