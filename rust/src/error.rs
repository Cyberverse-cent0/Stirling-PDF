use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::SecondsFormat;
use serde_json::json;
use thiserror::Error;

#[allow(dead_code)]
#[derive(Debug, Error)]
pub enum AppError {
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("payload too large: {0}")]
    PayloadTooLarge(String),
    #[error("unprocessable: {0}")]
    Unprocessable(String),
    #[error("compliance not met: {0}")]
    ComplianceNotMet(String),
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("internal error: {0}")]
    Internal(#[from] anyhow::Error),
    #[error("pdf error: {0}")]
    Pdf(String),
}

/// Java answers API errors as `application/problem+json` ProblemDetail
/// bodies (`GlobalExceptionHandler`): `detail` carries the exception message,
/// `title` the status reason phrase, plus a `timestamp` property. `type` is
/// the RFC 7807 URI from Java's `ErrorTypes` constants where one applies.
pub fn problem_response(status: StatusCode, type_uri: &str, detail: &str) -> Response {
    problem_response_titled(
        status,
        type_uri,
        status.canonical_reason().unwrap_or("Error"),
        detail,
    )
}

fn problem_response_titled(
    status: StatusCode,
    type_uri: &str,
    title: &str,
    detail: &str,
) -> Response {
    let body = json!({
        "type": type_uri,
        "title": title,
        "status": status.as_u16(),
        "detail": detail,
        "timestamp": chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
    });
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/problem+json")],
        Json(body),
    )
        .into_response()
}

/// `NothingToExtractException` (`handleNothingToExtract`): 422 with the
/// "Nothing Found to Extract" title. Table and image extraction endpoints
/// return this instead of an empty file when nothing matches.
pub fn nothing_to_extract(detail: &str) -> Response {
    problem_response_titled(
        StatusCode::UNPROCESSABLE_ENTITY,
        "/errors/nothing-to-extract",
        "Nothing Found to Extract",
        detail,
    )
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, type_uri, detail) = match &self {
            AppError::BadRequest(m) => (
                StatusCode::BAD_REQUEST,
                "/errors/invalid-argument",
                m.clone(),
            ),
            AppError::NotFound(m) => (StatusCode::NOT_FOUND, "/errors/not-found", m.clone()),
            AppError::PayloadTooLarge(m) => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "/errors/file-too-large",
                m.clone(),
            ),
            AppError::Unprocessable(m) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "/errors/pdf-unrepairable",
                m.clone(),
            ),
            // `handleComplianceNotMet`: 422 with the localized title.
            AppError::ComplianceNotMet(m) => {
                return problem_response_titled(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "/errors/compliance-not-met",
                    "Compliance Standard Not Met",
                    m,
                );
            }
            AppError::Forbidden(m) => (StatusCode::FORBIDDEN, "/errors/app-error", m.clone()),
            AppError::Internal(e) => {
                tracing::error!(error = %e, "internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "/errors/unexpected",
                    "internal error".into(),
                )
            }
            AppError::Pdf(e) => {
                tracing::error!(error = %e, "pdf error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "/errors/unexpected",
                    "pdf processing error".into(),
                )
            }
        };
        problem_response(status, type_uri, &detail)
    }
}

#[allow(dead_code)]
pub type AppResult<T> = Result<T, AppError>;

#[cfg(test)]
mod tests {
    use super::*;

    async fn body_json(response: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn bad_request_maps_to_400_problem_detail() {
        let res = AppError::BadRequest("no file".into()).into_response();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            res.headers().get(axum::http::header::CONTENT_TYPE).unwrap(),
            "application/problem+json"
        );
        let body = body_json(res).await;
        assert_eq!(body["status"], 400);
        assert_eq!(body["type"], "/errors/invalid-argument");
        assert_eq!(body["detail"], "no file");
        assert_eq!(body["title"], "Bad Request");
        assert!(body["timestamp"].is_string());
    }

    #[tokio::test]
    async fn error_variants_map_to_java_statuses() {
        let cases = [
            (
                AppError::NotFound("x".into()),
                StatusCode::NOT_FOUND,
                "/errors/not-found",
            ),
            (
                AppError::PayloadTooLarge("x".into()),
                StatusCode::PAYLOAD_TOO_LARGE,
                "/errors/file-too-large",
            ),
            (
                AppError::Unprocessable("x".into()),
                StatusCode::UNPROCESSABLE_ENTITY,
                "/errors/pdf-unrepairable",
            ),
            (
                AppError::Forbidden("x".into()),
                StatusCode::FORBIDDEN,
                "/errors/app-error",
            ),
        ];
        for (err, status, type_uri) in cases {
            let res = err.into_response();
            assert_eq!(res.status(), status);
            let body = body_json(res).await;
            assert_eq!(body["type"], type_uri);
            assert_eq!(body["status"], status.as_u16());
        }
    }

    #[tokio::test]
    async fn internal_and_pdf_errors_hide_details() {
        // Java's GlobalExceptionHandler does not leak internal messages;
        // the detail must be the generic string, not the anyhow chain.
        let res = AppError::Internal(anyhow::anyhow!("secret path /home/x")).into_response();
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = body_json(res).await;
        assert_eq!(body["detail"], "internal error");
        assert_ne!(body["detail"], "secret path /home/x");

        // Pdf gets its own generic detail.
        let res = AppError::Pdf("lopdf exploded".into()).into_response();
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = body_json(res).await;
        assert_eq!(body["detail"], "pdf processing error");
        assert_ne!(body["detail"], "lopdf exploded");
    }
}
