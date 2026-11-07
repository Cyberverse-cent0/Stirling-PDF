//! `GET /capabilities`, `POST /ingest` for DocParse. Port of
//! `stirling.api.routes.docparse`.

use std::collections::HashMap;

use axum::extract::State;
use axum::routing::{get, post};
use axum::Router;

use crate::contracts::docparse::{DocparseCapabilities, IngestRequest, IngestResponse};
use crate::contracts::documents::PageText;
use crate::docparse::chunking::{pack_blocks, page_texts};
use crate::documents::service::{CONTENT_TYPE_METADATA_KEY, DOCPARSE_CHUNK_CONTENT_TYPE};
use crate::error::{ApiError, ApiResult};
use crate::extract::Json;
use crate::middleware::RequiredUserId;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/capabilities", get(capabilities))
        .route("/ingest", post(ingest))
}

async fn capabilities(State(state): State<AppState>) -> Json<DocparseCapabilities> {
    Json(DocparseCapabilities {
        indexing_configured: state.runtime.load().documents.embedder().await.configured(),
    })
}

fn chunk_metadata(chunk: &crate::contracts::docparse::DocChunk) -> HashMap<String, String> {
    let mut meta = HashMap::new();
    meta.insert(
        CONTENT_TYPE_METADATA_KEY.to_string(),
        DOCPARSE_CHUNK_CONTENT_TYPE.to_string(),
    );
    if let Some(start) = chunk.page_start {
        meta.insert("page_start".to_string(), start.to_string());
    }
    if let Some(end) = chunk.page_end {
        meta.insert("page_end".to_string(), end.to_string());
    }
    if !chunk.heading_path.is_empty() {
        meta.insert("heading_path".to_string(), chunk.heading_path.join(" > "));
    }
    meta
}

/// Pack the caller's Markdown blocks into chunks, then embed and index them.
/// Re-ingesting a documentId replaces its stored content. `index=false`
/// skips the store; `includeChunks` returns the chunks.
async fn ingest(
    State(state): State<AppState>,
    RequiredUserId(user_id): RequiredUserId,
    Json(request): Json<IngestRequest>,
) -> ApiResult<Json<IngestResponse>> {
    request.validate().map_err(ApiError::unprocessable)?;
    let settings = state.runtime.load().settings.clone();
    let chunk_size = request.chunk_size.unwrap_or(settings.rag_chunk_size);
    let overlap = request.overlap.unwrap_or(settings.rag_chunk_overlap);

    if request.blocks.is_empty() {
        // The caller converts the text layer, so an empty payload means the
        // document has none — a scan, not a conversion failure.
        return Err(ApiError::unprocessable(
            "no extractable text: the document has no text layer, so OCR it before ingesting",
        ));
    }
    if overlap >= chunk_size {
        // chunk_text derives its stride as chunk_size - overlap; a zero stride
        // would stall and a negative one silently drops over-long sentences.
        return Err(ApiError::unprocessable(format!(
            "overlap ({overlap}) must be smaller than chunkSize ({chunk_size})"
        )));
    }
    if !request.index && !request.include_chunks {
        return Err(ApiError::unprocessable(
            "nothing to do: enable index or includeChunks",
        ));
    }

    let chunks = pack_blocks(&request.blocks, chunk_size, overlap);

    let mut chunks_indexed = 0i64;
    if request.index {
        // Owner/ACL semantics mirror IngestDocumentRequest; omitted values
        // default to the authenticated caller (personal-doc behaviour).
        let owner_id = request.owner_id.clone().unwrap_or_else(|| user_id.clone());
        let read_principals = request
            .read_principals
            .clone()
            .unwrap_or_else(|| vec![owner_id.clone()]);
        let prepared: Vec<(String, HashMap<String, String>)> = chunks
            .iter()
            .map(|c| (c.text.clone(), chunk_metadata(c)))
            .collect();
        let pages: Vec<PageText> = page_texts(&request.blocks);
        chunks_indexed = state
            .runtime
            .load()
            .documents
            .clone()
            .ingest_prepared(
                &request.document_id,
                &prepared,
                &request.source,
                &owner_id,
                &read_principals,
                request.expires_at,
                Some(&pages),
            )
            .await? as i64;
    }

    tracing::info!(
        document_id = %request.document_id,
        chunks_indexed,
        "docparse: ingested document"
    );
    Ok(Json(IngestResponse {
        document_id: request.document_id,
        chunks_indexed,
        chunks: if request.include_chunks {
            Some(chunks)
        } else {
            None
        },
    }))
}
