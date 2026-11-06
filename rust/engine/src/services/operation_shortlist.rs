//! Operation shortlist: ranks the tool catalogue against a request using
//! embedding cosine similarity, caching catalogue vectors. Port of
//! `stirling.services.operation_shortlist`.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::contracts::common::ConversationMessage;
use crate::documents::embedder::EmbeddingService;
use crate::documents::service::DocumentService;
use crate::models::tool_models::ToolEndpoint;
use crate::models::tool_texts::retrieval_text;

const QUERY_HISTORY_TURNS: usize = 4;
const QUERY_TURN_CHARS: usize = 500;

/// The text to rank operations against: the recent turns plus the current
/// message. A follow-up like "do the same to the other file" names no
/// operation, so ranking it alone retrieves an arbitrary twenty.
pub fn retrieval_query(message: &str, conversation_history: &[ConversationMessage]) -> String {
    let mut turns: Vec<String> = conversation_history
        .iter()
        .rev()
        .take(QUERY_HISTORY_TURNS)
        .filter_map(|t| {
            let c = t.content.trim();
            if c.is_empty() {
                None
            } else {
                Some(c.chars().take(QUERY_TURN_CHARS).collect::<String>())
            }
        })
        .collect();
    turns.reverse();
    turns.push(message.to_string());
    turns.join("\n")
}

fn cosine(left: &[f32], right: &[f32]) -> f64 {
    let dot: f64 = left
        .iter()
        .zip(right)
        .map(|(a, b)| *a as f64 * *b as f64)
        .sum();
    let ln: f64 = left
        .iter()
        .map(|a| (*a as f64) * (*a as f64))
        .sum::<f64>()
        .sqrt();
    let rn: f64 = right
        .iter()
        .map(|b| (*b as f64) * (*b as f64))
        .sum::<f64>()
        .sqrt();
    if ln != 0.0 && rn != 0.0 {
        dot / (ln * rn)
    } else {
        0.0
    }
}

/// Shared by every request; the catalogue is embedded once, and the cache is
/// dropped when a config push swaps the embedder (mirrors the Python
/// `resolve_embedder` identity check).
type CatalogueCache = Mutex<Option<(Arc<EmbeddingService>, Arc<HashMap<ToolEndpoint, Vec<f32>>>)>>;

pub struct OperationShortlist {
    documents: Arc<DocumentService>,
    cache: CatalogueCache,
}

impl OperationShortlist {
    pub fn new(documents: Arc<DocumentService>) -> Self {
        Self {
            documents,
            cache: Mutex::new(None),
        }
    }

    async fn catalogue_vectors(
        &self,
        embedder: &Arc<EmbeddingService>,
    ) -> anyhow::Result<Arc<HashMap<ToolEndpoint, Vec<f32>>>> {
        let mut guard = self.cache.lock().await;
        let stale = match guard.as_ref() {
            Some((cached, _)) => !Arc::ptr_eq(cached, embedder),
            None => true,
        };
        if stale {
            let texts: Vec<String> = ToolEndpoint::ALL
                .iter()
                .map(|op| retrieval_text(*op).to_string())
                .collect();
            let embeddings = embedder.embed_documents(&texts).await?;
            let vectors: HashMap<ToolEndpoint, Vec<f32>> =
                ToolEndpoint::ALL.iter().copied().zip(embeddings).collect();
            *guard = Some((embedder.clone(), Arc::new(vectors)));
        }
        Ok(guard.as_ref().unwrap().1.clone())
    }

    /// The `limit` operations closest to `query`, or all of them when
    /// ranking is unavailable — a deployment with no reachable embedding
    /// provider still plans, just with a larger prompt.
    pub async fn select(
        &self,
        query: &str,
        operations: &[ToolEndpoint],
        limit: usize,
    ) -> Vec<ToolEndpoint> {
        if limit == 0 || operations.len() <= limit {
            return operations.to_vec();
        }
        let embedder = self.documents.embedder().await;
        let vectors = match self.catalogue_vectors(&embedder).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "[pdf-edit] operation ranking unavailable, showing the full catalogue");
                return operations.to_vec();
            }
        };
        let query_vector = match embedder.embed_query(query).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "[pdf-edit] operation ranking unavailable, showing the full catalogue");
                return operations.to_vec();
            }
        };
        let mut ranked: Vec<(ToolEndpoint, f64)> = operations
            .iter()
            .map(|op| (*op, cosine(&query_vector, &vectors[op])))
            .collect();
        // Python sorts by descending cosine; ties keep catalogue order via
        // stable sort on the index order of `operations`.
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        ranked.truncate(limit);
        ranked.into_iter().map(|(op, _)| op).collect()
    }
}
