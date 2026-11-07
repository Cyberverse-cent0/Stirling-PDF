//! `POST /api/v1/ai/pdf-comment-agent/generate`. Port of
//! `stirling.api.routes.pdf_comments`.

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};

use crate::agents::pdf_comment;
use crate::contracts::pdf_comments::{PdfCommentRequest, PdfCommentResponse};
use crate::error::ApiResult;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/generate", post(generate))
}

async fn generate(
    State(state): State<AppState>,
    Json(request): Json<PdfCommentRequest>,
) -> ApiResult<Json<PdfCommentResponse>> {
    let runtime = state.runtime.load();
    let provider = runtime.settings.chat_provider.clone();
    let response = pdf_comment::generate(&runtime.fast_model, &provider, &request).await?;
    Ok(Json(response))
}
