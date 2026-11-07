//! Port of `engine/tests/test_reply_language.py`: the orchestrator binds
//! `request.locale` as the reply language for the whole delegate call
//! tree, so prompts built downstream (pdf-question's `build_prompt`,
//! pdf-edit's parameter calls, …) carry the `language_directive` line.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use stirling_engine::agents::orchestrator::OrchestratorAgent;
use stirling_engine::config::AppSettings;
use stirling_engine::contracts::common::{OrchestratorResponse, SupportedCapability};
use stirling_engine::contracts::orchestrator::OrchestratorRequest;
use stirling_engine::services::language::language_directive;
use stirling_engine::services::tracking;
use stirling_engine::state::{AppState, PushCredentials};

#[derive(Clone, Default)]
struct Mock {
    /// Queued `final_result` argument payloads, one per provider call.
    queue: Arc<Mutex<VecDeque<String>>>,
    /// Every request body the provider received, in order.
    seen: Arc<Mutex<Vec<Value>>>,
}

impl Mock {
    fn push(&self, args: Value) {
        self.queue
            .lock()
            .unwrap()
            .push_back(serde_json::to_string(&args).unwrap());
    }
}

async fn chat_completions(
    State(mock): State<Mock>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    mock.seen.lock().unwrap().push(body);
    let args = mock
        .queue
        .lock()
        .unwrap()
        .pop_front()
        .expect("mock provider queue exhausted");
    (
        StatusCode::OK,
        Json(json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "final_result", "arguments": args}
                    }]
                }
            }]
        })),
    )
}

async fn spawn_mock() -> (String, Mock) {
    let mock = Mock::default();
    let app = Router::new()
        .route("/chat/completions", post(chat_completions))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), mock)
}

fn creds(base_url: &str) -> PushCredentials {
    PushCredentials {
        provider: Some("openai".to_string()),
        api_key: Some("test".to_string()),
        base_url: Some(base_url.to_string()),
    }
}

async fn state_for(base_url: &str) -> AppState {
    let mut settings = AppSettings::for_test();
    settings.chat_provider = "openai".to_string();
    AppState::with_push(
        settings,
        Some(creds(base_url)),
        // The question path never embeds (no files ingested), so a dead
        // embedder URL is fine — it only has to resolve.
        Some((
            "mock-embed".to_string(),
            PushCredentials {
                provider: Some("custom".to_string()),
                api_key: Some("test".to_string()),
                base_url: Some("http://127.0.0.1:1".to_string()),
            },
        )),
    )
    .unwrap()
}

#[test]
fn orchestrator_request_carries_the_locale() {
    let req: OrchestratorRequest =
        serde_json::from_value(json!({"userMessage": "hi", "locale": "de-DE"})).unwrap();
    assert_eq!(req.locale.as_deref(), Some("de-DE"));
    let req: OrchestratorRequest = serde_json::from_value(json!({"userMessage": "hi"})).unwrap();
    assert_eq!(req.locale, None);
}

#[tokio::test]
async fn directive_falls_back_and_scoped_locale_pins() {
    assert!(language_directive().contains("same language as their message"));
    let out = stirling_engine::services::language::scoped(Some("fr-FR".to_string()), async {
        language_directive()
    })
    .await;
    assert!(out.contains("locale 'fr-FR'"));
}

/// The Python test monkeypatches the delegate; Rust has no seam for that,
/// so the assertion moves one level down: every provider request the
/// delegate makes must contain the locale directive in its prompt.
#[tokio::test]
async fn orchestrator_binds_the_locale_for_delegates() {
    let (base, mock) = spawn_mock().await;
    let state = state_for(&base).await;
    let agent = OrchestratorAgent::new(state);

    // Call 1: the math-intent classifier inside `orchestrate`.
    mock.push(json!({"isMath": false}));
    // Call 2: the answer agent's structured terminal response.
    mock.push(json!({"outcome": "answer", "answer": "ok", "evidence": []}));

    let request = OrchestratorRequest {
        user_message: "Combien de pages ?".to_string(),
        locale: Some("fr-FR".to_string()),
        resume_with: Some(SupportedCapability::PdfQuestion),
        files: vec![],
        conversation_history: vec![],
        artifacts: vec![],
        enabled_endpoints: vec![],
    };

    let response = tracking::scoped(Some("u-1".to_string()), async {
        agent.handle(&request).await
    })
    .await
    .unwrap();

    assert!(matches!(response, OrchestratorResponse::Answer(_)));
    let seen = mock.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    // Call 0 is the internal math-intent classifier — its prompt is not
    // user-facing and carries no directive, matching Python. Call 1 is the
    // answer agent whose `build_prompt` must pin the reply language.
    let answer_call = serde_json::to_string(&seen[1]).unwrap();
    assert!(
        answer_call.contains("locale 'fr-FR'"),
        "answer-agent prompt missing locale directive: {answer_call}"
    );
}
