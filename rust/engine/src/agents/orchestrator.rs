//! Top-level orchestrator. Port of `stirling.agents.orchestrator`.
//!
//! Routes a user message to exactly one delegate: pdf_edit, pdf_question,
//! user_spec (agent draft), pdf_review, pdf_create, or unsupported.
//!
//! Two routing mechanisms, mirroring Python:
//! - Real providers: six `ToolOutput`-style terminal tools — the model picks
//!   a delegate and that call's result IS the response.
//! - Ollama/custom (can't drive zero-arg tool delegates): a small
//!   structured router picks a capability by name, then dispatch runs the
//!   delegate in-process.
//!
//! `resume_with` requests bypass the router entirely — Java re-enters with
//! the capability that emitted the plan.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agents::output_mode::uses_tool_output;
use crate::agents::pdf_create::PdfCreateAgent;
use crate::agents::pdf_questions::PdfQuestionAgent;
use crate::agents::pdf_review::PdfReviewAgent;
use crate::agents::runtime::{run_agent, AgentError, AgentOutput, AgentRun, AgentTool};
use crate::agents::structured::StructuredAgent;
use crate::agents::user_spec::UserSpecAgent;
use crate::contracts::agent_drafts::AgentDraftWorkflowResponse;
use crate::contracts::common::{
    format_conversation_history, format_file_names, OrchestratorResponse, SupportedCapability,
};
use crate::contracts::orchestrator::OrchestratorRequest;
use crate::contracts::pdf_create::PdfCreateOrchestrateResponse;
use crate::contracts::pdf_edit::PdfEditResponse;
use crate::contracts::pdf_questions::PdfQuestionOrchestrateResponse;
use crate::contracts::pdf_review::PdfReviewOrchestrateResponse;
use crate::services::language::scoped;
use crate::state::AppState;

const ROUTER_SYSTEM_PROMPT: &str =
    "You are the top-level router. Choose exactly one capability that best handles the request:\n\
- pdf_edit: modify or convert one or more attached PDFs.\n\
- pdf_question: answer questions about the contents of the attached PDFs.\n\
- user_spec: create or define an agent spec.\n\
- pdf_review: return the PDF with review comments/annotations attached.\n\
- pdf_create: generate a NEW document from scratch (invoice, report, letter) - no input file.\n\
- unsupported: none of the above fit, or the user asks about the assistant itself; put a \
helpful message in 'message'.\n\
Respond with the capability and (only for unsupported) a message.\n\
\n\
Decide by what the user wants BACK. Information read out of the document is pdf_question; \
a changed file handed back is pdf_edit. Mentioning a document is not asking to change it.\n\
'  \"Is there a table of contents?\" -> pdf_question\n\
'  \"Add a table of contents\" -> pdf_edit\n\
'  \"Put a note on every paragraph that needs work\" -> pdf_review\n\
'  \"Build me a purchase order from scratch\" -> pdf_create\n\
'  \"Make a rule that stamps every upload\" -> user_spec\n\
'  \"Which version of the app is this?\" -> unsupported'";

const ORCHESTRATOR_SYSTEM_PROMPT: &str = "You are the top-level orchestrator. \
Choose exactly one output function that best handles the request. \
Use delegate_pdf_edit for any request to modify or convert one or more PDFs. \
Use delegate_pdf_question for questions about the contents of the attached PDFs. \
Use delegate_user_spec for requests to create or define an agent spec. \
Use delegate_pdf_review when the user wants the PDF returned with review\
 comments attached — anything like 'review this', 'annotate with comments',\
 'leave feedback on the PDF'. \
Use delegate_pdf_create when the user wants to generate a new document from\
 scratch with no input file — invoices, reports, letters, contracts, etc. \
Use unsupported_capability when the user asks about the assistant itself \
or when none of the other outputs fit; supply a helpful message.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum RouteCapability {
    PdfEdit,
    PdfQuestion,
    UserSpec,
    PdfReview,
    PdfCreate,
    Unsupported,
}

/// Local models add stray tool args and send null for optional fields;
/// unknown fields are ignored like Python's `extra="ignore"`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct RouteDecision {
    capability: RouteCapability,
    #[serde(default)]
    message: Option<String>,
}

pub struct OrchestratorAgent {
    state: AppState,
    /// Enum-path router — only used when `uses_tool_output(provider)`.
    router: Option<StructuredAgent>,
}

impl OrchestratorAgent {
    pub fn new(state: AppState) -> Self {
        let router = uses_tool_output(&state.runtime.load().settings.chat_provider).then(|| {
            let runtime = state.runtime.load();
            StructuredAgent::json::<RouteDecision>(
                runtime.fast_model.clone(),
                ROUTER_SYSTEM_PROMPT,
                &runtime.settings.chat_provider,
            )
        });
        Self { state, router }
    }

    pub async fn handle(
        &self,
        request: &OrchestratorRequest,
    ) -> Result<OrchestratorResponse, AgentError> {
        let locale = request.locale.clone();
        let request = request.clone();
        // Bound once; delegates and worker tasks inherit it.
        scoped(locale, async move {
            tracing::info!(
                files = ?request.files.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
                resume_with = ?request.resume_with,
                artifacts = request.artifacts.len(),
                message = %request.user_message,
                "[orchestrator] handle"
            );
            if let Some(capability) = request.resume_with {
                return resume(&self.state, &request, capability).await;
            }
            if self.router.is_some() {
                return route_and_dispatch(&self.state, &self.router, &request).await;
            }
            let fast_model = self.state.runtime.load().fast_model.clone();
            let output = run_agent(AgentRun {
                client: &fast_model,
                system: ORCHESTRATOR_SYSTEM_PROMPT,
                prompt: build_prompt(&request),
                tools: delegate_tools(&self.state, &request),
                output_schema: None,
                max_iterations: 0,
            })
            .await?;
            let AgentOutput::Json(value) = output else {
                return Err(AgentError::Output(
                    "orchestrator produced text instead of a delegate result".to_string(),
                ));
            };
            serde_json::from_value::<OrchestratorResponse>(value)
                .map_err(|e| AgentError::Output(format!("orchestrator output invalid: {e}")))
        })
        .await
    }
}

/// Local-model routing: pick a capability by name, then dispatch.
async fn route_and_dispatch(
    state: &AppState,
    router: &Option<StructuredAgent>,
    request: &OrchestratorRequest,
) -> Result<OrchestratorResponse, AgentError> {
    let router = router.as_ref().expect("enum router configured");
    let decision: RouteDecision = router.run(&build_prompt(request)).await?;
    tracing::info!("[orchestrator] enum-routed -> {:?}", decision.capability);
    match decision.capability {
        RouteCapability::PdfEdit => run_pdf_edit(state, request).await,
        RouteCapability::PdfQuestion => run_pdf_question(state, request).await,
        RouteCapability::UserSpec => run_agent_draft(state, request).await,
        RouteCapability::PdfReview => run_pdf_review(state, request).await,
        RouteCapability::PdfCreate => run_pdf_create(state, request).await,
        RouteCapability::Unsupported => Ok(OrchestratorResponse::UnsupportedCapability {
            capability: "orchestrate".to_string(),
            message: decision
                .message
                .unwrap_or_else(|| "I can't help with that request.".to_string()),
        }),
    }
}

/// Fast-path back to the correct delegate without an AI routing call —
/// also the multi-turn re-entry after Java runs an emitted plan.
async fn resume(
    state: &AppState,
    request: &OrchestratorRequest,
    capability: SupportedCapability,
) -> Result<OrchestratorResponse, AgentError> {
    match capability {
        SupportedCapability::PdfQuestion => run_pdf_question(state, request).await,
        SupportedCapability::PdfReview => run_pdf_review(state, request).await,
        SupportedCapability::PdfEdit => run_pdf_edit(state, request).await,
        SupportedCapability::AgentDraft => run_agent_draft(state, request).await,
        SupportedCapability::PdfCreate => run_pdf_create(state, request).await,
        SupportedCapability::Orchestrate
        | SupportedCapability::AgentRevise
        | SupportedCapability::AgentNextAction
        | SupportedCapability::MathAuditorAgent => Err(AgentError::Output(format!(
            "Cannot resume orchestrator with capability: {capability:?}"
        ))),
    }
}

/// The six `ToolOutput` delegates. Each is terminal: the model's choice IS
/// the response, serialized as the union's `outcome`-tagged JSON.
fn delegate_tools(state: &AppState, request: &OrchestratorRequest) -> Vec<AgentTool> {
    const EMPTY_PARAMS: &str = r#"{"type":"object","properties":{},"additionalProperties":false}"#;
    let mut tools = Vec::new();
    for (name, description, delegate) in [
        (
            "delegate_pdf_edit",
            "Delegate requests to modify or convert PDFs and return the PDF edit result.",
            Delegate::PdfEdit,
        ),
        (
            "delegate_pdf_question",
            "Delegate questions about PDF contents and return the PDF question result.",
            Delegate::PdfQuestion,
        ),
        (
            "delegate_user_spec",
            "Delegate requests to create or revise a user agent spec and return the draft result.",
            Delegate::UserSpec,
        ),
        (
            "delegate_pdf_review",
            "Delegate requests to review a PDF and leave review comments, notes, or\
             sticky-note annotations on the document itself. Use this when the user\
             wants the PDF returned with comments attached (e.g. 'review this',\
             'add review comments', 'flag unclear sentences', 'annotate with\
             feedback').",
            Delegate::PdfReview,
        ),
        (
            "delegate_pdf_create",
            "Delegate requests to create a new PDF document from scratch based on a\
             description. Use this when the user wants to generate a new document\
             (e.g. 'create an invoice', 'write a report', 'make a contract',\
             'draft a letter'). No input file is required.",
            Delegate::PdfCreate,
        ),
    ] {
        tools.push(delegate_tool(
            name,
            description,
            serde_json::from_str(EMPTY_PARAMS).unwrap(),
            state.clone(),
            request.clone(),
            delegate,
        ));
    }

    tools.push(
        AgentTool::new(
            "unsupported_capability",
            "Return this when none of the delegate outputs fit the request.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "capability": {"type": "string"},
                    "message": {"type": "string"}
                },
                "required": ["capability", "message"],
                "additionalProperties": false
            }),
            |args: Value, _ctx: &crate::agents::runtime::ToolContext| {
                Box::pin(async move {
                    let capability = args
                        .get("capability")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let message = args
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    serde_json::to_string(&OrchestratorResponse::UnsupportedCapability {
                        capability,
                        message,
                    })
                    .map_err(|e| e.to_string())
                })
            },
        )
        .terminal(),
    );
    tools
}

#[derive(Clone, Copy)]
enum Delegate {
    PdfEdit,
    PdfQuestion,
    UserSpec,
    PdfReview,
    PdfCreate,
}

/// A zero-arg terminal delegate: runs the chosen sub-agent against the
/// captured state and request, then serializes the union response as the
/// tool's JSON result.
fn delegate_tool(
    name: &'static str,
    description: &'static str,
    parameters: Value,
    state: AppState,
    request: OrchestratorRequest,
    delegate: Delegate,
) -> AgentTool {
    AgentTool::new(
        name,
        description,
        parameters,
        move |_args: Value, _ctx: &crate::agents::runtime::ToolContext| {
            let state = state.clone();
            let request = request.clone();
            Box::pin(async move {
                let response = match delegate {
                    Delegate::PdfEdit => run_pdf_edit(&state, &request).await,
                    Delegate::PdfQuestion => run_pdf_question(&state, &request).await,
                    Delegate::UserSpec => run_agent_draft(&state, &request).await,
                    Delegate::PdfReview => run_pdf_review(&state, &request).await,
                    Delegate::PdfCreate => run_pdf_create(&state, &request).await,
                }
                .map_err(|e| e.to_string())?;
                serde_json::to_string(&response)
                    .map_err(|e| format!("delegate {name} serialization failed: {e}"))
            })
        },
    )
    .terminal()
}

async fn run_pdf_edit(
    state: &AppState,
    request: &OrchestratorRequest,
) -> Result<OrchestratorResponse, AgentError> {
    let pdf_edit = state.runtime.load().pdf_edit.clone();
    let response = pdf_edit
        .orchestrate(request)
        .await
        .map_err(|e| AgentError::Output(format!("pdf-edit failed: {e}")))?;
    Ok(match response {
        PdfEditResponse::Plan(p) => OrchestratorResponse::Plan(p),
        PdfEditResponse::NeedClarification(p) => OrchestratorResponse::NeedClarification(p),
        PdfEditResponse::CannotDo(p) => OrchestratorResponse::CannotDo(p),
        PdfEditResponse::NeedContent(p) => OrchestratorResponse::NeedContent(p),
    })
}

async fn run_pdf_question(
    state: &AppState,
    request: &OrchestratorRequest,
) -> Result<OrchestratorResponse, AgentError> {
    let runtime = state.runtime.load();
    let agent = PdfQuestionAgent::new(
        runtime.settings.clone(),
        runtime.smart_model.clone(),
        runtime.fast_model.clone(),
        runtime.documents.clone(),
    )
    .map_err(|e| AgentError::Output(format!("pdf-question init failed: {e}")))?;
    let response = agent
        .orchestrate(request)
        .await
        .map_err(|e| AgentError::Output(format!("pdf-question failed: {e}")))?;
    Ok(match response {
        PdfQuestionOrchestrateResponse::Answer(p) => OrchestratorResponse::Answer(p),
        PdfQuestionOrchestrateResponse::NotFound(p) => OrchestratorResponse::NotFound(p),
        PdfQuestionOrchestrateResponse::NeedIngest(p) => OrchestratorResponse::NeedIngest(p),
        PdfQuestionOrchestrateResponse::Plan(p) => OrchestratorResponse::Plan(p),
        PdfQuestionOrchestrateResponse::CannotDo(p) => OrchestratorResponse::CannotDo(p),
    })
}

async fn run_agent_draft(
    state: &AppState,
    request: &OrchestratorRequest,
) -> Result<OrchestratorResponse, AgentError> {
    let runtime = state.runtime.load();
    let agent = UserSpecAgent::new(
        runtime.smart_model.clone(),
        &runtime.settings.chat_provider,
        runtime.pdf_edit.clone(),
    );
    let response = agent.orchestrate(request).await?;
    Ok(match response {
        AgentDraftWorkflowResponse::Draft(p) => OrchestratorResponse::Draft(p),
        AgentDraftWorkflowResponse::NeedClarification(p) => {
            OrchestratorResponse::NeedClarification(p)
        }
        AgentDraftWorkflowResponse::CannotDo(p) => OrchestratorResponse::CannotDo(p),
    })
}

async fn run_pdf_review(
    state: &AppState,
    request: &OrchestratorRequest,
) -> Result<OrchestratorResponse, AgentError> {
    let runtime = state.runtime.load();
    let agent = PdfReviewAgent::new(
        runtime.settings.clone(),
        runtime.fast_model.clone(),
        runtime.documents.clone(),
    )
    .map_err(|e| AgentError::Output(format!("pdf-review init failed: {e}")))?;
    let response = agent
        .orchestrate(request)
        .await
        .map_err(|e| AgentError::Output(format!("pdf-review failed: {e}")))?;
    Ok(match response {
        PdfReviewOrchestrateResponse::Plan(p) => OrchestratorResponse::Plan(p),
        PdfReviewOrchestrateResponse::NeedIngest(p) => OrchestratorResponse::NeedIngest(p),
    })
}

async fn run_pdf_create(
    state: &AppState,
    request: &OrchestratorRequest,
) -> Result<OrchestratorResponse, AgentError> {
    let runtime = state.runtime.load();
    let agent = PdfCreateAgent::new(runtime.smart_model.clone(), &runtime.settings.chat_provider);
    let response = agent.orchestrate(request).await?;
    Ok(match response {
        PdfCreateOrchestrateResponse::Plan(p) => OrchestratorResponse::Plan(p),
        PdfCreateOrchestrateResponse::CannotDo(p) => OrchestratorResponse::CannotDo(p),
    })
}

fn build_prompt(request: &OrchestratorRequest) -> String {
    let artifact_summary = describe_artifacts(request);
    let history = format_conversation_history(&request.conversation_history);
    format!(
        "Conversation history:\n{history}\n\
         User message: {}\n\
         Files: {}\n\
         Available artifacts:\n{artifact_summary}\n\
         {}",
        request.user_message,
        format_file_names(&request.files),
        crate::services::language::language_directive(),
    )
}

fn describe_artifacts(request: &OrchestratorRequest) -> String {
    use crate::contracts::orchestrator::WorkflowArtifact;
    if request.artifacts.is_empty() {
        return "- none".to_string();
    }
    let mut descriptions = Vec::new();
    for artifact in &request.artifacts {
        match artifact {
            WorkflowArtifact::ExtractedText { files } => {
                let total_pages: usize = files.iter().map(|f| f.pages.len()).sum();
                let names: Vec<&str> = files.iter().map(|f| f.file_name.as_str()).collect();
                descriptions.push(format!(
                    "- extracted_text: {total_pages} pages from {names:?}"
                ));
            }
            WorkflowArtifact::ToolReport { .. } => {
                descriptions.push("- tool_report".to_string());
            }
        }
    }
    descriptions.join("\n")
}
