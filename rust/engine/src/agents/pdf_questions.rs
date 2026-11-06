//! PDF question agent. Port of `stirling.agents.pdf_questions`: answers
//! questions about ingested PDFs via three retrieval tools —
//! `search_knowledge` (targeted), `read_full_document` (whole-doc), and
//! `find_contradictions` (consistency audit) — picked by the smart model in
//! a single call that also writes the answer.
//!
//! The orchestrator entry additionally routes math-verification prompts to
//! the math-auditor specialist: first turn emits a one-step plan with
//! `resume_with=PDF_QUESTION`; the resume turn digests the returned
//! `Verdict` into a localised prose answer.

use std::sync::Arc;

use serde_json::Value;

use crate::agents::contradiction::{ContradictionCapability, ContradictionDetector, DEFAULT_MAX_AUDITS};
use crate::agents::math_presentation::{extract_math_verdict, MathIntentClassifier};
use crate::agents::runtime::{run_agent, AgentError, AgentOutput, AgentRun, AgentTool};
use crate::agents::structured::StructuredAgent;
use crate::agents::whole_doc_reader::{WholeDocReaderCapability, DEFAULT_MAX_READS};
use crate::agents::chunked_reasoner::{ChunkedReasoner, ChunkedReasonerError};
use crate::agents::contradiction::detector::DetectorError;
use crate::agents::chunked_mapper::ChunkedMapperError;
use crate::config::AppSettings;
use crate::contracts::common::{
    format_conversation_history, format_file_names, AiFile, NeedIngestResponse, PdfContentType,
    SupportedCapability, ToolOperationStep,
};
use crate::contracts::orchestrator::OrchestratorRequest;
use crate::contracts::pdf_edit::EditPlanPayload;
use crate::contracts::pdf_questions::{
    PdfQuestionAnswerPayload, PdfQuestionOrchestrateResponse, PdfQuestionRequest,
    PdfQuestionResponse, PdfQuestionTerminalResponse,
};
use crate::contracts::ledger::Verdict;
use crate::documents::rag_capability::RagCapability;
use crate::documents::service::DocumentService;
use crate::llm::client::LlmClient;
use crate::models::tool_models::AgentToolId;
use crate::models::ToolId;
use crate::services::language::language_directive;
use crate::services::tracking::require_current_user_id;

const PDF_QUESTION_SYSTEM_PROMPT: &str = "You answer questions about PDF documents using three retrieval tools:\n\
\n\
1. search_knowledge(query) - returns the passages most semantically similar \
to the query. Use it for targeted lookups: a specific fact, a named section, \
a particular passage. Typically one or two calls is enough.\n\
\n\
2. read_full_document(query) - reads every page of the attached documents in \
parallel and returns notes relevant to the query. Use it when answering \
requires seeing the whole document end-to-end: summaries, aggregations \
(largest, shortest, count), comparisons across sections. It is more \
expensive than search_knowledge, so prefer search_knowledge when one or two \
passages would suffice.\n\
\n\
3. find_contradictions(query) - audits the attached documents for textual \
contradictions across pages (opposing claims, conflicting recommendations, \
inconsistent deadlines, etc.) and returns a notes-style report. Use it when \
the question is about logical or textual consistency of the content (NOT \
numerical math). One call audits the entire document set.\n\
\n\
Pick the right tool, call it, then answer from what you got back. Do not \
guess or use outside knowledge.\n\
\n\
Guidelines:\n\
- If the retrieved content does not support a confident answer, return not_found.\n\
- Include a short list of evidence snippets (with page numbers where available) \
drawn from what the tools returned.\n\
\n\
Writing the not_found reason:\n\
- The reason is shown directly to the end user, so write it in plain, friendly \
language. One or two short sentences.\n\
- NEVER mention 'RAG', 'retrieval', 'chunks', 'search results', 'targeted search', \
'search_knowledge', 'read_full_document', 'find_contradictions', or other \
implementation details.\n\
- For questions where the answer just isn't in the document, say so directly: \
'I couldn't find that information in the document.'\n\
- Do not make it sound like you're choosing not to answer.";

const MATH_SYNTH_SYSTEM_PROMPT: &str = "You are given a math-audit Verdict (structured JSON) and the user's \
original question. Answer the question in plain prose using only \
facts from the Verdict; do not invent figures or pages. \
Reply in the SAME LANGUAGE as the user's question. Keep the answer \
concise — a sentence or short paragraph. \
Quote any stated/expected numeric values from the Verdict verbatim — \
do not paraphrase, abbreviate, or convert units.";

#[derive(Debug, thiserror::Error)]
pub enum QuestionError {
    #[error(transparent)]
    Agent(#[from] AgentError),
    #[error(transparent)]
    Reasoner(#[from] ChunkedReasonerError),
    #[error(transparent)]
    Mapper(#[from] ChunkedMapperError),
    #[error(transparent)]
    Detector(#[from] DetectorError),
    #[error(transparent)]
    Documents(#[from] anyhow::Error),
    #[error("{0}")]
    NoUserId(String),
}

pub struct PdfQuestionAgent {
    smart_model: LlmClient,
    settings: Arc<AppSettings>,
    documents: Arc<DocumentService>,
    math_synth_agent: StructuredAgent,
    math_intent_classifier: MathIntentClassifier,
    /// Shared across whole-doc-reader instances so the worker agent and
    /// semaphore are constructed once and reused per request.
    chunked_reasoner: Arc<ChunkedReasoner>,
    /// Shared from the question agent's long-lived instance.
    contradiction_detector: Arc<ContradictionDetector>,
}

impl PdfQuestionAgent {
    pub fn new(
        settings: Arc<AppSettings>,
        smart_model: LlmClient,
        fast_model: LlmClient,
        documents: Arc<DocumentService>,
    ) -> Result<Self, QuestionError> {
        let chat_provider = settings.chat_provider.clone();
        let chunked_reasoner = Arc::new(ChunkedReasoner::new(
            &settings,
            fast_model.clone(),
            smart_model.clone(),
            &chat_provider,
            None,
            None,
            None,
            None,
        )?);
        let contradiction_detector = Arc::new(ContradictionDetector::new(
            settings.clone(),
            fast_model.clone(),
            &chat_provider,
            documents.clone(),
        )?);
        Ok(Self {
            smart_model,
            settings,
            documents,
            math_synth_agent: StructuredAgent::text(
                fast_model.clone(),
                MATH_SYNTH_SYSTEM_PROMPT,
                &chat_provider,
            ),
            math_intent_classifier: MathIntentClassifier::new(fast_model, &chat_provider),
            chunked_reasoner,
            contradiction_detector,
        })
    }

    pub async fn handle(
        &self,
        request: &PdfQuestionRequest,
    ) -> Result<PdfQuestionResponse, QuestionError> {
        tracing::info!(
            files = ?request.files.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
            question = %request.question,
            "[pdf-question] handle"
        );
        let missing = self.find_missing_files(&request.files).await?;
        if !missing.is_empty() {
            tracing::info!(
                missing = ?missing.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
                "[pdf-question] missing ingestions"
            );
            return Ok(PdfQuestionResponse::NeedIngest(NeedIngestResponse {
                resume_with: SupportedCapability::PdfQuestion,
                reason: "Some files have not been ingested yet.".to_string(),
                files_to_ingest: missing,
                content_types: vec![PdfContentType::PageText],
            }));
        }
        let terminal = self.run_answer_agent(request).await?;
        Ok(match terminal {
            PdfQuestionTerminalResponse::Answer(p) => PdfQuestionResponse::Answer(p),
            PdfQuestionTerminalResponse::NotFound(p) => PdfQuestionResponse::NotFound(p),
        })
    }

    /// Orchestrator entry: routes math-verification prompts to the
    /// math-auditor specialist. Resume turns digest the captured `Verdict`
    /// into a localised prose answer; first turns emit a one-step plan with
    /// `resume_with=PDF_QUESTION`.
    pub async fn orchestrate(
        &self,
        request: &OrchestratorRequest,
    ) -> Result<PdfQuestionOrchestrateResponse, QuestionError> {
        if let Some(verdict) = extract_math_verdict(request) {
            let answer = self
                .synthesise_math_answer(&request.user_message, verdict)
                .await?;
            return Ok(PdfQuestionOrchestrateResponse::Answer(
                PdfQuestionAnswerPayload {
                    answer,
                    evidence: Vec::new(),
                },
            ));
        }

        if self
            .math_intent_classifier
            .classify(&request.user_message)
            .await?
        {
            return Ok(PdfQuestionOrchestrateResponse::Plan(EditPlanPayload {
                summary: String::new(),
                rationale: None,
                steps: vec![ToolOperationStep {
                    tool: ToolId::Agent(AgentToolId::MathAuditorAgent),
                    // MathAuditorAgentParams: tolerance defaults to "0.01".
                    parameters: serde_json::json!({"tolerance": "0.01"}),
                }],
                resume_with: Some(SupportedCapability::PdfQuestion),
            }));
        }

        let response = self
            .handle(&PdfQuestionRequest {
                question: request.user_message.clone(),
                files: request.files.clone(),
                conversation_history: request.conversation_history.clone(),
            })
            .await?;
        Ok(match response {
            PdfQuestionResponse::Answer(p) => PdfQuestionOrchestrateResponse::Answer(p),
            PdfQuestionResponse::NotFound(p) => PdfQuestionOrchestrateResponse::NotFound(p),
            PdfQuestionResponse::NeedIngest(p) => PdfQuestionOrchestrateResponse::NeedIngest(p),
        })
    }

    async fn find_missing_files(&self, files: &[AiFile]) -> Result<Vec<AiFile>, QuestionError> {
        let user_id = require_current_user_id().map_err(QuestionError::NoUserId)?;
        let principals = vec![user_id];
        let mut missing = Vec::new();
        for file in files {
            if !self
                .documents
                .has_collection(&file.id, &principals)
                .await?
            {
                missing.push(file.clone());
            }
        }
        Ok(missing)
    }

    /// Drive a single smart-model agent with all three retrieval tools. The
    /// agent picks `search_knowledge` for targeted lookups,
    /// `read_full_document` for whole-document questions, and
    /// `find_contradictions` for consistency checks — removing the upstream
    /// classifier keeps that judgement in the same call that writes the
    /// answer.
    async fn run_answer_agent(
        &self,
        request: &PdfQuestionRequest,
    ) -> Result<PdfQuestionTerminalResponse, QuestionError> {
        let user_id = require_current_user_id().map_err(QuestionError::NoUserId)?;
        let principals = vec![user_id];
        let collections: Vec<String> = request.files.iter().map(|f| f.id.clone()).collect();

        let rag = RagCapability::new(
            self.documents.clone(),
            principals.clone(),
            Some(collections),
            self.settings.rag_default_top_k,
            self.settings.rag_max_searches,
        );
        let whole_doc = WholeDocReaderCapability::new(
            self.documents.clone(),
            request.files.clone(),
            principals.clone(),
            self.chunked_reasoner.clone(),
            DEFAULT_MAX_READS,
        );
        let contradiction = ContradictionCapability::new(
            self.contradiction_detector.clone(),
            request.files.clone(),
            principals,
            DEFAULT_MAX_AUDITS,
        );

        // pydantic-ai `instructions` are system-level additions resolved at
        // run time; concatenating them onto the system prompt preserves the
        // content the model sees.
        let mut system = PDF_QUESTION_SYSTEM_PROMPT.to_string();
        for instruction in [
            rag.instructions().await,
            whole_doc.instructions(),
            contradiction.instructions(),
        ] {
            system.push_str("\n\n");
            system.push_str(&instruction);
        }

        let mut tools: Vec<AgentTool> = Vec::new();
        if let Some(tool) = rag.tool() {
            tools.push(tool);
        }
        tools.push(whole_doc.tool());
        tools.push(contradiction.tool());

        let output = run_agent(AgentRun {
            client: &self.smart_model,
            system: &system,
            prompt: build_prompt(request),
            tools,
            output_schema: Some(
                serde_json::to_value(schemars::schema_for!(PdfQuestionTerminalResponse))
                    .unwrap_or(Value::Null),
            ),
            max_iterations: 0,
        })
        .await?;
        let AgentOutput::Json(value) = output else {
            return Err(AgentError::Output(
                "answer agent produced text instead of structured output".to_string(),
            )
            .into());
        };
        serde_json::from_value::<PdfQuestionTerminalResponse>(value)
            .map_err(|e| AgentError::Output(format!("answer output invalid: {e}")).into())
    }

    /// Render the structured `Verdict` as a natural-language answer in the
    /// user's language; the system prompt forbids invented figures.
    async fn synthesise_math_answer(
        &self,
        user_message: &str,
        verdict: &Verdict,
    ) -> Result<String, QuestionError> {
        let prompt = format!(
            "User question:\n{user_message}\n\nMath audit Verdict (JSON):\n{}\n\n{}",
            serde_json::to_string_pretty(verdict).unwrap_or_default(),
            language_directive(),
        );
        Ok(self.math_synth_agent.run_text(&prompt).await?)
    }
}

fn build_prompt(request: &PdfQuestionRequest) -> String {
    let history = format_conversation_history(&request.conversation_history);
    format!(
        "Conversation history:\n{history}\n\
         Files: {}\n\
         Question: {}\n\
         Pick the right retrieval tool for this question, then answer from what it returns.\n\
         {}",
        format_file_names(&request.files),
        request.question,
        language_directive(),
    )
}
