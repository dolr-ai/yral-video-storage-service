use aws_config::BehaviorVersion;
use aws_sdk_s3::config::{Credentials, Region};
use aws_sdk_s3::Client;

use crate::consts::{STORJ_GATEWAY_ACCESS_KEY, STORJ_GATEWAY_SECRET_KEY, STORJ_SFW_BUCKET};
use crate::s3_client::{S3Client, S3ObjectInfo};

pub const STORJ_GATEWAY_ENDPOINT: &str = "https://gateway.storjshare.io";

#[derive(Clone)]
pub struct StorjS3Client(S3Client);

impl StorjS3Client {
    pub async fn new() -> Self {
        let creds = Credentials::new(
            STORJ_GATEWAY_ACCESS_KEY.as_str(),
            STORJ_GATEWAY_SECRET_KEY.as_str(),
            None,
            None,
            "storj_gateway",
        );

        let config = aws_sdk_s3::config::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1")) // Storj gateway ignores region
            .endpoint_url(STORJ_GATEWAY_ENDPOINT)
            .credentials_provider(creds)
            .force_path_style(true)
            .build();

        let client = Client::from_conf(config);
        let inner = S3Client::from_raw(client, STORJ_SFW_BUCKET.clone());
        Self(inner)
    }

    pub async fn list_objects(
        &self,
        prefix: Option<&str>,
    ) -> Result<Vec<S3ObjectInfo>, String> {
        self.0.list_objects(prefix).await
    }

    pub async fn object_exists(&self, key: &str) -> Result<bool, String> {
        self.0.object_exists(key).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storj_s3_client_uses_gateway_endpoint() {
        assert_eq!(STORJ_GATEWAY_ENDPOINT, "https://gateway.storjshare.io");
    }
}
