use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use rand::RngCore;
use yral_types::delegated_identity::DelegatedIdentityWire;

pub const INTERNAL_ENCRYPTION_SECRET_ENV: &str = "INTERNAL_ENCRYPTION_SECRET";

pub struct IdentityCrypto {
    key: [u8; 32],
}

impl IdentityCrypto {
    /// Build from `INTERNAL_ENCRYPTION_SECRET` (same env var and key derivation as off-chain).
    ///
    /// Key derivation: raw string bytes, truncated/zero-padded to 32 bytes.
    pub fn from_env() -> Result<Self> {
        let secret = std::env::var(INTERNAL_ENCRYPTION_SECRET_ENV)
            .with_context(|| format!("{INTERNAL_ENCRYPTION_SECRET_ENV} must be set"))?;
        if secret.is_empty() {
            return Err(anyhow!(
                "{INTERNAL_ENCRYPTION_SECRET_ENV} must not be empty"
            ));
        }
        let mut key = [0u8; 32];
        let secret_bytes = secret.as_bytes();
        let len = secret_bytes.len().min(32);
        key[..len].copy_from_slice(&secret_bytes[..len]);
        Ok(Self { key })
    }

    pub fn encrypt(&self, identity: &DelegatedIdentityWire) -> Result<String> {
        let json = serde_json::to_string(identity).context("serialize identity")?;
        let cipher =
            Aes256Gcm::new_from_slice(&self.key).map_err(|e| anyhow!("invalid key: {e}"))?;

        let mut nonce_bytes = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext = cipher
            .encrypt(nonce, json.as_bytes())
            .map_err(|e| anyhow!("encrypt: {e}"))?;

        let mut combined = nonce_bytes.to_vec();
        combined.extend_from_slice(&ciphertext);

        Ok(BASE64.encode(combined))
    }

    pub fn decrypt(&self, blob: &str) -> Result<DelegatedIdentityWire> {
        let combined = BASE64
            .decode(blob)
            .context("base64 decode encrypted identity")?;
        if combined.len() < 12 {
            return Err(anyhow!("encrypted payload too short"));
        }

        let (nonce_bytes, ciphertext) = combined.split_at(12);
        let nonce = Nonce::from_slice(nonce_bytes);
        let cipher =
            Aes256Gcm::new_from_slice(&self.key).map_err(|e| anyhow!("invalid key: {e}"))?;

        let plaintext = cipher
            .decrypt(nonce, ciphertext)
            .map_err(|e| anyhow!("decrypt: {e}"))?;

        serde_json::from_slice(&plaintext).context("deserialize identity")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_crypto() -> IdentityCrypto {
        std::env::set_var(
            INTERNAL_ENCRYPTION_SECRET_ENV,
            "test_secret_key_for_unit_tests!!",
        );
        IdentityCrypto::from_env().unwrap()
    }

    #[test]
    fn roundtrip_encrypt_decrypt() {
        let crypto = test_crypto();
        let identity = serde_json::from_str::<DelegatedIdentityWire>(
            r#"{"from_key":[1,2,3],"to_secret":{"crv":"secp256k1","d":"AAECBAUGAAECBAUGAAECBAUGAAECBAUGAAECBAUGAAE=","kty":"EC","x":"AAECBAUGAAECBAUGAAECBAUGAAECBAUGAAECBAUGAAE=","y":"AAECBAUGAAECBAUGAAECBAUGAAECBAUGAAECBAUGAAE="},"delegation_chain":[]}"#,
        )
        .expect("parse test identity");

        let encrypted = crypto.encrypt(&identity).unwrap();
        let decrypted = crypto.decrypt(&encrypted).unwrap();
        assert_eq!(identity.from_key, decrypted.from_key);
    }

    #[test]
    fn decrypt_wrong_data_fails() {
        let crypto = test_crypto();
        let result = crypto.decrypt("aGVsbG8="); // too short / not valid ciphertext
        assert!(result.is_err());
    }

    #[test]
    fn from_env_fails_when_empty() {
        std::env::set_var(INTERNAL_ENCRYPTION_SECRET_ENV, "");
        assert!(IdentityCrypto::from_env().is_err());
    }
}
