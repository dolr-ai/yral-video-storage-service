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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideogenConfig {
    pub moderation_timeout_ms: u64,
    pub generate_dedupe_window_secs: u64,
    pub vast_submit_timeout_secs: u64,
    pub upload_destination_timeout_secs: u64,
    pub upload_url_pre_submit_margin_secs: u64,
    pub vast_image_stage_timeout_secs: u64,
    pub context_created_timeout_secs: u64,
    pub ltx_generation_timeout_secs: u64,
    pub completion_retry_grace_secs: u64,
    pub vast_upload_retry_window_secs: u64,
    pub vast_upload_expiry_refresh_margin_secs: u64,
    pub upload_url_safety_buffer_secs: u64,
    pub upload_url_ttl_secs: u64,
    pub reconciliation_interval_secs: u64,
    pub reconciliation_batch_size: u32,
    pub draft_create_max_attempts: u32,
    pub draft_create_timeout_secs: u64,
    pub draft_created_complete_timeout_secs: u64,
    pub draft_retry_retention_hours: u64,
    pub completion_hmac_skew_secs: u64,
    pub moderation_mode: ModerationMode,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VideogenConfigError {
    #[error("{name} must be a valid integer: {value}")]
    InvalidInteger { name: &'static str, value: String },
    #[error("ANSUMAN_MODERATION_MODE must be remote or mock_allow: {0}")]
    InvalidModerationMode(String),
    #[error("ANSUMAN_MODERATION_MODE=mock_allow is not allowed when ENVIRONMENT=production")]
    MockModerationInProduction,
}

impl VideogenConfig {
    pub fn from_env() -> Result<Self, VideogenConfigError> {
        let cfg = Self {
            moderation_timeout_ms: read_u64(consts::ANSUMAN_TIMEOUT_MS, 3000)?,
            generate_dedupe_window_secs: read_u64(
                consts::VIDEOGEN_GENERATE_DEDUPE_WINDOW_SECS,
                120,
            )?,
            vast_submit_timeout_secs: read_u64(consts::VIDEOGEN_VAST_SUBMIT_TIMEOUT_SECS, 10)?,
            upload_destination_timeout_secs: read_u64(
                consts::VIDEOGEN_UPLOAD_DESTINATION_TIMEOUT_SECS,
                10,
            )?,
            upload_url_pre_submit_margin_secs: read_u64(
                consts::VIDEOGEN_UPLOAD_URL_PRE_SUBMIT_MARGIN_SECS,
                10,
            )?,
            vast_image_stage_timeout_secs: read_u64(
                consts::VIDEOGEN_VAST_IMAGE_STAGE_TIMEOUT_SECS,
                30,
            )?,
            context_created_timeout_secs: read_u64(
                consts::VIDEOGEN_CONTEXT_CREATED_TIMEOUT_SECS,
                120,
            )?,
            ltx_generation_timeout_secs: read_u64(
                consts::VIDEOGEN_LTX_GENERATION_TIMEOUT_SECS,
                1800,
            )?,
            completion_retry_grace_secs: read_u64(
                consts::VIDEOGEN_COMPLETION_RETRY_GRACE_SECS,
                900,
            )?,
            vast_upload_retry_window_secs: read_u64(
                consts::VIDEOGEN_VAST_UPLOAD_RETRY_WINDOW_SECS,
                900,
            )?,
            vast_upload_expiry_refresh_margin_secs: read_u64(
                consts::VIDEOGEN_VAST_UPLOAD_EXPIRY_REFRESH_MARGIN_SECS,
                300,
            )?,
            upload_url_safety_buffer_secs: read_u64(
                consts::VIDEOGEN_UPLOAD_URL_SAFETY_BUFFER_SECS,
                300,
            )?,
            upload_url_ttl_secs: read_u64(consts::VIDEOGEN_UPLOAD_URL_TTL_SECS, 4200)?,
            reconciliation_interval_secs: read_u64(
                consts::VIDEOGEN_RECONCILIATION_INTERVAL_SECS,
                60,
            )?,
            reconciliation_batch_size: read_u32(consts::VIDEOGEN_RECONCILIATION_BATCH_SIZE, 100)?,
            draft_create_max_attempts: read_u32(consts::VIDEOGEN_DRAFT_CREATE_MAX_ATTEMPTS, 3)?,
            draft_create_timeout_secs: read_u64(consts::VIDEOGEN_DRAFT_CREATE_TIMEOUT_SECS, 600)?,
            draft_created_complete_timeout_secs: read_u64(
                consts::VIDEOGEN_DRAFT_CREATED_COMPLETE_TIMEOUT_SECS,
                120,
            )?,
            draft_retry_retention_hours: read_u64(
                consts::VIDEOGEN_DRAFT_RETRY_RETENTION_HOURS,
                72,
            )?,
            completion_hmac_skew_secs: read_u64(consts::VIDEOGEN_COMPLETION_HMAC_SKEW_SECS, 120)?,
            moderation_mode: Self::parse_moderation_mode(
                &std::env::var(consts::ANSUMAN_MODERATION_MODE)
                    .unwrap_or_else(|_| "remote".to_string()),
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
            upload_url_pre_submit_margin_secs: 10,
            vast_image_stage_timeout_secs: 30,
            context_created_timeout_secs: 120,
            ltx_generation_timeout_secs: 1800,
            completion_retry_grace_secs: 900,
            vast_upload_retry_window_secs: 900,
            vast_upload_expiry_refresh_margin_secs: 300,
            upload_url_safety_buffer_secs: 300,
            upload_url_ttl_secs: 4200,
            reconciliation_interval_secs: 60,
            reconciliation_batch_size: 100,
            draft_create_max_attempts: 3,
            draft_create_timeout_secs: 600,
            draft_created_complete_timeout_secs: 120,
            draft_retry_retention_hours: 72,
            completion_hmac_skew_secs: 120,
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
    use super::{ModerationMode, VideogenConfig, VideogenConfigError};
    use crate::videogen::types::VideogenContextState;

    #[test]
    fn upload_url_ttl_default_exceeds_required_window() {
        let cfg = VideogenConfig::test_defaults();
        assert!(
            cfg.upload_url_ttl_secs
                >= cfg.upload_url_pre_submit_margin_secs
                    + cfg.ltx_generation_timeout_secs
                    + cfg.completion_retry_grace_secs
                    + cfg.vast_upload_retry_window_secs
                    + cfg.upload_url_safety_buffer_secs
        );
        assert_eq!(cfg.upload_url_ttl_secs, 4200);
    }

    #[test]
    fn terminal_states_are_absorbing() {
        assert!(VideogenContextState::Complete.is_terminal());
        assert!(VideogenContextState::SubmitFailed.is_terminal());
        assert!(VideogenContextState::StaleFailed.is_terminal());
        assert!(VideogenContextState::DraftFailed.is_terminal());
        assert!(VideogenContextState::Failed.is_terminal());
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

    #[test]
    fn context_state_round_trips_db_values() {
        assert_eq!(
            VideogenContextState::try_from_db("draft_creating").unwrap(),
            VideogenContextState::DraftCreating
        );
        assert_eq!(
            VideogenContextState::DraftCreating.as_str(),
            "draft_creating"
        );
        assert!(VideogenContextState::try_from_db("draft-creating").is_err());
    }

    #[test]
    fn context_state_rejects_backward_transitions() {
        assert!(
            !VideogenContextState::DraftCreated.can_transition_to(VideogenContextState::Uploaded)
        );
        assert!(VideogenContextState::DraftCreated
            .ensure_can_transition_to(VideogenContextState::Uploaded)
            .is_err());
    }

    #[test]
    fn terminal_states_do_not_transition_to_non_terminal_states() {
        assert!(!VideogenContextState::Complete.can_transition_to(VideogenContextState::Submitted));
        assert!(
            !VideogenContextState::Failed.can_transition_to(VideogenContextState::DraftCreating)
        );
    }
}
