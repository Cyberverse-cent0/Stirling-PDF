//! Document-store wire contracts. Port of `stirling.contracts.documents`.

use chrono::{DateTime, FixedOffset};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::models::{FileId, OwnerId, PrincipalId};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PageText {
    #[serde(alias = "page_number")]
    pub page_number: i64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Page {
    #[serde(alias = "page_number")]
    pub page_number: i64,
    pub text: String,
    #[serde(alias = "char_count")]
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

impl PageText {
    /// `page_number` is `Field(ge=1)` in the pydantic contract.
    pub fn validate(&self) -> Result<(), String> {
        if self.page_number < 1 {
            return Err("pageNumber must be >= 1".to_string());
        }
        Ok(())
    }
}

impl IngestDocumentRequest {
    /// `Field(min_length=1)`/`ge` checks from the pydantic contract.
    pub fn validate(&self) -> Result<(), String> {
        if self.document_id.is_empty() {
            return Err("documentId must not be empty".to_string());
        }
        if self.source.is_empty() {
            return Err("source must not be empty".to_string());
        }
        if self.owner_id.is_empty() {
            return Err("ownerId must not be empty".to_string());
        }
        if self.read_principals.is_empty() {
            return Err("readPrincipals must not be empty".to_string());
        }
        if let Some(pages) = &self.page_text {
            for page in pages {
                page.validate()?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IngestDocumentResponse {
    #[serde(alias = "document_id")]
    pub document_id: FileId,
    #[serde(alias = "chunks_indexed")]
    pub chunks_indexed: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeleteDocumentResponse {
    #[serde(alias = "document_id")]
    pub document_id: FileId,
    pub deleted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PurgeOwnerResponse {
    #[serde(alias = "owner_id")]
    pub owner_id: OwnerId,
    pub deleted: i64,
}
