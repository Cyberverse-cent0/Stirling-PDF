//! `POST /api/v1/pdf/edit`. Port of `stirling.api.routes.pdf_edit`.

use axum::extract::State;
use axum::routing::post;
use axum::Router;

use crate::contracts::pdf_edit::{PdfEditRequest, PdfEditResponse};
use crate::error::{ApiError, ApiResult};
use crate::extract::Json;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/edit", post(edit))
}

async fn edit(
    State(state): State<AppState>,
    Json(request): Json<PdfEditRequest>,
) -> ApiResult<Json<PdfEditResponse>> {
    request.validate().map_err(ApiError::unprocessable)?;
    let pdf_edit = state.runtime.load().pdf_edit.clone();
    let response = pdf_edit.handle(&request).await?;
    Ok(Json(response))
}
