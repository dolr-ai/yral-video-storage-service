pub mod consts;
pub mod db;
pub mod jobs;
pub mod storj_s3_client;
pub mod duplicate {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Serialize};

    /// Args for duplication request
    #[derive(Serialize, Deserialize, Debug, Clone, utoipa::ToSchema)]
    pub struct Args {
        /// The publisher user principal supplied to off chain agent
        ///
        /// This used as directory key
        pub publisher_user_id: String,
        /// The video id on cloudflare
        ///
        /// This is used as object key
        pub video_id: String,
        /// Whether the video contains nsfw content
        ///
        /// This is used for segregation
        pub is_nsfw: bool,
        /// key-value pair to be added to video's metadata on storj
        pub metadata: BTreeMap<String, String>,
    }
}

pub mod s3_client;
pub mod thumbnail;

pub mod move2nsfw {
    use serde::{Deserialize, Serialize};

    /// Args for moving a video to nsfw bucket
    #[derive(Serialize, Deserialize, Debug, Clone, utoipa::ToSchema)]
    pub struct Args {
        /// The publisher user principal supplied to off chain agent
        ///
        /// This used as directory key
        pub publisher_user_id: String,
        /// The video id on cloudflare
        ///
        /// This is used as object key
        pub video_id: String,
    }
}
