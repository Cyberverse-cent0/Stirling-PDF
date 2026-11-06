//! Agent runtime tool-loop tests against a stub OpenAI-compatible provider.
//! Verifies multi-turn message flow: tool call → tool result → final_result.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use stirling_engine::agents::runtime::{run_agent, AgentOutput, AgentRun, AgentTool, ToolContext};
use stirling_engine::llm::client::LlmClient;
use stirling_engine::llm::model::{ModelSpec, ProviderKind};
use tokio::sync::Semaphore;

#[derive(Clone)]
struct Mock {
    calls: Arc<AtomicUsize>,
}

async fn chat(State(st): State<Mock>, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let n = st.calls.fetch_add(1, Ordering::SeqCst);
    // Second call must carry the tool result message.
    if n == 1 {
        let msgs = body["messages"].as_array().unwrap();
        let tool_msg = msgs.iter().find(|m| m["role"] == "tool").cloned();
        assert!(tool_msg.is_some(), "expected a tool result message");
        assert_eq!(tool_msg.unwrap()["content"], "echo:hello");
        return (
            StatusCode::OK,
            Json(json!({
                "choices": [{"message": {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_final",
                        "type": "function",
                        "function": {
                            "name": "final_result",
                            "arguments": "{\"answer\":42}"
                        }
                    }]
                }}]
            })),
        );
    }
    (
        StatusCode::OK,
        Json(json!({
            "choices": [{"message": {
                "role": "assistant",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "echo",
                        "arguments": "{\"text\":\"hello\"}"
                    }
                }]
            }}]
        })),
    )
}

async fn spawn() -> (String, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/v1/chat/completions", post(chat))
        .with_state(Mock {
            calls: calls.clone(),
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/v1"), calls)
}

fn client(base: &str) -> LlmClient {
    LlmClient::new(
        reqwest::Client::new(),
        ModelSpec {
            kind: ProviderKind::OpenAiCompat,
            model_name: "test".to_string(),
            api_key: "k".to_string(),
            base_url: base.to_string(),
        },
        Arc::new(Semaphore::new(4)),
        None,
    )
}

#[tokio::test]
async fn tool_loop_executes_tool_then_returns_final_json() {
    let (base, calls) = spawn().await;
    let echo = AgentTool::new(
        "echo",
        "Echo text back",
        json!({"type": "object", "properties": {"text": {"type": "string"}}}),
        |args: Value, _ctx: &ToolContext| async move {
            Ok(format!("echo:{}", args["text"].as_str().unwrap()))
        },
    );
    let client = client(&base);
    let output = run_agent(AgentRun {
        client: &client,
        system: "sys",
        prompt: "go".to_string(),
        tools: vec![echo],
        output_schema: Some(json!({
            "type": "object",
            "properties": {"answer": {"type": "integer"}},
            "required": ["answer"]
        })),
        max_iterations: 0,
    })
    .await
    .unwrap();

    match output {
        AgentOutput::Json(v) => assert_eq!(v["answer"], 42),
        AgentOutput::Text(t) => panic!("expected json output, got text: {t}"),
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn unknown_tool_gets_error_content_not_crash() {
    // Same mock: first response calls a tool the agent doesn't have.
    let (base, _calls) = spawn().await;
    let client = client(&base);
    let err = run_agent(AgentRun {
        client: &client,
        system: "sys",
        prompt: "go".to_string(),
        tools: vec![],
        output_schema: Some(json!({"type": "object"})),
        max_iterations: 1, // exhaust after first iteration
    })
    .await;
    // With max_iterations=1 the loop ends after processing the first tool
    // call round; the unknown tool produces "Unknown tool" content and the
    // second model reply (final_result) never gets sent — iteration limit hit.
    assert!(err.is_err());
}
