//! Authentication gate for `/api/**`, reproducing the *OSS* Java security
//! model: core Stirling-PDF ships no authentication — `spring-boot-starter-
//! security` and the JWT/session/OAuth2/SAML2/API-key stack only exist in the
//! `proprietary` and `saas` modules. Rust therefore defaults to open, exactly
//! like an OSS Java deployment.
//!
//! Two operator-facing controls:
//!
//! - `RUST_API_KEY` — when set, every `/api/**` request must carry the key in
//!   `X-API-Key` or `Authorization: Bearer`. This is a deployment guard for
//!   standalone Rust, not a port of proprietary auth; it does not implement
//!   users, teams, sessions, JWT, or licence checks.
//! - A startup warning fires when proprietary/saas markers are present
//!   (`DOCKER_ENABLE_SECURITY=true`, `SAAS_MODE`, license envs) because those
//!   deployments' Java edge enforces authz that Rust cannot replicate — the
//!   correct topology there is Java-in-front, Rust behind it.
//!
//! Exemptions mirror `RequestUriUtils.isPublicAuthEndpoint`'s spirit: the
//! phone-side mobile-scanner calls carry no credentials by design, and
//! health probes must stay open.

use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use chrono::SecondsFormat;
use serde_json::json;

/// `^/api/v1/mobile-scanner/(validate-session|upload)/[A-Za-z0-9-]+$` —
/// `MOBILE_SCANNER_PHONE_PATTERN` in `RequestUriUtils`.
fn is_phone_scanner_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/api/v1/mobile-scanner/") else {
        return false;
    };
    let mut parts = rest.split('/');
    let (Some(op), Some(id), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    (op == "validate-session" || op == "upload")
        && !id.is_empty()
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn is_public(path: &str) -> bool {
    is_phone_scanner_path(path)
        || path.starts_with("/health")
        || path.starts_with("/healthz")
        || path.starts_with("/liveness")
        || path.starts_with("/readiness")
        || path.starts_with("/actuator/")
}

fn unauthorized(path: &str) -> Response {
    let body = json!({
        "type": "about:blank",
        "title": "Unauthorized",
        "status": StatusCode::UNAUTHORIZED.as_u16(),
        "detail": "Authentication required",
        "instance": path,
        "timestamp": chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
    });
    (
        StatusCode::UNAUTHORIZED,
        [
            (header::CONTENT_TYPE, "application/problem+json"),
            (header::WWW_AUTHENTICATE, "Bearer"),
        ],
        axum::Json(body),
    )
        .into_response()
}

fn key_matches(req: &Request, expected: &str) -> bool {
    let api_key = req.headers().get("x-api-key").and_then(|v| v.to_str().ok());
    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    api_key == Some(expected) || bearer == Some(expected)
}

pub async fn auth_guard(req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    if !path.starts_with("/api/") || is_public(&path) {
        return next.run(req).await;
    }
    let expected = std::env::var("RUST_API_KEY").unwrap_or_default();
    let expected = expected.trim();
    if expected.is_empty() || key_matches(&req, expected) {
        return next.run(req).await;
    }
    unauthorized(&path)
}

/// Startup check: proprietary/saas deployments get their authz from Java's
/// security filters; warn loudly if Rust is being exposed in that mode.
pub fn warn_if_secured_deployment() {
    let security_enabled = std::env::var("DOCKER_ENABLE_SECURITY")
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let saas = std::env::var("SAAS_MODE")
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
        || std::env::var("SPRING_PROFILES_ACTIVE")
            .map(|v| {
                v.split(',')
                    .any(|p| p.trim() == "saas" || p.trim() == "proprietary")
            })
            .unwrap_or(false);
    if security_enabled || saas {
        tracing::warn!(
            "proprietary/saas security markers detected: Rust serves /api/** without \
             Java's JWT/session/API-key authorization. Keep Java as the public edge or \
             set RUST_API_KEY; full proprietary auth is not ported."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;

    #[test]
    fn phone_scanner_paths_match_java_regex() {
        assert!(is_phone_scanner_path(
            "/api/v1/mobile-scanner/validate-session/abc-123"
        ));
        assert!(is_phone_scanner_path(
            "/api/v1/mobile-scanner/upload/session-123"
        ));
        // Underscores are outside Java's [A-Za-z0-9-]+ session-id pattern.
        assert!(!is_phone_scanner_path(
            "/api/v1/mobile-scanner/upload/SESSION_1"
        ));
        // Other ops, extra segments, and bad ids are not phone paths.
        assert!(!is_phone_scanner_path("/api/v1/mobile-scanner/files/abc"));
        assert!(!is_phone_scanner_path("/api/v1/mobile-scanner/session/abc"));
        assert!(!is_phone_scanner_path(
            "/api/v1/mobile-scanner/validate-session/a/b"
        ));
        assert!(!is_phone_scanner_path(
            "/api/v1/mobile-scanner/validate-session/bad_id!"
        ));
        assert!(!is_phone_scanner_path("/api/v1/general/merge-pdfs"));
    }

    #[test]
    fn public_paths_cover_health_and_scanner() {
        assert!(is_public("/health"));
        assert!(is_public("/healthz"));
        assert!(is_public("/actuator/health"));
        assert!(is_public("/api/v1/mobile-scanner/upload/s1"));
        assert!(!is_public("/api/v1/general/merge-pdfs"));
        assert!(!is_public("/api/v1/mobile-scanner/files/s1"));
    }

    fn request(headers: &[(&str, &str)]) -> Request<Body> {
        let mut builder = Request::builder().uri("/api/v1/general/merge-pdfs");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(Body::empty()).unwrap()
    }

    #[test]
    fn key_matches_accepts_both_header_styles() {
        assert!(key_matches(&request(&[("x-api-key", "s3cret")]), "s3cret"));
        assert!(key_matches(
            &request(&[("authorization", "Bearer s3cret")]),
            "s3cret"
        ));
        assert!(!key_matches(&request(&[("x-api-key", "wrong")]), "s3cret"));
        assert!(!key_matches(&request(&[]), "s3cret"));
        // Bearer prefix is required — a bare token in Authorization fails.
        assert!(!key_matches(
            &request(&[("authorization", "s3cret")]),
            "s3cret"
        ));
        // Case-insensitive header lookup (http crate normalises).
        assert!(key_matches(&request(&[("X-API-KEY", "s3cret")]), "s3cret"));
    }
}
