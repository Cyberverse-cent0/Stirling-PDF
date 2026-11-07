//! Port of `engine/tests/test_documents.py`: SqliteVecStore ACL/search
//! semantics, DocumentService ingest/search/pages behaviour, RagCapability
//! tool + instructions, and concurrent store startup. Embeddings go through
//! a stub `/embeddings` server — the Rust analogue of Python's
//! `StubEmbeddingService`.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use chrono::{Duration, Utc};
use serde_json::{json, Value};
use stirling_engine::agents::runtime::ToolContext;
use stirling_engine::contracts::documents::{PageRange, PageText};
use stirling_engine::documents::embedder::{
    EmbedderConfig, EmbeddingProvider, EmbeddingService, DEFAULT_EMBED_BATCH_SIZE,
};
use stirling_engine::documents::rag_capability::RagCapability;
use stirling_engine::documents::service::DocumentService;
use stirling_engine::documents::sqlite_vec_store::SqliteVecStore;
use stirling_engine::documents::store::{Document, DocumentStore};

const OWNER: &str = "test-user";
const OTHER_OWNER: &str = "other-user";

fn doc(id: &str, text: &str) -> Document {
    Document {
        id: id.to_string(),
        text: text.to_string(),
        metadata: HashMap::from([("source".to_string(), "test".to_string())]),
    }
}

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

fn embedder(base: &str) -> Arc<EmbeddingService> {
    Arc::new(EmbeddingService::new(
        reqwest::Client::new(),
        EmbedderConfig {
            model: "test-embed".to_string(),
            provider: EmbeddingProvider::Custom,
            api_key: None,
            base_url: Some(base.to_string()),
        },
        512,
        64,
        DEFAULT_EMBED_BATCH_SIZE,
    ))
}

async fn service() -> Arc<DocumentService> {
    let base = spawn_embedder_mock().await;
    let store: Arc<dyn DocumentStore> =
        Arc::new(SqliteVecStore::ephemeral().unwrap());
    Arc::new(DocumentService::new(embedder(&base), store, 3))
}

fn pages(text: &str) -> Vec<PageText> {
    vec![PageText {
        page_number: 1,
        text: text.to_string(),
    }]
}

// ── SqliteVecStore ─────────────────────────────────────────────────────

#[tokio::test]
async fn store_add_and_search() {
    let store = SqliteVecStore::ephemeral().unwrap();
    store
        .ensure_collection("test-col", "test.pdf", &OWNER.to_string(), None)
        .await
        .unwrap();
    store
        .grant_read("test-col", &OWNER.to_string(), &[OWNER.to_string()])
        .await
        .unwrap();
    let docs = vec![
        doc("1", "Python is a programming language"),
        doc("2", "Java is another programming language"),
        doc("3", "The weather today is sunny"),
    ];
    let embeddings = vec![
        vec![1.0, 0.0, 0.0],
        vec![0.9, 0.1, 0.0],
        vec![0.0, 0.0, 1.0],
    ];
    store
        .add_documents("test-col", &docs, &embeddings, &OWNER.to_string())
        .await
        .unwrap();

    let results = store
        .search("test-col", &[1.0, 0.05, 0.0], 2, &[OWNER.to_string()])
        .await
        .unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].document.id, "1");
    assert!(results[0].score > 0.5);
}

#[tokio::test]
async fn store_list_and_has_collection() {
    let store = SqliteVecStore::ephemeral().unwrap();
    store
        .ensure_collection("my-collection", "test.pdf", &OWNER.to_string(), None)
        .await
        .unwrap();
    store
        .grant_read("my-collection", &OWNER.to_string(), &[OWNER.to_string()])
        .await
        .unwrap();
    store
        .add_documents(
            "my-collection",
            &[doc("1", "test")],
            &[vec![1.0, 0.0]],
            &OWNER.to_string(),
        )
        .await
        .unwrap();

    let collections = store.list_collections(&[OWNER.to_string()]).await.unwrap();
    assert!(collections.contains(&"my-collection".to_string()));
    assert!(store
        .has_collection("my-collection", &[OWNER.to_string()])
        .await
        .unwrap());
    assert!(!store
        .has_collection("nonexistent", &[OWNER.to_string()])
        .await
        .unwrap());
}

#[tokio::test]
async fn store_delete_collection() {
    let store = SqliteVecStore::ephemeral().unwrap();
    store
        .ensure_collection("to-delete", "test.pdf", &OWNER.to_string(), None)
        .await
        .unwrap();
    store
        .grant_read("to-delete", &OWNER.to_string(), &[OWNER.to_string()])
        .await
        .unwrap();
    store
        .add_documents(
            "to-delete",
            &[doc("1", "test")],
            &[vec![1.0]],
            &OWNER.to_string(),
        )
        .await
        .unwrap();

    assert!(store
        .has_collection("to-delete", &[OWNER.to_string()])
        .await
        .unwrap());
    store
        .delete_collection("to-delete", &OWNER.to_string())
        .await
        .unwrap();
    assert!(!store
        .has_collection("to-delete", &[OWNER.to_string()])
        .await
        .unwrap());
}

#[tokio::test]
async fn store_search_returns_fewer_than_top_k() {
    let store = SqliteVecStore::ephemeral().unwrap();
    store
        .ensure_collection("empty-test", "test.pdf", &OWNER.to_string(), None)
        .await
        .unwrap();
    store
        .grant_read("empty-test", &OWNER.to_string(), &[OWNER.to_string()])
        .await
        .unwrap();
    store
        .add_documents(
            "empty-test",
            &[doc("1", "test")],
            &[vec![1.0, 0.0]],
            &OWNER.to_string(),
        )
        .await
        .unwrap();
    let results = store
        .search("empty-test", &[1.0, 0.0], 5, &[OWNER.to_string()])
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
}

#[tokio::test]
async fn store_rejects_mismatched_docs_and_embeddings() {
    let store = SqliteVecStore::ephemeral().unwrap();
    let err = store
        .add_documents(
            "bad",
            &[doc("1", "test")],
            &[vec![1.0], vec![2.0]],
            &OWNER.to_string(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("documents"));
}

#[tokio::test]
async fn store_collections_isolated_by_owner() {
    let store = SqliteVecStore::ephemeral().unwrap();
    for (owner, principals, name) in [
        (OWNER, vec![OWNER.to_string()], "alice.pdf"),
        (OTHER_OWNER, vec![OTHER_OWNER.to_string()], "bob.pdf"),
    ] {
        store
            .ensure_collection("shared-id", name, &owner.to_string(), None)
            .await
            .unwrap();
        store
            .grant_read("shared-id", &owner.to_string(), &principals)
            .await
            .unwrap();
    }
    store
        .add_documents(
            "shared-id",
            &[doc("1", "alice content")],
            &[vec![1.0, 0.0]],
            &OWNER.to_string(),
        )
        .await
        .unwrap();
    store
        .add_documents(
            "shared-id",
            &[doc("1", "bob content")],
            &[vec![1.0, 0.0]],
            &OTHER_OWNER.to_string(),
        )
        .await
        .unwrap();

    let alice = store
        .search("shared-id", &[1.0, 0.0], 5, &[OWNER.to_string()])
        .await
        .unwrap();
    let bob = store
        .search("shared-id", &[1.0, 0.0], 5, &[OTHER_OWNER.to_string()])
        .await
        .unwrap();
    let alice_texts: Vec<&str> = alice.iter().map(|r| r.document.text.as_str()).collect();
    let bob_texts: Vec<&str> = bob.iter().map(|r| r.document.text.as_str()).collect();
    assert_eq!(alice_texts, ["alice content"]);
    assert_eq!(bob_texts, ["bob content"]);

    store
        .delete_collection("shared-id", &OWNER.to_string())
        .await
        .unwrap();
    assert!(!store
        .has_collection("shared-id", &[OWNER.to_string()])
        .await
        .unwrap());
    assert!(store
        .has_collection("shared-id", &[OTHER_OWNER.to_string()])
        .await
        .unwrap());
}

#[tokio::test]
async fn store_purge_owner_removes_only_that_owner() {
    let store = SqliteVecStore::ephemeral().unwrap();
    for (owner, name) in [(OWNER, "a.pdf"), (OWNER, "b.pdf"), (OTHER_OWNER, "c.pdf")] {
        store
            .ensure_collection(name, name, &owner.to_string(), None)
            .await
            .unwrap();
        store
            .grant_read(name, &owner.to_string(), &[owner.to_string()])
            .await
            .unwrap();
        store
            .add_documents(
                name,
                &[doc("1", "x")],
                &[vec![1.0, 0.0]],
                &owner.to_string(),
            )
            .await
            .unwrap();
    }

    let deleted = store.purge_owner(&OWNER.to_string()).await.unwrap();
    assert_eq!(deleted, 2);
    assert!(store
        .list_collections(&[OWNER.to_string()])
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .list_collections(&[OTHER_OWNER.to_string()])
            .await
            .unwrap(),
        ["c.pdf"]
    );
}

#[tokio::test]
async fn store_reap_expired_drops_past_ttl_only() {
    let store = SqliteVecStore::ephemeral().unwrap();
    let now = Utc::now().fixed_offset();
    store
        .ensure_collection(
            "fresh",
            "fresh.pdf",
            &OWNER.to_string(),
            Some(now + Duration::hours(1)),
        )
        .await
        .unwrap();
    store
        .grant_read("fresh", &OWNER.to_string(), &[OWNER.to_string()])
        .await
        .unwrap();
    store
        .ensure_collection(
            "stale",
            "stale.pdf",
            &OWNER.to_string(),
            Some(now - Duration::seconds(1)),
        )
        .await
        .unwrap();
    store
        .grant_read("stale", &OWNER.to_string(), &[OWNER.to_string()])
        .await
        .unwrap();

    assert_eq!(store.reap_expired().await.unwrap(), 1);
    assert_eq!(
        store.list_collections(&[OWNER.to_string()]).await.unwrap(),
        ["fresh"]
    );
}

#[tokio::test]
async fn store_reap_keeps_persistent_and_unexpired() {
    let store = SqliteVecStore::ephemeral().unwrap();
    let now = Utc::now().fixed_offset();
    store
        .ensure_collection(
            "session",
            "s.pdf",
            &OWNER.to_string(),
            Some(now + Duration::hours(1)),
        )
        .await
        .unwrap();
    store
        .grant_read("session", &OWNER.to_string(), &[OWNER.to_string()])
        .await
        .unwrap();
    store
        .ensure_collection("persistent", "p.pdf", &OTHER_OWNER.to_string(), None)
        .await
        .unwrap();
    store
        .grant_read(
            "persistent",
            &OTHER_OWNER.to_string(),
            &[OTHER_OWNER.to_string()],
        )
        .await
        .unwrap();

    assert_eq!(store.reap_expired().await.unwrap(), 0);
    assert_eq!(
        store.list_collections(&[OWNER.to_string()]).await.unwrap(),
        ["session"]
    );
    assert_eq!(
        store
            .list_collections(&[OTHER_OWNER.to_string()])
            .await
            .unwrap(),
        ["persistent"]
    );
}

#[tokio::test]
async fn store_acl_grants_and_revokes_read() {
    let store = SqliteVecStore::ephemeral().unwrap();
    let eng = "group:engineering".to_string();
    store
        .ensure_collection("doc", "engineering-runbook.pdf", &OWNER.to_string(), None)
        .await
        .unwrap();
    store
        .grant_read("doc", &OWNER.to_string(), &[OWNER.to_string()])
        .await
        .unwrap();
    store
        .add_documents(
            "doc",
            &[doc("1", "how to deploy")],
            &[vec![1.0, 0.0]],
            &OWNER.to_string(),
        )
        .await
        .unwrap();

    assert!(!store.has_collection("doc", &[eng.clone()]).await.unwrap());
    store
        .grant_read("doc", &OWNER.to_string(), &[eng.clone()])
        .await
        .unwrap();
    assert!(store.has_collection("doc", &[eng.clone()]).await.unwrap());
    store.revoke("doc", &OWNER.to_string(), &eng).await.unwrap();
    assert!(!store.has_collection("doc", &[eng]).await.unwrap());
    assert!(store
        .has_collection("doc", &[OWNER.to_string()])
        .await
        .unwrap());
}

#[test]
fn many_stores_open_the_same_file_without_locking_out() {
    let dir = std::env::temp_dir().join(format!("stirling-store-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("rag.db");
    let path = db_path.clone();
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let p = path.clone();
            std::thread::spawn(move || SqliteVecStore::open(&p).unwrap())
        })
        .collect();
    let stores: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(stores.len(), 8);
    let _ = std::fs::remove_dir_all(&dir);
}

// ── DocumentService ────────────────────────────────────────────────────

#[tokio::test]
async fn service_ingest_and_search() {
    let svc = service().await;
    let count = svc
        .ingest(
            &"docs".to_string(),
            &pages(
                "Python is great for data science. It has many libraries like pandas and numpy.",
            ),
            "guide.pdf",
            &OWNER.to_string(),
            &[OWNER.to_string()],
            None,
        )
        .await
        .unwrap();
    assert!(count > 0);
    let results = svc
        .search(
            "Python libraries",
            &[OWNER.to_string()],
            Some(&"docs".to_string()),
            None,
        )
        .await
        .unwrap();
    assert!(!results.is_empty());
    assert!(!results[0].document.text.is_empty());
}

#[tokio::test]
async fn service_ingest_empty_text_returns_zero() {
    let svc = service().await;
    let count = svc
        .ingest(
            &"docs".to_string(),
            &pages(""),
            "empty.pdf",
            &OWNER.to_string(),
            &[OWNER.to_string()],
            None,
        )
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn service_search_nonexistent_collection_empty() {
    let svc = service().await;
    let results = svc
        .search(
            "anything",
            &[OWNER.to_string()],
            Some(&"nonexistent".to_string()),
            None,
        )
        .await
        .unwrap();
    assert!(results.is_empty());
}

#[tokio::test]
async fn service_search_all_collections_for_principal() {
    let svc = service().await;
    for (col, text, source) in [
        ("col-a", "Machine learning overview.", "ml.pdf"),
        ("col-b", "Deep learning with neural networks.", "dl.pdf"),
    ] {
        svc.ingest(
            &col.to_string(),
            &pages(text),
            source,
            &OWNER.to_string(),
            &[OWNER.to_string()],
            None,
        )
        .await
        .unwrap();
    }
    let results = svc
        .search("neural networks", &[OWNER.to_string()], None, None)
        .await
        .unwrap();
    assert!(!results.is_empty());
}

#[tokio::test]
async fn service_search_does_not_cross_owner_boundary() {
    let svc = service().await;
    svc.ingest(
        &"col-a".to_string(),
        &pages("Alice's private notes."),
        "alice.pdf",
        &OWNER.to_string(),
        &[OWNER.to_string()],
        None,
    )
    .await
    .unwrap();
    svc.ingest(
        &"col-b".to_string(),
        &pages("Bob's private notes."),
        "bob.pdf",
        &OTHER_OWNER.to_string(),
        &[OTHER_OWNER.to_string()],
        None,
    )
    .await
    .unwrap();

    let alice = svc
        .search("notes", &[OWNER.to_string()], None, None)
        .await
        .unwrap();
    let bob = svc
        .search("notes", &[OTHER_OWNER.to_string()], None, None)
        .await
        .unwrap();
    let alice_texts: Vec<&str> = alice.iter().map(|r| r.document.text.as_str()).collect();
    let bob_texts: Vec<&str> = bob.iter().map(|r| r.document.text.as_str()).collect();
    assert!(alice_texts.iter().any(|t| t.contains("Alice")));
    assert!(!alice_texts.iter().any(|t| t.contains("Bob")));
    assert!(bob_texts.iter().any(|t| t.contains("Bob")));
    assert!(!bob_texts.iter().any(|t| t.contains("Alice")));
}

#[tokio::test]
async fn service_org_doc_visible_only_to_granted_group() {
    let svc = service().await;
    let org = "org:acme".to_string();
    let eng = "group:engineering".to_string();
    let hr = "group:hr".to_string();
    svc.ingest(
        &"runbook".to_string(),
        &pages("Production deploy steps."),
        "runbook.pdf",
        &org,
        &[eng.clone()],
        None,
    )
    .await
    .unwrap();

    let hr_results = svc.search("deploy", &[hr.clone()], None, None).await.unwrap();
    assert!(hr_results.is_empty());
    assert!(!svc
        .search("deploy", &[eng.clone()], None, None)
        .await
        .unwrap()
        .is_empty());
    assert!(!svc
        .search("deploy", &[hr, eng], None, None)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn service_delete_collection() {
    let svc = service().await;
    svc.ingest(
        &"temp".to_string(),
        &pages("Temporary data."),
        "tmp.pdf",
        &OWNER.to_string(),
        &[OWNER.to_string()],
        None,
    )
    .await
    .unwrap();
    assert!(svc
        .list_collections(&[OWNER.to_string()])
        .await
        .unwrap()
        .contains(&"temp".to_string()));
    svc.delete_collection(&"temp".to_string(), &OWNER.to_string())
        .await
        .unwrap();
    assert!(!svc
        .list_collections(&[OWNER.to_string()])
        .await
        .unwrap()
        .contains(&"temp".to_string()));
}

#[tokio::test]
async fn service_ingest_stores_pages_in_order() {
    let svc = service().await;
    let pages = vec![
        PageText {
            page_number: 1,
            text: "First page text.".into(),
        },
        PageText {
            page_number: 2,
            text: "Second page text.".into(),
        },
        PageText {
            page_number: 3,
            text: "Third page text.".into(),
        },
    ];
    svc.ingest(
        &"ordered".to_string(),
        &pages,
        "ordered.pdf",
        &OWNER.to_string(),
        &[OWNER.to_string()],
        None,
    )
    .await
    .unwrap();
    let stored = svc
        .read_pages(&"ordered".to_string(), &[OWNER.to_string()], None)
        .await
        .unwrap();
    assert_eq!(
        stored.iter().map(|p| p.page_number).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(stored[0].text, "First page text.");
    assert_eq!(
        stored[0].char_count,
        "First page text.".chars().count() as i64
    );
}

#[tokio::test]
async fn service_read_pages_with_range() {
    let svc = service().await;
    let pages: Vec<PageText> = (1..=5)
        .map(|i| PageText {
            page_number: i,
            text: format!("page {i}"),
        })
        .collect();
    svc.ingest(
        &"ranged".to_string(),
        &pages,
        "r.pdf",
        &OWNER.to_string(),
        &[OWNER.to_string()],
        None,
    )
    .await
    .unwrap();
    let subset = svc
        .read_pages(
            &"ranged".to_string(),
            &[OWNER.to_string()],
            Some(PageRange { start: 2, end: 4 }),
        )
        .await
        .unwrap();
    assert_eq!(
        subset.iter().map(|p| p.page_number).collect::<Vec<_>>(),
        [2, 3, 4]
    );
}

#[tokio::test]
async fn service_ingest_replaces_previous_pages() {
    let svc = service().await;
    svc.ingest(
        &"doc".to_string(),
        &[
            PageText {
                page_number: 1,
                text: "old".into(),
            },
            PageText {
                page_number: 2,
                text: "old2".into(),
            },
        ],
        "v1.pdf",
        &OWNER.to_string(),
        &[OWNER.to_string()],
        None,
    )
    .await
    .unwrap();
    svc.ingest(
        &"doc".to_string(),
        &[PageText {
            page_number: 1,
            text: "new".into(),
        }],
        "v2.pdf",
        &OWNER.to_string(),
        &[OWNER.to_string()],
        None,
    )
    .await
    .unwrap();
    let stored = svc
        .read_pages(&"doc".to_string(), &[OWNER.to_string()], None)
        .await
        .unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].text, "new");
}

#[tokio::test]
async fn service_ingest_keeps_blank_pages_in_page_store() {
    let svc = service().await;
    let pages = vec![
        PageText {
            page_number: 1,
            text: "Real text on page 1.".into(),
        },
        PageText {
            page_number: 2,
            text: "   ".into(),
        },
        PageText {
            page_number: 3,
            text: "Real text on page 3.".into(),
        },
    ];
    svc.ingest(
        &"with-blanks".to_string(),
        &pages,
        "blanks.pdf",
        &OWNER.to_string(),
        &[OWNER.to_string()],
        None,
    )
    .await
    .unwrap();
    let stored = svc
        .read_pages(&"with-blanks".to_string(), &[OWNER.to_string()], None)
        .await
        .unwrap();
    assert_eq!(
        stored.iter().map(|p| p.page_number).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert!(stored[1].text.trim().is_empty());
}

// ── RagCapability ──────────────────────────────────────────────────────

async fn invoke_search(cap: &RagCapability, query: &str, max_results: Option<u64>) -> String {
    let tool = cap.tool().expect("search_knowledge tool");
    let mut args = json!({"query": query});
    if let Some(k) = max_results {
        args["max_results"] = json!(k);
    }
    (tool.handler)(args, &ToolContext::default()).await.unwrap()
}

#[tokio::test]
async fn rag_instructions_static_when_collections_pinned() {
    let svc = service().await;
    let cap = RagCapability::new(
        svc,
        vec![OWNER.to_string()],
        Some(vec!["docs".to_string(), "manuals".to_string()]),
        5,
        5,
    );
    let instructions = cap.instructions().await;
    assert!(instructions.contains("docs, manuals"));
    assert!(instructions.contains("search_knowledge"));
}

#[tokio::test]
async fn rag_instructions_dynamic_lists_collections() {
    let svc = service().await;
    for (col, text) in [("col-a", "Alpha content."), ("col-b", "Beta content.")] {
        svc.ingest(
            &col.to_string(),
            &pages(text),
            &format!("{col}.pdf"),
            &OWNER.to_string(),
            &[OWNER.to_string()],
            None,
        )
        .await
        .unwrap();
    }
    let cap = RagCapability::new(svc, vec![OWNER.to_string()], None, 5, 5);
    let instructions = cap.instructions().await;
    assert!(instructions.contains("col-a"));
    assert!(instructions.contains("col-b"));
}

#[tokio::test]
async fn rag_instructions_dynamic_when_store_empty() {
    let svc = service().await;
    let cap = RagCapability::new(svc, vec![OWNER.to_string()], None, 5, 5);
    let instructions = cap.instructions().await;
    assert!(instructions.to_lowercase().contains("empty"));
}

#[tokio::test]
async fn rag_search_returns_no_results_message_when_empty() {
    let svc = service().await;
    let cap = RagCapability::new(svc, vec![OWNER.to_string()], None, 5, 5);
    let output = invoke_search(&cap, "anything", None).await;
    assert_eq!(output, "No relevant results found in the knowledge base.");
}

#[tokio::test]
async fn rag_search_formats_results_with_source_and_score() {
    let svc = service().await;
    svc.ingest(
        &"docs".to_string(),
        &pages("Python is a programming language."),
        "guide.pdf",
        &OWNER.to_string(),
        &[OWNER.to_string()],
        None,
    )
    .await
    .unwrap();
    let cap = RagCapability::new(svc, vec![OWNER.to_string()], None, 5, 5);
    let output = invoke_search(&cap, "Python", None).await;
    assert!(output.contains("[Result 1"));
    assert!(output.contains("source: guide.pdf"));
    assert!(output.contains("chunk:"));
    assert!(output.contains("relevance:"));
}

#[tokio::test]
async fn rag_search_restricts_to_pinned_collections() {
    let svc = service().await;
    for (col, text, source) in [
        ("pinned", "Pinned collection content.", "pinned.pdf"),
        ("other", "Content in another collection.", "other.pdf"),
    ] {
        svc.ingest(
            &col.to_string(),
            &pages(text),
            source,
            &OWNER.to_string(),
            &[OWNER.to_string()],
            None,
        )
        .await
        .unwrap();
    }
    let cap = RagCapability::new(
        svc,
        vec![OWNER.to_string()],
        Some(vec!["pinned".to_string()]),
        5,
        5,
    );
    let output = invoke_search(&cap, "content", None).await;
    assert!(output.contains("pinned.pdf"));
    assert!(!output.contains("other.pdf"));
}

#[tokio::test]
async fn rag_search_respects_max_results() {
    let svc = service().await;
    let paragraphs = (0..10)
        .map(|i| format!("Paragraph {i} about topic."))
        .collect::<Vec<_>>()
        .join("\n\n");
    svc.ingest(
        &"bulk".to_string(),
        &pages(&paragraphs),
        "bulk.pdf",
        &OWNER.to_string(),
        &[OWNER.to_string()],
        None,
    )
    .await
    .unwrap();
    let cap = RagCapability::new(svc, vec![OWNER.to_string()], None, 5, 5);
    let output = invoke_search(&cap, "topic", Some(2)).await;
    assert!(output.contains("[Result 1"));
    assert!(output.contains("[Result 2"));
    assert!(!output.contains("[Result 3"));
}

#[tokio::test]
async fn rag_tool_hidden_after_budget_exhausted() {
    let svc = service().await;
    svc.ingest(
        &"docs".to_string(),
        &pages("Some content."),
        "x.pdf",
        &OWNER.to_string(),
        &[OWNER.to_string()],
        None,
    )
    .await
    .unwrap();
    let cap = RagCapability::new(svc, vec![OWNER.to_string()], None, 5, 2);
    assert!(cap.tool().is_some());
    invoke_search(&cap, "content", None).await;
    invoke_search(&cap, "content", None).await;
    assert!(cap.tool().is_none());
}
