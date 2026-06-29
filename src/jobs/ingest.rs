//! Resolve a videogen upload's `bucket_url` + `object_key` to a storage triple.
//!
//! New videos complete via a videogen "complete" webhook carrying
//! `(video_id, object_key, bucket_url)`. The downstream pHash worker downloads
//! the object by branching on `storage_provider` ("storj" | "hetzner") +
//! `object_key`, so the completion's `bucket_url` must be mapped to the correct
//! storage triple before registration.

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

/// Minimal resolver: videogen uploads are always Storj `yral-sfw` today.
///
/// The videogen `bucket_url` is built from `STORJ_SFW_SHARE_URL`
/// (env `SFW_SHARE_EU1_URL`, e.g.
/// `https://link.storjshare.io/raw/<token>/yral-sfw`) as
/// `{base}/{user_principal}/{video_id}.mp4`, so it reliably contains the
/// bucket name `yral-sfw`. The NSFW share base ends in `yral-nsfw-videos`,
/// which does not contain `yral-sfw` as a substring, so there is no collision.
///
/// Extend (host-parse) when other upload backends are introduced.
pub fn resolve_source(bucket_url: &str, object_key: &str) -> Result<VideoSource, ResolveError> {
    if bucket_url.contains("yral-sfw") {
        Ok(VideoSource {
            storage_provider: "storj",
            bucket: "yral-sfw".into(),
            object_key: object_key.to_string(),
        })
    } else {
        Err(ResolveError::UnknownSource(bucket_url.to_string()))
    }
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
pub async fn on_video_ingested(db_url: &str, video_id: &str, object_key: &str, bucket_url: &str) {
    let src = match resolve_source(bucket_url, object_key) {
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

        let src = resolve_source(
            "https://link.storjshare.io/raw/x/yral-sfw/principal/vid-1.mp4",
            "k/1.mp4",
        )
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
        assert_eq!(r.object_key.as_deref(), Some("k/1.mp4"));
    }

    #[test]
    fn resolves_videogen_storj_sfw() {
        // Realistic bucket_url: STORJ_SFW_SHARE_URL ("https://link.storjshare.io/raw/<token>/yral-sfw")
        // joined with "/{user_principal}/{video_id}.mp4".
        let src = resolve_source(
            "https://link.storjshare.io/raw/jxepcyfzxbj5mk4d676jhsfjpg5a/yral-sfw/canister/abc.mp4",
            "canister/abc.mp4",
        )
        .unwrap();
        assert_eq!(src.storage_provider, "storj");
        assert_eq!(src.bucket, "yral-sfw");
        assert_eq!(src.object_key, "canister/abc.mp4");
    }

    #[test]
    fn rejects_unknown_host() {
        assert!(resolve_source("https://unknown.example/x", "k").is_err());
    }
}
