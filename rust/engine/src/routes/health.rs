use axum::extract::State;
use axum::routing::get;
use axum::Json;
use axum::Router;

use crate::contracts::health::HealthResponse;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/health", get(healthcheck))
}

async fn healthcheck(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
        smart_model: state.settings.smart_model_name.clone(),
        fast_model: state.settings.fast_model_name.clone(),
    })
}
