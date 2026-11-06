//! RAG capability: bundles `search_knowledge` tool + instructions for agent
//! injection. Port of `stirling.documents.rag_capability`. One instance per
//! agent run, bound to the caller's principal set.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::Value;

use crate::agents::runtime::{AgentTool, ToolContext};
use crate::documents::service::DocumentService;
use crate::documents::store::SearchResult;
use crate::models::{FileId, PrincipalId};

const SEARCH_KNOWLEDGE_PARAMETERS: &str = r#"{
  "type": "object",
  "properties": {
    "query": {"type": "string", "description": "The search query describing what information you need."},
    "max_results": {"type": "integer", "description": "Maximum number of results to return."}
  },
  "required": ["query"],
  "additionalProperties": false
}"#;

pub struct RagCapability {
    documents: Arc<DocumentService>,
    principals: Vec<PrincipalId>,
    collections: Option<Vec<FileId>>,
    top_k: usize,
    max_searches: usize,
    search_count: Arc<AtomicUsize>,
}

impl RagCapability {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        documents: Arc<DocumentService>,
        principals: Vec<PrincipalId>,
        collections: Option<Vec<FileId>>,
        top_k: usize,
        max_searches: usize,
    ) -> Self {
        Self {
            documents,
            principals,
            collections,
            top_k,
            max_searches,
            search_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Dynamic when no collections are pinned: the instruction names the
    /// collections the caller can actually read at run time.
    pub async fn instructions(&self) -> String {
        let collection_desc = match &self.collections {
            Some(cols) if !cols.is_empty() => format!("collections: {}", cols.join(", ")),
            _ => {
                let collections = self
                    .documents
                    .list_collections(&self.principals)
                    .await
                    .unwrap_or_default();
                if collections.is_empty() {
                    "the knowledge base (currently empty — no collections indexed yet)".to_string()
                } else {
                    format!(
                        "the following knowledge base collections: {}",
                        collections.join(", ")
                    )
                }
            }
        };
        format!(
            "You have access to a knowledge base search tool called 'search_knowledge'. \
             It searches {collection_desc} for relevant information. \
             Use it when the provided context is insufficient to answer the user's question, \
             or when you think additional background information would improve your answer. \
             You do not have to use it if the answer is already clear from the provided text."
        )
    }

    /// `None` once the per-run search budget is exhausted, which removes the
    /// tool and prevents runaway search loops (Python `prepare` hook).
    pub fn tool(&self) -> Option<AgentTool> {
        if self.search_count.load(Ordering::SeqCst) >= self.max_searches {
            return None;
        }
        let documents = self.documents.clone();
        let principals = self.principals.clone();
        let collections = self.collections.clone();
        let top_k = self.top_k;
        let count = self.search_count.clone();
        Some(AgentTool::new(
            "search_knowledge",
            "Search the knowledge base for information relevant to the query.",
            serde_json::from_str(SEARCH_KNOWLEDGE_PARAMETERS).unwrap(),
            move |args: Value, _ctx: &ToolContext| {
                let documents = documents.clone();
                let principals = principals.clone();
                let collections = collections.clone();
                let count = count.clone();
                Box::pin(async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    let query = args
                        .get("query")
                        .and_then(Value::as_str)
                        .ok_or("query is required")?
                        .to_string();
                    let k = args
                        .get("max_results")
                        .and_then(Value::as_u64)
                        .map(|v| v as usize)
                        .unwrap_or(top_k);
                    let results =
                        search(&documents, &principals, collections.as_deref(), &query, k)
                            .await
                            .map_err(|e| e.to_string())?;
                    if results.is_empty() {
                        return Ok("No relevant results found in the knowledge base.".to_string());
                    }
                    Ok(format_results(&results))
                })
            },
        ))
    }
}

async fn search(
    documents: &DocumentService,
    principals: &[PrincipalId],
    collections: Option<&[FileId]>,
    query: &str,
    k: usize,
) -> anyhow::Result<Vec<SearchResult>> {
    match collections {
        Some(cols) if !cols.is_empty() => {
            let mut all = Vec::new();
            for col in cols {
                all.extend(
                    documents
                        .search(query, principals, Some(col), Some(k))
                        .await?,
                );
            }
            all.sort_by(|a, b| b.score.total_cmp(&a.score));
            all.truncate(k);
            Ok(all)
        }
        _ => documents.search(query, principals, None, Some(k)).await,
    }
}

fn format_results(results: &[SearchResult]) -> String {
    results
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let source = r
                .document
                .metadata
                .get("source")
                .map(String::as_str)
                .unwrap_or("unknown");
            let chunk = r
                .document
                .metadata
                .get("chunk_index")
                .map(String::as_str)
                .unwrap_or("?");
            format!(
                "[Result {} | source: {source}, chunk: {chunk}, relevance: {:.3}]\n{}",
                i + 1,
                r.score,
                r.document.text
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n---\n\n")
}
