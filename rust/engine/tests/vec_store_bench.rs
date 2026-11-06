//! Release-mode benchmarks for `SqliteVecStore`. Ignored by default:
//!
//! ```bash
//! cargo test --release --test vec_store_bench -- --ignored --nocapture
//! ```
//!
//! Vectors are deterministic LCG noise in [-1, 1); ingest goes through
//! `add_documents` in 1 024-document batches (the same call shape
//! `DocumentService` uses per page batch).

use std::sync::Arc;
use std::time::Instant;

use stirling_engine::documents::sqlite_vec_store::SqliteVecStore;
use stirling_engine::documents::store::{Document, DocumentStore};

const BATCH: usize = 1024;

struct Lcg(u64);
impl Lcg {
    fn next_f32(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
    }
}

fn rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        })
        .unwrap_or(0)
}

async fn bench(n: usize, dim: usize) {
    let store = SqliteVecStore::ephemeral().unwrap();
    let owner = "bench".to_string();
    store
        .ensure_collection("bench", "bench", &owner, None)
        .await
        .unwrap();
    store
        .grant_read("bench", &owner, std::slice::from_ref(&owner))
        .await
        .unwrap();

    let mut rng = Lcg(0x9e3779b97f4a7c15);
    let query: Vec<f32> = (0..dim).map(|_| rng.next_f32()).collect();

    let rss0 = rss_kb();
    let t0 = Instant::now();
    let mut done = 0;
    while done < n {
        let take = BATCH.min(n - done);
        let docs: Vec<Document> = (done..done + take)
            .map(|i| Document {
                id: format!("doc-{i}"),
                text: "lorem ipsum".to_string(),
                metadata: Default::default(),
            })
            .collect();
        let embeddings: Vec<Vec<f32>> = (0..take)
            .map(|_| (0..dim).map(|_| rng.next_f32()).collect())
            .collect();
        store
            .add_documents("bench", &docs, &embeddings, &owner)
            .await
            .unwrap();
        done += take;
    }
    let ingest = t0.elapsed();
    let rss_ingest = rss_kb();

    // First search pays the blob decode + index build.
    let t = Instant::now();
    let first = store
        .search("bench", &query, 10, std::slice::from_ref(&owner))
        .await
        .unwrap();
    let first_search = t.elapsed();
    assert_eq!(first.len(), 10);
    let rss_index = rss_kb();

    // Cached searches.
    let iters = 30;
    let t = Instant::now();
    for _ in 0..iters {
        store
            .search("bench", &query, 10, std::slice::from_ref(&owner))
            .await
            .unwrap();
    }
    let cached = t.elapsed() / iters as u32;

    // Concurrent searches: the connection mutex serializes them, so this
    // measures serialized throughput under contention.
    let store = Arc::new(store);
    let t = Instant::now();
    let mut handles = Vec::new();
    for _ in 0..4 {
        let s = Arc::clone(&store);
        let q = query.clone();
        let o = owner.clone();
        handles.push(tokio::spawn(async move {
            for _ in 0..10 {
                s.search("bench", &q, 10, std::slice::from_ref(&o))
                    .await
                    .unwrap();
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    let concurrent = t.elapsed() / 40;

    // Cache invalidation cost: rewrite the whole collection once.
    let t = Instant::now();
    let docs: Vec<Document> = (0..256)
        .map(|i| Document {
            id: format!("extra-{i}"),
            text: "x".to_string(),
            metadata: Default::default(),
        })
        .collect();
    let embeddings: Vec<Vec<f32>> = (0..256)
        .map(|_| (0..dim).map(|_| rng.next_f32()).collect())
        .collect();
    store
        .add_documents("bench", &docs, &embeddings, &owner)
        .await
        .unwrap();
    let invalidate_plus_write = t.elapsed();

    println!(
        "BENCH n={n} dim={dim} threads={}\n  \
         ingest:            {:>9.2?}  ({:.0} docs/s)\n  \
         first search:      {:>9.2?}\n  \
         cached search:     {:>9.2?}\n  \
         concurrent search: {:>9.2?} (per op, 4 tasks)\n  \
         invalidate+write:  {:>9.2?}\n  \
         RSS: baseline {} MB, post-ingest {} MB, post-index {} MB",
        rayon::current_num_threads(),
        ingest,
        n as f64 / ingest.as_secs_f64(),
        first_search,
        cached,
        concurrent,
        invalidate_plus_write,
        rss0 / 1024,
        rss_ingest / 1024,
        rss_index / 1024,
    );
}

/// Multi-collection cache behavior: budgeted LRU eviction, rebuild cost,
/// and search latency while a writer mutates the same collection.
#[tokio::test]
#[ignore]
async fn bench_cache_budget_and_mixed_workload() {
    const COLS: usize = 4;
    const N: usize = 10_000;
    const DIM: usize = 384;
    // Each index is ~10k*(8+384*4) B ≈ 15.4 MB; a 40 MB budget holds ~2-3.
    let store = Arc::new(SqliteVecStore::ephemeral_with_budget(40 * 1024 * 1024).unwrap());
    let owner = "bench".to_string();
    let mut rng = Lcg(0xdeadbeefcafef00d);

    for c in 0..COLS {
        let col = format!("col{c}");
        store
            .ensure_collection(&col, "bench", &owner, None)
            .await
            .unwrap();
        store
            .grant_read(&col, &owner, std::slice::from_ref(&owner))
            .await
            .unwrap();
        for start in (0..N).step_by(BATCH) {
            let take = BATCH.min(N - start);
            let docs: Vec<Document> = (start..start + take)
                .map(|i| Document {
                    id: format!("{col}-{i}"),
                    text: "x".into(),
                    metadata: Default::default(),
                })
                .collect();
            let embeddings: Vec<Vec<f32>> = (0..take)
                .map(|_| (0..DIM).map(|_| rng.next_f32()).collect())
                .collect();
            store
                .add_documents(&col, &docs, &embeddings, &owner)
                .await
                .unwrap();
        }
    }
    let query: Vec<f32> = (0..DIM).map(|_| rng.next_f32()).collect();

    // Round-robin searches: every pass evicts the LRU entry, so each search
    // after the first few pays a decode+rebuild — the eviction worst case.
    let t = Instant::now();
    for round in 0..2 {
        for c in 0..COLS {
            store
                .search(&format!("col{c}"), &query, 10, std::slice::from_ref(&owner))
                .await
                .unwrap();
        }
        if round == 0 {
            println!("  eviction pass (cold after LRU evict): {:?}", t.elapsed());
        }
    }
    // Re-search the two most-recent collections: cache hits.
    let t = Instant::now();
    for _ in 0..10 {
        store
            .search("col3", &query, 10, std::slice::from_ref(&owner))
            .await
            .unwrap();
    }
    println!(
        "  cached search col3: {:.2?} (10 iters, {:?}/op)",
        t.elapsed(),
        t.elapsed() / 10
    );

    // Mixed workload: 4 searchers + 1 writer on the SAME collection —
    // every write invalidates the index, exercising snapshot retries and
    // rebuild cost under contention.
    let writer = {
        let s = Arc::clone(&store);
        let o = owner.clone();
        tokio::spawn(async move {
            for i in 0..25 {
                s.add_documents(
                    "col1",
                    &[Document {
                        id: format!("w-{i}"),
                        text: "x".into(),
                        metadata: Default::default(),
                    }],
                    &[vec![1.0; DIM]],
                    &o,
                )
                .await
                .unwrap();
            }
        })
    };
    let t = Instant::now();
    let mut handles = Vec::new();
    for _ in 0..4 {
        let s = Arc::clone(&store);
        let q = query.clone();
        let o = owner.clone();
        handles.push(tokio::spawn(async move {
            for _ in 0..15 {
                s.search("col1", &q, 10, std::slice::from_ref(&o))
                    .await
                    .unwrap();
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    writer.await.unwrap();
    println!(
        "  mixed r/w: {:.2?} for 60 searches + 25 writes on col1",
        t.elapsed()
    );
    println!("BENCH multicollection n={N}x{COLS} dim={DIM} budget=40MiB done");
}

#[tokio::test]
#[ignore]
async fn bench_1k_x_384() {
    bench(1_000, 384).await;
}

#[tokio::test]
#[ignore]
async fn bench_20k_x_768() {
    bench(20_000, 768).await;
}

#[tokio::test]
#[ignore]
async fn bench_50k_x_768() {
    bench(50_000, 768).await;
}
