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

impl TextChunk {
    /// `Field(min_length/max_length/ge=...)` checks from the pydantic contract.
    pub fn validate(&self) -> Result<(), String> {
        if self.id.is_empty() || self.id.chars().count() > 64 {
            return Err("chunk id must be 1-64 characters".to_string());
        }
        if self.page < 0 {
            return Err("chunk page must be >= 0".to_string());
        }
        if self.width < 0.0 || self.height < 0.0 {
            return Err("chunk width/height must be >= 0".to_string());
        }
        let len = self.text.chars().count();
        if len == 0 || len > MAX_CHUNK_TEXT_LENGTH {
            return Err(format!(
                "chunk text must be 1-{MAX_CHUNK_TEXT_LENGTH} characters"
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PdfCommentRequest {
    #[serde(alias = "session_id")]
    pub session_id: String,
    #[serde(alias = "user_message")]
    pub user_message: String,
    #[serde(default)]
    pub chunks: Vec<TextChunk>,
}

impl PdfCommentRequest {
    /// `Field(min_length/max_length)` checks from the pydantic contract.
    pub fn validate(&self) -> Result<(), String> {
        let session_len = self.session_id.chars().count();
        if session_len == 0 || session_len > 128 {
            return Err("sessionId must be 1-128 characters".to_string());
        }
        let message_len = self.user_message.chars().count();
        if message_len == 0 || message_len > MAX_USER_MESSAGE_LENGTH {
            return Err(format!(
                "userMessage must be 1-{MAX_USER_MESSAGE_LENGTH} characters"
            ));
        }
        if self.chunks.len() > MAX_CHUNKS_PER_REQUEST {
            return Err(format!(
                "chunks must have at most {MAX_CHUNKS_PER_REQUEST} entries"
            ));
        }
        for chunk in &self.chunks {
            chunk.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PdfCommentInstruction {
    #[serde(alias = "chunk_id")]
    pub chunk_id: String,
    #[serde(alias = "comment_text")]
    pub comment_text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PdfCommentResponse {
    #[serde(alias = "session_id")]
    pub session_id: String,
    #[serde(default)]
    pub comments: Vec<PdfCommentInstruction>,
    pub rationale: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PdfCommentReport {
    #[serde(alias = "annotations_applied")]
    pub annotations_applied: i64,
    #[serde(alias = "instructions_received")]
    pub instructions_received: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
}
