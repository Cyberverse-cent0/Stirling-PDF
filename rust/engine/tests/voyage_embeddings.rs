//! Port of `engine/tests/test_voyage_embeddings.py`: the Voyage provider
//! posts OpenAI-shaped requests to `{base}/embeddings` with bearer auth and
//! a top-level `input_type` field.

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use stirling_engine::documents::embedder::{EmbedderConfig, EmbeddingProvider, EmbeddingService};

#[derive(Clone, Default)]
struct Sent {
    auth: Arc<Mutex<Option<String>>>,
    body: Arc<Mutex<Option<Value>>>,
}

async fn embeddings(
    State(sent): State<Sent>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    *sent.auth.lock().unwrap() = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    *sent.body.lock().unwrap() = Some(body);
    (
        StatusCode::OK,
        Json(json!({
            "object": "list",
            "data": [
                {"object": "embedding", "embedding": [0.1, 0.2], "index": 0},
                {"object": "embedding", "embedding": [0.3, 0.4], "index": 1}
            ],
            "model": "voyage-4",
            "usage": {"total_tokens": 7}
        })),
    )
}

#[tokio::test]
async fn voyage_posts_openai_shape_with_input_type_and_bearer() {
    let sent = Sent::default();
    let app = Router::new()
        .route("/embeddings", post(embeddings))
        .with_state(sent.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let service = EmbeddingService::new(
        reqwest::Client::new(),
        EmbedderConfig {
            model: "voyage-4".to_string(),
            provider: EmbeddingProvider::Voyage,
            api_key: Some("pa-test-key".to_string()),
            base_url: Some(format!("http://{addr}")),
        },
        512,
        64,
        256,
    );

    let vectors = service
        .embed_documents(&["alpha".to_string(), "beta".to_string()])
        .await
        .unwrap();
    assert_eq!(vectors.len(), 2);
    assert_eq!(vectors[0], vec![0.1, 0.2]);

    let auth = sent.auth.lock().unwrap().clone().unwrap();
    assert_eq!(auth, "Bearer pa-test-key");
    let body = sent.body.lock().unwrap().clone().unwrap();
    assert_eq!(body["model"], json!("voyage-4"));
    assert_eq!(body["input"], json!(["alpha", "beta"]));
    // Voyage gets input_type at top level (pydantic-ai extra_body merge).
    assert_eq!(body["input_type"], json!("document"));

    // Queries carry the retrieval-optimised input type.
    service.embed_query("find me").await.unwrap();
    let body = sent.body.lock().unwrap().clone().unwrap();
    assert_eq!(body["input_type"], json!("query"));
    assert_eq!(body["input"], json!(["find me"]));
}
