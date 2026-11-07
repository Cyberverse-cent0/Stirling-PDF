//! Misc automation endpoints: `auto-rename`, `auto-rotate-pdf`, `repair`,
//! `auto-split-pdf`, and the `/api/v1/mobile-scanner/*` session API.
//!
//! Modes needing a renderer or Tesseract that has no native port
//! (`auto-rotate-pdf` `detectionMode=osd`) are rejected rather than
//! approximated.

use std::collections::HashMap;
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::multipart::Field;
use axum::extract::{Multipart, Path as AxumPath};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::stream;
use serde_json::{json, Value};

use crate::error::{AppError, AppResult};
use crate::handlers::jobs::AppState;
use crate::handlers::merge::pdf_response;
use crate::handlers::misc;
use crate::pdfdoc;
use crate::settings;
use crate::text::{self, TextItem};
use crate::upload::{output_name, param, param_bool, simple_name, Upload, FILE_FIELD};

fn last_param<'a>(params: &'a [(String, String)], name: &str) -> Option<&'a str> {
    params
        .iter()
        .rev()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

fn param_bool_default(params: &[(String, String)], name: &str, default: bool) -> bool {
    match last_param(params, name) {
        Some(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "true" | "1" | "on" | "yes"
        ),
        None => default,
    }
}

// `io.github.pixee.security.Filenames.toSimpleFileName`: basename only.
// ---------------------------------------------------------------------------
// auto-rename
// ---------------------------------------------------------------------------

/// `AutoRenameController.LINE_LIMIT` is 200, but the stripper's
/// `lineCount < LINE_LIMIT` guard means the 200th line's glyphs are never
/// collected — at most 199 lines reach `lineInfos`.
const RENAME_LINE_LIMIT: usize = 199;

/// `POST /api/v1/misc/auto-rename` — names the download after the largest
/// heading text, like `AutoRenameController`. The PDF bytes are unchanged;
/// only the `Content-Disposition` filename differs.
#[stirling_macros::endpoint]
fn auto_rename(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    let use_first_text_fallback = param_bool(&params, "useFirstTextAsFallback");

    let mut document = pdfdoc::load(&upload.name, &upload.bytes)?;
    let items: Vec<TextItem> = text::document_items(&document).concat();
    let mut lines = text::lines_of(&items);
    lines.truncate(RENAME_LINE_LIMIT);

    // Merge runs of adjacent lines sharing an identical font size, like the
    // Java controller's second pass.
    let mut merged: Vec<(String, f64)> = Vec::new();
    for (line_text, font_size) in lines {
        if let Some(last) = merged.last_mut() {
            if last.1 == font_size {
                last.0.push(' ');
                last.0.push_str(&line_text);
                continue;
            }
        }
        merged.push((line_text, font_size));
    }
    merged.sort_by(|a, b| b.1.total_cmp(&a.1));

    let title = merged.first().map(|(t, _)| t.clone());
    let chosen = match title {
        Some(t) => Some(t),
        None if use_first_text_fallback => merged.last().map(|(t, _)| t.clone()),
        None => None,
    };

    pdfdoc::touch_loaded_info(&mut document);
    let bytes = pdfdoc::save(&mut document)?;
    let name = match chosen {
        Some(header) if header.encode_utf16().count() < 255 => {
            let sanitized: String = header
                .chars()
                .filter(|c| {
                    !matches!(
                        c,
                        '/' | '\\' | '?' | '%' | '*' | ':' | '|' | '"' | '<' | '>'
                    )
                })
                .collect::<String>()
                .trim()
                .to_string();
            format!("{sanitized}.pdf")
        }
        _ => simple_name(&upload.name).to_string(),
    };
    Ok(pdf_response(bytes, &name))
}

// ---------------------------------------------------------------------------
// auto-rotate-pdf
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct PageRotation {
    #[serde(rename = "pageNumber")]
    page_number: Option<i64>,
    rotation: Option<i64>,
}

#[derive(serde::Serialize)]
struct PageResult {
    #[serde(rename = "pageNumber")]
    page_number: usize,
    #[serde(rename = "currentRotation")]
    current_rotation: i64,
    correction: i64,
    confidence: Option<f64>,
    method: &'static str,
    apply: bool,
    note: Option<&'static str>,
}

/// `POST /api/v1/misc/auto-rotate-pdf`. `detectionMode=osd` needs page
/// rendering plus Tesseract, which has no native port and is rejected.
/// The `text`/`auto` paths run natively on the positioned text extractor;
/// pages the text signal cannot decide report `tesseractUnavailable`
/// rather than a wrong guess.
#[stirling_macros::endpoint]
fn auto_rotate_pdf(
    state: AppState,
    query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let mode = param(&params, "detectionMode", "auto").to_lowercase();
    if !matches!(mode.as_str(), "auto" | "text" | "osd") {
        return Err(AppError::BadRequest(
            "detectionMode must be one of auto, text, osd".into(),
        ));
    }
    if mode == "osd" {
        // OSD orientation detection runs Tesseract over rasterised
        // pages — no native port.
        return Err(AppError::BadRequest(
            "detectionMode=osd is not implemented; the Rust backend has no Tesseract OSD path"
                .into(),
        ));
    }
    auto_rotate_impl(state, query, uploads, params)
}

fn auto_rotate_impl(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };

    // Explicit rotations skip detection entirely, like the Java controller;
    // an empty list falls through to detection.
    let explicit = match last_param(&params, "pageRotations") {
        Some(raw) if !raw.trim().is_empty() => Some(
            serde_json::from_str::<Vec<PageRotation>>(raw)
                .map_err(|e| AppError::BadRequest(format!("invalid pageRotations: {e}")))?,
        ),
        _ => None,
    };
    if let Some(rotations) = explicit.filter(|r| !r.is_empty()) {
        let mut document = pdfdoc::load(&upload.name, &upload.bytes)?;
        apply_explicit_rotations(&mut document, &rotations)?;
        pdfdoc::touch_loaded_info(&mut document);
        let bytes = pdfdoc::save(&mut document)?;
        return Ok(pdf_response(
            bytes,
            &output_name(Some(&upload.name), "_auto_rotated.pdf"),
        ));
    }

    // Parsed for the @Min(0) validation; the value itself only gates OSD,
    // which the handler rejects before reaching this function.
    if let Some(raw) = last_param(&params, "confidenceThreshold") {
        if !raw.trim().is_empty() {
            let threshold: f64 = raw
                .trim()
                .parse()
                .map_err(|_| AppError::BadRequest("confidenceThreshold must be a number".into()))?;
            if threshold < 0.0 {
                return Err(AppError::BadRequest(
                    "confidenceThreshold must be non-negative".into(),
                ));
            }
        }
    }
    let dry_run = param_bool(&params, "dryRun");
    let infer_undetected = param_bool_default(&params, "inferUndetected", true);
    let mode = param(&params, "detectionMode", "auto").to_lowercase();

    let mut document = pdfdoc::load(&upload.name, &upload.bytes)?;
    let page_ids = pdfdoc::page_ids(&document);
    let page_count = page_ids.len();

    // Text-direction detection for every page in one pass, like Java's
    // DirectionCountingStripper. `osd` never reaches this function — the
    // handler rejects it.
    let mut text_directions: Vec<(i64, f64, usize)> = Vec::with_capacity(page_count);
    for id in &page_ids {
        let counts = text::page_direction_counts(&text::page_items(&document, *id));
        let total: usize = counts.iter().sum();
        let (best_index, best_count) = counts
            .iter()
            .enumerate()
            .max_by_key(|(_, c)| **c)
            .map(|(i, c)| (i, *c))
            .unwrap_or((0, 0));
        let dominance = if total == 0 {
            0.0
        } else {
            best_count as f64 / total as f64
        };
        text_directions.push((best_index as i64 * 90, dominance, total));
    }

    let mut results: Vec<PageResult> = Vec::with_capacity(page_count);
    for (i, id) in page_ids.iter().enumerate() {
        let current_rotation = pdfdoc::rotation(&document, *id)?;
        let (dominant, dominance, glyph_count) = text_directions[i];
        let mut result = PageResult {
            page_number: i + 1,
            current_rotation,
            correction: 0,
            confidence: None,
            method: "none",
            apply: false,
            note: None,
        };

        // AutoRotateDetection.isConclusive: 30 glyphs at 95% dominance, or
        // 8 glyphs at 99%.
        let conclusive =
            (glyph_count >= 30 && dominance >= 0.95) || (glyph_count >= 8 && dominance >= 0.99);
        if conclusive {
            // correctionFromTextDirection: floorMod(dominant - rotation, 360)
            let correction = (dominant - current_rotation).rem_euclid(360);
            result.method = "text";
            result.correction = correction;
            result.confidence = Some(dominance * 100.0);
            result.apply = correction != 0;
        } else if mode == "text" {
            result.note = Some(if glyph_count < 30 {
                "tooFewGlyphs"
            } else {
                "noDominantDirection"
            });
        } else {
            // auto mode: Java would try Tesseract OSD here. Rust has no
            // renderer or OCR integration, so the page stays undecided
            // rather than guessing.
            result.note = Some("tesseractUnavailable");
        }
        results.push(result);
    }

    if infer_undetected {
        infer_pages(&mut results);
    }

    if dry_run {
        return Ok(axum::Json(analysis_json(&results, page_count)).into_response());
    }

    for result in &results {
        if result.apply {
            let id = page_ids[result.page_number - 1];
            pdfdoc::set_rotation(
                &mut document,
                id,
                (result.current_rotation + result.correction).rem_euclid(360),
            )?;
        }
    }
    pdfdoc::touch_loaded_info(&mut document);
    let bytes = pdfdoc::save(&mut document)?;
    Ok(pdf_response(
        bytes,
        &output_name(Some(&upload.name), "_auto_rotated.pdf"),
    ))
}

/// Java `AutoRotateDetection.correctionFromTextDirection` is
/// `floorMod(dominantDirection - pageRotation, 360)`; the per-page block
/// above applies it inline.
fn infer_pages(results: &mut [PageResult]) {
    // rotation -> the single agreed correction, or conflicted
    let mut consensus: HashMap<i64, i64> = HashMap::new();
    let mut conflicted: Vec<i64> = Vec::new();
    for result in results.iter() {
        if result.method == "none" {
            continue;
        }
        let rotation = result.current_rotation;
        if conflicted.contains(&rotation) {
            continue;
        }
        match consensus.get(&rotation) {
            None => {
                consensus.insert(rotation, result.correction);
            }
            Some(existing) if *existing != result.correction => {
                conflicted.push(rotation);
                consensus.remove(&rotation);
            }
            _ => {}
        }
    }
    for result in results.iter_mut() {
        if result.method != "none" {
            continue;
        }
        let Some(correction) = consensus.get(&result.current_rotation).copied() else {
            continue;
        };
        result.method = "inferred";
        result.correction = correction;
        result.confidence = None;
        result.apply = correction != 0;
        result.note = Some("inferredFromDocument");
    }
}

fn analysis_json(results: &[PageResult], total_pages: usize) -> Value {
    let pages: Vec<Value> = results
        .iter()
        .map(|r| serde_json::to_value(r).unwrap_or(Value::Null))
        .collect();
    let count = |method: &str| results.iter().filter(|r| r.method == method).count();
    json!({
        "pages": pages,
        "totalPages": total_pages,
        "pagesToRotate": results.iter().filter(|r| r.apply).count(),
        "detectedByText": count("text"),
        "detectedByOsd": count("osd"),
        "inferred": count("inferred"),
        "undetected": count("none"),
    })
}

fn apply_explicit_rotations(
    document: &mut lopdf::Document,
    rotations: &[PageRotation],
) -> AppResult<()> {
    let page_ids = pdfdoc::page_ids(document);
    let page_count = page_ids.len();
    let mut seen: Vec<i64> = Vec::new();
    for entry in rotations {
        let (Some(page_number), Some(angle)) = (entry.page_number, entry.rotation) else {
            return Err(AppError::BadRequest(
                "pageRotations entries must have pageNumber and rotation".into(),
            ));
        };
        if page_number < 1 || page_number as usize > page_count || angle % 90 != 0 {
            return Err(AppError::BadRequest(
                "pageRotations page numbers must exist and rotations must be multiples of 90"
                    .into(),
            ));
        }
        if seen.contains(&page_number) {
            return Err(AppError::BadRequest(format!(
                "pageRotations page {page_number} is listed more than once"
            )));
        }
        seen.push(page_number);
        let id = page_ids[page_number as usize - 1];
        let current = pdfdoc::rotation(document, id)?;
        pdfdoc::set_rotation(document, id, (current + angle).rem_euclid(360))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// repair
// ---------------------------------------------------------------------------

/// `POST /api/v1/misc/repair` — Ghostscript first, qpdf fallback, lopdf
/// load+save when neither binary exists. Mirrors `RepairController`: when at
/// least one repair tool is present but all decline the file, the request
/// fails 422 like Java's `PdfUnrepairableException`.
#[stirling_macros::endpoint]
fn repair(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    _params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };

    let workdir =
        tempfile::tempdir().map_err(|e| AppError::Internal(anyhow::anyhow!("tempdir: {e}")))?;
    let input_path = workdir.path().join("repair-in.pdf");
    let output_path = workdir.path().join("repair-out.pdf");
    std::fs::write(&input_path, &upload.bytes)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("repair input: {e}")))?;

    // RepairController gates each tool on its endpoint *group* being enabled
    // in settings, not on the binary existing: a disabled group is skipped
    // outright, and the lopdf fallback only runs when both groups are off.
    let gs_enabled = crate::endpointconfig::is_group_enabled("Ghostscript");
    let qpdf_enabled = crate::endpointconfig::is_group_enabled("qpdf");
    let mut repaired: Option<Vec<u8>> = None;

    // `gs -o out -sDEVICE=pdfwrite in`, exactly RepairController's invocation.
    if gs_enabled {
        match std::process::Command::new(crate::convert::resolve(&crate::settings::engine_program(
            "gs",
        )))
        .arg("-o")
        .arg(&output_path)
        .arg("-sDEVICE=pdfwrite")
        .arg(&input_path)
        .output()
        {
            Ok(output) => {
                if output.status.success() {
                    repaired = std::fs::read(&output_path).ok();
                }
            }
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                tracing::warn!(error = %e, "ghostscript repair attempt failed");
            }
            Err(_) => {}
        }
    }

    if repaired.is_none() && qpdf_enabled {
        // `qpdf --qdf --object-streams=disable in out`; exit 3 means qpdf
        // recovered with warnings, which still produces a repaired file.
        let _ = std::fs::remove_file(&output_path);
        match std::process::Command::new(crate::convert::resolve(&crate::settings::engine_program(
            "qpdf",
        )))
        .arg("--qdf")
        .arg("--object-streams=disable")
        .arg(&input_path)
        .arg(&output_path)
        .output()
        {
            Ok(output) => {
                let code = output.status.code().unwrap_or(-1);
                if code == 0 || code == 3 {
                    repaired = std::fs::read(&output_path).ok();
                }
            }
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                tracing::warn!(error = %e, "qpdf repair attempt failed");
            }
            Err(_) => {}
        }
    }

    let bytes = match repaired {
        Some(bytes) => bytes,
        None if !gs_enabled && !qpdf_enabled => {
            // Neither tool group is enabled: the PDFBox-equivalent fallback
            // is a lopdf load + save round-trip.
            let mut document = pdfdoc::load(&upload.name, &upload.bytes)?;
            pdfdoc::touch_loaded_info(&mut document);
            pdfdoc::save(&mut document)?
        }
        None => {
            return Err(AppError::Unprocessable(
                "This document is damaged in a way the repair tools cannot fix.".into(),
            ));
        }
    };

    Ok(pdf_response(
        bytes,
        &output_name(Some(&upload.name), "_repaired.pdf"),
    ))
}

// ---------------------------------------------------------------------------
// mobile-scanner
// ---------------------------------------------------------------------------

const SESSION_TIMEOUT_MS: u64 = 10 * 60 * 1000;
const MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;
const MAX_SESSION_BYTES: u64 = 500 * 1024 * 1024;
const MAX_FILES_PER_SESSION: usize = 500;
const MAX_TOTAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_ACTIVE_SESSIONS: usize = 1000;
const SWEEP_INTERVAL: Duration = Duration::from_secs(5 * 60);

#[derive(Clone)]
struct FileMeta {
    filename: String,
    size: u64,
    content_type: Option<String>,
}

struct SessionData {
    created_at: u64,
    files: Vec<FileMeta>,
    /// filename → downloaded flag, like Java's `downloadedFiles` map.
    downloaded: HashMap<String, bool>,
    stored_sizes: HashMap<String, u64>,
    stored_bytes: u64,
    /// Every file ever accepted; downloads do not give the slot back.
    file_count: usize,
}

impl SessionData {
    fn expires_at(&self) -> u64 {
        self.created_at + SESSION_TIMEOUT_MS
    }
    fn is_expired(&self, now: u64) -> bool {
        now > self.expires_at()
    }
}

struct ScannerState {
    sessions: HashMap<String, SessionData>,
    temp_dir: PathBuf,
}

static SCANNER: OnceLock<Mutex<ScannerState>> = OnceLock::new();

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn scanner_state() -> &'static Mutex<ScannerState> {
    SCANNER.get_or_init(|| {
        let temp_dir = std::env::temp_dir().join("stirling-mobile-scanner");
        let _ = std::fs::create_dir_all(&temp_dir);
        // Java's @Scheduled(cleanupExpiredSessions) equivalent: purge expired
        // sessions and orphaned session directories on an interval.
        let sweep_dir = temp_dir.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(SWEEP_INTERVAL);
            sweep(&sweep_dir);
        });
        Mutex::new(ScannerState {
            sessions: HashMap::new(),
            temp_dir,
        })
    })
}

/// Removes expired sessions and session directories with no live session,
/// mirroring `cleanupExpiredSessions` + `deleteOrphanedDirectories`.
fn sweep(temp_dir: &Path) {
    let now = now_millis();
    let mut state = scanner_state().lock().unwrap();
    let expired: Vec<String> = state
        .sessions
        .iter()
        .filter(|(_, s)| s.is_expired(now))
        .map(|(id, _)| id.clone())
        .collect();
    for id in expired {
        state.sessions.remove(&id);
        delete_session_dir(temp_dir, &id);
    }
    drop(state);

    let Ok(entries) = std::fs::read_dir(temp_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !valid_session_id(name) {
            continue;
        }
        let stale = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .is_some_and(|d| d.as_millis() as u64 + SESSION_TIMEOUT_MS <= now_millis());
        if !stale {
            continue;
        }
        let state = scanner_state().lock().unwrap();
        if !state.sessions.contains_key(name) {
            drop(state);
            delete_session_dir(temp_dir, name);
        }
    }
}

fn delete_session_dir(temp_dir: &Path, session_id: &str) {
    let dir = temp_dir.join(session_id);
    if dir.is_dir() {
        let _ = std::fs::remove_dir_all(&dir);
    }
}

fn valid_session_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn session_dir(state: &ScannerState, session_id: &str) -> PathBuf {
    state.temp_dir.join(session_id)
}

fn error_json(status: StatusCode, message: &str) -> Response {
    (status, axum::Json(json!({ "error": message }))).into_response()
}

/// `checkFeatureEnabled`: either the scanner or the signature-draw feature
/// keeps these endpoints alive. Java reads both flags from application
/// properties (`system.enableMobileScanner`/`enableMobileSignature`, both
/// default true); this resolves the same values from the loaded settings.
fn feature_enabled() -> bool {
    settings::mobile_features_enabled(&settings::current().system)
}

fn feature_check() -> Option<Response> {
    if feature_enabled() {
        None
    } else {
        Some(
            (
                StatusCode::FORBIDDEN,
                axum::Json(json!({
                    "error": "Mobile scanner feature is not enabled",
                    "enabled": false,
                })),
            )
                .into_response(),
        )
    }
}

fn session_times(session: &SessionData) -> (u64, u64) {
    (session.created_at, session.expires_at())
}

/// `POST /api/v1/mobile-scanner/create-session/{sessionId}`
/// (desktop side of the QR handshake).
pub async fn scanner_create_session(AxumPath(session_id): AxumPath<String>) -> Response {
    if let Some(denied) = feature_check() {
        return denied;
    }
    if !valid_session_id(&session_id) {
        return error_json(StatusCode::BAD_REQUEST, "Invalid session ID format");
    }
    let mut state = scanner_state().lock().unwrap();
    // Replacing an existing id deletes its files first, like Java.
    if state.sessions.remove(&session_id).is_some() {
        delete_session_dir(&state.temp_dir, &session_id);
    }
    let now = now_millis();
    let live = state
        .sessions
        .values()
        .filter(|s| !s.is_expired(now))
        .count();
    if live >= MAX_ACTIVE_SESSIONS {
        return error_json(
            StatusCode::SERVICE_UNAVAILABLE,
            "Too many active mobile upload sessions",
        );
    }
    let session = SessionData {
        created_at: now,
        files: Vec::new(),
        downloaded: HashMap::new(),
        stored_sizes: HashMap::new(),
        stored_bytes: 0,
        file_count: 0,
    };
    let (created_at, expires_at) = session_times(&session);
    state.sessions.insert(session_id.clone(), session);
    axum::Json(json!({
        "success": true,
        "sessionId": session_id,
        "createdAt": created_at,
        "expiresAt": expires_at,
        "timeoutMs": SESSION_TIMEOUT_MS,
    }))
    .into_response()
}

/// `GET /api/v1/mobile-scanner/validate-session/{sessionId}`
pub async fn scanner_validate_session(AxumPath(session_id): AxumPath<String>) -> Response {
    if let Some(denied) = feature_check() {
        return denied;
    }
    let mut state = scanner_state().lock().unwrap();
    let now = now_millis();
    let expired = state
        .sessions
        .get(&session_id)
        .is_some_and(|s| s.is_expired(now));
    if expired {
        // Java's validateSession deletes the session (files included) once
        // it has lapsed.
        state.sessions.remove(&session_id);
        delete_session_dir(&state.temp_dir, &session_id);
    }
    let valid = if expired {
        None
    } else {
        state.sessions.get(&session_id).map(session_times)
    };
    match valid {
        Some((created_at, expires_at)) => axum::Json(json!({
            "valid": true,
            "sessionId": session_id,
            "createdAt": created_at,
            "expiresAt": expires_at,
            "timeoutMs": SESSION_TIMEOUT_MS,
        }))
        .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            axum::Json(json!({
                "valid": false,
                "error": "Session not found or expired",
            })),
        )
            .into_response(),
    }
}

/// One uploaded scan file, spooled to a temp file like `Upload` so a batch
/// never holds every file's bytes in memory at once. `len` is tracked during
/// the spooling write so the per-file cap is enforced mid-stream.
struct ScanUpload {
    filename: Option<String>,
    content_type: Option<String>,
    file: tempfile::NamedTempFile,
    len: u64,
}

/// Reads the `files` parts of the upload multipart body, enforcing the
/// per-file cap while streaming so an oversized part never fully buffers.
async fn read_scan_uploads(mut multipart: Multipart) -> AppResult<Vec<ScanUpload>> {
    let mut files = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(format!("multipart error: {e}")))?
    {
        if field.name() != Some("files") {
            continue;
        }
        files.push(read_scan_field(field).await?);
    }
    Ok(files)
}

async fn read_scan_field(mut field: Field<'_>) -> AppResult<ScanUpload> {
    let filename = field.file_name().map(str::to_string);
    let content_type = field.content_type().map(str::to_string);
    let mut file = tempfile::NamedTempFile::new()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("scan upload tempfile: {e}")))?;
    let mut len = 0u64;
    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|e| AppError::BadRequest(format!("failed to read upload: {e}")))?
    {
        len += chunk.len() as u64;
        if len > MAX_FILE_BYTES {
            return Err(AppError::PayloadTooLarge(
                "File exceeds the mobile upload size limit".into(),
            ));
        }
        file.write_all(&chunk)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("scan upload write: {e}")))?;
    }
    Ok(ScanUpload {
        filename,
        content_type,
        file,
        len,
    })
}

/// `POST /api/v1/mobile-scanner/upload/{sessionId}`
pub async fn scanner_upload_files(
    AxumPath(session_id): AxumPath<String>,
    multipart: Multipart,
) -> Response {
    if let Some(denied) = feature_check() {
        return denied;
    }
    if !valid_session_id(&session_id) {
        return error_json(StatusCode::BAD_REQUEST, "Invalid session ID format");
    }
    let mut files = match read_scan_uploads(multipart).await {
        Ok(files) => files,
        Err(AppError::PayloadTooLarge(message)) => {
            return error_json(StatusCode::PAYLOAD_TOO_LARGE, &message);
        }
        Err(e) => return e.into_response(),
    };
    if files.is_empty() {
        return error_json(StatusCode::BAD_REQUEST, "No files provided");
    }

    let non_empty: Vec<&mut ScanUpload> = files.iter_mut().filter(|f| f.len > 0).collect();
    let batch_bytes: u64 = non_empty.iter().map(|f| f.len).sum();
    if non_empty.iter().any(|f| f.len > MAX_FILE_BYTES) {
        return error_json(
            StatusCode::PAYLOAD_TOO_LARGE,
            "File exceeds the mobile upload size limit",
        );
    }

    // Reserve capacity under the lock, like Java's `reserve`, so concurrent
    // uploads cannot both pass the same check.
    let mut state = scanner_state().lock().unwrap();
    let now = now_millis();
    {
        let session = match state.sessions.get(&session_id) {
            Some(session) if !session.is_expired(now) => session,
            _ => {
                return error_json(StatusCode::BAD_REQUEST, "Session not found or expired");
            }
        };
        if session.file_count + non_empty.len() > MAX_FILES_PER_SESSION
            || session.stored_bytes + batch_bytes > MAX_SESSION_BYTES
        {
            return error_json(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Session upload limit reached",
            );
        }
    }
    let mut total_stored: u64 = state.sessions.values().map(|s| s.stored_bytes).sum();
    if total_stored + batch_bytes > MAX_TOTAL_BYTES {
        // Java purges expired sessions once before declaring the server full.
        let expired: Vec<String> = state
            .sessions
            .iter()
            .filter(|(_, s)| s.is_expired(now))
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            state.sessions.remove(&id);
            delete_session_dir(&state.temp_dir, &id);
        }
        total_stored = state.sessions.values().map(|s| s.stored_bytes).sum();
        if total_stored + batch_bytes > MAX_TOTAL_BYTES {
            return error_json(
                StatusCode::SERVICE_UNAVAILABLE,
                "Mobile upload storage is full",
            );
        }
    }
    if let Some(session) = state.sessions.get_mut(&session_id) {
        session.file_count += non_empty.len();
        session.stored_bytes += batch_bytes;
    }

    let dir = session_dir(&state, &session_id);
    let _ = std::fs::create_dir_all(&dir);
    let mut stored = 0usize;
    let mut written_bytes = 0u64;
    let non_empty_len = non_empty.len();
    for file in non_empty {
        match store_scan_file(&dir, file) {
            Ok((name, size)) => {
                stored += 1;
                written_bytes += size;
                if let Some(session) = state.sessions.get_mut(&session_id) {
                    session.files.push(FileMeta {
                        filename: name.clone(),
                        size,
                        content_type: file.content_type.clone(),
                    });
                    session.downloaded.insert(name.clone(), false);
                    session.stored_sizes.insert(name, size);
                }
            }
            Err(message) => {
                // Roll back the reservation for files never stored, like
                // Java's unstoredFiles/unstoredBytes accounting.
                if let Some(session) = state.sessions.get_mut(&session_id) {
                    session.file_count -= non_empty_len - stored;
                    session.stored_bytes -= batch_bytes - written_bytes;
                }
                return error_json(StatusCode::INTERNAL_SERVER_ERROR, &message);
            }
        }
    }

    axum::Json(json!({
        "success": true,
        "sessionId": session_id,
        "filesUploaded": files.len(),
        "message": "Files uploaded successfully",
    }))
    .into_response()
}

fn sanitize_scan_filename(filename: Option<&str>) -> String {
    // Java substitutes "upload-<millis>" for a missing/blank name, then maps
    // every char outside [a-zA-Z0-9._-] to '_'; separators are not stripped
    // first, they become underscores.
    let base = filename
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("upload-{}", now_millis()));
    let sanitized: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        format!("upload-{}", now_millis())
    } else {
        sanitized
    }
}

/// Writes one uploaded file into the session directory with Java's
/// collision rule (`name-N.ext`) and path-traversal guard. The caller holds
/// the scanner lock for the whole batch, so the session cannot be replaced
/// mid-write the way Java's per-file check guards against.
fn store_scan_file(dir: &Path, file: &mut ScanUpload) -> Result<(String, u64), String> {
    let mut safe = sanitize_scan_filename(file.filename.as_deref());
    if safe == "." || safe == ".." {
        return Err("Invalid filename".into());
    }
    let mut counter = 1u32;
    loop {
        let path = dir.join(&safe);
        // The sanitized alphabet has no separators, so only "."/".." could
        // escape; both are rejected above and re-checked here.
        if safe != "." && safe != ".." {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut handle) => {
                    let src = file.file.as_file_mut();
                    src.seek(std::io::SeekFrom::Start(0))
                        .map_err(|e| format!("failed to save file: {e}"))?;
                    std::io::copy(src, &mut handle)
                        .map_err(|e| format!("failed to save file: {e}"))?;
                    return Ok((safe, file.len));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let (stem, ext) = split_scan_name(&safe);
                    safe = format!("{stem}-{counter}{ext}");
                    counter += 1;
                    continue;
                }
                Err(e) => return Err(format!("failed to save file: {e}")),
            }
        }
        return Err("Invalid filename".into());
    }
}

/// Java's `nameWithoutExt`/`ext` split for the `-N` collision suffix: the
/// `[.][^.]+$` regex removes the last dot plus a non-empty, dot-free tail;
/// the extension keeps everything from the last dot regardless.
fn split_scan_name(name: &str) -> (String, String) {
    match name.rfind('.') {
        Some(dot) if dot < name.len() - 1 => (name[..dot].to_string(), name[dot..].to_string()),
        _ => (
            name.to_string(),
            name.rfind('.')
                .map(|d| name[d..].to_string())
                .unwrap_or_default(),
        ),
    }
}

/// `GET /api/v1/mobile-scanner/files/{sessionId}`
pub async fn scanner_session_files(AxumPath(session_id): AxumPath<String>) -> Response {
    if let Some(denied) = feature_check() {
        return denied;
    }
    let state = scanner_state().lock().unwrap();
    let now = now_millis();
    let files: Vec<Value> = match state.sessions.get(&session_id) {
        Some(session) if !session.is_expired(now) => session
            .files
            .iter()
            .map(|f| {
                json!({
                    "filename": f.filename.clone(),
                    "size": f.size,
                    "contentType": f.content_type.clone(),
                })
            })
            .collect(),
        _ => Vec::new(),
    };
    axum::Json(json!({
        "sessionId": session_id,
        "files": files,
        "count": files.len(),
    }))
    .into_response()
}

fn scan_mime(filename: &str) -> &'static str {
    let ext = filename
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "heic" | "heif" => "image/heic",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
}

/// Releases a download claim without deleting the file — the partial-read
/// path of Java's `releaseDownloadClaim`.
fn release_download_claim(session_id: &str, filename: &str) {
    let mut state = scanner_state().lock().unwrap();
    if let Some(session) = state.sessions.get_mut(session_id) {
        if session.downloaded.get(filename) == Some(&true) {
            session.downloaded.insert(filename.to_string(), false);
        }
    }
}

/// Deletes the stored file and its bookkeeping after a complete download,
/// like Java's `deleteFileAfterDownload`.
fn delete_after_download(session_id: &str, filename: &str) {
    let path = {
        let state = scanner_state().lock().unwrap();
        let Some(session) = state.sessions.get(session_id) else {
            return;
        };
        if session.downloaded.get(filename) != Some(&true) {
            return;
        }
        session_dir(&state, session_id).join(filename)
    };
    {
        let mut state = scanner_state().lock().unwrap();
        if let Some(session) = state.sessions.get_mut(session_id) {
            if let Some(size) = session.stored_sizes.remove(filename) {
                session.stored_bytes = session.stored_bytes.saturating_sub(size);
            }
            session.files.retain(|f| f.filename != filename);
            session.downloaded.remove(filename);
        }
    }
    let _ = std::fs::remove_file(path);
}

/// `GET /api/v1/mobile-scanner/download/{sessionId}/{filename}` — claims the
/// file (served once), streams it, deletes it after a complete read, and
/// releases the claim if the stream is dropped early.
pub async fn scanner_download_file(
    AxumPath((session_id, filename)): AxumPath<(String, String)>,
) -> Response {
    if !feature_enabled() {
        return StatusCode::FORBIDDEN.into_response();
    }
    if filename.is_empty()
        || filename.contains("..")
        || filename.contains('/')
        || filename.contains('\\')
    {
        return StatusCode::NOT_FOUND.into_response();
    }

    let path = {
        let mut state = scanner_state().lock().unwrap();
        let now = now_millis();
        let valid = match state.sessions.get_mut(&session_id) {
            Some(session) if !session.is_expired(now) => {
                let stored = session.stored_sizes.contains_key(&filename);
                let already = session.downloaded.get(&filename) == Some(&true);
                if stored && !already {
                    session.downloaded.insert(filename.clone(), true);
                    true
                } else {
                    false
                }
            }
            _ => false,
        };
        if !valid {
            return StatusCode::NOT_FOUND.into_response();
        }
        session_dir(&state, &session_id).join(&filename)
    };
    if !path.is_file() {
        release_download_claim(&session_id, &filename);
        return StatusCode::NOT_FOUND.into_response();
    }

    let file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(_) => {
            release_download_claim(&session_id, &filename);
            return StatusCode::NOT_FOUND.into_response();
        }
    };
    use tokio::io::AsyncReadExt;
    let reader = file;
    let buffer = vec![0u8; 8192];
    let claim = ClaimGuard {
        session_id: session_id.clone(),
        filename: filename.clone(),
        completed: false,
    };
    let stream = stream::unfold(
        (reader, buffer, 0usize, 0usize, claim),
        |(mut reader, mut buffer, mut pos, mut len, mut claim)| async move {
            if pos == len {
                match reader.read(&mut buffer).await {
                    Ok(0) => {
                        claim.completed = true;
                        delete_after_download(&claim.session_id, &claim.filename);
                        return None;
                    }
                    Ok(n) => {
                        len = n;
                        pos = 0;
                    }
                    Err(_) => return None,
                }
            }
            let chunk = bytes::Bytes::copy_from_slice(&buffer[pos..len]);
            pos += chunk.len();
            if pos == len {
                pos = 0;
                len = 0;
            }
            Some((
                Ok::<_, std::io::Error>(chunk),
                (reader, buffer, pos, len, claim),
            ))
        },
    );

    (
        [
            (header::CONTENT_TYPE, scan_mime(&filename).to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

/// Releases the download claim if the response stream is dropped before
/// reaching EOF (client disconnect, range abort), like Java's
/// `releaseDownloadClaim` on a partial read.
struct ClaimGuard {
    session_id: String,
    filename: String,
    completed: bool,
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        if !self.completed {
            release_download_claim(&self.session_id, &self.filename);
        }
    }
}

/// `DELETE /api/v1/mobile-scanner/session/{sessionId}`
pub async fn scanner_delete_session(AxumPath(session_id): AxumPath<String>) -> Response {
    if let Some(denied) = feature_check() {
        return denied;
    }
    let mut state = scanner_state().lock().unwrap();
    if valid_session_id(&session_id) {
        state.sessions.remove(&session_id);
        delete_session_dir(&state.temp_dir, &session_id);
    }
    axum::Json(json!({
        "success": true,
        "sessionId": session_id,
        "message": "Session deleted",
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// auto-split-pdf
// ---------------------------------------------------------------------------

/// `AutoSplitPdfController.VALID_QR_CONTENTS` — only these decoded strings
/// mark a divider page.
const QR_DIVIDER_CONTENTS: &[&str] = &[
    "https://github.com/Stirling-Tools/Stirling-PDF",
    "https://github.com/Frooodle/Stirling-PDF",
    "https://stirlingpdf.com",
];

const QR_DETECTION_DPI: u32 = 150;
const QR_RETRY_DPI: u32 = 300;
const MAX_IMAGES_FOR_DIRECT_EXTRACTION: usize = 3;

/// `POST /api/v1/misc/auto-split-pdf` — splits at divider pages carrying a
/// Stirling QR code and returns a ZIP of `{base}_1.pdf`, `{base}_2.pdf`, …
/// like `AutoSplitPdfController`.
///
/// Divergence: Java decodes with ZXing's HybridBinarizer then
/// GlobalHistogramBinarizer; `rqrr` does its own binarization in a single
/// pass. Java's `maxDPI` clamp on the 300 DPI retry pass has no Rust
/// settings counterpart, so the retry always runs at 300 DPI.
#[stirling_macros::endpoint]
fn auto_split_pdf(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    let duplex_mode = param_bool(&params, "duplexMode");

    let document = pdfdoc::load(&upload.name, &upload.bytes)?;
    let page_ids = pdfdoc::page_ids(&document);
    let rasterizer = misc::BlankRasterizer::probe(&upload.name, &upload.bytes);

    let mut groups: Vec<Vec<u32>> = Vec::new();
    let mut page = 0usize;
    while page < page_ids.len() {
        let index = page as u32;
        let divider = detect_qr_divider(&document, page_ids[page], index + 1, rasterizer.as_ref())
            .is_some_and(|text| QR_DIVIDER_CONTENTS.contains(&text.as_str()));
        if divider && index != 0 {
            groups.push(Vec::new());
        }
        if !divider || index == 0 {
            match groups.last_mut() {
                Some(group) => group.push(index),
                None => groups.push(vec![index]),
            }
        }
        page += 1 + usize::from(duplex_mode && divider);
    }
    groups.retain(|group| !group.is_empty());

    let base = crate::upload::remove_extension(Some(simple_name(&upload.name)));
    let mut entries = Vec::with_capacity(groups.len());
    for (i, order) in groups.iter().enumerate() {
        let mut doc = pdfdoc::load(&upload.name, &upload.bytes)?;
        pdfdoc::subset(&mut doc, order)?;
        pdfdoc::touch_loaded_info(&mut doc);
        entries.push((format!("{base}_{}.pdf", i + 1), pdfdoc::save(&mut doc)?));
    }
    crate::handlers::split::zip_response(entries, &format!("{base}.zip"))
}

/// One page's divider check, mirroring the controller's order: embedded
/// images first when there are few enough to try directly, then the
/// rendered page at 150 DPI with a 300 DPI retry.
fn detect_qr_divider(
    doc: &lopdf::Document,
    page_id: lopdf::ObjectId,
    page_number: u32,
    rasterizer: Option<&misc::BlankRasterizer>,
) -> Option<String> {
    let images = direct_page_images(doc, page_id);
    if !images.is_empty() && images.len() <= MAX_IMAGES_FOR_DIRECT_EXTRACTION {
        for image in &images {
            if let Some((width, height, gray)) = pixels_to_luma(image) {
                if let Some(text) = qr_decode(width, height, &gray) {
                    return Some(text);
                }
            }
        }
    }
    let rasterizer = rasterizer?;
    for dpi in [QR_DETECTION_DPI, QR_RETRY_DPI] {
        if let Some(text) = rasterizer
            .render(page_number, dpi)
            .and_then(|img| dynamic_to_luma(&img))
            .and_then(|(w, h, gray)| qr_decode(w, h, &gray))
        {
            return Some(text);
        }
    }
    None
}

/// The page's direct `/Image` XObject streams — `countPageImages` plus
/// `checkPageImagesDirect`'s iteration, which Java limits to the page's
/// own resources (form XObjects are not descended).
fn direct_page_images(doc: &lopdf::Document, page_id: lopdf::ObjectId) -> Vec<lopdf::Stream> {
    let mut images = Vec::new();
    let Ok(Some((_, resources))) = pdfdoc::page_resources(doc, page_id) else {
        return images;
    };
    let Some(resources) = pdfdoc::dict_owned(doc, &resources).ok().flatten() else {
        return images;
    };
    let Ok(xobjects) = resources.get(b"XObject") else {
        return images;
    };
    let Ok(Some(xobjects)) = pdfdoc::dict_owned(doc, &xobjects.clone()) else {
        return images;
    };
    for (_, value) in xobjects.iter() {
        let value = value.clone();
        let Some(dict) = pdfdoc::peek_dict(doc, &value) else {
            continue;
        };
        let is_image =
            misc::dict_name(doc, dict, b"Subtype").is_some_and(|name| name.as_slice() == b"Image");
        if !is_image {
            continue;
        }
        let stream = match &value {
            lopdf::Object::Reference(id) => doc.objects.get(id).and_then(|o| o.as_stream().ok()),
            _ => value.as_stream().ok(),
        };
        if let Some(stream) = stream {
            images.push(stream.clone());
        }
    }
    images
}

/// `(width, height, luma)` for an embedded image XObject, or `None` when
/// `imgcomp` cannot decode it — Java's `getImage()` likewise yields
/// nothing usable for out-of-subset images.
fn pixels_to_luma(stream: &lopdf::Stream) -> Option<(u32, u32, Vec<u8>)> {
    let (width, height, pixels) = crate::imgcomp::decode_image(stream)?;
    let gray = match pixels {
        crate::imgcomp::Pixels::Gray(img) => img.pixels().map(|p| p.0[0]).collect(),
        crate::imgcomp::Pixels::Rgb(img) => img
            .pixels()
            .map(|p| luma(p.0[0], p.0[1], p.0[2], 255))
            .collect(),
        crate::imgcomp::Pixels::Rgba(img) => img
            .pixels()
            .map(|p| luma(p.0[0], p.0[1], p.0[2], p.0[3]))
            .collect(),
    };
    Some((width, height, gray))
}

fn dynamic_to_luma(img: &image::DynamicImage) -> Option<(u32, u32, Vec<u8>)> {
    let gray = img.to_luma8();
    Some((
        gray.width(),
        gray.height(),
        gray.pixels().map(|p| p.0[0]).collect(),
    ))
}

/// ZXing's luminance approximation, compositing alpha over white like
/// `RGBLuminanceSource` does for transparent pixels.
fn luma(r: u8, g: u8, b: u8, a: u8) -> u8 {
    let (r, g, b) = if a == 255 {
        (r as u32, g as u32, b as u32)
    } else {
        let a = a as u32;
        (
            (r as u32 * a + 255 * (255 - a)) / 255,
            (g as u32 * a + 255 * (255 - a)) / 255,
            (b as u32 * a + 255 * (255 - a)) / 255,
        )
    };
    ((299 * r + 587 * g + 114 * b) / 1000) as u8
}

/// `decodeQRCode`: skip near-blank images, then let `rqrr` find and read
/// the symbol. Returns the decoded text or `None`.
fn qr_decode(width: u32, height: u32, gray: &[u8]) -> Option<String> {
    const BLANK_CHECK_SAMPLES: usize = 20;
    if gray.is_empty() {
        return None;
    }
    let step = (gray.len() / BLANK_CHECK_SAMPLES).max(1);
    if gray.iter().skip(step).step_by(step).all(|&p| p == gray[0]) {
        return None;
    }
    let mut prepared =
        rqrr::PreparedImage::prepare_from_greyscale(width as usize, height as usize, |x, y| {
            gray[y * width as usize + x]
        });
    for grid in prepared.detect_grids() {
        if let Ok((_, text)) = grid.decode() {
            return Some(text);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::dictionary;

    fn session(created_at: u64) -> SessionData {
        SessionData {
            created_at,
            files: Vec::new(),
            downloaded: HashMap::new(),
            stored_sizes: HashMap::new(),
            stored_bytes: 0,
            file_count: 0,
        }
    }

    #[test]
    fn session_id_accepts_java_charset() {
        // Java's SESSION_ID_VALIDATION_PATTERN is [a-zA-Z0-9-]+ — no '_'.
        assert!(!valid_session_id("abc-123_XYZ"));
        assert!(valid_session_id("a"));
        assert!(!valid_session_id(""));
        assert!(!valid_session_id("has space"));
        assert!(!valid_session_id("slash/id"));
        assert!(!valid_session_id("dot.id"));
        assert!(!valid_session_id("under_score"));
    }

    #[test]
    fn session_expiry_matches_java_timeout() {
        let s = session(1_000);
        assert_eq!(s.expires_at(), 1_000 + SESSION_TIMEOUT_MS);
        assert!(!s.is_expired(1_000 + SESSION_TIMEOUT_MS));
        assert!(s.is_expired(1_000 + SESSION_TIMEOUT_MS + 1));
        assert_eq!(session_times(&s), (1_000, 1_000 + SESSION_TIMEOUT_MS));
    }

    #[test]
    fn scan_filename_sanitizes_like_java() {
        assert_eq!(sanitize_scan_filename(Some("photo.jpg")), "photo.jpg");
        assert_eq!(
            sanitize_scan_filename(Some("my file (1).pdf")),
            "my_file__1_.pdf"
        );
        // Blank/missing names fall back to upload-<millis> like Java.
        for blank in [None, Some(""), Some("   ")] {
            assert!(sanitize_scan_filename(blank).starts_with("upload-"));
        }
        // "." and ".." survive sanitising; store_scan_file rejects them.
        assert_eq!(sanitize_scan_filename(Some("..")), "..");
    }

    #[test]
    fn scan_name_split_matches_java_regex() {
        assert_eq!(split_scan_name("file.pdf"), ("file".into(), ".pdf".into()));
        assert_eq!(
            split_scan_name("archive.tar.gz"),
            ("archive.tar".into(), ".gz".into())
        );
        assert_eq!(split_scan_name("noext"), ("noext".into(), String::new()));
        assert_eq!(
            split_scan_name(".hidden"),
            (String::new(), ".hidden".into())
        );
        assert_eq!(split_scan_name("file."), ("file.".into(), ".".into()));
    }

    /// A grayscale image XObject stream carrying a scannable QR code —
    /// `imgcomp`'s raw-sample path needs no filter.
    fn qr_image_stream(pdf: &mut crate::testkit::Pdf, content: &[u8]) -> lopdf::ObjectId {
        let code = qrcode::QrCode::new(content).unwrap();
        let modules = code.width();
        let colors = code.to_colors();
        let (quiet, scale) = (4usize, 4usize);
        let size = (modules + 2 * quiet) * scale;
        let mut gray = vec![255u8; size * size];
        for y in 0..modules {
            for x in 0..modules {
                if colors[y * modules + x].select(true, false) {
                    for dy in 0..scale {
                        for dx in 0..scale {
                            gray[((y + quiet) * scale + dy) * size + (x + quiet) * scale + dx] = 0;
                        }
                    }
                }
            }
        }
        pdf.add_stream(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => size as i64,
                "Height" => size as i64,
                "ColorSpace" => "DeviceGray",
                "BitsPerComponent" => 8,
            },
            &gray,
        )
    }

    fn qr_page(pdf: &mut crate::testkit::Pdf, content: &[u8]) -> lopdf::ObjectId {
        let image = qr_image_stream(pdf, content);
        let stream = pdf.add_stream(dictionary! {}, b"q 200 0 0 200 100 400 cm /Im1 Do Q");
        pdf.add_page(
            [0, 0, 612, 792],
            dictionary! {
                "Contents" => lopdf::Object::Reference(stream),
                "Resources" => dictionary! {
                    "XObject" => dictionary! {
                        "Im1" => lopdf::Object::Reference(image),
                    },
                },
            },
        )
    }

    async fn split_names(bytes: Vec<u8>) -> Vec<String> {
        let uploads = vec![Upload {
            name: "doc.pdf".into(),
            bytes: crate::upload::UploadBytes::from_vec(bytes),
            content_type: None,
        }];
        let response =
            auto_split_pdf_work(AppState::new(), HashMap::new(), uploads, vec![]).unwrap();
        let zip_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip_bytes)).unwrap();
        (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect()
    }

    #[tokio::test]
    async fn auto_split_drops_stirling_qr_divider_pages() {
        let mut pdf = crate::testkit::Pdf::new();
        qr_page(&mut pdf, b"https://stirlingpdf.com");
        pdf.add_page([0, 0, 612, 792], lopdf::Dictionary::new());
        let names = split_names(pdf.bytes()).await;
        // Page 0 is empty content, the QR page is a mid-document divider
        // (dropped), page 2 trails — two single-page outputs.
        assert_eq!(
            names,
            vec!["doc_1.pdf".to_string(), "doc_2.pdf".to_string()]
        );
    }

    #[tokio::test]
    async fn auto_split_keeps_first_page_divider() {
        let mut pdf = crate::testkit::Pdf::new();
        // Replace the default first page's contents with the QR image.
        let image = qr_image_stream(&mut pdf, b"https://stirlingpdf.com");
        let stream = pdf.add_stream(dictionary! {}, b"q 200 0 0 200 100 400 cm /Im1 Do Q");
        pdf.edit_page(|dict| {
            dict.set("Contents", lopdf::Object::Reference(stream));
            dict.set(
                "Resources",
                dictionary! {
                    "XObject" => dictionary! {
                        "Im1" => lopdf::Object::Reference(image),
                    },
                },
            );
        });
        pdf.add_page([0, 0, 612, 792], lopdf::Dictionary::new());
        let names = split_names(pdf.bytes()).await;
        assert_eq!(names, vec!["doc_1.pdf".to_string()]);
    }
}

#[cfg(test)]
mod scan_tests {
    use super::*;

    fn params(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn param_bool_default_respects_absent_vs_blank() {
        // Absent → caller default; present-but-blank → false; truthy set.
        assert!(param_bool_default(&params(&[]), "flag", true));
        assert!(!param_bool_default(&params(&[("flag", "")]), "flag", true));
        assert!(param_bool_default(
            &params(&[("flag", "yes")]),
            "flag",
            false
        ));
        assert!(!param_bool_default(
            &params(&[("flag", "no")]),
            "flag",
            true
        ));
    }

    #[test]
    fn simple_name_strips_directories() {
        assert_eq!(simple_name("dir/sub/file.pdf"), "file.pdf");
        assert_eq!(simple_name("C:\\win\\file.pdf"), "file.pdf");
        assert_eq!(simple_name("file.pdf"), "file.pdf");
    }

    fn result(page: usize, rotation: i64, correction: i64, method: &'static str) -> PageResult {
        PageResult {
            page_number: page,
            current_rotation: rotation,
            correction,
            confidence: None,
            method,
            apply: correction != 0,
            note: None,
        }
    }

    #[test]
    fn infer_pages_consensus_rules() {
        // Two pages at rotation 0 agree on correction 90 → the "none"
        // page at rotation 0 inherits it.
        let mut results = vec![
            result(1, 0, 90, "text"),
            result(2, 0, 0, "none"),
            result(3, 90, 0, "text"),
        ];
        infer_pages(&mut results);
        assert_eq!(results[1].method, "inferred");
        assert_eq!(results[1].correction, 90);
        assert!(results[1].apply);
        assert_eq!(results[1].note, Some("inferredFromDocument"));
        // The 90°-rotated page had its own signal — untouched.
        assert_eq!(results[2].method, "text");
    }

    #[test]
    fn infer_pages_skips_conflicts() {
        // Conflicting corrections at the same rotation → no inference.
        let mut results = vec![
            result(1, 0, 90, "text"),
            result(2, 0, 270, "osd"),
            result(3, 0, 0, "none"),
        ];
        infer_pages(&mut results);
        assert_eq!(results[2].method, "none");
        assert!(!results[2].apply);
    }

    #[test]
    fn analysis_json_counts_methods() {
        let results = vec![
            result(1, 0, 90, "text"),
            result(2, 0, 0, "none"),
            result(3, 90, 180, "inferred"),
        ];
        let json = analysis_json(&results, 3);
        assert_eq!(json["totalPages"], 3);
        assert_eq!(json["pagesToRotate"], 2);
        assert_eq!(json["detectedByText"], 1);
        assert_eq!(json["inferred"], 1);
        assert_eq!(json["undetected"], 1);
        assert_eq!(json["pages"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn apply_explicit_rotations_validates() {
        let mut pdf = crate::testkit::Pdf::new();
        pdf.add_page([0, 0, 300, 300], lopdf::Dictionary::new());
        let mut doc = lopdf::Document::load_mem(&pdf.bytes()).unwrap();

        // Out-of-range page, non-multiple-of-90, missing fields, duplicates.
        for rotations in [
            vec![PageRotation {
                page_number: Some(9),
                rotation: Some(90),
            }],
            vec![PageRotation {
                page_number: Some(1),
                rotation: Some(45),
            }],
            vec![PageRotation {
                page_number: None,
                rotation: Some(90),
            }],
            vec![
                PageRotation {
                    page_number: Some(1),
                    rotation: Some(90),
                },
                PageRotation {
                    page_number: Some(1),
                    rotation: Some(90),
                },
            ],
        ] {
            assert!(apply_explicit_rotations(&mut doc, &rotations).is_err());
        }
        // Valid: page 2 gets +90 on top of its existing rotation.
        let rotations = vec![PageRotation {
            page_number: Some(2),
            rotation: Some(90),
        }];
        assert!(apply_explicit_rotations(&mut doc, &rotations).is_ok());
    }

    #[test]
    fn sanitize_scan_filename_matches_java_rules() {
        assert_eq!(sanitize_scan_filename(Some("my file.pdf")), "my_file.pdf");
        assert_eq!(sanitize_scan_filename(Some("a/b:c*d.pdf")), "a_b_c_d.pdf");
        // Blank and missing names both fall back to upload-<millis>.
        assert!(sanitize_scan_filename(Some("  ")).starts_with("upload-"));
        assert!(sanitize_scan_filename(None).starts_with("upload-"));
        // Everything sanitized away still yields a usable name.
        let weird = sanitize_scan_filename(Some("///"));
        assert!(!weird.is_empty());
    }

    #[test]
    fn split_scan_name_handles_dot_variants() {
        assert_eq!(
            split_scan_name("photo.jpg"),
            ("photo".into(), ".jpg".into())
        );
        assert_eq!(
            split_scan_name("archive.tar.gz"),
            ("archive.tar".into(), ".gz".into())
        );
        assert_eq!(split_scan_name("noext"), ("noext".into(), "".into()));
        // Trailing dot: the `[.][^.]+$` regex needs a non-empty tail, so
        // the stem keeps the dot and the ext is the bare dot.
        assert_eq!(split_scan_name("file."), ("file.".into(), ".".into()));
    }

    #[test]
    fn scan_mime_covers_common_types() {
        assert_eq!(scan_mime("a.JPG"), "image/jpeg");
        assert_eq!(scan_mime("a.png"), "image/png");
        assert_eq!(scan_mime("a.pdf"), "application/pdf");
        assert_eq!(scan_mime("a.heic"), "image/heic");
        assert_eq!(scan_mime("a.xyz"), "application/octet-stream");
        assert_eq!(scan_mime("noext"), "application/octet-stream");
    }

    #[test]
    fn valid_session_id_charset() {
        assert!(!valid_session_id("abc-123_X"));
        assert!(!valid_session_id(""));
        assert!(!valid_session_id("bad id"));
        assert!(!valid_session_id("../escape"));
        assert!(!valid_session_id("semi;colon"));
    }
}
