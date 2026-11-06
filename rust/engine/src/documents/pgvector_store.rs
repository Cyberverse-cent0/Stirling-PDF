//! PostgreSQL + pgvector backed store. Port of
//! `stirling.documents.pgvector_store`.
//!
//! Vectors cross the wire as pgvector's text form (`'[1,2,3]'` cast to
//! `vector`), so no pgvector client crate is needed; `<=>` returns cosine
//! distance as float8 and `1 - distance` is the score, matching Python.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, FixedOffset};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgPool, Row};
use tokio::sync::{Mutex, OnceCell};

use crate::contracts::documents::{Page, PageRange};
use crate::documents::store::{Document, DocumentStore, SearchResult, StoredPage};
use crate::models::{OwnerId, PrincipalId};

const READ_PERMISSION: &str = "read";

/// pgvector text literal: `'[1.0,2.0,...]'`.
fn vec_literal(v: &[f32]) -> String {
    let mut s = String::with_capacity(v.len() * 12 + 2);
    s.push('[');
    for (i, f) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&f.to_string());
    }
    s.push(']');
    s
}

pub struct PgVectorStore {
    dsn: String,
    pool_min_size: u32,
    pool_max_size: u32,
    pool: OnceCell<PgPool>,
    init_lock: Mutex<()>,
}

impl PgVectorStore {
    pub fn new(dsn: &str, pool_min_size: u32, pool_max_size: u32) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !dsn.is_empty(),
            "pgvector backend requires a non-empty DSN (STIRLING_DOCUMENTS_PGVECTOR_DSN)"
        );
        Ok(Self {
            dsn: dsn.to_string(),
            pool_min_size,
            pool_max_size,
            pool: OnceCell::new(),
            init_lock: Mutex::new(()),
        })
    }

    async fn pool(&self) -> anyhow::Result<&PgPool> {
        self.pool
            .get_or_try_init(|| async {
                let _guard = self.init_lock.lock().await;
                if let Some(pool) = self.pool.get() {
                    return Ok::<PgPool, anyhow::Error>(pool.clone());
                }
                self.bootstrap_schema().await?;
                let pool = PgPoolOptions::new()
                    .min_connections(self.pool_min_size)
                    .max_connections(self.pool_max_size)
                    .connect(&self.dsn)
                    .await?;
                Ok::<PgPool, anyhow::Error>(pool)
            })
            .await
    }

    /// The `vector` type must exist before pgvector queries run; on a fresh
    /// database the extension doesn't yet, so bootstrap on a raw connection.
    async fn bootstrap_schema(&self) -> anyhow::Result<()> {
        let mut conn = sqlx::PgConnection::connect(&self.dsn).await?;
        sqlx::query("CREATE EXTENSION IF NOT EXISTS vector")
            .execute(&mut conn)
            .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS documents_meta (
                collection TEXT NOT NULL,
                owner_id TEXT NOT NULL,
                source TEXT NOT NULL,
                expires_at TIMESTAMP WITH TIME ZONE,
                PRIMARY KEY (collection, owner_id)
            )",
        )
        .execute(&mut conn)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_meta_expires_at
             ON documents_meta(expires_at) WHERE expires_at IS NOT NULL",
        )
        .execute(&mut conn)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS rag_documents (
                id TEXT NOT NULL,
                collection TEXT NOT NULL,
                owner_id TEXT NOT NULL,
                text TEXT NOT NULL,
                metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
                embedding vector NOT NULL,
                PRIMARY KEY (id, collection, owner_id),
                FOREIGN KEY (collection, owner_id)
                    REFERENCES documents_meta(collection, owner_id) ON DELETE CASCADE
            )",
        )
        .execute(&mut conn)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_rag_collection_owner ON rag_documents(collection, owner_id)",
        )
        .execute(&mut conn)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS document_pages (
                collection TEXT NOT NULL,
                owner_id TEXT NOT NULL,
                page_number INTEGER NOT NULL,
                text TEXT NOT NULL,
                char_count INTEGER NOT NULL,
                PRIMARY KEY (collection, owner_id, page_number),
                FOREIGN KEY (collection, owner_id)
                    REFERENCES documents_meta(collection, owner_id) ON DELETE CASCADE
            )",
        )
        .execute(&mut conn)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_pages_collection_owner ON document_pages(collection, owner_id)",
        )
        .execute(&mut conn)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS document_acl (
                collection TEXT NOT NULL,
                owner_id TEXT NOT NULL,
                principal_id TEXT NOT NULL,
                permission TEXT NOT NULL,
                PRIMARY KEY (collection, owner_id, principal_id, permission),
                FOREIGN KEY (collection, owner_id)
                    REFERENCES documents_meta(collection, owner_id) ON DELETE CASCADE
            )",
        )
        .execute(&mut conn)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_acl_principal_permission ON document_acl(principal_id, permission)",
        )
        .execute(&mut conn)
        .await?;
        conn.close().await?;
        Ok(())
    }

    async fn readable_owner_for(
        conn: &mut sqlx::PgConnection,
        collection: &str,
        principals: &[PrincipalId],
    ) -> anyhow::Result<Option<String>> {
        let owner: Option<String> = sqlx::query_scalar(
            "SELECT owner_id FROM document_acl
             WHERE collection = $1 AND permission = $2 AND principal_id = ANY($3)
             ORDER BY owner_id LIMIT 1",
        )
        .bind(collection)
        .bind(READ_PERMISSION)
        .bind(principals)
        .fetch_optional(conn)
        .await?;
        Ok(owner)
    }
}

#[async_trait]
impl DocumentStore for PgVectorStore {
    async fn ensure_collection(
        &self,
        collection: &str,
        source: &str,
        owner_id: &OwnerId,
        expires_at: Option<DateTime<FixedOffset>>,
    ) -> anyhow::Result<()> {
        let pool = self.pool().await?;
        sqlx::query(
            "INSERT INTO documents_meta (collection, owner_id, source, expires_at)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (collection, owner_id) DO UPDATE SET
                 source = EXCLUDED.source, expires_at = EXCLUDED.expires_at",
        )
        .bind(collection)
        .bind(owner_id)
        .bind(source)
        .bind(expires_at)
        .execute(pool)
        .await?;
        Ok(())
    }

    async fn purge_owner(&self, owner_id: &OwnerId) -> anyhow::Result<usize> {
        let pool = self.pool().await?;
        let n = sqlx::query("DELETE FROM documents_meta WHERE owner_id = $1")
            .bind(owner_id)
            .execute(pool)
            .await?
            .rows_affected() as usize;
        Ok(n)
    }

    async fn reap_expired(&self) -> anyhow::Result<usize> {
        let pool = self.pool().await?;
        let n = sqlx::query(
            "DELETE FROM documents_meta WHERE expires_at IS NOT NULL AND expires_at < NOW()",
        )
        .execute(pool)
        .await?
        .rows_affected() as usize;
        Ok(n)
    }

    async fn delete_collection(
        &self,
        collection: &str,
        owner_id: &OwnerId,
    ) -> anyhow::Result<bool> {
        let pool = self.pool().await?;
        // Cascade FKs handle rag_documents, document_pages, document_acl.
        let n = sqlx::query("DELETE FROM documents_meta WHERE collection = $1 AND owner_id = $2")
            .bind(collection)
            .bind(owner_id)
            .execute(pool)
            .await?
            .rows_affected();
        Ok(n > 0)
    }

    async fn add_documents(
        &self,
        collection: &str,
        documents: &[Document],
        embeddings: &[Vec<f32>],
        owner_id: &OwnerId,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            documents.len() == embeddings.len(),
            "Got {} documents but {} embeddings",
            documents.len(),
            embeddings.len()
        );
        if documents.is_empty() {
            return Ok(());
        }
        let pool = self.pool().await?;
        let mut tx = pool.begin().await?;
        for (doc, emb) in documents.iter().zip(embeddings.iter()) {
            let metadata = serde_json::to_string(&doc.metadata)?;
            sqlx::query(
                "INSERT INTO rag_documents (id, collection, owner_id, text, metadata, embedding)
                 VALUES ($1, $2, $3, $4, $5::jsonb, $6::vector)
                 ON CONFLICT (id, collection, owner_id) DO UPDATE SET
                     text = EXCLUDED.text, metadata = EXCLUDED.metadata, embedding = EXCLUDED.embedding",
            )
            .bind(&doc.id)
            .bind(collection)
            .bind(owner_id)
            .bind(&doc.text)
            .bind(&metadata)
            .bind(vec_literal(emb))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn add_pages(
        &self,
        collection: &str,
        pages: &[StoredPage],
        owner_id: &OwnerId,
    ) -> anyhow::Result<()> {
        let pool = self.pool().await?;
        let mut tx = pool.begin().await?;
        sqlx::query("DELETE FROM document_pages WHERE collection = $1 AND owner_id = $2")
            .bind(collection)
            .bind(owner_id)
            .execute(&mut *tx)
            .await?;
        for p in pages {
            sqlx::query(
                "INSERT INTO document_pages (collection, owner_id, page_number, text, char_count)
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(collection)
            .bind(owner_id)
            .bind(p.page_number)
            .bind(&p.text)
            .bind(p.char_count)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn grant_read(
        &self,
        collection: &str,
        owner_id: &OwnerId,
        principals: &[PrincipalId],
    ) -> anyhow::Result<()> {
        if principals.is_empty() {
            return Ok(());
        }
        let pool = self.pool().await?;
        for p in principals {
            sqlx::query(
                "INSERT INTO document_acl (collection, owner_id, principal_id, permission)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (collection, owner_id, principal_id, permission) DO NOTHING",
            )
            .bind(collection)
            .bind(owner_id)
            .bind(p)
            .bind(READ_PERMISSION)
            .execute(pool)
            .await?;
        }
        Ok(())
    }

    async fn revoke(
        &self,
        collection: &str,
        owner_id: &OwnerId,
        principal: &PrincipalId,
    ) -> anyhow::Result<()> {
        let pool = self.pool().await?;
        sqlx::query(
            "DELETE FROM document_acl WHERE collection = $1 AND owner_id = $2 AND principal_id = $3",
        )
        .bind(collection)
        .bind(owner_id)
        .bind(principal)
        .execute(pool)
        .await?;
        Ok(())
    }

    async fn search(
        &self,
        collection: &str,
        query_embedding: &[f32],
        top_k: usize,
        principals: &[PrincipalId],
    ) -> anyhow::Result<Vec<SearchResult>> {
        if principals.is_empty() {
            return Ok(Vec::new());
        }
        let pool = self.pool().await?;
        let mut conn = pool.acquire().await?;
        let Some(owner_id) = Self::readable_owner_for(&mut conn, collection, principals).await?
        else {
            return Ok(Vec::new());
        };
        let qvec = vec_literal(query_embedding);
        let rows = sqlx::query(
            "SELECT id, text, metadata, 1 - (embedding <=> $1::vector) AS score
             FROM rag_documents
             WHERE collection = $2 AND owner_id = $3
             ORDER BY embedding <=> $1::vector
             LIMIT $4",
        )
        .bind(&qvec)
        .bind(collection)
        .bind(&owner_id)
        .bind(top_k as i64)
        .fetch_all(&mut *conn)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let metadata: serde_json::Value = r.try_get("metadata").unwrap_or_default();
            let metadata: std::collections::HashMap<String, String> =
                serde_json::from_value(metadata).unwrap_or_default();
            out.push(SearchResult {
                document: Document {
                    id: r.try_get("id")?,
                    text: r.try_get("text")?,
                    metadata,
                },
                score: r.try_get::<f64, _>("score")?,
            });
        }
        Ok(out)
    }

    async fn read_pages(
        &self,
        collection: &str,
        page_range: Option<PageRange>,
        principals: &[PrincipalId],
    ) -> anyhow::Result<Vec<Page>> {
        if principals.is_empty() {
            return Ok(Vec::new());
        }
        let pool = self.pool().await?;
        let mut conn = pool.acquire().await?;
        let Some(owner_id) = Self::readable_owner_for(&mut conn, collection, principals).await?
        else {
            return Ok(Vec::new());
        };
        let rows = match page_range {
            None => {
                sqlx::query(
                    "SELECT page_number, text, char_count FROM document_pages
                 WHERE collection = $1 AND owner_id = $2 ORDER BY page_number",
                )
                .bind(collection)
                .bind(&owner_id)
                .fetch_all(&mut *conn)
                .await?
            }
            Some(range) => {
                sqlx::query(
                    "SELECT page_number, text, char_count FROM document_pages
                 WHERE collection = $1 AND owner_id = $2
                   AND page_number BETWEEN $3 AND $4
                 ORDER BY page_number",
                )
                .bind(collection)
                .bind(&owner_id)
                .bind(range.start)
                .bind(range.end)
                .fetch_all(&mut *conn)
                .await?
            }
        };
        Ok(rows
            .iter()
            .map(|r| Page {
                page_number: r.try_get("page_number").unwrap_or(0),
                text: r.try_get("text").unwrap_or_default(),
                char_count: r.try_get("char_count").unwrap_or(0),
            })
            .collect())
    }

    async fn has_collection(
        &self,
        collection: &str,
        principals: &[PrincipalId],
    ) -> anyhow::Result<bool> {
        if principals.is_empty() {
            return Ok(false);
        }
        let pool = self.pool().await?;
        let mut conn = pool.acquire().await?;
        Ok(Self::readable_owner_for(&mut conn, collection, principals)
            .await?
            .is_some())
    }

    async fn list_collections(&self, principals: &[PrincipalId]) -> anyhow::Result<Vec<String>> {
        if principals.is_empty() {
            return Ok(Vec::new());
        }
        let pool = self.pool().await?;
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT collection FROM document_acl
             WHERE permission = $1 AND principal_id = ANY($2)
             ORDER BY collection",
        )
        .bind(READ_PERMISSION)
        .bind(principals)
        .fetch_all(pool)
        .await?;
        Ok(rows)
    }

    async fn close(&self) -> anyhow::Result<()> {
        if let Some(pool) = self.pool.get() {
            pool.close().await;
        }
        Ok(())
    }
}

/// `Arc` alias kept for symmetry with the Python store's shared ownership.
#[allow(dead_code)]
type SharedPool = Arc<PgPool>;
