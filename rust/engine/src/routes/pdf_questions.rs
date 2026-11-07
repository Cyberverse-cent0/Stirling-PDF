//! `POST /api/v1/pdf/questions`. Port of `stirling.api.routes.pdf_questions`.

use axum::extract::State;
use axum::routing::post;
use axum::Router;

use crate::agents::pdf_questions::{PdfQuestionAgent, QuestionError};
use crate::contracts::pdf_questions::{PdfQuestionRequest, PdfQuestionResponse};
use crate::error::{ApiError, ApiResult};
use crate::extract::Json;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/questions", post(questions))
}

async fn questions(
    State(state): State<AppState>,
    Json(request): Json<PdfQuestionRequest>,
) -> ApiResult<Json<PdfQuestionResponse>> {
    request.validate().map_err(ApiError::unprocessable)?;
    let agent = PdfQuestionAgent::new(
        state.runtime.load().settings.clone(),
        state.runtime.load().smart_model.clone(),
        state.runtime.load().fast_model.clone(),
        state.runtime.load().documents.clone(),
    )
    .map_err(|e| ApiError::internal(format!("pdf-question init failed: {e}")))?;
    let response = agent.handle(&request).await.map_err(question_error)?;
    Ok(Json(response))
}

fn question_error(e: QuestionError) -> ApiError {
    match e {
        QuestionError::NoUserId(m) => ApiError::internal(m),
        other => ApiError::internal(other.to_string()),
    }
}
