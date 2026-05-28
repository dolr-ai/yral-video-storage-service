use std::{collections::BTreeMap, fmt};

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use rand::{rngs::OsRng, RngCore};

#[derive(Clone, PartialEq, Eq)]
pub struct IdentityEncryptionKey {
    id: String,
    bytes: [u8; 32],
}

impl fmt::Debug for IdentityEncryptionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IdentityEncryptionKey")
            .field("id", &self.id)
            .field("bytes", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct IdentityEncryptionKeyRegistry {
    keys: BTreeMap<String, IdentityEncryptionKey>,
}

impl fmt::Debug for IdentityEncryptionKeyRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let key_ids: Vec<&str> = self.keys.keys().map(String::as_str).collect();
        f.debug_struct("IdentityEncryptionKeyRegistry")
            .field("key_ids", &key_ids)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct EncryptedDelegatedIdentity {
    pub encryption_key_id: String,
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
}

impl fmt::Debug for EncryptedDelegatedIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncryptedDelegatedIdentity")
            .field("encryption_key_id", &self.encryption_key_id)
            .field("nonce_len", &self.nonce.len())
            .field("ciphertext_len", &self.ciphertext.len())
            .finish()
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IdentityCryptoError {
    #[error("identity encryption key registry is empty")]
    EmptyRegistry,
    #[error("invalid identity encryption key entry")]
    InvalidKeyEntry,
    #[error("identity encryption key id is empty")]
    EmptyKeyId,
    #[error("identity encryption key is invalid base64")]
    InvalidBase64Key,
    #[error("identity encryption key must decode to 32 bytes")]
    InvalidKeyLength,
    #[error("duplicate identity encryption key id: {0}")]
    DuplicateKeyId(String),
    #[error("unknown identity encryption key id")]
    UnknownKeyId,
    #[error("identity encryption nonce must be 12 bytes")]
    InvalidNonceLength,
    #[error("failed to encrypt delegated identity")]
    EncryptFailed,
    #[error("failed to decrypt delegated identity")]
    DecryptFailed,
}

impl IdentityEncryptionKeyRegistry {
    pub fn parse(input: &str) -> Result<Self, IdentityCryptoError> {
        let mut keys = BTreeMap::new();

        for entry in input
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
        {
            let (id, encoded_key) = entry
                .split_once(':')
                .ok_or(IdentityCryptoError::InvalidKeyEntry)?;
            let id = id.trim();
            if id.is_empty() {
                return Err(IdentityCryptoError::EmptyKeyId);
            }
            if keys.contains_key(id) {
                return Err(IdentityCryptoError::DuplicateKeyId(id.to_string()));
            }

            let key_bytes = STANDARD
                .decode(encoded_key.trim())
                .map_err(|_| IdentityCryptoError::InvalidBase64Key)?;
            let bytes: [u8; 32] = key_bytes
                .try_into()
                .map_err(|_| IdentityCryptoError::InvalidKeyLength)?;

            keys.insert(
                id.to_string(),
                IdentityEncryptionKey {
                    id: id.to_string(),
                    bytes,
                },
            );
        }

        if keys.is_empty() {
            return Err(IdentityCryptoError::EmptyRegistry);
        }

        Ok(Self { keys })
    }

    pub fn get(&self, key_id: &str) -> Option<&IdentityEncryptionKey> {
        self.keys.get(key_id)
    }
}

pub fn encrypt_delegated_identity(
    delegated_identity: &[u8],
    registry: &IdentityEncryptionKeyRegistry,
    active_key_id: &str,
) -> Result<EncryptedDelegatedIdentity, IdentityCryptoError> {
    let key = registry
        .get(active_key_id)
        .ok_or(IdentityCryptoError::UnknownKeyId)?;
    let cipher = Aes256Gcm::new_from_slice(&key.bytes).expect("AES-256-GCM accepts 32-byte keys");

    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), delegated_identity)
        .map_err(|_| IdentityCryptoError::EncryptFailed)?;

    Ok(EncryptedDelegatedIdentity {
        encryption_key_id: key.id.clone(),
        nonce: nonce.to_vec(),
        ciphertext,
    })
}

pub fn decrypt_delegated_identity(
    encrypted: &EncryptedDelegatedIdentity,
    registry: &IdentityEncryptionKeyRegistry,
) -> Result<Vec<u8>, IdentityCryptoError> {
    let key = registry
        .get(&encrypted.encryption_key_id)
        .ok_or(IdentityCryptoError::UnknownKeyId)?;

    if encrypted.nonce.len() != 12 {
        return Err(IdentityCryptoError::InvalidNonceLength);
    }

    let cipher = Aes256Gcm::new_from_slice(&key.bytes).expect("AES-256-GCM accepts 32-byte keys");
    cipher
        .decrypt(
            Nonce::from_slice(&encrypted.nonce),
            encrypted.ciphertext.as_ref(),
        )
        .map_err(|_| IdentityCryptoError::DecryptFailed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZERO_KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    const ONE_KEY: &str = "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=";

    #[test]
    fn delegated_identity_encrypts_and_decrypts_with_key_id() {
        let registry =
            IdentityEncryptionKeyRegistry::parse(&format!("v1:{ZERO_KEY},v2:{ONE_KEY}")).unwrap();

        let encrypted =
            encrypt_delegated_identity(b"delegated identity bytes", &registry, "v2").unwrap();

        assert_eq!(encrypted.encryption_key_id, "v2");
        assert_eq!(encrypted.nonce.len(), 12);
        assert_ne!(encrypted.ciphertext, b"delegated identity bytes");
        assert_eq!(
            decrypt_delegated_identity(&encrypted, &registry).unwrap(),
            b"delegated identity bytes"
        );
    }

    #[test]
    fn decrypting_existing_row_fails_when_old_key_is_missing() {
        let old_registry = IdentityEncryptionKeyRegistry::parse(&format!("v1:{ZERO_KEY}")).unwrap();
        let encrypted =
            encrypt_delegated_identity(b"delegated identity bytes", &old_registry, "v1").unwrap();
        let new_registry = IdentityEncryptionKeyRegistry::parse(&format!("v2:{ONE_KEY}")).unwrap();

        assert!(matches!(
            decrypt_delegated_identity(&encrypted, &new_registry),
            Err(IdentityCryptoError::UnknownKeyId)
        ));
    }

    #[test]
    fn duplicate_key_ids_are_rejected() {
        assert!(matches!(
            IdentityEncryptionKeyRegistry::parse(&format!("v1:{ZERO_KEY},v1:{ONE_KEY}")),
            Err(IdentityCryptoError::DuplicateKeyId(id)) if id == "v1"
        ));
    }

    #[test]
    fn key_registry_debug_redacts_key_material() {
        let registry = IdentityEncryptionKeyRegistry::parse(&format!("v1:{ZERO_KEY}")).unwrap();
        let debug = format!("{registry:?}");

        assert!(debug.contains("v1"));
        assert!(!debug.contains("bytes: ["));
        assert!(!debug.contains(ZERO_KEY));
    }

    #[test]
    fn encrypted_identity_debug_redacts_ciphertext() {
        let registry = IdentityEncryptionKeyRegistry::parse(&format!("v1:{ZERO_KEY}")).unwrap();
        let encrypted =
            encrypt_delegated_identity(b"delegated identity bytes", &registry, "v1").unwrap();
        let debug = format!("{encrypted:?}");

        assert!(debug.contains("ciphertext_len"));
        assert!(!debug.contains("delegated identity bytes"));
        assert!(!debug.contains("ciphertext: ["));
    }

    #[test]
    fn invalid_nonce_length_is_rejected() {
        let registry = IdentityEncryptionKeyRegistry::parse(&format!("v1:{ZERO_KEY}")).unwrap();
        let encrypted = EncryptedDelegatedIdentity {
            encryption_key_id: "v1".to_string(),
            nonce: vec![0; 11],
            ciphertext: vec![0; 16],
        };

        assert_eq!(
            decrypt_delegated_identity(&encrypted, &registry),
            Err(IdentityCryptoError::InvalidNonceLength)
        );
    }
}
