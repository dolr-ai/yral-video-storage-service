use std::fmt;

#[derive(Clone, PartialEq, Eq)]
pub struct ModerationInput {
    pub request_id: String,
    pub user_principal: String,
    pub prompt: String,
    pub image_url: Option<String>,
}

impl fmt::Debug for ModerationInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModerationInput")
            .field("request_id", &self.request_id)
            .field("user_principal", &self.user_principal)
            .field("prompt", &"<redacted>")
            .field("image_url", &self.image_url.as_ref().map(|_| "<redacted>"))
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
    use super::{MockAllowModerationClient, ModerationClient, ModerationDecision, ModerationInput};
    use crate::videogen::config::{ModerationMode, VideogenConfig, VideogenConfigError};

    #[tokio::test]
    async fn mock_allow_returns_safe_decision() {
        let client = MockAllowModerationClient;
        let decision = client
            .moderate(ModerationInput {
                request_id: "018f5fa2-05c7-4b4a-8934-19b1f3c29d49".to_string(),
                user_principal: "aaaaa-aa".to_string(),
                prompt: "make a sunrise over mountains".to_string(),
                image_url: Some("https://example.test/image.png".to_string()),
            })
            .await
            .unwrap();

        assert_eq!(decision, ModerationDecision::Safe);
    }

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
