//! Request/response JSON binding with FastAPI-compatible rejection
//! semantics: malformed bodies and type mismatches surface as 422
//! `{"detail": ...}` instead of axum's default 400 plain-text rejection.

use axum::body::Bytes;
use axum::extract::FromRequest;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::error::ApiError;

pub struct Json<T>(pub T);

impl<T> Json<T> {
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T: Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}

fn json_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_none_or(|v| v.contains("json"))
}

impl<T, S> FromRequest<S> for Json<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(
        req: axum::extract::Request,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        if !json_content_type(req.headers()) {
            return Err(ApiError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Content-Type must be application/json",
            ));
        }
        let bytes = Bytes::from_request(req, _state)
            .await
            .map_err(|e| ApiError::bad_request(format!("failed to read body: {e}")))?;
        serde_json::from_slice(&bytes)
            .map(Json)
            .map_err(|e| ApiError::unprocessable(format!("invalid request body: {e}")))
    }
}
