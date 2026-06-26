//! Phase-1 finalize helper: POSTs to this service's own `/duplicate_raw/finalize`
//! (a self-hop). Mirrors `yral-video-upload-service`'s `StorjInterface::finalize_upload`.
//! Phase 3 replaces this with an in-process call to the finalize handler.

use std::collections::HashMap;

use reqwest::Url;
use serde_json::json;

use super::types::AppError;

/// Build the finalize URL with URL-encoded query params (S16).
// allow(dead_code): used by update_metadata_impl (Task 1.7b).
#[allow(dead_code)]
pub fn finalize_url(base: &str, publisher_user_id: &str, video_id: &str, is_nsfw: bool) -> String {
    let mut url = Url::parse(&format!(
        "{}/duplicate_raw/finalize",
        base.trim_end_matches('/')
    ))
    .expect("PUBLIC_BASE_URL must be a valid base URL");
    url.query_pairs_mut()
        .append_pair("publisher_user_id", publisher_user_id)
        .append_pair("video_id", video_id)
        .append_pair("is_nsfw", if is_nsfw { "true" } else { "false" });
    url.to_string()
}

/// POST the finalize request (`{ "metadata": {..} }`) to `{base}/duplicate_raw/finalize`.
// allow(dead_code): used by update_metadata_impl (Task 1.7b).
#[allow(dead_code)]
pub async fn finalize_via_http(
    base: &str,
    publisher_user_id: &str,
    video_id: &str,
    is_nsfw: bool,
    metadata: HashMap<String, String>,
) -> Result<(), AppError> {
    let url = finalize_url(base, publisher_user_id, video_id, is_nsfw);
    let resp = reqwest::Client::new()
        .post(&url)
        .json(&json!({ "metadata": metadata }))
        .send()
        .await
        .map_err(|e| AppError::StorageError(e.to_string()))?;

    if resp.status().is_success() {
        Ok(())
    } else {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(AppError::StorageError(format!(
            "finalize returned {status}: {body}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finalize_url_encodes_query() {
        let u = finalize_url("https://x.test", "p abc", "v1", false);
        assert!(u.starts_with("https://x.test/duplicate_raw/finalize?"));
        assert!(!u.contains("p abc"), "publisher must be encoded: {u}");
        assert!(u.contains("video_id=v1"));
        assert!(u.contains("is_nsfw=false"));
    }

    #[test]
    fn finalize_url_trims_trailing_slash() {
        let u = finalize_url("https://x.test/", "p", "v", true);
        assert!(u.starts_with("https://x.test/duplicate_raw/finalize?"));
        assert!(u.contains("is_nsfw=true"));
    }
}
