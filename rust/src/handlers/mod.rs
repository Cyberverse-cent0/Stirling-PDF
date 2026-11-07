pub mod analysis;
pub mod automation;
pub mod convert;
pub mod eml;
pub mod filter;
pub mod form;
pub mod general;
pub mod image_scans;
pub mod jobs;
pub mod layout;
pub mod merge;
pub mod misc;
pub mod ocr;
pub mod passwords;
pub mod pipeline;
pub mod platform;
pub mod scanner;
pub mod security;
pub mod split;
pub mod transform;
pub mod visual;

use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::json;

use crate::handlers::jobs::AppState;

/// Explicit routes win over the fallback, which 404s unmigrated `/api/**`
/// paths and serves the SPA/static surface for everything else.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/robots.txt", get(crate::web::robots_txt))
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
        .route(
            "/api/v1/general/split-pdf-by-sections",
            post(split::split_by_sections),
        )
        .route(
            "/api/v1/general/split-for-poster-print",
            post(split::poster_pdf),
        )
        .route("/api/v1/general/overlay-pdfs", post(general::overlay_pdfs))
        .route(
            "/api/v1/general/edit-table-of-contents",
            post(general::edit_table_of_contents),
        )
        .route(
            "/api/v1/general/extract-bookmarks",
            post(general::extract_bookmarks),
        )
        .route("/api/v1/general/scale-pages", post(transform::scale_pages))
        .route("/api/v1/general/crop", post(transform::crop_pdf))
        .route(
            "/api/v1/general/pdf-to-single-page",
            post(general::pdf_to_single_page),
        )
        .route(
            "/api/v1/general/multi-page-layout",
            post(layout::multi_page_layout),
        )
        .route(
            "/api/v1/general/booklet-imposition",
            post(layout::booklet_imposition),
        )
        .route("/api/v1/misc/decompress-pdf", post(misc::decompress_pdf))
        .route("/api/v1/misc/update-metadata", post(misc::update_metadata))
        .route("/api/v1/misc/flatten", post(misc::flatten))
        .route("/api/v1/misc/add-attachments", post(misc::add_attachments))
        .route("/api/v1/misc/extract-images", post(misc::extract_images))
        .route("/api/v1/misc/compress-pdf", post(misc::compress_pdf))
        .route("/api/v1/misc/show-javascript", post(misc::show_javascript))
        .route(
            "/api/v1/misc/list-attachments",
            post(misc::list_attachments),
        )
        .route(
            "/api/v1/misc/extract-single-attachment",
            post(misc::extract_single_attachment),
        )
        .route(
            "/api/v1/misc/extract-attachments",
            post(misc::extract_attachments),
        )
        .route(
            "/api/v1/misc/rename-attachment",
            post(misc::rename_attachment),
        )
        .route(
            "/api/v1/misc/delete-attachment",
            post(misc::delete_attachment),
        )
        .route(
            "/api/v1/misc/batch-process-attachments",
            post(misc::batch_process_attachments),
        )
        .route(
            "/api/v1/misc/create-portfolio",
            post(misc::create_portfolio),
        )
        .route(
            "/api/v1/misc/flatten-portfolio",
            post(misc::flatten_portfolio),
        )
        .route(
            "/api/v1/misc/unlock-pdf-forms",
            post(misc::unlock_pdf_forms),
        )
        .route(
            "/api/v1/misc/add-page-numbers",
            post(misc::add_page_numbers),
        )
        .route(
            "/api/v1/misc/replace-invert-pdf",
            post(misc::replace_invert_pdf),
        )
        .route("/api/v1/misc/add-image", post(misc::add_image))
        .route("/api/v1/misc/add-comments", post(misc::add_comments))
        .route("/api/v1/misc/remove-blanks", post(misc::remove_blanks))
        .route("/api/v1/misc/ocr-pdf", post(ocr::ocr_pdf))
        .route(
            "/api/v1/misc/extract-image-scans",
            post(image_scans::extract_image_scans),
        )
        .route("/api/v1/misc/scanner-effect", post(scanner::scanner_effect))
        .route(
            "/api/v1/misc/auto-split-pdf",
            post(automation::auto_split_pdf),
        )
        .route("/api/v1/misc/add-stamp", post(visual::add_stamp))
        .route("/api/v1/misc/auto-rename", post(automation::auto_rename))
        .route(
            "/api/v1/misc/auto-rotate-pdf",
            post(automation::auto_rotate_pdf),
        )
        .route("/api/v1/misc/repair", post(automation::repair))
        .route(
            "/api/v1/mobile-scanner/create-session/{sessionId}",
            post(automation::scanner_create_session),
        )
        .route(
            "/api/v1/mobile-scanner/validate-session/{sessionId}",
            get(automation::scanner_validate_session),
        )
        .route(
            "/api/v1/mobile-scanner/upload/{sessionId}",
            post(automation::scanner_upload_files),
        )
        .route(
            "/api/v1/mobile-scanner/files/{sessionId}",
            get(automation::scanner_session_files),
        )
        .route(
            "/api/v1/mobile-scanner/download/{sessionId}/{filename}",
            get(automation::scanner_download_file),
        )
        .route(
            "/api/v1/mobile-scanner/session/{sessionId}",
            delete(automation::scanner_delete_session),
        )
        .route(
            "/api/v1/security/sanitize-pdf",
            post(security::sanitize_pdf),
        )
        .route(
            "/api/v1/security/get-info-on-pdf",
            post(security::get_info_on_pdf),
        )
        .route(
            "/api/v1/security/remove-cert-sign",
            post(security::remove_cert_sign),
        )
        .route("/api/v1/security/redact", post(security::redact))
        .route("/api/v1/security/verify-pdf", post(security::verify_pdf))
        .route(
            "/api/v1/security/validate-compliance",
            post(security::validate_compliance),
        )
        .route(
            "/api/v1/security/accessibility-report",
            post(security::accessibility_report),
        )
        .route(
            "/api/v1/security/cert-sign",
            post(crate::certsign::cert_sign),
        )
        .route(
            "/api/v1/security/validate-signature",
            post(crate::signature::validate_signature),
        )
        .route(
            "/api/v1/security/timestamp-pdf",
            post(crate::timestamp::timestamp_pdf),
        )
        .route("/api/v1/security/auto-redact", post(security::auto_redact))
        .route(
            "/api/v1/security/redact-execute",
            post(security::redact_execute),
        )
        .route(
            "/api/v1/security/add-watermark",
            post(visual::add_watermark),
        )
        .route(
            "/api/v1/security/add-password",
            post(passwords::add_password),
        )
        .route(
            "/api/v1/security/remove-password",
            post(passwords::remove_password),
        )
        .route(
            "/api/v1/security/restore-pdf-protection",
            post(passwords::restore_pdf_protection),
        )
        .route(
            "/api/v1/security/prepare-pdf-session",
            post(passwords::prepare_pdf_session),
        )
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
        .route(
            "/api/v1/filter/filter-contains-text",
            post(filter::contains_text),
        )
        .route(
            "/api/v1/filter/filter-contains-image",
            post(filter::contains_image),
        )
        .route("/api/v1/filter/filter-page-count", post(filter::page_count))
        .route("/api/v1/filter/filter-page-size", post(filter::page_size))
        .route("/api/v1/filter/filter-file-size", post(filter::file_size))
        .route(
            "/api/v1/filter/filter-page-rotation",
            post(filter::page_rotation),
        )
        .route("/api/v1/form/fields", post(form::list_fields))
        .route(
            "/api/v1/form/fields-with-coordinates",
            post(form::list_fields_with_coordinates),
        )
        .route("/api/v1/form/extract-csv", post(form::extract_csv))
        .route("/api/v1/form/extract-xlsx", post(form::extract_xlsx))
        .route("/api/v1/form/fill", post(form::fill_form))
        .route("/api/v1/form/add-fields", post(form::add_fields_endpoint))
        .route("/api/v1/form/edit-fields", post(form::edit_fields_endpoint))
        .route(
            "/api/v1/form/modify-fields",
            post(form::modify_fields_endpoint),
        )
        .route(
            "/api/v1/form/delete-fields",
            post(form::delete_fields_endpoint),
        )
        .route("/api/v1/convert/img/pdf", post(convert::img_pdf))
        .route("/api/v1/convert/cbz/pdf", post(convert::cbz_pdf))
        .route("/api/v1/convert/cbr/pdf", post(convert::cbr_pdf))
        .route("/api/v1/convert/ebook/pdf", post(convert::ebook_to_pdf))
        .route("/api/v1/convert/pdf/cbz", post(convert::pdf_to_cbz))
        .route("/api/v1/convert/pdf/cbr", post(convert::pdf_to_cbr))
        .route("/api/v1/convert/pdf/epub", post(convert::pdf_to_epub))
        .route("/api/v1/convert/pdf/img", post(convert::pdf_to_img))
        .route("/api/v1/convert/svg/pdf", post(convert::svg_to_pdf))
        .route("/api/v1/convert/file/pdf", post(convert::office_to_pdf))
        .route("/api/v1/convert/pdf/word", post(convert::pdf_to_word))
        .route(
            "/api/v1/convert/pdf/presentation",
            post(convert::pdf_to_presentation),
        )
        .route("/api/v1/convert/pdf/xml", post(convert::pdf_to_xml))
        .route("/api/v1/convert/pdf/text", post(convert::pdf_to_text))
        .route("/api/v1/convert/pdf/csv", post(convert::pdf_to_csv))
        .route("/api/v1/convert/pdf/xlsx", post(convert::pdf_to_xlsx))
        .route(
            "/api/v1/convert/pdf/markdown",
            post(convert::pdf_to_markdown),
        )
        .route("/api/v1/convert/html/pdf", post(convert::html_to_pdf))
        .route(
            "/api/v1/convert/markdown/pdf",
            post(convert::markdown_to_pdf),
        )
        .route("/api/v1/convert/url/pdf", post(convert::url_to_pdf))
        .route("/api/v1/convert/eml/pdf", post(eml::eml_to_pdf))
        .route("/api/v1/convert/pdf/html", post(convert::pdf_to_html))
        .route("/api/v1/convert/pdf/pdfa", post(convert::pdf_to_pdfa))
        .route("/api/v1/convert/vector/pdf", post(convert::vector_to_pdf))
        .route("/api/v1/convert/pdf/vector", post(convert::pdf_to_vector))
        .merge(crate::metrics::routes())
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
        .route("/api/v1/config/app-config", get(platform::app_config))
        .route(
            "/api/v1/config/login-disclaimer",
            get(platform::login_disclaimer),
        )
        .route(
            "/api/v1/config/endpoint-enabled",
            get(platform::endpoint_enabled),
        )
        .route(
            "/api/v1/config/endpoints-enabled",
            get(platform::endpoints_enabled),
        )
        .route(
            "/api/v1/config/endpoints-availability",
            get(platform::endpoints_availability),
        )
        .route("/api/v1/config/group-enabled", get(platform::group_enabled))
        .route("/api/v1/ui-data/home", get(platform::ui_data_home))
        .route("/api/v1/ui-data/licenses", get(platform::ui_data_licenses))
        .route("/api/v1/ui-data/ocr-pdf", get(platform::ui_data_ocr_pdf))
        .route(
            "/api/v1/ui-data/footer-info",
            get(platform::ui_data_footer_info),
        )
        .route("/api/v1/ui-data/pipeline", get(platform::ui_data_pipeline))
        .route("/api/v1/ui-data/sign", get(platform::ui_data_sign))
        .route(
            "/api/v1/general/signatures/{fileName}",
            get(platform::signature_image),
        )
        .route(
            "/js/additionalLanguageCode.js",
            get(platform::additional_language_js),
        )
        .route("/api/v1/pipeline/handleData", post(pipeline::handle_data))
        .route(
            "/api/v1/settings/get-endpoints-status",
            get(platform::get_endpoints_status),
        )
        .route(
            "/api/v1/settings/update-enable-analytics",
            post(platform::update_enable_analytics),
        )
        .fallback(fallback)
        .layer(axum::middleware::from_fn(crate::guard::api_guard))
        .layer(axum::middleware::from_fn(crate::auth::auth_guard))
        .layer(axum::middleware::from_fn(crate::metrics::track))
        .with_state(state)
}

/// Unmigrated `/api/**` routes 404; every other path is the SPA/static
/// surface owned by `web::serve`.
async fn fallback(req: axum::extract::Request) -> axum::response::Response {
    if req.uri().path().starts_with("/api/") {
        return crate::error::AppError::NotFound("not found".into()).into_response();
    }
    crate::web::serve(req).await
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
