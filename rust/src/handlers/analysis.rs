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
    // Java's PDDocument.getVersion() is a float; the API returns a JSON number.
    let version = document.version.parse::<f64>().unwrap_or(1.7);
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
        // Java emits two decimals (595.28), not the raw f32 expansion.
        let round = |v: f64| (v * 100.0).round() / 100.0;
        dimensions.push(json!({ "width": round(width), "height": round(height) }));
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

/// `POST /api/v1/analysis/font-info` → fontCount + fonts. Lists the font
/// resource names of each page's `/Resources/Font` dictionary, like Java's
/// `PDResources.getFontNames()`.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit;
    use lopdf::{dictionary, Dictionary, Object};

    fn upload(name: &str, bytes: Vec<u8>) -> Vec<Upload> {
        vec![Upload {
            name: name.into(),
            bytes: crate::upload::UploadBytes::from_vec(bytes),
            content_type: None,
        }]
    }

    fn state() -> AppState {
        AppState::new()
    }

    #[test]
    fn page_count_reports_document_pages() {
        let mut pdf = testkit::Pdf::new();
        pdf.add_page([0, 0, 300, 400], Dictionary::new());
        let result = page_count_work(
            state(),
            HashMap::new(),
            upload("in.pdf", pdf.bytes()),
            vec![],
        )
        .unwrap();
        assert_eq!(result.0["pageCount"], 2);
    }

    #[test]
    fn basic_info_includes_version_and_size() {
        let bytes = testkit::Pdf::new().bytes();
        let size = bytes.len();
        let result =
            basic_info_work(state(), HashMap::new(), upload("in.pdf", bytes), vec![]).unwrap();
        assert_eq!(result.0["pageCount"], 1);
        assert_eq!(result.0["pdfVersion"], 1.7);
        assert_eq!(result.0["fileSize"], size as u64);
    }

    #[test]
    fn page_dimensions_uses_mediabox_in_points() {
        let result = page_dimensions_work(
            state(),
            HashMap::new(),
            upload("in.pdf", testkit::Pdf::new().bytes()),
            vec![],
        )
        .unwrap();
        let dims = result.0.as_array().unwrap();
        assert_eq!(dims.len(), 1);
        assert_eq!(dims[0]["width"], 612.0);
        assert_eq!(dims[0]["height"], 792.0);
    }

    #[test]
    fn document_properties_reads_info_dict() {
        let mut pdf = testkit::Pdf::new();
        pdf.set_info(dictionary! {
            "Title" => Object::string_literal("My Doc"),
            "Author" => Object::string_literal("Agent"),
        });
        let result = document_properties_work(
            state(),
            HashMap::new(),
            upload("in.pdf", pdf.bytes()),
            vec![],
        )
        .unwrap();
        assert_eq!(result.0["title"], "My Doc");
        assert_eq!(result.0["author"], "Agent");
        // Absent keys are JSON null, not missing.
        assert!(result.0["subject"].is_null());
        assert!(result.0.get("keywords").is_some());
    }

    #[test]
    fn form_fields_counts_widgets() {
        let mut pdf = testkit::Pdf::new();
        let widget = pdf.add(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Widget",
            "FT" => "Tx",
            "T" => Object::string_literal("field1"),
            "Rect" => Object::Array(vec![0.into(), 0.into(), 100.into(), 20.into()]),
        });
        pdf.edit_page(|page| {
            page.set("Annots", Object::Array(vec![Object::Reference(widget)]));
        });
        let result = form_fields_work(
            state(),
            HashMap::new(),
            upload("form.pdf", pdf.bytes()),
            vec![],
        )
        .unwrap();
        assert_eq!(result.0["fieldCount"], 1);
        assert_eq!(result.0["hasXFA"], false);
        assert_eq!(result.0["isSignaturesExist"], false);
    }

    #[test]
    fn annotation_info_breaks_down_by_subtype() {
        let mut pdf = testkit::Pdf::new();
        for subtype in ["Text", "Text", "Link"] {
            let id = pdf.add(dictionary! {
                "Type" => "Annot",
                "Subtype" => subtype,
                "Rect" => Object::Array(vec![0.into(), 0.into(), 10.into(), 10.into()]),
            });
            pdf.edit_page(|page| {
                let mut annots = page
                    .get(b"Annots")
                    .ok()
                    .and_then(|o| match o {
                        Object::Array(a) => Some(a.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                annots.push(Object::Reference(id));
                page.set("Annots", Object::Array(annots));
            });
        }
        let result = annotation_info_work(
            state(),
            HashMap::new(),
            upload("in.pdf", pdf.bytes()),
            vec![],
        )
        .unwrap();
        assert_eq!(result.0["totalCount"], 3);
        assert_eq!(result.0["typeBreakdown"]["Text"], 2);
        assert_eq!(result.0["typeBreakdown"]["Link"], 1);
    }

    #[test]
    fn font_info_lists_resource_font_names() {
        let mut pdf = testkit::Pdf::new();
        pdf.edit_page(|page| {
            page.set(
                "Resources",
                dictionary! {
                    "Font" => dictionary! {
                        "F1" => dictionary! {
                            "Type" => "Font",
                            "Subtype" => "Type1",
                            "BaseFont" => "Helvetica",
                        },
                    },
                },
            );
        });
        let result = font_info_work(
            state(),
            HashMap::new(),
            upload("in.pdf", pdf.bytes()),
            vec![],
        )
        .unwrap();
        assert_eq!(result.0["fontCount"], 1);
        assert_eq!(result.0["fonts"], serde_json::json!(["F1"]));
    }

    #[test]
    fn security_info_reports_unencrypted() {
        let result = security_info_work(
            state(),
            HashMap::new(),
            upload("in.pdf", testkit::Pdf::new().bytes()),
            vec![],
        )
        .unwrap();
        assert_eq!(result.0["isEncrypted"], false);
    }

    #[test]
    fn missing_upload_is_a_bad_request() {
        let err = page_count_work(state(), HashMap::new(), vec![], vec![]).unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn malformed_pdf_is_an_error_not_a_panic() {
        let err = page_count_work(
            state(),
            HashMap::new(),
            upload("bad.pdf", b"not a pdf".to_vec()),
            vec![],
        )
        .unwrap_err();
        assert!(matches!(err, AppError::Pdf(_) | AppError::BadRequest(_)));
    }
}
