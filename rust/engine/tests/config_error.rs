//! Config parsing and error-mapping tests for `stirling-engine`.
//! Complements `tests/contracts.rs` (serde shapes) and
//! `tests/http_classify.rs` (router-level behaviour).

use std::str::FromStr;

use axum::body::to_bytes;
use axum::http::StatusCode;
use axum::response::IntoResponse;

use stirling_engine::config::DocumentsBackend;
use stirling_engine::error::ApiError;

#[test]
fn documents_backend_parses_known_values() {
    assert_eq!(
        DocumentsBackend::from_str("sqlite"),
        Ok(DocumentsBackend::Sqlite)
    );
    assert_eq!(
        DocumentsBackend::from_str("pgvector"),
        Ok(DocumentsBackend::Pgvector)
    );
}

#[test]
fn documents_backend_rejects_unknown_values() {
    let err = DocumentsBackend::from_str("mysql").unwrap_err();
    assert!(err.contains("STIRLING_DOCUMENTS_BACKEND"));
    // Case-sensitive, matching pydantic's literal validation.
    assert!(DocumentsBackend::from_str("SQLite").is_err());
    assert!(DocumentsBackend::from_str("").is_err());
}

#[tokio::test]
async fn api_error_renders_fastapi_style_detail_body() {
    let res = ApiError::bad_request("missing fileInput").into_response();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json, serde_json::json!({ "detail": "missing fileInput" }));
}

#[tokio::test]
async fn api_error_statuses_map_to_http_codes() {
    for (err, expected) in [
        (ApiError::unauthorized("nope"), StatusCode::UNAUTHORIZED),
        (ApiError::not_found("gone"), StatusCode::NOT_FOUND),
        (
            ApiError::unprocessable("bad"),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            ApiError::internal("boom"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    ] {
        assert_eq!(err.into_response().status(), expected);
    }
}

#[test]
fn anyhow_errors_become_500_internal() {
    let err = ApiError::from(anyhow::anyhow!("db exploded"));
    assert_eq!(err.status, StatusCode::INTERNAL_SERVER_ERROR);
    // The internal detail is deliberately generic — the real error is
    // only logged. Starlette's unhandled-exception middleware answers
    // plain-text "Internal Server Error", not a JSON detail body.
    assert_eq!(err.detail, "Internal Server Error");
    let response = err.into_response();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/plain; charset=utf-8"
    );
}
