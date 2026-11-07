//! Shared model semaphore caps concurrency across model tiers — port of
//! `engine/tests/test_model_concurrency.py`. Two `LlmClient`s (the fast and
//! smart tiers) share one `Semaphore`, exactly like `AppState` wires them.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use stirling_engine::llm::client::LlmClient;
use stirling_engine::llm::model::{ModelSpec, ProviderKind};
use tokio::sync::Semaphore;

#[derive(Clone)]
struct Stub {
    active: Arc<AtomicUsize>,
    max_active: Arc<AtomicUsize>,
}

async fn handler(State(st): State<Stub>, Json(_body): Json<Value>) -> Json<Value> {
    let now = st.active.fetch_add(1, Ordering::SeqCst) + 1;
    st.max_active.fetch_max(now, Ordering::SeqCst);
    // Yield so every gathered request overlaps before any completes.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    st.active.fetch_sub(1, Ordering::SeqCst);
    Json(json!({
        "choices": [{"message": {"role": "assistant", "content": "ok"}}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1}
    }))
}

async fn spawn_stub() -> (String, Arc<AtomicUsize>) {
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/v1/chat/completions", post(handler))
        .with_state(Stub {
            active: active.clone(),
            max_active: max_active.clone(),
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/v1"), max_active)
}

fn client(base_url: &str, semaphore: Arc<Semaphore>) -> LlmClient {
    LlmClient::new(
        reqwest::Client::new(),
        ModelSpec {
            kind: ProviderKind::OpenAiCompat,
            model_name: "stub".to_string(),
            api_key: "k".to_string(),
            base_url: base_url.to_string(),
        },
        semaphore,
        None,
    )
}

#[tokio::test]
async fn shared_semaphore_caps_concurrency_across_models() {
    let (base_url, max_active) = spawn_stub().await;
    let semaphore = Arc::new(Semaphore::new(2));
    let fast = client(&base_url, semaphore.clone());
    let smart = client(&base_url, semaphore);

    let mut tasks = Vec::new();
    for _ in 0..5 {
        let fast = fast.clone();
        tasks.push(tokio::spawn(async move {
            fast.complete_text("sys", "hi").await.unwrap()
        }));
        let smart = smart.clone();
        tasks.push(tokio::spawn(async move {
            smart.complete_text("sys", "hi").await.unwrap()
        }));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap(), "ok");
    }
    assert_eq!(max_active.load(Ordering::SeqCst), 2);
}
