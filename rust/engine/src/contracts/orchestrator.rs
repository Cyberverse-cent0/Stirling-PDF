//! Orchestrator contracts. Port of `stirling.contracts.orchestrator`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::contracts::common::{AiFile, ConversationMessage, ExtractedFileText};
use crate::contracts::ledger::Verdict;
use crate::models::tool_models::deserialize_endpoints;
use crate::models::{FileId, ToolEndpoint};

use super::common::{OrchestratorResponse, SupportedCapability};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkflowArtifact {
    #[serde(rename = "extracted_text")]
    ExtractedText { files: Vec<ExtractedFileText> },
    #[serde(rename = "tool_report")]
    ToolReport {
        #[serde(default)]
        source_tool: MathAuditorSourceTag,
        report: Verdict,
    },
}

/// `source_tool` on a tool-report artifact; today only math-auditor produces one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default, JsonSchema)]
pub enum MathAuditorSourceTag {
    #[serde(rename = "/api/v1/ai/tools/math-auditor-agent")]
    #[default]
    MathAuditorAgent,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OrchestratorRequest {
    #[serde(alias = "user_message")]
    pub user_message: String,
    #[serde(default)]
    pub files: Vec<AiFile>,
    #[serde(default)]
    #[serde(alias = "conversation_history")]
    pub conversation_history: Vec<ConversationMessage>,
    #[serde(default)]
    pub artifacts: Vec<WorkflowArtifact>,
    #[serde(default)]
    #[serde(alias = "resume_with")]
    pub resume_with: Option<SupportedCapability>,
    /// Reply language (IETF tag); unset falls back to the message's own language.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    /// See `PdfEditRequest.enabled_endpoints`; unknown paths are dropped.
    #[serde(default, deserialize_with = "deserialize_endpoints")]
    #[serde(alias = "enabled_endpoints")]
    pub enabled_endpoints: Vec<ToolEndpoint>,
}

pub type OrchestratorResponseUnion = OrchestratorResponse;

/// `get_extracted_text_artifact` — the extracted-text artifact carried on a
/// resume-turn request, if any.
pub fn get_extracted_text_artifact(
    request: &OrchestratorRequest,
) -> Option<&Vec<crate::contracts::common::ExtractedFileText>> {
    request.artifacts.iter().find_map(|a| match a {
        WorkflowArtifact::ExtractedText { files } => Some(files),
        _ => None,
    })
}

// Keep the FileId import honest for downstream modules that re-export it.
#[allow(dead_code)]
type _FileIdAlias = FileId;
