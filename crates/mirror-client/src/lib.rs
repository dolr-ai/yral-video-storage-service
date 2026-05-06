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

/// Response from cancel_all endpoint.
#[derive(Debug, serde::Deserialize)]
pub struct CancelResponse {
    pub message: String,
    pub jobs_running_at_cancel: Vec<String>,
}

/// Response from status endpoint.
#[derive(Debug, serde::Deserialize)]
pub struct JobStatus {
    pub scan_storj: bool,
    pub scan_hetzner: bool,
    pub phash: bool,
    pub mirror: bool,
    #[serde(default)]
    pub pipeline: bool,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ConfigResponse {
    pub phash_concurrency: usize,
    pub mirror_concurrency: usize,
    pub scan_page_size: i64,
}

#[derive(Debug, serde::Serialize)]
pub struct ConfigUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phash_concurrency: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mirror_concurrency: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scan_page_size: Option<i64>,
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
    pub status_breakdown: std::collections::HashMap<String, i64>,
    pub duplicate_phashes: Vec<DuplicateEntry>,
}

#[derive(Debug, serde::Deserialize)]
pub struct VideoEntry {
    pub video_id: String,
    pub storj_key: Option<String>,
    pub hetzner_key: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
pub struct DuplicateEntry {
    pub phash: String,
    pub videos: Vec<VideoEntry>,
}

#[derive(Debug, serde::Deserialize)]
pub struct DuplicatesResponse {
    pub total_groups: usize,
    pub total_duplicate_videos: usize,
    pub groups: Vec<DuplicateGroup>,
}

#[derive(Debug, serde::Deserialize)]
pub struct DuplicateGroup {
    pub phash: String,
    pub count: usize,
    pub videos: Vec<VideoEntry>,
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

    async fn post_job(
        &self,
        path: &str,
        limit: Option<u64>,
        prefix: Option<&str>,
    ) -> Result<(), MirrorError> {
        let mut url = self.http.post(format!("{}{}", self.base_url, path));
        if let Some(n) = limit {
            url = url.query(&[("limit", n.to_string())]);
        }
        if let Some(p) = prefix {
            url = url.query(&[("prefix", p)]);
        }
        let (ts, sig) = self.sign("POST", path);
        let resp = url
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

    pub async fn scan_storj(
        &self,
        limit: Option<u64>,
        prefix: Option<&str>,
    ) -> Result<(), MirrorError> {
        self.post_job("/mirror/jobs/scan-storj", limit, prefix)
            .await
    }

    pub async fn scan_hetzner(
        &self,
        limit: Option<u64>,
        prefix: Option<&str>,
    ) -> Result<(), MirrorError> {
        self.post_job("/mirror/jobs/scan-hetzner", limit, prefix)
            .await
    }

    pub async fn phash_backfill(&self, limit: Option<u64>) -> Result<(), MirrorError> {
        self.post_job("/mirror/jobs/phash", limit, None).await
    }

    pub async fn mirror(&self, limit: Option<u64>) -> Result<(), MirrorError> {
        self.post_job("/mirror/jobs/mirror", limit, None).await
    }

    /// Trigger full pipeline on the server and return immediately (fire-and-forget).
    /// Use `job_status` or `audit` to track progress.
    pub async fn trigger_pipeline(
        &self,
        limit: Option<u64>,
        prefix: Option<&str>,
    ) -> Result<(), MirrorError> {
        self.post_job("/mirror/jobs/run-pipeline", limit, prefix)
            .await
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

    pub async fn duplicates(&self) -> Result<DuplicatesResponse, MirrorError> {
        let path = "/mirror/duplicates";
        let (ts, sig) = self.sign("GET", path);
        let resp = self
            .http
            .get(format!("{}{}", self.base_url, path))
            .header("X-Timestamp", &ts)
            .header("Authorization", format!("HMAC-SHA256 {sig}"))
            .send()
            .await?;

        match resp.status().as_u16() {
            200 => Ok(resp.json::<DuplicatesResponse>().await?),
            401 | 403 => Err(MirrorError::Unauthorized),
            status => {
                let body = resp.text().await.unwrap_or_default();
                Err(MirrorError::ServerError { status, body })
            }
        }
    }

    /// Cancel all running background jobs.
    pub async fn cancel_all(&self) -> Result<CancelResponse, MirrorError> {
        let path = "/mirror/jobs/cancel";
        let (ts, sig) = self.sign("POST", path);
        let resp = self
            .http
            .post(format!("{}{}", self.base_url, path))
            .header("X-Timestamp", &ts)
            .header("Authorization", format!("HMAC-SHA256 {sig}"))
            .send()
            .await?;

        match resp.status().as_u16() {
            200 => Ok(resp.json::<CancelResponse>().await?),
            401 | 403 => Err(MirrorError::Unauthorized),
            status => {
                let body = resp.text().await.unwrap_or_default();
                Err(MirrorError::ServerError { status, body })
            }
        }
    }

    /// Get the status of all background jobs.
    pub async fn job_status(&self) -> Result<JobStatus, MirrorError> {
        let path = "/mirror/jobs/status";
        let (ts, sig) = self.sign("GET", path);
        let resp = self
            .http
            .get(format!("{}{}", self.base_url, path))
            .header("X-Timestamp", &ts)
            .header("Authorization", format!("HMAC-SHA256 {sig}"))
            .send()
            .await?;

        match resp.status().as_u16() {
            200 => Ok(resp.json::<JobStatus>().await?),
            401 | 403 => Err(MirrorError::Unauthorized),
            status => {
                let body = resp.text().await.unwrap_or_default();
                Err(MirrorError::ServerError { status, body })
            }
        }
    }

    pub async fn config_get(&self) -> Result<ConfigResponse, MirrorError> {
        let path = "/mirror/config";
        let (ts, sig) = self.sign("GET", path);
        let resp = self
            .http
            .get(format!("{}{}", self.base_url, path))
            .header("X-Timestamp", &ts)
            .header("Authorization", format!("HMAC-SHA256 {sig}"))
            .send()
            .await?;

        match resp.status().as_u16() {
            200 => Ok(resp.json::<ConfigResponse>().await?),
            401 | 403 => Err(MirrorError::Unauthorized),
            status => {
                let body = resp.text().await.unwrap_or_default();
                Err(MirrorError::ServerError { status, body })
            }
        }
    }

    pub async fn config_update(
        &self,
        payload: &ConfigUpdate,
    ) -> Result<ConfigResponse, MirrorError> {
        let path = "/mirror/config";
        let (ts, sig) = self.sign("POST", path);
        let resp = self
            .http
            .post(format!("{}{}", self.base_url, path))
            .json(payload)
            .header("X-Timestamp", &ts)
            .header("Authorization", format!("HMAC-SHA256 {sig}"))
            .send()
            .await?;

        match resp.status().as_u16() {
            200 => Ok(resp.json::<ConfigResponse>().await?),
            401 | 403 => Err(MirrorError::Unauthorized),
            status => {
                let body = resp.text().await.unwrap_or_default();
                Err(MirrorError::ServerError { status, body })
            }
        }
    }
}
