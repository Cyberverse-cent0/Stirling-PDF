//! Shared contract types. Struct fields serialize camelCase (pydantic
//! `alias_generator=to_camel` + `serialize_by_alias`); unions are internally
//! tagged on their discriminator (`outcome`/`kind`/`phase`/`type`) exactly as
//! the Python `Field(discriminator=...)` unions are.
//!
//! Validation note: pydantic enforces `Field(ge=...)`, `min_length`, etc. Rust
//! port relies on typed shapes plus agent-side schema validation; wire-level
//! range checks are a known parity gap tracked for the contracts phase.

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

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
pub struct ConversationMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
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
pub struct PdfTextSelection {
    #[serde(alias = "page_number")]
    pub page_number: Option<i64>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ExtractedFileText {
    #[serde(alias = "file_name")]
    pub file_name: String,
    #[serde(default)]
    pub pages: Vec<PdfTextSelection>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NeedContentFileRequest {
    pub file: AiFile,
    #[serde(default)]
    #[serde(alias = "page_numbers")]
    pub page_numbers: Vec<i64>,
    #[serde(alias = "content_types")]
    pub content_types: Vec<PdfContentType>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NeedContentResponse {
    pub files: Vec<NeedContentFileRequest>,
    #[serde(alias = "max_pages")]
    pub max_pages: i64,
    #[serde(alias = "max_characters")]
    pub max_characters: i64,
    // `resume_with`, `reason` and the `outcome` discriminator live on the union.
    #[serde(alias = "resume_with")]
    pub resume_with: SupportedCapability,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NeedIngestResponse {
    #[serde(alias = "resume_with")]
    pub resume_with: SupportedCapability,
    pub reason: String,
    #[serde(alias = "files_to_ingest")]
    pub files_to_ingest: Vec<AiFile>,
    #[serde(default)]
    #[serde(alias = "content_types")]
    pub content_types: Vec<PdfContentType>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
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

/// Pydantic `datetime` accepts a wider ISO-8601 range than `DateTime`'s
/// RFC 3339: a space separator, a missing offset (naive → UTC), and a
/// bare date. Anything else is rejected like pydantic would.
pub fn parse_wire_datetime(s: &str) -> Option<DateTime<FixedOffset>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt);
    }
    let normalized;
    let s = if s.len() > 10 && s.as_bytes()[10] == b' ' {
        normalized = format!("{}T{}", &s[..10], &s[11..]);
        normalized.as_str()
    } else {
        s
    };
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt);
    }
    const NAIVE_FORMATS: &[&str] = &[
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
    ];
    if let Some(ndt) = NAIVE_FORMATS
        .iter()
        .find_map(|f| NaiveDateTime::parse_from_str(s, f).ok())
    {
        return Some(ndt.and_utc().fixed_offset());
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|ndt| ndt.and_utc().fixed_offset())
}

/// Serde `deserialize_with` for `Option<DateTime<FixedOffset>>` fields that
/// must accept the pydantic datetime range (`expires_at` on ingest requests).
pub fn de_opt_wire_datetime<'de, D>(deserializer: D) -> Result<Option<DateTime<FixedOffset>>, D::Error>
where
    D: Deserializer<'de>,
{
    match Option::<String>::deserialize(deserializer)? {
        None => Ok(None),
        Some(s) => parse_wire_datetime(&s)
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom(format!("invalid datetime: {s}"))),
    }
}
