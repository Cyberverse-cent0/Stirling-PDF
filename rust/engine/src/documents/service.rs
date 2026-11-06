//! Top-level document facade. Port of `stirling.documents.service`.
//!
//! Every document lives under a `(collection, owner_id)` pair as vector chunks
//! (RAG search) plus ordered pages (whole-document reads). `owner_id` is the
//! tenant; `principals` is the caller's accessible principal set matched
//! against the ACL on every read.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, FixedOffset};
use tokio::sync::RwLock;

use crate::contracts::documents::{Page, PageRange, PageText};
use crate::documents::embedder::EmbeddingService;
use crate::documents::store::{Document, DocumentStore, SearchResult, StoredPage};
use crate::models::{FileId, OwnerId, PrincipalId};

pub const PAGE_NUMBER_METADATA_KEY: &str = "page_number";
pub const CONTENT_TYPE_METADATA_KEY: &str = "content_type";
pub const PAGE_TEXT_CONTENT_TYPE: &str = "page_text";
pub const DOCPARSE_CHUNK_CONTENT_TYPE: &str = "docparse_chunk";

pub struct DocumentService {
    embedder: RwLock<Arc<EmbeddingService>>,
    store: Arc<dyn DocumentStore>,
    default_top_k: RwLock<usize>,
}

impl DocumentService {
    pub fn new(
        embedder: Arc<EmbeddingService>,
        store: Arc<dyn DocumentStore>,
        default_top_k: usize,
    ) -> Self {
        Self {
            embedder: RwLock::new(embedder),
            store,
            default_top_k: RwLock::new(default_top_k),
        }
    }

    pub async fn embedder(&self) -> Arc<EmbeddingService> {
        self.embedder.read().await.clone()
    }

    /// Config-push swaps the embedding model while keeping the live store;
    /// existing vectors need re-indexing.
    pub async fn set_embedder(&self, embedder: Arc<EmbeddingService>) {
        *self.embedder.write().await = embedder;
    }

    /// Config-push retunes retrieval breadth without rebuilding the store.
    pub async fn set_default_top_k(&self, value: usize) {
        *self.default_top_k.write().await = value;
    }

    pub async fn default_top_k(&self) -> usize {
        *self.default_top_k.read().await
    }

    /// Replace-ingest a document. Returns the number of vector chunks indexed.
    ///
    /// Wipes any previously-stored content for `(collection, owner_id)` and
    /// writes both representations from the same `pages` payload. Pages with
    /// empty/whitespace-only text are skipped for chunking but still written
    /// to the page store so page numbering is preserved.
    pub async fn ingest(
        &self,
        collection: &FileId,
        pages: &[PageText],
        source: &str,
        owner_id: &OwnerId,
        read_principals: &[PrincipalId],
        expires_at: Option<DateTime<FixedOffset>>,
    ) -> anyhow::Result<usize> {
        anyhow::ensure!(
            !read_principals.is_empty(),
            "read_principals must not be empty - every doc needs at least one reader"
        );

        self.store.delete_collection(collection, owner_id).await?;
        self.store
            .ensure_collection(collection, source, owner_id, expires_at)
            .await?;

        let stored_pages: Vec<StoredPage> = pages
            .iter()
            .map(|p| StoredPage {
                page_number: p.page_number,
                text: p.text.clone(),
                char_count: p.text.chars().count() as i64,
            })
            .collect();
        self.store
            .add_pages(collection, &stored_pages, owner_id)
            .await?;
        self.store
            .grant_read(collection, owner_id, read_principals)
            .await?;

        let embedder = self.embedder().await;
        let mut chunks: Vec<Document> = Vec::new();
        for page in pages {
            if page.text.trim().is_empty() {
                continue;
            }
            let mut meta = HashMap::new();
            meta.insert(
                PAGE_NUMBER_METADATA_KEY.to_string(),
                page.page_number.to_string(),
            );
            meta.insert(
                CONTENT_TYPE_METADATA_KEY.to_string(),
                PAGE_TEXT_CONTENT_TYPE.to_string(),
            );
            chunks.extend(embedder.chunk_and_prepare(
                &page.text,
                &format!("{source}:page:{}", page.page_number),
                Some(&meta),
            ));
        }

        if chunks.is_empty() {
            return Ok(0);
        }
        let texts: Vec<String> = chunks.iter().map(|d| d.text.clone()).collect();
        let embeddings = embedder.embed_documents(&texts).await?;
        self.store
            .add_documents(collection, &chunks, &embeddings, owner_id)
            .await?;
        Ok(chunks.len())
    }

    /// Replace-ingest pre-chunked content (e.g. docparse structure-aware
    /// chunks). Embedding happens before the collection is wiped: the embedder
    /// is a remote call that can fail part-way, and deleting first would leave
    /// the document erased with nothing to replace it.
    #[allow(clippy::too_many_arguments)] // mirrors the Python service signature
    pub async fn ingest_prepared(
        &self,
        collection: &FileId,
        chunks: &[(String, HashMap<String, String>)],
        source: &str,
        owner_id: &OwnerId,
        read_principals: &[PrincipalId],
        expires_at: Option<DateTime<FixedOffset>>,
        pages: Option<&[PageText]>,
    ) -> anyhow::Result<usize> {
        anyhow::ensure!(
            !read_principals.is_empty(),
            "read_principals must not be empty - every doc needs at least one reader"
        );

        let mut documents: Vec<Document> = Vec::new();
        for (text, metadata) in chunks {
            if text.trim().is_empty() {
                continue;
            }
            let index = documents.len();
            let mut meta = metadata.clone();
            meta.insert("source".to_string(), source.to_string());
            meta.insert("chunk_index".to_string(), index.to_string());
            documents.push(Document {
                id: format!("{source}:docparse:{index}"),
                text: text.clone(),
                metadata: meta,
            });
        }

        let embedder = self.embedder().await;
        let embeddings = if documents.is_empty() {
            Vec::new()
        } else {
            let texts: Vec<String> = documents.iter().map(|d| d.text.clone()).collect();
            embedder.embed_documents(&texts).await?
        };

        self.store.delete_collection(collection, owner_id).await?;
        self.store
            .ensure_collection(collection, source, owner_id, expires_at)
            .await?;
        if let Some(pages) = pages {
            let stored_pages: Vec<StoredPage> = pages
                .iter()
                .map(|p| StoredPage {
                    page_number: p.page_number,
                    text: p.text.clone(),
                    char_count: p.text.chars().count() as i64,
                })
                .collect();
            self.store
                .add_pages(collection, &stored_pages, owner_id)
                .await?;
        }
        self.store
            .grant_read(collection, owner_id, read_principals)
            .await?;

        if documents.is_empty() {
            return Ok(0);
        }
        self.store
            .add_documents(collection, &documents, &embeddings, owner_id)
            .await?;
        Ok(documents.len())
    }

    /// Embed `query` and search collections readable by `principals`. With no
    /// `collection`, search every readable collection, merge, and return
    /// top-k; per-collection errors (e.g. dimension mismatch) are skipped.
    pub async fn search(
        &self,
        query: &str,
        principals: &[PrincipalId],
        collection: Option<&FileId>,
        top_k: Option<usize>,
    ) -> anyhow::Result<Vec<SearchResult>> {
        let k = match top_k {
            Some(k) => k,
            None => self.default_top_k().await,
        };
        let embedder = self.embedder().await;
        let query_embedding = embedder.embed_query(query).await?;

        if let Some(collection) = collection {
            if !self.store.has_collection(collection, principals).await? {
                return Ok(Vec::new());
            }
            return self
                .store
                .search(collection, &query_embedding, k, principals)
                .await;
        }

        let collections = self.store.list_collections(principals).await?;
        let mut all_results: Vec<SearchResult> = Vec::new();
        for col_name in collections {
            match self
                .store
                .search(&col_name, &query_embedding, k, principals)
                .await
            {
                Ok(results) => all_results.extend(results),
                Err(e) => {
                    tracing::warn!(
                        "Skipping collection {col_name} during cross-collection search: {e}"
                    );
                }
            }
        }
        all_results.sort_by(|a, b| b.score.total_cmp(&a.score));
        all_results.truncate(k);
        Ok(all_results)
    }

    pub async fn read_pages(
        &self,
        collection: &FileId,
        principals: &[PrincipalId],
        page_range: Option<PageRange>,
    ) -> anyhow::Result<Vec<Page>> {
        self.store
            .read_pages(collection, page_range, principals)
            .await
    }

    pub async fn delete_collection(
        &self,
        collection: &FileId,
        owner_id: &OwnerId,
    ) -> anyhow::Result<bool> {
        self.store.delete_collection(collection, owner_id).await
    }

    pub async fn purge_owner(&self, owner_id: &OwnerId) -> anyhow::Result<usize> {
        self.store.purge_owner(owner_id).await
    }

    pub async fn reap_expired(&self) -> anyhow::Result<usize> {
        self.store.reap_expired().await
    }

    pub async fn has_collection(
        &self,
        collection: &FileId,
        principals: &[PrincipalId],
    ) -> anyhow::Result<bool> {
        self.store.has_collection(collection, principals).await
    }

    pub async fn list_collections(
        &self,
        principals: &[PrincipalId],
    ) -> anyhow::Result<Vec<FileId>> {
        self.store.list_collections(principals).await
    }

    pub async fn grant_read(
        &self,
        collection: &FileId,
        owner_id: &OwnerId,
        principals: &[PrincipalId],
    ) -> anyhow::Result<()> {
        self.store
            .grant_read(collection, owner_id, principals)
            .await
    }

    pub async fn revoke(
        &self,
        collection: &FileId,
        owner_id: &OwnerId,
        principal: &PrincipalId,
    ) -> anyhow::Result<()> {
        self.store.revoke(collection, owner_id, principal).await
    }

    pub async fn close(&self) -> anyhow::Result<()> {
        self.store.close().await
    }
}
