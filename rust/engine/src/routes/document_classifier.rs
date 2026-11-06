//! `POST /api/v1/documents/classify`. Port of
//! `stirling.api.routes.document_classifier` — self-contained request, no
//! `X-User-Id` requirement beyond the router-level gate.

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};

use crate::agents::document_classifier;
use crate::contracts::document_classifier::{
    ClassifyDocumentRequest, DocumentClassificationResponse,
};
use crate::error::ApiResult;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/classify", post(classify_document))
}

async fn classify_document(
    State(state): State<AppState>,
    Json(request): Json<ClassifyDocumentRequest>,
) -> ApiResult<Json<DocumentClassificationResponse>> {
    let provider = state.settings.chat_provider.clone();
    let response = document_classifier::classify(&state.fast_model, &provider, &request).await?;
    Ok(Json(response))
}
