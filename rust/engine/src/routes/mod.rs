pub mod agents;
pub mod config;
pub mod docparse;
pub mod document_classifier;
pub mod documents;
pub mod health;
pub mod ledger;
pub mod orchestrator;
pub mod pdf_comments;
pub mod pdf_edit;
pub mod pdf_questions;

use axum::Router;

use crate::middleware;
use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    // Python applies the identity gate to every router except config push,
    // so config sits outside the gated merge but still behind engine_auth.
    let gated = Router::new()
        .merge(health::router())
        .nest(
            "/api/v1/documents",
            documents::router().merge(document_classifier::router()),
        )
        .nest("/api/v1/ai/pdf-comment-agent", pdf_comments::router())
        .nest("/api/v1/ai/math-auditor-agent", ledger::router())
        .nest("/api/v1/agents", agents::router())
        .nest("/api/v1/docparse", docparse::router())
        .nest(
            "/api/v1/pdf",
            pdf_edit::router().merge(pdf_questions::router()),
        )
        .nest("/api/v1/orchestrator", orchestrator::router());
    Router::new()
        .merge(gated.layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::require_user_id_gate,
        )))
        .nest("/api/v1/config", config::router())
        .with_state(state)
}
