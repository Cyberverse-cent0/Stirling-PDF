//! Ports of `AnalysisController` (`/api/v1/analysis/*`): read-only PDF
//! inspection endpoints that answer JSON instead of files. All run on the
//! `lopdf` object model via `pdfdoc`.

use std::collections::HashMap;

use axum::Json;
use serde_json::{json, Map, Value};

use crate::error::{AppError, AppResult};
use crate::handlers::jobs::AppState;
use crate::pdfdoc;
use crate::upload::{Upload, FILE_FIELD};

fn load_pdf(uploads: Vec<Upload>) -> AppResult<lopdf::Document> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    pdfdoc::load(&upload.name, &upload.bytes)
}

/// `POST /api/v1/analysis/page-count` → `{"pageCount": n}`.
#[stirling_macros::endpoint]
fn page_count(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    _params: Vec<(String, String)>,
) -> AppResult<Json<Value>> {
    let document = load_pdf(uploads)?;
    Ok(Json(
        json!({ "pageCount": pdfdoc::page_ids(&document).len() }),
    ))
}

/// `POST /api/v1/analysis/basic-info` → pageCount, pdfVersion, fileSize.
#[stirling_macros::endpoint]
fn basic_info(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    _params: Vec<(String, String)>,
) -> AppResult<Json<Value>> {
    let file_size = uploads.first().map(|u| u.bytes.len()).unwrap_or(0);
    let document = load_pdf(uploads)?;
    let version = if document.version.is_empty() {
        "1.7".to_string()
    } else {
        document.version.clone()
    };
    Ok(Json(json!({
        "pageCount": pdfdoc::page_ids(&document).len(),
        "pdfVersion": version,
        "fileSize": file_size,
    })))
}

/// `POST /api/v1/analysis/page-dimensions` → `[{width, height}]` per page, in
/// points (CropBox preferred, MediaBox fallback, rotation applied).
#[stirling_macros::endpoint]
fn page_dimensions(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    _params: Vec<(String, String)>,
) -> AppResult<Json<Value>> {
    let document = load_pdf(uploads)?;
    let mut dimensions = Vec::new();
    for page_id in pdfdoc::page_ids(&document) {
        let (width, height) = pdfdoc::page_dimensions(&document, page_id)?;
        dimensions.push(json!({ "width": width, "height": height }));
    }
    Ok(Json(Value::Array(dimensions)))
}

/// `POST /api/v1/analysis/document-properties` → the Info-dictionary fields
/// Java surfaces, with `null` for absent values. Date fields keep the raw
/// PDF `D:` string, matching the previous backend's documented divergence.
#[stirling_macros::endpoint]
fn document_properties(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    _params: Vec<(String, String)>,
) -> AppResult<Json<Value>> {
    let document = load_pdf(uploads)?;
    let tag = |key: &str| {
        pdfdoc::info_value(&document, key)
            .map(Value::String)
            .unwrap_or(Value::Null)
    };

    let mut properties = Map::new();
    for (name, key) in [
        ("title", "Title"),
        ("author", "Author"),
        ("subject", "Subject"),
        ("keywords", "Keywords"),
        ("creator", "Creator"),
        ("producer", "Producer"),
        ("creationDate", "CreationDate"),
        ("modificationDate", "ModDate"),
    ] {
        properties.insert(name.to_string(), tag(key));
    }
    Ok(Json(Value::Object(properties)))
}

/// `POST /api/v1/analysis/form-fields` → fieldCount, hasXFA,
/// isSignaturesExist. `fieldCount` counts `/Widget` annotations (the
/// lopdf-level equivalent of Java's field count for typical forms).
#[stirling_macros::endpoint]
fn form_fields(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    _params: Vec<(String, String)>,
) -> AppResult<Json<Value>> {
    let document = load_pdf(uploads)?;

    let mut field_count = 0usize;
    for page_id in pdfdoc::page_ids(&document) {
        field_count += pdfdoc::annotations(&document, page_id)?
            .iter()
            .filter(|a| pdfdoc::annotation_subtype(a) == "Widget")
            .count();
    }

    Ok(Json(json!({
        "fieldCount": field_count,
        "hasXFA": pdfdoc::has_xfa(&document),
        "isSignaturesExist": pdfdoc::signature_field_exists(&document)?,
    })))
}

/// `POST /api/v1/analysis/annotation-info` → totalCount + typeBreakdown,
/// keyed by the PDF `/Subtype` name like Java's `PDAnnotation.getSubtype()`.
#[stirling_macros::endpoint]
fn annotation_info(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    _params: Vec<(String, String)>,
) -> AppResult<Json<Value>> {
    let document = load_pdf(uploads)?;

    let mut total = 0usize;
    let mut breakdown: HashMap<String, usize> = HashMap::new();
    for page_id in pdfdoc::page_ids(&document) {
        for annotation in pdfdoc::annotations(&document, page_id)? {
            total += 1;
            *breakdown
                .entry(pdfdoc::annotation_subtype(&annotation))
                .or_insert(0) += 1;
        }
    }

    Ok(Json(json!({
        "totalCount": total,
        "typeBreakdown": breakdown,
    })))
}

/// `POST /api/v1/analysis/font-info` → fontCount + fonts. Lists the base
/// font names referenced by each page's resources (Java lists resource
/// keys; names are the lopdf-level equivalent).
#[stirling_macros::endpoint]
fn font_info(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    _params: Vec<(String, String)>,
) -> AppResult<Json<Value>> {
    let document = load_pdf(uploads)?;

    let mut fonts = std::collections::BTreeSet::new();
    for page_id in pdfdoc::page_ids(&document) {
        for name in pdfdoc::page_fonts(&document, page_id)? {
            if !name.is_empty() {
                fonts.insert(name);
            }
        }
    }

    Ok(Json(json!({
        "fontCount": fonts.len(),
        "fonts": Value::Array(fonts.into_iter().map(Value::String).collect()),
    })))
}

/// `POST /api/v1/analysis/security-info` → isEncrypted + permissions from
/// the `/Encrypt` dictionary's `/P` bits (PDF spec bits 3-6).
#[stirling_macros::endpoint]
fn security_info(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    _params: Vec<(String, String)>,
) -> AppResult<Json<Value>> {
    let document = load_pdf(uploads)?;
    if !pdfdoc::is_encrypted(&document) {
        return Ok(Json(json!({ "isEncrypted": false })));
    }
    let permissions = pdfdoc::encrypt_permissions(&document).unwrap_or(0);
    Ok(Json(json!({
        "isEncrypted": true,
        "permissions": {
            "preventPrinting": permissions & 0b100 == 0,
            "preventModify": permissions & 0b1000 == 0,
            "preventExtractContent": permissions & 0b10000 == 0,
            "preventModifyAnnotations": permissions & 0b100000 == 0,
        },
    })))
}
