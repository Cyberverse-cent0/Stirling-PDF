//! `POST /api/v1/pipeline/handleData` — runs a JSON-described chain of tool
//! operations over the uploaded files, porting Java's `PipelineController` +
//! `PipelineProcessor` + `InternalApiClient`.
//!
//! Each operation is an `/api/v1/...` path dispatched back to this server
//! over loopback HTTP (Java's `InternalApiClient` does the same against its
//! own port), so a pipeline step hits this server's own endpoints — and
//! per-endpoint enablement, auth, and async job handling all behave exactly
//! as a direct call.
//!
//! Deliberate divergences from Java:
//! - `apiDocService.isValidOperation` reads the live `/v1/api-docs`; Rust has
//!   no spec service, so validity is approximated by the dispatch allowlist
//!   (`InternalApiClient.ALLOWED_ENDPOINT_PATH`) plus a small table of
//!   required `@RequestParam`s. Non-dispatchable or invalid operations get a
//!   400 — Java's catch-all turns them into an empty 200.
//! - A sub-request answered with 4xx/5xx aborts the whole run (Java's
//!   RestTemplate error handler throws the same way) and yields the same
//!   empty 200 the controller's `return null` produces.
//! - `internalApi` timeouts come from `internalApi.*` settings/env; the
//!   request timeout covers the whole exchange where Java's read timeout is
//!   per-read.

use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::OnceLock;

use axum::body::Body;
use axum::extract::{Multipart, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Map, Value};
use tempfile::NamedTempFile;

use crate::error::{AppError, AppResult};
use crate::handlers::jobs::{self, AppState};
use crate::handlers::platform;
use crate::pipeline_table;
use crate::settings;
use crate::upload::{receive_multipart, simple_name, Upload};

const MAX_UNZIP_DEPTH: u32 = 10;
const ZIP_MAGIC: [u8; 4] = [0x50, 0x4B, 0x03, 0x04];

// ---------------------------------------------------------------------------
// Request model (PipelineConfig / PipelineOperation / HandleDataRequest)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct PipelineConfig {
    #[serde(default)]
    #[allow(dead_code)]
    name: Option<String>,
    #[serde(default, rename = "pipeline")]
    operations: Option<Vec<PipelineOperation>>,
    #[serde(default, rename = "outputDir")]
    #[allow(dead_code)]
    output_dir: Option<String>,
    #[serde(default, rename = "outputFileName")]
    #[allow(dead_code)]
    output_file_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PipelineOperation {
    operation: String,
    #[serde(default)]
    parameters: Option<Map<String, Value>>,
}

// ---------------------------------------------------------------------------
// Operation validity — InternalApiClient.ALLOWED_ENDPOINT_PATH plus the
// required-parameter slice of ApiDocService.isValidOperation.
// ---------------------------------------------------------------------------

/// `InternalApiClient.isDispatchable`: fixed namespace prefixes, no traversal,
/// encoding, query/fragment or backslash characters.
fn is_dispatchable(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/api/v1/") else {
        return false;
    };
    let valid_segment = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    };
    let segments: Vec<&str> = rest.split('/').collect();
    match segments[0] {
        "general" | "misc" | "security" | "convert" | "filter" | "integration" | "docparse" => {
            segments.len() >= 2 && segments[1..].iter().all(|s| valid_segment(s))
        }
        // `/api/v1/ai/tools/*` is dispatchable; the rest of `/api/v1/ai/` is not.
        "ai" => {
            segments.len() >= 3
                && segments[1] == "tools"
                && segments[2..].iter().all(|s| valid_segment(s))
        }
        // Of `/api/v1/form/*` only filling and listing fields are dispatchable.
        "form" => rest == "form/fill" || rest == "form/fields",
        _ => false,
    }
}

/// Required `@RequestParam`s (no default, `required` not false) on
/// dispatchable endpoints — the part of `isValidOperation` that can actually
/// reject a pipeline step. File-part params are deliberately absent: whether
/// springdoc lists them under `parameters` is unverifiable here, and the
/// sub-request fails either way when they are genuinely required.
fn required_params(path: &str) -> &'static [&'static str] {
    match path {
        "/api/v1/integration/external-api-call" => &["connectionId"],
        "/api/v1/integration/purview-apply-label" => &["connectionId", "labelId"],
        "/api/v1/integration/purview-read-label" => &["connectionId"],
        _ => &[],
    }
}

fn is_valid_operation(operation: &PipelineOperation) -> bool {
    if !is_dispatchable(&operation.operation) {
        return false;
    }
    let parameters = operation.parameters.as_ref();
    required_params(&operation.operation)
        .iter()
        .all(|name| parameters.is_some_and(|p| p.contains_key(*name)))
}

// ---------------------------------------------------------------------------
// Files flowing between stages
// ---------------------------------------------------------------------------

/// One file in the pipeline. `path` is a spool file that stays valid while
/// `_temp` (or the originating `Upload`) is alive; dropping the stage deletes
/// pipeline-owned temps, matching `PipelineResult.releaseTempFiles`.
struct PipeFile {
    name: String,
    path: PathBuf,
    _temp: Option<NamedTempFile>,
}

impl PipeFile {
    fn byte_len(&self) -> u64 {
        std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0)
    }

    fn starts_with_zip_magic(&self) -> bool {
        let mut head = [0u8; 4];
        std::fs::File::open(&self.path)
            .and_then(|mut f| f.read_exact(&mut head))
            .is_ok()
            && head == ZIP_MAGIC
    }
}

/// `ZipExtractionUtils.isZip`: ZIP magic bytes, `.cbz` names excluded.
fn is_zip(file: &PipeFile) -> bool {
    file.byte_len() >= ZIP_MAGIC.len() as u64
        && !file.name.to_lowercase().ends_with(".cbz")
        && file.starts_with_zip_magic()
}

/// `PipelineProcessor.removeTrailingNaming`: strips the last `_segment` of
/// the name part when an extension exists.
fn remove_trailing_naming(filename: &str) -> String {
    let Some(dot) = filename.rfind('.') else {
        return filename.to_string();
    };
    let (name, extension) = filename.split_at(dot);
    match name.rfind('_') {
        Some(underscore) => format!("{}{}", &name[..underscore], extension),
        None => filename.to_string(),
    }
}

/// `GeneralUtils.generateFilename(name, "(n)")` for zip dedup — the suffix
/// replaces the extension rather than appending to it.
fn dedup_name(original: &str, count: usize) -> String {
    crate::upload::remove_extension(Some(original)) + &format!("({count})")
}

/// `ZipExtractionUtils.extractZip`: flat extraction, nested ZIPs recurse to
/// `MAX_UNZIP_DEPTH`, entries keep their archive names as display names.
fn extract_zip(file: PipeFile, depth: u32, out: &mut Vec<PipeFile>) -> AppResult<()> {
    if depth > MAX_UNZIP_DEPTH {
        tracing::warn!("ZIP nesting depth {depth} exceeds limit {MAX_UNZIP_DEPTH}");
        out.push(file);
        return Ok(());
    }
    let archive_file = std::fs::File::open(&file.path)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("open zip: {e}")))?;
    let mut archive = zip::ZipArchive::new(archive_file)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("read zip: {e}")))?;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("zip entry {i}: {e}")))?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_string();
        let mut temp = NamedTempFile::new()
            .map_err(|e| AppError::Internal(anyhow::anyhow!("zip temp: {e}")))?;
        std::io::copy(&mut entry, &mut temp)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("zip extract: {e}")))?;
        let extracted = PipeFile {
            name,
            path: temp.path().to_path_buf(),
            _temp: Some(temp),
        };
        if is_zip(&extracted) {
            extract_zip(extracted, depth + 1, out)?;
        } else {
            out.push(extracted);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Loopback dispatch — InternalApiClient.post
// ---------------------------------------------------------------------------

fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        let (connect, read) = settings::internal_api_timeouts();
        reqwest::Client::builder()
            .connect_timeout(connect)
            .timeout(read)
            .build()
            .expect("pipeline http client")
    })
}

/// `InternalApiClient.getBaseUrl`: this server's bound address, falling back
/// to `127.0.0.1:$PORT` when the listener address was never recorded.
fn self_base_url() -> String {
    if let Some(addr) = platform::bound_addr() {
        let ip = if addr.ip().is_unspecified() {
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        } else {
            addr.ip()
        };
        return format!("http://{ip}:{}", addr.port());
    }
    let port = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(8081);
    format!("http://127.0.0.1:{port}")
}

/// `InternalApiClient.extractFilename` + `WebResponseUtils.encodeAttachmentName`
/// inverse: `filename=` part, all quotes stripped, form-decoded (`+` → space).
fn response_filename(headers: &HeaderMap) -> String {
    const DEFAULT: &str = "default-filename.ext";
    let Some(cd) = headers.get(header::CONTENT_DISPOSITION) else {
        return DEFAULT.to_string();
    };
    let Ok(cd) = cd.to_str() else {
        return DEFAULT.to_string();
    };
    for part in cd.split(';') {
        let part = part.trim();
        if part.starts_with("filename") {
            let Some((_, value)) = part.split_once('=') else {
                continue;
            };
            // Java splits on every '=' and keeps [1]; a filename containing
            // '=' is truncated there too.
            let value = part.split('=').nth(1).unwrap_or(value).trim();
            let value = value.replace('"', "");
            return url_decode(&value);
        }
    }
    DEFAULT.to_string()
}

/// `URLDecoder.decode(s, UTF_8)`: `%XX` byte sequences and `+` → space.
fn url_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = &value[i + 1..i + 3];
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One step's response, already spooled to a temp file like
/// `InternalApiClient.TempFileResource`.
struct StepResponse {
    status: StatusCode,
    filename: String,
    file: PipeFile,
}

/// `MultiValueMap` parameter flattening: a JSON array becomes repeated form
/// fields; scalars stringify the way Spring's form converter writes them.
fn param_fields(parameters: &Map<String, Value>) -> Vec<(String, String)> {
    let mut fields = Vec::new();
    for (key, value) in parameters {
        match value {
            Value::Array(items) => {
                for item in items {
                    if let Some(s) = scalar_string(item) {
                        fields.push((key.clone(), s));
                    }
                }
            }
            _ => {
                if let Some(s) = scalar_string(value) {
                    fields.push((key.clone(), s));
                }
            }
        }
    }
    fields
}

fn scalar_string(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => Some(s.clone()),
        // Nested structures have no Spring scalar form; compact JSON is the
        // closest faithful rendering.
        other => Some(serde_json::to_string(other).unwrap_or_default()),
    }
}

/// `InternalApiClient.post`: multipart `fileInput` parts plus the operation's
/// parameters, dispatched to this server's own router. Returns `None` for the
/// fatal case — a transport failure or a 4xx/5xx, which Java's RestTemplate
/// error handler turns into an exception that aborts the whole pipeline.
async fn dispatch(
    operation: &str,
    files: &[&PipeFile],
    parameters: &Map<String, Value>,
    run_id: &str,
) -> Option<StepResponse> {
    let url = format!("{}{}", self_base_url(), operation);
    let mut form = reqwest::multipart::Form::new();
    for file in files {
        let Ok(tokio_file) = tokio::fs::File::open(&file.path).await else {
            return None;
        };
        let len = file.byte_len();
        let part =
            reqwest::multipart::Part::stream_with_length(reqwest::Body::from(tokio_file), len)
                .file_name(file.name.clone());
        form = form.part(crate::upload::FILE_FIELD, part);
    }
    for (name, value) in param_fields(parameters) {
        form = form.text(name, value);
    }

    let mut request = http_client().post(&url).multipart(form);
    let mut headers = HeaderMap::new();
    headers.insert("x-stirling-automation", "true".parse().unwrap());
    headers.insert("x-stirling-run-id", run_id.parse().unwrap());
    // Standalone deployments may gate /api/** behind RUST_API_KEY; the
    // loopback call must carry it the way Java attaches its internal key.
    if let Ok(key) = std::env::var("RUST_API_KEY") {
        let key = key.trim();
        if !key.is_empty() {
            if let Ok(value) = axum::http::HeaderValue::from_str(key) {
                headers.insert("x-api-key", value);
            }
        }
    }
    request = request.headers(headers);

    let response = request.send().await.ok()?;
    let status = StatusCode::from_u16(response.status().as_u16())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    // RestTemplate's error handler throws on 4xx/5xx before the extractor
    // runs, so these statuses never reach PipelineProcessor as a response.
    if status.is_client_error() || status.is_server_error() {
        return None;
    }
    let filename = response_filename(response.headers());
    let mut temp = NamedTempFile::new().ok()?;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            return None;
        };
        use std::io::Write;
        if temp.write_all(&chunk).is_err() {
            return None;
        }
    }
    let _ = temp.as_file().sync_all();
    let file = PipeFile {
        name: filename.clone(),
        path: temp.path().to_path_buf(),
        _temp: Some(temp),
    };
    Some(StepResponse {
        status,
        filename,
        file,
    })
}

// ---------------------------------------------------------------------------
// Pipeline run — PipelineProcessor.runPipelineAgainstFilesInternal
// ---------------------------------------------------------------------------

/// `processOutputFiles`: zip responses unpack into the next stage's inputs;
/// everything else carries the (possibly renamed) response file forward.
fn collect_outputs(
    operation: &str,
    response: StepResponse,
    out: &mut Vec<PipeFile>,
) -> AppResult<()> {
    let new_filename = if operation.contains("auto-rename") {
        response.filename
    } else {
        remove_trailing_naming(&response.filename)
    };
    let file = PipeFile {
        name: new_filename,
        path: response.file.path.clone(),
        _temp: response.file._temp,
    };
    if is_zip(&file) {
        extract_zip(file, 0, out)
    } else {
        out.push(file);
        Ok(())
    }
}

async fn run_pipeline(uploads: Vec<Upload>, config: PipelineConfig) -> AppResult<Response> {
    let (max_steps, max_files) = settings::policy_limits();
    run_pipeline_with_limits(uploads, config, max_steps, max_files).await
}

async fn run_pipeline_with_limits(
    uploads: Vec<Upload>,
    config: PipelineConfig,
    max_steps: i64,
    max_files: i64,
) -> AppResult<Response> {
    let operations = config.operations.unwrap_or_default();
    if max_steps > 0 && operations.len() as i64 > max_steps {
        return Err(AppError::BadRequest(format!(
            "A pipeline may have at most {max_steps} steps; raise policies.maxSteps to allow more"
        )));
    }
    // Java validates every operation before the first step runs.
    for operation in &operations {
        if !is_valid_operation(operation) {
            let parameters = operation.parameters.clone().unwrap_or_default();
            return Err(AppError::BadRequest(format!(
                "Invalid operation: {} with parameters: {:?}",
                operation.operation, parameters
            )));
        }
    }

    // Keep the upload spool files alive for the whole run; PipeFile paths
    // point into them and UploadBytes deletes on drop.
    let mut files: Vec<PipeFile> = uploads
        .iter()
        .map(|u| PipeFile {
            name: simple_name(&u.name).to_string(),
            path: u.bytes.temp_path().to_path_buf(),
            _temp: None,
        })
        .collect();
    if files.is_empty() {
        return Ok(empty_ok());
    }

    let run_id = jobs::new_id();
    let mut has_errors = false;

    for operation in &operations {
        let meta = pipeline_table::meta(&operation.operation);
        // getExtensionTypes(false, op): no declaration or empty extension set
        // means unrestricted — Java's callers express that as ["ALL"].
        let input_types: Vec<&str> = if meta.extensions.is_empty() {
            vec!["ALL"]
        } else {
            meta.extensions.to_vec()
        };
        let parameters = operation.parameters.clone().unwrap_or_default();

        let mut new_files: Vec<PipeFile> = Vec::new();
        if !meta.multi_input {
            for file in &files {
                let mut matched = false;
                for extension in &input_types {
                    let lower = file.name.to_lowercase();
                    if *extension == "ALL" || lower.ends_with(extension) {
                        matched = true;
                        let Some(response) =
                            dispatch(&operation.operation, &[file], &parameters, &run_id).await
                        else {
                            // Fatal sub-request: Java's RestTemplate throws and
                            // the controller's catch-all returns an empty 200.
                            tracing::error!("pipeline step {} failed", operation.operation);
                            return Ok(empty_ok());
                        };
                        let is_filter = operation.operation.starts_with("/api/v1/filter/filter-");
                        if is_filter && response.file.byte_len() == 0 {
                            tracing::info!(
                                "pipeline: skipping file filtered by {}",
                                operation.operation
                            );
                            continue;
                        }
                        if response.status != StatusCode::OK {
                            tracing::warn!(
                                "pipeline step {} returned {}",
                                operation.operation,
                                response.status
                            );
                            has_errors = true;
                            continue;
                        }
                        collect_outputs(&operation.operation, response, &mut new_files)?;
                    }
                }
                if !matched {
                    let provided = file
                        .name
                        .rsplit_once('.')
                        .map(|(_, ext)| format!(".{}", ext.to_lowercase()))
                        .unwrap_or_else(|| "no extension".to_string());
                    tracing::warn!(
                        "pipeline: no files with extension {} found for operation {}. \
                         Provided file '{}' has extension: {}",
                        input_types.join(", "),
                        operation.operation,
                        file.name,
                        provided
                    );
                    has_errors = true;
                }
            }
        } else {
            let all: Vec<&PipeFile> = files.iter().collect();
            let matching: Vec<&PipeFile> = if input_types.contains(&"ALL") {
                all
            } else {
                files
                    .iter()
                    .filter(|f| {
                        let lower = f.name.to_lowercase();
                        input_types.iter().any(|ext| lower.ends_with(ext))
                    })
                    .collect()
            };
            if !matching.is_empty() {
                let Some(response) =
                    dispatch(&operation.operation, &matching, &parameters, &run_id).await
                else {
                    tracing::error!("pipeline step {} failed", operation.operation);
                    return Ok(empty_ok());
                };
                if response.status == StatusCode::OK {
                    collect_outputs(&operation.operation, response, &mut new_files)?;
                } else {
                    tracing::warn!(
                        "pipeline multi-input step {} returned {}",
                        operation.operation,
                        response.status
                    );
                    has_errors = true;
                }
            } else {
                let provided: Vec<String> = files
                    .iter()
                    .map(|f| {
                        f.name
                            .rsplit_once('.')
                            .map(|(_, ext)| format!(".{}", ext.to_lowercase()))
                            .unwrap_or_else(|| "no extension".to_string())
                    })
                    .collect();
                tracing::warn!(
                    "pipeline: no files with extension {} found for multi-input operation {}. \
                     Provided files have extensions: {} (total files: {})",
                    input_types.join(", "),
                    operation.operation,
                    provided.join(", "),
                    files.len()
                );
                has_errors = true;
            }
        }

        // PipelineLimits.requireWithinFileLimit, checked once per step with
        // the same cumulative count Java reaches file-by-file.
        if max_files > 0 && new_files.len() as i64 > max_files {
            return Err(AppError::BadRequest(format!(
                "Step {} produced more than {max_files} files; \
                 raise policies.maxFilesPerStep to allow more",
                operation.operation
            )));
        }
        files = new_files;
    }

    if has_errors {
        tracing::warn!("pipeline completed with per-file errors");
    }

    pipeline_response(&files).await
}

fn empty_ok() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .body(Body::empty())
        .expect("static response")
}

/// `WebResponseUtils.encodeAttachmentName`: UTF-8 percent-encoding with
/// `+` rewritten to `%20` (URLEncoder output post-processed).
fn encode_attachment_name(name: &str) -> String {
    let mut out = String::new();
    for &b in name.as_bytes() {
        let unreserved = b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'*' | b'_');
        if unreserved {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

async fn file_download(path: &std::path::Path, name: &str) -> AppResult<Response> {
    let len = std::fs::metadata(path)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("stat result: {e}")))?
        .len();
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("open result: {e}")))?;
    let stream = tokio_util::io::ReaderStream::new(file);
    Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, len)
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", encode_attachment_name(name)),
        )
        .body(Body::from_stream(stream))
        .map_err(|e| AppError::Internal(anyhow::anyhow!("response build: {e}")))
}

/// Single output → raw file; zero or many → `output.zip` (Java zips even an
/// empty result set).
async fn pipeline_response(files: &[PipeFile]) -> AppResult<Response> {
    if files.len() == 1 {
        return file_download(&files[0].path, &files[0].name).await;
    }
    let temp =
        NamedTempFile::new().map_err(|e| AppError::Internal(anyhow::anyhow!("zip temp: {e}")))?;
    {
        let file = std::fs::File::create(temp.path())
            .map_err(|e| AppError::Internal(anyhow::anyhow!("zip create: {e}")))?;
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        let mut counts: HashMap<String, usize> = HashMap::new();
        for entry in files {
            let original = entry.name.clone();
            let name = match counts.get(&original) {
                Some(&count) => {
                    counts.insert(original.clone(), count + 1);
                    dedup_name(&original, count)
                }
                None => {
                    counts.insert(original.clone(), 1);
                    original
                }
            };
            zip.start_file(name, options)
                .map_err(|e| AppError::Internal(anyhow::anyhow!("zip start: {e}")))?;
            let mut input = std::fs::File::open(&entry.path)
                .map_err(|e| AppError::Internal(anyhow::anyhow!("zip read: {e}")))?;
            std::io::copy(&mut input, &mut zip)
                .map_err(|e| AppError::Internal(anyhow::anyhow!("zip write: {e}")))?;
        }
        zip.finish()
            .map_err(|e| AppError::Internal(anyhow::anyhow!("zip finish: {e}")))?;
    }
    file_download(temp.path(), "output.zip").await
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// `POST /api/v1/pipeline/handleData` — multipart `fileInput[]` + `json`.
pub async fn handle_data(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, params) = receive_multipart(multipart).await?;
    let async_requested = jobs::async_requested(&query, &params);
    let run = run_request(uploads, params);
    if async_requested {
        jobs::submit(&state, run).await
    } else {
        run.await
    }
}

async fn run_request(uploads: Vec<Upload>, params: Vec<(String, String)>) -> AppResult<Response> {
    if uploads.is_empty() {
        return Ok(empty_ok());
    }
    // `HandleDataRequest.json` — last occurrence wins like other params.
    let json = params
        .iter()
        .rev()
        .find(|(n, _)| n == "json")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let config: PipelineConfig = serde_json::from_str(&json)
        .map_err(|e| AppError::BadRequest(format!("invalid pipeline json: {e}")))?;
    if config.operations.is_none() {
        return Err(AppError::BadRequest(
            "pipeline configuration requires a 'pipeline' operations list".into(),
        ));
    }
    run_pipeline(uploads, config).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatchable_paths_match_java_allowlist() {
        for ok in [
            "/api/v1/general/merge-pdfs",
            "/api/v1/misc/compress-pdf",
            "/api/v1/security/add-password",
            "/api/v1/convert/pdf/word",
            "/api/v1/filter/filter-page-count",
            "/api/v1/integration/external-api-call",
            "/api/v1/docparse/ingest",
            "/api/v1/ai/tools/classify-and-label",
            "/api/v1/form/fill",
            "/api/v1/form/fields",
        ] {
            assert!(is_dispatchable(ok), "{ok} should be dispatchable");
        }
        for bad in [
            "/api/v1/pipeline/handleData",
            "/api/v1/config/app-config",
            "/api/v1/form/fields-with-coordinates",
            "/api/v1/form/form-detection/detect",
            "/api/v1/ai/orchestrate",
            "/api/v1/general/../etc",
            "/api/v1/general/bad%20path",
            "/api/v1",
            "/health",
            "general/merge-pdfs",
        ] {
            assert!(!is_dispatchable(bad), "{bad} should not be dispatchable");
        }
    }

    #[test]
    fn remove_trailing_naming_matches_java() {
        assert_eq!(remove_trailing_naming("file_2.pdf"), "file.pdf");
        assert_eq!(remove_trailing_naming("a_b_c.pdf"), "a_b.pdf");
        assert_eq!(remove_trailing_naming("plain.pdf"), "plain.pdf");
        assert_eq!(remove_trailing_naming("noext"), "noext");
        assert_eq!(remove_trailing_naming(".hidden"), ".hidden");
        assert_eq!(remove_trailing_naming("a_b."), "a.");
    }

    #[test]
    fn dedup_name_drops_extension_like_java() {
        assert_eq!(dedup_name("report.pdf", 1), "report(1)");
        assert_eq!(dedup_name("report.final.pdf", 2), "report.final(2)");
    }

    #[test]
    fn url_decode_form_semantics() {
        assert_eq!(url_decode("a%20b.pdf"), "a b.pdf");
        assert_eq!(url_decode("a+b.pdf"), "a b.pdf");
        assert_eq!(url_decode("%E2%82%AC.pdf"), "€.pdf");
        assert_eq!(url_decode("plain.pdf"), "plain.pdf");
    }

    #[test]
    fn response_filename_parses_content_disposition() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_DISPOSITION,
            "attachment; filename=\"out%20file.pdf\"".parse().unwrap(),
        );
        assert_eq!(response_filename(&headers), "out file.pdf");

        let headers = HeaderMap::new();
        assert_eq!(response_filename(&headers), "default-filename.ext");
    }

    #[test]
    fn param_fields_flattens_arrays_and_skips_nulls() {
        let mut params = Map::new();
        params.insert("a".into(), Value::from("x"));
        params.insert("b".into(), Value::from(true));
        params.insert("c".into(), Value::from(2));
        params.insert("d".into(), Value::Null);
        params.insert(
            "e".into(),
            Value::Array(vec![Value::from("1"), Value::from("2")]),
        );
        params.insert("f".into(), serde_json::json!({"k": 1}));
        let fields = param_fields(&params);
        let get = |k: &str| -> Vec<&str> {
            fields
                .iter()
                .filter(|(n, _)| n == k)
                .map(|(_, v)| v.as_str())
                .collect()
        };
        assert_eq!(get("a"), ["x"]);
        assert_eq!(get("b"), ["true"]);
        assert_eq!(get("c"), ["2"]);
        assert!(get("d").is_empty());
        assert_eq!(get("e"), ["1", "2"]);
        assert_eq!(get("f"), ["{\"k\":1}"]);
    }

    fn pipe_file(name: &str, bytes: &[u8]) -> (PipeFile, NamedTempFile) {
        let temp = NamedTempFile::new().unwrap();
        std::fs::write(temp.path(), bytes).unwrap();
        (
            PipeFile {
                name: name.to_string(),
                path: temp.path().to_path_buf(),
                _temp: None,
            },
            temp,
        )
    }

    #[test]
    fn is_zip_detects_magic_and_cbz_exclusion() {
        let (file, _t) = pipe_file("a.pdf", &[0x50, 0x4B, 0x03, 0x04, 1, 2]);
        assert!(is_zip(&file));
        let (file, _t) = pipe_file("a.cbz", &[0x50, 0x4B, 0x03, 0x04, 1, 2]);
        assert!(!is_zip(&file));
        let (file, _t) = pipe_file("a.pdf", b"not a zip");
        assert!(!is_zip(&file));
    }

    #[test]
    fn extract_zip_flattens_entries() {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file("dir/inner.txt", options).unwrap();
            use std::io::Write;
            zip.write_all(b"hello").unwrap();
            zip.add_directory("empty/", options).unwrap();
            zip.finish().unwrap();
        }
        let bytes = buf.into_inner();
        let (file, _t) = pipe_file("bundle.zip", &bytes);
        // Give the file a real temp so extract_zip owns it.
        let owned = PipeFile {
            name: file.name.clone(),
            path: file.path.clone(),
            _temp: Some(_t),
        };
        let mut out = Vec::new();
        extract_zip(owned, 0, &mut out).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "dir/inner.txt");
        assert_eq!(std::fs::read(&out[0].path).unwrap(), b"hello");
    }

    fn upload(name: &str, bytes: &[u8]) -> Upload {
        Upload {
            name: name.to_string(),
            bytes: crate::upload::UploadBytes::from_vec(bytes.to_vec()),
            content_type: None,
        }
    }

    fn config(operations: &[(&str, serde_json::Value)]) -> PipelineConfig {
        let ops: Vec<PipelineOperation> = operations
            .iter()
            .map(|(operation, parameters)| PipelineOperation {
                operation: operation.to_string(),
                parameters: parameters.as_object().cloned(),
            })
            .collect();
        PipelineConfig {
            name: None,
            operations: Some(ops),
            output_dir: None,
            output_file_name: None,
        }
    }

    /// `platform::BOUND_ADDR` is process-global, so tests that repoint it at
    /// their stub must not run concurrently — a sibling's dispatch would hit
    /// this test's listener (or a dead one) and flake.
    static ADDR_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn bound_stub() -> tokio::sync::MutexGuard<'static, ()> {
        let guard = ADDR_LOCK.lock().await;
        platform::note_bound_addr(stub_pipeline_server());
        guard
    }

    /// A one-endpoint stand-in for the Rust server: returns a fixed file with
    /// a Content-Disposition name so `dispatch` has something to unpack.
    fn stub_pipeline_server() -> std::net::SocketAddr {
        use axum::routing::post;
        async fn echo(headers: HeaderMap, body: axum::body::Bytes) -> Response {
            let _ = headers;
            let _ = body;
            Response::builder()
                .header(header::CONTENT_TYPE, "application/pdf")
                .header(
                    header::CONTENT_DISPOSITION,
                    "attachment; filename=\"step_out.pdf\"",
                )
                .body(Body::from("step-bytes"))
                .unwrap()
        }
        async fn empty_filter() -> Response {
            Response::builder()
                .status(StatusCode::OK)
                .body(Body::empty())
                .unwrap()
        }
        let app = axum::Router::new()
            .route("/api/v1/general/merge-pdfs", post(echo))
            .route("/api/v1/general/split-pages", post(echo))
            .route("/api/v1/filter/filter-page-count", post(empty_filter));
        // Serve on a dedicated thread: a #[tokio::test] runtime dies with its
        // test, which would leave BOUND_ADDR pointing at a closed port while
        // sibling tests still dispatch through it.
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let addr = std_listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(std_listener).unwrap();
                axum::serve(listener, app).await.unwrap();
            });
        });
        addr
    }

    async fn response_bytes(response: Response) -> (HeaderMap, Vec<u8>) {
        let (parts, body) = response.into_parts();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        (parts.headers, bytes.to_vec())
    }

    #[tokio::test]
    async fn single_step_returns_the_step_file() {
        let _guard = bound_stub().await;
        let cfg = config(&[("/api/v1/general/merge-pdfs", serde_json::json!({}))]);
        let response = run_pipeline(vec![upload("a.pdf", b"pdf")], cfg)
            .await
            .unwrap();
        let (headers, body) = response_bytes(response).await;
        assert_eq!(body, b"step-bytes");
        // removeTrailingNaming strips the "_out" segment like Java.
        assert_eq!(
            headers[header::CONTENT_DISPOSITION].to_str().unwrap(),
            "attachment; filename=\"step.pdf\""
        );
    }

    #[tokio::test]
    async fn filter_empty_body_drops_the_file() {
        let _guard = bound_stub().await;
        let cfg = config(&[
            ("/api/v1/filter/filter-page-count", serde_json::json!({})),
            ("/api/v1/general/merge-pdfs", serde_json::json!({})),
        ]);
        let response = run_pipeline(vec![upload("a.pdf", b"pdf")], cfg)
            .await
            .unwrap();
        let (_, body) = response_bytes(response).await;
        // The filter dropped the only file, so the merge step saw no inputs
        // and the result set is empty — Java returns an empty zip here.
        assert!(body.starts_with(b"PK"));
    }

    #[tokio::test]
    async fn two_outputs_zip_with_dedup_names() {
        let _guard = bound_stub().await;
        // One single-input step over two uploads yields two same-named files.
        let cfg = config(&[("/api/v1/general/split-pages", serde_json::json!({}))]);
        let response = run_pipeline(vec![upload("a.pdf", b"pdf"), upload("b.pdf", b"pdf")], cfg)
            .await
            .unwrap();
        let (headers, body) = response_bytes(response).await;
        assert_eq!(
            headers[header::CONTENT_DISPOSITION].to_str().unwrap(),
            "attachment; filename=\"output.zip\""
        );
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(body)).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        // Java keeps the first occurrence's name and suffixes later ones.
        assert_eq!(names, ["step.pdf", "step(1)"]);
    }

    #[tokio::test]
    async fn step_and_file_limits_are_enforced() {
        let _guard = bound_stub().await;
        let ops = |n: usize| PipelineConfig {
            name: None,
            operations: Some(
                (0..n)
                    .map(|_| PipelineOperation {
                        operation: "/api/v1/general/merge-pdfs".into(),
                        parameters: Some(Map::new()),
                    })
                    .collect(),
            ),
            output_dir: None,
            output_file_name: None,
        };
        let err = run_pipeline_with_limits(vec![upload("a.pdf", b"pdf")], ops(3), 2, 0)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("at most 2 steps"));

        // Single-input step over two uploads produces two files — over a
        // per-step limit of one.
        let err = run_pipeline_with_limits(
            vec![upload("a.pdf", b"pdf"), upload("b.pdf", b"pdf")],
            config(&[("/api/v1/general/split-pages", serde_json::json!({}))]),
            50,
            1,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("more than 1 files"));
    }
}
