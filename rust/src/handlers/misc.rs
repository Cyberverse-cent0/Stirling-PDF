//! Ports of `MiscApi` endpoints migrated so far. These need raw PDF object
//! access, so they run directly on `lopdf` rather than `pdfdoc` helpers.

use std::collections::{HashMap, HashSet};

use axum::extract::{Multipart, Query, State};
use axum::response::{IntoResponse, Response};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};

use crate::error::{AppError, AppResult};
use crate::handlers::jobs;
use crate::handlers::jobs::AppState;
use crate::handlers::merge::{file_response, pdf_response};
use crate::page_select;
use crate::pdfdoc;
use crate::upload::{
    output_name, param, param_bool, receive_files_multipart, receive_multipart, remove_extension,
    simple_name, Upload, FILE_FIELD,
};

/// Shared tail of a migrated endpoint: `?async=true` goes through the job
/// framework, everything else runs inline on the blocking pool — the same
/// contract the `#[endpoint]` macro generates. Handlers that must inspect
/// params before deciding (e.g. rejecting an unported mode) use this
/// instead of the macro.
pub(crate) async fn dispatch<F>(
    state: AppState,
    query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
    work: F,
) -> AppResult<Response>
where
    F: FnOnce(
            AppState,
            HashMap<String, String>,
            Vec<Upload>,
            Vec<(String, String)>,
        ) -> AppResult<Response>
        + Send
        + 'static,
{
    if jobs::async_requested(&query, &params) {
        let work_state = state.clone();
        return jobs::submit(&state, async move {
            tokio::task::spawn_blocking(move || {
                work(work_state, query, uploads, params).map(IntoResponse::into_response)
            })
            .await
            .map_err(|e| AppError::Internal(anyhow::Error::new(e)))?
        })
        .await;
    }
    tokio::task::spawn_blocking(move || {
        work(state, query, uploads, params).map(IntoResponse::into_response)
    })
    .await
    .map_err(|e| AppError::Internal(anyhow::Error::new(e)))?
}

/// Multipart field names that are request plumbing, not custom metadata.
const METADATA_RESERVED: &[&str] = &[
    "fileInput",
    "async",
    "deleteAll",
    "author",
    "creationDate",
    "creator",
    "keywords",
    "modificationDate",
    "producer",
    "subject",
    "title",
    "trapped",
];

/// `POST /api/v1/misc/update-metadata` — rewrites the document `/Info`
/// dictionary. `deleteAll=true` clears it plus the catalog's XMP
/// `/Metadata` and `/PieceInfo`, like `MetadataController`.
///
/// Divergence: Java's `PdfMetadataService` round-trips dates through
/// `Calendar`; here `yyyy/MM/dd HH:mm:ss` is converted straight to the PDF
/// `D:` format, and unparsable dates remove the entry like Java's null.
#[stirling_macros::endpoint]
fn update_metadata(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    let mut document = pdfdoc::load(&upload.name, &upload.bytes)?;

    // Last occurrence wins, matching Spring's parameter binding.
    let mut fields: HashMap<String, String> = HashMap::new();
    for (name, value) in params {
        fields.insert(name, value);
    }
    let truthy = |name: &str| {
        fields.get(name).is_some_and(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "true" | "1" | "on" | "yes"
            )
        })
    };
    let delete_all = truthy("deleteAll");

    // Resolve the /Info dictionary, creating it inline when absent.
    let mut info = match document.trailer.get(b"Info").ok().cloned() {
        Some(lopdf::Object::Reference(id)) => document
            .get_object(id)
            .ok()
            .and_then(|o| o.as_dict().ok().cloned())
            .unwrap_or_default(),
        Some(lopdf::Object::Dictionary(dict)) => dict,
        _ => lopdf::Dictionary::new(),
    };

    if delete_all {
        info = lopdf::Dictionary::new();
        // Strip XMP metadata and piece info from the catalog, as Java does.
        if let Some(lopdf::Object::Reference(id)) = document.trailer.get(b"Root").ok().cloned() {
            if let Ok(lopdf::Object::Dictionary(mut catalog)) = document.get_object(id).cloned() {
                catalog.remove(b"Metadata");
                catalog.remove(b"PieceInfo");
                document
                    .objects
                    .insert(id, lopdf::Object::Dictionary(catalog));
            }
        }
    } else {
        // Standard keys: absent or "undefined" removes the entry, like
        // Java's unconditional setX(null) calls.
        for (field, pdf_key) in [
            ("author", "Author"),
            ("creationDate", "CreationDate"),
            ("creator", "Creator"),
            ("keywords", "Keywords"),
            ("modificationDate", "ModDate"),
            ("producer", "Producer"),
            ("subject", "Subject"),
            ("title", "Title"),
        ] {
            match fields.get(field) {
                Some(value) if !value.eq_ignore_ascii_case("undefined") => {
                    info.set(pdf_key, lopdf::Object::string_literal(value.as_str()));
                }
                _ => {
                    info.remove(pdf_key.as_bytes());
                }
            }
        }
        // Dates use the request's "yyyy/MM/dd HH:mm:ss" pattern.
        for (field, pdf_key) in [
            ("creationDate", "CreationDate"),
            ("modificationDate", "ModDate"),
        ] {
            if let Some(date) = pdf_date(fields.get(field).map(String::as_str)) {
                info.set(pdf_key, lopdf::Object::string_literal(date.as_str()));
            }
        }
        match fields.get("trapped").map(String::as_str) {
            Some("True") => info.set("Trapped", lopdf::Object::Name(b"True".to_vec())),
            Some("False") => info.set("Trapped", lopdf::Object::Name(b"False".to_vec())),
            Some("Unknown") => info.set("Trapped", lopdf::Object::Name(b"Unknown".to_vec())),
            _ => {
                info.remove(b"Trapped");
            }
        }

        // Custom metadata: non-reserved params become Info entries;
        // customKeyN/customValueN pairs set metadata[customKeyN-value]=customValueN.
        for (key, value) in &fields {
            if METADATA_RESERVED.contains(&key.as_str())
                || key.eq_ignore_ascii_case("author")
                || key.contains("customValue")
            {
                continue;
            }
            if let Some(number) = key
                .strip_prefix("customKey")
                .and_then(|n| n.parse::<u32>().ok())
            {
                if let Some(custom_key) = fields.get(&format!("customKey{number}")) {
                    if let Some(custom_value) = fields.get(&format!("customValue{number}")) {
                        info.set(
                            custom_key.as_str(),
                            lopdf::Object::string_literal(custom_value.as_str()),
                        );
                    }
                }
                continue;
            }
            // Java skips keys matching the standard names case-insensitively.
            let is_standard = [
                "author",
                "creationdate",
                "creator",
                "keywords",
                "modificationdate",
                "producer",
                "subject",
                "title",
                "trapped",
            ]
            .contains(&key.to_ascii_lowercase().as_str());
            if !is_standard {
                info.set(key.as_str(), lopdf::Object::string_literal(value.as_str()));
            }
        }
    }

    document
        .trailer
        .set("Info", lopdf::Object::Dictionary(info));

    let bytes = pdfdoc::save(&mut document)?;
    Ok(pdf_response(
        bytes,
        &output_name(Some(&upload.name), "_metadata.pdf"),
    ))
}

/// `"yyyy/MM/dd HH:mm:ss"` → PDF `D:yyyyMMddHHmmSS`; `None` when absent,
/// "undefined", or malformed (Java's `parseToCalendar` null path).
fn pdf_date(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("undefined") {
        return None;
    }
    let (date, time) = value.split_once(' ')?;
    let d: Vec<&str> = date.split('/').collect();
    let t: Vec<&str> = time.split(':').collect();
    if d.len() != 3 || t.len() != 3 {
        return None;
    }
    let digits = |s: &str, len: usize| {
        (s.len() == len && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.to_string())
    };
    Some(format!(
        "D:{}{}{}{}{}{}",
        digits(d[0], 4)?,
        digits(d[1], 2)?,
        digits(d[2], 2)?,
        digits(t[0], 2)?,
        digits(t[1], 2)?,
        digits(t[2], 2)?
    ))
}

/// `POST /api/v1/misc/show-javascript` — concatenates every document-level
/// JavaScript action into one `.js` file, like `ShowJavascript`.
#[stirling_macros::endpoint]
fn show_javascript(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    _params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    let document = pdfdoc::load(&upload.name, &upload.bytes)?;

    let mut script = String::new();
    let mut found = false;
    for (name, code) in pdfdoc::javascript_entries(&document)? {
        if !code.trim().is_empty() {
            script.push_str(&format!(
                "// File: {}, Script: {}\n{}\n",
                upload.name, name, code
            ));
            found = true;
        }
    }
    if !found {
        script = format!("PDF '{}' does not contain Javascript", upload.name);
    }

    Ok(file_response(
        script.into_bytes(),
        &format!("{}.js", upload.name),
        "text/plain",
    ))
}

/// `POST /api/v1/misc/decompress-pdf` — decodes every stream and drops its
/// `/Filter` entries, mirroring `DecompressPdfController`: per-stream
/// failures are logged and skipped, not fatal.
///
/// Unlike a page-import backend this one edits the file at the object
/// level, so object streams and cross-reference streams are decompressed
/// too — same coverage as the Java xref-table walk.
#[stirling_macros::endpoint]
fn decompress_pdf(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    _params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };

    let mut document = pdfdoc::load(&upload.name, &upload.bytes)?;

    for object in document.objects.values_mut() {
        if let Object::Stream(stream) = object {
            if let Err(e) = stream.decompress() {
                tracing::debug!(error = %e, "skipping stream that failed to decompress");
            }
        }
    }

    pdfdoc::touch_loaded_info(&mut document);
    let bytes = pdfdoc::save(&mut document)?;

    Ok(pdf_response(
        bytes,
        &output_name(Some(&upload.name), "_decompressed.pdf"),
    ))
}

/// `POST /api/v1/misc/flatten` — `flattenOnlyForms=true` merges AcroForm
/// widget appearances into the page content natively. The default path
/// rasterises every page and rebuilds the document image-only; that mode
/// has no native implementation, so it is rejected rather than
/// approximated.
pub async fn flatten(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, params) = receive_multipart(multipart).await?;
    if param_bool(&params, "flattenOnlyForms") {
        return dispatch(state, query, uploads, params, flatten_work).await;
    }
    Err(AppError::BadRequest(
        "flatten without flattenOnlyForms rasterises every page, which the Rust backend does not implement".into(),
    ))
}

fn flatten_work(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    _params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    let mut document = pdfdoc::load(&upload.name, &upload.bytes)?;
    crate::forms::flatten(&mut document)?;
    // FlattenController builds its output via createNewDocumentBasedOnOldDocument.
    pdfdoc::restamp_info(&mut document);
    let bytes = pdfdoc::save(&mut document)?;
    // Java returns the upload's own filename, not a tool-suffixed one.
    Ok(pdf_response(bytes, simple_name(&upload.name)))
}

/// Raster half of `/flatten`: every page becomes a JPEG via Ghostscript —
/// `PDFRenderer.renderImageWithDPI` + `JPEGFactory` in Java. The Rust
/// settings tree has no `system.maxDPI`, so the ceiling stays at Java's
/// 300 DPI default; an explicit `renderDpi` clamps to [72, 300].
#[allow(dead_code)] // raster /flatten not yet routed
fn flatten_raster_work(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    let src = pdfdoc::load(&upload.name, &upload.bytes)?;
    let src_pages = pdfdoc::page_ids(&src);
    if src_pages.is_empty() {
        return Err(AppError::Pdf("document has no pages".into()));
    }

    let dpi = match param(&params, "renderDpi", "").parse::<i64>() {
        Ok(requested) => requested.clamp(72, 300),
        // Java: min(FLATTEN_RENDER_DPI=300, maxDPI); maxDPI defaults to 300.
        Err(_) => 300,
    } as u32;

    let dir = tempfile::tempdir()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("flatten tempdir: {e}")))?;
    let input = dir.path().join("input.pdf");
    std::fs::write(&input, &upload.bytes)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("write flatten input: {e}")))?;
    let spec = crate::convert::EngineSpec::new(&crate::settings::engine_program("gs"))
        .args([
            "-dNOPAUSE".to_string(),
            "-dBATCH".to_string(),
            "-dQUIET".to_string(),
            "-dSAFER".to_string(),
            "-sDEVICE=jpeg".to_string(),
            format!("-r{dpi}"),
            format!("-sOutputFile={}", dir.path().join("page-%d.jpg").display()),
        ])
        .arg(input.to_string_lossy().to_string())
        .timeout(std::time::Duration::from_secs(600));
    let out = crate::convert::run(&spec)?;
    crate::convert::require_success("gs", &out)?;

    let mut doc = pdfdoc::new_document();
    pdfdoc::import_info(&mut doc, &src)?;
    for (i, src_page) in src_pages.iter().enumerate() {
        let image_path = dir.path().join(format!("page-{}.jpg", i + 1));
        let jpeg = std::fs::read(&image_path).map_err(|e| {
            AppError::Pdf(format!("ghostscript produced no page {} image: {e}", i + 1))
        })?;
        let (iw, ih) = image::image_dimensions(&image_path)
            .map_err(|e| AppError::Pdf(format!("cannot read rendered page {}: {e}", i + 1)))?;
        let mb = pdfdoc::media_box(&src, *src_page)?;
        let (pw, ph) = (mb[2] - mb[0], mb[3] - mb[1]);

        let mut img_dict = Dictionary::new();
        img_dict.set("Type", Object::Name(b"XObject".to_vec()));
        img_dict.set("Subtype", Object::Name(b"Image".to_vec()));
        img_dict.set("Width", Object::Integer(iw as i64));
        img_dict.set("Height", Object::Integer(ih as i64));
        img_dict.set("ColorSpace", Object::Name(b"DeviceRGB".to_vec()));
        img_dict.set("BitsPerComponent", Object::Integer(8));
        img_dict.set("Filter", Object::Name(b"DCTDecode".to_vec()));
        let img_id = doc.new_object_id();
        doc.objects
            .insert(img_id, Object::Stream(Stream::new(img_dict, jpeg)));

        let mut xobjects = Dictionary::new();
        xobjects.set("Im0", Object::Reference(img_id));
        let mut resources = Dictionary::new();
        resources.set("XObject", Object::Dictionary(xobjects));

        let mut page = Dictionary::new();
        page.set("Type", Object::Name(b"Page".to_vec()));
        page.set(
            "MediaBox",
            Object::Array(mb.iter().map(|v| Object::Real(*v as f32)).collect()),
        );
        page.set("Resources", Object::Dictionary(resources));
        let page_id = pdfdoc::append_page(&mut doc, page)?;

        // `drawImage(pdImage, 0, 0, pageWidth, pageHeight)`: the unit
        // image square maps onto [0,w]×[0,h] — same stretch Java applies.
        let content = format!(
            "q {} 0 0 {} 0 0 cm /Im0 Do Q\n",
            pdfdoc::pdf_num(pw),
            pdfdoc::pdf_num(ph)
        );
        let content_id = doc.new_object_id();
        doc.objects.insert(
            content_id,
            Object::Stream(Stream::new(Dictionary::new(), content.into_bytes())),
        );
        doc.get_dictionary_mut(page_id)
            .map_err(|e| AppError::Pdf(format!("page dict: {e}")))?
            .set("Contents", Object::Reference(content_id));
    }

    let bytes = pdfdoc::save(&mut doc)?;
    Ok(pdf_response(bytes, simple_name(&upload.name)))
}

/// `POST /api/v1/misc/extract-images` — image XObject extraction to a ZIP.
/// The `image` crate's codec set (png, jpeg/jpg, tiff, bmp, webp, gif)
/// runs natively; PDF image types outside `imgextract`'s decode subset
/// (JPX/CCITT/JBIG2, Indexed/ICCBased colour spaces, `/Decode`, soft
/// masks) are rejected — silently dropping them from the ZIP would be
/// worse than a clear error.
pub async fn extract_images(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, params) = receive_multipart(multipart).await?;
    let format = param(&params, "format", "png");
    // Java accepts any ImageIO-writable name; the `image` crate's codec
    // set is the faithful subset — anything else gets the same failure
    // Java's "no image writer" path produces.
    if !matches!(
        format.trim().to_ascii_lowercase().as_str(),
        "png" | "jpeg" | "jpg" | "tiff" | "tif" | "bmp" | "webp" | "gif"
    ) {
        return Err(AppError::BadRequest(format!(
            "unsupported image format: {format}"
        )));
    }
    // Any image outside the native decode subset would be silently
    // missing from the ZIP, so refuse the request instead.
    if let Some(upload) = uploads.first() {
        if !crate::imgextract::supported(&upload.name, &upload.bytes)? {
            return Err(AppError::BadRequest(format!(
                "{} contains image types extract-images cannot decode",
                upload.name
            )));
        }
    }
    dispatch(state, query, uploads, params, extract_images_work).await
}

fn extract_images_work(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    let format = param(&params, "format", "png");
    let bytes = crate::imgextract::extract(&upload.name, &upload.bytes, format)?;
    Ok(file_response(
        bytes,
        &output_name(Some(&upload.name), "_extracted-images.zip"),
        "application/zip",
    ))
}

/// `POST /api/v1/misc/compress-pdf` — qpdf/Ghostscript sidecars plus the
/// lopdf image-recompression pass. `lineArt=true` needs the proprietary
/// ImageMagick line-art service, which has no Rust port.
pub async fn compress_pdf(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, params) = receive_multipart(multipart).await?;
    if param_bool(&params, "lineArt") {
        return Err(AppError::BadRequest(
            "compress-pdf lineArt=true needs the ImageMagick line-art service, which the Rust backend does not implement".into(),
        ));
    }
    dispatch(state, query, uploads, params, compress_pdf_work).await
}

fn last_param<'a>(params: &'a [(String, String)], name: &str) -> Option<&'a str> {
    params
        .iter()
        .rev()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

/// `GeneralUtils.convertSizeToBytes(sizeStr, "MB")`: binary units, bare
/// numbers default to megabytes.
fn size_to_bytes(size: &str) -> Option<u64> {
    let size = size
        .trim()
        .to_uppercase()
        .replace(',', ".")
        .replace(' ', "");
    let (number, power) = if let Some(rest) = size.strip_suffix("TB") {
        (rest, 4)
    } else if let Some(rest) = size.strip_suffix("GB") {
        (rest, 3)
    } else if let Some(rest) = size.strip_suffix("MB") {
        (rest, 2)
    } else if let Some(rest) = size.strip_suffix("KB") {
        (rest, 1)
    } else if size.ends_with('B') {
        (&size[..size.len() - 1], 0)
    } else {
        (size.as_str(), 2)
    };
    let value: f64 = number.parse().ok()?;
    if value < 0.0 {
        return None;
    }
    let bytes = (value * 1024f64.powi(power)).floor();
    if bytes > i64::MAX as f64 {
        return None;
    }
    Some(bytes as u64)
}

fn scale_factor_for_level(level: i32) -> f64 {
    match level {
        1 => 0.98,
        2 => 0.95,
        3 => 0.88,
        4 => 0.78,
        5 => 0.68,
        6 => 0.58,
        7 => 0.48,
        8 => 0.38,
        9 => 0.28,
        _ => 1.0,
    }
}

fn jpeg_quality_for_level(level: i32) -> f32 {
    match level {
        1 => 0.92,
        2 => 0.88,
        3 => 0.85,
        4 => 0.80,
        5 => 0.72,
        6 => 0.65,
        7 => 0.55,
        8 => 0.45,
        9 => 0.35,
        _ => 0.75,
    }
}

/// `determineOptimizeLevel`: picks the level whose typical output lands near
/// the requested ratio of input size.
fn determine_level(expected: u64, input_size: u64) -> i32 {
    let ratio = if input_size == 0 {
        0.0
    } else {
        expected as f64 / input_size as f64
    };
    if ratio > 0.9 {
        1
    } else if ratio > 0.8 {
        2
    } else if ratio > 0.7 {
        3
    } else if ratio > 0.6 {
        4
    } else if ratio > 0.3 {
        5
    } else if ratio > 0.2 {
        6
    } else if ratio > 0.15 {
        7
    } else if ratio > 0.1 {
        8
    } else {
        9
    }
}

fn increment_level(current: i32, output_size: u64, target: u64) -> i32 {
    let ratio = output_size as f64 / target.max(1) as f64;
    if ratio > 2.0 {
        (current + 3).min(9)
    } else if ratio > 1.5 {
        (current + 2).min(9)
    } else {
        (current + 1).min(9)
    }
}

fn gs_critical(output: &str) -> bool {
    let lower = output.to_lowercase();
    lower.contains("page drawing error")
        || lower.contains("could not draw this page")
        || lower.contains("eps files may not contain multiple pages")
}

fn compress_pdf_work(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    if upload.bytes.is_empty() {
        return Err(AppError::BadRequest("file is empty".into()));
    }

    let level_param = last_param(&params, "optimizeLevel");
    let level: Option<i32> = match level_param {
        Some(raw) if !raw.trim().is_empty() => Some(
            raw.trim()
                .parse()
                .map_err(|_| AppError::BadRequest("optimizeLevel must be an integer".into()))?,
        ),
        _ => None,
    };
    let expected_str = last_param(&params, "expectedOutputSize");
    let (mut level, mut auto_mode, mut expected_bytes) = (level.unwrap_or(5), false, 0u64);
    if expected_str.is_some_and(|s| s.len() > 1) {
        let parsed = size_to_bytes(expected_str.unwrap())
            .ok_or_else(|| AppError::BadRequest("invalid expectedOutputSize".into()))?;
        expected_bytes = parsed;
        auto_mode = true;
        level = determine_level(expected_bytes, upload.bytes.len() as u64);
    }
    if level_param.is_none() && expected_str.is_none() {
        // Java's model defaults optimizeLevel to 5, so this can't trigger in
        // practice; kept for parity with an explicitly emptied parameter.
    }

    let linearize = param_bool(&params, "linearize");
    let normalize = param_bool(&params, "normalize");
    let grayscale = param_bool(&params, "grayscale");

    let workdir =
        tempfile::tempdir().map_err(|e| AppError::Internal(anyhow::anyhow!("tempdir: {e}")))?;
    let mut current = upload.bytes.to_vec();
    let input_size = current.len() as u64;

    let mut size_met = false;
    let mut image_applied = false;
    while !size_met && level <= 9 {
        let mut ghostscript_ok = false;
        if level >= 6 {
            if let Some(bytes) = run_ghostscript(workdir.path(), &current, level, grayscale)? {
                current = bytes;
                ghostscript_ok = true;
            }
        }

        if let Some(bytes) = run_qpdf(workdir.path(), &current, level, normalize, linearize)? {
            if bytes.len() < current.len() {
                current = bytes;
            }
        }

        if ghostscript_ok {
            image_applied = true;
        }
        if (level >= 4 || grayscale) && !image_applied {
            current = crate::imgcomp::compress_images(
                &upload.name,
                &current,
                scale_factor_for_level(level),
                jpeg_quality_for_level(level),
                grayscale,
            )?;
            image_applied = true;
        }

        let output_size = current.len() as u64;
        if output_size <= expected_bytes || !auto_mode {
            size_met = true;
        } else {
            let new_level = increment_level(level, output_size, expected_bytes);
            if new_level == level {
                size_met = true;
            } else {
                image_applied = false;
                level = new_level;
            }
        }
    }

    // Java returns the untouched upload when compression didn't shrink it.
    let bytes = if current.len() >= input_size as usize {
        upload.bytes.to_vec()
    } else {
        current
    };
    Ok(pdf_response(
        bytes,
        &output_name(Some(&upload.name), "_Optimized.pdf"),
    ))
}

/// Runs Ghostscript with the same flags Java builds per optimize level.
/// `Ok(None)` covers every non-fatal failure (missing binary, non-zero exit,
/// unusable output); a critical page-drawing error fails the request like
/// Java's `GhostscriptException`.
fn run_ghostscript(
    dir: &std::path::Path,
    input: &[u8],
    level: i32,
    grayscale: bool,
) -> AppResult<Option<Vec<u8>>> {
    let input_path = dir.join("gs-in.pdf");
    let output_path = dir.join("gs-out.pdf");
    std::fs::write(&input_path, input)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("gs input: {e}")))?;

    let mut command = std::process::Command::new(crate::convert::resolve(
        &crate::settings::engine_program("gs"),
    ));
    command
        .arg("-sDEVICE=pdfwrite")
        .arg("-dCompatibilityLevel=1.5")
        .arg("-dNOPAUSE")
        .arg("-dQUIET")
        .arg("-dBATCH")
        .arg("-dDetectDuplicateImages=true")
        .arg("-dDownsampleColorImages=true")
        .arg("-dCompressFonts=true")
        .arg("-dSubsetFonts=true");
    match level {
        1 => {
            command.arg("-dPDFSETTINGS=/prepress");
        }
        2 => {
            command.arg("-dPDFSETTINGS=/printer");
        }
        3 => {
            command.arg("-dPDFSETTINGS=/ebook");
        }
        6 | 7 => {
            command
                .arg("-dPDFSETTINGS=/screen")
                .arg("-dColorImageResolution=150")
                .arg("-dGrayImageResolution=150")
                .arg("-dMonoImageResolution=300");
        }
        8 => {
            command
                .arg("-dPDFSETTINGS=/screen")
                .arg("-dColorImageResolution=100")
                .arg("-dGrayImageResolution=100")
                .arg("-dMonoImageResolution=200");
        }
        9 => {
            command
                .arg("-dPDFSETTINGS=/screen")
                .arg("-dColorImageResolution=72")
                .arg("-dGrayImageResolution=72")
                .arg("-dMonoImageResolution=150");
        }
        _ => {
            command.arg("-dPDFSETTINGS=/screen");
        }
    }
    if grayscale {
        command
            .arg("-dColorConversionStrategy=/Gray")
            .arg("-dProcessColorModel=/DeviceGray");
    }
    if level >= 7 && !grayscale {
        command.arg("-dConvertCMYKImagesToRGB=true");
    }
    command
        .arg(format!("-sOutputFile={}", output_path.display()))
        .arg(&input_path);

    let output = match command.output() {
        Ok(output) => output,
        Err(e) => {
            tracing::warn!(error = %e, "ghostscript unavailable, continuing without it");
            return Ok(None);
        }
    };
    let mut messages = String::from_utf8_lossy(&output.stdout).into_owned();
    messages.push_str(&String::from_utf8_lossy(&output.stderr));
    if gs_critical(&messages) {
        return Err(AppError::Internal(anyhow::anyhow!(
            "ghostscript reported a page drawing error"
        )));
    }
    if !output.status.success() {
        tracing::warn!("ghostscript exited with {}", output.status);
        return Ok(None);
    }
    match std::fs::read(&output_path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) => {
            tracing::warn!(error = %e, "ghostscript produced no output");
            Ok(None)
        }
    }
}

/// Runs qpdf with the flags Java builds per optimize level. The caller keeps
/// the result only when it is smaller, like `applyQpdfCompression`.
fn run_qpdf(
    dir: &std::path::Path,
    input: &[u8],
    level: i32,
    normalize: bool,
    linearize: bool,
) -> AppResult<Option<Vec<u8>>> {
    let input_path = dir.join("qpdf-in.pdf");
    let output_path = dir.join("qpdf-out.pdf");
    std::fs::write(&input_path, input)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("qpdf input: {e}")))?;

    let compression_level = match level {
        1 => 3,
        2 => 5,
        3..=5 => 7,
        _ => 9,
    };
    let jpeg_quality = match level {
        5 => 78,
        6 => 68,
        7 => 58,
        8 => 46,
        _ => 34,
    };

    let mut args: Vec<String> = Vec::new();
    if normalize {
        args.push("--normalize-content=y".into());
    }
    if linearize {
        args.push("--linearize".into());
    }
    args.extend([
        "--decode-level=generalized".into(),
        "--recompress-flate".into(),
        format!("--compression-level={compression_level}"),
        "--compress-streams=y".into(),
        "--stream-data=compress".into(),
    ]);
    if level <= 3 {
        args.push("--preserve-unreferenced".into());
    }
    if level >= 5 {
        args.push("--optimize-images".into());
        args.push(format!("--jpeg-quality={jpeg_quality}"));
    }
    args.push("--object-streams=generate".into());
    args.push(input_path.display().to_string());
    args.push(output_path.display().to_string());

    let mut command = std::process::Command::new(crate::convert::resolve(
        &crate::settings::engine_program("qpdf"),
    ));
    if level >= 8 && !cfg!(windows) {
        command.arg("env").arg("QPDF_ZOPFLI=silent");
    }
    command.args(&args);

    let output = match command.output() {
        Ok(output) => output,
        Err(e) => {
            tracing::warn!(error = %e, "qpdf unavailable, continuing without it");
            return Ok(None);
        }
    };
    if !output.status.success() {
        tracing::warn!("qpdf exited with {}", output.status);
        return Ok(None);
    }
    match std::fs::read(&output_path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) => {
            tracing::warn!(error = %e, "qpdf produced no output");
            Ok(None)
        }
    }
}

/// `POST /api/v1/misc/list-attachments` — embedded-file metadata as JSON,
/// like `AttachmentController.listAttachments`.
#[stirling_macros::endpoint]
fn list_attachments(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    _params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    let document = pdfdoc::load(&upload.name, &upload.bytes)?;
    let attachments = crate::attachments::list_attachments(&document)?;
    Ok(axum::Json(attachments).into_response())
}

/// `POST /api/v1/misc/extract-single-attachment` — one embedded file's bytes.
#[stirling_macros::endpoint]
fn extract_single_attachment(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    let name = params
        .iter()
        .rev()
        .find(|(n, _)| n == "attachmentName")
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| AppError::BadRequest("attachmentName is required".into()))?;
    let document = pdfdoc::load(&upload.name, &upload.bytes)?;
    let Some(bytes) = crate::attachments::extract_single(&document, &name)? else {
        return Err(AppError::BadRequest(format!(
            "attachment '{name}' not found in the provided PDF"
        )));
    };
    Ok(file_response(
        bytes,
        simple_name(&name),
        "application/octet-stream",
    ))
}

/// `POST /api/v1/misc/extract-attachments` — every embedded file in a ZIP.
#[stirling_macros::endpoint]
fn extract_attachments(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    _params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    let document = pdfdoc::load(&upload.name, &upload.bytes)?;
    let Some(bytes) = crate::attachments::extract_all(&document)? else {
        return Err(AppError::BadRequest(
            "no embedded attachments found in the provided PDF".into(),
        ));
    };
    Ok(file_response(
        bytes,
        &output_name(Some(&upload.name), "_attachments.zip"),
        "application/zip",
    ))
}

/// `POST /api/v1/misc/rename-attachment`.
#[stirling_macros::endpoint]
fn rename_attachment(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    let field = |name: &str| {
        params
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| AppError::BadRequest(format!("{name} is required")))
    };
    let attachment_name = field("attachmentName")?;
    let new_name = field("newName")?;
    let mut document = pdfdoc::load(&upload.name, &upload.bytes)?;
    crate::attachments::rename_attachment(&mut document, &attachment_name, &new_name)?;
    pdfdoc::touch_loaded_info(&mut document);
    let bytes = pdfdoc::save(&mut document)?;
    Ok(pdf_response(
        bytes,
        &output_name(Some(&upload.name), "_attachment_renamed.pdf"),
    ))
}

/// `POST /api/v1/misc/delete-attachment`.
#[stirling_macros::endpoint]
fn delete_attachment(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    let name = params
        .iter()
        .rev()
        .find(|(n, _)| n == "attachmentName")
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| AppError::BadRequest("attachmentName is required".into()))?;
    let mut document = pdfdoc::load(&upload.name, &upload.bytes)?;
    crate::attachments::delete_attachment(&mut document, &name)?;
    pdfdoc::touch_loaded_info(&mut document);
    let bytes = pdfdoc::save(&mut document)?;
    Ok(pdf_response(
        bytes,
        &output_name(Some(&upload.name), "_attachment_deleted.pdf"),
    ))
}

/// `POST /api/v1/misc/add-attachments` — the base PDF arrives as `fileInput`,
/// the files to embed as repeated `attachments` parts. `convertToPdfA3b=true`
/// is rejected: the PDF/A-3b pass would drop the embedded files.
pub async fn add_attachments(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, extras, params) =
        crate::upload::receive_files_multipart(multipart, &["attachments"]).await?;
    let pdfa = param_bool(&params, "convertToPdfA3b");
    dispatch(state, query, uploads, params, move |_st, _q, u, p| {
        maybe_pdfa_response(add_attachments_work(u, extras)?, &p, pdfa)
    })
    .await
}

const MAX_ATTACHMENT: usize = 50 * 1024 * 1024;
const MAX_TOTAL_ATTACHMENTS: usize = 200 * 1024 * 1024;

fn add_attachments_work(
    uploads: Vec<Upload>,
    extras: Vec<(String, Upload)>,
) -> AppResult<(Vec<u8>, String)> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    let attachments: Vec<Upload> = extras
        .into_iter()
        .filter(|(field, _)| field == "attachments")
        .map(|(_, u)| u)
        .collect();
    if attachments.is_empty() {
        return Err(AppError::BadRequest(
            "at least one attachment is required".into(),
        ));
    }
    validate_attachment_uploads(&attachments)?;

    let mut document = pdfdoc::load(&upload.name, &upload.bytes)?;
    let files: Vec<(String, Vec<u8>)> = attachments
        .iter()
        .map(|a| (a.name.clone(), a.bytes.to_vec()))
        .collect();
    crate::attachments::add_attachments(&mut document, &files)?;
    pdfdoc::touch_loaded_info(&mut document);
    let bytes = pdfdoc::save(&mut document)?;
    Ok((
        bytes,
        output_name(Some(&upload.name), "_with_attachments.pdf"),
    ))
}

/// `opsJson` payload of `batch-process-attachments`: ordered rename/delete
/// lists applied before any additions, like `AttachmentController.BatchOpsData`.
#[derive(serde::Deserialize, Default)]
struct BatchOps {
    #[serde(default)]
    renames: Option<Vec<BatchRename>>,
    #[serde(default)]
    deletions: Option<Vec<String>>,
}

#[derive(serde::Deserialize)]
struct BatchRename {
    #[serde(rename = "oldName", default)]
    old_name: Option<String>,
    #[serde(rename = "newName", default)]
    new_name: Option<String>,
}

/// `POST /api/v1/misc/batch-process-attachments` — deletions, renames and
/// additions in one pass. `convertToPdfA3b=true` is rejected for the same
/// reason as `add-attachments`.
pub async fn batch_process_attachments(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, extras, params) =
        crate::upload::receive_files_multipart(multipart, &["attachments"]).await?;
    let pdfa = param_bool(&params, "convertToPdfA3b");
    dispatch(state, query, uploads, params, move |_st, _q, u, p| {
        maybe_pdfa_response(batch_process_attachments_work(u, extras, &p)?, &p, pdfa)
    })
    .await
}

fn batch_process_attachments_work(
    uploads: Vec<Upload>,
    extras: Vec<(String, Upload)>,
    params: &[(String, String)],
) -> AppResult<(Vec<u8>, String)> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    let attachments: Vec<Upload> = extras
        .into_iter()
        .filter(|(field, _)| field == "attachments")
        .map(|(_, u)| u)
        .collect();
    if !attachments.is_empty() {
        validate_attachment_uploads(&attachments)?;
    }

    let ops_json = param(params, "opsJson", "");
    let ops = if ops_json.trim().is_empty() {
        None
    } else {
        Some(
            serde_json::from_str::<BatchOps>(ops_json)
                .map_err(|e| AppError::BadRequest(format!("invalid opsJson: {e}")))?,
        )
    };

    let mut document = pdfdoc::load(&upload.name, &upload.bytes)?;
    if let Some(ops) = &ops {
        // Java logs and skips individual failures rather than aborting.
        if let Some(deletions) = &ops.deletions {
            for name in deletions {
                if let Err(e) = crate::attachments::delete_attachment(&mut document, name) {
                    tracing::warn!("batch deletion of '{name}' skipped: {e}");
                }
            }
        }
        if let Some(renames) = &ops.renames {
            for rename in renames {
                let (Some(old), Some(new)) = (&rename.old_name, &rename.new_name) else {
                    continue;
                };
                if let Err(e) = crate::attachments::rename_attachment(&mut document, old, new) {
                    tracing::warn!("batch rename from '{old}' to '{new}' skipped: {e}");
                }
            }
        }
    }
    if !attachments.is_empty() {
        let files: Vec<(String, Vec<u8>)> = attachments
            .iter()
            .map(|a| (a.name.clone(), a.bytes.to_vec()))
            .collect();
        crate::attachments::add_attachments(&mut document, &files)?;
    }

    pdfdoc::touch_loaded_info(&mut document);
    let bytes = pdfdoc::save(&mut document)?;
    Ok((
        bytes,
        output_name(Some(&upload.name), "_attachments_modified.pdf"),
    ))
}

/// `convertToPdfA3b` shared by the attachment endpoints: Java ran a
/// PDF/A-3b Ghostscript pass, but PDF/A-3b must preserve the embedded files
/// and no native path does that yet, so the flag is rejected outright.
fn maybe_pdfa_response(
    (bytes, name): (Vec<u8>, String),
    _params: &[(String, String)],
    pdfa: bool,
) -> AppResult<Response> {
    if pdfa {
        return Err(AppError::BadRequest(
            "convertToPdfA3b is not implemented for attachment endpoints".into(),
        ));
    }
    Ok(pdf_response(bytes, &name))
}

/// The 50 MB-per-file / 200 MB-total caps `validateAttachmentRequest`
/// applies to `attachments` parts.
fn validate_attachment_uploads(attachments: &[Upload]) -> AppResult<()> {
    let mut total = 0usize;
    for attachment in attachments {
        if attachment.bytes.is_empty() {
            return Err(AppError::BadRequest(
                "attachment files cannot be null or empty".into(),
            ));
        }
        if attachment.bytes.len() > MAX_ATTACHMENT {
            return Err(AppError::BadRequest(format!(
                "attachment '{}' exceeds maximum size",
                attachment.name
            )));
        }
        total += attachment.bytes.len();
    }
    if total > MAX_TOTAL_ATTACHMENTS {
        return Err(AppError::BadRequest(
            "total attachment size exceeds maximum".into(),
        ));
    }
    Ok(())
}

/// `POST /api/v1/misc/flatten-portfolio` — drops the catalog `/Collection`
/// dictionary so the file opens as a regular PDF, keeping the embedded
/// files. Files without a `/Collection` dictionary are rejected like Java.
#[stirling_macros::endpoint]
fn flatten_portfolio(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    _params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    let mut document = pdfdoc::load(&upload.name, &upload.bytes)?;
    let is_portfolio = document
        .catalog()
        .ok()
        .and_then(|catalog| catalog.get(b"Collection").ok())
        .is_some_and(|object| pdfdoc::peek_dict(&document, object).is_some());
    if !is_portfolio {
        return Err(AppError::BadRequest(
            "the provided PDF is not a portfolio and cannot be flattened".into(),
        ));
    }
    if let Ok(catalog) = document.catalog_mut() {
        catalog.remove(b"Collection");
    }
    set_use_attachments(&mut document)?;
    pdfdoc::touch_loaded_info(&mut document);
    let bytes = pdfdoc::save(&mut document)?;
    Ok(pdf_response(
        bytes,
        &output_name(Some(simple_name(&upload.name)), ".pdf"),
    ))
}

/// `POST /api/v1/misc/create-portfolio` — bundles the `files` parts into a
/// new PDF: one A4 cover page plus every file embedded under the catalog's
/// `/Names /EmbeddedFiles` tree and a `/Collection` dictionary. No Java
/// fallback is needed; every input is a plain byte embed.
pub async fn create_portfolio(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, extras, params) =
        crate::upload::receive_files_multipart(multipart, &["files"]).await?;
    dispatch(state, query, uploads, params, move |_st, _q, _u, p| {
        create_portfolio_work(extras, p)
    })
    .await
}

fn create_portfolio_work(
    extras: Vec<(String, Upload)>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let files: Vec<Upload> = extras
        .into_iter()
        .filter(|(field, _)| field == "files")
        .map(|(_, u)| u)
        .collect();
    if files.is_empty() {
        return Err(AppError::BadRequest("at least one file is required".into()));
    }
    let mut total = 0usize;
    for file in &files {
        if file.bytes.is_empty() {
            return Err(AppError::BadRequest(
                "portfolio files cannot be null or empty".into(),
            ));
        }
        if file.bytes.len() > MAX_ATTACHMENT {
            return Err(AppError::BadRequest(format!(
                "file '{}' exceeds maximum size",
                file.name
            )));
        }
        total += file.bytes.len();
    }
    if total > MAX_TOTAL_ATTACHMENTS {
        return Err(AppError::BadRequest("total size exceeds maximum".into()));
    }

    // Java defaults coverTitle to "PDF Portfolio" and treats blank as unset.
    let title = {
        let raw = param(&params, "coverTitle", "");
        if raw.trim().is_empty() {
            "PDF Portfolio"
        } else {
            raw
        }
    };

    let bytes = build_portfolio(&files, title)?;
    Ok(pdf_response(bytes, "portfolio.pdf"))
}

fn catalog_root(doc: &Document) -> AppResult<ObjectId> {
    doc.trailer
        .get(b"Root")
        .ok()
        .and_then(|root| root.as_reference().ok())
        .ok_or_else(|| AppError::Pdf("missing catalog".into()))
}

fn pages_root_id(doc: &Document) -> AppResult<ObjectId> {
    let catalog = doc.catalog().map_err(|e| AppError::Pdf(e.to_string()))?;
    catalog
        .get(b"Pages")
        .ok()
        .and_then(|pages| pages.as_reference().ok())
        .ok_or_else(|| AppError::Pdf("catalog /Pages is not a reference".into()))
}

/// `AttachmentUtils.setCatalogViewerPreferences(USE_ATTACHMENTS)`: catalog
/// `/PageMode` plus `/ViewerPreferences` `/NonFullScreenPageMode` and
/// `/DisplayDocTitle`, mutating an indirect preferences dict in place.
fn set_use_attachments(doc: &mut Document) -> AppResult<()> {
    let catalog_id = catalog_root(doc)?;
    enum PrefsSlot {
        Indirect(ObjectId),
        Inline,
        Missing,
    }
    let slot = {
        let catalog = doc
            .get_dictionary(catalog_id)
            .map_err(|e| AppError::Pdf(e.to_string()))?;
        match catalog.get(b"ViewerPreferences") {
            Ok(Object::Reference(id))
                if pdfdoc::peek_dict(doc, &Object::Reference(*id)).is_some() =>
            {
                PrefsSlot::Indirect(*id)
            }
            Ok(Object::Dictionary(_)) => PrefsSlot::Inline,
            _ => PrefsSlot::Missing,
        }
    };
    let mut prefs = match slot {
        PrefsSlot::Indirect(id) => doc.get_dictionary(id).cloned().unwrap_or_default(),
        PrefsSlot::Inline => doc
            .get_dictionary(catalog_id)
            .ok()
            .and_then(|catalog| catalog.get(b"ViewerPreferences").ok())
            .and_then(pdfdoc::dict_of)
            .cloned()
            .unwrap_or_default(),
        PrefsSlot::Missing => Dictionary::new(),
    };
    prefs.set(
        "NonFullScreenPageMode",
        Object::Name(b"UseAttachments".to_vec()),
    );
    prefs.set("DisplayDocTitle", true);

    doc.get_dictionary_mut(catalog_id)
        .map_err(|e| AppError::Pdf(e.to_string()))?
        .set("PageMode", Object::Name(b"UseAttachments".to_vec()));
    match slot {
        PrefsSlot::Indirect(id) => {
            doc.objects.insert(id, Object::Dictionary(prefs));
        }
        _ => {
            doc.get_dictionary_mut(catalog_id)
                .map_err(|e| AppError::Pdf(e.to_string()))?
                .set("ViewerPreferences", Object::Dictionary(prefs));
        }
    }
    Ok(())
}

/// The cover page: A4 with the portfolio title at 24pt and two lines of
/// body text at 12pt, positioned exactly like `PortfolioService.addCoverPage`.
fn add_cover_page(doc: &mut Document, title: &str, item_count: usize) -> AppResult<()> {
    let page_id = doc.new_object_id();
    let content_id = doc.new_object_id();
    let font_id = doc.new_object_id();

    let mut font = Dictionary::new();
    font.set("Type", "Font");
    font.set("Subtype", "Type1");
    font.set("BaseFont", "Helvetica");
    font.set("Encoding", "WinAnsiEncoding");
    doc.objects.insert(font_id, Object::Dictionary(font));

    let mut content: Vec<u8> = Vec::new();
    content.extend_from_slice(b"BT /F1 24 Tf 60 760 Td (");
    push_pdf_literal(&mut content, &winansi_safe(title));
    content.extend_from_slice(b") Tj ET\n");
    let body1 = format!("This document is a PDF Portfolio containing {item_count} file(s).");
    content.extend_from_slice(b"BT /F1 12 Tf 60 720 Td (");
    push_pdf_literal(&mut content, body1.as_bytes());
    content.extend_from_slice(b") Tj ET\n");
    content.extend_from_slice(b"BT /F1 12 Tf 60 700 Td (");
    push_pdf_literal(
        &mut content,
        b"Open it in a viewer that supports portfolios to browse the files.",
    );
    content.extend_from_slice(b") Tj ET\n");
    doc.objects.insert(
        content_id,
        Object::Stream(Stream::new(Dictionary::new(), content)),
    );

    let mut font_map = Dictionary::new();
    font_map.set("F1", Object::Reference(font_id));
    let mut resources = Dictionary::new();
    resources.set("Font", Object::Dictionary(font_map));

    let mut page = Dictionary::new();
    page.set("Type", "Page");
    page.set("Parent", pages_root_id(doc)?);
    page.set(
        "MediaBox",
        Object::Array(vec![
            Object::Integer(0),
            Object::Integer(0),
            Object::Integer(595),
            Object::Integer(842),
        ]),
    );
    page.set("Resources", Object::Dictionary(resources));
    page.set("Contents", Object::Reference(content_id));
    doc.objects.insert(page_id, Object::Dictionary(page));

    let pages_id = pages_root_id(doc)?;
    let kids = {
        let pages = doc
            .get_dictionary(pages_id)
            .map_err(|e| AppError::Pdf(e.to_string()))?;
        let mut kids = pages
            .get(b"Kids")
            .ok()
            .and_then(|k| k.as_array().ok())
            .cloned()
            .unwrap_or_default();
        kids.push(Object::Reference(page_id));
        let count = pages
            .get(b"Count")
            .ok()
            .and_then(|c| c.as_i64().ok())
            .unwrap_or(0);
        (kids, count + 1)
    };
    let pages = doc
        .get_dictionary_mut(pages_id)
        .map_err(|e| AppError::Pdf(e.to_string()))?;
    pages.set("Kids", Object::Array(kids.0));
    pages.set("Count", kids.1);
    Ok(())
}

/// `PortfolioService.toWinAnsiSafe`: keeps printable Latin-1, maps
/// everything else — including unpaired UTF-16 units — to `?`.
pub(crate) fn winansi_safe(text: &str) -> Vec<u8> {
    text.chars().map(winansi_byte).collect()
}

/// One char → its Windows-1252 byte, `?` when unmapped. Standard-14 fonts
/// encode through WinAnsiEncoding, which is CP1252: the 0x80–0x9F slots
/// hold punctuation, not C1 controls.
fn winansi_byte(ch: char) -> u8 {
    let c = ch as u32;
    if (0x20..=0x7E).contains(&c) || (0xA0..=0xFF).contains(&c) {
        return c as u8;
    }
    match c {
        0x20AC => 0x80,
        0x201A => 0x82,
        0x0192 => 0x83,
        0x201E => 0x84,
        0x2026 => 0x85,
        0x2020 => 0x86,
        0x2021 => 0x87,
        0x02C6 => 0x88,
        0x2030 => 0x89,
        0x0160 => 0x8A,
        0x2039 => 0x8B,
        0x0152 => 0x8C,
        0x017D => 0x8E,
        0x2018 => 0x91,
        0x2019 => 0x92,
        0x201C => 0x93,
        0x201D => 0x94,
        0x2022 => 0x95,
        0x2013 => 0x96,
        0x2014 => 0x97,
        0x02DC => 0x98,
        0x2122 => 0x99,
        0x0161 => 0x9A,
        0x203A => 0x9B,
        0x0153 => 0x9C,
        0x017E => 0x9E,
        0x0178 => 0x9F,
        _ => b'?',
    }
}

/// Appends `bytes` to a PDF literal string body, escaping `(`, `)` and `\`.
/// Input must already be WinAnsi-sanitized so every byte is printable.
pub(crate) fn push_pdf_literal(out: &mut Vec<u8>, bytes: &[u8]) {
    for &b in bytes {
        if matches!(b, b'(' | b')' | b'\\') {
            out.push(b'\\');
        }
        out.push(b);
    }
}

/// `PortfolioService.uniqueName`: `name (2).ext`, `name (3).ext`, … until
/// the candidate is unused.
fn unique_portfolio_name(candidate: String, taken: &mut HashSet<String>) -> String {
    if !taken.contains(&candidate) {
        taken.insert(candidate.clone());
        return candidate;
    }
    let dot = candidate.rfind('.').filter(|&d| d > 0);
    let (base, extension) = match dot {
        Some(d) => (candidate[..d].to_string(), candidate[d..].to_string()),
        None => (candidate.clone(), String::new()),
    };
    for suffix in 2..=taken.len() + 2 {
        let next = format!("{base} ({suffix}){extension}");
        if !taken.contains(&next) {
            taken.insert(next.clone());
            return next;
        }
    }
    let next = format!("{base} ({}){extension}", taken.len() + 3);
    taken.insert(next.clone());
    next
}

/// `PortfolioService.resolveFilename`: basename, or `file_<n>` when blank.
fn portfolio_filename(file: &Upload, index: usize) -> String {
    let name = simple_name(&file.name);
    if name.trim().is_empty() {
        format!("file_{}", index + 1)
    } else {
        name.to_string()
    }
}

/// `/CollectionField` entries of the portfolio schema: the viewer fills
/// Name/Description/Size/ModDate from each file specification.
fn collection_field(subtype: &str, display_name: &str, order: i64) -> Object {
    let mut field = Dictionary::new();
    field.set("Type", "CollectionField");
    field.set("Subtype", Object::Name(subtype.as_bytes().to_vec()));
    field.set("N", crate::attachments::text_string_object(display_name));
    field.set("O", order);
    field.set("V", true);
    Object::Dictionary(field)
}

/// Builds the portfolio document: cover page, embedded-file name tree and
/// `/Collection` dictionary, matching `PortfolioService.buildPortfolio`.
fn build_portfolio(files: &[Upload], title: &str) -> AppResult<Vec<u8>> {
    let mut doc = pdfdoc::new_document();
    add_cover_page(&mut doc, title, files.len())?;

    let date = crate::attachments::pdf_date_now();
    let mut taken = HashSet::new();
    let mut names_array = Vec::with_capacity(files.len() * 2);
    let mut first_file_name = None;

    for (index, file) in files.iter().enumerate() {
        let filename = unique_portfolio_name(portfolio_filename(file, index), &mut taken);

        let mut stream_dict = Dictionary::new();
        stream_dict.set("Type", "EmbeddedFile");
        if let Some(content_type) = file
            .content_type
            .as_deref()
            .filter(|ct| !ct.trim().is_empty())
        {
            stream_dict.set("Subtype", Object::Name(content_type.as_bytes().to_vec()));
        }
        let mut params = Dictionary::new();
        params.set("Size", file.bytes.len() as i64);
        params.set(
            "CreationDate",
            crate::attachments::text_string_object(&date),
        );
        params.set("ModDate", crate::attachments::text_string_object(&date));
        stream_dict.set("Params", Object::Dictionary(params));

        let stream_id = doc.new_object_id();
        doc.objects.insert(
            stream_id,
            Object::Stream(Stream::new(stream_dict, file.bytes.to_vec())),
        );

        let mut ef = Dictionary::new();
        ef.set("F", Object::Reference(stream_id));
        ef.set("UF", Object::Reference(stream_id));
        let mut spec = Dictionary::new();
        spec.set("Type", "Filespec");
        spec.set("F", crate::attachments::text_string_object(&filename));
        spec.set("UF", crate::attachments::text_string_object(&filename));
        spec.set(
            "Desc",
            crate::attachments::text_string_object(&format!("Portfolio item: {filename}")),
        );
        spec.set("EF", Object::Dictionary(ef));
        let spec_id = doc.new_object_id();
        doc.objects.insert(spec_id, Object::Dictionary(spec));

        names_array.push(crate::attachments::text_string_object(&filename));
        names_array.push(Object::Reference(spec_id));
        if first_file_name.is_none() {
            first_file_name = Some(filename);
        }
    }

    let embedded_root_id = doc.new_object_id();
    let mut embedded_root = Dictionary::new();
    embedded_root.set("Names", Object::Array(names_array));
    doc.objects
        .insert(embedded_root_id, Object::Dictionary(embedded_root));

    let names_id = doc.new_object_id();
    let mut names = Dictionary::new();
    names.set("EmbeddedFiles", Object::Reference(embedded_root_id));
    doc.objects.insert(names_id, Object::Dictionary(names));

    let catalog_id = catalog_root(&doc)?;
    {
        let catalog = doc
            .get_dictionary_mut(catalog_id)
            .map_err(|e| AppError::Pdf(e.to_string()))?;
        catalog.set("Names", Object::Reference(names_id));

        let mut collection = Dictionary::new();
        collection.set("Type", "Collection");
        collection.set("View", Object::Name(b"D".to_vec()));
        if let Some(first) = &first_file_name {
            collection.set("D", crate::attachments::text_string_object(first));
        }
        let mut schema = Dictionary::new();
        schema.set("Type", "CollectionSchema");
        schema.set("Name", collection_field("F", "Name", 0));
        schema.set("Description", collection_field("Desc", "Description", 1));
        schema.set("Size", collection_field("Size", "Size", 2));
        schema.set("ModDate", collection_field("ModDate", "Modified", 3));
        collection.set("Schema", Object::Dictionary(schema));
        catalog.set("Collection", Object::Dictionary(collection));
    }

    // Some viewers fall back to the attachments pane when they cannot render
    // the collection UI — same PageMode switch Java applies.
    set_use_attachments(&mut doc)?;
    pdfdoc::touch_loaded_info(&mut doc);
    pdfdoc::save(&mut doc)
}

// ---------------------------------------------------------------------------
// unlock-pdf-forms / repair / add-page-numbers / auto-rotate-pdf /
// replace-invert-pdf / add-image / add-comments
// ---------------------------------------------------------------------------

/// `POST /api/v1/misc/unlock-pdf-forms` — clears the read-only field flag,
/// drops `/Lock` entries, sets `/NeedAppearances`, and rewrites XFA
/// `access="readOnly"` to `access="open"`, like `UnlockPDFFormsController`.
#[stirling_macros::endpoint]
fn unlock_pdf_forms(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    _params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    let mut document = pdfdoc::load(&upload.name, &upload.bytes)?;
    unlock_forms(&mut document)?;
    pdfdoc::touch_loaded_info(&mut document);
    let bytes = pdfdoc::save(&mut document)?;
    Ok(pdf_response(
        bytes,
        &output_name(Some(simple_name(&upload.name)), ".pdf"),
    ))
}

fn unlock_forms(doc: &mut Document) -> AppResult<()> {
    let catalog_id = catalog_root(doc)?;
    let acro_slot = doc
        .get_object(catalog_id)
        .map_err(|e| AppError::Pdf(e.to_string()))?
        .as_dict()
        .ok()
        .and_then(|d| d.get(b"AcroForm").ok().cloned());
    let Some(acro_slot) = acro_slot else {
        return Ok(());
    };

    // Fields tree: every dictionary reachable from /Fields, matching
    // PDFBox's getFieldTree() walk (Kids recursion, cycle-guarded).
    let mut field_ids = Vec::new();
    let mut visited = HashSet::new();
    let fields_obj = match &acro_slot {
        Object::Reference(id) => doc
            .get_object(*id)
            .map_err(|e| AppError::Pdf(e.to_string()))?
            .as_dict()
            .ok()
            .and_then(|d| d.get(b"Fields").ok().cloned()),
        Object::Dictionary(d) => d.get(b"Fields").ok().cloned(),
        _ => None,
    };
    if let Some(fields) = fields_obj {
        let (_, resolved) = doc
            .dereference(&fields)
            .map_err(|e| AppError::Pdf(e.to_string()))?;
        if let Object::Array(items) = resolved {
            for item in items {
                collect_field_dicts(doc, item, &mut field_ids, &mut visited, 0)?;
            }
        }
    }

    for id in field_ids {
        let dict = doc
            .get_dictionary_mut(id)
            .map_err(|e| AppError::Pdf(e.to_string()))?;
        dict.remove(b"Lock");
        if let Ok(ff) = dict.get(b"Ff").and_then(|o| o.as_i64()) {
            if ff & 1 != 0 {
                dict.set("Ff", Object::Integer(ff & !1));
            }
        }
    }

    // NeedAppearances lives on the AcroForm dictionary itself.
    match &acro_slot {
        Object::Reference(id) => {
            let dict = doc
                .get_dictionary_mut(*id)
                .map_err(|e| AppError::Pdf(e.to_string()))?;
            dict.set("NeedAppearances", Object::Boolean(true));
        }
        Object::Dictionary(dict) => {
            let mut dict = dict.clone();
            dict.set("NeedAppearances", Object::Boolean(true));
            // Inline AcroForm dicts are rare; rewrite the catalog slot.
            let catalog = doc
                .get_dictionary_mut(catalog_id)
                .map_err(|e| AppError::Pdf(e.to_string()))?;
            catalog.set("AcroForm", Object::Dictionary(dict));
        }
        _ => {}
    }

    unlock_xfa(doc, &acro_slot)?;
    Ok(())
}

fn collect_field_dicts(
    doc: &Document,
    object: &Object,
    ids: &mut Vec<ObjectId>,
    visited: &mut HashSet<ObjectId>,
    depth: usize,
) -> AppResult<()> {
    if depth > 100 {
        return Ok(());
    }
    let Ok(id) = object.as_reference() else {
        return Ok(());
    };
    if !visited.insert(id) {
        return Ok(());
    }
    let Some(dict) = pdfdoc::dict_of(
        doc.get_object(id)
            .map_err(|e| AppError::Pdf(e.to_string()))?,
    ) else {
        return Ok(());
    };
    let dict = dict.clone();
    ids.push(id);
    if let Ok(kids) = dict.get(b"Kids") {
        let (_, resolved) = doc
            .dereference(kids)
            .map_err(|e| AppError::Pdf(e.to_string()))?;
        if let Object::Array(items) = resolved {
            for kid in items {
                collect_field_dicts(doc, kid, ids, visited, depth + 1)?;
            }
        }
    }
    Ok(())
}

/// XFA streams carry `access="readOnly"` in their datasets XML; the Java
/// port regex-replaces it with `access="open"` on every stream in the
/// `/XFA` entry (a lone stream or the `[name, stream, …]` array form).
fn unlock_xfa(doc: &mut Document, acro_slot: &Object) -> AppResult<()> {
    let acro_dict = match acro_slot {
        Object::Reference(id) => pdfdoc::dict_of(
            doc.get_object(*id)
                .map_err(|e| AppError::Pdf(e.to_string()))?,
        )
        .cloned(),
        Object::Dictionary(d) => Some(d.clone()),
        _ => None,
    };
    let Some(xfa) = acro_dict.and_then(|d| d.get(b"XFA").ok().cloned()) else {
        return Ok(());
    };
    let (xfa_id, resolved) = doc
        .dereference(&xfa)
        .map_err(|e| AppError::Pdf(e.to_string()))?;
    // Collect stream ids first: `resolved` borrows `doc`, so the rewrites
    // (which need `&mut doc`) run only after the borrow ends. The id
    // `dereference` returns is the deepest reference — the stream's own.
    let mut stream_ids = Vec::new();
    match resolved {
        Object::Stream(_) => {
            if let Some(id) = xfa_id {
                stream_ids.push(id);
            }
        }
        Object::Array(items) => {
            for pair in items.chunks(2) {
                if let Some(stream_obj) = pair.get(1) {
                    if let Ok((Some(id), _)) = doc.dereference(stream_obj) {
                        stream_ids.push(id);
                    }
                }
            }
        }
        _ => {}
    }
    for id in stream_ids {
        rewrite_xfa_stream(doc, id)?;
    }
    Ok(())
}

fn rewrite_xfa_stream(doc: &mut Document, id: ObjectId) -> AppResult<()> {
    let Some(Object::Stream(stream)) = doc.objects.get(&id).cloned() else {
        return Ok(());
    };
    rewrite_stream_bytes(doc, id, stream)
}

fn rewrite_stream_bytes(doc: &mut Document, id: ObjectId, stream: Stream) -> AppResult<()> {
    let Ok(raw) = stream.decompressed_content() else {
        return Ok(());
    };
    let Ok(xml) = String::from_utf8(raw) else {
        return Ok(());
    };
    let unlocked = xfa_unlock_access(&xml);
    if unlocked == xml {
        return Ok(());
    }
    let mut dict = stream.dict;
    dict.remove(b"Filter");
    dict.remove(b"DecodeParms");
    dict.remove(b"Length");
    let mut new_stream = Stream::new(dict, unlocked.into_bytes());
    let _ = new_stream.compress();
    doc.objects.insert(id, Object::Stream(new_stream));
    Ok(())
}

/// Literal port of the Java regex `access\s*=\s*"readOnly"` →
/// `access="open"` (case-sensitive, `\s` = ASCII whitespace).
fn xfa_unlock_access(xml: &str) -> String {
    const NEEDLE: &str = "access";
    let bytes = xml.as_bytes();
    let mut out = String::with_capacity(xml.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(NEEDLE.as_bytes()) {
            let mut j = i + NEEDLE.len();
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'=' {
                j += 1;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if xml[j..].starts_with("\"readOnly\"") {
                    out.push_str("access=\"open\"");
                    i = j + "\"readOnly\"".len();
                    continue;
                }
            }
        }
        // Copy one char (UTF-8 safe).
        let ch_len = utf8_len(bytes[i]);
        out.push_str(&xml[i..i + ch_len]);
        i += ch_len;
    }
    out
}

fn utf8_len(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first < 0xE0 {
        2
    } else if first < 0xF0 {
        3
    } else {
        4
    }
}

/// `POST /api/v1/misc/add-page-numbers` — standard-14 text at a 3×3 grid
/// position, ported from `PageNumbersController` including margin factors,
/// zero-padding, `{n}`/`{total}`/`{filename}` placeholders and the
/// per-font ascent/descent baseline math.
#[stirling_macros::endpoint]
fn add_page_numbers(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    let mut document = pdfdoc::load(&upload.name, &upload.bytes)?;
    let total_pages = pdfdoc::page_ids(&document).len();

    let font_type = param(&params, "fontType", "helvetica").to_ascii_lowercase();
    let base_font = match font_type.as_str() {
        "courier" => "Courier",
        "times" => "Times-Roman",
        _ => "Helvetica",
    };

    let position_raw = param(&params, "position", "8");
    let position: i64 = position_raw
        .trim()
        .parse()
        .map_err(|_| AppError::BadRequest(format!("invalid position: {position_raw}")))?;
    let position = position.clamp(1, 9);

    let mut custom_text = param(&params, "customText", "{n}").to_string();
    if custom_text.is_empty() {
        custom_text = "{n}".into();
    }
    let mut pages_to_number = param(&params, "pagesToNumber", "all").to_string();
    if pages_to_number.is_empty() {
        pages_to_number = "all".into();
    }
    let starting_number: i64 = param(&params, "startingNumber", "1")
        .trim()
        .parse()
        .map_err(|_| AppError::BadRequest("invalid startingNumber".into()))?;
    let font_size: f64 = param(&params, "fontSize", "12")
        .trim()
        .parse()
        .map_err(|_| AppError::BadRequest("invalid fontSize".into()))?;
    let font_color = parse_pdf_color(param(&params, "fontColor", "#000000"));
    let zero_pad: i64 = param(&params, "zeroPad", "0")
        .trim()
        .parse()
        .map_err(|_| AppError::BadRequest("invalid zeroPad".into()))?;

    let margin_factor = match param(&params, "customMargin", "medium")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "small" => 0.02,
        "large" => 0.05,
        "x-large" => 0.075,
        _ => 0.035,
    };

    page_select::require_valid_page_selection(&pages_to_number, total_pages)
        .map_err(AppError::BadRequest)?;
    let page_list = page_select::parse_page_list(&pages_to_number, total_pages)
        .map_err(AppError::BadRequest)?;

    let filename = simple_name(&upload.name);
    let filename_no_ext = remove_extension(Some(filename));
    let page_ids = pdfdoc::page_ids(&document);

    for (page_number, index) in (starting_number..).zip(page_list) {
        let page_id = page_ids[index];
        let n_formatted = if zero_pad > 0 {
            format!("{:0width$}", page_number, width = zero_pad as usize)
        } else {
            page_number.to_string()
        };
        let text = custom_text
            .replace("{n}", &n_formatted)
            .replace("{total}", &total_pages.to_string())
            .replace("{filename}", &filename_no_ext);

        let encoded = winansi_safe(&text);
        let text_width = crate::fontmetrics::string_width(base_font, &encoded) / 1000.0 * font_size;
        let (ascent, descent) = crate::fontmetrics::ascent_descent(base_font);
        let ascent = ascent / 1000.0 * font_size;
        let descent = descent / 1000.0 * font_size;

        let media = pdfdoc::media_box(&document, page_id)?;
        let (llx, lly, urx, ury) = (media[0], media[1], media[2], media[3]);
        let width = urx - llx;
        let height = ury - lly;
        let left_x = llx + margin_factor * width;
        let mid_x = llx + width / 2.0;
        let right_x = urx - margin_factor * width;
        let bot_y = lly + margin_factor * height;
        let mid_y = lly + height / 2.0;
        let top_y = ury - margin_factor * height;

        let col = (position - 1) % 3 + 1;
        let row = (position - 1) / 3 + 1;
        let x = match col {
            1 => left_x,
            2 => mid_x - text_width / 2.0,
            _ => right_x - text_width,
        };
        let y = match row {
            1 => top_y - ascent,
            2 => mid_y - (ascent + descent) / 2.0,
            _ => bot_y,
        };

        let font_name = ensure_page_font(&mut document, page_id, base_font)?;
        append_page_content(
            &mut document,
            page_id,
            &format!(
                "BT /{font_name} {font_size} Tf {r} {g} {b} rg 1 0 0 1 {x} {y} Tm <{}> Tj ET\n",
                hex_bytes(&encoded),
                r = font_color[0],
                g = font_color[1],
                b = font_color[2],
            ),
        )?;
    }

    pdfdoc::touch_loaded_info(&mut document);
    let bytes = pdfdoc::save(&mut document)?;
    Ok(pdf_response(
        bytes,
        &output_name(Some(simple_name(&upload.name)), ".pdf"),
    ))
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02X}"));
    }
    out
}

/// `Color.decode` subset: `#RRGGBB`, `#AARRGGBB`, decimal integer, else
/// black — the Java fallback on `NumberFormatException`.
fn parse_pdf_color(value: &str) -> [f64; 3] {
    let black = [0.0, 0.0, 0.0];
    let v = value.trim();
    let hex = v.strip_prefix('#').unwrap_or(v);
    if v.starts_with('#') && (hex.len() == 6 || hex.len() == 8) {
        if let Ok(n) = u64::from_str_radix(&hex[hex.len() - 6..], 16) {
            return [
                ((n >> 16) & 0xFF) as f64 / 255.0,
                ((n >> 8) & 0xFF) as f64 / 255.0,
                (n & 0xFF) as f64 / 255.0,
            ];
        }
        return black;
    }
    if let Ok(n) = v.parse::<i64>() {
        let n = n as u32;
        return [
            ((n >> 16) & 0xFF) as f64 / 255.0,
            ((n >> 8) & 0xFF) as f64 / 255.0,
            (n & 0xFF) as f64 / 255.0,
        ];
    }
    black
}

/// Registers a standard-14 font on the page's `/Resources` under the
/// next free `F<n>` name (PDFBox's `PDResources.add(font, "F")` scheme)
/// and returns the resource name. The resolved (possibly inherited)
/// resource dict is copied to page level so shared/ancestor dicts are
/// never mutated.
fn ensure_page_font(doc: &mut Document, page_id: ObjectId, base_font: &str) -> AppResult<String> {
    let mut resources = pdfdoc::page_resources_dict(doc, page_id)?.unwrap_or_default();

    let mut font_names: Vec<String> = Vec::new();
    if let Ok(fonts) = resources.get(b"Font") {
        let fonts = fonts.clone();
        let (_, resolved) = doc
            .dereference(&fonts)
            .map_err(|e| AppError::Pdf(e.to_string()))?;
        if let Object::Dictionary(fd) = resolved {
            for (k, _) in fd.iter() {
                font_names.push(String::from_utf8_lossy(k).to_string());
            }
        }
        // An indirect /Font dict must be inlined before merging, or the
        // upsert below would drop every sibling entry.
        if matches!(fonts, Object::Reference(_)) {
            if let Object::Dictionary(_) = resolved {
                resources.set("Font", resolved.clone());
            }
        }
    }
    let mut n = 1;
    let font_name = loop {
        let candidate = format!("F{n}");
        if !font_names.contains(&candidate) {
            break candidate;
        }
        n += 1;
    };

    let mut font_dict = Dictionary::new();
    font_dict.set("Type", Object::Name(b"Font".to_vec()));
    font_dict.set("Subtype", Object::Name(b"Type1".to_vec()));
    font_dict.set("BaseFont", Object::Name(base_font.as_bytes().to_vec()));
    font_dict.set("Encoding", Object::Name(b"WinAnsiEncoding".to_vec()));
    upsert_font_entry(&mut resources, font_name.as_str(), font_dict);

    let page = doc
        .get_dictionary_mut(page_id)
        .map_err(|e| AppError::Pdf(e.to_string()))?;
    page.set("Resources", Object::Dictionary(resources));
    Ok(font_name)
}

/// Inserts `/<font_name> <font_dict>` into a Resources dictionary's
/// `/Font` sub-dictionary, creating the sub-dict when absent or
/// indirect-shared (cloned inline so siblings keep their own).
fn upsert_font_entry(resources: &mut Dictionary, font_name: &str, font_dict: Dictionary) {
    let mut fonts = match resources.get(b"Font").ok().cloned() {
        Some(Object::Dictionary(d)) => d,
        _ => Dictionary::new(),
    };
    fonts.set(font_name, Object::Dictionary(font_dict));
    resources.set("Font", Object::Dictionary(fonts));
}

/// Appends raw PDF graphics operators to a page's `/Contents`, creating
/// the stream when absent and extending the last stream of an array —
/// the placement `PDPageContentStream(APPEND)` uses.
pub(crate) fn append_page_content(
    doc: &mut Document,
    page_id: ObjectId,
    ops: &str,
) -> AppResult<()> {
    let contents_slot = doc
        .get_object(page_id)
        .map_err(|e| AppError::Pdf(e.to_string()))?
        .as_dict()
        .ok()
        .and_then(|d| d.get(b"Contents").ok().cloned());

    match contents_slot {
        Some(Object::Reference(id)) => append_to_stream(doc, id, ops)?,
        Some(Object::Array(mut items)) => {
            // Concatenated content streams render identically; appending a
            // new stream avoids decompressing the last one.
            let stream_id = doc.new_object_id();
            let mut stream = Stream::new(Dictionary::new(), ops.as_bytes().to_vec());
            let _ = stream.compress();
            doc.objects.insert(stream_id, Object::Stream(stream));
            items.push(Object::Reference(stream_id));
            let page = doc
                .get_dictionary_mut(page_id)
                .map_err(|e| AppError::Pdf(e.to_string()))?;
            page.set("Contents", Object::Array(items));
        }
        _ => push_content_stream(doc, page_id, ops)?,
    }
    Ok(())
}

fn append_to_stream(doc: &mut Document, id: ObjectId, ops: &str) -> AppResult<()> {
    let Some(obj) = doc.objects.get(&id).cloned() else {
        return Ok(());
    };
    let Object::Stream(stream) = obj else {
        return Ok(());
    };
    let mut content = stream
        .decompressed_content()
        .unwrap_or_else(|_| stream.content.clone());
    content.extend_from_slice(ops.as_bytes());
    let mut dict = stream.dict;
    dict.remove(b"Filter");
    dict.remove(b"DecodeParms");
    dict.remove(b"Length");
    let mut new_stream = Stream::new(dict, content);
    let _ = new_stream.compress();
    doc.objects.insert(id, Object::Stream(new_stream));
    Ok(())
}

fn push_content_stream(doc: &mut Document, page_id: ObjectId, ops: &str) -> AppResult<()> {
    let id = doc.new_object_id();
    let mut stream = Stream::new(Dictionary::new(), ops.as_bytes().to_vec());
    let _ = stream.compress();
    doc.objects.insert(id, Object::Stream(stream));
    let page = doc
        .get_dictionary_mut(page_id)
        .map_err(|e| AppError::Pdf(e.to_string()))?;
    page.set("Contents", Object::Reference(id));
    Ok(())
}

/// `POST /api/v1/misc/replace-invert-pdf` — the CMYK color-space
/// conversion runs Ghostscript natively; the raster-based modes
/// (full inversion, custom/high-contrast recolouring) need a renderer
/// and text layout the Rust backend does not have, so they are
/// rejected.
pub async fn replace_invert_pdf(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, params) = receive_multipart(multipart).await?;
    let mode = param(&params, "replaceAndInvertOption", "");
    if mode != "COLOR_SPACE_CONVERSION" {
        return Err(AppError::BadRequest(format!(
            "replaceAndInvertOption '{mode}' is not supported; only COLOR_SPACE_CONVERSION is implemented"
        )));
    }
    dispatch(state, query, uploads, params, replace_invert_cmyk_work).await
}

fn replace_invert_cmyk_work(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    _params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    let dir = tempfile::tempdir()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("cmyk tempdir: {e}")))?;
    let input_path = dir.path().join("cmyk-in.pdf");
    let output_path = dir.path().join("cmyk-out.pdf");
    std::fs::write(&input_path, &upload.bytes)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("cmyk input: {e}")))?;

    let status = std::process::Command::new(crate::convert::resolve(
        &crate::settings::engine_program("gs"),
    ))
    .arg("-sDEVICE=pdfwrite")
    .arg("-dCompatibilityLevel=1.5")
    .arg("-dPDFSETTINGS=/prepress")
    .arg("-dNOPAUSE")
    .arg("-dQUIET")
    .arg("-dBATCH")
    .arg("-sProcessColorModel=DeviceCMYK")
    .arg("-sColorConversionStrategy=CMYK")
    .arg("-sColorConversionStrategyForImages=CMYK")
    .arg(format!("-sOutputFile={}", output_path.display()))
    .arg(&input_path)
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .status()
    .map_err(|e| AppError::BadRequest(format!("Ghostscript unavailable: {e}")))?;
    if !status.success() {
        return Err(AppError::BadRequest(
            "CMYK color space conversion failed".into(),
        ));
    }
    let bytes = std::fs::read(&output_path)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("cmyk output: {e}")))?;
    Ok(pdf_response(
        bytes,
        &output_name(Some(simple_name(&upload.name)), ".pdf"),
    ))
}

/// `POST /api/v1/misc/add-image` — overlays a raster image at PDF
/// user-space (x, y) on the first page or every page. Anything the
/// `image` crate cannot decode (SVG, …) is rejected.
pub async fn add_image(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, extras, params) = receive_files_multipart(multipart, &["imageFile"]).await?;
    // The first `imageFile` part decides raster support; `add_image_work`
    // decodes through the `image` crate, so every format it knows
    // converts natively.
    let first_image = extras
        .iter()
        .find(|(field, _)| field == "imageFile")
        .map(|(_, u)| u);
    let raster = first_image.is_some_and(|u| image::guess_format(&u.bytes).is_ok());
    if !raster {
        return Err(AppError::BadRequest(
            "imageFile must be a supported raster image".into(),
        ));
    }

    let mut extras = extras.into_iter();
    let Some((_, image)) = extras.find(|(field, _)| field == "imageFile") else {
        return Err(AppError::BadRequest("imageFile is required".into()));
    };
    let work = move |state: AppState,
                     query: HashMap<String, String>,
                     uploads: Vec<Upload>,
                     params: Vec<(String, String)>| {
        add_image_work(state, query, uploads, params, image)
    };
    if jobs::async_requested(&query, &params) {
        let work_state = state.clone();
        return jobs::submit(&state, async move {
            tokio::task::spawn_blocking(move || {
                work(work_state, query, uploads, params).map(IntoResponse::into_response)
            })
            .await
            .map_err(|e| AppError::Internal(anyhow::Error::new(e)))?
        })
        .await;
    }
    tokio::task::spawn_blocking(move || {
        work(state, query, uploads, params).map(IntoResponse::into_response)
    })
    .await
    .map_err(|e| AppError::Internal(anyhow::Error::new(e)))?
}

fn add_image_work(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
    image_upload: Upload,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    let x: f64 = param(&params, "x", "0")
        .trim()
        .parse()
        .map_err(|_| AppError::BadRequest("invalid x".into()))?;
    let y: f64 = param(&params, "y", "0")
        .trim()
        .parse()
        .map_err(|_| AppError::BadRequest("invalid y".into()))?;
    let every_page = param_bool(&params, "everyPage");

    let decoded = image::load_from_memory(&image_upload.bytes)
        .map_err(|_| AppError::BadRequest("imageFile must be a supported image".into()))?;
    let (width, height, rgb, alpha) = if decoded.color().has_alpha() {
        let rgba = decoded.to_rgba8();
        let (w, h) = rgba.dimensions();
        let mut rgb_buf = Vec::with_capacity((w * h * 3) as usize);
        let mut alpha_buf = Vec::with_capacity((w * h) as usize);
        for px in rgba.pixels() {
            rgb_buf.extend_from_slice(&[px[0], px[1], px[2]]);
            alpha_buf.push(px[3]);
        }
        (w, h, rgb_buf, Some(alpha_buf))
    } else {
        let rgb_img = decoded.to_rgb8();
        let (w, h) = rgb_img.dimensions();
        (w, h, rgb_img.into_raw(), None)
    };

    let mut document = pdfdoc::load(&upload.name, &upload.bytes)?;
    let page_ids = pdfdoc::page_ids(&document);
    let targets: Vec<ObjectId> = if every_page {
        page_ids
    } else {
        page_ids.into_iter().take(1).collect()
    };

    // One image XObject shared by every target page, like a single
    // PDImageXObject.createFromByteArray result.
    let image_id = embed_rgb_image(&mut document, width, height, rgb)?;
    if let Some(alpha) = alpha {
        let mask_id = embed_gray_image(&mut document, width, height, alpha)?;
        if let Some(Object::Stream(stream)) = document.objects.get_mut(&image_id) {
            stream.dict.set("SMask", Object::Reference(mask_id));
        }
    }

    for page_id in targets {
        let name = register_page_xobject(&mut document, page_id, image_id)?;
        append_page_content(
            &mut document,
            page_id,
            &format!("q 1 0 0 1 {x} {y} cm /{name} Do Q\n"),
        )?;
    }

    pdfdoc::touch_loaded_info(&mut document);
    let bytes = pdfdoc::save(&mut document)?;
    Ok(pdf_response(
        bytes,
        &output_name(Some(simple_name(&upload.name)), ".pdf"),
    ))
}

/// FlateDecode `/Image` XObject holding DeviceRGB samples.
fn embed_rgb_image(
    doc: &mut Document,
    width: u32,
    height: u32,
    rgb: Vec<u8>,
) -> AppResult<ObjectId> {
    let mut dict = Dictionary::new();
    dict.set("Type", Object::Name(b"XObject".to_vec()));
    dict.set("Subtype", Object::Name(b"Image".to_vec()));
    dict.set("Width", Object::Integer(width as i64));
    dict.set("Height", Object::Integer(height as i64));
    dict.set("ColorSpace", Object::Name(b"DeviceRGB".to_vec()));
    dict.set("BitsPerComponent", Object::Integer(8));
    let mut stream = Stream::new(dict, rgb);
    let _ = stream.compress();
    let id = doc.new_object_id();
    doc.objects.insert(id, Object::Stream(stream));
    Ok(id)
}

/// `/SMask` source: a DeviceGray image the same size as its parent.
fn embed_gray_image(
    doc: &mut Document,
    width: u32,
    height: u32,
    gray: Vec<u8>,
) -> AppResult<ObjectId> {
    let mut dict = Dictionary::new();
    dict.set("Type", Object::Name(b"XObject".to_vec()));
    dict.set("Subtype", Object::Name(b"Image".to_vec()));
    dict.set("Width", Object::Integer(width as i64));
    dict.set("Height", Object::Integer(height as i64));
    dict.set("ColorSpace", Object::Name(b"DeviceGray".to_vec()));
    dict.set("BitsPerComponent", Object::Integer(8));
    let mut stream = Stream::new(dict, gray);
    let _ = stream.compress();
    let id = doc.new_object_id();
    doc.objects.insert(id, Object::Stream(stream));
    Ok(id)
}

/// Adds `/<name> <image-ref>` to the page's `/Resources` `/XObject`
/// dict under the next free `Im<n>` name, returning the resource name.
/// Like `ensure_page_font`, the resolved resource dict is copied to
/// page level so shared/ancestor dicts are never mutated.
fn register_page_xobject(
    doc: &mut Document,
    page_id: ObjectId,
    image_id: ObjectId,
) -> AppResult<String> {
    let mut resources = pdfdoc::page_resources_dict(doc, page_id)?.unwrap_or_default();

    let mut names: Vec<String> = Vec::new();
    if let Ok(xobj) = resources.get(b"XObject") {
        let xobj = xobj.clone();
        let (_, resolved) = doc
            .dereference(&xobj)
            .map_err(|e| AppError::Pdf(e.to_string()))?;
        if let Object::Dictionary(xd) = resolved {
            for (k, _) in xd.iter() {
                names.push(String::from_utf8_lossy(k).to_string());
            }
        }
        // Same indirect-dict hazard as /Font: inline before merging.
        if matches!(xobj, Object::Reference(_)) {
            if let Object::Dictionary(_) = resolved {
                resources.set("XObject", resolved.clone());
            }
        }
    }
    let mut n = 1;
    let name = loop {
        let candidate = format!("Im{n}");
        if !names.contains(&candidate) {
            break candidate;
        }
        n += 1;
    };

    // The image stream's own dictionary describes it (including /SMask),
    // so the resource value is a bare reference.
    upsert_xobject(&mut resources, name.as_str(), image_id);
    let page = doc
        .get_dictionary_mut(page_id)
        .map_err(|e| AppError::Pdf(e.to_string()))?;
    page.set("Resources", Object::Dictionary(resources));
    Ok(name)
}

/// Inserts `/<name> <image-ref>` into a Resources dictionary's
/// `/XObject` sub-dict, creating it when absent or shared-indirect
/// (a shared dict is replaced inline so sibling pages keep theirs).
fn upsert_xobject(resources: &mut Dictionary, name: &str, image_id: ObjectId) {
    let mut xobjects = match resources.get(b"XObject").ok().cloned() {
        Some(Object::Dictionary(d)) => d,
        _ => Dictionary::new(),
    };
    xobjects.set(name, Object::Reference(image_id));
    resources.set("XObject", Object::Dictionary(xobjects));
}

/// `POST /api/v1/misc/add-comments` — sticky-note (`/Text`) annotations
/// from a JSON spec list. Specs carrying `anchorText` need PDF text
/// location, which is not implemented; coordinate-only specs run
/// natively.
pub async fn add_comments(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, params) = receive_multipart(multipart).await?;
    let comments_raw = params
        .iter()
        .rev()
        .find(|(n, _)| n == "comments")
        .map(|(_, v)| v.clone());
    let parsed = comments_raw
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Vec<serde_json::Value>>(raw).ok());
    let Some(specs) = parsed else {
        return Err(AppError::BadRequest("comments must be a JSON array".into()));
    };
    let anchored = specs.iter().any(|s| {
        s.get("anchorText")
            .and_then(|v| v.as_str())
            .is_some_and(|t| !t.trim().is_empty())
    });
    if anchored {
        return Err(AppError::BadRequest(
            "comments specs with anchorText are not supported".into(),
        ));
    }
    dispatch(state, query, uploads, params, add_comments_work).await
}

fn add_comments_work(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    let raw = param(&params, "comments", "");
    let specs: Vec<serde_json::Value> = serde_json::from_str(raw)
        .map_err(|_| AppError::BadRequest("comments must be a JSON array".into()))?;

    let mut document = pdfdoc::load(&upload.name, &upload.bytes)?;
    let page_ids = pdfdoc::page_ids(&document);
    let total_pages = page_ids.len();
    let now = pdf_date_now();

    for spec in &specs {
        let Some(loc) = spec.get("pageIndex").and_then(|v| v.as_i64()) else {
            continue;
        };
        let f = |key: &str| spec.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0);
        let (x, y, w, h) = (f("x"), f("y"), f("width"), f("height"));
        let text = spec.get("text").and_then(|v| v.as_str()).unwrap_or("");
        if text.trim().is_empty()
            || text.encode_utf16().count() > 100_000
            || w <= 0.0
            || h <= 0.0
            || loc < 0
            || loc as usize >= total_pages
        {
            continue;
        }
        let author = spec
            .get("author")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("Stirling AI");
        let subject = spec
            .get("subject")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("Stirling AI Comment");

        let mut annot = Dictionary::new();
        annot.set("Type", Object::Name(b"Annot".to_vec()));
        annot.set("Subtype", Object::Name(b"Text".to_vec()));
        annot.set(
            "Rect",
            Object::Array(vec![
                Object::Real(x as f32),
                Object::Real(y as f32),
                Object::Real((x + w) as f32),
                Object::Real((y + h) as f32),
            ]),
        );
        annot.set("Contents", crate::attachments::text_string_object(text));
        annot.set("T", crate::attachments::text_string_object(author));
        annot.set("Subj", crate::attachments::text_string_object(subject));
        annot.set(
            "C",
            Object::Array(vec![
                Object::Real(1.0),
                Object::Real(0.95),
                Object::Real(0.4),
            ]),
        );
        annot.set("CA", Object::Real(0.9));
        annot.set("Name", Object::Name(b"Comment".to_vec()));
        annot.set("M", crate::attachments::text_string_object(&now));
        annot.set("CreationDate", crate::attachments::text_string_object(&now));

        let annot_id = document.new_object_id();
        document.objects.insert(annot_id, Object::Dictionary(annot));
        let page_id = page_ids[loc as usize];
        append_annotation(&mut document, page_id, annot_id)?;
    }

    pdfdoc::touch_loaded_info(&mut document);
    let bytes = pdfdoc::save(&mut document)?;
    Ok(pdf_response(
        bytes,
        &output_name(Some(simple_name(&upload.name)), ".pdf"),
    ))
}

/// Appends an annotation reference to a page's `/Annots` array,
/// creating the array when absent (PDFBox `getAnnotations().add`).
fn append_annotation(doc: &mut Document, page_id: ObjectId, annot_id: ObjectId) -> AppResult<()> {
    let annots_slot = doc
        .get_object(page_id)
        .map_err(|e| AppError::Pdf(e.to_string()))?
        .as_dict()
        .ok()
        .and_then(|d| d.get(b"Annots").ok().cloned());
    match annots_slot {
        Some(Object::Reference(id)) => {
            if let Some(Object::Array(mut items)) = doc.objects.get(&id).cloned() {
                items.push(Object::Reference(annot_id));
                doc.objects.insert(id, Object::Array(items));
            }
        }
        Some(Object::Array(mut items)) => {
            items.push(Object::Reference(annot_id));
            let page = doc
                .get_dictionary_mut(page_id)
                .map_err(|e| AppError::Pdf(e.to_string()))?;
            page.set("Annots", Object::Array(items));
        }
        _ => {
            let page = doc
                .get_dictionary_mut(page_id)
                .map_err(|e| AppError::Pdf(e.to_string()))?;
            page.set("Annots", Object::Array(vec![Object::Reference(annot_id)]));
        }
    }
    Ok(())
}

/// `D:yyyyMMddHHmmss` PDF date for annotation timestamps.
fn pdf_date_now() -> String {
    let now = chrono::Utc::now();
    now.format("D:%Y%m%d%H%M%S").to_string()
}
/// `POST /api/v1/misc/remove-blanks` — classifies each page like
/// `BlankPageController` (text first, then a rendered white-pixel ratio
/// for pages with images) and returns a ZIP with the kept and blank page
/// sets as separate PDFs.
///
/// Divergence: when Ghostscript is unavailable or fails to render a page,
/// the white ratio falls back to an estimate from the drawn image XObjects
/// (decoded via `imgcomp`) weighted by their CTM coverage, with any vector
/// painting marking the page non-blank — the estimate only ever errs
/// toward keeping.
#[stirling_macros::endpoint]
fn remove_blanks(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    let threshold: i64 = param(&params, "threshold", "10")
        .trim()
        .parse()
        .map_err(|_| AppError::BadRequest("invalid threshold".into()))?;
    let white_percent: f64 = param(&params, "whitePercent", "99.9")
        .trim()
        .parse()
        .map_err(|_| AppError::BadRequest("invalid whitePercent".into()))?;

    let document = pdfdoc::load(&upload.name, &upload.bytes)?;
    let page_ids = pdfdoc::page_ids(&document);
    let rasterizer = BlankRasterizer::probe(&upload.name, &upload.bytes);
    let mut kept = Vec::new();
    let mut blanked = Vec::new();
    for (index, &page_id) in page_ids.iter().enumerate() {
        if page_is_blank(
            &document,
            page_id,
            index as u32,
            rasterizer.as_ref(),
            threshold,
            white_percent,
        )? {
            blanked.push(index as u32);
        } else {
            kept.push(index as u32);
        }
    }

    let base = remove_extension(Some(simple_name(&upload.name)));
    let mut entries = Vec::new();
    if kept.is_empty() {
        let all: Vec<u32> = (0..page_ids.len() as u32).collect();
        entries.push((
            format!("{base}_allBlankPages.pdf"),
            subset_pdf_bytes(&upload.name, &upload.bytes, &all)?,
        ));
    } else {
        entries.push((
            format!("{base}_nonBlankPages.pdf"),
            subset_pdf_bytes(&upload.name, &upload.bytes, &kept)?,
        ));
        if !blanked.is_empty() {
            entries.push((
                format!("{base}_blankPages.pdf"),
                subset_pdf_bytes(&upload.name, &upload.bytes, &blanked)?,
            ));
        }
    }
    crate::handlers::split::zip_response(entries, &format!("{base}_processed.zip"))
}

/// Ghostscript page rasteriser shared by `remove-blanks` and
/// `auto-split-pdf`'s QR divider detection. `probe` writes the PDF once
/// and verifies the binary; each page renders through `png16m`.
pub(crate) struct BlankRasterizer {
    program: std::path::PathBuf,
    input: std::path::PathBuf,
    dir: tempfile::TempDir,
}

impl BlankRasterizer {
    pub(crate) fn probe(name: &str, bytes: &[u8]) -> Option<Self> {
        let program = crate::convert::resolve(&crate::settings::engine_program("gs"));
        let ok = std::process::Command::new(&program)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            return None;
        }
        let dir = tempfile::tempdir().ok()?;
        let input = dir.path().join(format!("{name}.pdf"));
        std::fs::write(&input, bytes).ok()?;
        Some(Self {
            program,
            input,
            dir,
        })
    }

    /// One rendered page as an RGB image, or `None` when gs fails.
    pub(crate) fn render(&self, page: u32, dpi: u32) -> Option<image::DynamicImage> {
        let out = self.dir.path().join(format!("page-{page}-{dpi}.png"));
        let status = std::process::Command::new(&self.program)
            .args([
                "-dNOPAUSE",
                "-dBATCH",
                "-q",
                "-sDEVICE=png16m",
                &format!("-r{dpi}"),
                &format!("-dFirstPage={page}"),
                &format!("-dLastPage={page}"),
                &format!("-sOutputFile={}", out.display()),
            ])
            .arg(&self.input)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()?;
        if !status.success() {
            return None;
        }
        let png = std::fs::read(&out).ok()?;
        image::load_from_memory(&png).ok()
    }

    /// White-pixel percentage of one rendered page at 150 DPI —
    /// `BlankPageController`'s `renderImageWithDPI` equivalent (Java's
    /// `maxDPI` clamp has no Rust settings counterpart) — or `None` when
    /// gs fails so the caller can fall back.
    fn white_percent(&self, page: u32, threshold: i64) -> Option<f64> {
        let image = self.render(page, 150)?.to_rgb8();
        let (width, height) = (image.width(), image.height());
        if width == 0 || height == 0 {
            return None;
        }
        let white_floor = 255 - threshold;
        let white = image
            .pixels()
            .filter(|p| i64::from(p.0[2]) >= white_floor)
            .count() as f64;
        Some(white / (width as f64 * height as f64) * 100.0)
    }
}

fn subset_pdf_bytes(name: &str, bytes: &[u8], order: &[u32]) -> AppResult<Vec<u8>> {
    let mut doc = pdfdoc::load(name, bytes)?;
    pdfdoc::subset(&mut doc, order)?;
    pdfdoc::touch_loaded_info(&mut doc);
    pdfdoc::save(&mut doc)
}

fn page_is_blank(
    doc: &Document,
    page_id: ObjectId,
    page_index: u32,
    rasterizer: Option<&BlankRasterizer>,
    threshold: i64,
    white_percent: f64,
) -> AppResult<bool> {
    // `PDFTextStripper`: the positioned extractor shared with the filter
    // endpoints decodes through font encodings and form XObjects.
    let has_text = crate::text::page_items(doc, page_id)
        .iter()
        .any(|item| !item.text.trim().is_empty());
    if has_text {
        return Ok(false);
    }
    let resources = pdfdoc::page_resources(doc, page_id)?;
    let mut image_visited = HashSet::new();
    let has_images = match &resources {
        Some((_, object)) => resources_contain_image(doc, object, &mut image_visited, 0)?,
        None => false,
    };
    if !has_images {
        return Ok(true);
    }
    match rasterizer.and_then(|r| r.white_percent(page_index + 1, threshold)) {
        Some(white) => Ok(white >= white_percent),
        // No faithful raster check without gs; keeping the page errs
        // toward retention.
        None => Ok(false),
    }
}

/// `PdfUtils.hasImagesOnPage`: any `/Image` XObject reachable through the
/// resource dictionaries, descending into form XObjects' own resources.
fn resources_contain_image(
    doc: &Document,
    resources: &Object,
    visited: &mut HashSet<ObjectId>,
    depth: usize,
) -> AppResult<bool> {
    if depth > 64 {
        return Ok(false);
    }
    let Some(dict) = pdfdoc::dict_owned(doc, resources)? else {
        return Ok(false);
    };
    let Ok(xobjects) = dict.get(b"XObject") else {
        return Ok(false);
    };
    let xobjects = xobjects.clone();
    let Some(xd) = pdfdoc::dict_owned(doc, &xobjects)? else {
        return Ok(false);
    };
    for (_, value) in xd.iter() {
        let value = value.clone();
        let Some(xobj_dict) = pdfdoc::peek_dict(doc, &value) else {
            continue;
        };
        match dict_name(doc, xobj_dict, b"Subtype").as_deref() {
            Some(name) if name == b"Image" => return Ok(true),
            Some(name) if name == b"Form" => {
                let Some(form_id) = (match &value {
                    Object::Reference(id) => Some(*id),
                    _ => None,
                }) else {
                    continue;
                };
                if !visited.insert(form_id) {
                    continue;
                }
                if let Ok(form_resources) = xobj_dict.get(b"Resources") {
                    let form_resources = form_resources.clone();
                    if resources_contain_image(doc, &form_resources, visited, depth + 1)? {
                        return Ok(true);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(false)
}

/// A dictionary entry that may be an indirect name.
pub(crate) fn dict_name(doc: &Document, dict: &Dictionary, key: &[u8]) -> Option<Vec<u8>> {
    match dict.get(key).ok()? {
        Object::Name(n) => Some(n.clone()),
        Object::Reference(id) => match doc.objects.get(id) {
            Some(Object::Name(n)) => Some(n.clone()),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit;
    use lopdf::dictionary;

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

    fn params(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    async fn response_document(response: Response) -> Document {
        testkit::response_document(response).await
    }

    async fn response_bytes(response: Response) -> Vec<u8> {
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec()
    }

    fn text_of(object: &Object) -> String {
        match object {
            Object::String(bytes, _) => String::from_utf8_lossy(bytes).to_string(),
            other => format!("{other:?}"),
        }
    }

    /// Concatenated, decompressed `/Contents` bytes of a page.
    fn content_bytes(doc: &Document, page_id: ObjectId) -> Vec<u8> {
        let mut out = Vec::new();
        let mut stack: Vec<Object> = doc
            .get_dictionary(page_id)
            .ok()
            .and_then(|d| d.get(b"Contents").ok().cloned())
            .into_iter()
            .collect();
        while let Some(object) = stack.pop() {
            let resolved = match &object {
                Object::Reference(id) => doc.get_object(*id).cloned().unwrap_or(object),
                _ => object,
            };
            match resolved {
                Object::Stream(stream) => out.extend(
                    stream
                        .decompressed_content()
                        .unwrap_or_else(|_| stream.content.clone()),
                ),
                Object::Array(items) => stack.extend(items),
                _ => {}
            }
        }
        out
    }

    fn page_dict(doc: &Document, page_id: ObjectId) -> Dictionary {
        doc.get_dictionary(page_id).unwrap().clone()
    }

    fn catalog_dict(doc: &Document) -> Dictionary {
        let id = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        doc.get_dictionary(id).unwrap().clone()
    }

    #[tokio::test]
    async fn update_metadata_sets_standard_fields() {
        let mut pdf = testkit::Pdf::new();
        let bytes = pdf.bytes();
        let response = update_metadata_work(
            state(),
            HashMap::new(),
            upload("in.pdf", bytes),
            params(&[("title", "Hello"), ("author", "Me")]),
        )
        .unwrap();
        let doc = response_document(response).await;
        let info = doc.trailer.get(b"Info").unwrap().as_dict().unwrap().clone();
        assert_eq!(text_of(info.get(b"Title").unwrap()), "Hello");
        assert_eq!(text_of(info.get(b"Author").unwrap()), "Me");
    }

    #[tokio::test]
    async fn update_metadata_delete_all_clears_info() {
        let mut pdf = testkit::Pdf::new();
        pdf.set_info(dictionary! {
            "Title" => Object::string_literal("Old"),
        });
        let bytes = pdf.bytes();
        let response = update_metadata_work(
            state(),
            HashMap::new(),
            upload("in.pdf", bytes),
            params(&[("deleteAll", "true")]),
        )
        .unwrap();
        let doc = response_document(response).await;
        let info = doc.trailer.get(b"Info").unwrap().as_dict().unwrap();
        assert!(info.get(b"Title").is_err());
    }

    #[tokio::test]
    async fn add_page_numbers_draws_text_on_each_page() {
        let mut pdf = testkit::Pdf::new();
        pdf.add_page([0, 0, 300, 400], Dictionary::new());
        let bytes = pdf.bytes();
        let response = add_page_numbers_work(
            state(),
            HashMap::new(),
            upload("doc.pdf", bytes),
            params(&[
                ("fontType", "helvetica"),
                ("position", "8"),
                ("customText", "{n}/{total}"),
                ("fontSize", "12"),
            ]),
        )
        .unwrap();
        let doc = response_document(response).await;
        for page_id in pdfdoc::page_ids(&doc) {
            let content = content_bytes(&doc, page_id);
            assert!(content.windows(2).any(|w| w == b"Tj"), "no Tj op");
            assert!(content.windows(3).any(|w| w == b" Tf"), "no Tf op");
            let page = page_dict(&doc, page_id);
            let resources = page.get(b"Resources").unwrap().as_dict().unwrap();
            assert!(resources.get(b"Font").is_ok(), "no /Font resource");
        }
    }

    #[tokio::test]
    async fn add_comments_appends_text_annotation() {
        let mut pdf = testkit::Pdf::new();
        let bytes = pdf.bytes();
        let comments = r#"[{"pageIndex":0,"x":10,"y":20,"width":30,"height":40,"text":"note","author":"alice"}]"#;
        let response = add_comments_work(
            state(),
            HashMap::new(),
            upload("doc.pdf", bytes),
            params(&[("comments", comments)]),
        )
        .unwrap();
        let doc = response_document(response).await;
        let page = page_dict(&doc, pdf.page);
        let annots = page.get(b"Annots").unwrap().as_array().unwrap();
        assert_eq!(annots.len(), 1);
        let annot_id = annots[0].as_reference().unwrap();
        let annot = doc.get_dictionary(annot_id).unwrap();
        assert_eq!(annot.get(b"Subtype").unwrap().as_name().unwrap(), b"Text");
        assert_eq!(text_of(annot.get(b"T").unwrap()), "alice");
    }

    #[tokio::test]
    async fn add_comments_skips_out_of_range_pages() {
        let mut pdf = testkit::Pdf::new();
        let bytes = pdf.bytes();
        let comments = r#"[{"pageIndex":5,"x":1,"y":1,"width":2,"height":2,"text":"x"}]"#;
        let response = add_comments_work(
            state(),
            HashMap::new(),
            upload("doc.pdf", bytes),
            params(&[("comments", comments)]),
        )
        .unwrap();
        let doc = response_document(response).await;
        let page = page_dict(&doc, pdf.page);
        assert!(page.get(b"Annots").is_err());
    }

    #[tokio::test]
    async fn unlock_forms_removes_lock_and_readonly_flag() {
        let mut pdf = testkit::Pdf::new();
        let field_id = pdf.add(dictionary! {
            "FT" => "Tx",
            "T" => Object::string_literal("f"),
            "Ff" => Object::Integer(1),
            "Lock" => dictionary! { "P" => -4 },
        });
        let acro_id = pdf.add(dictionary! {
            "Fields" => vec![Object::Reference(field_id)],
        });
        pdf.edit_catalog(|catalog| {
            catalog.set("AcroForm", Object::Reference(acro_id));
        });
        let bytes = pdf.bytes();

        let response =
            unlock_pdf_forms_work(state(), HashMap::new(), upload("form.pdf", bytes), vec![])
                .unwrap();
        let doc = response_document(response).await;
        let catalog = catalog_dict(&doc);
        let acro = catalog.get(b"AcroForm").unwrap().as_reference().unwrap();
        let acro = doc.get_dictionary(acro).unwrap();
        assert!(matches!(
            acro.get(b"NeedAppearances").unwrap(),
            Object::Boolean(true)
        ));
        // Java walks only the field tree, so /Lock on the AcroForm dict
        // itself is out of scope for both implementations.
        let field = doc.get_dictionary(field_id).unwrap();
        assert!(field.get(b"Lock").is_err());
        assert_eq!(field.get(b"Ff").unwrap().as_i64().unwrap(), 0);
    }

    #[tokio::test]
    async fn remove_blanks_splits_text_and_empty_pages() {
        let mut pdf = testkit::Pdf::new();
        let font = pdf.add(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let stream_id = pdf.add_stream(Dictionary::new(), b"BT /F1 12 Tf 10 700 Td (Hi) Tj ET");
        pdf.edit_page(|page| {
            page.set("Contents", Object::Reference(stream_id));
            page.set(
                "Resources",
                dictionary! {
                    "Font" => dictionary! {
                        "F1" => Object::Reference(font),
                    },
                },
            );
        });
        pdf.add_page([0, 0, 612, 792], Dictionary::new());
        let bytes = pdf.bytes();

        let response =
            remove_blanks_work(state(), HashMap::new(), upload("doc.pdf", bytes), vec![]).unwrap();
        let zip_bytes = response_bytes(response).await;
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip_bytes)).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        assert!(names.contains(&"doc_nonBlankPages.pdf".to_string()));
        assert!(names.contains(&"doc_blankPages.pdf".to_string()));

        let mut non_blank = Vec::new();
        std::io::Read::read_to_end(
            &mut archive.by_name("doc_nonBlankPages.pdf").unwrap(),
            &mut non_blank,
        )
        .unwrap();
        let doc = Document::load_mem(&non_blank).unwrap();
        assert_eq!(pdfdoc::page_ids(&doc).len(), 1);
    }

    #[tokio::test]
    async fn remove_blanks_all_blank_single_entry() {
        let mut pdf = testkit::Pdf::new();
        pdf.add_page([0, 0, 612, 792], Dictionary::new());
        let bytes = pdf.bytes();
        let response =
            remove_blanks_work(state(), HashMap::new(), upload("doc.pdf", bytes), vec![]).unwrap();
        let zip_bytes = response_bytes(response).await;
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip_bytes)).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        assert_eq!(names, vec!["doc_allBlankPages.pdf".to_string()]);
    }

    #[tokio::test]
    async fn create_portfolio_embeds_files_and_cover() {
        let extras = vec![(
            "files".to_string(),
            Upload {
                name: "notes.txt".into(),
                bytes: crate::upload::UploadBytes::from_vec(b"hello".to_vec()),
                content_type: None,
            },
        )];
        let response = create_portfolio_work(extras, vec![]).unwrap();
        let doc = response_document(response).await;
        let catalog = catalog_dict(&doc);
        let collection = catalog.get(b"Collection").unwrap();
        assert!(pdfdoc::peek_dict(&doc, collection).is_some());
        let names = catalog.get(b"Names").unwrap();
        let (_, resolved) = doc.dereference(names).unwrap();
        assert!(resolved.as_dict().unwrap().get(b"EmbeddedFiles").is_ok());
        // Cover page + the portfolio page tree is intact.
        assert!(!pdfdoc::page_ids(&doc).is_empty());
    }

    #[tokio::test]
    async fn flatten_portfolio_removes_collection() {
        let extras = vec![(
            "files".to_string(),
            Upload {
                name: "a.bin".into(),
                bytes: crate::upload::UploadBytes::from_vec(vec![1, 2, 3]),
                content_type: None,
            },
        )];
        let portfolio = create_portfolio_work(extras, vec![]).unwrap();
        let bytes = response_bytes(portfolio).await;

        let response = flatten_portfolio_work(
            state(),
            HashMap::new(),
            upload("portfolio.pdf", bytes),
            vec![],
        )
        .unwrap();
        let doc = response_document(response).await;
        let catalog = catalog_dict(&doc);
        assert!(catalog.get(b"Collection").is_err());
        assert_eq!(
            catalog.get(b"PageMode").unwrap().as_name().unwrap(),
            b"UseAttachments"
        );
    }

    #[tokio::test]
    async fn flatten_portfolio_rejects_non_portfolio() {
        let mut pdf = testkit::Pdf::new();
        let bytes = pdf.bytes();
        let err =
            flatten_portfolio_work(state(), HashMap::new(), upload("plain.pdf", bytes), vec![])
                .unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[tokio::test]
    async fn add_image_embeds_xobject_and_draws_it() {
        let rgb = image::RgbImage::from_pixel(2, 2, image::Rgb([255u8, 0, 0]));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(rgb)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        let image_upload = Upload {
            name: "logo.png".into(),
            bytes: crate::upload::UploadBytes::from_vec(buf.into_inner()),
            content_type: None,
        };

        let mut pdf = testkit::Pdf::new();
        let bytes = pdf.bytes();
        let response = add_image_work(
            state(),
            HashMap::new(),
            upload("doc.pdf", bytes),
            params(&[("x", "10"), ("y", "20")]),
            image_upload,
        )
        .unwrap();
        let doc = response_document(response).await;
        let page = page_dict(&doc, pdf.page);
        let resources = page.get(b"Resources").unwrap().as_dict().unwrap();
        let xobjects = resources.get(b"XObject").unwrap().as_dict().unwrap();
        let image_ref = xobjects.get(b"Im1").unwrap().as_reference().unwrap();
        let stream = doc.get_object(image_ref).unwrap().as_stream().unwrap();
        assert_eq!(
            stream.dict.get(b"Subtype").unwrap().as_name().unwrap(),
            b"Image"
        );
        let content = content_bytes(&doc, pdf.page);
        assert!(content.windows(4).any(|w| w == b" Do "), "no Do op");
    }
}

#[cfg(test)]
mod util_tests {
    use super::*;
    use crate::testkit;
    use lopdf::dictionary;

    fn upload(name: &str, bytes: Vec<u8>) -> Upload {
        Upload {
            name: name.into(),
            bytes: crate::upload::UploadBytes::from_vec(bytes),
            content_type: None,
        }
    }

    #[test]
    fn size_to_bytes_matches_java_units() {
        assert_eq!(size_to_bytes("10"), Some(10 << 20));
        assert_eq!(size_to_bytes("10MB"), Some(10 << 20));
        assert_eq!(
            size_to_bytes("1.5GB"),
            Some((1.5 * 1024.0 * 1024.0 * 1024.0) as u64)
        );
        assert_eq!(size_to_bytes("512KB"), Some(512 << 10));
        assert_eq!(size_to_bytes("2TB"), Some(2u64 << 40));
        assert_eq!(size_to_bytes("100B"), Some(100));
        assert_eq!(
            size_to_bytes("1,5 MB"),
            Some((1.5 * 1024.0 * 1024.0) as u64)
        );
        assert_eq!(size_to_bytes(""), None);
        assert_eq!(size_to_bytes("abc"), None);
        assert_eq!(size_to_bytes("-5MB"), None);
    }

    #[test]
    fn compress_level_tables_are_monotonic() {
        // Higher level → smaller scale factor and JPEG quality.
        for level in 1..9 {
            assert!(scale_factor_for_level(level) > scale_factor_for_level(level + 1));
            assert!(jpeg_quality_for_level(level) > jpeg_quality_for_level(level + 1));
        }
        assert_eq!(scale_factor_for_level(0), 1.0);
        assert_eq!(jpeg_quality_for_level(0), 0.75);
        assert_eq!(scale_factor_for_level(9), 0.28);
        assert_eq!(jpeg_quality_for_level(9), 0.35);
    }

    #[test]
    fn determine_level_tracks_target_ratio() {
        assert_eq!(determine_level(95, 100), 1);
        assert_eq!(determine_level(50, 100), 5);
        assert_eq!(determine_level(5, 100), 9);
        // Zero input size can't divide — Java treats it as ratio 0 → level 9.
        assert_eq!(determine_level(10, 0), 9);
    }

    #[test]
    fn increment_level_escalates_on_oversize() {
        assert_eq!(increment_level(3, 100, 100), 4);
        assert_eq!(increment_level(3, 200, 100), 5); // ratio 2.0 → +2
        assert_eq!(increment_level(3, 500, 100), 6); // ratio 5.0 → +3
        assert_eq!(increment_level(8, 500, 100), 9); // capped at 9
    }

    #[test]
    fn gs_critical_matches_java_fragments() {
        assert!(gs_critical("GPL Ghostscript: Page drawing error"));
        assert!(gs_critical("Could not draw this page"));
        assert!(!gs_critical("processing page 1"));
    }

    #[test]
    fn pdf_date_converts_java_format() {
        assert_eq!(
            pdf_date(Some("2024/03/05 09:30:15")),
            Some("D:20240305093015".into())
        );
        assert_eq!(pdf_date(Some("undefined")), None);
        assert_eq!(pdf_date(Some("")), None);
        assert_eq!(pdf_date(None), None);
        assert_eq!(pdf_date(Some("not a date")), None);
        assert_eq!(pdf_date(Some("2024/3/5 09:30:15")), None); // needs 2-digit parts
    }

    #[test]
    fn parse_pdf_color_handles_java_formats() {
        // #RRGGBB and #AARRGGBB (alpha ignored).
        assert_eq!(parse_pdf_color("#FF0000"), [1.0, 0.0, 0.0]);
        // #AARRGGBB — Color.decode puts alpha first, so the RGB tail is FF0080.
        assert_eq!(parse_pdf_color("#00FF0080"), [1.0, 0.0, 128.0 / 255.0]);
        // Decimal integer, like Java's NumberFormatException fallback.
        assert_eq!(parse_pdf_color("16711680"), [1.0, 0.0, 0.0]);
        assert_eq!(parse_pdf_color("0"), [0.0, 0.0, 0.0]);
        // Garbage → black, matching Java's catch path.
        assert_eq!(parse_pdf_color("red"), [0.0, 0.0, 0.0]);
        assert_eq!(parse_pdf_color(""), [0.0, 0.0, 0.0]);
    }

    #[test]
    fn winansi_safe_maps_non_latin1_to_question_mark() {
        assert_eq!(winansi_safe("abc"), b"abc".to_vec());
        // Latin-1 supplement survives.
        assert_eq!(winansi_safe("é"), vec![0xE9]);
        // Beyond Latin-1 and control chars become '?', one per char even
        // for non-BMP input (a UTF-16 surrogate pair is still one char).
        assert_eq!(winansi_safe("\u{4e2d}"), b"?".to_vec());
        assert_eq!(winansi_safe("\u{1F600}"), b"?".to_vec());
        assert_eq!(winansi_safe("\u{7f}"), b"?".to_vec());
        assert_eq!(winansi_safe("\u{80}"), b"?".to_vec());
        assert_eq!(winansi_safe(""), Vec::<u8>::new());
    }

    #[test]
    fn push_pdf_literal_escapes_pdf_syntax() {
        let mut out = Vec::new();
        push_pdf_literal(&mut out, b"a(b)c\\d");
        assert_eq!(out, b"a\\(b\\)c\\\\d".to_vec());
    }

    #[test]
    fn unique_portfolio_name_allocates_suffixes() {
        let mut taken = HashSet::new();
        assert_eq!(unique_portfolio_name("a.pdf".into(), &mut taken), "a.pdf");
        assert_eq!(
            unique_portfolio_name("a.pdf".into(), &mut taken),
            "a (2).pdf"
        );
        assert_eq!(
            unique_portfolio_name("a.pdf".into(), &mut taken),
            "a (3).pdf"
        );
        // Extensionless names still get the suffix before the end.
        let mut taken = HashSet::new();
        taken.insert("noext".to_string());
        assert_eq!(
            unique_portfolio_name("noext".into(), &mut taken),
            "noext (2)"
        );
    }

    #[test]
    fn portfolio_filename_uses_basename_or_fallback() {
        let f = |name: &str| Upload {
            name: name.into(),
            bytes: crate::upload::UploadBytes::from_vec(vec![]),
            content_type: None,
        };
        assert_eq!(
            portfolio_filename(&f("dir/sub/report.pdf"), 0),
            "report.pdf"
        );
        assert_eq!(portfolio_filename(&f("C:\\win\\file.docx"), 1), "file.docx");
        assert_eq!(portfolio_filename(&f(""), 2), "file_3");
    }

    #[test]
    fn hex_bytes_uppercases() {
        assert_eq!(hex_bytes(&[0x0a, 0xff, 0x10]), "0AFF10");
        assert_eq!(hex_bytes(&[]), "");
    }

    #[tokio::test]
    async fn update_metadata_sets_info_fields() {
        let mut pdf = testkit::Pdf::new();
        pdf.set_info(dictionary! {
            "Title" => Object::string_literal("Old"),
            "Author" => Object::string_literal("Someone"),
        });
        let response = update_metadata_work(
            AppState::new(),
            HashMap::new(),
            vec![upload("in.pdf", pdf.bytes())],
            vec![
                ("title".into(), "New Title".into()),
                ("author".into(), "undefined".into()), // removes the entry
                ("creationDate".into(), "2024/01/02 03:04:05".into()),
                ("trapped".into(), "True".into()),
                ("customKey1".into(), "Department".into()),
                ("customValue1".into(), "R&D".into()),
            ],
        )
        .unwrap();
        let doc = testkit::response_document(response).await;
        assert_eq!(
            pdfdoc::info_value(&doc, "Title").as_deref(),
            Some("New Title")
        );
        assert_eq!(pdfdoc::info_value(&doc, "Author"), None);
        assert_eq!(
            pdfdoc::info_value(&doc, "CreationDate").as_deref(),
            Some("D:20240102030405")
        );
        assert_eq!(
            pdfdoc::info_value(&doc, "Department").as_deref(),
            Some("R&D")
        );
    }

    #[tokio::test]
    async fn update_metadata_delete_all_clears_info() {
        let mut pdf = testkit::Pdf::new();
        pdf.set_info(dictionary! {
            "Title" => Object::string_literal("Old"),
        });
        let response = update_metadata_work(
            AppState::new(),
            HashMap::new(),
            vec![upload("in.pdf", pdf.bytes())],
            vec![("deleteAll".into(), "true".into())],
        )
        .unwrap();
        let doc = testkit::response_document(response).await;
        assert_eq!(pdfdoc::info_value(&doc, "Title"), None);
    }

    #[test]
    fn decompress_pdf_rejects_missing_upload() {
        let err = decompress_pdf_work(AppState::new(), HashMap::new(), vec![], vec![]).unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[tokio::test]
    async fn decompress_pdf_returns_valid_pdf() {
        let bytes = testkit::Pdf::new().bytes();
        let response = decompress_pdf_work(
            AppState::new(),
            HashMap::new(),
            vec![upload("in.pdf", bytes)],
            vec![],
        )
        .unwrap();
        let doc = testkit::response_document(response).await;
        assert_eq!(pdfdoc::page_ids(&doc).len(), 1);
    }

    #[test]
    fn subset_pdf_bytes_reorders_pages() {
        let mut pdf = testkit::Pdf::new();
        pdf.add_page([0, 0, 300, 300], Dictionary::new());
        pdf.add_page([0, 0, 400, 400], Dictionary::new());
        let bytes = pdf.bytes();
        // Reverse order: page 2 then page 1.
        let out = subset_pdf_bytes("in.pdf", &bytes, &[1, 0]).unwrap();
        let doc = Document::load_mem(&out).unwrap();
        let ids = pdfdoc::page_ids(&doc);
        assert_eq!(ids.len(), 2);
        // Index 1 is the 300×300 page; the fixture's first page is 612×792.
        let media = pdfdoc::media_box(&doc, ids[0]).unwrap();
        assert_eq!(media, [0.0, 0.0, 300.0, 300.0]);
        let media = pdfdoc::media_box(&doc, ids[1]).unwrap();
        assert_eq!(media, [0.0, 0.0, 612.0, 792.0]);
    }

    #[test]
    fn blank_classification_reads_real_pdf_text() {
        // Regression: content streams arrive FlateDecode-compressed, so
        // text detection must go through the decoding extractor, not raw
        // content operations.
        let bytes = std::fs::read("../app/common/src/test/resources/example.pdf").unwrap();
        let doc = Document::load_mem(&bytes).unwrap();
        for id in pdfdoc::page_ids(&doc) {
            assert!(!page_is_blank(&doc, id, 0, None, 10, 99.9).unwrap());
        }
    }

    #[test]
    fn blank_page_classification() {
        // A page with no content and no images is blank.
        let mut pdf = testkit::Pdf::new();
        let doc = Document::load_mem(&pdf.bytes()).unwrap();
        let page_id = pdfdoc::page_ids(&doc)[0];
        assert!(page_is_blank(&doc, page_id, 0, None, 10, 99.9).unwrap());

        // A page showing text is not blank.
        let mut pdf = testkit::Pdf::new();
        let font = pdf.add(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let content = pdf.add_stream(dictionary! {}, b"BT /F1 12 Tf 72 720 Td (Hi) Tj ET");
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
        let doc = Document::load_mem(&pdf.bytes()).unwrap();
        let page_id = pdfdoc::page_ids(&doc)[0];
        assert!(!page_is_blank(&doc, page_id, 0, None, 10, 99.9).unwrap());
    }
}
