//! End-to-end test of `POST /api/v1/documents/classify` against a stub
//! OpenAI-compatible (Ollama-shape) provider. Exercises settings loading,
//! AppState bootstrap, middleware-free route, agent prompt build, provider
//! call, and label validation.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use stirling_engine::config::AppSettings;
use stirling_engine::routes;
use stirling_engine::state::AppState;

async fn chat_completions(
    State(calls): State<Arc<AtomicUsize>>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    calls.fetch_add(1, Ordering::SeqCst);
    // Echo back a label that exists in the request's allowed vocabulary so
    // validate_labels has something to match; proves the prompt reached the
    // model by checking the system prompt is present.
    let system = body
        .pointer("/messages/0/content")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        system.contains("allowed labels"),
        "unexpected system prompt"
    );
    (
        StatusCode::OK,
        Json(json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "final_result",
                            "arguments": "{\"labels\":[\"invoice\",\"Invoice\",\"Bogus\"]}"
                        }
                    }]
                }
            }]
        })),
    )
}

#[tokio::test]
async fn classify_route_roundtrip() {
    // Stub provider.
    let calls = Arc::new(AtomicUsize::new(0));
    let mock = Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(calls.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });

    // Env for AppSettings::load (this test binary is its own process).
    let db = std::env::temp_dir().join(format!("stirling-engine-test-{}.db", std::process::id()));
    std::env::set_var("STIRLING_SMART_MODEL", "ollama:smart-test");
    std::env::set_var("STIRLING_FAST_MODEL", "ollama:fast-test");
    std::env::set_var("STIRLING_RAG_EMBEDDING_MODEL", "ollama:nomic-embed-text");
    std::env::set_var("OLLAMA_BASE_URL", format!("http://{mock_addr}/v1"));
    std::env::set_var(
        "STIRLING_DOCUMENTS_SQLITE_PATH",
        db.to_string_lossy().to_string(),
    );

    let settings = AppSettings::load().unwrap();
    let state = AppState::new(settings).unwrap();
    let app = routes::router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/api/v1/documents/classify"))
        .json(&json!({
            "fileName": "invoice.pdf",
            "pages": [{"pageNumber": 1, "text": "INVOICE\nAmount due: $100"}],
            "labels": [
                {"id": "inv", "name": "Invoice"},
                {"id": "rep", "name": "Report"}
            ]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    // "invoice"/"Invoice" both match "Invoice" → deduped to id "inv"; "Bogus" dropped.
    assert_eq!(body["labels"], json!(["inv"]));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let _ = std::fs::remove_file(&db);
}
