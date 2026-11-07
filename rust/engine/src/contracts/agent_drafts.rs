//! Agent draft contracts. Port of `stirling.contracts.agent_drafts`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::contracts::agent_specs::AgentSpecStep;
use crate::contracts::common::{ConversationMessage, StepKind};
use crate::contracts::pdf_edit::{EditCannotDoPayload, EditClarificationPayload};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentDraftStep {
    #[serde(rename = "kind")]
    #[serde(alias = "step_kind")]
    pub step_kind: DraftStepKind,
    pub title: String,
    pub description: String,
}

/// `kind` accepts either `tool` or `ai_tool` (unlike AgentSpecStep it is not
/// itself discriminated — it is a display label on draft steps).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DraftStepKind {
    Tool,
    AiTool,
}

impl From<StepKind> for DraftStepKind {
    fn from(k: StepKind) -> Self {
        match k {
            StepKind::Tool => Self::Tool,
            StepKind::AiTool => Self::AiTool,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentDraft {
    pub name: String,
    pub description: String,
    pub objective: String,
    #[serde(default)]
    pub steps: Vec<AgentSpecStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentDraftRequest {
    #[serde(alias = "user_message")]
    pub user_message: String,
    #[serde(default)]
    #[serde(alias = "conversation_history")]
    pub conversation_history: Vec<ConversationMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentDraftResponsePayload {
    pub draft: AgentDraft,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentRevisionRequest {
    #[serde(alias = "user_message")]
    pub user_message: String,
    #[serde(default)]
    #[serde(alias = "conversation_history")]
    pub conversation_history: Vec<ConversationMessage>,
    #[serde(alias = "current_draft")]
    pub current_draft: AgentDraft,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentRevisionResponsePayload {
    pub draft: AgentDraft,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum AgentDraftWorkflowResponse {
    #[serde(rename = "draft")]
    Draft(AgentDraftResponsePayload),
    #[serde(rename = "need_clarification")]
    NeedClarification(EditClarificationPayload),
    #[serde(rename = "cannot_do")]
    CannotDo(EditCannotDoPayload),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum AgentRevisionWorkflowResponse {
    #[serde(rename = "draft")]
    Draft(AgentRevisionResponsePayload),
    #[serde(rename = "need_clarification")]
    NeedClarification(EditClarificationPayload),
    #[serde(rename = "cannot_do")]
    CannotDo(EditCannotDoPayload),
}
