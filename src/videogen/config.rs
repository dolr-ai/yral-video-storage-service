use crate::consts;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModerationMode {
    Remote,
    MockAllow,
}

impl ModerationMode {
    fn parse(value: &str) -> Result<Self, VideogenConfigError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "remote" => Ok(Self::Remote),
            "mock_allow" => Ok(Self::MockAllow),
            _ => Err(VideogenConfigError::InvalidModerationMode(
                value.to_string(),
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Remote => "remote",
            Self::MockAllow => "mock_allow",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VastSubmitTransport {
    Http,
    RabbitMq,
}

impl VastSubmitTransport {
    pub fn parse(value: &str) -> Result<Self, VideogenConfigError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "http" => Ok(Self::Http),
            "rabbitmq" | "amqp" => Ok(Self::RabbitMq),
            _ => Err(VideogenConfigError::InvalidVastSubmitTransport(
                value.to_string(),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideogenConfig {
    pub moderation_timeout_ms: u64,
    pub generate_dedupe_window_secs: u64,
    pub vast_submit_timeout_secs: u64,
    pub upload_destination_timeout_secs: u64,
    pub vast_image_stage_timeout_secs: u64,
    pub ltx_generation_timeout_secs: u64,
    pub vast_upload_expiry_refresh_margin_secs: u64,
    pub upload_url_ttl_secs: u64,
    pub completion_hmac_skew_secs: u64,
    pub vast_submit_transport: VastSubmitTransport,
    pub rabbitmq_amqps_urls: Vec<String>,
    pub rabbitmq_exchange: String,
    pub rabbitmq_routing_key: String,
    pub rabbitmq_publish_timeout_secs: u64,
    pub rabbitmq_connection_name: String,
    pub rabbitmq_tls_ca_cert_pem_b64: Option<String>,
    pub moderation_mode: ModerationMode,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VideogenConfigError {
    #[error("{name} must be a valid integer: {value}")]
    InvalidInteger { name: &'static str, value: String },
    #[error("MODERATION_MODE must be remote or mock_allow: {0}")]
    InvalidModerationMode(String),
    #[error("MODERATION_MODE=mock_allow is not allowed when ENVIRONMENT=production")]
    MockModerationInProduction,
    #[error("VIDEOGEN_VAST_SUBMIT_TRANSPORT must be http, rabbitmq, or amqp: {0}")]
    InvalidVastSubmitTransport(String),
    #[error(
        "VIDEOGEN_RABBITMQ_AMQPS_URLS is required when VIDEOGEN_VAST_SUBMIT_TRANSPORT=rabbitmq"
    )]
    RabbitMqUrlsRequired,
}

impl VideogenConfig {
    pub fn from_env() -> Result<Self, VideogenConfigError> {
        let cfg = Self {
            moderation_timeout_ms: read_u64(consts::MODERATION_TIMEOUT_MS, 3000)?,
            generate_dedupe_window_secs: read_u64(
                consts::VIDEOGEN_GENERATE_DEDUPE_WINDOW_SECS,
                120,
            )?,
            vast_submit_timeout_secs: read_u64(consts::VIDEOGEN_VAST_SUBMIT_TIMEOUT_SECS, 10)?,
            upload_destination_timeout_secs: read_u64(
                consts::VIDEOGEN_UPLOAD_DESTINATION_TIMEOUT_SECS,
                10,
            )?,
            vast_image_stage_timeout_secs: read_u64(
                consts::VIDEOGEN_VAST_IMAGE_STAGE_TIMEOUT_SECS,
                30,
            )?,
            ltx_generation_timeout_secs: read_u64(
                consts::VIDEOGEN_LTX_GENERATION_TIMEOUT_SECS,
                1800,
            )?,
            vast_upload_expiry_refresh_margin_secs: read_u64(
                consts::VIDEOGEN_VAST_UPLOAD_EXPIRY_REFRESH_MARGIN_SECS,
                300,
            )?,
            upload_url_ttl_secs: read_u64(consts::VIDEOGEN_UPLOAD_URL_TTL_SECS, 4200)?,
            completion_hmac_skew_secs: read_u64(consts::VIDEOGEN_COMPLETION_HMAC_SKEW_SECS, 120)?,
            vast_submit_transport: VastSubmitTransport::parse(
                &std::env::var(consts::VIDEOGEN_VAST_SUBMIT_TRANSPORT)
                    .unwrap_or_else(|_| "http".to_string()),
            )?,
            rabbitmq_amqps_urls: {
                let password =
                    std::env::var(consts::VIDEOGEN_RABBITMQ_PUBLISHER_PASSWORD).unwrap_or_default();
                if !password.is_empty() {
                    vec![
                        format!("amqps://prakash_videogen_publisher:{password}@94.130.13.115:5671/%2Fvideogen"),
                        format!("amqps://prakash_videogen_publisher:{password}@88.99.151.102:5671/%2Fvideogen"),
                        format!("amqps://prakash_videogen_publisher:{password}@138.201.129.173:5671/%2Fvideogen"),
                    ]
                } else {
                    std::env::var(consts::VIDEOGEN_RABBITMQ_AMQPS_URLS)
                        .map(|s| {
                            s.split(',')
                                .map(|u| u.trim().to_string())
                                .filter(|u| !u.is_empty())
                                .collect()
                        })
                        .unwrap_or_default()
                }
            },
            rabbitmq_exchange: std::env::var(consts::VIDEOGEN_RABBITMQ_EXCHANGE)
                .unwrap_or_else(|_| "videogen.jobs".to_string()),
            rabbitmq_routing_key: std::env::var(consts::VIDEOGEN_RABBITMQ_ROUTING_KEY)
                .unwrap_or_else(|_| "ltx.generate".to_string()),
            rabbitmq_publish_timeout_secs: read_u64(
                consts::VIDEOGEN_RABBITMQ_PUBLISH_TIMEOUT_SECS,
                10,
            )?,
            rabbitmq_connection_name: std::env::var(consts::VIDEOGEN_RABBITMQ_CONNECTION_NAME)
                .unwrap_or_else(|_| "yral-video-storage-service-videogen-publisher".to_string()),
            rabbitmq_tls_ca_cert_pem_b64: std::env::var(
                consts::VIDEOGEN_RABBITMQ_TLS_CA_CERT_PEM_B64,
            )
            .ok(),
            moderation_mode: Self::parse_moderation_mode(
                &std::env::var(consts::MODERATION_MODE).unwrap_or_else(|_| "remote".to_string()),
            )?,
        };
        cfg.validate_for_environment(
            &std::env::var(consts::ENVIRONMENT).unwrap_or_else(|_| "production".to_string()),
        )?;
        Ok(cfg)
    }

    pub fn test_defaults() -> Self {
        Self {
            moderation_timeout_ms: 3000,
            generate_dedupe_window_secs: 120,
            vast_submit_timeout_secs: 10,
            upload_destination_timeout_secs: 10,
            vast_image_stage_timeout_secs: 30,
            ltx_generation_timeout_secs: 1800,
            vast_upload_expiry_refresh_margin_secs: 300,
            upload_url_ttl_secs: 4200,
            completion_hmac_skew_secs: 120,
            vast_submit_transport: VastSubmitTransport::Http,
            rabbitmq_amqps_urls: vec![],
            rabbitmq_exchange: "videogen.jobs".to_string(),
            rabbitmq_routing_key: "ltx.generate".to_string(),
            rabbitmq_publish_timeout_secs: 10,
            rabbitmq_connection_name: "yral-video-storage-service-videogen-publisher".to_string(),
            rabbitmq_tls_ca_cert_pem_b64: None,
            moderation_mode: ModerationMode::Remote,
        }
    }

    fn parse_moderation_mode(value: &str) -> Result<ModerationMode, VideogenConfigError> {
        ModerationMode::parse(value)
    }

    pub fn validate_for_environment(&self, environment: &str) -> Result<(), VideogenConfigError> {
        if environment.trim().eq_ignore_ascii_case("production")
            && self.moderation_mode == ModerationMode::MockAllow
        {
            return Err(VideogenConfigError::MockModerationInProduction);
        }
        if self.vast_submit_transport == VastSubmitTransport::RabbitMq
            && self.rabbitmq_amqps_urls.is_empty()
        {
            return Err(VideogenConfigError::RabbitMqUrlsRequired);
        }
        Ok(())
    }
}

fn read_u64(name: &'static str, default: u64) -> Result<u64, VideogenConfigError> {
    match std::env::var(name) {
        Ok(value) => value
            .parse()
            .map_err(|_| VideogenConfigError::InvalidInteger { name, value }),
        Err(_) => Ok(default),
    }
}

#[allow(dead_code)]
fn read_u32(name: &'static str, default: u32) -> Result<u32, VideogenConfigError> {
    match std::env::var(name) {
        Ok(value) => value
            .parse()
            .map_err(|_| VideogenConfigError::InvalidInteger { name, value }),
        Err(_) => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use super::{ModerationMode, VastSubmitTransport, VideogenConfig, VideogenConfigError};

    #[test]
    fn default_submit_transport_is_http_for_rollback() {
        let cfg = VideogenConfig::test_defaults();
        assert_eq!(cfg.vast_submit_transport, VastSubmitTransport::Http);
    }

    #[test]
    fn parses_rabbitmq_submit_transport() {
        assert_eq!(
            VastSubmitTransport::parse("rabbitmq").unwrap(),
            VastSubmitTransport::RabbitMq
        );
        assert_eq!(
            VastSubmitTransport::parse("amqp").unwrap(),
            VastSubmitTransport::RabbitMq
        );
    }

    #[test]
    fn rabbitmq_publish_confirm_timeout_defaults_to_submit_timeout() {
        let cfg = VideogenConfig::test_defaults();
        assert_eq!(
            cfg.rabbitmq_publish_timeout_secs,
            cfg.vast_submit_timeout_secs
        );
    }

    #[test]
    fn upload_url_ttl_default_has_expected_value() {
        let cfg = VideogenConfig::test_defaults();
        assert_eq!(cfg.upload_url_ttl_secs, 4200);
    }

    #[test]
    fn production_rejects_mock_moderation() {
        let mut cfg = VideogenConfig::test_defaults();
        cfg.moderation_mode = ModerationMode::MockAllow;

        assert_eq!(
            cfg.validate_for_environment("production"),
            Err(VideogenConfigError::MockModerationInProduction)
        );
    }

    #[test]
    fn production_rejects_mock_moderation_with_case_and_spacing() {
        let mut cfg = VideogenConfig::test_defaults();
        cfg.moderation_mode = VideogenConfig::parse_moderation_mode("MOCK_ALLOW ").unwrap();

        assert_eq!(
            cfg.validate_for_environment("production"),
            Err(VideogenConfigError::MockModerationInProduction)
        );
    }

    #[test]
    fn unknown_moderation_mode_is_rejected() {
        assert_eq!(
            VideogenConfig::parse_moderation_mode("disabled"),
            Err(VideogenConfigError::InvalidModerationMode(
                "disabled".to_string()
            ))
        );
    }

    #[test]
    fn moderation_mode_accepts_remote_with_case_and_spacing() {
        assert_eq!(
            VideogenConfig::parse_moderation_mode(" REMOTE ").unwrap(),
            ModerationMode::Remote
        );
        assert_eq!(ModerationMode::Remote.as_str(), "remote");
    }
}
