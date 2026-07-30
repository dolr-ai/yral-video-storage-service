#![recursion_limit = "256"]
pub mod consts;
pub mod db;
pub mod jobs;
pub mod media_index;
pub mod migrations;
pub mod storj_s3_client;
pub mod videogen;

pub mod s3_client;
pub mod thumbnail;
pub mod transcode;

pub mod move2nsfw {
    use serde::{Deserialize, Serialize};

    /// Args for moving a video to nsfw bucket
    #[derive(Serialize, Deserialize, Debug, Clone, utoipa::ToSchema)]
    pub struct Args {
        /// The publisher user principal supplied to off chain agent
        ///
        /// This used as directory key
        pub publisher_user_id: String,
        /// The video id
        ///
        /// This is used as object key
        pub video_id: String,
    }
}
