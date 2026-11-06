//! User-spec agent — creates or revises a saved agent draft from a request
//! and its edit plan. Port of `stirling.agents.user_spec`.
//!
//! Pipeline: run the PDF-edit agent in terminal mode to get a plan, then a
//! small smart-model call writes the draft's name/description/objective.
//! Revision preserves the current draft's `ai_tool` steps (they are not
//! re-plannable).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::agents::pdf_edit::PdfEditAgent;
use crate::agents::structured::StructuredAgent;
use crate::contracts::agent_drafts::{
    AgentDraft, AgentDraftRequest, AgentDraftResponsePayload, AgentDraftWorkflowResponse,
    AgentRevisionRequest, AgentRevisionResponsePayload, AgentRevisionWorkflowResponse,
};
use crate::contracts::agent_specs::AgentSpecStep;
use crate::contracts::common::{format_conversation_history, ConversationMessage};
use crate::contracts::orchestrator::OrchestratorRequest;
use crate::contracts::pdf_edit::{EditPlanPayload, PdfEditRequest, PdfEditTerminalResponse};
use crate::llm::client::LlmClient;
use crate::services::language::language_directive;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UserSpecMetadata {
    name: String,
    description: String,
    objective: String,
}

pub struct UserSpecAgent {
    pdf_edit_agent: PdfEditAgent,
    agent: StructuredAgent,
}

impl UserSpecAgent {
    pub fn new(smart_model: LlmClient, chat_provider: &str, pdf_edit_agent: PdfEditAgent) -> Self {
        Self {
            pdf_edit_agent,
            agent: StructuredAgent::json::<UserSpecMetadata>(
                smart_model,
                "Create or revise a saved agent draft from the provided request and edit plan. \
                 Return a concise name, description, and objective. \
                 Keep the workflow grounded and practical.",
                chat_provider,
            ),
        }
    }

    /// Orchestrator entry — adapts the orchestrator request into an
    /// `AgentDraftRequest` and runs the standard draft pipeline.
    pub async fn orchestrate(
        &self,
        request: &OrchestratorRequest,
    ) -> Result<AgentDraftWorkflowResponse, crate::agents::runtime::AgentError> {
        self.draft(&AgentDraftRequest {
            user_message: request.user_message.clone(),
            conversation_history: request.conversation_history.clone(),
        })
        .await
    }

    pub async fn draft(
        &self,
        request: &AgentDraftRequest,
    ) -> Result<AgentDraftWorkflowResponse, crate::agents::runtime::AgentError> {
        let edit_plan = self
            .build_edit_plan(&request.user_message, &request.conversation_history)
            .await?;
        let EditPlanPayload {
            summary,
            rationale,
            steps,
            resume_with: _,
        } = match edit_plan {
            PdfEditTerminalResponse::Plan(p) => p,
            PdfEditTerminalResponse::NeedClarification(p) => {
                return Ok(AgentDraftWorkflowResponse::NeedClarification(p));
            }
            PdfEditTerminalResponse::CannotDo(p) => {
                return Ok(AgentDraftWorkflowResponse::CannotDo(p));
            }
        };
        let edit_plan = EditPlanPayload {
            summary,
            rationale,
            steps,
            resume_with: None,
        };
        let draft = self.run_draft_agent(request, &edit_plan).await?;
        Ok(AgentDraftWorkflowResponse::Draft(AgentDraftResponsePayload {
            draft,
        }))
    }

    pub async fn revise(
        &self,
        request: &AgentRevisionRequest,
    ) -> Result<AgentRevisionWorkflowResponse, crate::agents::runtime::AgentError> {
        let edit_plan = self
            .build_edit_plan(
                &format!(
                    "Current objective: {}\nRevision request: {}",
                    request.current_draft.objective, request.user_message
                ),
                &request.conversation_history,
            )
            .await?;
        let plan = match edit_plan {
            PdfEditTerminalResponse::Plan(p) => p,
            PdfEditTerminalResponse::NeedClarification(p) => {
                return Ok(AgentRevisionWorkflowResponse::NeedClarification(p));
            }
            PdfEditTerminalResponse::CannotDo(p) => {
                return Ok(AgentRevisionWorkflowResponse::CannotDo(p));
            }
        };
        let draft = self.run_revision_agent(request, &plan).await?;
        Ok(AgentRevisionWorkflowResponse::Draft(
            AgentRevisionResponsePayload { draft },
        ))
    }

    async fn run_draft_agent(
        &self,
        request: &AgentDraftRequest,
        edit_plan: &EditPlanPayload,
    ) -> Result<AgentDraft, crate::agents::runtime::AgentError> {
        let metadata: UserSpecMetadata = self.agent.run(&build_draft_prompt(request, edit_plan)).await?;
        Ok(AgentDraft {
            name: metadata.name,
            description: metadata.description,
            objective: metadata.objective,
            steps: edit_plan
                .steps
                .iter()
                .cloned()
                .map(AgentSpecStep::Tool)
                .collect(),
        })
    }

    async fn run_revision_agent(
        &self,
        request: &AgentRevisionRequest,
        edit_plan: &EditPlanPayload,
    ) -> Result<AgentDraft, crate::agents::runtime::AgentError> {
        let metadata: UserSpecMetadata =
            self.agent
                .run(&build_revision_prompt(request, edit_plan))
                .await?;
        // `ai_tool` steps are preserved verbatim — they are not re-plannable.
        let mut steps: Vec<AgentSpecStep> = edit_plan
            .steps
            .iter()
            .cloned()
            .map(AgentSpecStep::Tool)
            .collect();
        steps.extend(
            request
                .current_draft
                .steps
                .iter()
                .filter(|s| matches!(s, AgentSpecStep::AiTool(_)))
                .cloned(),
        );
        Ok(AgentDraft {
            name: metadata.name,
            description: metadata.description,
            objective: metadata.objective,
            steps,
        })
    }

    /// `handle(allow_need_content=False)` — a `need_content` pause cannot be
    /// honoured inside a draft, so the edit agent runs terminal-only.
    async fn build_edit_plan(
        &self,
        user_message: &str,
        conversation_history: &[ConversationMessage],
    ) -> Result<PdfEditTerminalResponse, crate::agents::runtime::AgentError> {
        self.pdf_edit_agent
            .handle_terminal(&PdfEditRequest {
                user_message: user_message.to_string(),
                files: Vec::new(),
                conversation_history: conversation_history.to_vec(),
                page_text: Vec::new(),
                enabled_endpoints: Vec::new(),
            })
            .await
            .map_err(|e| crate::agents::runtime::AgentError::Output(e.to_string()))
    }
}

fn plan_json(plan: &EditPlanPayload) -> String {
    serde_json::to_string_pretty(plan).unwrap_or_default()
}

fn build_draft_prompt(request: &AgentDraftRequest, edit_plan: &EditPlanPayload) -> String {
    format!(
        "User request:\n{}\n\n\
         Conversation history:\n{}\n\n\
         Edit plan summary:\n{}\n\n\
         Edit plan rationale:\n{}\n\n\
         Edit plan steps:\n{}\n\n{}",
        request.user_message,
        format_conversation_history(&request.conversation_history),
        edit_plan.summary,
        edit_plan.rationale.as_deref().unwrap_or("None"),
        plan_json(edit_plan),
        language_directive(),
    )
}

fn build_revision_prompt(request: &AgentRevisionRequest, edit_plan: &EditPlanPayload) -> String {
    format!(
        "Revision request:\n{}\n\n\
         Conversation history:\n{}\n\n\
         Current draft:\n{}\n\n\
         Edit plan summary:\n{}\n\n\
         Edit plan rationale:\n{}\n\n\
         Edit plan steps:\n{}\n\n{}",
        request.user_message,
        format_conversation_history(&request.conversation_history),
        serde_json::to_string_pretty(&request.current_draft).unwrap_or_default(),
        edit_plan.summary,
        edit_plan.rationale.as_deref().unwrap_or("None"),
        plan_json(edit_plan),
        language_directive(),
    )
}

