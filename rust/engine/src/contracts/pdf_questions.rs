//! PDF question contracts. Port of `stirling.contracts.pdf_questions`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::contracts::common::{
    AiFile, ConversationMessage, ExtractedFileText, NeedIngestResponse,
};
use crate::contracts::pdf_edit::{EditCannotDoPayload, EditPlanPayload};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PdfQuestionRequest {
    pub question: String,
    #[serde(default)]
    pub files: Vec<AiFile>,
    #[serde(default)]
    #[serde(alias = "conversation_history")]
    pub conversation_history: Vec<ConversationMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PdfQuestionAnswerPayload {
    pub answer: String,
    #[serde(default)]
    pub evidence: Vec<ExtractedFileText>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PdfQuestionNotFoundPayload {
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PdfQuestionTerminalResponse {
    #[serde(rename = "answer")]
    Answer(PdfQuestionAnswerPayload),
    #[serde(rename = "not_found")]
    NotFound(PdfQuestionNotFoundPayload),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PdfQuestionResponse {
    #[serde(rename = "answer")]
    Answer(PdfQuestionAnswerPayload),
    #[serde(rename = "not_found")]
    NotFound(PdfQuestionNotFoundPayload),
    #[serde(rename = "need_ingest")]
    NeedIngest(NeedIngestResponse),
}

/// `orchestrate` may also emit a plan on the math-routing first turn.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PdfQuestionOrchestrateResponse {
    #[serde(rename = "answer")]
    Answer(PdfQuestionAnswerPayload),
    #[serde(rename = "not_found")]
    NotFound(PdfQuestionNotFoundPayload),
    #[serde(rename = "need_ingest")]
    NeedIngest(NeedIngestResponse),
    #[serde(rename = "plan")]
    Plan(EditPlanPayload),
    #[serde(rename = "cannot_do")]
    CannotDo(EditCannotDoPayload),
}
