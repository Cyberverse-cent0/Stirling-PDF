use std::fmt;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

/// FastAPI-style error body: handled errors are `{"detail": ...}`; unhandled
/// exceptions render Starlette's plain-text `Internal Server Error` instead.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub detail: String,
    plain: bool,
}

impl ApiError {
    pub fn new(status: StatusCode, detail: impl Into<String>) -> Self {
        Self {
            status,
            detail: detail.into(),
            plain: false,
        }
    }

    fn plain(status: StatusCode, detail: &'static str) -> Self {
        Self {
            status,
            detail: detail.to_string(),
            plain: true,
        }
    }

    pub fn bad_request(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, detail)
    }

    pub fn unauthorized(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, detail)
    }

    pub fn forbidden(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, detail)
    }

    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, detail)
    }

    pub fn unprocessable(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, detail)
    }

    pub fn internal(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, detail)
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for ApiError {}

#[derive(Serialize)]
struct ErrorBody<'a> {
    detail: &'a str,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.plain {
            return (self.status, self.detail).into_response();
        }
        (
            self.status,
            Json(ErrorBody {
                detail: &self.detail,
            }),
        )
            .into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        tracing::error!(error = %err, "unhandled engine error");
        Self::plain(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
