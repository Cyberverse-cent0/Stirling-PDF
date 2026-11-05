//! Ports of `MiscApi` endpoints migrated so far. These need raw PDF object
//! access, so they run directly on `lopdf` rather than `pdfdoc` helpers.

use std::collections::HashMap;

use axum::extract::{Multipart, OriginalUri, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use lopdf::Object;

use crate::error::{AppError, AppResult};
use crate::handlers::jobs::AppState;
use crate::handlers::merge::{file_response, pdf_response};
use crate::handlers::{jobs, proxy};
use crate::pdfdoc;
use crate::upload::{output_name, param_bool, receive_multipart, Upload, FILE_FIELD};

/// Shared tail of a migrated endpoint: `?async=true` goes through the job
/// framework, everything else runs inline on the blocking pool — the same
/// contract the `#[endpoint]` macro generates. Handlers that must inspect
/// params before deciding (delegation to Java) use this instead of the macro.
async fn dispatch<F>(
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
        return jobs::submit(
            &state,
            async move {
                tokio::task::spawn_blocking(move || {
                    work(work_state, query, uploads, params)
                        .map(IntoResponse::into_response)
                })
                .await
                .map_err(|e| AppError::Internal(anyhow::Error::new(e)))?
            },
        )
        .await;
    }
    tokio::task::spawn_blocking(move || {
        work(state, query, uploads, params).map(IntoResponse::into_response)
    })
    .await
    .map_err(|e| AppError::Internal(anyhow::Error::new(e)))?
}

/// `Filenames.toSimpleFileName`: basename of an uploaded filename.
fn simple_name(name: &str) -> &str {
    name.rsplit(['/', '\\']).next().unwrap_or(name)
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

    let bytes = pdfdoc::save(&mut document)?;

    Ok(pdf_response(
        bytes,
        &output_name(Some(&upload.name), "_decompressed.pdf"),
    ))
}

/// `POST /api/v1/misc/flatten` — `flattenOnlyForms=true` merges AcroForm
/// widget appearances into the page content natively; every other request
/// (full-page rasterisation) is forwarded to Java, which owns the renderer.
/// Hand-written rather than `#[endpoint]` so delegation sees the parsed
/// params before any work is dispatched.
pub async fn flatten(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    uri: OriginalUri,
    multipart: Multipart,
) -> AppResult<Response> {
    let path_and_query = uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/api/v1/misc/flatten")
        .to_string();
    let (uploads, params) = receive_multipart(multipart).await?;
    if !param_bool(&params, "flattenOnlyForms") {
        return Ok(proxy::forward_multipart(
            &state, &path_and_query, &headers, uploads, params,
        )
        .await);
    }
    dispatch(state, query, uploads, params, flatten_work).await
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
    let bytes = pdfdoc::save(&mut document)?;
    // Java returns the upload's own filename, not a tool-suffixed one.
    Ok(pdf_response(bytes, simple_name(&upload.name)))
}

/// `POST /api/v1/misc/compress-pdf` — qpdf/Ghostscript sidecars plus the
/// lopdf image-recompression pass. `lineArt=true` needs the ImageMagick
/// service and is forwarded to Java.
pub async fn compress_pdf(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    uri: OriginalUri,
    multipart: Multipart,
) -> AppResult<Response> {
    let path_and_query = uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/api/v1/misc/compress-pdf")
        .to_string();
    let (uploads, params) = receive_multipart(multipart).await?;
    if param_bool(&params, "lineArt") {
        return Ok(proxy::forward_multipart(
            &state, &path_and_query, &headers, uploads, params,
        )
        .await);
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
    let size = size.trim().to_uppercase().replace(',', ".").replace(' ', "");
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

    let workdir = tempfile::tempdir()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("tempdir: {e}")))?;
    let mut current = upload.bytes.clone();
    let input_size = current.len() as u64;

    let mut size_met = false;
    let mut image_applied = false;
    while !size_met && level <= 9 {
        let mut ghostscript_ok = false;
        if level >= 6 {
            match run_ghostscript(workdir.path(), &current, level, grayscale)? {
                Some(bytes) => {
                    current = bytes;
                    ghostscript_ok = true;
                }
                None => {}
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
        upload.bytes
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

    let mut command = std::process::Command::new("gs");
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

    let mut command = std::process::Command::new("qpdf");
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
