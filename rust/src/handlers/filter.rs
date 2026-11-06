//! Ports of `FilterController` (`/api/v1/filter/*`): pass/fail gates over a
//! single uploaded PDF. A match answers 200 — the untouched upload for the
//! comparison filters, a re-saved copy for the content filters, like
//! `pdfDocToWebResponse` vs `multiPartFileToWebResponse` — and a non-match
//! answers 204, like Java's `ResponseEntity.noContent()`.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use axum::body::Body;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;
use lopdf::{Document, Object, ObjectId};

use crate::error::{AppError, AppResult};
use crate::handlers::jobs::AppState;
use crate::handlers::merge::{file_response, pdf_response};
use crate::page_select;
use crate::pdfdoc;
use crate::text;
use crate::upload::{first_upload, simple_name, Upload};

/// `@ModelAttribute` binding reads the query string before multipart
/// fields; within the fields the last occurrence wins, like `upload::param`.
fn field<'a>(
    query: &'a HashMap<String, String>,
    params: &'a [(String, String)],
    name: &str,
) -> Option<&'a str> {
    query.get(name).map(String::as_str).or_else(|| {
        params
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    })
}

fn field_or<'a>(
    query: &'a HashMap<String, String>,
    params: &'a [(String, String)],
    name: &str,
    fallback: &'a str,
) -> &'a str {
    field(query, params, name).unwrap_or(fallback)
}

/// The filter-miss response. The JSON content type keeps `jobs::submit`
/// treating the empty body as an inline (null) result rather than a
/// zero-byte file, matching Java's 204 job completing with no file.
fn filter_miss() -> Response {
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::empty())
        .expect("static response parts")
}

/// `WebResponseUtils.multiPartFileToWebResponse`: the untouched upload with
/// the part's declared content type and bare filename.
fn pass_through(upload: &Upload) -> Response {
    let content_type = upload
        .content_type
        .as_deref()
        .filter(|value| HeaderValue::from_str(value).is_ok())
        .unwrap_or("application/pdf");
    file_response(
        upload.bytes.to_vec(),
        simple_name(&upload.name),
        content_type,
    )
}

/// `FilterController.compare`: Greater/Equal/Less on the already-computed
/// ordering. Anything else is Java's `INVALID_ARGUMENT` (HTTP 400).
fn compare(actual: Ordering, comparator: &str) -> AppResult<bool> {
    match comparator {
        "Greater" => Ok(actual == Ordering::Greater),
        "Equal" => Ok(actual == Ordering::Equal),
        "Less" => Ok(actual == Ordering::Less),
        _ => Err(AppError::BadRequest(format!(
            "Invalid argument: comparator '{comparator}'"
        ))),
    }
}

fn parse_i32(raw: &str, name: &str) -> AppResult<i32> {
    raw.trim()
        .parse::<i32>()
        .map_err(|_| AppError::BadRequest(format!("Invalid argument: {name} '{raw}'")))
}

fn parse_i64(raw: &str, name: &str) -> AppResult<i64> {
    raw.trim()
        .parse::<i64>()
        .map_err(|_| AppError::BadRequest(format!("Invalid argument: {name} '{raw}'")))
}

/// Page positions selected by `pageNumbers`, like `PdfUtils.hasText` /
/// `hasImages`: blank means "all", malformed selections are rejected, and a
/// selection matching no page is allowed (it simply finds nothing).
fn selected_pages(document: &Document, selection: &str) -> AppResult<Vec<usize>> {
    let total = pdfdoc::page_ids(document).len();
    let selection = if selection.trim().is_empty() {
        "all"
    } else {
        selection
    };
    page_select::require_well_formed_page_selection(selection, total)
        .map_err(AppError::BadRequest)?;
    page_select::parse_page_list(selection, total).map_err(AppError::BadRequest)
}

/// `POST /api/v1/filter/filter-contains-text` — 200 with a re-saved copy of
/// the PDF when any selected page's text contains `text`, else 204.
///
/// Text comes from `text::page_items`, the positioned extractor shared with
/// the automation endpoints: it decodes through each font's `Encoding`
/// (including ToUnicode CMaps) and descends into form XObjects like
/// PDFBox's stripper. Runs are joined with a space — PDFBox inserts
/// whitespace only where glyph positions warrant it, so multi-word phrases
/// spanning runs can still disagree.
#[stirling_macros::endpoint]
fn contains_text(
    _state: AppState,
    query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let upload = first_upload(uploads)?;
    let Some(needle) = field(&query, &params, "text") else {
        return Err(AppError::BadRequest("text is required".into()));
    };
    let page_numbers = field_or(&query, &params, "pageNumbers", "all");

    let mut document = pdfdoc::load(&upload.name, &upload.bytes)?;
    let page_ids = pdfdoc::page_ids(&document);
    for index in selected_pages(&document, page_numbers)? {
        let Some(&page_id) = page_ids.get(index) else {
            continue;
        };
        let page_text = text::page_items(&document, page_id)
            .iter()
            .map(|item| item.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        if page_text.contains(needle) {
            pdfdoc::touch_loaded_info(&mut document);
            let bytes = pdfdoc::save(&mut document)?;
            return Ok(pdf_response(bytes, simple_name(&upload.name)));
        }
    }
    Ok(filter_miss())
}

/// `POST /api/v1/filter/filter-contains-image` — 200 with a re-saved copy
/// when any selected page draws an image XObject, directly or through a
/// form XObject, else 204.
#[stirling_macros::endpoint]
fn contains_image(
    _state: AppState,
    query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let upload = first_upload(uploads)?;
    let page_numbers = field_or(&query, &params, "pageNumbers", "all");

    let mut document = pdfdoc::load(&upload.name, &upload.bytes)?;
    let page_ids = pdfdoc::page_ids(&document);
    for index in selected_pages(&document, page_numbers)? {
        if let Some(&page_id) = page_ids.get(index) {
            if page_has_image(&document, page_id)? {
                pdfdoc::touch_loaded_info(&mut document);
                let bytes = pdfdoc::save(&mut document)?;
                return Ok(pdf_response(bytes, simple_name(&upload.name)));
            }
        }
    }
    Ok(filter_miss())
}

/// `PdfUtils.containsImage`: walks the page's `/Resources` XObjects,
/// recursing into `/Subtype /Form` XObjects. `visited` plays the role of
/// Java's identity set on `COSDictionary`, deduping shared indirect
/// resource dictionaries and form XObjects so cyclic graphs terminate.
fn resources_have_image(
    document: &Document,
    resources: &Object,
    visited: &mut HashSet<ObjectId>,
) -> AppResult<bool> {
    if let Object::Reference(id) = resources {
        if !visited.insert(*id) {
            return Ok(false);
        }
    }
    let Some(resources_obj) = pdfdoc::resolve_owned(document, resources) else {
        return Ok(false);
    };
    let Some(xobjects) =
        pdfdoc::dict_of(&resources_obj).and_then(|dict| dict.get(b"XObject").ok().cloned())
    else {
        return Ok(false);
    };
    let Some(xobjects_obj) = pdfdoc::resolve_owned(document, &xobjects) else {
        return Ok(false);
    };
    let Some(xobjects_dict) = pdfdoc::dict_of(&xobjects_obj) else {
        return Ok(false);
    };

    for (_name, entry) in xobjects_dict.iter() {
        let Some(entry_dict) = pdfdoc::peek_dict(document, entry) else {
            continue;
        };
        let subtype = entry_dict
            .get(b"Subtype")
            .ok()
            .and_then(|object| object.as_name().ok())
            .map(<[u8]>::to_vec);
        match subtype.as_deref() {
            Some(b"Image") => return Ok(true),
            Some(b"Form") => {
                if let Object::Reference(id) = entry {
                    if !visited.insert(*id) {
                        continue;
                    }
                }
                let Some(form_resources) = entry_dict.get(b"Resources").ok().cloned() else {
                    continue;
                };
                if resources_have_image(document, &form_resources, visited)? {
                    return Ok(true);
                }
            }
            _ => {}
        }
    }
    Ok(false)
}

fn page_has_image(document: &Document, page_id: ObjectId) -> AppResult<bool> {
    let Some((_, resources)) = pdfdoc::page_resources(document, page_id)? else {
        return Ok(false);
    };
    let mut visited = HashSet::new();
    resources_have_image(document, &resources, &mut visited)
}

/// `POST /api/v1/filter/filter-page-count` — compares the page count
/// against `pageCount` with `comparator`; the untouched upload on a pass.
#[stirling_macros::endpoint]
fn page_count(
    _state: AppState,
    query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let upload = first_upload(uploads)?;
    let expected = parse_i32(field_or(&query, &params, "pageCount", "0"), "pageCount")? as i64;
    let comparator = field_or(&query, &params, "comparator", "");

    let document = pdfdoc::load(&upload.name, &upload.bytes)?;
    let actual = pdfdoc::page_ids(&document).len() as i64;
    if compare(actual.cmp(&expected), comparator)? {
        Ok(pass_through(&upload))
    } else {
        Ok(filter_miss())
    }
}

/// `POST /api/v1/filter/filter-page-size` — compares the first page's
/// `/MediaBox` area (points²) against the named standard size.
///
/// Divergence: PDFBox's `PDPage.getMediaBox()` falls back to US Letter
/// when the page has no usable MediaBox; [`pdfdoc::media_box`] errors, so
/// the fallback is applied here on any resolution failure.
#[stirling_macros::endpoint]
fn page_size(
    _state: AppState,
    query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let upload = first_upload(uploads)?;
    let size_name = field_or(&query, &params, "standardPageSize", "A4");
    let comparator = field_or(&query, &params, "comparator", "");
    let Some((standard_w, standard_h)) = standard_page_size(size_name) else {
        return Err(AppError::BadRequest(format!(
            "Invalid page size: {size_name}"
        )));
    };

    let document = pdfdoc::load(&upload.name, &upload.bytes)?;
    let Some(&first_page) = pdfdoc::page_ids(&document).first() else {
        return Err(AppError::Pdf("document has no pages".into()));
    };
    let bbox = pdfdoc::media_box(&document, first_page).unwrap_or([0.0, 0.0, 612.0, 792.0]);
    let actual_area = (bbox[2] as f32 - bbox[0] as f32) * (bbox[3] as f32 - bbox[1] as f32);
    let standard_area = standard_w * standard_h;

    if compare(actual_area.total_cmp(&standard_area), comparator)? {
        Ok(pass_through(&upload))
    } else {
        Ok(filter_miss())
    }
}

/// `POST /api/v1/filter/filter-file-size` — compares the upload's byte
/// length against `fileSize`. Java never opens the document here, so a
/// non-PDF can pass this filter; the same holds here.
#[stirling_macros::endpoint]
fn file_size(
    _state: AppState,
    query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let upload = first_upload(uploads)?;
    let expected = parse_i64(field_or(&query, &params, "fileSize", "0"), "fileSize")?;
    let comparator = field_or(&query, &params, "comparator", "");

    // `byte_len` reads the spool length without materializing the upload —
    // Java's `MultipartFile.getSize()` never touches the bytes either.
    let actual = upload.bytes.byte_len() as i64;
    if compare(actual.cmp(&expected), comparator)? {
        Ok(pass_through(&upload))
    } else {
        Ok(filter_miss())
    }
}

/// `POST /api/v1/filter/filter-page-rotation` — compares the first page's
/// `/Rotate` (page-tree inherited, normalised like `PDPage.getRotation`)
/// against `rotation`.
#[stirling_macros::endpoint]
fn page_rotation(
    _state: AppState,
    query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let upload = first_upload(uploads)?;
    let expected = parse_i32(field_or(&query, &params, "rotation", "0"), "rotation")? as i64;
    let comparator = field_or(&query, &params, "comparator", "");

    let document = pdfdoc::load(&upload.name, &upload.bytes)?;
    let Some(&first_page) = pdfdoc::page_ids(&document).first() else {
        return Err(AppError::Pdf("document has no pages".into()));
    };
    let actual = pdfdoc::page_rotation(&document, first_page)?;
    if compare(actual.cmp(&expected), comparator)? {
        Ok(pass_through(&upload))
    } else {
        Ok(filter_miss())
    }
}

/// PDFBox `PDRectangle` constants for the `textToPageSize` names, as
/// `(width, height)` in points computed with the same float arithmetic:
/// `POINTS_PER_MM = 1 / (10 * 2.54f) * 72` and LETTER/LEGAL in inches.
fn standard_page_size(name: &str) -> Option<(f32, f32)> {
    const POINTS_PER_MM: f32 = 1.0 / (10.0 * 2.54) * 72.0;
    let (w_mm, h_mm) = match name.to_ascii_uppercase().as_str() {
        "A0" => (841.0, 1189.0),
        "A1" => (594.0, 841.0),
        "A2" => (420.0, 594.0),
        "A3" => (297.0, 420.0),
        "A4" => (210.0, 297.0),
        "A5" => (148.0, 210.0),
        "A6" => (105.0, 148.0),
        "LETTER" => return Some((8.5 * 72.0, 11.0 * 72.0)),
        "LEGAL" => return Some((8.5 * 72.0, 14.0 * 72.0)),
        _ => return None,
    };
    Some((w_mm * POINTS_PER_MM, h_mm * POINTS_PER_MM))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comparator_matches_java_cases() {
        assert!(compare(Ordering::Greater, "Greater").unwrap());
        assert!(!compare(Ordering::Less, "Greater").unwrap());
        assert!(compare(Ordering::Equal, "Equal").unwrap());
        assert!(compare(Ordering::Less, "Less").unwrap());
        assert!(compare(Ordering::Greater, "less").is_err());
        assert!(compare(Ordering::Equal, "").is_err());
    }

    #[test]
    fn standard_sizes_match_pdfbox() {
        // PDRectangle.A4 = 210mm x 297mm at 72/25.4 points per mm.
        let (w, h) = standard_page_size("a4").unwrap();
        assert!((w - 595.2756).abs() < 0.01);
        assert!((h - 841.8898).abs() < 0.01);
        assert_eq!(standard_page_size("LETTER"), Some((612.0, 792.0)));
        assert_eq!(standard_page_size("LEGAL"), Some((612.0, 1008.0)));
        assert!(standard_page_size("TABLOID").is_none());
    }
}

#[cfg(test)]
mod endpoint_tests {
    use super::*;
    use crate::testkit;
    use lopdf::{dictionary, Stream};

    fn state() -> AppState {
        AppState::new()
    }

    fn upload(name: &str, bytes: Vec<u8>) -> Vec<Upload> {
        vec![Upload {
            name: name.into(),
            bytes: crate::upload::UploadBytes::from_vec(bytes),
            content_type: None,
        }]
    }

    fn params(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    async fn body_bytes(response: Response) -> Vec<u8> {
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec()
    }

    /// One-page PDF carrying a Helvetica text run, like `text.rs` tests.
    fn pdf_with_text(ops: &str) -> Vec<u8> {
        let mut pdf = testkit::Pdf::new();
        let font = pdf.add(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let content = pdf.add_stream(dictionary! {}, ops.as_bytes());
        pdf.edit_page(|page| {
            page.set("Contents", Object::Reference(content));
            page.set(
                "Resources",
                dictionary! {
                    "Font" => dictionary! {
                        "F1" => Object::Reference(font),
                    },
                },
            );
        });
        pdf.bytes()
    }

    fn pdf_with_image() -> Vec<u8> {
        let mut pdf = testkit::Pdf::new();
        let mut stream = Stream::new(testkit::image_xobject(), vec![255, 0, 0]);
        let _ = stream.compress();
        let id = pdf.add(Object::Stream(stream));
        pdf.edit_page(|page| {
            page.set(
                "Resources",
                dictionary! {
                    "XObject" => dictionary! {
                        "Im1" => Object::Reference(id),
                    },
                },
            );
        });
        pdf.bytes()
    }

    #[tokio::test]
    async fn page_count_pass_returns_upload_and_miss_is_204() {
        let bytes = testkit::Pdf::new().bytes();
        let pass = page_count_work(
            state(),
            HashMap::new(),
            upload("in.pdf", bytes.clone()),
            params(&[("pageCount", "1"), ("comparator", "Equal")]),
        )
        .unwrap();
        assert_eq!(pass.status(), StatusCode::OK);
        assert_eq!(body_bytes(pass).await, bytes);

        let miss = page_count_work(
            state(),
            HashMap::new(),
            upload("in.pdf", testkit::Pdf::new().bytes()),
            params(&[("pageCount", "5"), ("comparator", "Equal")]),
        )
        .unwrap();
        assert_eq!(miss.status(), StatusCode::NO_CONTENT);
        assert!(body_bytes(miss).await.is_empty());
    }

    #[tokio::test]
    async fn file_size_never_parses_the_document() {
        // Java compares byte length only — non-PDF bytes can pass.
        let junk = b"definitely not a pdf".to_vec();
        let pass = file_size_work(
            state(),
            HashMap::new(),
            upload("notes.txt", junk.clone()),
            params(&[("fileSize", "21"), ("comparator", "Less")]),
        )
        .unwrap();
        assert_eq!(pass.status(), StatusCode::OK);
        assert_eq!(body_bytes(pass).await, junk);
    }

    #[tokio::test]
    async fn contains_text_matches_and_misses_per_page() {
        let bytes = pdf_with_text("BT /F1 12 Tf 72 720 Td (Hello) Tj ET");
        let hit = contains_text_work(
            state(),
            HashMap::new(),
            upload("doc.pdf", bytes),
            params(&[("text", "Hello")]),
        )
        .unwrap();
        assert_eq!(hit.status(), StatusCode::OK);

        let miss = contains_text_work(
            state(),
            HashMap::new(),
            upload(
                "doc.pdf",
                pdf_with_text("BT /F1 12 Tf 72 720 Td (Hello) Tj ET"),
            ),
            params(&[("text", "Goodbye")]),
        )
        .unwrap();
        assert_eq!(miss.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn contains_text_requires_the_text_field() {
        let err = contains_text_work(
            state(),
            HashMap::new(),
            upload("doc.pdf", testkit::Pdf::new().bytes()),
            vec![],
        )
        .unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[tokio::test]
    async fn contains_image_finds_page_xobjects() {
        let hit = contains_image_work(
            state(),
            HashMap::new(),
            upload("doc.pdf", pdf_with_image()),
            vec![],
        )
        .unwrap();
        assert_eq!(hit.status(), StatusCode::OK);

        let miss = contains_image_work(
            state(),
            HashMap::new(),
            upload("doc.pdf", testkit::Pdf::new().bytes()),
            vec![],
        )
        .unwrap();
        assert_eq!(miss.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn page_size_compares_first_page_area() {
        // testkit pages are US Letter (612x792); A4's area differs.
        let pass = page_size_work(
            state(),
            HashMap::new(),
            upload("doc.pdf", testkit::Pdf::new().bytes()),
            params(&[("standardPageSize", "LETTER"), ("comparator", "Equal")]),
        )
        .unwrap();
        assert_eq!(pass.status(), StatusCode::OK);

        let miss = page_size_work(
            state(),
            HashMap::new(),
            upload("doc.pdf", testkit::Pdf::new().bytes()),
            params(&[("standardPageSize", "A4"), ("comparator", "Equal")]),
        )
        .unwrap();
        assert_eq!(miss.status(), StatusCode::NO_CONTENT);

        let bad = page_size_work(
            state(),
            HashMap::new(),
            upload("doc.pdf", testkit::Pdf::new().bytes()),
            params(&[("standardPageSize", "TABLOID"), ("comparator", "Equal")]),
        )
        .unwrap_err();
        assert!(matches!(bad, AppError::BadRequest(_)));
    }

    #[tokio::test]
    async fn page_rotation_reads_first_page_rotate() {
        let mut pdf = testkit::Pdf::new();
        pdf.edit_page(|page| page.set("Rotate", 90));
        let pass = page_rotation_work(
            state(),
            HashMap::new(),
            upload("doc.pdf", pdf.bytes()),
            params(&[("rotation", "90"), ("comparator", "Equal")]),
        )
        .unwrap();
        assert_eq!(pass.status(), StatusCode::OK);

        let miss = page_rotation_work(
            state(),
            HashMap::new(),
            upload("doc.pdf", testkit::Pdf::new().bytes()),
            params(&[("rotation", "90"), ("comparator", "Equal")]),
        )
        .unwrap();
        assert_eq!(miss.status(), StatusCode::NO_CONTENT);
    }
}
