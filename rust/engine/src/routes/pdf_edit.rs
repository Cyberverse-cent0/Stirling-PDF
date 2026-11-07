//! `POST /api/v1/pdf/edit`. Port of `stirling.api.routes.pdf_edit`.

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};

use crate::contracts::pdf_edit::{PdfEditRequest, PdfEditResponse};
use crate::error::ApiResult;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/edit", post(edit))
}

async fn edit(
    State(state): State<AppState>,
    Json(request): Json<PdfEditRequest>,
) -> ApiResult<Json<PdfEditResponse>> {
    let pdf_edit = state.runtime.load().pdf_edit.clone();
    let response = pdf_edit.handle(&request).await?;
    Ok(Json(response))
}
