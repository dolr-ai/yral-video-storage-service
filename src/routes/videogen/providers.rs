use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ProviderCost {
    pub usd_cents: u32,
    pub dolr: u32,
    pub sats: u32,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ProviderItem {
    pub id: String,
    pub name: String,
    pub description: String,
    pub cost: ProviderCost,
    pub supports_image: bool,
    pub supports_negative_prompt: bool,
    pub supports_audio: bool,
    pub supports_seed: bool,
    pub allowed_aspect_ratios: Vec<String>,
    pub allowed_resolutions: Vec<String>,
    pub allowed_durations: Vec<u32>,
    pub default_aspect_ratio: String,
    pub default_resolution: Option<String>,
    pub default_duration: u32,
    pub is_available: bool,
    pub is_internal: bool,
    pub model_icon: Option<String>,
    #[schema(value_type = Object)]
    pub extra_info: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ProvidersResponse {
    pub providers: Vec<ProviderItem>,
}

fn ltx2_provider() -> ProviderItem {
    ProviderItem {
        id: "ltx2".to_string(),
        name: "Ltx2".to_string(),
        description: "LTX video generation".to_string(),
        cost: ProviderCost {
            usd_cents: 0,
            dolr: 0,
            sats: 0,
        },
        supports_image: true,
        supports_negative_prompt: false,
        supports_audio: true,
        supports_seed: true,
        allowed_aspect_ratios: vec!["16:9".to_string(), "9:16".to_string(), "1:1".to_string()],
        allowed_resolutions: vec![],
        allowed_durations: vec![5],
        default_aspect_ratio: "16:9".to_string(),
        default_resolution: None,
        default_duration: 5,
        is_available: true,
        is_internal: false,
        model_icon: None,
        extra_info: serde_json::json!({}),
    }
}

/// List all production-available video generation providers.
#[utoipa::path(
    get,
    path = "/api/v2/videogen/providers",
    tag = "videogen",
    responses(
        (status = 200, description = "List of available providers", body = ProvidersResponse),
    )
)]
pub async fn get_providers() -> Json<ProvidersResponse> {
    Json(ProvidersResponse {
        providers: vec![ltx2_provider()],
    })
}

/// List all video generation providers including disabled/internal ones.
#[utoipa::path(
    get,
    path = "/api/v2/videogen/providers-all",
    tag = "videogen",
    responses(
        (status = 200, description = "List of all providers", body = ProvidersResponse),
    )
)]
pub async fn get_providers_all() -> Json<ProvidersResponse> {
    Json(ProvidersResponse {
        providers: vec![ltx2_provider()],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn providers_returns_ltx2() {
        let Json(resp) = get_providers().await;
        assert_eq!(resp.providers.len(), 1);
        let p = &resp.providers[0];
        assert_eq!(p.id, "ltx2");
        assert_eq!(p.name, "Ltx2");
        assert!(p.is_available);
    }

    #[tokio::test]
    async fn providers_all_returns_same_as_providers() {
        let Json(available) = get_providers().await;
        let Json(all) = get_providers_all().await;
        assert_eq!(available.providers.len(), all.providers.len());
        assert_eq!(available.providers[0].id, all.providers[0].id);
    }
}
