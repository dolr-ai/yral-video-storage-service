#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RateLimiterRequestKey {
    pub principal: String,
    pub counter: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimiterTokenType {
    Free,
    Paid,
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

#[cfg(test)]
mod tests {
    use super::{prepare_create_request_options, RateLimiterRequestKey, RateLimiterTokenType};

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
    fn create_options_preserve_mobile_token_type_without_deducting_tokens() {
        let options = prepare_create_request_options(
            RateLimiterRequestKey {
                principal: "aaaaa-aa".to_string(),
                counter: 123,
            },
            Some(RateLimiterTokenType::Paid),
        );

        assert_eq!(options.token_type, RateLimiterTokenType::Paid);
        assert!(!options.is_paid);
        assert_eq!(options.payment_amount, None);
    }
}
