//! Engine shared-secret middleware tests — port of
//! `engine/tests/test_engine_auth.py`. Drives the real `engine_auth`
//! middleware over a minimal router.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::middleware::from_fn_with_state;
use axum::routing::{get, post};
use axum::Router;
use tower::ServiceExt;

use stirling_engine::config::AppSettings;
use stirling_engine::middleware;
use stirling_engine::state::AppState;

const SECRET: &str = "s3cr3t-between-java-and-engine";

async fn client(secret: &str, require_auth: bool) -> Router {
    let mut settings = AppSettings::for_test();
    settings.engine_shared_secret = secret.to_string();
    settings.engine_require_auth = require_auth;
    let state = AppState::new(settings).unwrap();
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/v1/agents/invoke", post(|| async { "ran" }))
        .layer(from_fn_with_state(state, middleware::engine_auth))
}

async fn status(router: &Router, method: &str, path: &str, auth: Option<&str>) -> StatusCode {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(value) = auth {
        builder = builder.header("x-engine-auth", value);
    }
    router
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn dev_mode_open_when_unset() {
    let app = client("", false).await;
    assert_eq!(
        status(&app, "POST", "/v1/agents/invoke", None).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn health_is_public_even_with_secret() {
    let app = client(SECRET, false).await;
    assert_eq!(status(&app, "GET", "/health", None).await, StatusCode::OK);
}

#[tokio::test]
async fn missing_header_rejected() {
    let app = client(SECRET, false).await;
    assert_eq!(
        status(&app, "POST", "/v1/agents/invoke", None).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn wrong_secret_rejected() {
    let app = client(SECRET, false).await;
    assert_eq!(
        status(&app, "POST", "/v1/agents/invoke", Some("not-the-secret")).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn valid_secret_allowed() {
    let app = client(SECRET, false).await;
    assert_eq!(
        status(&app, "POST", "/v1/agents/invoke", Some(SECRET)).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn require_auth_fails_closed_without_secret() {
    let app = client("", true).await;
    assert_eq!(
        status(&app, "POST", "/v1/agents/invoke", None).await,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(status(&app, "GET", "/health", None).await, StatusCode::OK);
}

#[tokio::test]
async fn require_auth_with_secret_enforces_normally() {
    let app = client(SECRET, true).await;
    assert_eq!(
        status(&app, "POST", "/v1/agents/invoke", None).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status(&app, "POST", "/v1/agents/invoke", Some(SECRET)).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn shared_state_is_arc_cheap() {
    // AppState clones share the runtime swap target — the config-push path
    // relies on this.
    let state = Arc::new(AppState::new(AppSettings::for_test()).unwrap());
    let clone = state.clone();
    assert!(Arc::ptr_eq(&state.runtime, &clone.runtime));
}
