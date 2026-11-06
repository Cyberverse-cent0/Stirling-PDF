//! Correctness, migration, cache and concurrency tests for `SqliteVecStore`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::DateTime;
use rusqlite::{params, Connection};
use stirling_engine::documents::sqlite_vec_store::SqliteVecStore;
use stirling_engine::documents::store::{Document, DocumentStore, StoredPage};

fn doc(id: &str, text: &str) -> Document {
    Document {
        id: id.to_string(),
        text: text.to_string(),
        metadata: Default::default(),
    }
}

fn temp_db(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("vecstore-audit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(format!("{tag}-{nanos}.db"))
}

fn f32le(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Reference cosine similarity in f64, matching the documented score
/// semantics `max(0, cosine)` for normalized vectors.
fn reference_cosine(a: &[f32], b: &[f32]) -> f64 {
    let dot: f64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| f64::from(*x) * f64::from(*y))
        .sum();
    let na: f64 = a
        .iter()
        .map(|x| f64::from(*x) * f64::from(*x))
        .sum::<f64>()
        .sqrt();
    let nb: f64 = b
        .iter()
        .map(|x| f64::from(*x) * f64::from(*x))
        .sum::<f64>()
        .sqrt();
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    (dot / (na * nb)).max(0.0)
}

#[tokio::test]
async fn ranking_matches_reference_cosine() {
    let store = SqliteVecStore::ephemeral().unwrap();
    let owner = "u".to_string();
    let principal = "u".to_string();
    store
        .ensure_collection("c", "s", &owner, None)
        .await
        .unwrap();
    // d5 is unnormalized on purpose: the store must normalize at write time.
    let docs = vec![
        doc("d1", "t"),
        doc("d2", "t"),
        doc("d3", "t"),
        doc("d4", "t"),
        doc("d5", "t"),
    ];
    let embeddings = vec![
        vec![1.0, 0.0, 0.0, 0.0],
        vec![1.0, 1.0, 0.0, 0.0],
        vec![0.0, 1.0, 0.0, 0.0],
        vec![-1.0, 0.0, 0.0, 0.0],
        vec![3.0, 4.0, 0.0, 0.0],
    ];
    store
        .add_documents("c", &docs, &embeddings, &owner)
        .await
        .unwrap();
    store
        .grant_read("c", &owner, std::slice::from_ref(&principal))
        .await
        .unwrap();

    let query = vec![1.0, 0.0, 0.0, 0.0];
    let results = store
        .search("c", &query, 5, std::slice::from_ref(&principal))
        .await
        .unwrap();
    assert_eq!(results.len(), 5);

    let mut expected: Vec<(&str, f64)> = docs
        .iter()
        .zip(&embeddings)
        .map(|(d, e)| (d.id.as_str(), reference_cosine(&query, e)))
        .collect();
    expected.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(b.0)));
    for (got, (want_id, want_score)) in results.iter().zip(&expected) {
        assert_eq!(&got.document.id, want_id);
        assert!(
            (got.score - want_score).abs() < 1e-6,
            "score for {}: got {}, want {}",
            want_id,
            got.score,
            want_score
        );
    }
}

#[tokio::test]
async fn zero_vectors_do_not_produce_nan() {
    let store = SqliteVecStore::ephemeral().unwrap();
    let owner = "u".to_string();
    let principal = "u".to_string();
    store
        .ensure_collection("c", "s", &owner, None)
        .await
        .unwrap();
    store
        .add_documents(
            "c",
            &[doc("zero", "t"), doc("one", "t")],
            &[vec![0.0, 0.0], vec![1.0, 0.0]],
            &owner,
        )
        .await
        .unwrap();
    store
        .grant_read("c", &owner, std::slice::from_ref(&principal))
        .await
        .unwrap();

    // Zero query vector: every score is 0, order falls back to rowid.
    let results = store
        .search("c", &[0.0, 0.0], 2, std::slice::from_ref(&principal))
        .await
        .unwrap();
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|r| r.score == 0.0));
    assert!(results.iter().all(|r| r.score.is_finite()));
    // Deterministic across repeats.
    let again = store
        .search("c", &[0.0, 0.0], 2, std::slice::from_ref(&principal))
        .await
        .unwrap();
    assert_eq!(
        results.iter().map(|r| &r.document.id).collect::<Vec<_>>(),
        again.iter().map(|r| &r.document.id).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn inconsistent_batch_dims_are_rejected() {
    let store = SqliteVecStore::ephemeral().unwrap();
    let owner = "u".to_string();
    store
        .ensure_collection("c", "s", &owner, None)
        .await
        .unwrap();
    let err = store
        .add_documents(
            "c",
            &[doc("a", "t"), doc("b", "t")],
            &[vec![1.0, 0.0], vec![1.0, 0.0, 0.0]],
            &owner,
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("Inconsistent embedding dims"));
    let err = store
        .add_documents("c", &[doc("a", "t")], &[vec![]], &owner)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("at least one dimension"));
}

#[tokio::test]
async fn empty_collection_and_top_k_edge_cases() {
    let store = SqliteVecStore::ephemeral().unwrap();
    let owner = "u".to_string();
    let principal = "u".to_string();
    store
        .ensure_collection("c", "s", &owner, None)
        .await
        .unwrap();
    store
        .grant_read("c", &owner, std::slice::from_ref(&principal))
        .await
        .unwrap();
    assert!(store
        .search("c", &[1.0], 5, std::slice::from_ref(&principal))
        .await
        .unwrap()
        .is_empty());
    // top_k = 0 and top_k > n are both safe once documents exist.
    store
        .add_documents("c", &[doc("a", "t")], &[vec![1.0]], &owner)
        .await
        .unwrap();
    assert!(store
        .search("c", &[1.0], 0, std::slice::from_ref(&principal))
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .search("c", &[1.0], 99, std::slice::from_ref(&principal))
            .await
            .unwrap()
            .len(),
        1
    );
    // Empty ingest is a no-op.
    store.add_documents("c", &[], &[], &owner).await.unwrap();
}

/// Builds a database in the pre-migration shape: same metadata tables plus
/// `vec0` virtual tables (simulated — sqlite_vec is not loadable here, so the
/// sqlite_master SQL is rewritten with writable_schema).
fn legacy_db(path: &PathBuf) -> Connection {
    let _ = std::fs::remove_file(path);
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "PRAGMA foreign_keys=ON;
         CREATE TABLE documents_meta (
            collection TEXT NOT NULL,
            owner_id TEXT NOT NULL,
            source TEXT NOT NULL,
            expires_at TIMESTAMP,
            PRIMARY KEY (collection, owner_id)
         );
         CREATE TABLE collections (
            collection TEXT NOT NULL,
            owner_id TEXT NOT NULL,
            dim INTEGER NOT NULL,
            table_name TEXT NOT NULL,
            PRIMARY KEY (collection, owner_id),
            FOREIGN KEY (collection, owner_id)
                REFERENCES documents_meta(collection, owner_id) ON DELETE CASCADE
         );
         CREATE TABLE documents (
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
         CREATE TABLE document_pages (
            collection TEXT NOT NULL,
            owner_id TEXT NOT NULL,
            page_number INTEGER NOT NULL,
            text TEXT NOT NULL,
            char_count INTEGER NOT NULL,
            PRIMARY KEY (collection, owner_id, page_number),
            FOREIGN KEY (collection, owner_id)
                REFERENCES documents_meta(collection, owner_id) ON DELETE CASCADE
         );
         CREATE TABLE document_acl (
            collection TEXT NOT NULL,
            owner_id TEXT NOT NULL,
            principal_id TEXT NOT NULL,
            permission TEXT NOT NULL,
            PRIMARY KEY (collection, owner_id, principal_id, permission),
            FOREIGN KEY (collection, owner_id)
                REFERENCES documents_meta(collection, owner_id) ON DELETE CASCADE
         );
         CREATE TABLE unrelated_data (k TEXT PRIMARY KEY, v TEXT);
         INSERT INTO unrelated_data VALUES ('keep', 'me');",
    )
    .unwrap();
    for (collection, owner) in [("legacy", "owner1"), ("other", "owner2")] {
        let table = format!("vec_{owner}_{collection}");
        conn.execute_batch(&format!(
            "INSERT INTO documents_meta(collection, owner_id, source) VALUES ('{collection}', '{owner}', 'test');
             INSERT INTO collections(collection, owner_id, dim, table_name) VALUES ('{collection}', '{owner}', 3, '{table}');
             INSERT INTO documents(id, collection, owner_id, text, vec_rowid) VALUES ('d1', '{collection}', '{owner}', 'chunk', 1);
             INSERT INTO document_pages(collection, owner_id, page_number, text, char_count) VALUES ('{collection}', '{owner}', 1, 'page', 4);
             INSERT INTO document_acl(collection, owner_id, principal_id, permission) VALUES ('{collection}', '{owner}', '{owner}', 'read');
             CREATE TABLE {table}(embedding BLOB NOT NULL);
             CREATE TABLE {table}_chunks(dummy BLOB);
             INSERT INTO {table}_chunks VALUES (x'00');",
        ))
        .unwrap();
        let mut stmt = conn
            .prepare(&format!("INSERT INTO {table}(embedding) VALUES (?)"))
            .unwrap();
        stmt.execute(params![f32le(&[1.0, 0.0, 0.0])]).unwrap();
        stmt.execute(params![f32le(&[0.0, 1.0, 0.0])]).unwrap();
    }
    // Rewrite the vec table DDL so it looks like a vec0 virtual table.
    conn.execute_batch("PRAGMA writable_schema=ON").unwrap();
    for (collection, owner) in [("legacy", "owner1"), ("other", "owner2")] {
        let table = format!("vec_{owner}_{collection}");
        conn.execute(
            "UPDATE sqlite_master SET sql = ? WHERE type = 'table' AND name = ?",
            params![
                format!("CREATE VIRTUAL TABLE {table} USING vec0(embedding float[3])"),
                table
            ],
        )
        .unwrap();
    }
    conn.execute_batch("PRAGMA writable_schema=OFF").unwrap();
    conn
}

#[tokio::test]
async fn legacy_vec0_tables_are_dropped_and_metadata_preserved() {
    let path = temp_db("legacy");
    legacy_db(&path);

    let store = SqliteVecStore::open(&path).unwrap();

    // Verify through a second connection: vec tables gone, derived rows gone,
    // metadata/pages/ACL/unrelated data preserved.
    let check = Connection::open(&path).unwrap();
    check
        .busy_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    let vec_tables: i64 = check
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name LIKE 'vec_%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(vec_tables, 0, "vec tables must be dropped");
    for table in ["collections", "documents"] {
        let n: i64 = check
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            n, 0,
            "{table} rows for migrated collections must be deleted"
        );
    }
    for (table, want) in [
        ("documents_meta", 2),
        ("document_pages", 2),
        ("document_acl", 2),
    ] {
        let n: i64 = check
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, want, "{table} must be preserved");
    }
    let keep: String = check
        .query_row("SELECT v FROM unrelated_data WHERE k = 'keep'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(keep, "me");
    drop(check);

    // Migrated collections are gone for reads but re-ingest recreates them.
    let principal = "owner1".to_string();
    assert!(store
        .search(
            "legacy",
            &[1.0, 0.0, 0.0],
            5,
            std::slice::from_ref(&principal)
        )
        .await
        .unwrap()
        .is_empty());
    store
        .add_documents(
            "legacy",
            &[doc("new", "chunk")],
            &[vec![1.0, 0.0, 0.0]],
            &"owner1".to_string(),
        )
        .await
        .unwrap();
    let results = store
        .search(
            "legacy",
            &[1.0, 0.0, 0.0],
            5,
            std::slice::from_ref(&principal),
        )
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].document.id, "new");

    // Re-opening is idempotent: the recreated table is a plain table, not vec0.
    let store2 = SqliteVecStore::open(&path).unwrap();
    let principal = "owner1".to_string();
    assert_eq!(
        store2
            .search(
                "legacy",
                &[1.0, 0.0, 0.0],
                5,
                std::slice::from_ref(&principal)
            )
            .await
            .unwrap()
            .len(),
        1
    );
    let _ = std::fs::remove_file(&path);
}

/// A `collections` row whose vector table was already dropped (the failure
/// mode of the pre-transactional migration) must not break ingest or leak
/// stale rowids into results.
#[tokio::test]
async fn partial_migration_state_recovers_on_ingest() {
    let path = temp_db("partial");
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE documents_meta (collection TEXT NOT NULL, owner_id TEXT NOT NULL,
                 source TEXT NOT NULL, expires_at TIMESTAMP,
                 PRIMARY KEY (collection, owner_id));
             CREATE TABLE collections (collection TEXT NOT NULL, owner_id TEXT NOT NULL,
                 dim INTEGER NOT NULL, table_name TEXT NOT NULL,
                 PRIMARY KEY (collection, owner_id),
                 FOREIGN KEY (collection, owner_id)
                     REFERENCES documents_meta(collection, owner_id) ON DELETE CASCADE);
             CREATE TABLE documents (id TEXT NOT NULL, collection TEXT NOT NULL,
                 owner_id TEXT NOT NULL, text TEXT NOT NULL, metadata TEXT NOT NULL DEFAULT '{}',
                 vec_rowid INTEGER NOT NULL,
                 PRIMARY KEY (id, collection, owner_id),
                 FOREIGN KEY (collection, owner_id)
                     REFERENCES documents_meta(collection, owner_id) ON DELETE CASCADE);
             CREATE TABLE document_acl (collection TEXT NOT NULL, owner_id TEXT NOT NULL,
                 principal_id TEXT NOT NULL, permission TEXT NOT NULL,
                 PRIMARY KEY (collection, owner_id, principal_id, permission),
                 FOREIGN KEY (collection, owner_id)
                     REFERENCES documents_meta(collection, owner_id) ON DELETE CASCADE);
             INSERT INTO documents_meta VALUES ('c', 'u', 's', NULL);
             INSERT INTO collections VALUES ('c', 'u', 2, 'vec_u_c');
             INSERT INTO documents VALUES ('ghost', 'c', 'u', 'stale', '{}', 1);
             INSERT INTO document_acl VALUES ('c', 'u', 'u', 'read');",
        )
        .unwrap();
    }

    let store = SqliteVecStore::open(&path).unwrap();
    let principal = "u".to_string();
    // Search degrades to empty rather than erroring on the missing table.
    assert!(store
        .search("c", &[1.0, 0.0], 5, std::slice::from_ref(&principal))
        .await
        .unwrap()
        .is_empty());

    store
        .add_documents(
            "c",
            &[doc("fresh", "new")],
            &[vec![1.0, 0.0]],
            &"u".to_string(),
        )
        .await
        .unwrap();
    let results = store
        .search("c", &[1.0, 0.0], 5, std::slice::from_ref(&principal))
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].document.id, "fresh");
    assert_eq!(results[0].document.text, "new");
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn cache_is_invalidated_by_every_mutation_path() {
    let store = SqliteVecStore::ephemeral().unwrap();
    let owner = "u".to_string();
    let principal = "u".to_string();
    store
        .ensure_collection("c", "s", &owner, None)
        .await
        .unwrap();
    store
        .add_documents("c", &[doc("a", "v1")], &[vec![1.0, 0.0]], &owner)
        .await
        .unwrap();
    store
        .grant_read("c", &owner, std::slice::from_ref(&principal))
        .await
        .unwrap();

    // Populate the cache.
    assert_eq!(
        store
            .search("c", &[1.0, 0.0], 5, std::slice::from_ref(&principal))
            .await
            .unwrap()
            .len(),
        1
    );
    // Write invalidates: the new vector must be visible immediately.
    store
        .add_documents("c", &[doc("b", "v2")], &[vec![0.0, 1.0]], &owner)
        .await
        .unwrap();
    assert_eq!(
        store
            .search("c", &[0.0, 1.0], 5, std::slice::from_ref(&principal))
            .await
            .unwrap()[0]
            .document
            .id,
        "b"
    );
    // Upsert of an existing id invalidates too.
    store
        .add_documents("c", &[doc("b", "v2b")], &[vec![1.0, 0.0]], &owner)
        .await
        .unwrap();
    let results = store
        .search("c", &[1.0, 0.0], 5, std::slice::from_ref(&principal))
        .await
        .unwrap();
    assert_eq!(results.len(), 2);
    assert!(results.iter().any(|r| r.document.text == "v2b"));

    // delete_collection drops the cached index.
    assert!(store.delete_collection("c", &owner).await.unwrap());
    assert!(store
        .search("c", &[1.0, 0.0], 5, std::slice::from_ref(&principal))
        .await
        .unwrap()
        .is_empty());

    // purge_owner removes only that owner's collections.
    let owner_b = "v".to_string();
    let principal_b = "v".to_string();
    for (o, p) in [(&owner, &principal), (&owner_b, &principal_b)] {
        store.ensure_collection("c", "s", o, None).await.unwrap();
        store
            .add_documents("c", &[doc("x", "t")], &[vec![1.0, 0.0]], o)
            .await
            .unwrap();
        store
            .grant_read("c", o, std::slice::from_ref(p))
            .await
            .unwrap();
    }
    assert_eq!(store.purge_owner(&owner).await.unwrap(), 1);
    assert!(store
        .search("c", &[1.0, 0.0], 5, std::slice::from_ref(&principal))
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .search("c", &[1.0, 0.0], 5, std::slice::from_ref(&principal_b))
            .await
            .unwrap()
            .len(),
        1
    );

    // reap_expired drops expired collections from the cache as well.
    let past = DateTime::parse_from_rfc3339("2020-01-01T00:00:00+00:00").unwrap();
    let owner_c: String = "w".to_string();
    let principal_c = "w".to_string();
    store
        .ensure_collection("exp", "s", &owner_c, Some(past))
        .await
        .unwrap();
    store
        .add_documents("exp", &[doc("e", "t")], &[vec![1.0, 0.0]], &owner_c)
        .await
        .unwrap();
    store
        .grant_read("exp", &owner_c, std::slice::from_ref(&principal_c))
        .await
        .unwrap();
    assert_eq!(
        store
            .search("exp", &[1.0, 0.0], 5, std::slice::from_ref(&principal_c))
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(store.reap_expired().await.unwrap(), 1);
    assert!(store
        .search("exp", &[1.0, 0.0], 5, std::slice::from_ref(&principal_c))
        .await
        .unwrap()
        .is_empty());
}

/// "u-1" and "u_1" sanitize to the same table name; they must not share a
/// vector table or each owner's search would score the other's vectors.
#[tokio::test]
async fn sanitized_name_collisions_stay_isolated() {
    let store = SqliteVecStore::ephemeral().unwrap();
    for (owner, vec) in [("u-1", vec![1.0, 0.0]), ("u_1", vec![0.0, 1.0])] {
        store
            .ensure_collection("c", "s", &owner.to_string(), None)
            .await
            .unwrap();
        store
            .add_documents("c", &[doc("d", "t")], &[vec], &owner.to_string())
            .await
            .unwrap();
        store
            .grant_read("c", &owner.to_string(), &[owner.to_string()])
            .await
            .unwrap();
    }
    let r1 = store
        .search("c", &[1.0, 0.0], 5, &["u-1".to_string()])
        .await
        .unwrap();
    assert_eq!(r1.len(), 1);
    assert_eq!(r1[0].score, 1.0);
    let r2 = store
        .search("c", &[0.0, 1.0], 5, &["u_1".to_string()])
        .await
        .unwrap();
    assert_eq!(r2.len(), 1);
    assert_eq!(r2[0].score, 1.0);
}

#[tokio::test]
async fn concurrent_searches_and_writes_stay_consistent() {
    let store = Arc::new(SqliteVecStore::ephemeral().unwrap());
    let owner = "u".to_string();
    store
        .ensure_collection("c", "s", &owner, None)
        .await
        .unwrap();
    store
        .grant_read("c", &owner, std::slice::from_ref(&owner))
        .await
        .unwrap();

    let mut handles = Vec::new();
    for w in 0..4 {
        let store = Arc::clone(&store);
        let owner = owner.clone();
        handles.push(tokio::spawn(async move {
            for i in 0..25 {
                let id = format!("w{w}-{i}");
                store
                    .add_documents(
                        "c",
                        &[doc(&id, "t")],
                        &[vec![1.0, (i % 7) as f32, 0.0]],
                        &owner,
                    )
                    .await
                    .unwrap();
            }
        }));
    }
    for _ in 0..8 {
        let store = Arc::clone(&store);
        let owner = owner.clone();
        handles.push(tokio::spawn(async move {
            for _ in 0..30 {
                let _ = store
                    .search("c", &[1.0, 0.5, 0.0], 10, std::slice::from_ref(&owner))
                    .await
                    .unwrap();
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    let results = store
        .search("c", &[1.0, 0.0, 0.0], 200, &[owner])
        .await
        .unwrap();
    assert_eq!(results.len(), 100);
    // Every returned document must hydrate to a real row (no stale rowids).
    let texts: std::collections::HashSet<_> =
        results.iter().map(|r| r.document.id.clone()).collect();
    assert_eq!(texts.len(), 100);
}

/// The decoded-index cache is budgeted: entries beyond it are evicted LRU
/// and transparently rebuilt on the next search, and a single collection
/// larger than the whole budget remains usable.
#[tokio::test]
async fn cache_budget_evicts_lru_and_preserves_correctness() {
    // Each index below is 8 docs x dim 8 = 64 B rowids + 256 B data = 320 B,
    // so a 700 B budget holds exactly two of the three collections.
    let store = SqliteVecStore::ephemeral_with_budget(700).unwrap();
    let owner = "u".to_string();
    let principal = "u".to_string();
    for (i, col) in ["c1", "c2", "c3"].iter().enumerate() {
        store
            .ensure_collection(col, "s", &owner, None)
            .await
            .unwrap();
        let docs: Vec<Document> = (0..8).map(|d| doc(&format!("{col}-{d}"), "t")).collect();
        // One-hot-ish vectors so each collection ranks its own docs first.
        let embeddings: Vec<Vec<f32>> = (0..8)
            .map(|d| {
                let mut v = vec![0.0f32; 8];
                v[(i + d) % 8] = 1.0;
                v
            })
            .collect();
        store
            .add_documents(col, &docs, &embeddings, &owner)
            .await
            .unwrap();
        store
            .grant_read(col, &owner, std::slice::from_ref(&principal))
            .await
            .unwrap();
    }
    // Touch c1, c2, c3 in order: c1 is evicted once c3 lands.
    for col in ["c1", "c2", "c3", "c1", "c2"] {
        let results = store
            .search(
                col,
                &[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                3,
                std::slice::from_ref(&principal),
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 3, "{col} must stay searchable");
        assert!(
            results.iter().all(|r| r.document.id.starts_with(col)),
            "{col} returned foreign documents: {:?}",
            results.iter().map(|r| &r.document.id).collect::<Vec<_>>()
        );
    }

    // A collection bigger than the entire budget is still served (the cache
    // keeps the newest entry rather than thrashing).
    let tiny = SqliteVecStore::ephemeral_with_budget(10).unwrap();
    tiny.ensure_collection("big", "s", &owner, None)
        .await
        .unwrap();
    let docs: Vec<Document> = (0..4).map(|d| doc(&format!("b{d}"), "t")).collect();
    let embeddings: Vec<Vec<f32>> = (0..4).map(|d| vec![d as f32, 1.0]).collect();
    tiny.add_documents("big", &docs, &embeddings, &owner)
        .await
        .unwrap();
    tiny.grant_read("big", &owner, std::slice::from_ref(&principal))
        .await
        .unwrap();
    for _ in 0..2 {
        let results = tiny
            .search("big", &[1.0, 0.0], 2, std::slice::from_ref(&principal))
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
    }
}

/// Cached searches may run without the connection lock; concurrent
/// identical searches must still return identical, deterministic results.
#[tokio::test]
async fn concurrent_cached_searches_are_deterministic() {
    let store = Arc::new(SqliteVecStore::ephemeral().unwrap());
    let owner = "u".to_string();
    store
        .ensure_collection("c", "s", &owner, None)
        .await
        .unwrap();
    let docs: Vec<Document> = (0..50).map(|i| doc(&format!("d{i}"), "t")).collect();
    let embeddings: Vec<Vec<f32>> = (0..50)
        .map(|i| vec![((i * 37) % 11) as f32, (i % 5) as f32, 0.25])
        .collect();
    store
        .add_documents("c", &docs, &embeddings, &owner)
        .await
        .unwrap();
    store
        .grant_read("c", &owner, std::slice::from_ref(&owner))
        .await
        .unwrap();
    // Warm the cache, then fan out identical searches.
    let expected = store
        .search("c", &[1.0, 0.5, 0.0], 10, std::slice::from_ref(&owner))
        .await
        .unwrap();
    assert_eq!(expected.len(), 10);
    let mut handles = Vec::new();
    for _ in 0..8 {
        let s = Arc::clone(&store);
        let o = owner.clone();
        handles.push(tokio::spawn(async move {
            s.search("c", &[1.0, 0.5, 0.0], 10, std::slice::from_ref(&o))
                .await
                .unwrap()
                .iter()
                .map(|r| (r.document.id.clone(), r.score.to_bits()))
                .collect::<Vec<_>>()
        }));
    }
    for h in handles {
        let got = h.await.unwrap();
        let want: Vec<_> = expected
            .iter()
            .map(|r| (r.document.id.clone(), r.score.to_bits()))
            .collect();
        assert_eq!(got, want);
    }
}

#[tokio::test]
async fn pages_roundtrip_and_range_filter() {
    let store = SqliteVecStore::ephemeral().unwrap();
    let owner = "u".to_string();
    let principal = "u".to_string();
    store
        .ensure_collection("c", "s", &owner, None)
        .await
        .unwrap();
    store
        .grant_read("c", &owner, std::slice::from_ref(&principal))
        .await
        .unwrap();
    let pages: Vec<StoredPage> = (1..=5)
        .map(|n| StoredPage {
            page_number: n,
            text: format!("page {n}"),
            char_count: 6,
        })
        .collect();
    store.add_pages("c", &pages, &owner).await.unwrap();
    let all = store
        .read_pages("c", None, std::slice::from_ref(&principal))
        .await
        .unwrap();
    assert_eq!(all.len(), 5);
    let range = stirling_engine::contracts::documents::PageRange { start: 2, end: 4 };
    let subset = store
        .read_pages("c", Some(range), std::slice::from_ref(&principal))
        .await
        .unwrap();
    assert_eq!(subset.len(), 3);
    assert_eq!(subset[0].page_number, 2);
}
