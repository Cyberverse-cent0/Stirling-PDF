pub mod agents;
pub mod docparse;
pub mod document_classifier;
pub mod documents;
pub mod health;
pub mod pdf_comments;
pub mod pdf_edit;

use axum::Router;

use crate::middleware;
use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(health::router())
        .nest(
            "/api/v1/documents",
            documents::router().merge(document_classifier::router()),
        )
        .nest("/api/v1/ai/pdf-comment-agent", pdf_comments::router())
        .nest("/api/v1/agents", agents::router())
        .nest("/api/v1/docparse", docparse::router())
        .nest("/api/v1/pdf", pdf_edit::router())
        // Python applies the identity gate to every router except config push.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::require_user_id_gate,
        ))
        .with_state(state)
}
