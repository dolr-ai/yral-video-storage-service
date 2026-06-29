//! Resolve a videogen upload's `bucket_url` + `object_key` to a storage triple.
//!
//! New videos complete via a videogen "complete" webhook carrying
//! `(video_id, object_key, bucket_url)`. The downstream pHash worker downloads
//! the object by branching on `storage_provider` ("storj" | "hetzner") +
//! `object_key`, so the completion's `bucket_url` must be mapped to the correct
//! storage triple before registration.

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

#[cfg(test)]
mod tests {
    use super::*;

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
