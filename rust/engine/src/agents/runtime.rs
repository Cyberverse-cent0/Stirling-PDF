//! Minimal agent runtime: multi-turn tool-calling loop over the provider
//! wire formats, replacing the pydantic-ai `Agent.run` core for agents that
//! need tools. Single-shot structured agents keep using
//! [`LlmClient::complete_json`].
//!
//! Output modes mirror `agents.output_mode`: when an output schema is given
//! it is delivered as a `final_result` tool the model calls when done —
//! the `ToolOutput` mechanism — which every supported provider accepts.
//! Providers that natively support json-schema output may use it later via
//! [`LlmClient::complete_json`]; the loop is provider-agnostic.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::llm::client::{LlmClient, LlmError};
use crate::llm::model::ProviderKind;

/// Safety bound on tool-call round trips; Python relies on per-tool budgets
/// (e.g. `max_searches`) plus provider timeouts. Kept generous so legitimate
/// multi-tool answers are never cut off.
const MAX_ITERATIONS: usize = 15;

pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;
pub type ToolHandler = Arc<dyn for<'a> Fn(Value, &'a ToolContext) -> ToolFuture<'a> + Send + Sync>;

/// Per-run context handed to tool handlers (deps, principal, request scope).
#[derive(Clone, Default)]
pub struct ToolContext {
    pub data: Value,
}

#[derive(Clone)]
pub struct AgentTool {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool arguments.
    pub parameters: Value,
    pub handler: ToolHandler,
    /// Dynamic visibility — the Python `prepare` hook that returns `None` to
    /// hide a tool (e.g. once a per-run budget is spent). Evaluated before
    /// every model round trip; hidden tools are neither advertised nor
    /// dispatchable.
    pub visible: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

impl AgentTool {
    pub fn new<F, Fut>(name: &str, description: &str, parameters: Value, handler: F) -> Self
    where
        F: for<'a> Fn(Value, &'a ToolContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<String, String>> + Send + 'static,
    {
        Self {
            name: name.to_string(),
            description: description.to_string(),
            parameters,
            handler: Arc::new(move |args, ctx| Box::pin(handler(args.clone(), ctx))),
            visible: None,
        }
    }

    /// Hide the tool from the model once `f` returns false (budget spent).
    pub fn visible(mut self, f: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        self.visible = Some(Arc::new(f));
        self
    }
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug)]
enum Message {
    User(String),
    Assistant {
        text: Option<String>,
        tool_calls: Vec<ToolCall>,
    },
    ToolResults(Vec<(String, String)>), // (tool_call_id, content)
}

#[derive(Debug)]
pub enum AgentOutput {
    Json(Value),
    Text(String),
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Llm(#[from] LlmError),
    #[error("agent exceeded {0} tool iterations without a final answer")]
    IterationLimit(usize),
    #[error("tool {0} returned an error: {1}")]
    Tool(String, String),
    #[error("agent output failed validation: {0}")]
    Output(String),
}

pub struct AgentRun<'a> {
    pub client: &'a LlmClient,
    pub system: &'a str,
    pub prompt: String,
    pub tools: Vec<AgentTool>,
    /// When set, the model must call `final_result` with this JSON Schema to
    /// finish; its arguments become [`AgentOutput::Json`].
    pub output_schema: Option<Value>,
    pub max_iterations: usize,
}

/// Run the tool loop until the model produces a final answer.
pub async fn run_agent(run: AgentRun<'_>) -> Result<AgentOutput, AgentError> {
    let mut messages = vec![Message::User(run.prompt)];
    let limit = if run.max_iterations == 0 {
        MAX_ITERATIONS
    } else {
        run.max_iterations
    };

    for _ in 0..limit {
        // Re-evaluate visibility each round so budget-bound tools disappear
        // mid-run, matching pydantic-ai's per-call `prepare` hooks.
        let tools: Vec<AgentTool> = run
            .tools
            .iter()
            .filter(|t| t.visible.as_ref().is_none_or(|f| f()))
            .cloned()
            .collect();
        let response = chat_once(
            run.client,
            run.system,
            &messages,
            &tools,
            run.output_schema.as_ref(),
        )
        .await?;

        if response.tool_calls.is_empty() {
            let text = response.text.unwrap_or_default();
            if let Some(schema) = run.output_schema.as_ref() {
                // No final_result call but an output was required: treat the
                // text as a last-ditch JSON payload if it parses, else fail —
                // mirrors pydantic-ai exhausting retries into an error.
                if let Ok(value) = serde_json::from_str::<Value>(text.trim()) {
                    let _ = schema;
                    return Ok(AgentOutput::Json(value));
                }
                return Err(AgentError::Llm(LlmError::EmptyOutput));
            }
            return Ok(AgentOutput::Text(text));
        }

        // Execute every requested tool call, then feed results back.
        let mut results = Vec::with_capacity(response.tool_calls.len());
        for call in &response.tool_calls {
            if run.output_schema.is_some() && call.name == "final_result" {
                return Ok(AgentOutput::Json(call.arguments.clone()));
            }
            let tool = tools.iter().find(|t| t.name == call.name);
            let content = match tool {
                Some(tool) => {
                    match (tool.handler)(call.arguments.clone(), &ToolContext::default()).await {
                        Ok(text) => text,
                        Err(e) => {
                            tracing::warn!(tool = %call.name, error = %e, "tool failed");
                            format!("Tool error: {e}")
                        }
                    }
                }
                None => format!("Unknown tool: {}", call.name),
            };
            results.push((call.id.clone(), content));
        }
        messages.push(Message::Assistant {
            text: response.text,
            tool_calls: response.tool_calls.clone(),
        });
        messages.push(Message::ToolResults(results));
    }
    Err(AgentError::IterationLimit(limit))
}

struct ChatResponse {
    text: Option<String>,
    tool_calls: Vec<ToolCall>,
}

/// One provider round trip. `final_result` is appended to the tool list when
/// an output schema is required.
async fn chat_once(
    client: &LlmClient,
    system: &str,
    messages: &[Message],
    tools: &[AgentTool],
    output_schema: Option<&Value>,
) -> Result<ChatResponse, LlmError> {
    let kind = client.provider_kind();
    match kind {
        ProviderKind::Anthropic => {
            anthropic_chat(client, system, messages, tools, output_schema).await
        }
        ProviderKind::OpenAiCompat => {
            openai_chat(client, system, messages, tools, output_schema).await
        }
    }
}

fn tool_defs<'a>(
    tools: &'a [AgentTool],
    output_schema: Option<&'a Value>,
) -> Vec<(String, String, Value)> {
    let mut defs: Vec<(String, String, Value)> = tools
        .iter()
        .map(|t| (t.name.clone(), t.description.clone(), t.parameters.clone()))
        .collect();
    if let Some(schema) = output_schema {
        defs.push((
            "final_result".to_string(),
            "Return the final structured result.".to_string(),
            schema.clone(),
        ));
    }
    defs
}

// ---- OpenAI-compatible ----

fn openai_messages(system: &str, messages: &[Message]) -> Value {
    let mut out = vec![json!({"role": "system", "content": system})];
    for msg in messages {
        match msg {
            Message::User(text) => out.push(json!({"role": "user", "content": text})),
            Message::Assistant { text, tool_calls } => {
                let mut m =
                    json!({"role": "assistant", "content": text.clone().unwrap_or_default()});
                if !tool_calls.is_empty() {
                    m["tool_calls"] = Value::Array(
                        tool_calls
                            .iter()
                            .map(|c| {
                                json!({
                                    "id": c.id,
                                    "type": "function",
                                    "function": {
                                        "name": c.name,
                                        "arguments": serde_json::to_string(&c.arguments).unwrap_or_default(),
                                    },
                                })
                            })
                            .collect(),
                    );
                }
                out.push(m);
            }
            Message::ToolResults(results) => {
                for (id, content) in results {
                    out.push(json!({"role": "tool", "tool_call_id": id, "content": content}));
                }
            }
        }
    }
    Value::Array(out)
}

async fn openai_chat(
    client: &LlmClient,
    system: &str,
    messages: &[Message],
    tools: &[AgentTool],
    output_schema: Option<&Value>,
) -> Result<ChatResponse, LlmError> {
    let defs = tool_defs(tools, output_schema);
    let mut body = json!({
        "model": client.model_name(),
        "messages": openai_messages(system, messages),
    });
    if !defs.is_empty() {
        body["tools"] = Value::Array(
            defs.iter()
                .map(|(name, desc, params)| {
                    json!({
                        "type": "function",
                        "function": {"name": name, "description": desc, "parameters": params},
                    })
                })
                .collect(),
        );
        // "auto" lets the model interleave tools and final_result freely.
        body["tool_choice"] = json!("auto");
    }
    let resp = client.post_chat(&body).await?;
    let message = resp
        .pointer("/choices/0/message")
        .ok_or(LlmError::EmptyOutput)?;
    let text = message
        .get("content")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let mut calls = Vec::new();
    if let Some(arr) = message.get("tool_calls").and_then(Value::as_array) {
        for (i, call) in arr.iter().enumerate() {
            let func = call.get("function").cloned().unwrap_or(Value::Null);
            let name = func
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let args_raw = func
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            let arguments = serde_json::from_str(args_raw).unwrap_or_else(|_| json!({}));
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("call_{i}"));
            calls.push(ToolCall {
                id,
                name,
                arguments,
            });
        }
    }
    Ok(ChatResponse {
        text,
        tool_calls: calls,
    })
}

// ---- Anthropic Messages API ----

fn anthropic_messages(messages: &[Message]) -> Value {
    // Anthropic requires alternating user/assistant turns; tool results are
    // user messages containing tool_result blocks. Consecutive ToolResults
    // merge into one user message.
    let mut out: Vec<Value> = Vec::new();
    for msg in messages {
        match msg {
            Message::User(text) => out.push(json!({"role": "user", "content": text})),
            Message::Assistant { text, tool_calls } => {
                let mut blocks: Vec<Value> = Vec::new();
                if let Some(t) = text {
                    if !t.is_empty() {
                        blocks.push(json!({"type": "text", "text": t}));
                    }
                }
                for c in tool_calls {
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": c.id,
                        "name": c.name,
                        "input": c.arguments,
                    }));
                }
                if blocks.is_empty() {
                    blocks.push(json!({"type": "text", "text": ""}));
                }
                out.push(json!({"role": "assistant", "content": blocks}));
            }
            Message::ToolResults(results) => {
                let blocks: Vec<Value> = results
                    .iter()
                    .map(|(id, content)| {
                        json!({"type": "tool_result", "tool_use_id": id, "content": content})
                    })
                    .collect();
                out.push(json!({"role": "user", "content": blocks}));
            }
        }
    }
    Value::Array(out)
}

async fn anthropic_chat(
    client: &LlmClient,
    system: &str,
    messages: &[Message],
    tools: &[AgentTool],
    output_schema: Option<&Value>,
) -> Result<ChatResponse, LlmError> {
    let defs = tool_defs(tools, output_schema);
    let mut body = json!({
        "model": client.model_name(),
        "max_tokens": client.max_tokens().unwrap_or(8192),
        "system": system,
        "messages": anthropic_messages(messages),
    });
    if !defs.is_empty() {
        body["tools"] = Value::Array(
            defs.iter()
                .map(|(name, desc, params)| {
                    json!({"name": name, "description": desc, "input_schema": params})
                })
                .collect(),
        );
    }
    let resp = client.post_chat(&body).await?;
    let blocks = resp
        .get("content")
        .and_then(Value::as_array)
        .ok_or(LlmError::EmptyOutput)?;
    let mut text_parts = Vec::new();
    let mut calls = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    text_parts.push(t.to_string());
                }
            }
            Some("tool_use") => {
                calls.push(ToolCall {
                    id: block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    name: block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    arguments: block.get("input").cloned().unwrap_or(json!({})),
                });
            }
            _ => {}
        }
    }
    let text = if text_parts.is_empty() {
        None
    } else {
        Some(text_parts.join(""))
    };
    Ok(ChatResponse {
        text,
        tool_calls: calls,
    })
}
