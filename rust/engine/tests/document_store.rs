//! Smoke tests for `SqliteVecStore` — brute-force KNN over plain blob
//! tables, no native extension.

use stirling_engine::documents::sqlite_vec_store::SqliteVecStore;
use stirling_engine::documents::store::{Document, DocumentStore, SearchResult, StoredPage};

fn doc(id: &str, text: &str) -> Document {
    Document {
        id: id.to_string(),
        text: text.to_string(),
        metadata: Default::default(),
    }
}

#[tokio::test]
async fn knn_roundtrip_search_and_acl() {
    let store = SqliteVecStore::ephemeral().unwrap();
    let owner = "user-1".to_string();
    let principal = "user-1".to_string();

    store
        .ensure_collection("knowledge", "test", &owner, None)
        .await
        .unwrap();
    store
        .add_documents(
            "knowledge",
            &[doc("a", "hello world"), doc("b", "goodbye world")],
            &[vec![1.0, 0.0, 0.0], vec![0.0, 1.0, 0.0]],
            &owner,
        )
        .await
        .unwrap();
    store
        .add_pages(
            "knowledge",
            &[StoredPage {
                page_number: 1,
                text: "page one".to_string(),
                char_count: 8,
            }],
            &owner,
        )
        .await
        .unwrap();
    store
        .grant_read("knowledge", &owner, std::slice::from_ref(&principal))
        .await
        .unwrap();

    let results: Vec<SearchResult> = store
        .search(
            "knowledge",
            &[1.0, 0.0, 0.0],
            2,
            std::slice::from_ref(&principal),
        )
        .await
        .unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].document.id, "a");
    assert!(results[0].score > results[1].score);

    // No ACL → empty.
    assert!(store
        .search("knowledge", &[1.0, 0.0, 0.0], 2, &["stranger".to_string()])
        .await
        .unwrap()
        .is_empty());

    let pages = store
        .read_pages("knowledge", None, std::slice::from_ref(&principal))
        .await
        .unwrap();
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].text, "page one");

    assert!(store
        .has_collection("knowledge", &[principal])
        .await
        .unwrap());
    assert!(store.delete_collection("knowledge", &owner).await.unwrap());
    assert!(!store.delete_collection("knowledge", &owner).await.unwrap());
}

#[tokio::test]
async fn ingest_replaces_documents_for_same_ids() {
    let store = SqliteVecStore::ephemeral().unwrap();
    let owner = "u".to_string();
    let principal = "u".to_string();
    store
        .ensure_collection("c", "s", &owner, None)
        .await
        .unwrap();
    store
        .add_documents("c", &[doc("x", "v1")], &[vec![1.0]], &owner)
        .await
        .unwrap();
    store
        .add_documents("c", &[doc("x", "v2")], &[vec![0.0, 1.0]], &owner)
        .await
        .unwrap_err(); // dim mismatch must fail like Python
    store
        .add_documents("c", &[doc("x", "v2")], &[vec![1.0]], &owner)
        .await
        .unwrap();
    store.grant_read("c", &owner, &[principal]).await.unwrap();
    let results = store
        .search("c", &[1.0], 5, &["u".to_string()])
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].document.text, "v2");
}
