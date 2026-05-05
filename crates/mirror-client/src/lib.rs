use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, thiserror::Error)]
pub enum MirrorError {
    #[error("job already running (409 Conflict)")]
    AlreadyRunning,
    #[error("unauthorized — check SERVICE_SECRET_TOKEN and system clock")]
    Unauthorized,
    #[error("server error {status}: {body}")]
    ServerError { status: u16, body: String },
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
}

/// Mirrors the server-side AuditResponse.
#[derive(Debug, serde::Deserialize)]
pub struct AuditResponse {
    pub total: i64,
    pub phash_computed: i64,
    pub mirrored: i64,
    pub missing_storj: i64,
    pub missing_hetzner: i64,
    pub cleanup_pending: i64,
    pub failed: i64,
    pub error_count: i64,
    pub duplicate_phashes: Vec<DuplicateEntry>,
}

#[derive(Debug, serde::Deserialize)]
pub struct DuplicateEntry {
    pub phash: String,
    pub video_ids: Vec<String>,
}

pub struct MirrorClient {
    base_url: String,
    secret: String,
    http: reqwest::Client,
}

impl MirrorClient {
    pub fn new(base_url: impl Into<String>, secret: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            secret: secret.into(),
            http: reqwest::Client::new(),
        }
    }

    fn sign(&self, method: &str, path: &str) -> (String, String) {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_secs()
            .to_string();
        let message = format!("{}\n{}\n{}", method, path, ts);
        let mut mac = HmacSha256::new_from_slice(self.secret.as_bytes())
            .expect("HMAC accepts any key length");
        mac.update(message.as_bytes());
        let sig = hex::encode(mac.finalize().into_bytes());
        (ts, sig)
    }

    async fn post_job(&self, path: &str, limit: Option<u64>) -> Result<(), MirrorError> {
        let url = match limit {
            Some(n) => format!("{}{}?limit={}", self.base_url, path, n),
            None => format!("{}{}", self.base_url, path),
        };
        let (ts, sig) = self.sign("POST", path);
        let resp = self
            .http
            .post(url)
            .header("X-Timestamp", &ts)
            .header("Authorization", format!("HMAC-SHA256 {sig}"))
            .send()
            .await?;

        match resp.status().as_u16() {
            202 => Ok(()),
            409 => Err(MirrorError::AlreadyRunning),
            401 | 403 => Err(MirrorError::Unauthorized),
            status => {
                let body = resp.text().await.unwrap_or_default();
                Err(MirrorError::ServerError { status, body })
            }
        }
    }

    /// Generates a signed URL valid for 5 minutes. Embed directly in a request
    /// without setting any headers — useful for webhooks or browser-initiated calls.
    pub fn signed_url(&self, method: &str, path: &str) -> String {
        let (ts, sig) = self.sign(method, path);
        format!("{}{}?timestamp={}&sig={}", self.base_url, path, ts, sig)
    }

    pub async fn scan_storj(&self, limit: Option<u64>) -> Result<(), MirrorError> {
        self.post_job("/mirror/jobs/scan-storj", limit).await
    }

    pub async fn scan_hetzner(&self, limit: Option<u64>) -> Result<(), MirrorError> {
        self.post_job("/mirror/jobs/scan-hetzner", limit).await
    }

    pub async fn phash_backfill(&self, limit: Option<u64>) -> Result<(), MirrorError> {
        self.post_job("/mirror/jobs/phash", limit).await
    }

    pub async fn mirror(&self, limit: Option<u64>) -> Result<(), MirrorError> {
        self.post_job("/mirror/jobs/mirror", limit).await
    }

    pub async fn audit(&self) -> Result<AuditResponse, MirrorError> {
        let path = "/mirror/audit";
        let (ts, sig) = self.sign("GET", path);
        let resp = self
            .http
            .get(format!("{}{}", self.base_url, path))
            .header("X-Timestamp", &ts)
            .header("Authorization", format!("HMAC-SHA256 {sig}"))
            .send()
            .await?;

        match resp.status().as_u16() {
            200 => Ok(resp.json::<AuditResponse>().await?),
            401 | 403 => Err(MirrorError::Unauthorized),
            status => {
                let body = resp.text().await.unwrap_or_default();
                Err(MirrorError::ServerError { status, body })
            }
        }
    }
}
