//! Port of `engine/tests/test_documents_routes.py`: ingest/replace/delete/
//! purge behaviour over HTTP against a stub `/embeddings` provider and an
//! ephemeral sqlite store — the Rust analogue of Python's `StubEmbedder` +
//! `SqliteVecStore.ephemeral()`.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use stirling_engine::config::AppSettings;
use stirling_engine::documents::embedder::{
    EmbedderConfig, EmbeddingProvider, EmbeddingService, DEFAULT_EMBED_BATCH_SIZE,
};
use stirling_engine::documents::service::DocumentService;
use stirling_engine::documents::sqlite_vec_store::SqliteVecStore;
use stirling_engine::documents::store::DocumentStore;
use stirling_engine::llm::client::LlmClient;
use stirling_engine::llm::model::resolve_model;
use stirling_engine::middleware;
use stirling_engine::routes;
use stirling_engine::services::operation_shortlist::OperationShortlist;
use stirling_engine::state::{AppState, Runtime};
use tower::ServiceExt;

const USER: &str = "test-user";

async fn embeddings(State(_): State<()>, Json(body): Json<Value>) -> Json<Value> {
    let n = body["input"].as_array().map_or(1, Vec::len);
    let data: Vec<Value> = (0..n)
        .map(|_| json!({"embedding": vec![0.1f32; 8]}))
        .collect();
    Json(json!({"data": data}))
}

async fn spawn_embedder_mock() -> String {
    let app = Router::new()
        .route("/v1/embeddings", post(embeddings))
        .with_state(());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/v1")
}

/// AppState whose document service embeds through the stub provider —
/// mirrors Python's dependency-override of `get_document_service`.
fn test_state(embed_base: &str) -> AppState {
    let settings = AppSettings::for_test();
    let http = reqwest::Client::new();
    let embedder = Arc::new(EmbeddingService::new(
        http.clone(),
        EmbedderConfig {
            model: "test-embed".to_string(),
            provider: EmbeddingProvider::Custom,
            api_key: None,
            base_url: Some(embed_base.to_string()),
        },
        settings.rag_chunk_size,
        settings.rag_chunk_overlap,
        DEFAULT_EMBED_BATCH_SIZE,
    ));
    let store: Arc<dyn DocumentStore> =
        Arc::new(SqliteVecStore::ephemeral().expect("ephemeral store"));
    let documents = Arc::new(DocumentService::new(
        embedder,
        store,
        settings.rag_default_top_k,
    ));
    let operation_shortlist = Arc::new(OperationShortlist::new(documents.clone()));
    let model_semaphore = Arc::new(tokio::sync::Semaphore::new(settings.model_max_concurrency));
    let fast_model = LlmClient::new(
        http.clone(),
        resolve_model(&settings.fast_model_name, None, None, None).unwrap(),
        model_semaphore.clone(),
        settings.fast_model_max_tokens,
    );
    let smart_model = LlmClient::new(
        http.clone(),
        resolve_model(&settings.smart_model_name, None, None, None).unwrap(),
        model_semaphore.clone(),
        settings.smart_model_max_tokens,
    );
    let pdf_edit = stirling_engine::agents::pdf_edit::PdfEditAgent::new(
        smart_model.clone(),
        settings.chat_provider.clone(),
        Arc::new(settings.clone()),
        operation_shortlist.clone(),
    );
    let runtime = Runtime {
        settings: Arc::new(settings),
        documents,
        fast_model,
        smart_model,
        model_semaphore,
        operation_shortlist,
        pdf_edit,
    };
    AppState {
        runtime: Arc::new(arc_swap::ArcSwap::from_pointee(runtime)),
        http,
        config_stamp: Arc::new(std::sync::Mutex::new(None)),
    }
}

/// Same layer stack as `main.rs`: user_id binds the principal, engine_auth
/// enforces the shared secret (unset in tests → pass-through).
fn app(state: AppState) -> Router {
    routes::router(state.clone())
        .layer(axum::middleware::from_fn(middleware::user_id))
        .layer(axum::middleware::from_fn_with_state(
            state,
            middleware::engine_auth,
        ))
}

async fn call(
    state: &AppState,
    method: &str,
    uri: &str,
    body: Option<Value>,
    user: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(u) = user {
        builder = builder.header("x-user-id", u);
    }
    let req = match body {
        Some(b) => builder
            .header("content-type", "application/json")
            .body(axum::body::Body::from(b.to_string()))
            .unwrap(),
        None => builder.body(axum::body::Body::empty()).unwrap(),
    };
    let resp = app(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

fn ingest_body(document_id: &str, source: &str, pages: Value) -> Value {
    json!({
        "documentId": document_id,
        "source": source,
        "pageText": pages,
        "ownerId": USER,
        "readPrincipals": [USER],
        "expiresAt": null,
    })
}

#[tokio::test]
async fn ingest_indexes_page_text() {
    let base = spawn_embedder_mock().await;
    let state = test_state(&base);
    let (status, body) = call(
        &state,
        "POST",
        "/api/v1/documents",
        Some(ingest_body(
            "doc-123",
            "report.pdf",
            json!([
                {"pageNumber": 1, "text": "The introduction covers the main topic."},
                {"pageNumber": 2, "text": "The conclusion summarises the findings."}
            ]),
        )),
        Some(USER),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["documentId"], "doc-123");
    assert!(body["chunksIndexed"].as_i64().unwrap() >= 2);
}

#[tokio::test]
async fn ingest_replaces_existing_content() {
    let base = spawn_embedder_mock().await;
    let state = test_state(&base);
    for text in [
        "Original content that existed before.",
        "New content that replaced the old.",
    ] {
        let (status, _) = call(
            &state,
            "POST",
            "/api/v1/documents",
            Some(ingest_body(
                "replace-me",
                "replace-me.pdf",
                json!([{"pageNumber": 1, "text": text}]),
            )),
            Some(USER),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    let results = state
        .runtime
        .load()
        .documents
        .search(
            "New content",
            &[USER.to_string()],
            Some(&"replace-me".to_string()),
            Some(5),
        )
        .await
        .unwrap();
    let texts: Vec<&str> = results.iter().map(|r| r.document.text.as_str()).collect();
    assert!(texts.iter().any(|t| t.contains("New content")));
    assert!(!texts.iter().any(|t| t.contains("Original content")));
}

#[tokio::test]
async fn ingest_skips_empty_pages() {
    let base = spawn_embedder_mock().await;
    let state = test_state(&base);
    let (status, body) = call(
        &state,
        "POST",
        "/api/v1/documents",
        Some(ingest_body(
            "mixed",
            "mixed.pdf",
            json!([
                {"pageNumber": 1, "text": "  "},
                {"pageNumber": 2, "text": "Real content on page 2."}
            ]),
        )),
        Some(USER),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["chunksIndexed"].as_i64().unwrap() >= 1);
}

#[tokio::test]
async fn ingest_with_no_content_returns_zero() {
    let base = spawn_embedder_mock().await;
    let state = test_state(&base);
    let mut body = ingest_body("empty", "empty.pdf", json!([]));
    body.as_object_mut().unwrap().remove("pageText");
    let (status, resp) = call(&state, "POST", "/api/v1/documents", Some(body), Some(USER)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["chunksIndexed"], 0);
}

#[tokio::test]
async fn ingest_rejects_invalid_bodies() {
    let base = spawn_embedder_mock().await;
    let state = test_state(&base);
    let cases = [
        // empty documentId
        json!({"documentId": "", "source": "x.pdf",
               "pageText": [{"pageNumber": 1, "text": "something"}],
               "ownerId": USER, "readPrincipals": [USER]}),
        // missing source
        json!({"documentId": "doc-1",
               "pageText": [{"pageNumber": 1, "text": "something"}],
               "ownerId": USER, "readPrincipals": [USER]}),
        // empty source
        json!({"documentId": "doc-1", "source": "",
               "pageText": [{"pageNumber": 1, "text": "something"}],
               "ownerId": USER, "readPrincipals": [USER]}),
        // pageNumber 0
        json!({"documentId": "bad-page", "source": "bad-page.pdf",
               "pageText": [{"pageNumber": 0, "text": "something"}],
               "ownerId": USER, "readPrincipals": [USER]}),
        // missing ownerId — never derived from the caller
        json!({"documentId": "no-owner", "source": "no-owner.pdf",
               "pageText": [{"pageNumber": 1, "text": "something"}],
               "readPrincipals": [USER]}),
        // empty ownerId
        json!({"documentId": "no-owner", "source": "no-owner.pdf",
               "pageText": [{"pageNumber": 1, "text": "something"}],
               "ownerId": "", "readPrincipals": [USER]}),
        // empty readPrincipals
        json!({"documentId": "no-readers", "source": "no-readers.pdf",
               "pageText": [{"pageNumber": 1, "text": "something"}],
               "ownerId": USER, "readPrincipals": []}),
    ];
    for body in cases {
        let (status, _) = call(&state, "POST", "/api/v1/documents", Some(body), Some(USER)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }
}

#[tokio::test]
async fn ingest_rejects_missing_user_header() {
    let base = spawn_embedder_mock().await;
    let state = test_state(&base);
    let (status, _) = call(
        &state,
        "POST",
        "/api/v1/documents",
        Some(ingest_body("doc-1", "x.pdf", json!([]))),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn delete_document_reports_deleted_flag() {
    let base = spawn_embedder_mock().await;
    let state = test_state(&base);
    call(
        &state,
        "POST",
        "/api/v1/documents",
        Some(ingest_body(
            "to-delete",
            "to-delete.pdf",
            json!([{"pageNumber": 1, "text": "Text."}]),
        )),
        Some(USER),
    )
    .await;
    let (status, body) = call(
        &state,
        "DELETE",
        "/api/v1/documents/by-id/to-delete",
        None,
        Some(USER),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"documentId": "to-delete", "deleted": true}));

    // Idempotent: deleting again reports false.
    let (_, body) = call(
        &state,
        "DELETE",
        "/api/v1/documents/by-id/to-delete",
        None,
        Some(USER),
    )
    .await;
    assert_eq!(body, json!({"documentId": "to-delete", "deleted": false}));

    let (_, body) = call(
        &state,
        "DELETE",
        "/api/v1/documents/by-id/never-existed",
        None,
        Some(USER),
    )
    .await;
    assert_eq!(
        body,
        json!({"documentId": "never-existed", "deleted": false})
    );
}

#[tokio::test]
async fn delete_document_removes_collection() {
    let base = spawn_embedder_mock().await;
    let state = test_state(&base);
    call(
        &state,
        "POST",
        "/api/v1/documents",
        Some(ingest_body(
            "gone",
            "gone.pdf",
            json!([{"pageNumber": 1, "text": "Text."}]),
        )),
        Some(USER),
    )
    .await;
    let docs = state.runtime.load().documents.clone();
    assert!(docs
        .has_collection(&"gone".to_string(), &[USER.to_string()])
        .await
        .unwrap());
    call(
        &state,
        "DELETE",
        "/api/v1/documents/by-id/gone",
        None,
        Some(USER),
    )
    .await;
    assert!(!docs
        .has_collection(&"gone".to_string(), &[USER.to_string()])
        .await
        .unwrap());
}

#[tokio::test]
async fn delete_document_rejects_missing_user_header() {
    let base = spawn_embedder_mock().await;
    let state = test_state(&base);
    let (status, _) = call(
        &state,
        "DELETE",
        "/api/v1/documents/by-id/anything",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn purge_by_owner_removes_only_callers_collections() {
    let base = spawn_embedder_mock().await;
    let state = test_state(&base);
    for (user, doc, text) in [
        ("alice", "alice-doc", "alice content"),
        ("bob", "bob-doc", "bob content"),
    ] {
        let mut body = ingest_body(
            doc,
            &format!("{doc}.pdf"),
            json!([{"pageNumber": 1, "text": text}]),
        );
        body["ownerId"] = json!(user);
        body["readPrincipals"] = json!([user]);
        call(&state, "POST", "/api/v1/documents", Some(body), Some(user)).await;
    }

    let (status, body) = call(
        &state,
        "DELETE",
        "/api/v1/documents/by-owner",
        None,
        Some("alice"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"ownerId": "alice", "deleted": 1}));

    let docs = state.runtime.load().documents.clone();
    assert!(!docs
        .has_collection(&"alice-doc".to_string(), &["alice".to_string()])
        .await
        .unwrap());
    assert!(docs
        .has_collection(&"bob-doc".to_string(), &["bob".to_string()])
        .await
        .unwrap());
}

#[tokio::test]
async fn purge_by_owner_is_idempotent() {
    let base = spawn_embedder_mock().await;
    let state = test_state(&base);
    let (status, body) = call(
        &state,
        "DELETE",
        "/api/v1/documents/by-owner",
        None,
        Some(USER),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"ownerId": USER, "deleted": 0}));
}

#[tokio::test]
async fn purge_by_owner_rejects_missing_user_header() {
    let base = spawn_embedder_mock().await;
    let state = test_state(&base);
    let (status, _) = call(&state, "DELETE", "/api/v1/documents/by-owner", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn delete_only_affects_calling_user() {
    let base = spawn_embedder_mock().await;
    let state = test_state(&base);
    for user in ["alice", "bob"] {
        let mut body = ingest_body(
            "shared",
            "shared.pdf",
            json!([{"pageNumber": 1, "text": "x"}]),
        );
        body["ownerId"] = json!(user);
        body["readPrincipals"] = json!([user]);
        call(&state, "POST", "/api/v1/documents", Some(body), Some(user)).await;
    }
    let (_, body) = call(
        &state,
        "DELETE",
        "/api/v1/documents/by-id/shared",
        None,
        Some("alice"),
    )
    .await;
    assert_eq!(body, json!({"documentId": "shared", "deleted": true}));
    // Bob's copy survives — the delete is scoped to the caller's owner_id.
    let (_, body) = call(
        &state,
        "DELETE",
        "/api/v1/documents/by-id/shared",
        None,
        Some("bob"),
    )
    .await;
    assert_eq!(body, json!({"documentId": "shared", "deleted": true}));
}
