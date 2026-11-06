use std::sync::Arc;

use tokio::sync::Semaphore;

use crate::config::{AppSettings, SharedSettings};
use crate::documents::embedder::{resolve_embedder, EmbeddingService, DEFAULT_EMBED_BATCH_SIZE};
use crate::documents::pgvector_store::PgVectorStore;
use crate::documents::service::DocumentService;
use crate::documents::sqlite_vec_store::SqliteVecStore;
use crate::documents::store::DocumentStore;
use crate::llm::client::LlmClient;
use crate::llm::model::resolve_model;

/// Shared application state. Grows as runtime pieces (model clients, agents)
/// are ported; config push swaps the settings behind an `RwLock`.
#[derive(Clone)]
pub struct AppState {
    pub settings: SharedSettings,
    pub documents: Arc<DocumentService>,
    pub fast_model: LlmClient,
    pub smart_model: LlmClient,
    /// One semaphore across both tiers: the cap protects the provider account
    /// and process resources, which the tiers share (Python `ConcurrencyLimitedModel`).
    pub model_semaphore: Arc<Semaphore>,
    pub http: reqwest::Client,
}

impl AppState {
    pub fn new(settings: AppSettings) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()?;

        let embedder_config = resolve_embedder(&settings.rag_embedding_model, None, None, None)?;
        let embedder = Arc::new(EmbeddingService::new(
            http.clone(),
            embedder_config,
            settings.rag_chunk_size,
            settings.rag_chunk_overlap,
            DEFAULT_EMBED_BATCH_SIZE,
        ));

        let store: Arc<dyn DocumentStore> = match settings.documents_backend {
            crate::config::DocumentsBackend::Sqlite => {
                Arc::new(SqliteVecStore::open(&settings.documents_sqlite_path)?)
            }
            crate::config::DocumentsBackend::Pgvector => Arc::new(PgVectorStore::new(
                &settings.documents_pgvector_dsn,
                settings.documents_pgvector_pool_min_size,
                settings.documents_pgvector_pool_max_size,
            )?),
        };

        let documents = Arc::new(DocumentService::new(
            embedder,
            store,
            settings.rag_default_top_k,
        ));

        let model_semaphore = Arc::new(Semaphore::new(settings.model_max_concurrency));
        let fast_model = LlmClient::new(
            http.clone(),
            resolve_model(&settings.fast_model_name, None, None, None)?,
            model_semaphore.clone(),
            settings.fast_model_max_tokens,
        );
        let smart_model = LlmClient::new(
            http.clone(),
            resolve_model(&settings.smart_model_name, None, None, None)?,
            model_semaphore.clone(),
            settings.smart_model_max_tokens,
        );

        Ok(Self {
            settings: Arc::new(settings),
            documents,
            fast_model,
            smart_model,
            model_semaphore,
            http,
        })
    }
}
