//! Config-push contracts. Port of `stirling.contracts.config`.
//! These models tolerate unknown fields (`extra="ignore"`) unlike most
//! contracts, so no `deny_unknown_fields` here.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigModelsSection {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub smart_model: String,
    #[serde(default)]
    pub fast_model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smart_max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast_max_tokens: Option<u32>,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub base_url: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigRagSection {
    #[serde(default)]
    pub embedding_provider: String,
    #[serde(default)]
    pub embedding_model: String,
    #[serde(default)]
    pub embedding_api_key: String,
    #[serde(default)]
    pub embedding_base_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_searches: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigLimitsSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_pages: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_characters: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_max_concurrency: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigPushRequest {
    #[serde(default)]
    pub models: ConfigModelsSection,
    #[serde(default)]
    pub rag: ConfigRagSection,
    #[serde(default)]
    pub limits: ConfigLimitsSection,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfigApplyResponse {
    pub status: String,
    pub provider: String,
    pub smart_model: String,
    pub fast_model: String,
    pub smart_max_tokens: u32,
    pub fast_max_tokens: u32,
    pub rag_embedding_model: String,
    pub rag_top_k: u32,
    pub rag_max_searches: u32,
    pub max_pages: u32,
    pub max_characters: u32,
    pub model_max_concurrency: u32,
    #[serde(default)]
    pub notes: Vec<String>,
}
