//! Resolve a videogen upload's `bucket_url` to a storage triple.
//!
//! New videos complete via a videogen "complete" webhook carrying
//! `(video_id, bucket_url)`. The downstream pHash worker downloads the object by
//! branching on `storage_provider` ("storj" | "hetzner") + `object_key`, so the
//! completion's `bucket_url` (the real download URL, which embeds the
//! principal-prefixed key) is parsed into the correct storage triple before
//! registration.

use tokio_postgres::Client;

use crate::media_index::ServableVideoInput;

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("unrecognized bucket_url: {0}")]
    UnknownSource(String),
}

pub struct VideoSource {
    pub storage_provider: &'static str, // "storj" | "hetzner"
    pub bucket: String,
    pub object_key: String,
}

/// Derive the storage triple from the videogen completion's `bucket_url`.
///
/// `bucket_url` is the real Storj download URL,
/// `{share_base}/yral-sfw/{user_principal}/{video_id}.mp4` (env
/// `SFW_SHARE_EU1_URL` = `https://link.storjshare.io/raw/<token>/yral-sfw`), so
/// the bucket-relative object key is everything after the first `/yral-sfw/`.
///
/// The completion also carries a bare `object_key` request field WITHOUT the
/// principal prefix — taking that as the key was the bug this replaces (the
/// pHash worker GET 404'd). The URL is authoritative, so we parse the key from
/// it instead. The NSFW share base ends in `yral-nsfw-videos`, which does not
/// contain `/yral-sfw/`, so there is no collision.
///
/// Extend (host-parse) when other upload backends are introduced.
pub fn resolve_source(bucket_url: &str) -> Result<VideoSource, ResolveError> {
    const MARKER: &str = "/yral-sfw/";
    let tail = bucket_url
        .split_once(MARKER)
        .map(|(_, rest)| rest)
        .ok_or_else(|| ResolveError::UnknownSource(bucket_url.to_string()))?;
    // Strip any query string / fragment, and surrounding slashes.
    let key = tail
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .trim_matches('/');
    if key.is_empty() {
        return Err(ResolveError::UnknownSource(bucket_url.to_string()));
    }
    Ok(VideoSource {
        storage_provider: "storj",
        bucket: "yral-sfw".into(),
        object_key: key.to_string(),
    })
}

/// Register a completed videogen video directly into the master table
/// `all_servable_videos_on_yral` so the missing-canonical-pHash scan picks it up
/// and a later worker downloads + hashes it.
///
/// This writes the master row via the production `upsert_servable_video_txn`
/// helper (master + source row commit together). It carries the storage
/// provider/key the pHash worker downloads from. We do NOT touch `video_index`
/// or synthesize a legacy row — the daily discovery import owns those, and
/// hashing only reads the master table.
pub(crate) async fn register_master_row(
    client: &mut Client,
    video_id: &str,
    src: &VideoSource,
) -> Result<(), tokio_postgres::Error> {
    let input = ServableVideoInput {
        video_id,
        publisher_user_id: None,
        post_id: None,
        source_kind: "videogen",
        source_ref: Some(video_id),
        servable_status: "servable",
        nsfw_state: None,
        storage_provider: Some(src.storage_provider),
        bucket: Some(&src.bucket),
        object_key: Some(&src.object_key),
        canonical_url: None,
        thumbnail_key: None,
        duration_ms: None,
        width: None,
        height: None,
        fps: None,
        container: None,
        video_codec: None,
        audio_codec: None,
        moov_atom_front: None,
        canonical_encoding_version: None,
        discovered_from: "videogen_completion",
    };

    let tx = client.transaction().await?;
    crate::media_index::upsert_servable_video_txn(&tx, input).await?;
    tx.commit().await?;
    Ok(())
}

/// Best-effort inline registration hook for the videogen "complete" webhook.
///
/// Resolves the upload's `(bucket_url, object_key)` to a storage triple and
/// registers the video into the master table so the pHash worker hashes it.
/// This MUST never panic or propagate — it is called best-effort from a request
/// handler. Any failure is logged; the periodic discovery sweep is the backstop
/// that will eventually register the video anyway.
pub async fn on_video_ingested(db_url: &str, video_id: &str, bucket_url: &str) {
    let src = match resolve_source(bucket_url) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                video_id,
                %e,
                "ingest: unresolved source, skipping inline register (sweep will catch it)"
            );
            return;
        }
    };
    match crate::db::connect(db_url).await {
        Ok(mut client) => {
            if let Err(e) = register_master_row(&mut client, video_id, &src).await {
                tracing::warn!(
                    video_id,
                    error = %e,
                    "ingest: register failed (best-effort; sweep backstop)"
                );
            }
        }
        Err(e) => tracing::warn!(video_id, error = %e, "ingest: db connect failed (best-effort)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_index::test_support::test_client;
    use phash::{HASH_KIND, HASH_VERSION};

    use crate::jobs::media_phash::INPUT_MEDIA_VERSION;

    #[tokio::test]
    async fn registers_video_into_missing_set() {
        let (_pg, mut client) = test_client().await;
        crate::media_index::init_schema(&client).await.unwrap();

        let src = resolve_source("https://link.storjshare.io/raw/x/yral-sfw/principal/vid-1.mp4")
            .unwrap();
        register_master_row(&mut client, "vid-1", &src)
            .await
            .unwrap();

        let rows = crate::media_index::videos_missing_canonical_phash(
            &client,
            HASH_KIND,
            HASH_VERSION,
            INPUT_MEDIA_VERSION,
            None,
            Some(10),
            None,
        )
        .await
        .unwrap();
        let r = rows
            .iter()
            .find(|r| r.video_id == "vid-1")
            .expect("registered row present");
        assert_eq!(r.storage_provider.as_deref(), Some("storj"));
        assert_eq!(r.object_key.as_deref(), Some("principal/vid-1.mp4"));
    }

    #[test]
    fn resolve_source_takes_key_from_bucket_url_not_object_key() {
        // bucket_url is the real download URL: {base}/yral-sfw/{principal}/{uuid}.mp4.
        // The bare `object_key` request field (no prefix) was the bug — must be ignored.
        let src = resolve_source(
            "https://link.storjshare.io/raw/tok/yral-sfw/km5ld-principal/5a08-uuid.mp4",
        )
        .unwrap();
        assert_eq!(src.storage_provider, "storj");
        assert_eq!(src.bucket, "yral-sfw");
        assert_eq!(src.object_key, "km5ld-principal/5a08-uuid.mp4");
    }

    #[test]
    fn resolve_source_strips_query_and_fragment() {
        let src =
            resolve_source("https://link.storjshare.io/raw/tok/yral-sfw/p/u.mp4?download=1#x")
                .unwrap();
        assert_eq!(src.object_key, "p/u.mp4");
    }

    #[test]
    fn resolve_source_rejects_missing_marker() {
        assert!(resolve_source("https://unknown.example/p/u.mp4").is_err());
    }

    #[test]
    fn resolve_source_rejects_empty_key_tail() {
        // bucket base with no object path → cannot derive a key.
        assert!(resolve_source("https://link.storjshare.io/raw/tok/yral-sfw/").is_err());
    }
}
