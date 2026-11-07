//! DocParse ingestion contracts. Port of `stirling.contracts.docparse`.

use chrono::{DateTime, FixedOffset};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::models::{FileId, OwnerId, PrincipalId};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DocparseCapabilities {
    #[serde(alias = "indexing_configured")]
    pub indexing_configured: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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

impl IngestRequest {
    /// `Field(ge=...)`/`min_length` checks from the pydantic contract.
    pub fn validate(&self) -> Result<(), String> {
        if self.document_id.is_empty() {
            return Err("documentId must not be empty".to_string());
        }
        if self.source.is_empty() {
            return Err("source must not be empty".to_string());
        }
        if self.read_principals.as_ref().is_some_and(Vec::is_empty) {
            return Err("readPrincipals must not be empty when provided".to_string());
        }
        for block in &self.blocks {
            if block.page_start < 1 || block.page_end < 1 {
                return Err("block pageStart/pageEnd must be >= 1".to_string());
            }
        }
        if let Some(size) = self.chunk_size {
            if !(64..=32_768).contains(&size) {
                return Err("chunkSize must be between 64 and 32768".to_string());
            }
        }
        if let Some(overlap) = self.overlap {
            if overlap > 4_096 {
                return Err("overlap must be at most 4096".to_string());
            }
        }
        Ok(())
    }
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
    #[serde(alias = "document_id")]
    pub document_id: FileId,
    #[serde(alias = "chunks_indexed")]
    pub chunks_indexed: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunks: Option<Vec<DocChunk>>,
}
