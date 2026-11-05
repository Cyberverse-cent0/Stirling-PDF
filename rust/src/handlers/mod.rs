pub mod analysis;
pub mod general;
pub mod jobs;
pub mod merge;
pub mod misc;
pub mod proxy;
pub mod split;

use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::json;

use crate::handlers::jobs::AppState;

/// Explicit routes win over the `/api/**` fallback proxy, so migrated
/// endpoints are served by Rust while everything else reaches Java.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/v1/info/version", get(version))
        .route("/api/v1/ping", post(ping))
        .route("/api/v1/general/merge-pdfs", post(merge::merge_pdfs))
        .route("/api/v1/general/rotate-pdf", post(general::rotate_pdf))
        .route("/api/v1/general/remove-pages", post(general::remove_pages))
        .route(
            "/api/v1/general/rearrange-pages",
            post(general::rearrange_pages),
        )
        .route("/api/v1/general/split-pages", post(split::split_pages))
        .route(
            "/api/v1/general/remove-image-pdf",
            post(general::remove_images),
        )
        .route(
            "/api/v1/general/split-by-size-or-count",
            post(split::split_by_size_or_count),
        )
        .route(
            "/api/v1/general/split-pdf-by-chapters",
            post(split::split_by_chapters),
        )
        .route("/api/v1/misc/decompress-pdf", post(misc::decompress_pdf))
        .route("/api/v1/misc/update-metadata", post(misc::update_metadata))
        .route("/api/v1/misc/show-javascript", post(misc::show_javascript))
        .route("/api/v1/analysis/page-count", post(analysis::page_count))
        .route("/api/v1/analysis/basic-info", post(analysis::basic_info))
        .route(
            "/api/v1/analysis/page-dimensions",
            post(analysis::page_dimensions),
        )
        .route(
            "/api/v1/analysis/document-properties",
            post(analysis::document_properties),
        )
        .route("/api/v1/analysis/form-fields", post(analysis::form_fields))
        .route(
            "/api/v1/analysis/annotation-info",
            post(analysis::annotation_info),
        )
        .route("/api/v1/analysis/font-info", post(analysis::font_info))
        .route(
            "/api/v1/analysis/security-info",
            post(analysis::security_info),
        )
        .route("/api/v1/general/job/{jobId}", get(jobs::job_status))
        .route("/api/v1/general/job/{jobId}/result", get(jobs::job_result))
        .route(
            "/api/v1/general/job/{jobId}/result/files",
            get(jobs::job_files),
        )
        .route("/api/v1/general/job/{jobId}", delete(jobs::cancel_job))
        .route(
            "/api/v1/general/files/{fileId}/metadata",
            get(jobs::file_metadata),
        )
        .route("/api/v1/general/files/{fileId}", get(jobs::download_file))
        .route("/api/v1/general/jobs/cleanup", post(jobs::cleanup_jobs))
        .fallback(proxy::proxy)
        .with_state(state)
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}

async fn version() -> Json<serde_json::Value> {
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "runtime": "rust",
    }))
}

async fn ping(Json(body): Json<serde_json::Value>) -> Json<serde_json::Value> {
    Json(json!({ "echo": body }))
}
