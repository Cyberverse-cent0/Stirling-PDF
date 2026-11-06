//! PDF comment agent contracts. Port of `stirling.contracts.pdf_comments`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const MAX_USER_MESSAGE_LENGTH: usize = 4_000;
pub const MAX_CHUNK_TEXT_LENGTH: usize = 1_000;
pub const MAX_COMMENT_TEXT_LENGTH: usize = 2_000;
pub const MAX_CHUNKS_PER_REQUEST: usize = 2_500;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TextChunk {
    pub id: String,
    pub page: i64,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PdfCommentRequest {
    pub session_id: String,
    pub user_message: String,
    #[serde(default)]
    pub chunks: Vec<TextChunk>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PdfCommentInstruction {
    pub chunk_id: String,
    pub comment_text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PdfCommentResponse {
    pub session_id: String,
    #[serde(default)]
    pub comments: Vec<PdfCommentInstruction>,
    pub rationale: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PdfCommentReport {
    pub annotations_applied: i64,
    pub instructions_received: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
}
