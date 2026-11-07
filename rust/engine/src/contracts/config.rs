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
    #[serde(alias = "smart_model")]
    pub smart_model: String,
    #[serde(default)]
    #[serde(alias = "fast_model")]
    pub fast_model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(alias = "smart_max_tokens")]
    pub smart_max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(alias = "fast_max_tokens")]
    pub fast_max_tokens: Option<u32>,
    #[serde(default)]
    #[serde(alias = "api_key")]
    pub api_key: String,
    #[serde(default)]
    #[serde(alias = "base_url")]
    pub base_url: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigRagSection {
    #[serde(default)]
    #[serde(alias = "embedding_provider")]
    pub embedding_provider: String,
    #[serde(default)]
    #[serde(alias = "embedding_model")]
    pub embedding_model: String,
    #[serde(default)]
    #[serde(alias = "embedding_api_key")]
    pub embedding_api_key: String,
    #[serde(default)]
    #[serde(alias = "embedding_base_url")]
    pub embedding_base_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(alias = "top_k")]
    pub top_k: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(alias = "max_searches")]
    pub max_searches: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigLimitsSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(alias = "max_pages")]
    pub max_pages: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(alias = "max_characters")]
    pub max_characters: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(alias = "model_max_concurrency")]
    pub model_max_concurrency: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ConfigPushRequest {
    #[serde(default)]
    pub models: ConfigModelsSection,
    #[serde(default)]
    pub rag: ConfigRagSection,
    #[serde(default)]
    pub limits: ConfigLimitsSection,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ConfigApplyResponse {
    pub status: String,
    pub provider: String,
    #[serde(alias = "smart_model")]
    pub smart_model: String,
    #[serde(alias = "fast_model")]
    pub fast_model: String,
    #[serde(alias = "smart_max_tokens")]
    pub smart_max_tokens: u32,
    #[serde(alias = "fast_max_tokens")]
    pub fast_max_tokens: u32,
    #[serde(alias = "rag_embedding_model")]
    pub rag_embedding_model: String,
    #[serde(alias = "rag_top_k")]
    pub rag_top_k: u32,
    #[serde(alias = "rag_max_searches")]
    pub rag_max_searches: u32,
    #[serde(alias = "max_pages")]
    pub max_pages: u32,
    #[serde(alias = "max_characters")]
    pub max_characters: u32,
    #[serde(alias = "model_max_concurrency")]
    pub model_max_concurrency: u32,
    #[serde(default)]
    pub notes: Vec<String>,
}
