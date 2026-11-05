//! Fallback proxy to the Java/Spring backend for every `/api/**` route the
//! Rust server hasn't migrated yet — the strangler-fig seam from
//! `MIGRATION.md`. Anything outside `/api/` stays a 404: Rust owns only the
//! API surface during transition.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use http_body_util::BodyStream;

use crate::config::JavaBackend;
use crate::error::AppError;
use crate::handlers::jobs::AppState;
use crate::upload::Upload;

/// Hop-by-hop headers that must not be forwarded (RFC 9110 §7.6.1), plus
/// `host` (reqwest sets it). `content-length` stays because the body is
/// proxied untransformed.
const SKIPPED_REQUEST_HEADERS: &[&str] = &[
    "connection",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
];

const SKIPPED_RESPONSE_HEADERS: &[&str] = &[
    "connection",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

#[derive(Clone)]
pub struct ProxyState {
    pub backend: Option<Arc<JavaBackend>>,
}

pub async fn proxy(State(state): State<AppState>, req: Request) -> Response {
    let state = ProxyState {
        backend: state.backend.clone(),
    };
    proxy_inner(state, req).await
}

async fn proxy_inner(state: ProxyState, req: Request) -> Response {
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    if !path_and_query.starts_with("/api/") {
        return AppError::NotFound("not found".into()).into_response();
    }
    let Some(backend) = &state.backend else {
        return AppError::NotFound("not found".into()).into_response();
    };

    let url = format!("{}{}", backend.base_url, path_and_query);
    let method = req.method().clone();
    let headers = forward_request_headers(req.headers());
    let body_stream = BodyStream::new(req.into_body()).map(|frame| match frame {
        Ok(frame) => match frame.into_data() {
            Ok(bytes) => Ok(bytes),
            Err(_) => Ok(bytes::Bytes::new()),
        },
        Err(e) => Err(e),
    });

    let upstream = backend
        .client
        .request(method, &url)
        .headers(headers)
        .body(reqwest::Body::wrap_stream(body_stream))
        .send()
        .await;

    let Ok(upstream) = upstream else {
        return AppError::BadRequest("upstream backend unavailable".into()).into_response();
    };

    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response = Response::builder().status(status);
    for (name, value) in upstream.headers() {
        if SKIPPED_RESPONSE_HEADERS.contains(&name.as_str()) {
            continue;
        }
        response = response.header(name.clone(), value.clone());
    }
    match response.body(Body::from_stream(upstream.bytes_stream())) {
        Ok(response) => response,
        Err(_) => {
            AppError::Internal(anyhow::anyhow!("proxy response build failed")).into_response()
        }
    }
}

/// Forwards a body-less request (GET/DELETE/HEAD) for a specific path — used
/// by the job endpoints to fall back to Java for jobs Rust doesn't own.
pub async fn forward(
    backend: &Option<Arc<JavaBackend>>,
    method: &str,
    path_and_query: &str,
    headers: &HeaderMap,
) -> Response {
    let Some(backend) = backend else {
        return AppError::NotFound("not found".into()).into_response();
    };
    let url = format!("{}{}", backend.base_url, path_and_query);
    let upstream = backend
        .client
        .request(method.parse().unwrap_or(axum::http::Method::GET), &url)
        .headers(forward_request_headers(headers))
        .send()
        .await;
    let Ok(upstream) = upstream else {
        return AppError::BadRequest("upstream backend unavailable".into()).into_response();
    };
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response = Response::builder().status(status);
    for (name, value) in upstream.headers() {
        if SKIPPED_RESPONSE_HEADERS.contains(&name.as_str()) {
            continue;
        }
        response = response.header(name.clone(), value.clone());
    }
    match response.body(Body::from_stream(upstream.bytes_stream())) {
        Ok(response) => response,
        Err(_) => {
            AppError::Internal(anyhow::anyhow!("proxy response build failed")).into_response()
        }
    }
}

/// Re-sends a parsed multipart request to Java verbatim — used by migrated
/// endpoints for parameter combinations Rust can't serve natively (e.g.
/// `compress-pdf` line-art, which needs the ImageMagick service). The body is
/// rebuilt from the parsed uploads/params, so the original `content-type`
/// boundary and `content-length` must not be forwarded.
pub async fn forward_multipart(
    state: &AppState,
    path_and_query: &str,
    headers: &HeaderMap,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> Response {
    let Some(backend) = &state.backend else {
        return AppError::NotFound("not found".into()).into_response();
    };
    let url = format!("{}{}", backend.base_url, path_and_query);

    let mut form = reqwest::multipart::Form::new();
    for upload in uploads {
        form = form.part(
            crate::upload::FILE_FIELD,
            reqwest::multipart::Part::bytes(upload.bytes).file_name(upload.name),
        );
    }
    for (name, value) in params {
        form = form.text(name, value);
    }

    let mut forwarded = forward_request_headers(headers);
    forwarded.remove(header::CONTENT_TYPE);
    forwarded.remove(header::CONTENT_LENGTH);

    let upstream = backend
        .client
        .post(&url)
        .headers(forwarded)
        .multipart(form)
        .send()
        .await;
    let Ok(upstream) = upstream else {
        return AppError::BadRequest("upstream backend unavailable".into()).into_response();
    };
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response = Response::builder().status(status);
    for (name, value) in upstream.headers() {
        if SKIPPED_RESPONSE_HEADERS.contains(&name.as_str()) {
            continue;
        }
        response = response.header(name.clone(), value.clone());
    }
    match response.body(Body::from_stream(upstream.bytes_stream())) {
        Ok(response) => response,
        Err(_) => {
            AppError::Internal(anyhow::anyhow!("proxy response build failed")).into_response()
        }
    }
}

fn forward_request_headers(headers: &HeaderMap) -> HeaderMap {
    let mut forwarded = HeaderMap::new();
    for (name, value) in headers {
        if SKIPPED_REQUEST_HEADERS.contains(&name.as_str()) {
            continue;
        }
        forwarded.insert(name.clone(), value.clone());
    }
    forwarded
}
