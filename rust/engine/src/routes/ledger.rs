//! `/api/v1/ai/math-auditor-agent/{examine,deliberate}`. Port of
//! `stirling.api.routes.ledger` — internal endpoints called only by the
//! Java MathAuditorOrchestrator.

use std::str::FromStr;

use axum::extract::{Query, State};
use axum::routing::post;
use axum::{Json, Router};
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::agents::ledger::MathAuditorAgent;
use crate::agents::runtime::AgentError;
use crate::contracts::ledger::{Evidence, FolioManifest, Requisition, Verdict};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/examine", post(examine))
        .route("/deliberate", post(deliberate))
}

fn agent(state: &AppState) -> MathAuditorAgent {
    let runtime = state.runtime.load();
    MathAuditorAgent::new(runtime.fast_model.clone(), &runtime.settings.chat_provider)
}

fn agent_error(e: AgentError) -> ApiError {
    ApiError::internal(format!("math-auditor failed: {e}"))
}

/// Round 1: Java presents a FolioManifest; the engine declares its
/// Requisition.
async fn examine(
    State(state): State<AppState>,
    Json(manifest): Json<FolioManifest>,
) -> ApiResult<Json<Requisition>> {
    let req = agent(&state)
        .examine(&manifest)
        .await
        .map_err(agent_error)?;
    Ok(Json(req))
}

#[derive(Deserialize)]
struct DeliberateQuery {
    #[serde(default = "default_tolerance")]
    tolerance: String,
}

fn default_tolerance() -> String {
    "0.01".to_string()
}

/// Round 2: Java presents fulfilled Evidence; the engine returns a Verdict.
async fn deliberate(
    State(state): State<AppState>,
    Query(query): Query<DeliberateQuery>,
    Json(evidence): Json<Evidence>,
) -> ApiResult<Json<Verdict>> {
    let tolerance = Decimal::from_str(query.tolerance.trim()).map_err(|_| {
        ApiError::bad_request(format!("Invalid tolerance value: {:?}", query.tolerance))
    })?;
    if tolerance < Decimal::ZERO {
        return Err(ApiError::bad_request("tolerance must be non-negative"));
    }
    let verdict = agent(&state)
        .audit(&evidence, tolerance)
        .await
        .map_err(agent_error)?;
    Ok(Json(verdict))
}
