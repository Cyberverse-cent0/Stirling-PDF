use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::Semaphore;

use crate::agents::pdf_edit::PdfEditAgent;
use crate::config::{AppSettings, SharedSettings};
use crate::documents::embedder::{resolve_embedder, EmbeddingService, DEFAULT_EMBED_BATCH_SIZE};
use crate::documents::pgvector_store::PgVectorStore;
use crate::documents::service::DocumentService;
use crate::documents::sqlite_vec_store::SqliteVecStore;
use crate::documents::store::DocumentStore;
use crate::llm::client::LlmClient;
use crate::llm::model::resolve_model;
use crate::services::operation_shortlist::OperationShortlist;

/// Explicit provider credentials from a config push; `None` fields fall back
/// to the provider defaults inside `resolve_model`/`resolve_embedder`.
/// All-`None` means "env-driven", matching Python's `_build_model` split.
#[derive(Debug, Clone, Default)]
pub struct PushCredentials {
    pub provider: Option<String>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
}

impl PushCredentials {
    pub fn is_explicit(&self) -> bool {
        self.provider.is_some() || self.api_key.is_some() || self.base_url.is_some()
    }

    pub fn as_options(&self) -> (Option<&str>, Option<&str>, Option<&str>) {
        (
            self.provider.as_deref(),
            self.api_key.as_deref(),
            self.base_url.as_deref(),
        )
    }
}

/// The swappable runtime bundle. A config push rebuilds this atomically —
/// settings, model clients, semaphore, and the pdf-edit agent move together
/// so no request ever observes a half-applied configuration (Python swaps
/// `app.state` fields in one synchronous step for the same reason).
/// `documents` and `operation_shortlist` are shared across swaps: the store
/// and its embedding cache survive, with the embedder retuned in place.
pub struct Runtime {
    pub settings: SharedSettings,
    pub documents: Arc<DocumentService>,
    pub fast_model: LlmClient,
    pub smart_model: LlmClient,
    /// One semaphore across both tiers: the cap protects the provider account
    /// and process resources, which the tiers share (Python `ConcurrencyLimitedModel`).
    pub model_semaphore: Arc<Semaphore>,
    /// Shared catalogue-embedding cache for the pdf-edit planner.
    pub operation_shortlist: Arc<OperationShortlist>,
    pub pdf_edit: PdfEditAgent,
}

/// Shared application state. `http` never changes; everything model- or
/// settings-derived lives in `runtime` so config push can swap it whole.
#[derive(Clone)]
pub struct AppState {
    pub runtime: Arc<ArcSwap<Runtime>>,
    pub http: reqwest::Client,
    /// Last-applied config-cache stamp, shared by the push route and the
    /// sibling-worker watcher so neither re-adopts its own write.
    pub config_stamp: Arc<std::sync::Mutex<Option<(i64, u64)>>>,
}

impl AppState {
    pub fn new(settings: AppSettings) -> anyhow::Result<Self> {
        Self::with_push(settings, None, None)
    }

    /// Boot constructor for a config restored from the encrypted cache:
    /// `chat`/`embedder` carry the pushed provider credentials so models and
    /// the embedder resolve exactly as the live push did.
    pub fn with_push(
        settings: AppSettings,
        chat: Option<PushCredentials>,
        embedder: Option<(String, PushCredentials)>,
    ) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()?;
        let embedder_ref = embedder.as_ref().map(|(m, c)| (m.as_str(), c));
        let runtime = build_runtime(&settings, http.clone(), None, chat.as_ref(), embedder_ref)?;
        Ok(Self {
            runtime: Arc::new(ArcSwap::from_pointee(runtime)),
            http,
            config_stamp: Arc::new(std::sync::Mutex::new(None)),
        })
    }

    /// Swap in `settings` resolved from a config push. `chat` carries explicit
    /// provider credentials (all-`None` = env-driven); `embedder` is `Some`
    /// only when the push touched embedding fields, carrying the bare model
    /// name (the `provider:` prefix lives in the credentials). Reuses the
    /// live document store and operation shortlist, retuning the embedder in
    /// place — Python's `apply_to_app`.
    pub async fn apply_push(
        &self,
        settings: AppSettings,
        chat: Option<&PushCredentials>,
        embedder: Option<(&str, &PushCredentials)>,
    ) -> anyhow::Result<()> {
        let current = self.runtime.load();
        let documents = current.documents.clone();
        let operation_shortlist = current.operation_shortlist.clone();
        drop(current);

        // Retune retrieval breadth on the reused store without rebuilding it.
        documents
            .set_default_top_k(settings.rag_default_top_k)
            .await;
        if let Some((model, creds)) = embedder {
            let (provider, api_key, base_url) = creds.as_options();
            let config = resolve_embedder(model, provider, api_key, base_url)?;
            let service = Arc::new(EmbeddingService::new(
                self.http.clone(),
                config,
                settings.rag_chunk_size,
                settings.rag_chunk_overlap,
                DEFAULT_EMBED_BATCH_SIZE,
            ));
            documents.set_embedder(service).await;
        }

        let chat = chat.filter(|c| c.is_explicit()).cloned();
        let runtime = build_runtime(
            &settings,
            self.http.clone(),
            Some((documents, operation_shortlist)),
            chat.as_ref(),
            None,
        )?;
        self.runtime.store(Arc::new(runtime));
        Ok(())
    }
}

/// Build a runtime. `reuse` carries the live document service and operation
/// shortlist across a config push; `None` (boot) constructs the store from
/// settings. `chat` is `Some` only for explicit provider credentials.
#[allow(clippy::too_many_arguments)]
fn build_runtime(
    settings: &AppSettings,
    http: reqwest::Client,
    reuse: Option<(Arc<DocumentService>, Arc<OperationShortlist>)>,
    chat: Option<&PushCredentials>,
    embedder: Option<(&str, &PushCredentials)>,
) -> anyhow::Result<Runtime> {
    let (documents, operation_shortlist) = match reuse {
        Some(pair) => pair,
        None => {
            let config = match embedder {
                Some((model, creds)) => {
                    let (p, k, u) = creds.as_options();
                    resolve_embedder(model, p, k, u)?
                }
                None => resolve_embedder(&settings.rag_embedding_model, None, None, None)?,
            };
            let embedder = Arc::new(EmbeddingService::new(
                http.clone(),
                config,
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
            let shortlist = Arc::new(OperationShortlist::new(documents.clone()));
            (documents, shortlist)
        }
    };

    let model_semaphore = Arc::new(Semaphore::new(settings.model_max_concurrency));
    let (provider, api_key, base_url) = chat
        .map(PushCredentials::as_options)
        .unwrap_or((None, None, None));
    let fast_model = LlmClient::new(
        http.clone(),
        resolve_model(&settings.fast_model_name, provider, api_key, base_url)?,
        model_semaphore.clone(),
        settings.fast_model_max_tokens,
    );
    let smart_model = LlmClient::new(
        http.clone(),
        resolve_model(&settings.smart_model_name, provider, api_key, base_url)?,
        model_semaphore.clone(),
        settings.smart_model_max_tokens,
    );

    let settings = Arc::new(settings.clone());
    let pdf_edit = PdfEditAgent::new(
        smart_model.clone(),
        settings.chat_provider.clone(),
        settings.clone(),
        operation_shortlist.clone(),
    );

    Ok(Runtime {
        settings,
        documents,
        fast_model,
        smart_model,
        model_semaphore,
        operation_shortlist,
        pdf_edit,
    })
}
