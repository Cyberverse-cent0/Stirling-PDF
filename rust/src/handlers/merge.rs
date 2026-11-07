use std::collections::HashMap;

use axum::body::Body;
use axum::http::header;
use axum::response::Response;
use lopdf::{Dictionary, Document, Object};

use crate::error::{AppError, AppResult};
use crate::handlers::jobs::AppState;
use crate::pdfdoc;
use crate::upload::{output_name, param, Upload};

struct Loaded {
    name: String,
    document: Document,
}

/// `POST /api/v1/general/merge-pdfs` — merges uploaded PDFs in the order
/// implied by `fileOrder`/`sortType`, mirroring `MergeController`.
///
/// `generateToc` is rejected rather than silently producing a different
/// document. `removeCertSign=false` is honoured by re-registering the
/// signature field-widgets that page import carried over as annotations;
/// signature fields whose field dictionary is not itself a page annotation
/// (hierarchical fields) cannot be recovered and are dropped, as are all
/// non-signature form fields.
#[stirling_macros::endpoint]
fn merge_pdfs(
    _state: AppState,
    query: HashMap<String, String>,
    mut files: Vec<Upload>,
    // `params` arrives as a Vec of (name, value) pairs from multipart parsing.
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    if files.is_empty() {
        return Err(AppError::BadRequest("no files provided".into()));
    }

    // @RequestParam resolves the query string before multipart fields.
    let file_order = query
        .get("fileOrder")
        .map(String::as_str)
        .or_else(|| {
            params
                .iter()
                .rev()
                .find(|(n, _)| n == "fileOrder")
                .map(|(_, v)| v.as_str())
        })
        .filter(|v| !v.trim().is_empty())
        .map(str::to_string);

    let sort_type = param(&params, "sortType", "orderProvided").to_string();
    let generate_toc = parse_bool(param(&params, "generateToc", "false"), "generateToc")?;
    let remove_cert_sign = parse_bool(param(&params, "removeCertSign", "true"), "removeCertSign")?;

    if generate_toc {
        return Err(AppError::BadRequest(
            "generateToc is not yet supported by the Rust backend".into(),
        ));
    }

    let mut loaded = Vec::with_capacity(files.len());
    for file in files.drain(..) {
        loaded.push(Loaded {
            document: pdfdoc::load(&file.name, &file.bytes)?,
            name: file.name,
        });
    }

    sort_uploads(&mut loaded, sort_type.as_str(), file_order.as_deref());

    let mut merged = pdfdoc::new_document();
    for item in &mut loaded {
        let all: Vec<usize> = (0..pdfdoc::page_ids(&item.document).len()).collect();
        pdfdoc::import_pages(&mut merged, &mut item.document, &all)?;
    }
    if let Some(first) = loaded.first() {
        pdfdoc::import_info(&mut merged, &first.document)?;
    }
    if !remove_cert_sign {
        preserve_signature_fields(&mut merged)?;
    }
    let bytes = pdfdoc::save(&mut merged)?;

    let first = loaded.first().map(|l| l.name.as_str());
    let out_name = output_name(first, "_merged_unsigned.pdf");

    Ok(pdf_response(bytes, &out_name))
}

/// Page import drops every document-level structure, so signature
/// field-widgets arrive as orphan annotations. `removeCertSign=false`
/// re-registers the ones that are merged field-widget dictionaries
/// (`/FT /Sig` on the annotation itself) under a fresh `/AcroForm`.
fn preserve_signature_fields(doc: &mut Document) -> AppResult<()> {
    let mut fields = Vec::new();
    for page_id in pdfdoc::page_ids(doc) {
        let Ok(page) = doc.get_dictionary(page_id) else {
            continue;
        };
        let Ok(annots) = page.get(b"Annots").and_then(|a| a.as_array()) else {
            continue;
        };
        for annot in annots {
            let Object::Reference(id) = annot else {
                continue;
            };
            let is_sig = doc
                .get_dictionary(*id)
                .ok()
                .and_then(|d| d.get(b"FT").ok())
                .and_then(|ft| ft.as_name().ok())
                == Some(b"Sig");
            if is_sig {
                fields.push(Object::Reference(*id));
            }
        }
    }
    if fields.is_empty() {
        return Ok(());
    }
    let mut acro = Dictionary::new();
    acro.set("Fields", Object::Array(fields));
    acro.set("SigFlags", Object::Integer(3));
    let acro_id = doc.new_object_id();
    doc.objects.insert(acro_id, Object::Dictionary(acro));
    let root = match doc.trailer.get(b"Root") {
        Ok(Object::Reference(id)) => *id,
        _ => return Err(AppError::Pdf("merged document has no catalog".into())),
    };
    doc.get_dictionary_mut(root)
        .map_err(|e| AppError::Pdf(format!("catalog: {e}")))?
        .set("AcroForm", Object::Reference(acro_id));
    Ok(())
}

/// Spring's boolean conversion accepts the usual truthy/falsy spellings.
pub(crate) fn parse_bool(value: &str, field: &str) -> AppResult<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "on" | "yes" => Ok(true),
        "false" | "0" | "off" | "no" => Ok(false),
        _ => Err(AppError::BadRequest(format!(
            "invalid boolean value for {field}: {value}"
        ))),
    }
}

pub(crate) fn pdf_response(bytes: Vec<u8>, name: &str) -> Response {
    file_response(bytes, name, "application/pdf")
}

pub(crate) fn file_response(bytes: Vec<u8>, name: &str, content_type: &str) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{name}\""),
        )
        .body(Body::from(bytes))
        .expect("static response parts")
}

fn sort_uploads(loaded: &mut Vec<Loaded>, sort_type: &str, file_order: Option<&str>) {
    if let Some(order) = file_order {
        let desired: Vec<&str> = order
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        let mut remaining = std::mem::take(loaded);
        for name in desired {
            if let Some(pos) = remaining.iter().position(|l| l.name == name) {
                loaded.push(remaining.remove(pos));
            }
        }
        loaded.extend(remaining);
        return;
    }

    match sort_type {
        "byFileName" => {
            loaded.sort_by_key(|l| l.name.to_lowercase());
        }
        "byPDFTitle" => loaded.sort_by(|a, b| {
            cmp_nulls_last(
                metadata(&a.document, "Title").as_deref(),
                metadata(&b.document, "Title").as_deref(),
            )
        }),
        // Java sorts both date types with one comparator: modification date,
        // falling back to creation date (then XMP, which we don't read yet).
        "byDateModified" | "byDateCreated" => {
            loaded.sort_by_key(|l| std::cmp::Reverse(sort_date(&l.document)))
        }
        _ => {}
    }
}

fn cmp_nulls_last(a: Option<&str>, b: Option<&str>) -> std::cmp::Ordering {
    match (a, b) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(a), Some(b)) => a.to_lowercase().cmp(&b.to_lowercase()),
    }
}

fn sort_date(document: &Document) -> Option<String> {
    metadata(document, "ModDate").or_else(|| metadata(document, "CreationDate"))
}

fn metadata(document: &Document, key: &str) -> Option<String> {
    pdfdoc::info_value(document, key).filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit;

    fn upload(name: &str, bytes: Vec<u8>) -> Upload {
        Upload {
            name: name.into(),
            bytes: crate::upload::UploadBytes::from_vec(bytes),
            content_type: None,
        }
    }

    fn loaded(name: &str, pages: usize) -> Loaded {
        let mut pdf = testkit::Pdf::new();
        for _ in 1..pages {
            pdf.add_page([0, 0, 612, 792], lopdf::Dictionary::new());
        }
        Loaded {
            name: name.into(),
            document: pdfdoc::load(name, &pdf.bytes()).unwrap(),
        }
    }

    #[test]
    fn parse_bool_accepts_spring_spellings() {
        for v in ["true", "1", "on", "yes", " TRUE "] {
            assert!(parse_bool(v, "f").unwrap(), "{v}");
        }
        for v in ["false", "0", "off", "no"] {
            assert!(!parse_bool(v, "f").unwrap(), "{v}");
        }
        assert!(parse_bool("maybe", "f").is_err());
        assert!(parse_bool("", "f").is_err());
    }

    #[test]
    fn file_order_sorts_by_listed_names() {
        let mut files = vec![loaded("a.pdf", 1), loaded("b.pdf", 1), loaded("c.pdf", 1)];
        sort_uploads(&mut files, "orderProvided", Some("c.pdf\nb.pdf"));
        let names: Vec<&str> = files.iter().map(|l| l.name.as_str()).collect();
        // Listed names come first in order; unlisted keep relative order.
        assert_eq!(names, vec!["c.pdf", "b.pdf", "a.pdf"]);
    }

    #[test]
    fn by_file_name_sorts_case_insensitively() {
        let mut files = vec![loaded("B.pdf", 1), loaded("a.pdf", 1)];
        sort_uploads(&mut files, "byFileName", None);
        let names: Vec<&str> = files.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, vec!["a.pdf", "B.pdf"]);
    }

    #[test]
    fn nulls_sort_last_for_title_and_dates() {
        assert_eq!(cmp_nulls_last(None, Some("x")), std::cmp::Ordering::Greater);
        assert_eq!(cmp_nulls_last(Some("x"), None), std::cmp::Ordering::Less);
        assert_eq!(cmp_nulls_last(None, None), std::cmp::Ordering::Equal);
        assert_eq!(
            cmp_nulls_last(Some("B"), Some("a")),
            std::cmp::Ordering::Greater // case-insensitive: "b" > "a"
        );
    }

    #[tokio::test]
    async fn merge_combines_pages_in_upload_order() {
        let mut a = testkit::Pdf::new();
        let mut b = testkit::Pdf::new();
        b.add_page([0, 0, 500, 500], lopdf::Dictionary::new());

        let response = merge_pdfs_work(
            AppState::new(),
            HashMap::new(),
            vec![upload("a.pdf", a.bytes()), upload("b.pdf", b.bytes())],
            vec![],
        )
        .unwrap();
        let doc = testkit::response_document(response).await;
        // `Pdf::new()` starts with one page: a=1, b=2 → 3 merged.
        assert_eq!(pdfdoc::page_ids(&doc).len(), 3);
    }

    #[test]
    fn merge_rejects_generate_toc() {
        let bytes = testkit::Pdf::new().bytes();
        let err = merge_pdfs_work(
            AppState::new(),
            HashMap::new(),
            vec![upload("a.pdf", bytes)],
            vec![("generateToc".into(), "true".into())],
        )
        .unwrap_err();
        match err {
            AppError::BadRequest(m) => assert!(m.contains("generateToc"), "{m}"),
            other => panic!("expected BadRequest, got {other}"),
        }
    }

    #[tokio::test]
    async fn merge_accepts_remove_cert_sign_false() {
        let mut a = testkit::Pdf::new();
        a.add_page([0, 0, 500, 500], lopdf::Dictionary::new());
        let response = merge_pdfs_work(
            AppState::new(),
            HashMap::new(),
            vec![upload("a.pdf", a.bytes())],
            vec![("removeCertSign".into(), "false".into())],
        )
        .unwrap();
        let doc = testkit::response_document(response).await;
        assert_eq!(pdfdoc::page_ids(&doc).len(), 2);
    }

    #[test]
    fn merge_rejects_empty_upload_list() {
        let err = merge_pdfs_work(AppState::new(), HashMap::new(), vec![], vec![]).unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn pdf_response_sets_attachment_headers() {
        let res = pdf_response(b"%PDF-1.7".to_vec(), "out_merged_unsigned.pdf");
        assert_eq!(
            res.headers().get(axum::http::header::CONTENT_TYPE).unwrap(),
            "application/pdf"
        );
        assert_eq!(
            res.headers()
                .get(axum::http::header::CONTENT_DISPOSITION)
                .unwrap(),
            "attachment; filename=\"out_merged_unsigned.pdf\""
        );
    }
}
