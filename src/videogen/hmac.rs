use std::{collections::BTreeMap, fmt};

use ::hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone, PartialEq, Eq)]
pub struct HmacKey {
    id: String,
    bytes: Vec<u8>,
}

impl HmacKey {
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl fmt::Debug for HmacKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HmacKey")
            .field("id", &self.id)
            .field("bytes", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct HmacKeyRegistry {
    keys: BTreeMap<String, HmacKey>,
}

impl fmt::Debug for HmacKeyRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let key_ids: Vec<&str> = self.keys.keys().map(String::as_str).collect();
        f.debug_struct("HmacKeyRegistry")
            .field("key_ids", &key_ids)
            .finish()
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HmacError {
    #[error("hmac key registry is empty")]
    EmptyRegistry,
    #[error("invalid hmac key entry")]
    InvalidKeyEntry,
    #[error("hmac key id is empty")]
    EmptyKeyId,
    #[error("hmac key must not be empty")]
    EmptyKey,
    #[error("duplicate hmac key id: {0}")]
    DuplicateKeyId(String),
    #[error("unknown hmac key id")]
    UnknownKeyId,
    #[error("timestamp is outside allowed skew")]
    TimestampOutsideSkew,
    #[error("signature is invalid")]
    InvalidSignature,
}

impl HmacKeyRegistry {
    pub fn parse(input: &str) -> Result<Self, HmacError> {
        let mut keys = BTreeMap::new();

        for entry in input
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
        {
            let (id, encoded_key) = entry.split_once(':').ok_or(HmacError::InvalidKeyEntry)?;
            let id = id.trim();
            if id.is_empty() {
                return Err(HmacError::EmptyKeyId);
            }
            if keys.contains_key(id) {
                return Err(HmacError::DuplicateKeyId(id.to_string()));
            }

            let bytes: Vec<u8> = encoded_key.trim().as_bytes().to_vec();
            if bytes.is_empty() {
                return Err(HmacError::EmptyKey);
            }

            keys.insert(
                id.to_string(),
                HmacKey {
                    id: id.to_string(),
                    bytes,
                },
            );
        }

        if keys.is_empty() {
            return Err(HmacError::EmptyRegistry);
        }

        Ok(Self { keys })
    }

    pub fn get(&self, key_id: &str) -> Option<&HmacKey> {
        self.keys.get(key_id)
    }

    /// Build a registry from a single plain-text token (e.g. AUTH_TOKEN).
    /// The token bytes are used directly as the HMAC key with a fixed key_id "v1".
    pub fn from_service_token(token: &str) -> Result<Self, HmacError> {
        if token.is_empty() {
            return Err(HmacError::EmptyKey);
        }
        let mut keys = BTreeMap::new();
        keys.insert(
            "v1".to_string(),
            HmacKey {
                id: "v1".to_string(),
                bytes: token.as_bytes().to_vec(),
            },
        );
        Ok(Self { keys })
    }
}

pub fn body_sha256_hex(raw_body: &[u8]) -> String {
    sha256_hex(raw_body)
}

pub fn sign_completion(
    method: &str,
    path: &str,
    timestamp: i64,
    body_hash_hex: &str,
    key: &HmacKey,
) -> String {
    let mut mac = HmacSha256::new_from_slice(&key.bytes).expect("HMAC accepts any non-empty key");
    mac.update(signature_message(method, path, timestamp, body_hash_hex).as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

#[allow(clippy::too_many_arguments)]
pub fn verify_completion_signature(
    registry: &HmacKeyRegistry,
    key_id: &str,
    method: &str,
    path: &str,
    timestamp: i64,
    body_hash_hex: &str,
    signature_hex: &str,
    now_timestamp: i64,
    allowed_skew_secs: i64,
) -> Result<(), HmacError> {
    let key = registry.get(key_id).ok_or(HmacError::UnknownKeyId)?;

    if now_timestamp.abs_diff(timestamp) > allowed_skew_secs.max(0) as u64 {
        return Err(HmacError::TimestampOutsideSkew);
    }

    let signature = hex::decode(signature_hex).map_err(|_| HmacError::InvalidSignature)?;
    let mut mac = HmacSha256::new_from_slice(&key.bytes).expect("HMAC accepts any non-empty key");
    mac.update(signature_message(method, path, timestamp, body_hash_hex).as_bytes());
    mac.verify_slice(&signature)
        .map_err(|_| HmacError::InvalidSignature)
}

fn signature_message(method: &str, path: &str, timestamp: i64, body_hash_hex: &str) -> String {
    format!("{method}\n{path}\n{timestamp}\n{body_hash_hex}")
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};

        hex::encode(Sha256::digest(bytes))
    }

    #[test]
    fn completion_signature_round_trips() {
        let registry = HmacKeyRegistry::parse("v1:test-auth-token-secret").unwrap();
        let body_hash = sha256_hex(br#"{"status":"success"}"#);
        let sig = sign_completion(
            "POST",
            "/api/v2/videogen/complete",
            1_777_000_000,
            &body_hash,
            registry.get("v1").unwrap(),
        );
        assert!(verify_completion_signature(
            &registry,
            "v1",
            "POST",
            "/api/v2/videogen/complete",
            1_777_000_000,
            &body_hash,
            &sig,
            1_777_000_001,
            120
        )
        .is_ok());
    }

    #[test]
    fn unknown_key_id_fails_without_fallback() {
        let registry = HmacKeyRegistry::parse("v1:test-auth-token-secret").unwrap();
        assert!(matches!(
            verify_completion_signature(
                &registry,
                "v2",
                "POST",
                "/api/v2/videogen/complete",
                1,
                "hash",
                "sig",
                1,
                120
            ),
            Err(HmacError::UnknownKeyId)
        ));
    }

    #[test]
    fn stale_timestamp_is_rejected() {
        let registry = HmacKeyRegistry::parse("v1:test-auth-token-secret").unwrap();
        let body_hash = sha256_hex(br#"{"status":"success"}"#);
        let sig = sign_completion(
            "POST",
            "/api/v2/videogen/complete",
            100,
            &body_hash,
            registry.get("v1").unwrap(),
        );

        assert!(matches!(
            verify_completion_signature(
                &registry,
                "v1",
                "POST",
                "/api/v2/videogen/complete",
                100,
                &body_hash,
                &sig,
                300,
                120
            ),
            Err(HmacError::TimestampOutsideSkew)
        ));
    }

    #[test]
    fn duplicate_key_ids_are_rejected() {
        assert!(matches!(
            HmacKeyRegistry::parse(
                "v1:test-auth-token-secret,v1:other-auth-token-secret"
            ),
            Err(HmacError::DuplicateKeyId(id)) if id == "v1"
        ));
    }

    #[test]
    fn key_registry_debug_redacts_key_material() {
        let registry = HmacKeyRegistry::parse("v1:test-auth-token-secret").unwrap();
        let debug = format!("{registry:?}");

        assert!(debug.contains("v1"));
        assert!(!debug.contains("bytes: ["));
        assert!(!debug.contains("test-auth-token-secret"));
    }
}
