# A06 — Background Jobs & Persistence (round 2, 2026-11-07)

## 1. Architecture summary

**Rust job framework** — `rust/src/handlers/jobs.rs` (762 lines) + `rust/src/jobstore.rs` (300 lines):

- `AppState` (`jobs.rs:31-39`): `jobs: HashMap<String, JobRecord>` (in-memory hot path), `files: HashMap<String, StoredFile>` (**all result bytes as `Vec<u8>` in RAM**), `store: Option<Arc<JobStore>>` (SQLite write-through).
- `AppState::persistent()` (`jobs.rs:54-107`, wired `main.rs:96`) opens `JobStore` at `<STIRLING_BASE_PATH>/configs/stirling-jobs.db` (`settings.rs:584-589`), hydrates all jobs, marks previously-incomplete jobs "Job interrupted by server restart". Any open/load failure → warn + silently memory-only.
- `JobStore` (`jobstore.rs`): SQLite via `rusqlite` bundled (`Cargo.toml:45`). Schema `jobs`/`job_files`/`file_data` (`jobstore.rs:19-41`); file **blobs as BLOBs inside SQLite**. `save_job` upserts + replaces `job_files` refs (`:72-110`); `put_file_data` separate (`:112-118`); `delete_job` cascades (`:123-136`); `load_all` hydrates everything incl. blobs (`:140-221`).
- `submit()` (`jobs.rs:191-289`): mints id, inserts incomplete record, persists, `tokio::spawn`s work. On success buffers **entire response body** (`to_bytes(body, usize::MAX)`, `:218`), classifies JSON → inline `result`, else stores bytes as `StoredFile` in memory + `file_data` blob, `complete=true`. **No JoinHandle, no timeout, no concurrency limit.**
- Routes (`mod.rs:323-335`): `GET /api/v1/general/job/{id}`, `GET .../result`, `GET .../result/files`, `DELETE .../job/{id}` (cancel), `GET /api/v1/general/files/{fileId}[/metadata]`, `POST /api/v1/general/jobs/cleanup`. All behind `api_guard` + optional `RUST_API_KEY` (`mod.rs:381-382`, `auth.rs:83-94`) — **no per-user/job ownership checks**.
- Only two call sites honor `?async=true`: `pipeline.rs:734-737` and `platform.rs:1210-1217` (`update-enable-analytics`). **Every other migrated endpoint ignores `async`.**
- Pipeline async: uploads spooled to `NamedTempFile`s inside `run` future (`upload.rs:156-191`); outputs stream from temp files; multi-output → single `output.zip` (`pipeline.rs:683-720`).

**Java counterpart** — `JobExecutorService.java`, `TaskManager.java`, `JobController.java`, `AutoJobAspect.java`, `JobResult.java`, `FileStorage`/`FileStore`:

- `JobExecutorService.runJobGeneric` (`:103-254`): UUID jobId, optional user scoping, **timeout** = `min(asyncRequestTimeout=1200000, session timeout)` (`:65-74,125,193`), **queueing** via `JobQueue`+`ResourceMonitor`+`resourceWeight` (`:135-176`), virtual threads.
- `TaskManager`: in-memory `ConcurrentHashMap` + write-through cluster `JobStore` (Valkey/H2, TTL 30 min); **scheduled cleanup every 10 min**: completed expire after `jobResultExpiryMinutes=30`, abandoned pending after `pendingJobExpiryMinutes=1440` (`:49-70,360-446`). `setFileResult` **unzips ZIP results into multiple `ResultFile`s** (`:110-149`).
- `JobController`: per-job ownership validation when `JobOwnershipService` present (`:73-77,483-494`); cancel **marks error "Job was cancelled by user" and keeps record/files** (`:185-202`); `/jobs/cleanup` retention/age-based unless forced (`:222-244`); downloads via `FileStore` on **disk**.
- `JobResult` serializes `createdAt`/`completedAt` ISO `LocalDateTime` + `notes`/`metadata`.

## 2. Findings

### A06-F01 — No retention/expiry: finished jobs and blobs grow forever — P1
`jobs.rs:455-485` (only manual `cleanup_jobs`); no scheduler in `rust/src`. Java auto-expires completed after 30 min, abandoned pending after 24 h, sweeping every 10 min (`TaskManager.java:49-70,380-446`). Rust `cleanup_jobs` only runs if client POSTs it, and only removes `complete` jobs. Every completed job's result bytes persist in `stirling-jobs.db` AND `files` map forever → unbounded disk+RAM; abandoned incomplete jobs never expire. Fix: tokio interval task replicating Java thresholds via `JobStore::delete_job` + `files` removal. SQLite adequate; missing piece is the sweeper.

### A06-F02 — No job timeout — P1
`jobs.rs:214-286` bare `tokio::spawn`, no `tokio::time::timeout`; Java caps at `min(asyncRequestTimeout=20min, sessionTimeout)` (`JobExecutorService.java:65-74,193-197`). Hung pipeline/engine call → job `complete=false` forever (until restart marks interrupted), holds temp files+memory indefinitely. Fix: `tokio::time::timeout` ~20 min configurable; note timeout can't kill blocking `spawn_blocking`/children unless cancellable.

### A06-F03 — Cancellation does not stop work and leaks result storage — P1
`jobs.rs:402-430` (cancel), `:214-286` (spawn), `:261-279` (completion). `cancel_job` removes record + DB row but spawned task never aborted (no JoinHandle/AbortHandle). Orphaned work finishing still `put_file_data` (`:261-265`) + `files.insert` (`:266`) but `jobs.get_mut` → None → no `job_files` ref → **permanent orphan BLOB + RAM leak** unreachable by `delete_job`/`cleanup_jobs`. Temp files live until work ends. Semantic divergence: Java cancel keeps record with `error="Job was cancelled by user"`; Rust 404s it. Fix: store AbortHandle and abort; or gate completion path on job existence before `put_file_data`/`files.insert`; match Java mark-error semantics or document.

### A06-F04 — All result bytes in RAM; SQLite duplicates; full hydration at startup — P1
`jobs.rs:35` (`files` map `Vec<u8>`), `jobstore.rs:174-207` (`load_all` pulls every BLOB), `main.rs:96`. Java `FileStore` keeps bytes on disk + streams. Rust keeps every result's full bytes for process lifetime; restart rehydrates ALL historical blobs at once; bytes exist twice (DB+heap). Memory grows with cumulative job volume; few large jobs can OOM. Fix: payloads on disk (`<base>/job-files/<file_id>`), metadata-only SQLite, stream downloads (`jobs.rs:373,438` clone full Vec per request).

### A06-F05 — Non-atomic persistence: crash windows lose/orphan data — P2
`jobstore.rs:72-118`. `save_job` upsert + `DELETE job_files` + inserts, no transaction (autocommit each). `put_file_data` separate write. Crash mid-`save_job` → job `complete` but refs gone → `/result` 500s (silent result loss). Crash between `put_file_data` and `save_job` → orphan `file_data` forever. Fix: `rusqlite::Transaction`; `PRAGMA journal_mode=WAL` + `busy_timeout`.

### A06-F06 — Store failure/corruption silently degrades to memory-only — P2
`jobs.rs:54-107`. `JobStore::open`/`load_all` failure (corrupt DB, perms, disk full) → `warn!` + continue `store=None` → jobs don't survive restart at all, no surfaced error/repair/metric. Fix: retry fresh/backup path or `error!` + health signal.

### A06-F07 — Multi-file/ZIP async results diverge from Java — P2
`pipeline.rs:683-720` (multi-output → `output.zip`), `:664-681` (`file_download` hardcodes `application/octet-stream`); `jobs.rs:356-363` (multi-file JSON branch dead — `record.files` always `vec![one]`, `:275`). Java `TaskManager.setFileResult` detects `.zip` → extracts to multiple `ResultFile`s → `/result` returns `{hasMultipleFiles:true,files:[...]}` (`TaskManager.java:110-149`). Rust stores zip as single octet-stream → `/result` returns raw zip bytes, `/result/files` `fileCount:1`; wrong Content-Type on downloads. Fix: unzip `output.zip` into multiple StoredFileRefs in `submit`, or preserve real content type + document contract.

### A06-F08 — Unbounded concurrency; entire result buffered — P2
`jobs.rs:214` bare spawn, `:218` `to_bytes(usize::MAX)`. No semaphore/queue/backpressure — N async submissions = N concurrent pipelines each holding full output Vec (+ StoredFile copy + SQLite BLOB copy). Java gates via `JobQueue`/`ResourceMonitor`/`resourceWeight` (`JobExecutorService.java:135-176`) and streams to disk. Fix: `tokio::sync::Semaphore` + queue-position reporting; stream result bodies to disk.

### A06-F09 — Job/file endpoints have no ownership checks — P2 (conditional)
`jobs.rs:339-505`; `auth.rs:83-118`. Java validates per-user job access when `JobOwnershipService` exists (`JobController.java:73-77,483-494`). Rust only optional `RUST_API_KEY`; `auth.rs:98-118` warns proprietary authz isn't ported. IDOR if Rust exposed directly in secured deployment (UUIDs unguessable but leak via logs/referers). Acceptable only behind Java edge. Fix: document "Java-in-front only" harder or implement owner claim check.

### A06-F10 — `createdAt`/`completedAt` epoch seconds not ISO — P3
`jobs.rs:150-155,299-311` vs `JobResult.java:38-41` ISO `LocalDateTime`. `notes`/`metadata` hardcoded `[]`/`{}`. Minor client-compat risk; frontend polls automation/pipeline-run APIs not `/general/job`.

### A06-F11 — Async coverage minimal — P3
Only `pipeline.rs:734` and `platform.rs:1210` call `jobs::submit`; module doc implies general support. All other migrated endpoints ignore `?async=true` unlike Java `@AutoJobPostMapping`. Fix: wire `async_requested`/`submit` into remaining POST handlers or narrow contract.

### A06-F12 — `new_id` fabricated UUID with per-process counter — P3
`jobs.rs:157-170`: time-seconds+counter bit-packed; counter resets per process → same-second cross-restart collision possible → `ON CONFLICT` overwrite (see A04-F20). Not CSPRNG (minor enumeration, mitigated by ~48-bit space). Fix: `uuid::Uuid::new_v4()`.

### A06-F13 — Malformed JSON results silently become `result:null` — P3
`jobs.rs:233-241`: JSON parse failure → `record.result` stays None but job `complete` → `/result` `{"result":null}` indistinguishable from empty. Fix: store raw body as file or set `error`.

### A06-F14 — Per-request full-byte clones on download — P3
`jobs.rs:373` (`stored.bytes.clone()`), `:438`. Every `/result` and `/files/{id}` clones entire result; combined with F04 triples peak memory per download. Subsumed by F04 disk/streaming fix.

## 3. Verified answers

- **In-memory vs persisted?** Both: memory maps hot path; SQLite `configs/stirling-jobs.db` real via `AppState::persistent()` (`main.rs:96`); schema sane; blobs in `file_data`. Fallback = silent memory-only (F06). Incomplete jobs marked "Job interrupted by server restart" (`jobs.rs:61-69`) — good.
- **Survive restart?** Yes for completed (fully hydrated); but F04 all-blobs→RAM and F06 failure→total loss.
- **Progress?** None — `JobRecord` has no progress field; `notes:[]`/`metadata:{}` hardcoded. Java similar (progress via notes/metadata) — rough parity but nothing reflects real work.
- **Cancellation?** Cosmetic: record deleted, work not aborted, storage leaks (F03). Temp files RAII inside work future — clean on completion, not on cancel.
- **Timeouts?** None (F02). **Failed-job recovery?** Errors recorded+persisted; restart marks stragglers interrupted. **Concurrency limits?** None (F08). **Temp cleanup?** Upload/intermediate temps `NamedTempFile`/`TempDir` RAII; **result blobs never cleaned except manual `/jobs/cleanup`** (F01). **Retention?** None automatic. **Idempotency?** None — duplicate submits duplicate jobs (same as Java).
- **Results storage/download/deletion?** Single-file in practice; RAM+SQLite; `/result` or `/files/{fileId}`; deletion only cancel-of-incomplete or manual cleanup — **completed-job blobs have no automatic deletion** (F01).
- **Frontend API shape?** Mostly compatible (`jobId`, `complete`, `error`, `allResultFiles`{fileId/fileName/contentType/fileSize}, `hasMultipleFiles`). Divergences: epoch vs ISO timestamps (F10), cancel→404 vs error-complete (F03), multi-file zip never produced (F07), wrong content types (F07), no `queueInfo` (moot).
- **Is SQLite adequate?** Yes for metadata at this scale; **storing result BLOBs in SQLite+RAM is the problem** (F04). Recommend: keep SQLite, move payloads to disk like Java `FileStore`, add WAL+transactions+retention sweeper.

## 4. Could not verify

- Whether Rust server ever deployed without Java front in secured mode (determines F09 severity).
- Whether external consumers poll `/api/v1/general/job/*` today; in-repo frontend uses automation/pipeline-run endpoints.
- No tests run (read-only); all findings from code inspection.
