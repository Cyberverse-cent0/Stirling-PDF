//! PDF review delegate. Port of `stirling.agents.pdf_review`: produces an
//! annotated PDF plan with review comments. Intent precedence —
//! contradiction > math > pdf-comment fallback:
//!
//! * **Contradiction** prompts → run the [`ContradictionDetector`] in this
//!   process, localise findings via a small LLM, emit a single plan with
//!   paired `ADD_COMMENTS` sticky notes. Single-turn; no resume.
//! * **Math** prompts → plan that consults the math-auditor specialist and
//!   re-enters on the resume turn with the `Verdict` to produce sticky-note
//!   specs.
//! * **Everything else** → delegate to the `pdf-comment-agent` tool.
//!
//! Sticky-note text is produced by a small LLM that reads the structured
//! verdict/report and writes comments in the SAME LANGUAGE as the prompt.
//! Bounding-box placement is deterministic; verbatim claims anchor by text
//! snippet, paraphrased claims fall back to margin geometry.

use std::collections::HashMap;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::agents::contradiction::detector::escape_for_tag;
use crate::agents::contradiction::prompts::review_localiser_prompt;
use crate::agents::contradiction::{ContradictionDetector, ContradictionIntentClassifier};
use crate::agents::math_presentation::{extract_math_verdict, MathIntentClassifier};
use crate::agents::structured::StructuredAgent;
use crate::config::AppSettings;
use crate::contracts::comments::CommentSpec;
use crate::contracts::common::{
    AiFile, NeedIngestResponse, PdfContentType, SupportedCapability, ToolOperationStep,
};
use crate::contracts::contradiction::{AnchorQuality, ContradictionReport};
use crate::contracts::ledger::{Discrepancy, Verdict};
use crate::contracts::orchestrator::OrchestratorRequest;
use crate::contracts::pdf_edit::EditPlanPayload;
use crate::contracts::pdf_review::PdfReviewOrchestrateResponse;
use crate::documents::service::DocumentService;
use crate::llm::client::LlmClient;
use crate::models::tool_models::AgentToolId;
use crate::models::{ToolEndpoint, ToolId};
use crate::services::language::language_directive;
use crate::services::tracking::require_current_user_id;

/// Fallback right-margin placement when a finding has no usable anchor —
/// A4/Letter portrait assumed.
const ICON_X: f64 = 520.0;
const ICON_Y_TOP: f64 = 770.0;
const ICON_Y_STRIDE: f64 = 28.0;
const ICON_SIZE: f64 = 20.0;

const DEFAULT_AUTHOR: &str = "Stirling Math Auditor";
const CONTRADICTION_AUTHOR: &str = "Stirling Contradiction Auditor";

const LOCALISER_SYSTEM_PROMPT: &str =
    "You are given a math-audit Verdict (structured JSON) and the user's \
original review request. Produce one sticky-note entry per Discrepancy \
the user would care about. Each entry carries the discrepancy's index \
in the input list, a short subject (a few words), and a body of one or \
two sentences. Reply in the SAME LANGUAGE as the user's request. Do \
not invent figures; only restate what the Verdict already says. \
When a Discrepancy carries `stated` or `expected` values, quote them \
verbatim in the comment body — do not paraphrase, abbreviate, or \
convert units.";

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LocalisedComment {
    /// 0-based index of the Discrepancy in `verdict.discrepancies`.
    discrepancy_index: i64,
    subject: String,
    text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LocalisedVerdict {
    #[serde(default)]
    comments: Vec<LocalisedComment>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum WhichClaim {
    #[serde(rename = "claim1")]
    Claim1,
    #[serde(rename = "claim2")]
    Claim2,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PairedLocalisedContradiction {
    contradiction_index: i64,
    /// Which claim of the pair this sticky note describes.
    which_claim: WhichClaim,
    subject: String,
    text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LocalisedContradictionReport {
    #[serde(default)]
    comments: Vec<PairedLocalisedContradiction>,
}

#[derive(Debug, thiserror::Error)]
pub enum ReviewError {
    #[error(transparent)]
    Agent(#[from] crate::agents::runtime::AgentError),
    #[error(transparent)]
    Detector(#[from] crate::agents::contradiction::detector::DetectorError),
    #[error(transparent)]
    Documents(#[from] anyhow::Error),
    #[error("{0}")]
    NoUserId(String),
}

pub struct PdfReviewAgent {
    documents: Arc<DocumentService>,
    localiser_agent: StructuredAgent,
    contradiction_localiser: StructuredAgent,
    math_intent_classifier: MathIntentClassifier,
    contradiction_intent_classifier: ContradictionIntentClassifier,
    contradiction_detector: ContradictionDetector,
}

impl PdfReviewAgent {
    pub fn new(
        settings: Arc<AppSettings>,
        fast_model: LlmClient,
        documents: Arc<DocumentService>,
    ) -> Result<Self, crate::agents::chunked_mapper::ChunkedMapperError> {
        let chat_provider = settings.chat_provider.clone();
        Ok(Self {
            localiser_agent: StructuredAgent::json::<LocalisedVerdict>(
                fast_model.clone(),
                LOCALISER_SYSTEM_PROMPT,
                &chat_provider,
            ),
            contradiction_localiser: StructuredAgent::json::<LocalisedContradictionReport>(
                fast_model.clone(),
                review_localiser_prompt(),
                &chat_provider,
            ),
            math_intent_classifier: MathIntentClassifier::new(fast_model.clone(), &chat_provider),
            contradiction_intent_classifier: ContradictionIntentClassifier::new(
                fast_model.clone(),
                &chat_provider,
            ),
            contradiction_detector: ContradictionDetector::new(
                settings.clone(),
                fast_model,
                &chat_provider,
                documents.clone(),
            )?,
            documents,
        })
    }

    pub async fn orchestrate(
        &self,
        request: &OrchestratorRequest,
    ) -> Result<PdfReviewOrchestrateResponse, ReviewError> {
        // Resume turn comes first: a math verdict attached to the request is
        // projected into sticky-note specs.
        if let Some(verdict) = extract_math_verdict(request) {
            let comments_json = self
                .build_localised_comments_payload(&request.user_message, verdict)
                .await?;
            return Ok(PdfReviewOrchestrateResponse::Plan(add_comments_plan(
                comments_json,
                None,
            )));
        }

        // Contradiction takes precedence over math (see module docs).
        if self
            .contradiction_intent_classifier
            .classify(&request.user_message)
            .await?
        {
            let missing = self.find_missing_files(&request.files).await?;
            if !missing.is_empty() {
                return Ok(PdfReviewOrchestrateResponse::NeedIngest(
                    NeedIngestResponse {
                        resume_with: SupportedCapability::PdfReview,
                        reason: "Some files have not been ingested yet.".to_string(),
                        files_to_ingest: missing,
                        content_types: vec![PdfContentType::PageText],
                    },
                ));
            }
            let user_id = require_current_user_id().map_err(ReviewError::NoUserId)?;
            let report = self
                .contradiction_detector
                .detect(&request.files, &[user_id], Some(&request.user_message))
                .await?;
            let comments_json = self
                .build_contradiction_comments_payload(&request.user_message, &report)
                .await?;
            return Ok(PdfReviewOrchestrateResponse::Plan(add_comments_plan(
                comments_json,
                None,
            )));
        }

        if self
            .math_intent_classifier
            .classify(&request.user_message)
            .await?
        {
            return Ok(PdfReviewOrchestrateResponse::Plan(EditPlanPayload {
                summary: String::new(),
                rationale: None,
                steps: vec![ToolOperationStep {
                    tool: ToolId::Agent(AgentToolId::MathAuditorAgent),
                    // MathAuditorAgentParams() — tolerance defaults to "0.01".
                    parameters: serde_json::json!({"tolerance": "0.01"}),
                }],
                resume_with: Some(SupportedCapability::PdfReview),
            }));
        }

        Ok(PdfReviewOrchestrateResponse::Plan(EditPlanPayload {
            summary: String::new(),
            rationale: None,
            steps: vec![ToolOperationStep {
                tool: ToolId::Agent(AgentToolId::PdfCommentAgent),
                parameters: serde_json::json!({"prompt": request.user_message}),
            }],
            resume_with: None,
        }))
    }

    async fn find_missing_files(&self, files: &[AiFile]) -> Result<Vec<AiFile>, ReviewError> {
        let user_id = require_current_user_id().map_err(ReviewError::NoUserId)?;
        let principals = vec![user_id];
        let mut missing = Vec::new();
        for file in files {
            if !self.documents.has_collection(&file.id, &principals).await? {
                missing.push(file.clone());
            }
        }
        Ok(missing)
    }

    /// Run the math localiser LLM, then fuse its output with deterministic
    /// placement geometry into the JSON the `add-comments` tool wants.
    async fn build_localised_comments_payload(
        &self,
        user_message: &str,
        verdict: &Verdict,
    ) -> Result<String, ReviewError> {
        let prompt = format!(
            "User review request:\n{user_message}\n\nMath audit Verdict (JSON):\n{}\n\n{}",
            serde_json::to_string(verdict).unwrap_or_default(),
            language_directive(),
        );
        let localised: LocalisedVerdict = self.localiser_agent.run(&prompt).await?;
        let specs = build_comment_specs(verdict, &localised.comments);
        Ok(serde_json::to_string(&specs).unwrap_or_default())
    }

    /// Build paired ADD_COMMENTS JSON from a contradiction report. Each
    /// contradiction produces two sticky notes (one per claim's page) that
    /// cross-reference each other; verbatim claims anchor by quote text,
    /// paraphrased claims use margin geometry.
    async fn build_contradiction_comments_payload(
        &self,
        user_message: &str,
        report: &ContradictionReport,
    ) -> Result<String, ReviewError> {
        let prompt = format!(
            "<user_message>{}</user_message>\n<verdict>{}</verdict>\n{}",
            escape_for_tag(user_message),
            escape_for_tag(&serde_json::to_string(report).unwrap_or_default()),
            language_directive(),
        );
        let localised: LocalisedContradictionReport =
            self.contradiction_localiser.run(&prompt).await?;
        let specs = build_paired_comment_specs(report, &localised.comments);
        Ok(serde_json::to_string(&specs).unwrap_or_default())
    }
}

fn add_comments_plan(
    comments_json: String,
    resume_with: Option<SupportedCapability>,
) -> EditPlanPayload {
    EditPlanPayload {
        summary: String::new(),
        rationale: None,
        steps: vec![ToolOperationStep {
            tool: ToolId::Endpoint(ToolEndpoint::AddComments),
            parameters: serde_json::json!({"comments": comments_json}),
        }],
        resume_with,
    }
}

/// Fuse LLM-localised text with deterministic position geometry.
/// Out-of-range ordinals are dropped (defence-in-depth bounds check).
fn build_comment_specs(verdict: &Verdict, localised: &[LocalisedComment]) -> Vec<CommentSpec> {
    let mut specs = Vec::new();
    let mut per_page_index: HashMap<i64, usize> = HashMap::new();
    for comment in localised {
        let Some(d) = verdict
            .discrepancies
            .get(comment.discrepancy_index.max(0) as usize)
        else {
            continue;
        };
        if comment.discrepancy_index < 0 {
            continue;
        }
        let stack_index = per_page_index.entry(d.page).or_insert(0);
        let y = ICON_Y_TOP - (*stack_index as f64) * ICON_Y_STRIDE;
        *stack_index += 1;
        specs.push(CommentSpec {
            page_index: d.page,
            x: ICON_X,
            y,
            width: ICON_SIZE,
            height: ICON_SIZE,
            text: comment.text.clone(),
            author: Some(DEFAULT_AUTHOR.to_string()),
            subject: Some(comment.subject.clone()),
            anchor_text: anchor_text_for(d),
        });
    }
    specs
}

/// Two specs per contradiction (one per claim). Contracts use 1-indexed
/// pages; `page_index` on the tool is 0-indexed.
fn build_paired_comment_specs(
    report: &ContradictionReport,
    localised: &[PairedLocalisedContradiction],
) -> Vec<CommentSpec> {
    let mut specs = Vec::new();
    let mut per_page_index: HashMap<i64, usize> = HashMap::new();
    for entry in localised {
        if entry.contradiction_index < 0 {
            continue;
        }
        let Some(contradiction) = report
            .contradictions
            .get(entry.contradiction_index as usize)
        else {
            continue;
        };
        let claim = match entry.which_claim {
            WhichClaim::Claim1 => &contradiction.claim1,
            WhichClaim::Claim2 => &contradiction.claim2,
        };
        let page_index = (claim.page - 1).max(0);
        let stack_index = per_page_index.entry(page_index).or_insert(0);
        let y = ICON_Y_TOP - (*stack_index as f64) * ICON_Y_STRIDE;
        *stack_index += 1;
        specs.push(CommentSpec {
            page_index,
            x: ICON_X,
            y,
            width: ICON_SIZE,
            height: ICON_SIZE,
            text: entry.text.clone(),
            author: Some(CONTRADICTION_AUTHOR.to_string()),
            subject: Some(entry.subject.clone()),
            anchor_text: (claim.anchor_quality == AnchorQuality::Verbatim)
                .then(|| claim.quote.clone()),
        });
    }
    specs
}

fn anchor_text_for(d: &Discrepancy) -> Option<String> {
    let stated = d.stated.trim();
    if !stated.is_empty() {
        return Some(stated.to_string());
    }
    let context = d.context.trim();
    (!context.is_empty()).then(|| context.to_string())
}
