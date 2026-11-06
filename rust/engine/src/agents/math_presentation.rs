//! Math-auditor presentation helpers. Port of
//! `stirling.agents.math_presentation`: decide whether a request needs the
//! math auditor, and pull a `Verdict` back out of resume-turn artifacts.
//!
//! Intent classification is a small LLM call rather than an English regex,
//! so a request like "vérifiez les totaux" routes to the math path the same
//! as "check the totals".

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::agents::structured::{AgentOutput, StructuredAgent};
use crate::contracts::ledger::Verdict;
use crate::contracts::orchestrator::{OrchestratorRequest, WorkflowArtifact};
use crate::llm::client::LlmClient;

/// Find a math-auditor `Verdict` in the request's artifacts, if any. Meta
/// agents call this on resume to detect whether the specialist already ran.
pub fn extract_math_verdict(request: &OrchestratorRequest) -> Option<&Verdict> {
    request.artifacts.iter().find_map(|a| match a {
        WorkflowArtifact::ToolReport { report, .. } => Some(report),
        _ => None,
    })
}

const MATH_INTENT_SYSTEM_PROMPT: &str = "Decide whether the user's prompt is asking for verification of \
numerical content — math correctness, audit, recalculation, totals, \
sums, percentages, balances, arithmetic, or financial figures. \
Set is_math=true if so, otherwise false. Decide from the meaning of \
the prompt, not specific keywords; the prompt may be in any language.";

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct MathIntentDecision {
    /// True if the prompt is about verifying numerical content (math, audit,
    /// calculations, totals, percentages, etc.).
    is_math: bool,
}

/// Tiny LLM classifier shared between the question and review agents.
/// One instance per consumer.
pub struct MathIntentClassifier {
    agent: StructuredAgent,
}

impl MathIntentClassifier {
    pub fn new(client: LlmClient, chat_provider: &str) -> Self {
        Self {
            // Local models emit valid structured output only intermittently;
            // the larger retry budget keeps this hot-path classifier reliable.
            agent: StructuredAgent::new(
                client,
                MATH_INTENT_SYSTEM_PROMPT,
                AgentOutput::Json {
                    schema: serde_json::to_value(schemars::schema_for!(MathIntentDecision))
                        .unwrap_or(serde_json::Value::Null),
                },
                chat_provider,
            ),
        }
    }

    /// True when the prompt asks for numerical verification; empty prompts
    /// short-circuit without a model call.
    pub async fn classify(&self, user_message: &str) -> Result<bool, crate::agents::runtime::AgentError> {
        if user_message.is_empty() {
            return Ok(false);
        }
        let decision: MathIntentDecision = self.agent.run(user_message).await?;
        Ok(decision.is_math)
    }
}
