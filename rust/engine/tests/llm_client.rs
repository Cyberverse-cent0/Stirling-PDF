//! LLM client wire-format tests against a stub provider. No real network
//! beyond localhost; covers Anthropic and OpenAI-compatible shapes, JSON
//! extraction, retry on malformed output, and API error mapping.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use stirling_engine::llm::client::LlmClient;
use stirling_engine::llm::model::{resolve_model, ProviderKind};
use tokio::sync::Semaphore;

#[derive(Clone)]
struct MockState {
    calls: Arc<AtomicUsize>,
    mode: MockMode,
}

#[derive(Clone, Copy)]
enum MockMode {
    AnthropicOk,
    AnthropicBadThenOk,
    OpenAiOk,
    OpenAiTextFallback,
    HttpError,
}

async fn handler(
    State(st): State<MockState>,
    Json(_body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let n = st.calls.fetch_add(1, Ordering::SeqCst);
    match st.mode {
        MockMode::HttpError => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": {"message": "bad request"}})),
        ),
        MockMode::AnthropicOk => (
            StatusCode::OK,
            Json(json!({
                "content": [{"type": "tool_use", "name": "final_result", "input": {"labels": ["Invoice"]}}]
            })),
        ),
        MockMode::AnthropicBadThenOk => {
            if n == 0 {
                (
                    StatusCode::OK,
                    Json(json!({"content": [{"type": "text", "text": "not json at all"}]})),
                )
            } else {
                (
                    StatusCode::OK,
                    Json(json!({
                        "content": [{"type": "tool_use", "input": {"labels": ["Report"]}}]
                    })),
                )
            }
        }
        MockMode::OpenAiOk => (
            StatusCode::OK,
            Json(json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {"name": "final_result", "arguments": "{\"labels\":[\"Contract\"]}"}
                        }]
                    }
                }]
            })),
        ),
        MockMode::OpenAiTextFallback => (
            StatusCode::OK,
            Json(json!({
                "choices": [{
                    "message": {"role": "assistant", "content": "{\"labels\":[\"Fallback\"]}"}
                }]
            })),
        ),
    }
}

async fn spawn(mode: MockMode) -> (String, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/v1/messages", post(handler))
        .route("/chat/completions", post(handler))
        .with_state(MockState {
            calls: calls.clone(),
            mode,
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), calls)
}

fn client(base_url: &str, kind: ProviderKind) -> LlmClient {
    LlmClient::new(
        reqwest::Client::new(),
        stirling_engine::llm::model::ModelSpec {
            kind,
            model_name: "test-model".to_string(),
            api_key: "test-key".to_string(),
            base_url: base_url.to_string(),
        },
        Arc::new(Semaphore::new(4)),
        Some(128),
    )
}

#[tokio::test]
async fn anthropic_tool_use_extracts_input() {
    let (base, _calls) = spawn(MockMode::AnthropicOk).await;
    let client = client(&base, ProviderKind::Anthropic);
    let out = client
        .complete_json("sys", "prompt", &json!({"type": "object"}), 0)
        .await
        .unwrap();
    assert_eq!(out["labels"][0], "Invoice");
}

#[tokio::test]
async fn anthropic_retries_malformed_output() {
    let (base, calls) = spawn(MockMode::AnthropicBadThenOk).await;
    let client = client(&base, ProviderKind::Anthropic);
    let out = client
        .complete_json("sys", "prompt", &json!({"type": "object"}), 2)
        .await
        .unwrap();
    assert_eq!(out["labels"][0], "Report");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn openai_tool_call_extracts_arguments() {
    let (base, _calls) = spawn(MockMode::OpenAiOk).await;
    let client = client(&base, ProviderKind::OpenAiCompat);
    let out = client
        .complete_json("sys", "prompt", &json!({"type": "object"}), 0)
        .await
        .unwrap();
    assert_eq!(out["labels"][0], "Contract");
}

#[tokio::test]
async fn openai_content_fallback_parses_json() {
    let (base, _calls) = spawn(MockMode::OpenAiTextFallback).await;
    let client = client(&base, ProviderKind::OpenAiCompat);
    let out = client
        .complete_json("sys", "prompt", &json!({"type": "object"}), 0)
        .await
        .unwrap();
    assert_eq!(out["labels"][0], "Fallback");
}

#[tokio::test]
async fn http_error_surfaces_status_and_body() {
    let (base, _calls) = spawn(MockMode::HttpError).await;
    let client = client(&base, ProviderKind::OpenAiCompat);
    let err = client
        .complete_text("sys", "prompt")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("400"), "unexpected error: {err}");
    assert!(err.contains("bad request"), "unexpected error: {err}");
}

#[test]
fn resolve_model_env_prefixes() {
    let spec = resolve_model("anthropic:claude-x", None, None, None).unwrap();
    assert_eq!(spec.kind, ProviderKind::Anthropic);
    assert_eq!(spec.model_name, "claude-x");

    let spec = resolve_model("openai:gpt-x", None, None, None).unwrap();
    assert_eq!(spec.kind, ProviderKind::OpenAiCompat);
    assert_eq!(spec.model_name, "gpt-x");

    let spec = resolve_model("bare-name", None, None, None).unwrap();
    assert_eq!(spec.kind, ProviderKind::OpenAiCompat);
    assert_eq!(spec.model_name, "bare-name");
}

#[test]
fn resolve_model_explicit_provider() {
    let spec = resolve_model(
        "qwen",
        Some("ollama"),
        Some("k"),
        Some("http://localhost:11434/v1"),
    )
    .unwrap();
    assert_eq!(spec.kind, ProviderKind::OpenAiCompat);
    assert_eq!(spec.api_key, "k");

    assert!(resolve_model("x", Some("bogus"), None, None).is_err());
}
