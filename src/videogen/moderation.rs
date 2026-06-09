use std::fmt;

/// What content to moderate.
/// Note: `detect-base64` endpoint is not yet functional on the service side.
/// Base64 images must be staged to Storj first and passed as `ImageUrl`.
pub enum ModerationSubject {
    TextOnly,
    ImageUrl(String),
}

pub struct ModerationInput {
    pub request_id: String,
    pub user_principal: String,
    pub prompt: String,
    pub subject: ModerationSubject,
}

impl fmt::Debug for ModerationInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let subject = match &self.subject {
            ModerationSubject::TextOnly => "text_only",
            ModerationSubject::ImageUrl(_) => "image_url:<redacted>",
        };
        f.debug_struct("ModerationInput")
            .field("request_id", &self.request_id)
            .field("user_principal", &self.user_principal)
            .field("prompt", &"<redacted>")
            .field("subject", &subject)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModerationDecision {
    Safe,
    Unsafe,
}

#[derive(Debug, thiserror::Error)]
pub enum ModerationError {
    #[error("moderation service request failed: {0}")]
    RequestFailed(String),
}

#[cfg(test)]
mod tests {
    use crate::videogen::config::{ModerationMode, VideogenConfig, VideogenConfigError};

    #[test]
    fn production_rejects_mock_allow_moderation_config() {
        let mut cfg = VideogenConfig::test_defaults();
        cfg.moderation_mode = ModerationMode::MockAllow;

        assert_eq!(
            cfg.validate_for_environment("production"),
            Err(VideogenConfigError::MockModerationInProduction)
        );
    }
}
