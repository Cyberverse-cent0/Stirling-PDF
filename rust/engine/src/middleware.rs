use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use subtle::ConstantTimeEq;

use crate::state::AppState;

/// `X-User-Id` carried through request extensions (the Python contextvar).
#[derive(Debug, Clone)]
pub struct RequestUserId(pub String);

const USER_ID_HEADER: &str = "x-user-id";
const ENGINE_AUTH_HEADER: &str = "x-engine-auth";

/// Public paths that skip shared-secret enforcement (liveness + docs).
const PUBLIC_PREFIXES: &[&str] = &["/health", "/docs", "/redoc", "/openapi.json"];

fn is_public(path: &str) -> bool {
    PUBLIC_PREFIXES.iter().any(|p| {
        path == *p
            || path
                .strip_prefix(p)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// Port of `EngineSharedSecretMiddleware`: non-public routes need a matching
/// `X-Engine-Auth` when a secret is configured; `require_auth` without a secret
/// fails closed with 503.
pub async fn engine_auth(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    if is_public(req.uri().path()) {
        return next.run(req).await;
    }
    let settings = &state.settings;
    if !settings.engine_shared_secret.is_empty() {
        let offered = req
            .headers()
            .get(ENGINE_AUTH_HEADER)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let valid = bool::from(
            offered
                .as_bytes()
                .ct_eq(settings.engine_shared_secret.as_bytes()),
        );
        if !valid {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"detail": "Missing or invalid X-Engine-Auth header."})),
            )
                .into_response();
        }
    } else if settings.engine_require_auth {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"detail": "Engine authentication is required but no shared secret is configured."})),
        )
            .into_response();
    }
    next.run(req).await
}

/// Port of `UserIdMiddleware`: stash `X-User-Id` in request extensions so
/// handlers and the tenancy gate can read it without re-parsing headers.
pub async fn user_id(req: Request, next: Next) -> Response {
    let user_id = req
        .headers()
        .get(USER_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let mut req = req;
    if let Some(id) = user_id {
        req.extensions_mut().insert(RequestUserId(id));
    }
    next.run(req).await
}

/// Extractor/guard equivalent of `require_user_id`: 401 when `X-User-Id` is absent.
#[derive(Debug, Clone)]
pub struct RequiredUserId(pub String);

impl axum::extract::FromRequestParts<AppState> for RequiredUserId {
    type Rejection = crate::error::ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        if let Some(id) = parts.extensions.get::<RequestUserId>() {
            return Ok(Self(id.0.clone()));
        }
        Err(crate::error::ApiError::unauthorized(
            "X-User-Id header is required",
        ))
    }
}

/// Router-level gate applied to every router except config push: when
/// `require_user_id` is set, requests without `X-User-Id` get 401.
pub async fn require_user_id_gate(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    if state.settings.require_user_id && req.headers().get(USER_ID_HEADER).is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"detail": "X-User-Id header is required"})),
        )
            .into_response();
    }
    next.run(req).await
}
