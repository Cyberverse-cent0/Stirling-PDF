//! Single-shot structured agent — the `Agent[None, T]` shape used by the
//! Python mapper extractors and synthesiser calls: a system prompt plus one
//! user prompt in, a typed result out. Multi-turn agents with tools keep
//! using [`crate::agents::runtime::run_agent`].
//!
//! Output validation retries mirror pydantic-ai's `output_retries` budget:
//! a malformed or schema-invalid result is fed back for another attempt,
//! while transport/API errors fail fast.

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::agents::output_mode::structured_output_retries;
use crate::agents::runtime::AgentError;
use crate::llm::client::{LlmClient, LlmError};

/// What the model must produce. `Json` forces the `final_result` tool call
/// (the `ToolOutput` mechanism every supported provider accepts); `Text` is
/// a plain completion for `Agent[None, str]`-style callers.
#[derive(Clone)]
pub enum AgentOutput {
    Json { schema: Value },
    Text,
}

#[derive(Clone)]
pub struct StructuredAgent {
    client: LlmClient,
    system: String,
    output: AgentOutput,
    retries: usize,
}

impl StructuredAgent {
    /// `chat_provider` is the `STIRLING_CHAT_PROVIDER`/resolved provider name
    /// (`anthropic`, `openai`, `ollama`, `custom`) — it selects the retry
    /// budget, matching `output_mode.output_retries`.
    pub fn new(
        client: LlmClient,
        system: impl Into<String>,
        output: AgentOutput,
        chat_provider: &str,
    ) -> Self {
        Self {
            client,
            system: system.into(),
            output,
            retries: structured_output_retries(chat_provider),
        }
    }

    /// Convenience constructor for JSON-output agents given a serde schema.
    pub fn json<T: schemars::JsonSchema>(
        client: LlmClient,
        system: impl Into<String>,
        chat_provider: &str,
    ) -> Self {
        Self::new(
            client,
            system,
            AgentOutput::Json {
                schema: serde_json::to_value(schemars::schema_for!(T)).unwrap_or(Value::Null),
            },
            chat_provider,
        )
    }

    /// Convenience constructor for text-output agents.
    pub fn text(client: LlmClient, system: impl Into<String>, chat_provider: &str) -> Self {
        Self::new(client, system, AgentOutput::Text, chat_provider)
    }

    /// Run the agent and deserialize the structured result.
    pub async fn run<T>(&self, prompt: &str) -> Result<T, AgentError>
    where
        T: DeserializeOwned + Send,
    {
        let AgentOutput::Json { schema } = &self.output else {
            return Err(AgentError::Output(
                "run() requires a JSON-output agent; use run_text()".to_string(),
            ));
        };
        let mut last_err = AgentError::Output("no attempt made".to_string());
        for _ in 0..=self.retries {
            // Single-attempt inner call so schema-level validation failures
            // share the same retry budget as unparseable output.
            let value = match self
                .client
                .complete_json(&self.system, prompt, schema, 0)
                .await
            {
                Ok(v) => v,
                Err(e @ (LlmError::InvalidJson(_) | LlmError::EmptyOutput)) => {
                    last_err = AgentError::Llm(e);
                    continue;
                }
                Err(e) => return Err(AgentError::Llm(e)),
            };
            match serde_json::from_value::<T>(value) {
                Ok(t) => return Ok(t),
                Err(e) => {
                    last_err = AgentError::Output(format!("output failed validation: {e}"));
                }
            }
        }
        Err(last_err)
    }

    /// Run a text-output agent.
    pub async fn run_text(&self, prompt: &str) -> Result<String, AgentError> {
        match &self.output {
            AgentOutput::Text => self
                .client
                .complete_text(&self.system, prompt)
                .await
                .map_err(AgentError::Llm),
            AgentOutput::Json { .. } => Err(AgentError::Output(
                "run_text() requires a text-output agent".to_string(),
            )),
        }
    }
}
