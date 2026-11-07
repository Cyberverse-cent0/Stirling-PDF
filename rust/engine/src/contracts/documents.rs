//! Document-store wire contracts. Port of `stirling.contracts.documents`.

use chrono::{DateTime, FixedOffset};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::models::{FileId, OwnerId, PrincipalId};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PageText {
    #[serde(alias = "page_number")]
    pub page_number: i64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Page {
    #[serde(alias = "page_number")]
    pub page_number: i64,
    pub text: String,
    #[serde(alias = "char_count")]
    pub char_count: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
pub struct PageRange {
    pub start: i64,
    pub end: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct IngestDocumentRequest {
    #[serde(alias = "document_id")]
    pub document_id: FileId,
    pub source: String,
    #[serde(default)]
    #[serde(alias = "page_text")]
    pub page_text: Option<Vec<PageText>>,
    #[serde(alias = "owner_id")]
    pub owner_id: OwnerId,
    #[serde(alias = "read_principals")]
    pub read_principals: Vec<PrincipalId>,
    #[serde(
        default,
        deserialize_with = "crate::contracts::common::de_opt_wire_datetime"
    )]
    #[serde(alias = "expires_at")]
    pub expires_at: Option<DateTime<FixedOffset>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct IngestDocumentResponse {
    #[serde(alias = "document_id")]
    pub document_id: FileId,
    #[serde(alias = "chunks_indexed")]
    pub chunks_indexed: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DeleteDocumentResponse {
    #[serde(alias = "document_id")]
    pub document_id: FileId,
    pub deleted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PurgeOwnerResponse {
    #[serde(alias = "owner_id")]
    pub owner_id: OwnerId,
    pub deleted: i64,
}
