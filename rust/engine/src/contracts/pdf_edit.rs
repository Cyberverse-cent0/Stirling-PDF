//! PDF edit contracts. Port of `stirling.contracts.pdf_edit`.
//! `outcome` is the serde tag on the response unions; payloads omit it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::contracts::common::{
    AiFile, ConversationMessage, ExtractedFileText, NeedContentResponse, SupportedCapability,
    ToolOperationStep,
};
use crate::models::tool_models::deserialize_endpoints;
use crate::models::ToolEndpoint;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PdfEditRequest {
    #[serde(alias = "user_message")]
    pub user_message: String,
    #[serde(default)]
    pub files: Vec<AiFile>,
    #[serde(default)]
    #[serde(alias = "conversation_history")]
    pub conversation_history: Vec<ConversationMessage>,
    #[serde(default)]
    #[serde(alias = "page_text")]
    pub page_text: Vec<ExtractedFileText>,
    /// Java's usable endpoint set; unknown paths are dropped, empty means
    /// "no operations available" (planner returns cannot_do).
    #[serde(default, deserialize_with = "deserialize_endpoints")]
    #[serde(alias = "enabled_endpoints")]
    pub enabled_endpoints: Vec<ToolEndpoint>,
}

impl PdfEditRequest {
    /// Nested `AiFile` entries carry `min_length=1` fields.
    pub fn validate(&self) -> Result<(), String> {
        crate::contracts::common::validate_ai_files(&self.files)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EditPlanPayload {
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
    pub steps: Vec<ToolOperationStep>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(alias = "resume_with")]
    pub resume_with: Option<SupportedCapability>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EditClarificationPayload {
    pub question: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EditCannotDoPayload {
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PdfEditTerminalResponse {
    #[serde(rename = "plan")]
    Plan(EditPlanPayload),
    #[serde(rename = "need_clarification")]
    NeedClarification(EditClarificationPayload),
    #[serde(rename = "cannot_do")]
    CannotDo(EditCannotDoPayload),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PdfEditResponse {
    #[serde(rename = "plan")]
    Plan(EditPlanPayload),
    #[serde(rename = "need_clarification")]
    NeedClarification(EditClarificationPayload),
    #[serde(rename = "cannot_do")]
    CannotDo(EditCannotDoPayload),
    #[serde(rename = "need_content")]
    NeedContent(NeedContentResponse),
}
