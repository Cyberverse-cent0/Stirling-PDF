//! Document storage port of `stirling.documents.store`.

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{DateTime, FixedOffset};

use crate::contracts::documents::{Page, PageRange};
use crate::models::{OwnerId, PrincipalId};

#[derive(Debug, Clone)]
pub struct Document {
    pub id: String,
    pub text: String,
    pub metadata: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub document: Document,
    pub score: f64,
}

#[derive(Debug, Clone)]
pub struct StoredPage {
    pub page_number: i64,
    pub text: String,
    pub char_count: i64,
}

/// Abstract interface for document storage backends. Each
/// `(collection, owner_id)` pair holds vector chunks (RAG search) and ordered
/// pages (whole-document reads) populated by a single ingest.
#[async_trait]
pub trait DocumentStore: Send + Sync {
    async fn ensure_collection(
        &self,
        collection: &str,
        source: &str,
        owner_id: &OwnerId,
        expires_at: Option<DateTime<FixedOffset>>,
    ) -> anyhow::Result<()>;

    /// `True` when a row matched and was deleted.
    async fn delete_collection(&self, collection: &str, owner_id: &OwnerId)
        -> anyhow::Result<bool>;

    /// Number of collections purged.
    async fn purge_owner(&self, owner_id: &OwnerId) -> anyhow::Result<usize>;

    /// Collections whose `expires_at` is set and in the past.
    async fn reap_expired(&self) -> anyhow::Result<usize>;

    async fn add_documents(
        &self,
        collection: &str,
        documents: &[Document],
        embeddings: &[Vec<f32>],
        owner_id: &OwnerId,
    ) -> anyhow::Result<()>;

    async fn add_pages(
        &self,
        collection: &str,
        pages: &[StoredPage],
        owner_id: &OwnerId,
    ) -> anyhow::Result<()>;

    async fn grant_read(
        &self,
        collection: &str,
        owner_id: &OwnerId,
        principals: &[PrincipalId],
    ) -> anyhow::Result<()>;

    async fn revoke(
        &self,
        collection: &str,
        owner_id: &OwnerId,
        principal: &PrincipalId,
    ) -> anyhow::Result<()>;

    async fn search(
        &self,
        collection: &str,
        query_embedding: &[f32],
        top_k: usize,
        principals: &[PrincipalId],
    ) -> anyhow::Result<Vec<SearchResult>>;

    async fn read_pages(
        &self,
        collection: &str,
        page_range: Option<PageRange>,
        principals: &[PrincipalId],
    ) -> anyhow::Result<Vec<Page>>;

    async fn has_collection(
        &self,
        collection: &str,
        principals: &[PrincipalId],
    ) -> anyhow::Result<bool>;

    async fn list_collections(&self, principals: &[PrincipalId]) -> anyhow::Result<Vec<String>>;

    async fn close(&self) -> anyhow::Result<()>;
}
