//! Document-store wire contracts. Port of `stirling.contracts.documents`.

use chrono::{DateTime, FixedOffset};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::models::{FileId, OwnerId, PrincipalId};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PageText {
    pub page_number: i64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Page {
    pub page_number: i64,
    pub text: String,
    pub char_count: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PageRange {
    pub start: i64,
    pub end: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IngestDocumentRequest {
    pub document_id: FileId,
    pub source: String,
    #[serde(default)]
    pub page_text: Option<Vec<PageText>>,
    pub owner_id: OwnerId,
    pub read_principals: Vec<PrincipalId>,
    #[serde(default)]
    pub expires_at: Option<DateTime<FixedOffset>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IngestDocumentResponse {
    pub document_id: FileId,
    pub chunks_indexed: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeleteDocumentResponse {
    pub document_id: FileId,
    pub deleted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PurgeOwnerResponse {
    pub owner_id: OwnerId,
    pub deleted: i64,
}
