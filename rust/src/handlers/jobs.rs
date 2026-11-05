//! Async job framework matching Java's `AutoJobAspect` + `JobController` +
//! `JobExecutorService` contract:
//!
//! - `?async=true` on a migrated endpoint returns
//!   `{"async":true,"jobId":"<uuid>","result":null}` immediately and runs the
//!   work on a tokio task.
//! - `GET /api/v1/general/job/{jobId}` → `JobResult` JSON.
//! - `GET /api/v1/general/job/{jobId}/result` → raw file bytes for single-file
//!   results, `{jobId,hasMultipleFiles,files}` JSON for multi-file, else the
//!   inline result.
//! - `GET /api/v1/general/job/{jobId}/result/files` → file list JSON.
//! - `DELETE /api/v1/general/job/{jobId}` → cancel.
//! - `GET /api/v1/general/files/{fileId}[/metadata]` → stored result bytes.
//!
//! Jobs this module doesn't know about (created by Java via the proxy) fall
//! through to the Java backend, so both job namespaces coexist.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::config::JavaBackend;
use crate::error::AppResult;
use crate::handlers::proxy;

#[derive(Clone)]
pub struct AppState {
    pub backend: Option<Arc<JavaBackend>>,
    pub jobs: Arc<Mutex<HashMap<String, JobRecord>>>,
    /// fileId → stored result file (Rust-owned jobs only).
    pub files: Arc<Mutex<HashMap<String, StoredFile>>>,
}

impl AppState {
    pub fn new(backend: Option<Arc<JavaBackend>>) -> Self {
        Self {
            backend,
            jobs: Arc::new(Mutex::new(HashMap::new())),
            files: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

pub struct StoredFile {
    pub bytes: Vec<u8>,
    pub file_name: String,
    pub content_type: String,
}

#[derive(Clone)]
pub struct JobRecord {
    pub complete: bool,
    pub error: Option<String>,
    /// Inline (non-file) result, e.g. analysis JSON — Java's `result` field.
    pub result: Option<Value>,
    /// Single file result, if the job produced one.
    pub file: Option<StoredFileRef>,
    /// Multi-file results (e.g. future SIMO jobs).
    pub files: Vec<StoredFileRef>,
    pub created_at: u64,
    pub completed_at: Option<u64>,
}

#[derive(Clone)]
pub struct StoredFileRef {
    pub file_id: String,
    pub file_name: String,
    pub content_type: String,
    pub file_size: u64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn new_id() -> String {
    // Java uses UUID.randomUUID(); a time+counter id is collision-free for
    // this process's lifetime.
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        now(),
        (n >> 20) & 0xffff,
        n & 0xfff,
        ((n << 8) & 0xffff) | 0x8000,
        n.rotate_left(17)
    )
}

/// `?async=true` gate used by migrated handlers. `params` is the multipart
/// field list from `receive_multipart` (last occurrence wins, like Spring).
pub fn is_async(params: &[(String, String)]) -> bool {
    params
        .iter()
        .rev()
        .find(|(n, _)| n == "async")
        .is_some_and(|(_, v)| v == "true")
}

/// Java's `request.getParameter("async")` reads the query string and
/// multipart fields alike.
pub fn async_requested(query: &HashMap<String, String>, params: &[(String, String)]) -> bool {
    query.get("async").is_some_and(|v| v == "true") || is_async(params)
}

/// Runs `work` on a background task and returns the Java `JobResponse` body.
/// `work` must produce the same `Response` the sync path would have; file
/// responses are captured into the job store for `/result` retrieval.
pub async fn submit(
    state: &AppState,
    work: impl std::future::Future<Output = AppResult<Response>> + Send + 'static,
) -> AppResult<Response> {
    let job_id = new_id();
    state.jobs.lock().unwrap().insert(
        job_id.clone(),
        JobRecord {
            complete: false,
            error: None,
            result: None,
            file: None,
            files: Vec::new(),
            created_at: now(),
            completed_at: None,
        },
    );

    let jobs = state.jobs.clone();
    let files = state.files.clone();
    let id = job_id.clone();
    tokio::spawn(async move {
        match work.await {
            Ok(response) => {
                let (parts, body) = response.into_parts();
                let bytes = match axum::body::to_bytes(body, usize::MAX).await {
                    Ok(b) => b.to_vec(),
                    Err(e) => {
                        finish_error(&jobs, &id, &format!("failed to buffer result: {e}"));
                        return;
                    }
                };
                let content_type = parts
                    .headers
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("application/octet-stream")
                    .to_string();
                // JSON bodies are inline results in Java, not stored files.
                if content_type.contains("json") {
                    let result = serde_json::from_slice(&bytes).ok();
                    if let Some(record) = jobs.lock().unwrap().get_mut(&id) {
                        record.result = result;
                        record.complete = true;
                        record.completed_at = Some(now());
                    }
                    return;
                }
                let file_name = parts
                    .headers
                    .get(header::CONTENT_DISPOSITION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| {
                        v.split("filename=\"")
                            .nth(1)
                            .and_then(|s| s.strip_suffix('"'))
                            .map(str::to_string)
                    })
                    .unwrap_or_else(|| "result.pdf".into());
                let file_id = new_id();
                let file_size = bytes.len() as u64;
                files.lock().unwrap().insert(
                    file_id.clone(),
                    StoredFile {
                        bytes,
                        file_name: file_name.clone(),
                        content_type: content_type.clone(),
                    },
                );
                let reference = StoredFileRef {
                    file_id,
                    file_name,
                    content_type,
                    file_size,
                };
                if let Some(record) = jobs.lock().unwrap().get_mut(&id) {
                    record.file = Some(reference.clone());
                    record.files = vec![reference];
                    record.complete = true;
                    record.completed_at = Some(now());
                }
            }
            Err(e) => finish_error(&jobs, &id, &e.to_string()),
        }
    });

    Ok(Json(json!({"async": true, "jobId": job_id, "result": null})).into_response())
}

fn finish_error(jobs: &Arc<Mutex<HashMap<String, JobRecord>>>, id: &str, error: &str) {
    if let Some(record) = jobs.lock().unwrap().get_mut(id) {
        record.complete = true;
        record.error = Some(error.to_string());
        record.completed_at = Some(now());
    }
}

fn job_json(record: &JobRecord, job_id: &str) -> Value {
    json!({
        "jobId": job_id,
        "complete": record.complete,
        "error": record.error,
        "createdAt": record.created_at,
        "completedAt": record.completed_at,
        "result": record.result,
        "notes": [],
        "metadata": {},
        "allResultFiles": file_json(&record.files),
    })
}

fn file_json(files: &[StoredFileRef]) -> Vec<Value> {
    files
        .iter()
        .map(|f| {
            json!({
                "fileId": f.file_id,
                "fileName": f.file_name,
                "contentType": f.content_type,
                "fileSize": f.file_size,
            })
        })
        .collect()
}

fn find_job(jobs: &Arc<Mutex<HashMap<String, JobRecord>>>, job_id: &str) -> Option<JobRecord> {
    jobs.lock().unwrap().get(job_id).cloned()
}

async fn forward_or_404(
    state: &AppState,
    headers: &HeaderMap,
    method: &str,
    path: &str,
) -> Response {
    proxy::forward(&state.backend, method, path, headers).await
}

pub async fn job_status(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Some(record) = find_job(&state.jobs, &job_id) {
        return Json(job_json(&record, &job_id)).into_response();
    }
    forward_or_404(
        &state,
        &headers,
        "GET",
        &format!("/api/v1/general/job/{job_id}"),
    )
    .await
}

pub async fn job_result(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Some(record) = find_job(&state.jobs, &job_id) else {
        return forward_or_404(
            &state,
            &headers,
            "GET",
            &format!("/api/v1/general/job/{job_id}/result"),
        )
        .await;
    };
    if !record.complete {
        return (StatusCode::BAD_REQUEST, "Job is not complete yet").into_response();
    }
    if let Some(error) = &record.error {
        return (StatusCode::BAD_REQUEST, format!("Job failed: {error}")).into_response();
    }
    if record.files.len() > 1 {
        return Json(json!({
            "jobId": job_id,
            "hasMultipleFiles": true,
            "files": file_json(&record.files),
        }))
        .into_response();
    }
    if let Some(file) = &record.file {
        let files = state.files.lock().unwrap();
        if let Some(stored) = files.get(&file.file_id) {
            return Response::builder()
                .header(header::CONTENT_TYPE, &stored.content_type)
                .header(
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{}\"", stored.file_name),
                )
                .body(axum::body::Body::from(stored.bytes.clone()))
                .unwrap();
        }
        return (StatusCode::INTERNAL_SERVER_ERROR, "Error retrieving file").into_response();
    }
    if let Some(result) = &record.result {
        return Json(result.clone()).into_response();
    }
    Json(json!({"jobId": job_id, "result": null})).into_response()
}

pub async fn job_files(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Some(record) = find_job(&state.jobs, &job_id) else {
        return forward_or_404(
            &state,
            &headers,
            "GET",
            &format!("/api/v1/general/job/{job_id}/result/files"),
        )
        .await;
    };
    if !record.complete {
        return (StatusCode::BAD_REQUEST, "Job is not complete yet").into_response();
    }
    if let Some(error) = &record.error {
        return (StatusCode::BAD_REQUEST, format!("Job failed: {error}")).into_response();
    }
    Json(json!({
        "jobId": job_id,
        "fileCount": record.files.len(),
        "files": file_json(&record.files),
    }))
    .into_response()
}

pub async fn cancel_job(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if state.jobs.lock().unwrap().remove(&job_id).is_some() {
        return Json(json!({"jobId": job_id, "cancelled": true})).into_response();
    }
    forward_or_404(
        &state,
        &headers,
        "DELETE",
        &format!("/api/v1/general/job/{job_id}"),
    )
    .await
}

pub async fn download_file(
    State(state): State<AppState>,
    Path(file_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let stored = state
        .files
        .lock()
        .unwrap()
        .get(&file_id)
        .map(|s| (s.bytes.clone(), s.file_name.clone(), s.content_type.clone()));
    if let Some((bytes, file_name, content_type)) = stored {
        return Response::builder()
            .header(header::CONTENT_TYPE, content_type)
            .header(
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{file_name}\""),
            )
            .body(axum::body::Body::from(bytes))
            .unwrap();
    }
    forward_or_404(
        &state,
        &headers,
        "GET",
        &format!("/api/v1/general/files/{file_id}"),
    )
    .await
}

/// `POST /api/v1/general/jobs/cleanup` — drops completed jobs and their
/// stored files. Java's version is retention-policy based; this clears all
/// finished work, which matches its effect on a fresh server.
pub async fn cleanup_jobs(State(state): State<AppState>) -> Response {
    let mut jobs = state.jobs.lock().unwrap();
    let mut files = state.files.lock().unwrap();
    let mut removed = 0usize;
    let ids: Vec<String> = jobs
        .iter()
        .filter(|(_, r)| r.complete)
        .map(|(id, _)| id.clone())
        .collect();
    for id in ids {
        if let Some(record) = jobs.remove(&id) {
            removed += 1;
            for f in record.files {
                files.remove(&f.file_id);
            }
        }
    }
    Json(json!({
        "jobsRemoved": removed,
        "filesDeleted": removed,
        "jobsRetained": jobs.len(),
    }))
    .into_response()
}

pub async fn file_metadata(
    State(state): State<AppState>,
    Path(file_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let stored = state.files.lock().unwrap().get(&file_id).map(|s| {
        (
            s.file_name.clone(),
            s.content_type.clone(),
            s.bytes.len() as u64,
        )
    });
    if let Some((file_name, content_type, file_size)) = stored {
        return Json(json!({
            "fileId": file_id,
            "fileName": file_name,
            "contentType": content_type,
            "fileSize": file_size,
        }))
        .into_response();
    }
    forward_or_404(
        &state,
        &headers,
        "GET",
        &format!("/api/v1/general/files/{file_id}/metadata"),
    )
    .await
}
