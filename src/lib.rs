pub mod consts;
pub mod db;
pub mod jobs;
pub mod storj_s3_client;

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
        /// The video id
        ///
        /// This is used as object key
        pub video_id: String,
    }
}
