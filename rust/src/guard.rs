//! Request guards applied to every `/api/` route, mirroring what Java's
//! servlet filters do before a controller runs:
//!
//! - `EndpointInterceptor` rejects disabled endpoints with 403 and sets
//!   `Cache-Control: private, no-store` on every `/api/` response.
//! - Enablement comes from `endpointconfig`, the port of Java's
//!   `EndpointConfiguration`: `settings.yml`/`custom_settings.yml`
//!   `endpoints.toRemove`/`groupsToRemove`, the `ENDPOINTS_*` env vars,
//!   `RUST_DISABLED_ENDPOINTS`, and Java's built-in dependency rules
//!   (no PDF/UA tagger or form-detection model → those endpoints off,
//!   `url-to-pdf` off unless `system.enableUrlToPDF`).

use axum::extract::Request;
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use chrono::SecondsFormat;
use serde_json::json;

/// Spring Boot's error dispatch body (`spring.web.error.include-message=never`
/// drops `message`; `error` is the reason phrase).
fn forbidden(path: &str) -> Response {
    let body = json!({
        "timestamp": chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        "status": StatusCode::FORBIDDEN.as_u16(),
        "error": "Forbidden",
        "path": path,
    });
    (
        StatusCode::FORBIDDEN,
        [(header::CONTENT_TYPE, "application/json")],
        axum::Json(body),
    )
        .into_response()
}

pub async fn api_guard(req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let is_api = path.starts_with("/api/");
    if is_api && !crate::endpointconfig::is_endpoint_enabled_for_uri(&path) {
        return forbidden(&path);
    }
    let mut response = next.run(req).await;
    if is_api {
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, no-store"),
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn forbidden_is_spring_style_403_json() {
        let res = forbidden("/api/v1/misc/ocr-pdf");
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            res.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(res.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(body["status"], 403);
        assert_eq!(body["error"], "Forbidden");
        assert_eq!(body["path"], "/api/v1/misc/ocr-pdf");
        assert!(body["timestamp"].is_string());
        // `include-message=never` parity: no `message` key.
        assert!(body.get("message").is_none());
    }
}
