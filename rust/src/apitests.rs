//! In-process HTTP tests against the real `handlers::router` — the Rust
//! counterpart of the Java MockMvc suite. Multipart bodies are assembled
//! by hand so no extra dev-dependency is needed; fixtures live in
//! `tests/fixtures/`.

use axum::body::Body;
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::Router;
use tower::ServiceExt;

use crate::handlers::jobs::AppState;

const BOUNDARY: &str = "stirling-apitest-boundary";

fn app() -> Router {
    // The router's metrics middleware panics on an uninitialised store;
    // `init` is idempotent so parallel tests can each call it.
    crate::metrics::init(true);
    crate::handlers::router(AppState::new())
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap_or_else(|e| panic!("fixture {name}: {e}"))
}

enum Part<'a> {
    File {
        name: &'a str,
        filename: &'a str,
        data: &'a [u8],
    },
    Field {
        name: &'a str,
        value: &'a str,
    },
}

fn multipart(parts: &[Part<'_>]) -> Vec<u8> {
    let mut body = Vec::new();
    for part in parts {
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        match part {
            Part::File {
                name,
                filename,
                data,
            } => {
                body.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
                    )
                    .as_bytes(),
                );
                body.extend_from_slice(data);
            }
            Part::Field { name, value } => {
                body.extend_from_slice(
                    format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}")
                        .as_bytes(),
                );
            }
        }
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

async fn send(
    app: &Router,
    method: Method,
    uri: &str,
    body: Vec<u8>,
    content_type: Option<&str>,
) -> (StatusCode, HeaderMap, bytes::Bytes) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(ct) = content_type {
        builder = builder.header(header::CONTENT_TYPE, ct);
    }
    let res = app
        .clone()
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, headers, bytes)
}

fn upload_pdf<'a>(name: &'a str, data: &'a [u8]) -> Part<'a> {
    Part::File {
        name: "fileInput",
        filename: name,
        data,
    }
}

async fn post_multipart(
    app: &Router,
    uri: &str,
    parts: &[Part<'_>],
) -> (StatusCode, HeaderMap, bytes::Bytes) {
    send(
        app,
        Method::POST,
        uri,
        multipart(parts),
        Some(&format!("multipart/form-data; boundary={BOUNDARY}")),
    )
    .await
}

#[tokio::test]
async fn health_and_version() {
    let app = app();
    let (status, _, body) = send(&app, Method::GET, "/health", vec![], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_ref(), br#"{"status":"ok"}"#);

    let (status, _, body) = send(&app, Method::GET, "/api/v1/info/version", vec![], None).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["runtime"], "rust");
}

#[tokio::test]
async fn rotate_pdf_produces_valid_pdf() {
    let app = app();
    let pdf = fixture("basic.pdf");
    let (status, headers, body) = post_multipart(
        &app,
        "/api/v1/general/rotate-pdf",
        &[upload_pdf("basic.pdf", &pdf)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/pdf")
    );
    assert!(body.starts_with(b"%PDF-"), "output is not a PDF");
    let doc = lopdf::Document::load_mem(&body).expect("rotated output must parse");
    assert_eq!(lopdf::Document::get_pages(&doc).len(), 1);
}

#[tokio::test]
async fn rotate_pdf_rejects_non_quarter_turn() {
    let app = app();
    let pdf = fixture("basic.pdf");
    let (status, headers, body) = post_multipart(
        &app,
        "/api/v1/general/rotate-pdf",
        &[
            upload_pdf("basic.pdf", &pdf),
            Part::Field {
                name: "angle",
                value: "45",
            },
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_problem_json(&headers, &body, 400);
}

#[tokio::test]
async fn rotate_pdf_requires_file() {
    let app = app();
    let (status, _, _) = post_multipart(&app, "/api/v1/general/rotate-pdf", &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn rotate_pdf_rejects_non_pdf_bytes() {
    let app = app();
    let (status, _, body) = post_multipart(
        &app,
        "/api/v1/general/rotate-pdf",
        &[upload_pdf("fake.pdf", b"this is not a pdf")],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["detail"].as_str().unwrap().contains("fake.pdf"));
}

#[tokio::test]
async fn merge_pdfs_produces_combined_pdf() {
    let app = app();
    let pdf = fixture("basic.pdf");
    let (status, _, body) = post_multipart(
        &app,
        "/api/v1/general/merge-pdfs",
        &[upload_pdf("a.pdf", &pdf), upload_pdf("b.pdf", &pdf)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let doc = lopdf::Document::load_mem(&body).expect("merged output must parse");
    assert_eq!(lopdf::Document::get_pages(&doc).len(), 2);
}

/// A one-page PDF carrying a real text-showing operation, built through
/// the same `pdfdoc::save` path uploads take. `basic.pdf` has no text
/// operators, so extraction on it legitimately returns an empty body.
fn pdf_with_text() -> Vec<u8> {
    use lopdf::{dictionary, Object};
    let mut pdf = crate::testkit::Pdf::new();
    let font = pdf.add(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });
    let content = pdf.add_stream(
        dictionary! {},
        b"BT /F1 24 Tf 100 700 Td (Hello Stirling) Tj ET",
    );
    pdf.edit_page(|page| {
        page.set("Contents", Object::Reference(content));
        page.set(
            "Resources",
            dictionary! {
                "Font" => dictionary! { "F1" => Object::Reference(font) },
            },
        );
    });
    pdf.bytes()
}

#[test]
fn debug_pdf_to_text_extraction() {
    let pdf = pdf_with_text();
    let doc = crate::pdfdoc::load("t.pdf", &pdf).unwrap();
    eprintln!("pages: {:?}", doc.get_pages());
    for (n, id) in doc.get_pages() {
        eprintln!("page {n} id={id:?}");
        eprintln!(
            "fonts: {:?}",
            doc.get_page_fonts(id)
                .map(|f| f.keys().cloned().collect::<Vec<_>>())
        );
        eprintln!(
            "content: {:?}",
            String::from_utf8_lossy(&doc.get_page_content(id)).into_owned()
        );
        eprintln!("extract: {:?}", doc.extract_text(&[n]));
    }
}

#[tokio::test]
async fn pdf_to_text_returns_plain_text() {
    let app = app();
    let pdf = pdf_with_text();
    let (status, headers, body) = post_multipart(
        &app,
        "/api/v1/convert/pdf/text",
        &[
            upload_pdf("text.pdf", &pdf),
            Part::Field {
                name: "outputFormat",
                value: "txt",
            },
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/plain")
    );
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        text.contains("Hello Stirling"),
        "extracted text missing expected content: {text:?}"
    );
}

#[tokio::test]
async fn unsupported_mode_returns_honest_400() {
    let app = app();
    let pdf = fixture("basic.pdf");
    let (status, headers, body) = post_multipart(
        &app,
        "/api/v1/security/add-watermark",
        &[
            upload_pdf("basic.pdf", &pdf),
            Part::Field {
                name: "watermarkType",
                value: "text",
            },
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_problem_json(&headers, &body, 400);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["detail"].as_str().unwrap().contains("not implemented"));
}

#[tokio::test]
async fn unknown_api_route_is_problem_json_404() {
    let app = app();
    let (status, headers, body) = send(
        &app,
        Method::GET,
        "/api/v1/general/definitely-not-a-route",
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_problem_json(&headers, &body, 404);
}

#[tokio::test]
async fn unknown_job_is_404() {
    let app = app();
    let (status, headers, body) = send(
        &app,
        Method::GET,
        "/api/v1/general/job/no-such-job",
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_problem_json(&headers, &body, 404);
}

#[tokio::test]
async fn async_job_lifecycle_returns_result_file() {
    let app = app();
    let pdf = fixture("basic.pdf");
    let (status, _, body) = post_multipart(
        &app,
        "/api/v1/general/rotate-pdf?async=true",
        &[upload_pdf("basic.pdf", &pdf)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["async"], true);
    let job_id = json["jobId"].as_str().unwrap().to_string();

    // The job runs on a blocking task; poll until complete.
    let mut record = serde_json::Value::Null;
    for _ in 0..100 {
        let (status, _, body) = send(
            &app,
            Method::GET,
            &format!("/api/v1/general/job/{job_id}"),
            vec![],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        record = serde_json::from_slice(&body).unwrap();
        if record["complete"] == true {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(record["complete"], true, "job never completed");
    assert!(record["error"].is_null(), "job errored: {record}");

    let files = record["allResultFiles"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    let file_id = files[0]["fileId"].as_str().unwrap();
    let (status, headers, body) = send(
        &app,
        Method::GET,
        &format!("/api/v1/general/files/{file_id}"),
        vec![],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/pdf")
    );
    assert!(body.starts_with(b"%PDF-"));
}

fn assert_problem_json(headers: &HeaderMap, body: &[u8], status: u16) {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        content_type.contains("application/problem+json")
            || content_type.contains("application/json"),
        "expected problem+json, got {content_type}"
    );
    let json: serde_json::Value = serde_json::from_slice(body).unwrap();
    assert_eq!(json["status"].as_u64(), Some(status as u64));
    assert!(json["title"].is_string());
    assert!(json["detail"].is_string());
}
