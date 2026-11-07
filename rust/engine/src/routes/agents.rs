//! `/api/v1/agents/*` — capabilities manifest and execution planning.
//! Ports of `stirling.api.routes.agent_capabilities` and
//! `stirling.api.routes.execution`.

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use schemars::JsonSchema;
use serde_json::{json, Value};

use crate::agents::runtime::AgentError;
use crate::agents::user_spec::UserSpecAgent;
use crate::contracts::agent_drafts::{
    AgentDraftRequest, AgentDraftWorkflowResponse, AgentRevisionRequest,
    AgentRevisionWorkflowResponse,
};
use crate::contracts::execution::{
    AgentExecutionRequest, CannotContinueExecutionPayload, NextExecutionAction,
};
use crate::contracts::ledger::{Evidence, FolioManifest};
use crate::contracts::pdf_comments::PdfCommentRequest;
use crate::contracts::pdf_edit::PdfEditRequest;
use crate::contracts::pdf_questions::PdfQuestionRequest;
use crate::error::ApiResult;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/capabilities", get(capabilities))
        .route("/next-action", post(next_action))
        .route("/draft", post(draft))
        .route("/revise", post(revise))
}

fn user_spec(state: &AppState) -> UserSpecAgent {
    let runtime = state.runtime.load();
    UserSpecAgent::new(
        runtime.smart_model.clone(),
        &runtime.settings.chat_provider,
        runtime.pdf_edit.clone(),
    )
}

fn agent_error(e: AgentError) -> crate::error::ApiError {
    crate::error::ApiError::internal(format!("agent draft failed: {e}"))
}

async fn draft(
    State(state): State<AppState>,
    Json(request): Json<AgentDraftRequest>,
) -> ApiResult<Json<AgentDraftWorkflowResponse>> {
    let response = user_spec(&state)
        .draft(&request)
        .await
        .map_err(agent_error)?;
    Ok(Json(response))
}

async fn revise(
    State(state): State<AppState>,
    Json(request): Json<AgentRevisionRequest>,
) -> ApiResult<Json<AgentRevisionWorkflowResponse>> {
    let response = user_spec(&state)
        .revise(&request)
        .await
        .map_err(agent_error)?;
    Ok(Json(response))
}

struct Capability {
    id: &'static str,
    description: &'static str,
    mode: &'static str,
    required_scope: &'static str,
    route: &'static str,
}

/// Schema comes from the contract type itself, matching Python's
/// `input_model.model_json_schema()` — the model is the single source of truth.
fn input_schema<T: JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or_else(|_| json!({"type": "object"}))
}

fn manifest_payload() -> Value {
    let capabilities = [
        Capability {
            id: "pdf-question-answer",
            description: "Answer a natural-language question about a PDF document.",
            mode: "sync",
            required_scope: "mcp.tools.read",
            route: "/api/v1/pdf-question",
        },
        Capability {
            id: "pdf-edit-plan",
            description: "Produce an edit plan (a structured sequence of PDF operations) from a natural-language edit request. The plan is executed by Java through the job pipeline; this capability does not modify files itself.",
            mode: "async",
            required_scope: "mcp.tools.write",
            route: "/api/v1/pdf-edit",
        },
        Capability {
            id: "agent-draft",
            description: "Draft a structured agent specification from a free-text description of the task the user wants automated.",
            mode: "sync",
            required_scope: "mcp.tools.read",
            route: "/api/v1/ai/agents/draft",
        },
        Capability {
            id: "agent-revise",
            description: "Revise an existing draft agent specification based on user feedback or constraint changes.",
            mode: "sync",
            required_scope: "mcp.tools.read",
            route: "/api/v1/ai/agents/revise",
        },
        Capability {
            id: "math-audit-examine",
            description: "Examine a folio manifest of financial / numeric documents and surface the evidence that needs to be checked for arithmetic consistency.",
            mode: "sync",
            required_scope: "mcp.tools.read",
            route: "/api/v1/ai/math-auditor-agent/examine",
        },
        Capability {
            id: "math-audit-deliberate",
            description: "Render a deliberated verdict on a single piece of evidence the examine step surfaced (does the arithmetic check out, with what caveats).",
            mode: "sync",
            required_scope: "mcp.tools.read",
            route: "/api/v1/ai/math-auditor-agent/deliberate",
        },
        Capability {
            id: "pdf-comment-generate",
            description: "Generate inline review comments for a PDF document.",
            mode: "sync",
            required_scope: "mcp.tools.read",
            route: "/api/v1/pdf-comment/generate",
        },
        Capability {
            id: "agent-next-action",
            description: "Decide the next execution step for an in-progress agent workflow. Returns a ToolCall, Completed, or CannotContinue action.",
            mode: "sync",
            required_scope: "mcp.tools.read",
            route: "/api/v1/agents/next-action",
        },
    ];

    let schemas: [Value; 8] = [
        input_schema::<PdfQuestionRequest>(),
        input_schema::<PdfEditRequest>(),
        input_schema::<AgentDraftRequest>(),
        input_schema::<AgentRevisionRequest>(),
        input_schema::<FolioManifest>(),
        input_schema::<Evidence>(),
        input_schema::<PdfCommentRequest>(),
        input_schema::<AgentExecutionRequest>(),
    ];

    let items: Vec<Value> = capabilities
        .iter()
        .zip(schemas.iter())
        .map(|(cap, schema)| {
            json!({
                "id": cap.id,
                "description": cap.description,
                "input_schema": schema,
                "mode": cap.mode,
                "required_scope": cap.required_scope,
                "route": cap.route,
            })
        })
        .collect();
    json!({"version": 1, "capabilities": items})
}

async fn capabilities() -> Json<Value> {
    Json(manifest_payload())
}

/// Python's `ExecutionPlanningAgent.next_action` is itself a stub returning
/// cannot_continue; kept identical until the planner is ported.
async fn next_action(
    Json(request): Json<AgentExecutionRequest>,
) -> ApiResult<Json<NextExecutionAction>> {
    Ok(Json(NextExecutionAction::CannotContinue(
        CannotContinueExecutionPayload {
            reason: format!(
                "Execution planning is not implemented yet for step {}.",
                request.current_step_index
            ),
        },
    )))
}
