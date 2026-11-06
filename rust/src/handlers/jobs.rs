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
//! Unknown job and file ids are 404s — there is no second backend owning
//! another job namespace.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::error::{AppError, AppResult};

#[derive(Clone)]
pub struct AppState {
    pub jobs: Arc<Mutex<HashMap<String, JobRecord>>>,
    /// fileId → stored result file (Rust-owned jobs only).
    pub files: Arc<Mutex<HashMap<String, StoredFile>>>,
    /// SQLite write-through store; `None` keeps jobs memory-only (tests,
    /// or a store that failed to open).
    pub store: Option<Arc<crate::jobstore::JobStore>>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            jobs: Arc::new(Mutex::new(HashMap::new())),
            files: Arc::new(Mutex::new(HashMap::new())),
            store: None,
        }
    }

    /// `new` plus the SQLite job store at `configs/stirling-jobs.db` under
    /// the base path — the Rust counterpart of Java's H2 job rows. Jobs
    /// left incomplete by a previous run are marked interrupted, matching
    /// what a restarted Java sees for dead executor threads.
    pub fn persistent() -> Self {
        let mut state = Self::new();
        let path = crate::settings::base_dir().join("configs/stirling-jobs.db");
        match crate::jobstore::JobStore::open(&path) {
            Ok(store) => match store.load_all() {
                Ok(stored) => {
                    for job in stored {
                        if !job.record.complete {
                            // A persisted incomplete job cannot resume — its
                            // work died with the last process.
                            let interrupted = JobRecord {
                                complete: true,
                                error: Some("Job interrupted by server restart".to_string()),
                                completed_at: Some(now()),
                                ..job.record.clone()
                            };
                            if let Err(e) = store.save_job(&job.job_id, &interrupted) {
                                tracing::warn!("job store save failed: {e}");
                            }
                            state
                                .jobs
                                .lock()
                                .unwrap()
                                .insert(job.job_id.clone(), interrupted);
                        } else {
                            state
                                .jobs
                                .lock()
                                .unwrap()
                                .insert(job.job_id.clone(), job.record.clone());
                        }
                        for (reference, stored_file) in job.files {
                            state
                                .files
                                .lock()
                                .unwrap()
                                .insert(reference.file_id.clone(), stored_file);
                        }
                    }
                    state.store = Some(Arc::new(store));
                    tracing::info!(
                        "job store: {} persisted jobs loaded from {}",
                        state.jobs.lock().unwrap().len(),
                        path.display()
                    );
                }
                Err(e) => {
                    tracing::warn!("job store load failed ({e}); jobs are memory-only")
                }
            },
            Err(e) => tracing::warn!("job store unavailable ({e:#}); jobs are memory-only"),
        }
        state
    }

    /// Writes the current in-memory record for `job_id` through to the
    /// store, if one is configured.
    fn persist_job(&self, job_id: &str) {
        let Some(store) = &self.store else { return };
        let record = self.jobs.lock().unwrap().get(job_id).cloned();
        if let Some(record) = record {
            if let Err(e) = store.save_job(job_id, &record) {
                tracing::warn!("job store save failed for {job_id}: {e}");
            }
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

pub(crate) fn new_id() -> String {
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
    state.persist_job(&job_id);

    let jobs = state.jobs.clone();
    let files = state.files.clone();
    let app_state = state.clone();
    let id = job_id.clone();
    tokio::spawn(async move {
        match work.await {
            Ok(response) => {
                let (parts, body) = response.into_parts();
                let bytes = match axum::body::to_bytes(body, usize::MAX).await {
                    Ok(b) => b.to_vec(),
                    Err(e) => {
                        finish_error(&jobs, &id, &format!("failed to buffer result: {e}"));
                        app_state.persist_job(&id);
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
                    app_state.persist_job(&id);
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
                let stored = StoredFile {
                    bytes,
                    file_name: file_name.clone(),
                    content_type: content_type.clone(),
                };
                if let Some(store) = &app_state.store {
                    if let Err(e) = store.put_file_data(&file_id, &stored) {
                        tracing::warn!("job store file write failed: {e}");
                    }
                }
                files.lock().unwrap().insert(file_id.clone(), stored);
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
                app_state.persist_job(&id);
            }
            Err(e) => {
                finish_error(&jobs, &id, &e.to_string());
                app_state.persist_job(&id);
            }
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

fn job_not_found(job_id: &str) -> Response {
    AppError::NotFound(format!("unknown job '{job_id}'")).into_response()
}

fn file_not_found(file_id: &str) -> Response {
    AppError::NotFound(format!("unknown file '{file_id}'")).into_response()
}

pub async fn job_status(State(state): State<AppState>, Path(job_id): Path<String>) -> Response {
    if let Some(record) = find_job(&state.jobs, &job_id) {
        return Json(job_json(&record, &job_id)).into_response();
    }
    job_not_found(&job_id)
}

pub async fn job_result(State(state): State<AppState>, Path(job_id): Path<String>) -> Response {
    let Some(record) = find_job(&state.jobs, &job_id) else {
        return job_not_found(&job_id);
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

pub async fn job_files(State(state): State<AppState>, Path(job_id): Path<String>) -> Response {
    let Some(record) = find_job(&state.jobs, &job_id) else {
        return job_not_found(&job_id);
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

pub async fn cancel_job(State(state): State<AppState>, Path(job_id): Path<String>) -> Response {
    // Java's cancelJob: unknown → 404, finished →
    // 400, otherwise the job is dropped and its stored files stay
    // downloadable until cleanup — same as Java keeping the result files.
    // Rust jobs have no queue, so `wasQueued` is always false.
    let record = state.jobs.lock().unwrap().get(&job_id).cloned();
    match record {
        None => job_not_found(&job_id),
        Some(r) if r.complete => (
            StatusCode::BAD_REQUEST,
            Json(json!({"message": "Cannot cancel job that is already complete"})),
        )
            .into_response(),
        Some(_) => {
            state.jobs.lock().unwrap().remove(&job_id);
            if let Some(store) = &state.store {
                if let Err(e) = store.delete_job(&job_id) {
                    tracing::warn!("job store delete failed for {job_id}: {e}");
                }
            }
            Json(json!({
                "message": "Job cancelled successfully",
                "wasQueued": false,
                "queuePosition": "n/a",
            }))
            .into_response()
        }
    }
}

pub async fn download_file(State(state): State<AppState>, Path(file_id): Path<String>) -> Response {
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
    file_not_found(&file_id)
}

/// `POST /api/v1/general/jobs/cleanup` — drops completed jobs and their
/// stored files. Java's version is retention-policy based; this clears all
/// finished work, which matches its effect on a fresh server.
pub async fn cleanup_jobs(State(state): State<AppState>) -> Response {
    let mut jobs = state.jobs.lock().unwrap();
    let mut files = state.files.lock().unwrap();
    let mut removed = 0usize;
    let mut files_deleted = 0usize;
    let ids: Vec<String> = jobs
        .iter()
        .filter(|(_, r)| r.complete)
        .map(|(id, _)| id.clone())
        .collect();
    for id in ids {
        if let Some(record) = jobs.remove(&id) {
            removed += 1;
            files_deleted += record.files.len();
            for f in record.files {
                files.remove(&f.file_id);
            }
            if let Some(store) = &state.store {
                if let Err(e) = store.delete_job(&id) {
                    tracing::warn!("job store delete failed for {id}: {e}");
                }
            }
        }
    }
    Json(json!({
        "jobsRemoved": removed,
        "filesDeleted": files_deleted,
        "jobsRetained": jobs.len(),
    }))
    .into_response()
}

pub async fn file_metadata(State(state): State<AppState>, Path(file_id): Path<String>) -> Response {
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
    file_not_found(&file_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn is_async_requires_exact_true() {
        assert!(is_async(&params(&[("async", "true")])));
        assert!(!is_async(&params(&[("async", "false")])));
        assert!(!is_async(&params(&[("async", "1")])));
        assert!(!is_async(&params(&[])));
        // Last occurrence wins, matching Spring's parameter binding.
        assert!(!is_async(&params(&[("async", "true"), ("async", "false")])));
    }

    #[test]
    fn async_requested_checks_query_and_fields() {
        let mut query = HashMap::new();
        query.insert("async".to_string(), "true".to_string());
        assert!(async_requested(&query, &[]));

        let mut query = HashMap::new();
        query.insert("async".to_string(), "yes".to_string());
        assert!(!async_requested(&query, &[]));
        assert!(async_requested(&query, &params(&[("async", "true")])));
    }

    async fn wait_complete(state: &AppState, job_id: &str) -> JobRecord {
        for _ in 0..100 {
            if let Some(record) = find_job(&state.jobs, job_id) {
                if record.complete {
                    return record;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("job {job_id} did not complete");
    }

    #[tokio::test]
    async fn submit_runs_work_and_stores_json_result() {
        let state = AppState::new();
        let response = submit(&state, async {
            Ok(Json(json!({"answer": 42})).into_response())
        })
        .await
        .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let ack: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(ack["async"], true);
        let job_id = ack["jobId"].as_str().unwrap().to_string();

        let record = wait_complete(&state, &job_id).await;
        assert!(record.error.is_none());
        assert_eq!(record.result, Some(json!({"answer": 42})));

        // /result returns the inline JSON, like Java's JobResult payload.
        let res = job_result(State(state.clone()), Path(job_id.clone())).await;
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let result: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(result["answer"], 42);
    }

    #[tokio::test]
    async fn submit_stores_file_results_for_download() {
        let state = AppState::new();
        submit(&state, async {
            Ok(crate::handlers::merge::file_response(
                b"%PDF-fake".to_vec(),
                "out.pdf",
                "application/pdf",
            ))
        })
        .await
        .unwrap();
        let job_id = state.jobs.lock().unwrap().keys().next().unwrap().clone();
        let record = wait_complete(&state, &job_id).await;
        let file_ref = record.file.expect("file result stored");

        // The stored file downloads with its recorded name/type.
        let res = download_file(State(state.clone()), Path(file_ref.file_id.clone())).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers().get(header::CONTENT_DISPOSITION).unwrap(),
            "attachment; filename=\"out.pdf\""
        );
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"%PDF-fake");

        // Metadata endpoint reports the same file.
        let res = file_metadata(State(state.clone()), Path(file_ref.file_id.clone())).await;
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let meta: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(meta["fileName"], "out.pdf");
        assert_eq!(meta["fileSize"], 9);
    }

    #[tokio::test]
    async fn submit_records_work_errors() {
        let state = AppState::new();
        submit(&state, async {
            Err(crate::error::AppError::BadRequest("boom".into()))
        })
        .await
        .unwrap();
        let job_id = state.jobs.lock().unwrap().keys().next().unwrap().clone();
        let record = wait_complete(&state, &job_id).await;
        assert!(record.error.is_some());

        // A failed job reports through /result like Java's error payload.
        let res = job_result(State(state), Path(job_id)).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn unknown_job_is_a_404() {
        // Unknown ids answer a 404 problem detail, not a panic.
        let state = AppState::new();
        let res = job_status(State(state), Path("nope".into())).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn cancel_removes_rust_owned_jobs() {
        let state = AppState::new();
        state.jobs.lock().unwrap().insert(
            "job-1".into(),
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
        let res = cancel_job(State(state.clone()), Path("job-1".into())).await;
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["message"], "Job cancelled successfully");
        assert_eq!(json["wasQueued"], false);
        assert!(state.jobs.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn cancel_rejects_completed_jobs() {
        let state = AppState::new();
        state.jobs.lock().unwrap().insert(
            "done".into(),
            JobRecord {
                complete: true,
                error: None,
                result: None,
                file: None,
                files: Vec::new(),
                created_at: now(),
                completed_at: Some(now()),
            },
        );
        let res = cancel_job(State(state.clone()), Path("done".into())).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(!state.jobs.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn cleanup_drops_completed_jobs_and_files() {
        let state = AppState::new();
        {
            let mut jobs = state.jobs.lock().unwrap();
            jobs.insert(
                "done".into(),
                JobRecord {
                    complete: true,
                    error: None,
                    result: Some(json!({})),
                    file: None,
                    files: vec![StoredFileRef {
                        file_id: "f1".into(),
                        file_name: "a.pdf".into(),
                        content_type: "application/pdf".into(),
                        file_size: 1,
                    }],
                    created_at: now(),
                    completed_at: Some(now()),
                },
            );
            jobs.insert(
                "running".into(),
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
        }
        state.files.lock().unwrap().insert(
            "f1".into(),
            StoredFile {
                bytes: vec![1],
                file_name: "a.pdf".into(),
                content_type: "application/pdf".into(),
            },
        );

        let res = cleanup_jobs(State(state.clone())).await;
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["jobsRemoved"], 1);
        assert_eq!(json["jobsRetained"], 1);
        assert!(state.jobs.lock().unwrap().contains_key("running"));
        assert!(!state.files.lock().unwrap().contains_key("f1"));
    }

    #[tokio::test]
    async fn job_result_rejects_incomplete_jobs() {
        let state = AppState::new();
        state.jobs.lock().unwrap().insert(
            "pending".into(),
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
        let res = job_result(State(state), Path("pending".into())).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }
}
