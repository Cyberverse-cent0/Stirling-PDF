use std::collections::HashMap;

use axum::body::Body;
use axum::http::header;
use axum::response::Response;
use lopdf::Document;

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
/// Subset of the Java endpoint for now: `generateToc` and `removeCertSign=false`
/// are rejected rather than silently producing a different document.
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
    if !remove_cert_sign {
        return Err(AppError::BadRequest(
            "removeCertSign=false is not yet supported by the Rust backend".into(),
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
    let bytes = pdfdoc::save(&mut merged)?;

    let first = loaded.first().map(|l| l.name.as_str());
    let out_name = output_name(first, "_merged_unsigned.pdf");

    Ok(pdf_response(bytes, &out_name))
}

/// Spring's boolean conversion accepts the usual truthy/falsy spellings.
fn parse_bool(value: &str, field: &str) -> AppResult<bool> {
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
