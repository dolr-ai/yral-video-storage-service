use crate::videogen::vast::{VastSubmitAccepted, VastSubmitRequest};
use chrono::Utc;
use lapin::{
    options::{BasicPublishOptions, ConfirmSelectOptions},
    tcp::OwnedTLSConfig,
    BasicProperties, Connection, ConnectionProperties,
};
use std::fmt;

pub struct RabbitMqPublishConfig {
    pub amqps_urls: Vec<String>,
    pub exchange: String,
    pub routing_key: String,
    pub connection_name: String,
    pub publish_timeout_secs: u64,
    pub tls_ca_cert_pem_b64: Option<String>,
}

impl fmt::Debug for RabbitMqPublishConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RabbitMqPublishConfig")
            .field(
                "amqps_urls",
                &format!("[{} url(s) redacted]", self.amqps_urls.len()),
            )
            .field("exchange", &self.exchange)
            .field("routing_key", &self.routing_key)
            .field("connection_name", &self.connection_name)
            .field("publish_timeout_secs", &self.publish_timeout_secs)
            .finish_non_exhaustive()
    }
}

pub struct RabbitMqPublishEnvelope {
    pub body: Vec<u8>,
    pub message_id: Option<String>,
    pub correlation_id: Option<String>,
    pub content_type: Option<String>,
    pub persistent: bool,
}

impl RabbitMqPublishEnvelope {
    pub fn from_request(request: &VastSubmitRequest) -> Result<Self, RabbitMqPublishError> {
        Ok(Self {
            body: serde_json::to_vec(request)?,
            message_id: Some(request.request_id.clone()),
            correlation_id: Some(request.request_id.clone()),
            content_type: Some("application/json".to_string()),
            persistent: true,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RabbitMqPublishError {
    #[error("failed to serialize RabbitMQ message: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("RabbitMQ connection failed: {0}")]
    Connect(String),
    #[error("RabbitMQ channel failed: {0}")]
    Channel(String),
    #[error("RabbitMQ publish failed: {0}")]
    Publish(String),
    #[error("RabbitMQ publish timed out")]
    Timeout,
    #[error("RabbitMQ publish was not confirmed")]
    NotConfirmed,
}

pub struct RabbitMqPublisher {
    config: RabbitMqPublishConfig,
}

impl RabbitMqPublisher {
    pub fn new(config: RabbitMqPublishConfig) -> Self {
        Self { config }
    }

    pub async fn publish(
        &self,
        request: VastSubmitRequest,
    ) -> Result<VastSubmitAccepted, RabbitMqPublishError> {
        let envelope = RabbitMqPublishEnvelope::from_request(&request)?;
        let request_id = request.request_id.clone();
        let mut last_error = None;
        for url in &self.config.amqps_urls {
            match self.try_publish_to_url(url, &envelope, &request_id).await {
                Ok(accepted) => return Ok(accepted),
                Err(e) => last_error = Some(e),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            RabbitMqPublishError::Connect("no AMQPS URLs configured".to_string())
        }))
    }

    async fn try_publish_to_url(
        &self,
        url: &str,
        envelope: &RabbitMqPublishEnvelope,
        request_id: &str,
    ) -> Result<VastSubmitAccepted, RabbitMqPublishError> {
        tokio::time::timeout(
            std::time::Duration::from_secs(self.config.publish_timeout_secs),
            self.do_publish(url, envelope, request_id),
        )
        .await
        .map_err(|_| RabbitMqPublishError::Timeout)?
    }

    async fn do_publish(
        &self,
        url: &str,
        envelope: &RabbitMqPublishEnvelope,
        request_id: &str,
    ) -> Result<VastSubmitAccepted, RabbitMqPublishError> {
        let tls_config = self.build_tls_config()?;

        let connection_name = self.config.connection_name.clone();
        let properties =
            ConnectionProperties::default().with_connection_name(connection_name.into());

        let runtime = lapin::runtime::default_runtime()
            .map_err(|e| RabbitMqPublishError::Connect(e.to_string()))?;

        let conn = Connection::connect_with_config(url, properties, tls_config, runtime)
            .await
            .map_err(|e| RabbitMqPublishError::Connect(e.to_string()))?;

        let channel = conn
            .create_channel()
            .await
            .map_err(|e| RabbitMqPublishError::Channel(e.to_string()))?;

        channel
            .confirm_select(ConfirmSelectOptions { nowait: false })
            .await
            .map_err(|e| RabbitMqPublishError::Channel(e.to_string()))?;

        let message_id = envelope.message_id.clone().unwrap_or_default();
        let correlation_id = envelope.correlation_id.clone().unwrap_or_default();
        let content_type = envelope.content_type.clone().unwrap_or_default();

        let confirm = channel
            .basic_publish(
                self.config.exchange.as_str().into(),
                self.config.routing_key.as_str().into(),
                BasicPublishOptions {
                    mandatory: true,
                    ..BasicPublishOptions::default()
                },
                &envelope.body,
                BasicProperties::default()
                    .with_content_type(content_type.as_str().into())
                    .with_message_id(message_id.as_str().into())
                    .with_correlation_id(correlation_id.as_str().into())
                    .with_delivery_mode(if envelope.persistent { 2 } else { 1 }),
            )
            .await
            .map_err(|e| RabbitMqPublishError::Publish(e.to_string()))?;

        let confirmation = confirm
            .await
            .map_err(|e| RabbitMqPublishError::Publish(e.to_string()))?;

        let returned = channel
            .wait_for_confirms()
            .await
            .map_err(|e| RabbitMqPublishError::Publish(e.to_string()))?;

        match confirmation {
            lapin::Confirmation::Ack(None) if returned.is_empty() => {}
            _ => return Err(RabbitMqPublishError::NotConfirmed),
        }

        Ok(VastSubmitAccepted {
            request_id: request_id.to_string(),
            status: "queued".to_string(),
            accepted_at: Utc::now(),
        })
    }

    fn build_tls_config(&self) -> Result<OwnedTLSConfig, RabbitMqPublishError> {
        match &self.config.tls_ca_cert_pem_b64 {
            None => Ok(OwnedTLSConfig::default()),
            Some(b64) => {
                let pem_bytes =
                    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)
                        .map_err(|e| {
                            RabbitMqPublishError::Connect(format!("invalid CA cert base64: {e}"))
                        })?;
                let pem_str = String::from_utf8(pem_bytes).map_err(|e| {
                    RabbitMqPublishError::Connect(format!("CA cert is not valid UTF-8: {e}"))
                })?;
                Ok(OwnedTLSConfig {
                    identity: None,
                    cert_chain: Some(pem_str),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::videogen::rate_limiter::RateLimiterRequestKey;
    use crate::videogen::upload_destination::UploadDestination;
    use crate::videogen::vast::VastSubmitRequest;
    use chrono::{DateTime, Utc};
    use serde_json::json;

    fn sample_vast_submit_request() -> VastSubmitRequest {
        VastSubmitRequest {
            request_id: "018f5fa2-05c7-4b4a-8934-19b1f3c29d49".to_string(),
            request_key: RateLimiterRequestKey {
                principal: "aaaaa-aa".to_string(),
                counter: 123,
            },
            user_principal: "aaaaa-aa".to_string(),
            model_id: "ltx2".to_string(),
            workflow_json: json!({ "workflow": "ltx2" }),
            input: json!({ "prompt": "sunrise" }),
            callback_url: "https://prakash.example/api/v2/videogen/complete".to_string(),
            upload_url_refresh_url: Some(
                "https://prakash.example/api/v2/videogen/upload-url/refresh".to_string(),
            ),
            upload_destination: UploadDestination {
                video_id: "video-1".to_string(),
                object_key: "videos/video-1.mp4".to_string(),
                upload_url: "https://upload.example.test/video-1".to_string(),
                expires_at: "2026-05-27T12:00:00Z".parse::<DateTime<Utc>>().unwrap(),
                bucket_url: Some(
                    "https://link.storjshare.io/raw/example/yral-sfw/aaaaa-aa/video-1.mp4"
                        .to_string(),
                ),
                encrypted_identity: None,
            },
        }
    }

    #[test]
    fn publish_envelope_uses_request_id_for_message_ids() {
        let request = sample_vast_submit_request();
        let envelope = RabbitMqPublishEnvelope::from_request(&request).unwrap();

        assert_eq!(
            envelope.message_id.as_deref(),
            Some(request.request_id.as_str())
        );
        assert_eq!(
            envelope.correlation_id.as_deref(),
            Some(request.request_id.as_str())
        );
        assert_eq!(envelope.content_type.as_deref(), Some("application/json"));
        assert!(envelope.persistent);
    }

    #[test]
    fn publish_envelope_body_is_vast_submit_request_json() {
        let request = sample_vast_submit_request();
        let envelope = RabbitMqPublishEnvelope::from_request(&request).unwrap();
        let decoded: serde_json::Value = serde_json::from_slice(&envelope.body).unwrap();

        assert_eq!(decoded["request_id"], request.request_id);
        assert_eq!(
            decoded["request_key"]["principal"],
            request.request_key.principal
        );
        assert_eq!(
            decoded["upload_destination"]["video_id"],
            request.upload_destination.video_id
        );
    }
}
