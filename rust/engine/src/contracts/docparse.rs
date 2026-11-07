//! DocParse ingestion contracts. Port of `stirling.contracts.docparse`.

use chrono::{DateTime, FixedOffset};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::models::{FileId, OwnerId, PrincipalId};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DocparseCapabilities {
    #[serde(alias = "indexing_configured")]
    pub indexing_configured: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MarkdownBlock {
    pub markdown: String,
    #[serde(alias = "page_start")]
    pub page_start: i64,
    #[serde(alias = "page_end")]
    pub page_end: i64,
    #[serde(default)]
    #[serde(alias = "heading_path")]
    pub heading_path: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DocChunk {
    pub index: i64,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(alias = "page_start")]
    pub page_start: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(alias = "page_end")]
    pub page_end: Option<i64>,
    #[serde(default)]
    #[serde(alias = "heading_path")]
    pub heading_path: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct IngestRequest {
    #[serde(alias = "document_id")]
    pub document_id: FileId,
    #[serde(default = "default_source")]
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(alias = "owner_id")]
    pub owner_id: Option<OwnerId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(alias = "read_principals")]
    pub read_principals: Option<Vec<PrincipalId>>,
    #[serde(
        default,
        deserialize_with = "crate::contracts::common::de_opt_wire_datetime"
    )]
    #[serde(alias = "expires_at")]
    pub expires_at: Option<DateTime<FixedOffset>>,
    #[serde(default)]
    pub blocks: Vec<MarkdownBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(alias = "chunk_size")]
    pub chunk_size: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlap: Option<usize>,
    #[serde(default = "default_true")]
    pub index: bool,
    #[serde(default)]
    #[serde(alias = "include_chunks")]
    pub include_chunks: bool,
}

fn default_source() -> String {
    "docparse".to_string()
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct IngestResponse {
    #[serde(alias = "document_id")]
    pub document_id: FileId,
    #[serde(alias = "chunks_indexed")]
    pub chunks_indexed: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunks: Option<Vec<DocChunk>>,
}
