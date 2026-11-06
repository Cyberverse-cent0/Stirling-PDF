//! `OperationShortlist` tests against a stub `/embeddings` server. The stub
//! returns one-hot vectors: batch (catalogue) requests get `e_i` per input,
//! single-input (query) requests get the one-hot of `MergePdfs`, so ranking
//! must surface that endpoint first.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use stirling_engine::contracts::common::ConversationMessage;
use stirling_engine::documents::embedder::{EmbedderConfig, EmbeddingProvider, EmbeddingService};
use stirling_engine::documents::service::DocumentService;
use stirling_engine::documents::sqlite_vec_store::SqliteVecStore;
use stirling_engine::models::tool_models::ToolEndpoint;
use stirling_engine::services::operation_shortlist::{retrieval_query, OperationShortlist};

const DIM: usize = 76;

#[derive(Clone)]
struct MockState {
    calls: Arc<AtomicUsize>,
    fail: bool,
    target: usize,
}

fn one_hot(i: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; DIM];
    v[i] = 1.0;
    v
}

async fn embeddings_handler(
    State(st): State<MockState>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    st.calls.fetch_add(1, Ordering::SeqCst);
    if st.fail {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "boom"})),
        );
    }
    let input = body["input"].as_array().unwrap();
    let data: Vec<Value> = if input.len() == 1 {
        // Query embedding: align with the target catalogue entry.
        vec![json!({"embedding": one_hot(st.target)})]
    } else {
        // Catalogue batch: e_i per input position.
        (0..input.len())
            .map(|i| json!({"embedding": one_hot(i)}))
            .collect()
    };
    (StatusCode::OK, Json(json!({"data": data})))
}

async fn spawn_mock(fail: bool) -> (String, Arc<AtomicUsize>, usize) {
    let target = ToolEndpoint::ALL
        .iter()
        .position(|e| *e == ToolEndpoint::MergePdfs)
        .unwrap();
    let state = MockState {
        calls: Arc::new(AtomicUsize::new(0)),
        fail,
        target,
    };
    let calls = state.calls.clone();
    let app = Router::new()
        .route("/embeddings", post(embeddings_handler))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), calls, target)
}

fn embedder(base_url: &str) -> Arc<EmbeddingService> {
    Arc::new(EmbeddingService::new(
        reqwest::Client::new(),
        EmbedderConfig {
            model: "mock-embed".to_string(),
            provider: EmbeddingProvider::Custom,
            api_key: None,
            base_url: Some(base_url.to_string()),
        },
        800,
        80,
        256,
    ))
}

async fn service_for(embedder: Arc<EmbeddingService>) -> Arc<DocumentService> {
    Arc::new(DocumentService::new(
        embedder,
        Arc::new(SqliteVecStore::ephemeral().unwrap()),
        5,
    ))
}

#[tokio::test]
async fn ranks_closest_operation_first_and_respects_limit() {
    let (base, calls, target) = spawn_mock(false).await;
    let documents = service_for(embedder(&base)).await;
    let shortlist = OperationShortlist::new(documents);

    let ops: Vec<ToolEndpoint> = ToolEndpoint::ALL.to_vec();
    let selected = shortlist.select("merge these pdfs", &ops, 10).await;

    assert_eq!(selected.len(), 10);
    assert_eq!(selected[0], ToolEndpoint::ALL[target]);
    assert!(selected.contains(&ToolEndpoint::MergePdfs));
    // One catalogue embed + one query embed.
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn catalogue_is_embedded_once_per_embedder() {
    let (base, calls, _target) = spawn_mock(false).await;
    let documents = service_for(embedder(&base)).await;
    let shortlist = OperationShortlist::new(documents.clone());

    let ops: Vec<ToolEndpoint> = ToolEndpoint::ALL[..20].to_vec();
    shortlist.select("a", &ops, 5).await;
    shortlist.select("b", &ops, 5).await;

    // 1 catalogue batch + 2 query embeds; the catalogue is not re-embedded.
    assert_eq!(calls.load(Ordering::SeqCst), 3);

    // A config-push embedder swap invalidates the cache.
    documents.set_embedder(embedder(&base)).await;
    shortlist.select("c", &ops, 5).await;
    assert_eq!(calls.load(Ordering::SeqCst), 5);
}

#[tokio::test]
async fn embedding_failure_falls_back_to_full_list() {
    let (base, _calls, _target) = spawn_mock(true).await;
    let documents = service_for(embedder(&base)).await;
    let shortlist = OperationShortlist::new(documents);

    let ops: Vec<ToolEndpoint> = ToolEndpoint::ALL[..30].to_vec();
    let selected = shortlist.select("anything", &ops, 10).await;
    assert_eq!(selected, ops);
}

#[tokio::test]
async fn small_or_unbounded_requests_skip_embedding() {
    let (base, calls, _target) = spawn_mock(false).await;
    let documents = service_for(embedder(&base)).await;
    let shortlist = OperationShortlist::new(documents);

    let few: Vec<ToolEndpoint> = ToolEndpoint::ALL[..3].to_vec();
    assert_eq!(shortlist.select("q", &few, 10).await, few);
    assert_eq!(shortlist.select("q", &few, 0).await, few);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn retrieval_query_joins_recent_history_and_message() {
    let history = vec![
        ConversationMessage {
            role: "user".to_string(),
            content: "first".to_string(),
        },
        ConversationMessage {
            role: "assistant".to_string(),
            content: "   ".to_string(),
        },
        ConversationMessage {
            role: "user".to_string(),
            content: "second".to_string(),
        },
    ];
    let query = retrieval_query("now do it", &history);
    assert_eq!(query, "first\nsecond\nnow do it");
}
