//! `POST /api/v1/documents`, `DELETE /by-id/{id}`, `DELETE /by-owner`.
//! Port of `stirling.api.routes.documents`.

use axum::extract::{Path, State};
use axum::routing::{delete, post};
use axum::Router;

use crate::contracts::documents::{
    DeleteDocumentResponse, IngestDocumentRequest, IngestDocumentResponse, PurgeOwnerResponse,
};
use crate::error::{ApiError, ApiResult};
use crate::extract::Json;
use crate::middleware::RequiredUserId;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", post(ingest_document))
        .route("/by-id/{document_id}", delete(delete_document))
        .route("/by-owner", delete(purge_caller_documents))
}

/// `X-User-Id` is still required for auth + analytics, but the body carries
/// `owner_id`/`read_principals` verbatim — it does not constrain the body.
async fn ingest_document(
    State(state): State<AppState>,
    RequiredUserId(_user_id): RequiredUserId,
    Json(request): Json<IngestDocumentRequest>,
) -> ApiResult<Json<IngestDocumentResponse>> {
    request.validate().map_err(ApiError::unprocessable)?;
    let documents = state.runtime.load().documents.clone();
    let chunks_indexed = documents
        .ingest(
            &request.document_id,
            request.page_text.as_deref().unwrap_or(&[]),
            &request.source,
            &request.owner_id,
            &request.read_principals,
            request.expires_at,
        )
        .await? as i64;
    Ok(Json(IngestDocumentResponse {
        document_id: request.document_id,
        chunks_indexed,
    }))
}

/// Personal-doc delete: owner is inferred from the caller.
async fn delete_document(
    State(state): State<AppState>,
    RequiredUserId(user_id): RequiredUserId,
    Path(document_id): Path<String>,
) -> ApiResult<Json<DeleteDocumentResponse>> {
    let principals = vec![user_id.clone()];
    let documents = state.runtime.load().documents.clone();
    let existed = documents.has_collection(&document_id, &principals).await?;
    if existed {
        documents.delete_collection(&document_id, &user_id).await?;
    }
    Ok(Json(DeleteDocumentResponse {
        document_id,
        deleted: existed,
    }))
}

/// Java calls this on logout so a user's document content disappears as soon
/// as the session ends. Only caller-owned collections are removed.
async fn purge_caller_documents(
    State(state): State<AppState>,
    RequiredUserId(user_id): RequiredUserId,
) -> ApiResult<Json<PurgeOwnerResponse>> {
    let deleted = state.runtime.load().documents.purge_owner(&user_id).await?;
    tracing::info!(deleted, owner = %user_id, "Purged caller document collections");
    Ok(Json(PurgeOwnerResponse {
        owner_id: user_id,
        deleted: deleted as i64,
    }))
}
