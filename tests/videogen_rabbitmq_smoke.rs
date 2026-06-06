use chrono::{DateTime, Utc};
use serde_json::json;
use storj_interface::videogen::rabbitmq::{RabbitMqPublishConfig, RabbitMqPublisher};
use storj_interface::videogen::rate_limiter::RateLimiterRequestKey;
use storj_interface::videogen::upload_destination::UploadDestination;
use storj_interface::videogen::vast::VastSubmitRequest;

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
        input: json!({ "prompt": "sunrise over mountains" }),
        callback_url: "https://prakash.example/api/v2/videogen/complete".to_string(),
        upload_url_refresh_url: Some(
            "https://prakash.example/api/v2/videogen/upload-url/refresh".to_string(),
        ),
        upload_destination: UploadDestination {
            video_id: "smoke-test-video-1".to_string(),
            object_key: "generated/smoke-test-video-1.mp4".to_string(),
            upload_url: "https://upload.example.test/smoke-test".to_string(),
            expires_at: "2026-05-27T12:00:00Z".parse::<DateTime<Utc>>().unwrap(),
            bucket_url: Some(
                "https://link.storjshare.io/raw/example/yral-sfw/aaaaa-aa/smoke-test-video-1.mp4"
                    .to_string(),
            ),
            encrypted_identity: None,
        },
    }
}

#[tokio::test]
#[ignore = "requires deployed RabbitMQ broker"]
async fn publishes_to_videogen_exchange_with_confirm() {
    let urls = std::env::var("VIDEOGEN_RABBITMQ_AMQPS_URLS")
        .expect("VIDEOGEN_RABBITMQ_AMQPS_URLS required");

    let config = RabbitMqPublishConfig {
        amqps_urls: urls.split(',').map(|s| s.trim().to_string()).collect(),
        exchange: "videogen.jobs".to_string(),
        routing_key: "ltx.generate".to_string(),
        connection_name: "yral-video-storage-service-smoke-test".to_string(),
        publish_timeout_secs: 10,
        tls_ca_cert_pem_b64: std::env::var("VIDEOGEN_RABBITMQ_TLS_CA_CERT_PEM_B64").ok(),
    };

    let accepted = RabbitMqPublisher::new(config)
        .publish(sample_vast_submit_request())
        .await
        .unwrap();

    assert_eq!(accepted.status, "queued");
}
