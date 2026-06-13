use std::collections::BTreeMap;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestFingerprintInput {
    pub principal: String,
    pub model_id: String,
    pub prompt: String,
    pub negative_prompt: Option<String>,
    pub aspect_ratio: String,
    pub duration: u32,
    pub resolution: String,
    pub seed: Option<i64>,
    pub generate_audio: bool,
    pub upload_handling: String,
    pub token_type: String,
    pub image: ImageIdentityInput,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageIdentityInput {
    None,
    Base64(String),
    Reference(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestFingerprint {
    pub version: u32,
    pub canonical_json: String,
    pub request_fingerprint: String,
    pub image_hash_hex: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FingerprintError {
    #[error("image base64 is invalid")]
    InvalidBase64Image,
    #[error("failed to serialize canonical fingerprint json")]
    SerializeCanonicalJson,
}

pub fn compute_request_fingerprint(
    input: &RequestFingerprintInput,
) -> Result<RequestFingerprint, FingerprintError> {
    let (image_identity_type, image_identity) = compute_image_identity(&input.image)?;

    let mut canonical = BTreeMap::new();
    canonical.insert("aspect_ratio", Value::String(input.aspect_ratio.clone()));
    canonical.insert("duration", Value::from(input.duration));
    canonical.insert("fingerprint_version", Value::from(1));
    canonical.insert("generate_audio", Value::from(input.generate_audio));
    canonical.insert(
        "image_identity",
        image_identity
            .as_ref()
            .map_or(Value::Null, |hash| Value::String(hash.clone())),
    );
    canonical.insert(
        "image_identity_type",
        Value::String(image_identity_type.to_string()),
    );
    canonical.insert("model_id", Value::String(input.model_id.clone()));
    canonical.insert(
        "negative_prompt",
        input
            .negative_prompt
            .as_ref()
            .map_or(Value::Null, |prompt| Value::String(prompt.clone())),
    );
    canonical.insert("principal", Value::String(input.principal.clone()));
    canonical.insert("prompt", Value::String(input.prompt.clone()));
    canonical.insert("resolution", Value::String(input.resolution.clone()));
    canonical.insert("seed", input.seed.map_or(Value::Null, Value::from));
    canonical.insert("token_type", Value::String(input.token_type.clone()));
    canonical.insert(
        "upload_handling",
        Value::String(input.upload_handling.clone()),
    );

    let canonical_json =
        serde_json::to_string(&canonical).map_err(|_| FingerprintError::SerializeCanonicalJson)?;
    let request_fingerprint = sha256_hex(canonical_json.as_bytes());

    Ok(RequestFingerprint {
        version: 1,
        canonical_json,
        request_fingerprint,
        image_hash_hex: image_identity.unwrap_or_default(),
    })
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn compute_image_identity(
    input: &ImageIdentityInput,
) -> Result<(&'static str, Option<String>), FingerprintError> {
    match input {
        ImageIdentityInput::None => Ok(("none", None)),
        ImageIdentityInput::Base64(image_base64) => {
            let bytes = STANDARD
                .decode(image_base64)
                .map_err(|_| FingerprintError::InvalidBase64Image)?;
            Ok(("base64", Some(sha256_hex(&bytes))))
        }
        ImageIdentityInput::Reference(reference) => {
            Ok(("reference", Some(sha256_hex(reference.as_bytes()))))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint_fixture_with_base64_image(image_base64: &str) -> RequestFingerprintInput {
        RequestFingerprintInput {
            principal: "aaaaa-aa".to_string(),
            model_id: "ltx-video".to_string(),
            prompt: "make a video".to_string(),
            negative_prompt: None,
            aspect_ratio: "16:9".to_string(),
            duration: 5,
            resolution: "720p".to_string(),
            seed: Some(42),
            generate_audio: true,
            upload_handling: "server".to_string(),
            token_type: "delegated_identity".to_string(),
            image: ImageIdentityInput::Base64(image_base64.to_string()),
        }
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};

        hex::encode(Sha256::digest(bytes))
    }

    #[test]
    fn fingerprint_hashes_decoded_base64_image_bytes() {
        let req = fingerprint_fixture_with_base64_image("aGVsbG8=");
        let fp = compute_request_fingerprint(&req).unwrap();
        assert_eq!(fp.version, 1);
        assert_eq!(fp.image_hash_hex, sha256_hex(b"hello"));
    }

    #[test]
    fn fingerprint_canonical_json_is_stable_and_sorted() {
        let req = fingerprint_fixture_with_base64_image("aGVsbG8=");
        let fp = compute_request_fingerprint(&req).unwrap();

        assert_eq!(
            fp.canonical_json,
            r#"{"aspect_ratio":"16:9","duration":5,"fingerprint_version":1,"generate_audio":true,"image_identity":"2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824","image_identity_type":"base64","model_id":"ltx-video","negative_prompt":null,"principal":"aaaaa-aa","prompt":"make a video","resolution":"720p","seed":42,"token_type":"delegated_identity","upload_handling":"server"}"#
        );
        assert_eq!(
            fp.request_fingerprint,
            sha256_hex(fp.canonical_json.as_bytes())
        );
    }

    #[test]
    fn fingerprint_includes_negative_prompt_exactly_as_received() {
        let mut req = fingerprint_fixture_with_base64_image("aGVsbG8=");
        req.negative_prompt = Some("  blurry, low contrast  ".to_string());

        let fp = compute_request_fingerprint(&req).unwrap();

        assert!(fp
            .canonical_json
            .contains(r#""negative_prompt":"  blurry, low contrast  ""#));
    }

    #[test]
    fn fingerprint_hashes_exact_url_reference_string() {
        let mut req = fingerprint_fixture_with_base64_image("aGVsbG8=");
        req.image = ImageIdentityInput::Reference("HTTPS://example.test/Image.png?x=1".to_string());

        let fp = compute_request_fingerprint(&req).unwrap();

        assert_eq!(
            fp.image_hash_hex,
            sha256_hex(b"HTTPS://example.test/Image.png?x=1")
        );
    }

    #[test]
    fn image_source_type_prevents_base64_reference_collisions() {
        let base64_req = fingerprint_fixture_with_base64_image("aGVsbG8=");
        let mut reference_req = base64_req.clone();
        reference_req.image = ImageIdentityInput::Reference("hello".to_string());

        let base64_fp = compute_request_fingerprint(&base64_req).unwrap();
        let reference_fp = compute_request_fingerprint(&reference_req).unwrap();

        assert_eq!(base64_fp.image_hash_hex, reference_fp.image_hash_hex);
        assert_ne!(base64_fp.canonical_json, reference_fp.canonical_json);
        assert_ne!(
            base64_fp.request_fingerprint,
            reference_fp.request_fingerprint
        );
        assert!(base64_fp
            .canonical_json
            .contains(r#""image_identity_type":"base64""#));
        assert!(reference_fp
            .canonical_json
            .contains(r#""image_identity_type":"reference""#));
    }
}
