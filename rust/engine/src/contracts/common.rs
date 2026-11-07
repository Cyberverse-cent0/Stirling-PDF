//! Shared contract types. Struct fields serialize camelCase (pydantic
//! `alias_generator=to_camel` + `serialize_by_alias`); unions are internally
//! tagged on their discriminator (`outcome`/`kind`/`phase`/`type`) exactly as
//! the Python `Field(discriminator=...)` unions are.
//!
//! Validation note: pydantic enforces `Field(ge=...)`, `min_length`, etc. Rust
//! port relies on typed shapes plus agent-side schema validation; wire-level
//! range checks are a known parity gap tracked for the contracts phase.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::contracts::ledger::Verdict;
use crate::models::tool_models::ToolId;
use crate::models::FileId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PdfContentType {
    PageLayout,
    DocumentMetadata,
    EncryptionInfo,
    Bookmarks,
    Layers,
    EmbeddedFiles,
    Javascript,
    Links,
    ImageInfo,
    Fonts,
    PageText,
    FullText,
    FormFields,
    Annotations,
    Signatures,
    StructureTree,
    XmpMetadata,
    Compliance,
    Images,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowOutcome {
    Answer,
    NeedContent,
    NeedIngest,
    NotFound,
    Plan,
    NeedClarification,
    CannotDo,
    Draft,
    ToolCall,
    Completed,
    CannotContinue,
    UnsupportedCapability,
    GenerateFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    ExtractedText,
    PageLayout,
    ToolReport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    Tool,
    AiTool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SupportedCapability {
    Orchestrate,
    PdfEdit,
    PdfQuestion,
    PdfReview,
    PdfCreate,
    AgentDraft,
    AgentRevise,
    AgentNextAction,
    MathAuditorAgent,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConversationMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AiFile {
    pub id: FileId,
    pub name: String,
}

pub fn format_conversation_history(history: &[ConversationMessage]) -> String {
    if history.is_empty() {
        return "None".to_string();
    }
    history
        .iter()
        .map(|m| format!("- {}: {}", m.role, m.content))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn format_file_names(files: &[AiFile]) -> String {
    if files.is_empty() {
        return "No file names were provided.".to_string();
    }
    files
        .iter()
        .map(|f| f.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PdfTextSelection {
    pub page_number: Option<i64>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtractedFileText {
    pub file_name: String,
    #[serde(default)]
    pub pages: Vec<PdfTextSelection>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NeedContentFileRequest {
    pub file: AiFile,
    #[serde(default)]
    pub page_numbers: Vec<i64>,
    pub content_types: Vec<PdfContentType>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NeedContentResponse {
    pub files: Vec<NeedContentFileRequest>,
    pub max_pages: i64,
    pub max_characters: i64,
    // `resume_with`, `reason` and the `outcome` discriminator live on the union.
    pub resume_with: SupportedCapability,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NeedIngestResponse {
    pub resume_with: SupportedCapability,
    pub reason: String,
    pub files_to_ingest: Vec<AiFile>,
    #[serde(default)]
    pub content_types: Vec<PdfContentType>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolOperationStep {
    pub tool: ToolId,
    /// Raw JSON parameters until the typify-generated `tool_params` module lands;
    /// the planner emits them and Java executes them, so Value round-trips cleanly.
    pub parameters: serde_json::Value,
}

/// `ToolReportArtifact` — the only artifact variant today. `kind`/`source_tool`
/// are supplied by the parent `WorkflowArtifact` tag, so the payload carries
/// only the verdict.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolReportArtifactPayload {
    pub report: Verdict,
}

/// The `source_tool` literal on a tool-report artifact (always math-auditor today).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum MathAuditorSource {
    #[serde(rename = "/api/v1/ai/tools/math-auditor-agent")]
    MathAuditorAgent,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GenerateFileResponse {
    pub content: String,
    pub filename: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// Union members shared by `OrchestratorResponse` and `PdfEditResponse`.
/// `outcome` is the serde tag; payload structs intentionally omit it.

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum OrchestratorResponse {
    #[serde(rename = "plan")]
    Plan(crate::contracts::pdf_edit::EditPlanPayload),
    #[serde(rename = "need_content")]
    NeedContent(NeedContentResponse),
    #[serde(rename = "need_ingest")]
    NeedIngest(NeedIngestResponse),
    #[serde(rename = "need_clarification")]
    NeedClarification(crate::contracts::pdf_edit::EditClarificationPayload),
    #[serde(rename = "cannot_do")]
    CannotDo(crate::contracts::pdf_edit::EditCannotDoPayload),
    #[serde(rename = "answer")]
    Answer(crate::contracts::pdf_questions::PdfQuestionAnswerPayload),
    #[serde(rename = "not_found")]
    NotFound(crate::contracts::pdf_questions::PdfQuestionNotFoundPayload),
    #[serde(rename = "draft")]
    Draft(crate::contracts::agent_drafts::AgentDraftResponsePayload),
    #[serde(rename = "generate_file")]
    GenerateFile(GenerateFileResponse),
    #[serde(rename = "tool_call")]
    ToolCall(crate::contracts::execution::ToolCallExecutionPayload),
    #[serde(rename = "completed")]
    Completed(crate::contracts::execution::CompletedExecutionPayload),
    #[serde(rename = "cannot_continue")]
    CannotContinue(crate::contracts::execution::CannotContinueExecutionPayload),
    #[serde(rename = "unsupported_capability")]
    UnsupportedCapability { capability: String, message: String },
}

/// `ArtifactKind`-discriminated artifact union lives in `orchestrator.rs`;
/// re-exported here so `common` stays the import hub like Python's `__init__`.
pub use crate::contracts::orchestrator::WorkflowArtifact;
