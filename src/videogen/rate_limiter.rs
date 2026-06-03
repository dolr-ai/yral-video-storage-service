use candid::Principal;
use yral_canisters_client::rate_limits::VideoGenRequestKey as CanisterVideoGenRequestKey;

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum RateLimiterError {
    #[error("rate limit exceeded: {0}")]
    Limited(String),
    #[error("rate limiter unavailable: {0}")]
    Unavailable(String),
    #[error("rate limiter rejected request: {0}")]
    Rejected(String),
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
pub struct RateLimiterRequestKey {
    pub principal: String,
    pub counter: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
pub enum RateLimiterTokenType {
    Free,
    Sats,
    Dolr,
    YralProSubscription,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimiterCreateOptions {
    pub request_key: RateLimiterRequestKey,
    pub token_type: RateLimiterTokenType,
    pub is_paid: bool,
    pub payment_amount: Option<u64>,
}

pub fn prepare_create_request_options(
    request_key: RateLimiterRequestKey,
    mobile_token_type: Option<RateLimiterTokenType>,
) -> RateLimiterCreateOptions {
    RateLimiterCreateOptions {
        request_key,
        token_type: mobile_token_type.unwrap_or(RateLimiterTokenType::Free),
        is_paid: false,
        payment_amount: None,
    }
}

pub fn to_canister_request_key(
    request_key: &RateLimiterRequestKey,
) -> Result<CanisterVideoGenRequestKey, RateLimiterError> {
    Ok(CanisterVideoGenRequestKey {
        principal: Principal::from_text(&request_key.principal)
            .map_err(|error| RateLimiterError::Rejected(error.to_string()))?,
        counter: request_key.counter,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        prepare_create_request_options, to_canister_request_key, RateLimiterRequestKey,
        RateLimiterTokenType,
    };

    #[test]
    fn create_options_default_to_free_without_deducting_tokens() {
        let options = prepare_create_request_options(
            RateLimiterRequestKey {
                principal: "aaaaa-aa".to_string(),
                counter: 123,
            },
            None,
        );

        assert_eq!(options.token_type, RateLimiterTokenType::Free);
        assert!(!options.is_paid);
        assert_eq!(options.payment_amount, None);
    }

    #[test]
    fn canister_request_key_rejects_invalid_principal() {
        let key = RateLimiterRequestKey {
            principal: "not-a-principal".to_string(),
            counter: 7,
        };
        assert!(to_canister_request_key(&key).is_err());
    }

    #[test]
    fn create_options_preserve_mobile_token_type_without_deducting_tokens() {
        let options = prepare_create_request_options(
            RateLimiterRequestKey {
                principal: "aaaaa-aa".to_string(),
                counter: 123,
            },
            Some(RateLimiterTokenType::Sats),
        );

        assert_eq!(options.token_type, RateLimiterTokenType::Sats);
        assert!(!options.is_paid);
        assert_eq!(options.payment_amount, None);
    }
}
