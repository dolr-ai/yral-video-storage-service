use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use rand::RngCore;
use yral_types::delegated_identity::DelegatedIdentityWire;

pub const ENCRYPTION_KEYS_ENV: &str = "VIDEOGEN_IDENTITY_ENCRYPTION_KEYS";
pub const ACTIVE_KEY_ID_ENV: &str = "VIDEOGEN_IDENTITY_ACTIVE_KEY_ID";

struct KeyEntry {
    key_id: String,
    key: [u8; 32],
}

pub struct IdentityCrypto {
    keys: Vec<KeyEntry>,
    active_key_id: String,
}

impl IdentityCrypto {
    /// Build from `VIDEOGEN_IDENTITY_ENCRYPTION_KEYS` and `VIDEOGEN_IDENTITY_ACTIVE_KEY_ID`.
    ///
    /// Key format: `key_id1:base64_key1,key_id2:base64_key2`
    pub fn from_env() -> Result<Self> {
        let keys_str = std::env::var(ENCRYPTION_KEYS_ENV)
            .with_context(|| format!("{ENCRYPTION_KEYS_ENV} must be set"))?;
        let active_key_id = std::env::var(ACTIVE_KEY_ID_ENV)
            .with_context(|| format!("{ACTIVE_KEY_ID_ENV} must be set"))?;

        let keys = parse_keys(&keys_str)?;
        if keys.is_empty() {
            return Err(anyhow!("{ENCRYPTION_KEYS_ENV} is empty"));
        }
        if !keys.iter().any(|k| k.key_id == active_key_id) {
            return Err(anyhow!(
                "active key id '{active_key_id}' not found in {ENCRYPTION_KEYS_ENV}"
            ));
        }
        Ok(Self {
            keys,
            active_key_id,
        })
    }

    pub fn encrypt(&self, identity: &DelegatedIdentityWire) -> Result<String> {
        let key_entry = self
            .keys
            .iter()
            .find(|k| k.key_id == self.active_key_id)
            .ok_or_else(|| {
                anyhow!(
                    "active key '{}' missing — invariant violated",
                    self.active_key_id
                )
            })?;

        let json = serde_json::to_string(identity).context("serialize identity")?;
        let cipher =
            Aes256Gcm::new_from_slice(&key_entry.key).map_err(|e| anyhow!("invalid key: {e}"))?;

        let mut nonce_bytes = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext = cipher
            .encrypt(nonce, json.as_bytes())
            .map_err(|e| anyhow!("encrypt: {e}"))?;

        let mut combined = nonce_bytes.to_vec();
        combined.extend_from_slice(&ciphertext);

        Ok(format!("{}:{}", key_entry.key_id, BASE64.encode(combined)))
    }

    pub fn decrypt(&self, blob: &str) -> Result<DelegatedIdentityWire> {
        let (key_id, b64) = blob
            .split_once(':')
            .ok_or_else(|| anyhow!("invalid encrypted blob format"))?;

        let key_entry = self
            .keys
            .iter()
            .find(|k| k.key_id == key_id)
            .ok_or_else(|| anyhow!("unknown key id '{key_id}'"))?;

        let combined = BASE64
            .decode(b64)
            .context("base64 decode encrypted identity")?;
        if combined.len() < 12 {
            return Err(anyhow!("encrypted payload too short"));
        }

        let (nonce_bytes, ciphertext) = combined.split_at(12);
        let nonce = Nonce::from_slice(nonce_bytes);
        let cipher =
            Aes256Gcm::new_from_slice(&key_entry.key).map_err(|e| anyhow!("invalid key: {e}"))?;

        let plaintext = cipher
            .decrypt(nonce, ciphertext)
            .map_err(|e| anyhow!("decrypt: {e}"))?;

        serde_json::from_slice(&plaintext).context("deserialize identity")
    }
}

fn parse_keys(s: &str) -> Result<Vec<KeyEntry>> {
    s.split(',')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (key_id, b64) = part
                .split_once(':')
                .ok_or_else(|| anyhow!("invalid key entry (expected key_id:base64): {part}"))?;
            let raw = BASE64
                .decode(b64)
                .with_context(|| format!("invalid base64 for key '{key_id}'"))?;
            if raw.len() != 32 {
                return Err(anyhow!(
                    "key '{key_id}' must be 32 bytes (got {})",
                    raw.len()
                ));
            }
            let mut key = [0u8; 32];
            key.copy_from_slice(&raw);
            Ok(KeyEntry {
                key_id: key_id.to_string(),
                key,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn test_crypto() -> IdentityCrypto {
        let key_bytes = [0u8; 32];
        let b64 = BASE64.encode(key_bytes);
        std::env::set_var(ENCRYPTION_KEYS_ENV, format!("v1:{b64}"));
        std::env::set_var(ACTIVE_KEY_ID_ENV, "v1");
        IdentityCrypto::from_env().unwrap()
    }

    #[test]
    fn roundtrip_encrypt_decrypt() {
        let crypto = test_crypto();
        let identity = serde_json::from_str::<DelegatedIdentityWire>(
            r#"{"from_key":[],"to_secret":{"crv":"secp256k1","d":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=","kty":"EC","x":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=","y":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="},"delegation_chain":[]}"#,
        ).unwrap_or_else(|_| {
            // Minimal fallback if secp256k1 zero key is invalid
            serde_json::from_str::<DelegatedIdentityWire>(
                r#"{"from_key":[1,2,3],"to_secret":{"crv":"secp256k1","d":"AAECBAUGAAECBAUGAAECBAUGAAECBAUGAAECBAUGAAE=","kty":"EC","x":"AAECBAUGAAECBAUGAAECBAUGAAECBAUGAAECBAUGAAE=","y":"AAECBAUGAAECBAUGAAECBAUGAAECBAUGAAECBAUGAAE="},"delegation_chain":[]}"#
            ).expect("fallback parse")
        });

        let encrypted = crypto.encrypt(&identity).unwrap();
        assert!(encrypted.starts_with("v1:"), "should include key_id prefix");

        let decrypted = crypto.decrypt(&encrypted).unwrap();
        assert_eq!(identity.from_key, decrypted.from_key);
    }

    #[test]
    fn decrypt_wrong_key_id_fails() {
        let crypto = test_crypto();
        let result = crypto.decrypt("v999:aGVsbG8=");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown key id"));
    }

    #[test]
    fn from_env_fails_missing_active_key() {
        let key_bytes = [0u8; 32];
        let b64 = BASE64.encode(key_bytes);
        std::env::set_var(ENCRYPTION_KEYS_ENV, format!("v1:{b64}"));
        std::env::set_var(ACTIVE_KEY_ID_ENV, "v99");
        assert!(IdentityCrypto::from_env().is_err());
    }
}
