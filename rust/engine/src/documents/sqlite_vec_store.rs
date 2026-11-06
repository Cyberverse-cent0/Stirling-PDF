//! SQLite-backed document store with brute-force KNN over normalized
//! embeddings. Port of `stirling.documents.sqlite_vec_store` — single-file
//! SQLite, one vector table per `(collection, owner_id)`, metadata in
//! regular tables. Vectors live as f32-le blobs in a plain table; scoring is
//! a rayon-parallel dot-product scan over an in-memory decoded cache. This
//! replaces the vendored sqlite-vec `vec0` extension, which was never
//! configured with an ANN index and so performed the same brute-force scan.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, FixedOffset, NaiveDateTime, TimeZone, Utc};
use rayon::prelude::*;
use rusqlite::{params, Connection, OptionalExtension, ToSql};
use tokio::sync::Mutex;

use crate::contracts::documents::{Page, PageRange};
use crate::documents::store::{Document, DocumentStore, SearchResult, StoredPage};
use crate::models::{OwnerId, PrincipalId};

const READ_PERMISSION: &str = "read";
const BUSY_TIMEOUT_MS: i64 = 5000;
const WAL_SWITCH_ATTEMPTS: usize = 10;
const WAL_RETRY_DELAY: Duration = Duration::from_millis(100);
const SQLITE_DATETIME_FMT: &str = "%Y-%m-%d %H:%M:%S";
/// Keeps document-hydration `IN` clauses under SQLite's bound-parameter cap.
const DOC_FETCH_CHUNK: usize = 400;

fn to_sqlite_utc(dt: Option<DateTime<FixedOffset>>) -> Option<String> {
    dt.map(|d| {
        d.with_timezone(&Utc)
            .naive_utc()
            .format(SQLITE_DATETIME_FMT)
            .to_string()
    })
}

fn normalize(vector: &[f32]) -> Vec<f32> {
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm == 0.0 {
        return vector.to_vec();
    }
    vector.iter().map(|x| x / norm).collect()
}

/// Embeddings persist as little-endian f32 blobs.
fn serialize_f32(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Unrolled dot product; the eight independent accumulators give the
/// compiler ILP/SIMD-friendly code without depending on fast-math.
fn dot(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    let mut acc = [0.0f32; 8];
    let (ac, ar) = a.as_chunks::<8>();
    let (bc, br) = b.as_chunks::<8>();
    for (x, y) in ac.iter().zip(bc) {
        for j in 0..8 {
            acc[j] += x[j] * y[j];
        }
    }
    let mut sum: f32 = acc.iter().sum();
    for (x, y) in ar.iter().zip(br) {
        sum += x * y;
    }
    sum
}

/// Decoded vectors for one `(collection, owner_id)` table, cached so repeat
/// searches skip the blob decode. Invalidated on every write to the table.
struct VecIndex {
    rowids: Vec<i64>,
    dim: usize,
    /// Row-major `rowids.len() * dim` matrix.
    data: Vec<f32>,
}

fn sanitize_table_name(collection: &str, owner_id: &str) -> String {
    let clean = |s: &str| {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>()
    };
    format!("vec_{}_{}", clean(owner_id), clean(collection))
}

fn enable_wal(conn: &Connection) {
    for _ in 0..WAL_SWITCH_ATTEMPTS {
        match conn.execute_batch("PRAGMA journal_mode=WAL") {
            Ok(()) => return,
            Err(_) => {
                let mode: String = conn
                    .query_row("PRAGMA journal_mode", [], |r| r.get(0))
                    .unwrap_or_default();
                if mode.eq_ignore_ascii_case("wal") {
                    return;
                }
                std::thread::sleep(WAL_RETRY_DELAY);
            }
        }
    }
    tracing::warn!(
        "Could not switch the document store to WAL; continuing on the default journal mode."
    );
}

pub struct SqliteVecStore {
    conn: Mutex<Connection>,
    /// `std::sync::Mutex`, not tokio: guards are only taken while the
    /// connection lock is held, where an `.await` would make the future
    /// `!Send` (`rusqlite::Connection` is not `Sync`).
    vec_index: std::sync::Mutex<HashMap<String, Arc<VecIndex>>>,
    db_path: Option<PathBuf>,
}

impl SqliteVecStore {
    pub fn open(db_path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(db_path)?;
        conn.execute_batch("PRAGMA foreign_keys=ON")?;
        conn.execute_batch(&format!("PRAGMA busy_timeout={BUSY_TIMEOUT_MS}"))?;
        enable_wal(&conn);
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            vec_index: std::sync::Mutex::new(HashMap::new()),
            db_path: Some(db_path.to_path_buf()),
        })
    }

    /// In-memory store for tests (`:memory:` in Python).
    pub fn ephemeral() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys=ON")?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            vec_index: std::sync::Mutex::new(HashMap::new()),
            db_path: None,
        })
    }

    fn init_schema(conn: &Connection) -> anyhow::Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS documents_meta (
                collection TEXT NOT NULL,
                owner_id TEXT NOT NULL,
                source TEXT NOT NULL,
                expires_at TIMESTAMP,
                PRIMARY KEY (collection, owner_id)
            );
            CREATE INDEX IF NOT EXISTS idx_meta_expires_at ON documents_meta(expires_at) WHERE expires_at IS NOT NULL;
            CREATE TABLE IF NOT EXISTS collections (
                collection TEXT NOT NULL,
                owner_id TEXT NOT NULL,
                dim INTEGER NOT NULL,
                table_name TEXT NOT NULL,
                PRIMARY KEY (collection, owner_id),
                FOREIGN KEY (collection, owner_id)
                    REFERENCES documents_meta(collection, owner_id) ON DELETE CASCADE
            );
            CREATE TABLE IF NOT EXISTS documents (
                id TEXT NOT NULL,
                collection TEXT NOT NULL,
                owner_id TEXT NOT NULL,
                text TEXT NOT NULL,
                metadata TEXT NOT NULL DEFAULT '{}',
                vec_rowid INTEGER NOT NULL,
                PRIMARY KEY (id, collection, owner_id),
                FOREIGN KEY (collection, owner_id)
                    REFERENCES documents_meta(collection, owner_id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_doc_collection_owner ON documents(collection, owner_id);
            CREATE TABLE IF NOT EXISTS document_pages (
                collection TEXT NOT NULL,
                owner_id TEXT NOT NULL,
                page_number INTEGER NOT NULL,
                text TEXT NOT NULL,
                char_count INTEGER NOT NULL,
                PRIMARY KEY (collection, owner_id, page_number),
                FOREIGN KEY (collection, owner_id)
                    REFERENCES documents_meta(collection, owner_id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_pages_collection_owner ON document_pages(collection, owner_id);
            CREATE TABLE IF NOT EXISTS document_acl (
                collection TEXT NOT NULL,
                owner_id TEXT NOT NULL,
                principal_id TEXT NOT NULL,
                permission TEXT NOT NULL,
                PRIMARY KEY (collection, owner_id, principal_id, permission),
                FOREIGN KEY (collection, owner_id)
                    REFERENCES documents_meta(collection, owner_id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_acl_principal_permission ON document_acl(principal_id, permission);",
        )?;
        Self::drop_legacy_vec0_tables(conn)?;
        Ok(())
    }

    /// Databases written while the store used the sqlite-vec extension keep
    /// `vec0` virtual tables this binary can no longer query. Their vectors
    /// are derived data, so drop the tables and their collection rows; the
    /// next ingest recreates them as plain blob tables.
    fn drop_legacy_vec0_tables(conn: &Connection) -> anyhow::Result<()> {
        let vec_tables: Vec<String> = {
            let mut stmt = conn.prepare(
                "SELECT name FROM sqlite_master
                 WHERE type = 'table' AND sql LIKE '%USING vec0%'",
            )?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            rows.collect::<Result<_, _>>()?
        };
        for name in vec_tables {
            tracing::warn!("dropping legacy vec0 table {name}; collection must be re-ingested");
            let keys: Vec<(String, String)> = {
                let mut stmt = conn
                    .prepare("SELECT collection, owner_id FROM collections WHERE table_name = ?")?;
                let rows = stmt.query_map(params![name], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?;
                rows.collect::<Result<_, _>>()?
            };
            conn.execute_batch(&format!("DROP TABLE IF EXISTS {name}"))?;
            for (collection, owner_id) in keys {
                conn.execute(
                    "DELETE FROM documents WHERE collection = ? AND owner_id = ?",
                    params![collection, owner_id],
                )?;
                conn.execute(
                    "DELETE FROM collections WHERE collection = ? AND owner_id = ?",
                    params![collection, owner_id],
                )?;
            }
        }
        Ok(())
    }

    fn readable_owner_for(
        conn: &Connection,
        collection: &str,
        principals: &[PrincipalId],
    ) -> anyhow::Result<Option<String>> {
        if principals.is_empty() {
            return Ok(None);
        }
        let placeholders = principals.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT owner_id FROM document_acl
             WHERE collection = ? AND permission = ? AND principal_id IN ({placeholders})
             ORDER BY owner_id LIMIT 1"
        );
        let mut params: Vec<&dyn rusqlite::ToSql> = vec![&collection, &READ_PERMISSION];
        params.extend(principals.iter().map(|p| p as &dyn rusqlite::ToSql));
        let owner: Option<String> = conn
            .query_row(&sql, params.as_slice(), |r| r.get(0))
            .optional()?;
        Ok(owner)
    }

    /// Returns the decoded vector matrix for `table_name`, building and
    /// caching it on first use. Caller must hold the connection lock so the
    /// cache cannot go stale mid-read.
    fn load_index(
        &self,
        conn: &Connection,
        table_name: &str,
        dim: usize,
    ) -> anyhow::Result<Arc<VecIndex>> {
        if let Some(index) = self
            .vec_index
            .lock()
            .expect("vec_index poisoned")
            .get(table_name)
        {
            return Ok(Arc::clone(index));
        }
        let mut stmt = conn.prepare(&format!("SELECT rowid, embedding FROM {table_name}"))?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))?;
        let mut rowids = Vec::new();
        let mut data = Vec::new();
        for row in rows {
            let (rowid, blob) = row?;
            anyhow::ensure!(
                blob.len() == dim * 4,
                "embedding blob is {} bytes, expected {} for dim {dim}",
                blob.len(),
                dim * 4
            );
            rowids.push(rowid);
            data.extend(
                blob.as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| f32::from_le_bytes(*c)),
            );
        }
        let index = Arc::new(VecIndex { rowids, dim, data });
        self.vec_index
            .lock()
            .expect("vec_index poisoned")
            .insert(table_name.to_string(), Arc::clone(&index));
        Ok(index)
    }
}

#[async_trait]
impl DocumentStore for SqliteVecStore {
    async fn ensure_collection(
        &self,
        collection: &str,
        source: &str,
        owner_id: &OwnerId,
        expires_at: Option<DateTime<FixedOffset>>,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO documents_meta(collection, owner_id, source, expires_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(collection, owner_id) DO UPDATE SET
                 source = excluded.source, expires_at = excluded.expires_at",
            params![collection, owner_id, source, to_sqlite_utc(expires_at)],
        )?;
        Ok(())
    }

    async fn delete_collection(
        &self,
        collection: &str,
        owner_id: &OwnerId,
    ) -> anyhow::Result<bool> {
        let conn = self.conn.lock().await;
        let table_name: Option<String> = conn
            .query_row(
                "SELECT table_name FROM collections WHERE collection = ? AND owner_id = ?",
                params![collection, owner_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(name) = table_name {
            conn.execute_batch(&format!("DROP TABLE IF EXISTS {name}"))?;
            self.vec_index
                .lock()
                .expect("vec_index poisoned")
                .remove(&name);
        }
        let n = conn.execute(
            "DELETE FROM documents_meta WHERE collection = ? AND owner_id = ?",
            params![collection, owner_id],
        )?;
        Ok(n > 0)
    }

    async fn purge_owner(&self, owner_id: &OwnerId) -> anyhow::Result<usize> {
        let conn = self.conn.lock().await;
        let tables: Vec<String> = {
            let mut stmt = conn.prepare("SELECT table_name FROM collections WHERE owner_id = ?")?;
            let rows = stmt.query_map(params![owner_id], |r| r.get(0))?;
            rows.collect::<Result<_, _>>()?
        };
        for name in &tables {
            conn.execute_batch(&format!("DROP TABLE IF EXISTS {name}"))?;
        }
        let mut cache = self.vec_index.lock().expect("vec_index poisoned");
        for name in &tables {
            cache.remove(name);
        }
        drop(cache);
        let n = conn.execute(
            "DELETE FROM documents_meta WHERE owner_id = ?",
            params![owner_id],
        )?;
        Ok(n)
    }

    async fn reap_expired(&self) -> anyhow::Result<usize> {
        let conn = self.conn.lock().await;
        let tables: Vec<String> = {
            let mut stmt = conn.prepare(
                "SELECT c.table_name FROM collections c
                 JOIN documents_meta m ON m.collection = c.collection AND m.owner_id = c.owner_id
                 WHERE m.expires_at IS NOT NULL AND m.expires_at < datetime('now')",
            )?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            rows.collect::<Result<_, _>>()?
        };
        for name in &tables {
            conn.execute_batch(&format!("DROP TABLE IF EXISTS {name}"))?;
        }
        let mut cache = self.vec_index.lock().expect("vec_index poisoned");
        for name in &tables {
            cache.remove(name);
        }
        drop(cache);
        let n = conn.execute(
            "DELETE FROM documents_meta WHERE expires_at IS NOT NULL AND expires_at < datetime('now')",
            [],
        )?;
        Ok(n)
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
        let conn = self.conn.lock().await;
        let dim = embeddings[0].len();
        let row: Option<(i64, String)> = conn
            .query_row(
                "SELECT dim, table_name FROM collections WHERE collection = ? AND owner_id = ?",
                params![collection, owner_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let table_name = match row {
            None => {
                let name = sanitize_table_name(collection, owner_id);
                conn.execute_batch(&format!(
                    "CREATE TABLE IF NOT EXISTS {name}(embedding BLOB NOT NULL)"
                ))?;
                conn.execute(
                    "INSERT INTO collections(collection, owner_id, dim, table_name) VALUES (?, ?, ?, ?)",
                    params![collection, owner_id, dim as i64, name],
                )?;
                name
            }
            Some((existing_dim, name)) => {
                anyhow::ensure!(
                    existing_dim as usize == dim,
                    "Collection {collection} for owner {owner_id} has dim {existing_dim}, got embedding of dim {dim}"
                );
                name
            }
        };

        // Upsert: remove existing rows for matching IDs first.
        let ids: Vec<&str> = documents.iter().map(|d| d.id.as_str()).collect();
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let existing: Vec<i64> = {
            let mut stmt = conn.prepare(&format!(
                "SELECT vec_rowid FROM documents WHERE collection = ? AND owner_id = ? AND id IN ({placeholders})"
            ))?;
            let mut params: Vec<&dyn rusqlite::ToSql> = vec![&collection, &owner_id];
            params.extend(ids.iter().map(|i| i as &dyn rusqlite::ToSql));
            let rows = stmt.query_map(params.as_slice(), |r| r.get(0))?;
            rows.collect::<Result<_, _>>()?
        };
        if !existing.is_empty() {
            let row_ph = existing.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let mut del_stmt = conn.prepare(&format!(
                "DELETE FROM {table_name} WHERE rowid IN ({row_ph})"
            ))?;
            del_stmt.execute(rusqlite::params_from_iter(existing.iter()))?;
            let mut del_stmt = conn.prepare(&format!(
                "DELETE FROM documents WHERE collection = ? AND owner_id = ? AND id IN ({placeholders})"
            ))?;
            let mut params: Vec<&dyn rusqlite::ToSql> = vec![&collection, &owner_id];
            params.extend(ids.iter().map(|i| i as &dyn rusqlite::ToSql));
            del_stmt.execute(params.as_slice())?;
        }

        let tx = conn.unchecked_transaction()?;
        for (doc, emb) in documents.iter().zip(embeddings.iter()) {
            let normalized = normalize(emb);
            tx.execute(
                &format!("INSERT INTO {table_name}(embedding) VALUES (?)"),
                params![serialize_f32(&normalized)],
            )?;
            let vec_rowid = tx.last_insert_rowid();
            let metadata = serde_json::to_string(&doc.metadata)?;
            tx.execute(
                "INSERT INTO documents(id, collection, owner_id, text, metadata, vec_rowid)
                 VALUES (?, ?, ?, ?, ?, ?)",
                params![doc.id, collection, owner_id, doc.text, metadata, vec_rowid],
            )?;
        }
        tx.commit()?;
        self.vec_index
            .lock()
            .expect("vec_index poisoned")
            .remove(&table_name);
        Ok(())
    }

    async fn add_pages(
        &self,
        collection: &str,
        pages: &[StoredPage],
        owner_id: &OwnerId,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "DELETE FROM document_pages WHERE collection = ? AND owner_id = ?",
            params![collection, owner_id],
        )?;
        for p in pages {
            conn.execute(
                "INSERT INTO document_pages(collection, owner_id, page_number, text, char_count)
                 VALUES (?, ?, ?, ?, ?)",
                params![collection, owner_id, p.page_number, p.text, p.char_count],
            )?;
        }
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
        let conn = self.conn.lock().await;
        for p in principals {
            conn.execute(
                "INSERT INTO document_acl(collection, owner_id, principal_id, permission)
                 VALUES (?, ?, ?, ?)
                 ON CONFLICT(collection, owner_id, principal_id, permission) DO NOTHING",
                params![collection, owner_id, p, READ_PERMISSION],
            )?;
        }
        Ok(())
    }

    async fn revoke(
        &self,
        collection: &str,
        owner_id: &OwnerId,
        principal: &PrincipalId,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "DELETE FROM document_acl WHERE collection = ? AND owner_id = ? AND principal_id = ?",
            params![collection, owner_id, principal],
        )?;
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
        let conn = self.conn.lock().await;
        let Some(owner_id) = Self::readable_owner_for(&conn, collection, principals)? else {
            return Ok(Vec::new());
        };
        let row: Option<(String, i64)> = conn
            .query_row(
                "SELECT table_name, dim FROM collections WHERE collection = ? AND owner_id = ?",
                params![collection, owner_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((table_name, dim)) = row else {
            return Ok(Vec::new());
        };
        anyhow::ensure!(
            query_embedding.len() == dim as usize,
            "Query embedding dim {} does not match collection dim {}",
            query_embedding.len(),
            dim
        );
        let normalized = normalize(query_embedding);
        let index = self.load_index(&conn, &table_name, dim as usize)?;
        let n = index.rowids.len();
        let k = top_k.min(n);
        if k == 0 {
            return Ok(Vec::new());
        }
        let dim = index.dim;
        // Stored and query vectors are normalized, so the dot product is the
        // cosine similarity, clamped at 0 like the previous MATCH query.
        // Chunk rows per worker: one rayon task per row would drown the scan
        // in scheduling overhead.
        let chunk_rows = n.div_ceil(rayon::current_num_threads() * 16).max(1);
        let mut scores = vec![0.0f32; n];
        scores
            .par_chunks_mut(chunk_rows)
            .enumerate()
            .for_each(|(chunk_idx, out)| {
                let start = chunk_idx * chunk_rows;
                for (off, slot) in out.iter_mut().enumerate() {
                    let row = &index.data[(start + off) * dim..(start + off + 1) * dim];
                    *slot = dot(row, &normalized).max(0.0);
                }
            });
        let mut order: Vec<usize> = (0..n).collect();
        order.select_nth_unstable_by(k - 1, |&a, &b| {
            scores[b]
                .total_cmp(&scores[a])
                .then(index.rowids[a].cmp(&index.rowids[b]))
        });
        let mut top: Vec<(i64, f32)> = order[..k]
            .iter()
            .map(|&i| (index.rowids[i], scores[i]))
            .collect();
        top.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));

        // Hydrate only the winning rows; `documents` can be wide, so avoid
        // pulling every text/metadata blob through the scan.
        let mut by_rowid: HashMap<i64, (String, String, String)> = HashMap::new();
        let rowids: Vec<i64> = top.iter().map(|(r, _)| *r).collect();
        for chunk in rowids.chunks(DOC_FETCH_CHUNK) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let mut stmt = conn.prepare(&format!(
                "SELECT vec_rowid, id, text, metadata FROM documents
                 WHERE collection = ? AND owner_id = ? AND vec_rowid IN ({placeholders})"
            ))?;
            let mut sql_params: Vec<&dyn ToSql> = vec![&collection, &owner_id];
            sql_params.extend(chunk.iter().map(|r| r as &dyn ToSql));
            let rows = stmt.query_map(sql_params.as_slice(), |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?;
            for row in rows {
                let (vec_rowid, id, text, metadata) = row?;
                by_rowid.insert(vec_rowid, (id, text, metadata));
            }
        }
        let mut out = Vec::with_capacity(top.len());
        for (rowid, score) in top {
            if let Some((id, text, metadata)) = by_rowid.remove(&rowid) {
                let metadata: HashMap<String, String> =
                    serde_json::from_str(&metadata).unwrap_or_default();
                out.push(SearchResult {
                    document: Document { id, text, metadata },
                    score: f64::from(score),
                });
            }
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
        let conn = self.conn.lock().await;
        let Some(owner_id) = Self::readable_owner_for(&conn, collection, principals)? else {
            return Ok(Vec::new());
        };
        let rows: Vec<(i64, String, i64)> = match page_range {
            None => {
                let mut stmt = conn.prepare(
                    "SELECT page_number, text, char_count FROM document_pages
                     WHERE collection = ? AND owner_id = ? ORDER BY page_number",
                )?;
                let rows = stmt.query_map(params![collection, owner_id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })?;
                rows.collect::<Result<_, _>>()?
            }
            Some(range) => {
                let mut stmt = conn.prepare(
                    "SELECT page_number, text, char_count FROM document_pages
                     WHERE collection = ? AND owner_id = ? AND page_number BETWEEN ? AND ?
                     ORDER BY page_number",
                )?;
                let rows = stmt
                    .query_map(params![collection, owner_id, range.start, range.end], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                    })?;
                rows.collect::<Result<_, _>>()?
            }
        };
        Ok(rows
            .into_iter()
            .map(|(page_number, text, char_count)| Page {
                page_number,
                text,
                char_count,
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
        let conn = self.conn.lock().await;
        Ok(Self::readable_owner_for(&conn, collection, principals)?.is_some())
    }

    async fn list_collections(&self, principals: &[PrincipalId]) -> anyhow::Result<Vec<String>> {
        if principals.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().await;
        let placeholders = principals.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT DISTINCT collection FROM document_acl
             WHERE permission = ? AND principal_id IN ({placeholders})
             ORDER BY collection"
        );
        let mut params: Vec<&dyn rusqlite::ToSql> = vec![&READ_PERMISSION];
        params.extend(principals.iter().map(|p| p as &dyn rusqlite::ToSql));
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params.as_slice(), |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    async fn close(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock().await;
        if let Some(path) = &self.db_path {
            let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
            drop(conn);
            // Connection closes when the Mutex guard drops; nothing else to do.
            let _ = path;
        }
        Ok(())
    }
}

/// Helper kept for parity with Python's datetime handling in tests.
#[allow(dead_code)]
fn _parse_sqlite_dt(s: &str) -> Option<DateTime<Utc>> {
    NaiveDateTime::parse_from_str(s, SQLITE_DATETIME_FMT)
        .ok()
        .map(|n| Utc.from_utc_datetime(&n))
}
