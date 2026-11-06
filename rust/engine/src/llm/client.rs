//! Chat-completions client for both provider wire formats.
//!
//! Structured output uses a forced single-tool call (`final_result`) on both
//! Anthropic and OpenAI-compatible endpoints — the same mechanism pydantic-ai
//! uses for `ToolOutput`, and accepted by OpenAI/Ollama/Anthropic alike. The
//! model's JSON arguments are the structured result.

use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::Semaphore;

use super::model::{ModelSpec, ProviderKind};

const STRUCTURED_TOOL_NAME: &str = "final_result";
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Anthropic requires `max_tokens`; used when settings leave it unset.
const DEFAULT_MAX_TOKENS: u32 = 8192;

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("model request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("model API error {status}: {body}")]
    Api { status: u16, body: String },
    #[error("model returned no structured output")]
    EmptyOutput,
    #[error("model output was not valid JSON: {0}")]
    InvalidJson(String),
}

#[derive(Clone)]
pub struct LlmClient {
    http: reqwest::Client,
    spec: ModelSpec,
    semaphore: Arc<Semaphore>,
    max_tokens: Option<u32>,
}

impl LlmClient {
    pub fn new(
        http: reqwest::Client,
        spec: ModelSpec,
        semaphore: Arc<Semaphore>,
        max_tokens: Option<u32>,
    ) -> Self {
        Self {
            http,
            spec,
            semaphore,
            max_tokens,
        }
    }

    pub fn model_name(&self) -> &str {
        &self.spec.model_name
    }

    pub fn provider_kind(&self) -> ProviderKind {
        self.spec.kind
    }

    pub fn max_tokens(&self) -> Option<u32> {
        self.max_tokens
    }

    /// POST a fully-formed chat request body to this client's provider,
    /// dispatching on the configured kind. Used by the agent runtime's
    /// multi-turn loop; single-shot callers use `complete_text`/`complete_json`.
    pub async fn post_chat(&self, body: &Value) -> Result<Value, LlmError> {
        let _permit = self.semaphore.acquire().await.expect("semaphore closed");
        match self.spec.kind {
            ProviderKind::Anthropic => self.post_anthropic(body).await,
            ProviderKind::OpenAiCompat => self.post_openai(body).await,
        }
    }

    /// Plain-text completion (system + single user prompt).
    pub async fn complete_text(&self, system: &str, prompt: &str) -> Result<String, LlmError> {
        let _permit = self.semaphore.acquire().await.expect("semaphore closed");
        match self.spec.kind {
            ProviderKind::Anthropic => self.anthropic_text(system, prompt).await,
            ProviderKind::OpenAiCompat => self.openai_text(system, prompt).await,
        }
    }

    /// Structured completion: returns the parsed JSON the model produced for
    /// `schema`. Retries `retries` extra times on unparseable output, matching
    /// pydantic-ai's output-validation retry budget (transport errors fail fast).
    pub async fn complete_json(
        &self,
        system: &str,
        prompt: &str,
        schema: &Value,
        retries: usize,
    ) -> Result<Value, LlmError> {
        for attempt in 0..=retries {
            match self.complete_json_once(system, prompt, schema).await {
                Ok(value) => return Ok(value),
                Err(err @ (LlmError::InvalidJson(_) | LlmError::EmptyOutput)) => {
                    if attempt == retries {
                        return Err(err);
                    }
                    tracing::debug!(attempt, error = %err, "retrying structured output");
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }

    async fn complete_json_once(
        &self,
        system: &str,
        prompt: &str,
        schema: &Value,
    ) -> Result<Value, LlmError> {
        let _permit = self.semaphore.acquire().await.expect("semaphore closed");
        match self.spec.kind {
            ProviderKind::Anthropic => self.anthropic_json(system, prompt, schema).await,
            ProviderKind::OpenAiCompat => self.openai_json(system, prompt, schema).await,
        }
    }

    // ---- Anthropic Messages API ----

    fn anthropic_url(&self) -> String {
        format!("{}/v1/messages", self.spec.base_url.trim_end_matches('/'))
    }

    async fn anthropic_text(&self, system: &str, prompt: &str) -> Result<String, LlmError> {
        let body = self.anthropic_body(system, prompt, None);
        let resp = self.post_anthropic(&body).await?;
        extract_anthropic_text(&resp)
    }

    async fn anthropic_json(
        &self,
        system: &str,
        prompt: &str,
        schema: &Value,
    ) -> Result<Value, LlmError> {
        let body = self.anthropic_body(system, prompt, Some(schema));
        let resp = self.post_anthropic(&body).await?;
        extract_anthropic_tool_input(&resp)
    }

    fn anthropic_body(&self, system: &str, prompt: &str, schema: Option<&Value>) -> Value {
        let mut body = json!({
            "model": self.spec.model_name,
            "max_tokens": self.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            "system": system,
            "messages": [{"role": "user", "content": prompt}],
        });
        if let Some(schema) = schema {
            body["tools"] = json!([{
                "name": STRUCTURED_TOOL_NAME,
                "description": "Return the structured result.",
                "input_schema": schema,
            }]);
            body["tool_choice"] = json!({"type": "tool", "name": STRUCTURED_TOOL_NAME});
        }
        body
    }

    async fn post_anthropic(&self, body: &Value) -> Result<Value, LlmError> {
        let resp = self
            .http
            .post(self.anthropic_url())
            .header("x-api-key", &self.spec.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(body)
            .send()
            .await?;
        parse_response(resp).await
    }

    // ---- OpenAI-compatible chat completions ----

    fn openai_url(&self) -> String {
        format!(
            "{}/chat/completions",
            self.spec.base_url.trim_end_matches('/')
        )
    }

    fn openai_body(&self, system: &str, prompt: &str, schema: Option<&Value>) -> Value {
        let mut body = json!({
            "model": self.spec.model_name,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": prompt},
            ],
        });
        if let Some(max) = self.max_tokens {
            body["max_tokens"] = json!(max);
        }
        if let Some(schema) = schema {
            body["tools"] = json!([{
                "type": "function",
                "function": {
                    "name": STRUCTURED_TOOL_NAME,
                    "description": "Return the structured result.",
                    "parameters": schema,
                },
            }]);
            body["tool_choice"] =
                json!({"type": "function", "function": {"name": STRUCTURED_TOOL_NAME}});
        }
        body
    }

    async fn post_openai(&self, body: &Value) -> Result<Value, LlmError> {
        let resp = self
            .http
            .post(self.openai_url())
            .header("Authorization", format!("Bearer {}", self.spec.api_key))
            .json(body)
            .send()
            .await?;
        parse_response(resp).await
    }

    async fn openai_text(&self, system: &str, prompt: &str) -> Result<String, LlmError> {
        let body = self.openai_body(system, prompt, None);
        let resp = self.post_openai(&body).await?;
        extract_openai_text(&resp)
    }

    async fn openai_json(
        &self,
        system: &str,
        prompt: &str,
        schema: &Value,
    ) -> Result<Value, LlmError> {
        let body = self.openai_body(system, prompt, Some(schema));
        let resp = self.post_openai(&body).await?;
        extract_openai_tool_arguments(&resp)
    }
}

async fn parse_response(resp: reqwest::Response) -> Result<Value, LlmError> {
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(LlmError::Api {
            status: status.as_u16(),
            body: text.chars().take(500).collect(),
        });
    }
    serde_json::from_str(&text)
        .map_err(|e| LlmError::InvalidJson(format!("response envelope: {e}")))
}

fn extract_anthropic_text(resp: &Value) -> Result<String, LlmError> {
    let blocks = resp
        .get("content")
        .and_then(Value::as_array)
        .ok_or(LlmError::EmptyOutput)?;
    let text: String = blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect();
    if text.is_empty() {
        Err(LlmError::EmptyOutput)
    } else {
        Ok(text)
    }
}

fn extract_anthropic_tool_input(resp: &Value) -> Result<Value, LlmError> {
    let blocks = resp
        .get("content")
        .and_then(Value::as_array)
        .ok_or(LlmError::EmptyOutput)?;
    for block in blocks {
        if block.get("type").and_then(Value::as_str) == Some("tool_use") {
            if let Some(input) = block.get("input") {
                return Ok(input.clone());
            }
        }
    }
    Err(LlmError::EmptyOutput)
}

fn extract_openai_text(resp: &Value) -> Result<String, LlmError> {
    resp.pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or(LlmError::EmptyOutput)
}

fn extract_openai_tool_arguments(resp: &Value) -> Result<Value, LlmError> {
    let message = resp
        .pointer("/choices/0/message")
        .ok_or(LlmError::EmptyOutput)?;
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            if let Some(args) = call.pointer("/function/arguments").and_then(Value::as_str) {
                return serde_json::from_str(args)
                    .map_err(|e| LlmError::InvalidJson(format!("tool arguments: {e}")));
            }
        }
    }
    // Some compatible endpoints ignore tool_choice and answer in content.
    if let Some(content) = message.get("content").and_then(Value::as_str) {
        if !content.trim().is_empty() {
            if let Ok(value) = serde_json::from_str(content) {
                return Ok(value);
            }
            return Err(LlmError::InvalidJson(format!("message content: {content}")));
        }
    }
    Err(LlmError::EmptyOutput)
}
