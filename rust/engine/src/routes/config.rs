//! `POST /api/v1/config` — admin-pushed AI settings. Port of
//! `stirling.api.routes.config`. The only router exempt from the user-id
//! gate (still behind the shared-secret middleware).

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, State};
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};

use crate::config_cache;
use crate::config_push::resolve_and_apply;
use crate::contracts::config::{ConfigApplyResponse, ConfigPushRequest};
use crate::error::ApiResult;
use crate::state::AppState;

/// Headers whose presence means `client.host` may be proxy-rewritten and
/// cannot be trusted as the transport peer — a spoofed X-Forwarded-For could
/// otherwise read as loopback.
const FORWARDING_HEADERS: &[&str] = &[
    "x-forwarded-for",
    "x-forwarded-host",
    "x-real-ip",
    "forwarded",
];

pub fn router() -> Router<AppState> {
    Router::new().route("", post(apply_config))
}

/// True only for a direct local connection with no proxy; fails closed if
/// any forwarding header is present.
fn is_direct_loopback_client(headers: &HeaderMap, client: Option<SocketAddr>) -> bool {
    if FORWARDING_HEADERS.iter().any(|h| headers.contains_key(*h)) {
        return false;
    }
    client.is_some_and(|addr| addr.ip().is_loopback())
}

/// Contract-level ranges from the Python model (`Field(ge=...)`): rejected
/// with 422 before anything is applied. `maxSearches` floors at 0 — "no
/// retrieval searches" is legitimate.
fn validate_ranges(request: &ConfigPushRequest) -> Result<(), crate::error::ApiError> {
    let m = &request.models;
    let r = &request.rag;
    let l = &request.limits;
    let positive = |v: Option<u32>| v.is_none_or(|n| n >= 1);
    if !positive(m.smart_max_tokens)
        || !positive(m.fast_max_tokens)
        || !positive(r.top_k)
        || !positive(l.max_pages)
        || !positive(l.max_characters)
        || !positive(l.model_max_concurrency)
    {
        return Err(crate::error::ApiError::unprocessable(
            "numeric fields must be >= 1",
        ));
    }
    Ok(())
}

async fn apply_config(
    State(state): State<AppState>,
    ConnectInfo(client): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(request): Json<ConfigPushRequest>,
) -> ApiResult<Json<ConfigApplyResponse>> {
    validate_ranges(&request)?;
    let current = state.runtime.load().settings.clone();
    if !current.allow_config_push {
        return Err(crate::error::ApiError::forbidden(
            "Config push is disabled on this deployment (STIRLING_ALLOW_CONFIG_PUSH is false).",
        ));
    }
    // Secure-by-default: with no shared secret set, only trust a direct
    // loopback caller — a pushed base_url/model could repoint the engine to
    // exfiltrate document content.
    if current.engine_shared_secret.is_empty() && !is_direct_loopback_client(&headers, Some(client))
    {
        tracing::warn!(
            "Rejected config push from non-local/proxied caller {} with no shared secret set",
            client.ip()
        );
        return Err(crate::error::ApiError::forbidden(
            "Config push from a non-local or proxied caller requires \
             STIRLING_ENGINE_SHARED_SECRET to be set on both the engine and the processor.",
        ));
    }

    let resolved = resolve_and_apply(&current, &request)
        .map_err(|e| crate::error::ApiError::bad_request(e.to_string()))?;

    // Swap live state first; persist is best-effort after.
    state
        .apply_push(
            resolved.settings.clone(),
            resolved.chat.as_ref(),
            resolved.embedder.as_ref().map(|(m, c)| (m.as_str(), c)),
        )
        .await
        .map_err(|e| crate::error::ApiError::bad_request(format!("config apply failed: {e}")))?;

    let mut notes = resolved.notes;
    match config_cache::save_config(&state.runtime.load().settings, &request) {
        Ok(()) => {
            // Claim the stamp we just wrote so the watcher does not rebuild for it.
            *state.config_stamp.lock().unwrap() = config_cache::cache_stamp();
        }
        Err(e) => {
            tracing::warn!("Applied AI config but failed to persist the encrypted cache: {e}");
            notes.push(
                "Config applied on this worker but could not be persisted; it will not survive an \
                 engine restart and other workers will not pick it up."
                    .to_string(),
            );
        }
    }

    let effective = &resolved.settings;
    tracing::info!(
        provider = %request.models.provider.trim(),
        smart_model = %effective.smart_model_name,
        fast_model = %effective.fast_model_name,
        top_k = effective.rag_default_top_k,
        "Applied pushed AI config"
    );

    Ok(Json(ConfigApplyResponse {
        status: "applied".to_string(),
        provider: request.models.provider.trim().to_string(),
        smart_model: effective.smart_model_name.clone(),
        fast_model: effective.fast_model_name.clone(),
        smart_max_tokens: effective.smart_model_max_tokens.unwrap_or(0),
        fast_max_tokens: effective.fast_model_max_tokens.unwrap_or(0),
        rag_embedding_model: effective.rag_embedding_model.clone(),
        rag_top_k: effective.rag_default_top_k as u32,
        rag_max_searches: effective.rag_max_searches as u32,
        max_pages: effective.max_pages as u32,
        max_characters: effective.max_characters as u32,
        model_max_concurrency: effective.model_max_concurrency as u32,
        notes,
    }))
}

/// Test-only helper kept for parity checks: the loopback predicate must
/// reject proxied callers even when the transport peer is local.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_requires_no_forwarding_headers() {
        let addr: SocketAddr = "127.0.0.1:5001".parse().unwrap();
        assert!(is_direct_loopback_client(&HeaderMap::new(), Some(addr)));

        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "127.0.0.1".parse().unwrap());
        assert!(!is_direct_loopback_client(&headers, Some(addr)));

        let remote: SocketAddr = "10.0.0.5:5001".parse().unwrap();
        assert!(!is_direct_loopback_client(&HeaderMap::new(), Some(remote)));
        assert!(!is_direct_loopback_client(&HeaderMap::new(), None));
    }
}
