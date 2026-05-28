#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideogenContextState {
    ContextCreated,
    Submitted,
    Uploaded,
    DraftCreating,
    DraftCreated,
    Complete,
    SubmitFailed,
    StaleFailed,
    DraftFailed,
    Failed,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VideogenContextStateError {
    #[error("unknown videogen context state: {0}")]
    UnknownState(String),
    #[error("invalid videogen context state transition from {from} to {to}")]
    InvalidTransition {
        from: &'static str,
        to: &'static str,
    },
}

impl VideogenContextState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ContextCreated => "context_created",
            Self::Submitted => "submitted",
            Self::Uploaded => "uploaded",
            Self::DraftCreating => "draft_creating",
            Self::DraftCreated => "draft_created",
            Self::Complete => "complete",
            Self::SubmitFailed => "submit_failed",
            Self::StaleFailed => "stale_failed",
            Self::DraftFailed => "draft_failed",
            Self::Failed => "failed",
        }
    }

    pub fn try_from_db(value: &str) -> Result<Self, VideogenContextStateError> {
        match value {
            "context_created" => Ok(Self::ContextCreated),
            "submitted" => Ok(Self::Submitted),
            "uploaded" => Ok(Self::Uploaded),
            "draft_creating" => Ok(Self::DraftCreating),
            "draft_created" => Ok(Self::DraftCreated),
            "complete" => Ok(Self::Complete),
            "submit_failed" => Ok(Self::SubmitFailed),
            "stale_failed" => Ok(Self::StaleFailed),
            "draft_failed" => Ok(Self::DraftFailed),
            "failed" => Ok(Self::Failed),
            _ => Err(VideogenContextStateError::UnknownState(value.to_string())),
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Complete
                | Self::SubmitFailed
                | Self::StaleFailed
                | Self::DraftFailed
                | Self::Failed
        )
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        if self == next {
            return true;
        }
        if self.is_terminal() {
            return false;
        }

        match self {
            Self::ContextCreated => matches!(
                next,
                Self::Submitted | Self::SubmitFailed | Self::StaleFailed | Self::Failed
            ),
            Self::Submitted => matches!(
                next,
                Self::Uploaded | Self::SubmitFailed | Self::StaleFailed | Self::Failed
            ),
            Self::Uploaded => {
                matches!(next, Self::DraftCreating | Self::StaleFailed | Self::Failed)
            }
            Self::DraftCreating => matches!(
                next,
                Self::DraftCreated | Self::DraftFailed | Self::StaleFailed | Self::Failed
            ),
            Self::DraftCreated => matches!(
                next,
                Self::Complete | Self::DraftFailed | Self::StaleFailed | Self::Failed
            ),
            Self::Complete
            | Self::SubmitFailed
            | Self::StaleFailed
            | Self::DraftFailed
            | Self::Failed => false,
        }
    }

    pub fn ensure_can_transition_to(self, next: Self) -> Result<(), VideogenContextStateError> {
        if self.can_transition_to(next) {
            Ok(())
        } else {
            Err(VideogenContextStateError::InvalidTransition {
                from: self.as_str(),
                to: next.as_str(),
            })
        }
    }
}
