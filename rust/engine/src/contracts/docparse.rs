//! DocParse ingestion contracts. Port of `stirling.contracts.docparse`.

use chrono::{DateTime, FixedOffset};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::models::{FileId, OwnerId, PrincipalId};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DocparseCapabilities {
    pub indexing_configured: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MarkdownBlock {
    pub markdown: String,
    pub page_start: i64,
    pub page_end: i64,
    #[serde(default)]
    pub heading_path: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DocChunk {
    pub index: i64,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_start: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_end: Option<i64>,
    #[serde(default)]
    pub heading_path: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IngestRequest {
    pub document_id: FileId,
    #[serde(default = "default_source")]
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_id: Option<OwnerId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_principals: Option<Vec<PrincipalId>>,
    #[serde(default)]
    pub expires_at: Option<DateTime<FixedOffset>>,
    #[serde(default)]
    pub blocks: Vec<MarkdownBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_size: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlap: Option<usize>,
    #[serde(default = "default_true")]
    pub index: bool,
    #[serde(default)]
    pub include_chunks: bool,
}

fn default_source() -> String {
    "docparse".to_string()
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IngestResponse {
    pub document_id: FileId,
    pub chunks_indexed: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunks: Option<Vec<DocChunk>>,
}
