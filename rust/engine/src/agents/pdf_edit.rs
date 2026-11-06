//! PDF edit planner. Port of `stirling.agents.pdf_edit`: a selection call
//! picks an ordered operation plan (or clarification/cannot-do/need-content),
//! tool-chain validation gets one repair retry, then a per-operation
//! parameter call fills each step.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::agents::page_text::{format_page_text, has_page_text};
use crate::agents::runtime::AgentError;
use crate::agents::structured::{AgentOutput, StructuredAgent};
use crate::config::SharedSettings;
use crate::contracts::common::{
    format_conversation_history, format_file_names, AiFile, NeedContentFileRequest,
    NeedContentResponse, PdfContentType, SupportedCapability, ToolOperationStep,
};
use crate::contracts::pdf_edit::{
    EditCannotDoPayload, EditClarificationPayload, EditPlanPayload, PdfEditRequest, PdfEditResponse,
};
use crate::error::{ApiError, ApiResult};
use crate::llm::client::LlmClient;
use crate::models::tool_models::{ToolEndpoint, ToolId};
use crate::models::tool_schemas::{operation_description, operation_parameters, tool_schema};
use crate::services::language_directive;
use crate::services::operation_shortlist::{retrieval_query, OperationShortlist};
use crate::services::tool_io_compat::{blocking, validate_tool_chain, ToolChainStep};

/// Agent-hidden endpoints: live for the manual UI, direct API and pipelines,
/// but the planner must never emit them. `AUTO_REDACT`/`REDACT` pre-date the
/// unified operation model; AI redaction goes through `REDACT_EXECUTE`.
const AGENT_HIDDEN: &[ToolEndpoint] = &[ToolEndpoint::AutoRedact, ToolEndpoint::Redact];

/// What the selection model may return. `need_content` is the LLM-facing
/// variant: it names files by display name and Python resolves the `AiFile`
/// objects, so the model cannot fabricate file ids.
#[derive(Debug, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum PlanSelection {
    Plan {
        rationale: String,
        #[serde(deserialize_with = "non_empty_endpoints")]
        operations: Vec<ToolEndpoint>,
        summary: String,
    },
    NeedClarification {
        question: String,
        reason: String,
    },
    CannotDo {
        reason: String,
    },
    NeedContent {
        reason: String,
        #[serde(default, rename = "fileNames", alias = "file_names")]
        file_names: Option<Vec<String>>,
        #[serde(default, rename = "maxPages", alias = "max_pages")]
        max_pages: Option<i64>,
        #[serde(default, rename = "maxCharacters", alias = "max_characters")]
        max_characters: Option<i64>,
    },
}

/// `Field(min_length=1)` on the Python selection model: an empty plan is a
/// validation failure, which earns an output retry.
fn non_empty_endpoints<'de, D>(deserializer: D) -> Result<Vec<ToolEndpoint>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let ops = Vec::<ToolEndpoint>::deserialize(deserializer)?;
    if ops.is_empty() {
        return Err(serde::de::Error::custom("operations must not be empty"));
    }
    Ok(ops)
}

impl PlanSelection {
    fn outcome_name(&self) -> &'static str {
        match self {
            Self::Plan { .. } => "plan",
            Self::NeedClarification { .. } => "need_clarification",
            Self::CannotDo { .. } => "cannot_do",
            Self::NeedContent { .. } => "need_content",
        }
    }
}

/// JSON Schema for the selection union, mirroring what pydantic-ai's
/// `NativeOutput` hands the provider (camelCase aliases, `const` outcome).
fn selection_schema(allow_need_content: bool) -> Value {
    let mut variants = vec![
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["outcome", "rationale", "operations", "summary"],
            "properties": {
                "outcome": {"const": "plan"},
                "rationale": {"type": "string"},
                "operations": {"type": "array", "minItems": 1, "items": {"type": "string"}},
                "summary": {"type": "string"}
            }
        }),
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["outcome", "question", "reason"],
            "properties": {
                "outcome": {"const": "need_clarification"},
                "question": {"type": "string"},
                "reason": {"type": "string"}
            }
        }),
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["outcome", "reason"],
            "properties": {
                "outcome": {"const": "cannot_do"},
                "reason": {"type": "string"}
            }
        }),
    ];
    if allow_need_content {
        variants.push(json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["outcome", "reason"],
            "properties": {
                "outcome": {"const": "need_content"},
                "reason": {"type": "string"},
                "fileNames": {"type": ["array", "null"], "items": {"type": "string"}},
                "maxPages": {"type": ["integer", "null"]},
                "maxCharacters": {"type": ["integer", "null"]}
            }
        }));
    }
    json!({"anyOf": variants})
}

#[derive(Clone)]
pub struct PdfEditAgent {
    smart: LlmClient,
    chat_provider: String,
    settings: SharedSettings,
    shortlist: Arc<OperationShortlist>,
}

impl PdfEditAgent {
    pub fn new(
        smart: LlmClient,
        chat_provider: String,
        settings: SharedSettings,
        shortlist: Arc<OperationShortlist>,
    ) -> Self {
        Self {
            smart,
            chat_provider,
            settings,
            shortlist,
        }
    }

    /// Entry point for the `/api/v1/pdf/edit` route. The orchestrator's
    /// `orchestrate` delegate lands with the orchestrator port.
    pub async fn handle(&self, request: &PdfEditRequest) -> ApiResult<PdfEditResponse> {
        let (supported, unavailable) = classify_operations(request);
        if supported.is_empty() {
            return Ok(PdfEditResponse::CannotDo(EditCannotDoPayload {
                reason: "No PDF edit operations are available on this server.".to_string(),
            }));
        }
        // One retry, told exactly which transition fails. Cheaper than
        // carrying the whole compatibility matrix in the prompt.
        let mut repair_note = String::new();
        let mut planned = None;
        for _attempt in 0..2 {
            let selection = self
                .select_plan(request, &supported, &unavailable, true, &repair_note)
                .await?;
            tracing::info!("[pdf-edit] selection -> {}", selection.outcome_name());
            match selection {
                PlanSelection::NeedClarification { question, reason } => {
                    return Ok(PdfEditResponse::NeedClarification(
                        EditClarificationPayload { question, reason },
                    ));
                }
                PlanSelection::CannotDo { reason } => {
                    return Ok(PdfEditResponse::CannotDo(EditCannotDoPayload { reason }));
                }
                PlanSelection::NeedContent {
                    reason,
                    file_names,
                    max_pages,
                    max_characters,
                } => {
                    return Ok(PdfEditResponse::NeedContent(self.need_content_response(
                        reason,
                        file_names,
                        max_pages,
                        max_characters,
                        request,
                    )));
                }
                PlanSelection::Plan {
                    rationale,
                    operations,
                    summary,
                } => {
                    let enabled: std::collections::HashSet<ToolEndpoint> =
                        supported.iter().copied().collect();
                    let unsupported: Vec<ToolEndpoint> = operations
                        .iter()
                        .copied()
                        .filter(|op| !enabled.contains(op))
                        .collect();
                    if !unsupported.is_empty() {
                        let names = unsupported
                            .iter()
                            .map(|op| op.member_name())
                            .collect::<Vec<_>>()
                            .join(", ");
                        return Ok(PdfEditResponse::CannotDo(EditCannotDoPayload {
                            reason: format!(
                                "The following operations are not available on this server \
                                 (either disabled by the administrator or not installed): {names}"
                            ),
                        }));
                    }
                    let problems = chain_problems(&operations);
                    if problems.is_empty() {
                        planned = Some((rationale, operations, summary));
                        break;
                    }
                    repair_note = problems;
                }
            }
        }
        let Some((rationale, operations, summary)) = planned else {
            return Ok(PdfEditResponse::CannotDo(EditCannotDoPayload {
                reason: format!(
                    "No workable order of the available operations achieves this: {repair_note}"
                ),
            }));
        };

        let mut steps: Vec<ToolOperationStep> = Vec::with_capacity(operations.len());
        for (operation_index, operation_id) in operations.iter().enumerate() {
            let parameters = self
                .select_parameters(request, &operations, operation_index, &steps)
                .await?;
            steps.push(ToolOperationStep {
                tool: ToolId::Endpoint(*operation_id),
                parameters,
            });
        }
        Ok(PdfEditResponse::Plan(EditPlanPayload {
            summary,
            rationale: Some(rationale),
            steps,
            resume_with: None,
        }))
    }

    async fn select_plan(
        &self,
        request: &PdfEditRequest,
        supported: &[ToolEndpoint],
        unavailable: &[ToolEndpoint],
        allow_need_content: bool,
        repair_note: &str,
    ) -> ApiResult<PlanSelection> {
        let can_request_content = allow_need_content && !has_page_text(&request.page_text);
        let candidates = self
            .shortlist
            .select(
                &retrieval_query(&request.user_message, &request.conversation_history),
                supported,
                self.settings.planner_shortlist_size,
            )
            .await;
        let system = selection_system_prompt(&candidates, unavailable, can_request_content);
        let prompt = selection_prompt(request, &candidates, repair_note);
        let agent = StructuredAgent::new(
            self.smart.clone(),
            system,
            AgentOutput::Json {
                schema: selection_schema(can_request_content),
            },
            &self.chat_provider,
        );
        agent
            .run::<PlanSelection>(&prompt)
            .await
            .map_err(|e| ApiError::internal(format!("pdf-edit selection failed: {e}")))
    }

    async fn select_parameters(
        &self,
        request: &PdfEditRequest,
        operation_plan: &[ToolEndpoint],
        operation_index: usize,
        generated_steps: &[ToolOperationStep],
    ) -> ApiResult<Value> {
        let operation_id = operation_plan[operation_index];
        let schema = tool_schema(operation_id).cloned().ok_or_else(|| {
            ApiError::internal(format!("no parameter schema for {operation_id:?}"))
        })?;
        let instructions = format!(
            "Generate only the parameters for the PDF operation `{}`. \
             Do not include fields from any other operation.",
            operation_id.member_name()
        );
        let system = format!(
            "Generate only the parameter object for the selected PDF operation. \
             Use reasonable defaults when the request does not specify optional details. \
             Only fill fields that belong to the selected operation's parameter model. \
             When extracted page text is provided, use it to compute precise parameters \
             (e.g. exact page ranges that match a described pattern).\n\n{instructions}"
        );
        let prompt = parameter_prompt(request, operation_plan, operation_index, generated_steps);
        let agent = StructuredAgent::new(
            self.smart.clone(),
            system,
            AgentOutput::Json { schema },
            &self.chat_provider,
        );
        agent.run::<Value>(&prompt).await.map_err(|e| match e {
            AgentError::Llm(inner) => {
                ApiError::internal(format!("pdf-edit parameters failed: {inner}"))
            }
            other => ApiError::internal(format!("pdf-edit parameters failed: {other}")),
        })
    }

    fn need_content_response(
        &self,
        reason: String,
        file_names: Option<Vec<String>>,
        max_pages: Option<i64>,
        max_characters: Option<i64>,
        request: &PdfEditRequest,
    ) -> NeedContentResponse {
        // File objects are resolved here by matching names against
        // request.files so the model can't fabricate file ids.
        let files = resolve_need_content_files(file_names, &request.files);
        NeedContentResponse {
            files: files
                .into_iter()
                .map(|file| NeedContentFileRequest {
                    file,
                    page_numbers: Vec::new(),
                    content_types: vec![PdfContentType::PageText],
                })
                .collect(),
            max_pages: max_pages.unwrap_or(self.settings.max_pages as i64),
            max_characters: max_characters.unwrap_or(self.settings.max_characters as i64),
            resume_with: SupportedCapability::PdfEdit,
            reason,
        }
    }
}

/// Names that match nothing fall back to all files rather than sending
/// nothing — same recovery as the Python branch.
fn resolve_need_content_files(file_names: Option<Vec<String>>, files: &[AiFile]) -> Vec<AiFile> {
    match file_names {
        Some(names) if !names.is_empty() => {
            let requested: std::collections::HashSet<&str> =
                names.iter().map(String::as_str).collect();
            let matched: Vec<AiFile> = files
                .iter()
                .filter(|f| requested.contains(f.name.as_str()))
                .cloned()
                .collect();
            if matched.is_empty() {
                files.to_vec()
            } else {
                matched
            }
        }
        _ => files.to_vec(),
    }
}

/// Split the universe of operations into (supported, unavailable). Hidden
/// endpoints are excluded from both regardless of enabled state.
fn classify_operations(request: &PdfEditRequest) -> (Vec<ToolEndpoint>, Vec<ToolEndpoint>) {
    let enabled: std::collections::HashSet<ToolEndpoint> =
        request.enabled_endpoints.iter().copied().collect();
    let supported: Vec<ToolEndpoint> = request
        .enabled_endpoints
        .iter()
        .copied()
        .filter(|op| !AGENT_HIDDEN.contains(op))
        .collect();
    let unavailable: Vec<ToolEndpoint> = ToolEndpoint::ALL
        .iter()
        .copied()
        .filter(|op| !enabled.contains(op) && !AGENT_HIDDEN.contains(op))
        .collect();
    (supported, unavailable)
}

/// Why this ordering cannot run, or empty if it can. Only errors count:
/// parameters are not chosen yet, so a conditional output merely warns.
fn chain_problems(operations: &[ToolEndpoint]) -> String {
    let steps: Vec<ToolChainStep> = operations
        .iter()
        .map(|op| ToolChainStep {
            operation: *op,
            parameters: Default::default(),
        })
        .collect();
    let diagnostics = validate_tool_chain(&steps, None, None);
    let errors = blocking(&diagnostics);
    if errors.is_empty() {
        return String::new();
    }
    errors
        .iter()
        .map(|d| {
            format!(
                "step {} ({}) {}",
                d.step_index + 1,
                operations[d.step_index].member_name(),
                d.message
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn operations_prompt(operations: &[ToolEndpoint]) -> String {
    operations
        .iter()
        .map(|op| format!("{} ({})", op.member_name(), op.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Each operation with its description and a flat list of param
/// descriptions — what the tool does and what knobs it has, not the schema.
fn supported_operations_prompt(operations: &[ToolEndpoint]) -> String {
    let mut lines = Vec::new();
    for op in operations {
        let mut head = format!("- {} ({})", op.member_name(), op.as_str());
        let description = operation_description(*op);
        if !description.is_empty() {
            head.push_str(&format!(": {description}"));
        }
        lines.push(head);
        for (name, description) in operation_parameters(*op) {
            if description.is_empty() {
                lines.push(format!("    {name}"));
            } else {
                lines.push(format!("    {name}: {description}"));
            }
        }
    }
    lines.join("\n")
}

fn selection_system_prompt(
    supported: &[ToolEndpoint],
    unavailable: &[ToolEndpoint],
    allow_need_content: bool,
) -> String {
    let unavailable_clause = if unavailable.is_empty() {
        String::new()
    } else {
        format!(
            " The following operations exist on this server but are NOT currently available \
             (disabled by the administrator or not installed in this build) and must NOT appear \
             in any plan: {}. \
             If the user asks for one of these, return cannot_do, name the operation, and explain \
             that it exists but isn't available on this server.",
            operations_prompt(unavailable)
        )
    };
    let mut prompt = format!(
        "Plan PDF edit requests. \
         Supported operations are: {}.{} \
         Each operation in the user-facing prompt is listed with its full set of parameters. \
         Treat that list as authoritative: an operation can ONLY do what its listed parameters \
         and description allow. \
         Return an ordered list of one or more supported operations for the plan. \
         Chain multiple operations together whenever the request needs effects that no single \
         supported operation provides on its own (for example, splitting then rotating then \
         merging, or extracting pages then re-inserting them). \
         Only return cannot_do when no sequence of the supported operations could achieve the request. \
         Do not produce operation parameters in this stage. \
         Return plan when a reasonable multi-step plan can be created. \
         Never return partial plans. \
         Return need_clarification only when the request is genuinely ambiguous in a way \
         that no reasonable interpretation could produce a correct plan — do not ask to \
         confirm details that are already clear from the user's message.",
        operations_prompt(supported),
        unavailable_clause
    );
    if allow_need_content {
        prompt.push_str(
            " Return need_content when planning a correct answer requires inspecting the actual PDF \
             page text (e.g. 'split after every page that says NEW PAGE', \
             'rotate pages that mention draft'). \
             Set fileNames to only the files that need to be read; omit it to read all files.",
        );
    }
    prompt
}

fn selection_prompt(
    request: &PdfEditRequest,
    supported: &[ToolEndpoint],
    repair_note: &str,
) -> String {
    let repair_line = if repair_note.is_empty() {
        String::new()
    } else {
        format!(
            "A previous attempt planned operations in an order that cannot run: {repair_note}\n\
             If running them in a different order still gives the user what they asked for, \
             return that plan. If it would not - reordering changes the result, or no order \
             works - return cannot_do and say plainly which step cannot accept the previous \
             step's output. This is the one case where cannot_do is right even though some \
             order of these operations would run.\n"
        )
    };
    format!(
        "Supported operations:\n{}\n\
         {repair_line}\
         Conversation history:\n{}\n\
         Files: {}\n\
         Extracted page text:\n{}\n\
         {}\n\
         User request: {}",
        supported_operations_prompt(supported),
        format_conversation_history(&request.conversation_history),
        format_file_names(&request.files),
        format_page_text(&request.page_text, "None"),
        language_directive(),
        request.user_message
    )
}

fn parameter_prompt(
    request: &PdfEditRequest,
    operation_plan: &[ToolEndpoint],
    operation_index: usize,
    generated_steps: &[ToolOperationStep],
) -> String {
    let operation_id = operation_plan[operation_index];
    let operation_list = operation_plan
        .iter()
        .map(|op| op.member_name())
        .collect::<Vec<_>>()
        .join(", ");
    let generated_steps_text = if generated_steps.is_empty() {
        "None".to_string()
    } else {
        generated_steps
            .iter()
            .enumerate()
            .map(|(i, step)| {
                format!(
                    "- Step {}: {}",
                    i + 1,
                    serde_json::to_string(step).unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "Conversation history:\n{}\n\
         User request: {}\n\
         Files: {}\n\
         Operation plan: {}\n\
         Selected operation index: {} of {}\n\
         Selected operation: {}\n\
         Already generated steps:\n{}\n\
         Extracted page text:\n{}\n\
         Return only the parameter object for the selected operation.",
        format_conversation_history(&request.conversation_history),
        request.user_message,
        format_file_names(&request.files),
        operation_list,
        operation_index + 1,
        operation_plan.len(),
        operation_id.member_name(),
        generated_steps_text,
        format_page_text(&request.page_text, "None")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(enabled: Vec<ToolEndpoint>) -> PdfEditRequest {
        PdfEditRequest {
            user_message: "merge these".to_string(),
            files: vec![],
            conversation_history: vec![],
            page_text: vec![],
            enabled_endpoints: enabled,
        }
    }

    #[test]
    fn hidden_endpoints_are_never_supported_or_unavailable() {
        let (supported, unavailable) = classify_operations(&request(vec![
            ToolEndpoint::MergePdfs,
            ToolEndpoint::AutoRedact,
            ToolEndpoint::Redact,
        ]));
        assert_eq!(supported, vec![ToolEndpoint::MergePdfs]);
        assert!(!unavailable.contains(&ToolEndpoint::AutoRedact));
        assert!(!unavailable.contains(&ToolEndpoint::Redact));
        assert_eq!(unavailable.len(), ToolEndpoint::ALL.len() - 3);
    }

    #[test]
    fn empty_enabled_set_yields_no_supported_operations() {
        let (supported, unavailable) = classify_operations(&request(vec![]));
        assert!(supported.is_empty());
        assert_eq!(unavailable.len(), ToolEndpoint::ALL.len() - 2);
    }

    #[test]
    fn chain_problems_accepts_a_clean_plan() {
        // pdf->img then img->pdf is a legal SISO->MISO? No: extractImages is
        // SIMO so the next step sees fan-out info, not an error.
        assert_eq!(
            chain_problems(&[ToolEndpoint::PdfToImg, ToolEndpoint::MergePdfs]),
            ""
        );
    }

    #[test]
    fn chain_problems_flags_a_report_into_a_file_step() {
        // accessibility-report produces JSON (a report); merge-pdfs needs PDF.
        let problems = chain_problems(&[
            ToolEndpoint::AccessibilityReport,
            ToolEndpoint::MergePdfs,
        ]);
        assert!(problems.contains("step 2"), "{problems}");
        assert!(problems.contains("ACCESSIBILITY_REPORT"), "{problems}");
    }

    #[test]
    fn member_name_matches_python_enum_names() {
        assert_eq!(ToolEndpoint::CbrToPdf.member_name(), "CBR_TO_PDF");
        assert_eq!(ToolEndpoint::MergePdfs.member_name(), "MERGE_PDFS");
        assert_eq!(ToolEndpoint::PdfToPdfa.member_name(), "PDF_TO_PDFA");
    }

    #[test]
    fn need_content_files_match_by_name_and_fall_back() {
        let files = vec![
            AiFile {
                id: "f1".into(),
                name: "a.pdf".into(),
            },
            AiFile {
                id: "f2".into(),
                name: "b.pdf".into(),
            },
        ];
        let matched = resolve_need_content_files(Some(vec!["b.pdf".into()]), &files);
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].id, "f2");

        let fallback = resolve_need_content_files(Some(vec!["nope.pdf".into()]), &files);
        assert_eq!(fallback.len(), 2);

        let all = resolve_need_content_files(None, &files);
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn supported_prompt_lists_description_and_params() {
        let text = supported_operations_prompt(&[ToolEndpoint::MergePdfs]);
        assert!(
            text.starts_with("- MERGE_PDFS (/api/v1/general/merge-pdfs)"),
            "{text}"
        );
        assert!(
            text.contains("    fileNames: ")
                || text.contains("    fileNames\n")
                || text.contains("    fileNames"),
            "{text}"
        );
    }

    #[test]
    fn selection_schema_gates_need_content() {
        let with = selection_schema(true);
        let without = selection_schema(false);
        let with_s = serde_json::to_string(&with).unwrap();
        let without_s = serde_json::to_string(&without).unwrap();
        assert!(with_s.contains("need_content"));
        assert!(!without_s.contains("need_content"));
    }
}
