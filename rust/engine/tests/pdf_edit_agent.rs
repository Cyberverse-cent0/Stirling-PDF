//! `PdfEditAgent::handle` against a scripted OpenAI-compatible provider.
//! The mock returns queued `final_result` tool calls in request order —
//! selection first, then one parameter call per planned operation — and
//! records every request body so tests can assert on the prompts the
//! planner actually saw (repair notes, generated steps, need_content
//! schema gating).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use stirling_engine::agents::pdf_edit::PdfEditAgent;
use stirling_engine::config::{AppSettings, DocumentsBackend};
use stirling_engine::contracts::common::{AiFile, ExtractedFileText, PdfTextSelection};
use stirling_engine::contracts::pdf_edit::{PdfEditRequest, PdfEditResponse};
use stirling_engine::documents::embedder::{EmbedderConfig, EmbeddingProvider, EmbeddingService};
use stirling_engine::documents::service::DocumentService;
use stirling_engine::documents::sqlite_vec_store::SqliteVecStore;
use stirling_engine::llm::client::LlmClient;
use stirling_engine::llm::model::{ModelSpec, ProviderKind};
use stirling_engine::models::tool_models::ToolEndpoint;
use stirling_engine::services::operation_shortlist::OperationShortlist;
use tokio::sync::Semaphore;

const ROTATE: &str = "/api/v1/general/rotate-pdf";
const FLATTEN: &str = "/api/v1/misc/flatten";
const REPORT: &str = "/api/v1/security/accessibility-report";
const MERGE: &str = "/api/v1/general/merge-pdfs";

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

    fn calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    fn body(&self, index: usize) -> Value {
        self.seen.lock().unwrap()[index].clone()
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

fn test_settings() -> AppSettings {
    AppSettings {
        smart_model_name: "mock".to_string(),
        fast_model_name: "mock".to_string(),
        chat_provider: "openai".to_string(),
        smart_model_max_tokens: None,
        fast_model_max_tokens: None,
        model_max_concurrency: 8,
        documents_backend: DocumentsBackend::Sqlite,
        documents_sqlite_path: PathBuf::from("/tmp/unused.db"),
        documents_pgvector_dsn: String::new(),
        documents_pgvector_pool_min_size: 1,
        documents_pgvector_pool_max_size: 5,
        rag_embedding_model: "mock".to_string(),
        rag_chunk_size: 512,
        rag_chunk_overlap: 64,
        rag_default_top_k: 5,
        rag_max_searches: 3,
        planner_shortlist_size: 20,
        documents_reaper_interval_seconds: 900,
        chunked_reasoner_chars_per_slice: 12000,
        chunked_reasoner_concurrency: 4,
        chunked_reasoner_worker_timeout_seconds: 120.0,
        chunked_reasoner_notes_char_budget: 400_000,
        contradiction_detect_concurrency: 5,
        contradiction_bucket_chunk_size: 12,
        contradiction_bucket_chunk_overlap: 2,
        contradiction_canonicaliser_batch_size: 500,
        max_pages: 200,
        max_characters: 200_000,
        require_user_id: false,
        log_level: "info".to_string(),
        log_file: String::new(),
        http_debug: false,
        posthog_enabled: false,
        posthog_api_key: String::new(),
        posthog_host: String::new(),
        engine_shared_secret: String::new(),
        engine_require_auth: false,
        allow_config_push: true,
        config_cache_poll_interval_seconds: 15,
    }
}

async fn agent_for() -> (PdfEditAgent, Mock) {
    let (base, mock) = spawn_mock().await;
    let client = LlmClient::new(
        reqwest::Client::new(),
        ModelSpec {
            kind: ProviderKind::OpenAiCompat,
            model_name: "mock".to_string(),
            api_key: "test".to_string(),
            base_url: base,
        },
        Arc::new(Semaphore::new(8)),
        None,
    );
    // The shortlist never reaches the embedder here: tests enable fewer
    // operations than `planner_shortlist_size`, so `select` returns early.
    let embedder = Arc::new(EmbeddingService::new(
        reqwest::Client::new(),
        EmbedderConfig {
            model: "mock".to_string(),
            provider: EmbeddingProvider::Custom,
            api_key: None,
            base_url: Some("http://127.0.0.1:1".to_string()),
        },
        512,
        64,
        256,
    ));
    let documents = Arc::new(DocumentService::new(
        embedder,
        Arc::new(SqliteVecStore::ephemeral().unwrap()),
        5,
    ));
    let settings = Arc::new(test_settings());
    let agent = PdfEditAgent::new(
        client,
        "openai".to_string(),
        settings,
        Arc::new(OperationShortlist::new(documents)),
    );
    (agent, mock)
}

fn request(message: &str, enabled: &[ToolEndpoint]) -> PdfEditRequest {
    PdfEditRequest {
        user_message: message.to_string(),
        files: vec![AiFile {
            id: "f-1".to_string(),
            name: "scan.pdf".to_string(),
        }],
        conversation_history: vec![],
        page_text: vec![],
        enabled_endpoints: enabled.to_vec(),
    }
}

fn plan(outcome_ops: &[&str]) -> Value {
    json!({
        "outcome": "plan",
        "rationale": "because",
        "operations": outcome_ops,
        "summary": "do the thing"
    })
}

#[tokio::test]
async fn builds_a_multi_step_plan_with_parameters() {
    let enabled = [ToolEndpoint::RotatePdf, ToolEndpoint::Flatten];
    let (agent, mock) = agent_for().await;
    mock.push(plan(&[ROTATE, FLATTEN]));
    mock.push(json!({"angle": 90}));
    mock.push(json!({"flattenOnlyForms": false}));

    let response = agent
        .handle(&request("rotate then flatten", &enabled))
        .await
        .unwrap();

    let PdfEditResponse::Plan(plan) = response else {
        panic!("expected plan, got {response:?}");
    };
    assert_eq!(plan.summary, "do the thing");
    assert_eq!(plan.rationale.as_deref(), Some("because"));
    assert_eq!(plan.steps.len(), 2);
    assert_eq!(plan.steps[0].parameters["angle"], json!(90));
    assert_eq!(plan.steps[1].parameters["flattenOnlyForms"], json!(false));
    assert_eq!(mock.calls(), 3);

    // The second parameter call sees the first generated step.
    let second_params = mock.body(2);
    let prompt = serde_json::to_string(&second_params).unwrap();
    assert!(prompt.contains("Already generated steps"), "{prompt}");
    assert!(prompt.contains(ROTATE), "{prompt}");
}

#[tokio::test]
async fn unchainable_plan_gets_one_repair_attempt() {
    let enabled = [ToolEndpoint::AccessibilityReport, ToolEndpoint::RotatePdf];
    let (agent, mock) = agent_for().await;
    // accessibility-report emits JSON; rotate-pdf needs a PDF — the chain
    // cannot run, so the planner is told and retries once.
    mock.push(plan(&[REPORT, ROTATE]));
    mock.push(plan(&[ROTATE]));
    mock.push(json!({"angle": 180}));

    let response = agent
        .handle(&request("report then rotate", &enabled))
        .await
        .unwrap();

    assert!(matches!(response, PdfEditResponse::Plan(_)), "{response:?}");
    assert_eq!(mock.calls(), 3);
    // The retry prompt carries the repair note naming the failed step.
    let retry = serde_json::to_string(&mock.body(1)).unwrap();
    assert!(retry.contains("ACCESSIBILITY_REPORT"), "{retry}");
    assert!(retry.contains("different order"), "{retry}");
}

#[tokio::test]
async fn cannot_do_after_two_bad_plans() {
    let enabled = [ToolEndpoint::AccessibilityReport, ToolEndpoint::RotatePdf];
    let (agent, mock) = agent_for().await;
    mock.push(plan(&[REPORT, ROTATE]));
    mock.push(plan(&[REPORT, ROTATE]));

    let response = agent
        .handle(&request("report then rotate", &enabled))
        .await
        .unwrap();

    let PdfEditResponse::CannotDo(cannot) = response else {
        panic!("expected cannot_do, got {response:?}");
    };
    assert!(
        cannot.reason.contains("No workable order"),
        "{}",
        cannot.reason
    );
    // Two selection calls, zero parameter calls.
    assert_eq!(mock.calls(), 2);
}

#[tokio::test]
async fn clarification_and_cannot_do_pass_through() {
    let enabled = [ToolEndpoint::RotatePdf];
    let (agent, mock) = agent_for().await;
    mock.push(json!({
        "outcome": "need_clarification",
        "question": "Which pages?",
        "reason": "ambiguous"
    }));
    let response = agent.handle(&request("rotate it", &enabled)).await.unwrap();
    assert!(
        matches!(response, PdfEditResponse::NeedClarification(_)),
        "{response:?}"
    );

    let (agent, mock) = agent_for().await;
    mock.push(json!({
        "outcome": "cannot_do",
        "reason": "needs OCR"
    }));
    let response = agent
        .handle(&request("read this scan", &enabled))
        .await
        .unwrap();
    assert!(
        matches!(response, PdfEditResponse::CannotDo(_)),
        "{response:?}"
    );
}

#[tokio::test]
async fn need_content_resolves_files_and_gates_schema() {
    let enabled = [ToolEndpoint::RotatePdf];
    let (agent, mock) = agent_for().await;
    mock.push(json!({
        "outcome": "need_content",
        "reason": "need page text",
        "fileNames": ["scan.pdf"],
        "maxPages": 50
    }));

    let mut req = request("split after NEW PAGE", &enabled);
    req.files.push(AiFile {
        id: "f-2".to_string(),
        name: "other.pdf".to_string(),
    });
    let response = agent.handle(&req).await.unwrap();

    let PdfEditResponse::NeedContent(content) = response else {
        panic!("expected need_content, got {response:?}");
    };
    // Only the named file is requested.
    assert_eq!(content.files.len(), 1);
    assert_eq!(content.files[0].file.name, "scan.pdf");
    assert_eq!(
        content.files[0].content_types,
        vec![stirling_engine::contracts::common::PdfContentType::PageText]
    );
    assert_eq!(content.max_pages, 50);
    assert_eq!(content.max_characters, 200_000);

    // The selection schema offered need_content because no page text was
    // supplied.
    let tools = serde_json::to_string(&mock.body(0)).unwrap();
    assert!(tools.contains("need_content"), "{tools}");
}

#[tokio::test]
async fn need_content_is_withheld_once_page_text_exists() {
    let enabled = [ToolEndpoint::RotatePdf];
    let (agent, mock) = agent_for().await;
    mock.push(plan(&[ROTATE]));
    mock.push(json!({"angle": 90}));

    let mut req = request("rotate it", &enabled);
    req.page_text = vec![ExtractedFileText {
        file_name: "scan.pdf".to_string(),
        pages: vec![PdfTextSelection {
            page_number: Some(1),
            text: "NEW PAGE".to_string(),
        }],
    }];
    let response = agent.handle(&req).await.unwrap();
    assert!(matches!(response, PdfEditResponse::Plan(_)), "{response:?}");

    let tools = serde_json::to_string(&mock.body(0)).unwrap();
    assert!(!tools.contains("need_content"), "{tools}");
}

#[tokio::test]
async fn plan_naming_an_unavailable_operation_is_cannot_do() {
    let enabled = [ToolEndpoint::Flatten];
    let (agent, mock) = agent_for().await;
    mock.push(plan(&[ROTATE]));

    let response = agent.handle(&request("rotate it", &enabled)).await.unwrap();

    let PdfEditResponse::CannotDo(cannot) = response else {
        panic!("expected cannot_do, got {response:?}");
    };
    assert!(cannot.reason.contains("not available"), "{}", cannot.reason);
    assert!(cannot.reason.contains("ROTATE_PDF"), "{}", cannot.reason);
    // No parameter call was made for the rejected plan.
    assert_eq!(mock.calls(), 1);
}

#[tokio::test]
async fn empty_enabled_set_cannot_do_without_a_model_call() {
    let enabled: [ToolEndpoint; 0] = [];
    let (agent, mock) = agent_for().await;
    let response = agent
        .handle(&request("do anything", &enabled))
        .await
        .unwrap();
    assert!(
        matches!(response, PdfEditResponse::CannotDo(_)),
        "{response:?}"
    );
    assert_eq!(mock.calls(), 0);
}

#[tokio::test]
async fn merge_after_split_is_fan_out_not_an_error() {
    // SIMO -> MISO chains are legal: the merge consumes every split output.
    let enabled = [ToolEndpoint::SplitPages, ToolEndpoint::MergePdfs];
    let (agent, mock) = agent_for().await;
    mock.push(plan(&["/api/v1/general/split-pages", MERGE]));
    mock.push(json!({"pageNumbers": "all"}));
    mock.push(json!({}));

    let response = agent
        .handle(&request("split then merge", &enabled))
        .await
        .unwrap();
    assert!(matches!(response, PdfEditResponse::Plan(_)), "{response:?}");
    assert_eq!(mock.calls(), 3);
}

#[tokio::test]
async fn malformed_selection_output_retries_then_fails() {
    let enabled = [ToolEndpoint::RotatePdf];
    let (agent, mock) = agent_for().await;
    // openai provider: 1 native retry -> two attempts, then internal error.
    mock.push(json!({"outcome": "plan", "operations": []})); // minItems violation
    mock.push(json!({"outcome": "bogus"})); // unknown discriminator

    let result = agent.handle(&request("rotate", &enabled)).await;
    assert!(result.is_err());
    assert_eq!(mock.calls(), 2);
}

#[test]
fn unknown_enabled_endpoints_are_dropped_on_the_wire() {
    let req: PdfEditRequest = serde_json::from_value(json!({
        "userMessage": "hi",
        "enabledEndpoints": [ROTATE, "/api/v1/not-real"]
    }))
    .unwrap();
    assert_eq!(req.enabled_endpoints, vec![ToolEndpoint::RotatePdf]);
}

#[test]
fn settings_fixture_covers_every_field() {
    // Compile-time completeness check for the literal above: if a field is
    // added to AppSettings this stops compiling until the fixture is updated.
    let s = test_settings();
    assert_eq!(s.max_pages, 200);
}
