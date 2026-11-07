# A04 — Rust Architecture & Code Quality (round 2, 2026-11-07)

## 1. Architecture summary

- Root workspace `rust/Cargo.toml`: members `[".", "macros", "cli"]`; `engine` excluded (own workspace).
- `stirling-server`: binary entry `rust/src/main.rs`; `lib.rs` effectively empty; integration-style tests via `#[cfg(test)] mod apitests` in main.rs.
- `rust/macros`: `#[stirling_macros::endpoint]` turns sync PDF work fns into Axum multipart handlers, adds `?async=true` job dispatch, runs work through `tokio::task::spawn_blocking` (`macros/src/lib.rs:193-224`).
- `rust/cli`: ops CLI (config, database, diagnostics, dev, engines, install, migration, runtime, smoke, test); platform modules linux/macos/windows.
- `rust/engine`: separate Axum AI/document service — routes → agents/services → contracts/documents/llm → state/config/middleware.

Server layering: boot/config `main.rs:91-98`; HTTP surface `handlers/mod.rs` (21 modules, central route table `:32-385`); middleware metrics→auth→api_guard→CORS→trace→compression (`main.rs:100-151`, `guard.rs:37-51`, `auth.rs:83-94`). Shared libs: `upload.rs`, `pdfdoc.rs`, `convert.rs`, `pdftext/`, `text.rs`, `tabula.rs`, `forms.rs`, `signature.rs`, `timestamp.rs`, `pdfua.rs`, `verapdf.rs`, `img*`, `attachments.rs`, `error.rs` (Java-compatible `application/problem+json`), `jobstore.rs`+`handlers/jobs.rs`, `settings.rs`, `config.rs`, `endpointconfig.rs`, `guard.rs`, `auth.rs`, `metrics.rs`, `web.rs`.

Largest files (approx lines): `handlers/form.rs` ~5,333; `handlers/convert.rs` ~5,155; `handlers/security.rs` ~3,990; `handlers/misc.rs` ~3,650; `pdfdoc.rs` ~3,290; `tabula.rs` ~2,650; `handlers/automation.rs` ~1,700; `handlers/platform.rs` ~1,650; `signature.rs` ~1,590; `handlers/pipeline.rs` ~1,100; `forms.rs` ~1,090.

Thread-safety: **no `unsafe` in server/engine**; one unsafe in CLI (`pre_exec` setsid, `cli/src/services/process.rs:129-137`). Shared state mostly short-held Mutex/OnceLock/RwLock; problems are lock-held I/O, unbounded in-memory job payloads, sync SQLite/FS in async paths.

## 2. Findings

### A04-F01 — Multipart upload lacks aggregate limits; blocking spool I/O on Tokio workers — P1
`upload.rs:38-60,164-212`; `form.rs:1316-1324`; `automation.rs:776-816`. Per-part cap only; no aggregate cap on file parts/total bytes/param bytes. `spool_part` uses sync `std::fs::File::write_all` (`upload.rs:46-58`); `Upload::from_field` calls `sync_all()` synchronously (`:175-176`); `read_part_bytes` accumulates params into `Vec` up to per-part cap (`:64-79`); `receive_multipart` pushes every `fileInput` (`:203-205`), `first_upload` ignores extras (`:246-251`); form params `field.text().await` uncapped (`form.rs:1320-1324`); scanner spools all `files` before session limits checked (`automation.rs:778-787,841-863`). Fix: async fs or bounded blocking writer; aggregate budgets; enforce scanner limits while streaming.

### A04-F02 — Async job results fully buffered in memory, duplicated memory+SQLite — P1
`jobs.rs:214-279,365-374,432-446`; `jobstore.rs:112-118,140-220`. `to_bytes(..., usize::MAX)` (`jobs.rs:218`); `StoredFile{bytes:Vec<u8>}` in `AppState.files` AND SQLite BLOB; downloads clone whole Vec under mutex (`jobs.rs:433-438`); `load_all` hydrates every BLOB at startup (`jobstore.rs:173-187`, `jobs.rs:85-90`). Fix: store once (Bytes/Arc or disk), lazy hydration, stream downloads, don't hold mutex while cloning.

### A04-F03 — Cancelling a job orphans its stored files permanently — P1
`jobs.rs:402-421,432-449,455-478`. `cancel_job` removes job record + DB rows but leaves in-memory `StoredFile`s; `cleanup_jobs` only iterates `state.jobs` → orphans undiscoverable; `download_file` serves by `file_id` without job existence. Large cancelled-job results leak in RAM indefinitely. Fix: keep cancelled record until cleanup, or purge orphan file_ids; delete StoredFiles on cancel if Java parity permits.

### A04-F04 — Job persistence non-atomic; tolerates inconsistent blob/ref state — P2
`jobs.rs:111-118,261-279`; `jobstore.rs:72-118,123-135,173-207`. `put_file_data` failure warn-only but job persisted with refs; `save_job` multi-statement no transaction; hydration keeps refs without payloads. Crash → orphaned blobs / missing files / partial metadata. Fix: single transaction for job+refs+blob; blob-write failure = job failure; startup reconciliation.

### A04-F05 — Hand-written async paths do synchronous FS/archive/SQLite on Tokio workers — P2
`pipeline.rs:186-219,409-420,454-455,462-639,664-720`; `automation.rs:852-926,1102,1228-1232`; `platform.rs:329-490,554-561`; `jobs.rs`+`jobstore.rs`. Pipeline ZIP extraction `std::fs`/`zip` in async `run_pipeline`; `pipeline_response` sync ZIP write; scanner mutex held during dir create+copy; sync `remove_file`/`remove_dir_all`; platform `read_dir`/`read_to_string`; jobstore rusqlite from async handlers. Fix: `spawn_blocking`/`tokio::fs`; narrow mutex scope.

### A04-F06 — Engine runner can hang after timeout if grandchildren hold pipes — P2
`convert.rs:172-257`. Reader threads `read_to_end`; timeout kills only direct child (no process-group kill, `:223-226`); `rx.recv()` unbounded (`:239-248`). Grandchild inheriting pipes → `recv()` blocks forever → spawn_blocking worker pinned. Output accumulated unbounded. Fix: process-group kill or bounded post-kill drain; cap captured output.

### A04-F07 — Engine SQLite vector store does sync rusqlite in async methods — P2
`rust/engine/src/documents/sqlite_vec_store.rs:295-321,672-824,883-972`. `Mutex<Connection>` (tokio) + synchronous queries/transactions under lock; `add_documents` schema checks+deletes+inserts+commit under `conn.lock().await`. Fix: `spawn_blocking`/dedicated blocking thread; Arc snapshots outside lock.

### A04-F08 — Settings/config loading silently swallows errors — P2
`settings.rs:591-594,619-628,693-705`; `config.rs:101-145`; `main.rs:91-98`. YAML read/parse failure → silent defaults, no logging; `serde_json::from_value(merged).unwrap_or_default()` (`settings.rs:627`); bad `PORT` → silent 8081, bad `HOST` → panic at bind (`config.rs:127-145`). Typos in settings.yml silently reset security-relevant flags. Fix: warn/fail-fast; validate HOST/PORT in `from_env` returning Err.

### A04-F09 — Handler modules are god files mixing endpoints, domain logic, tests — P2
`form.rs` ~5,333, `convert.rs` ~5,155, `security.rs` ~3,990, `misc.rs` ~3,650 lines. Fix: split by domain/endpoint family; keep `handlers/mod.rs` thin registry.

### A04-F10 — Multipart parameter helpers duplicated across handlers — P3
`upload.rs:254-310` canonical; identical `last_param`/`param_opt` reimplemented in `misc.rs:483-489`, `security.rs:1969-1985`, `automation.rs:31-47`, `passwords.rs:49-55`, `layout.rs:38-57`. `all_params` only in security; `param_bool_default`/`choice_param` variants elsewhere. Fix: add `param_opt`/`all_params`/defaulted-bool to `upload.rs`, delete local copies.

### A04-F11 — File validation ad hoc across endpoints — P3
`upload.rs:244-251`; `pdfdoc.rs:25-29`; `misc.rs:606-611`; `security.rs:2037-2042,3252-3257`; `form.rs:1328-1333`. Empty checks with inconsistent messages; no shared "required non-empty PDF upload" helper. Fix: `require_pdf_upload(uploads, field)` helper.

### A04-F12 — `certsign.rs` orphaned incomplete module; related crypto deps unused — P3
`rust/src/certsign.rs` (~386 lines) not declared in `main.rs:1-36`; ends `unreachable!()` (`certsign.rs:385`). `Cargo.toml` declares `http-body-util`, `const-oid`, `pkcs8`, `pkcs12`, `hmac`, `des`, `cbc`, `rc2` — no compiled uses. `endpointconfig.rs:370-372` comment "needs a crypto stack Rust does not have" is stale relative to orphan file/deps. Fix: finish+wire certsign or delete file+deps (verify `cargo machete`).

### A04-F13 — `pdftext` dead-code comment stale; blanket `allow(dead_code)` hides real dead surface — P3
`main.rs:22-25` claims pdftext "not yet routed" — false: `security.rs:2021-2071,2672-2824` uses `pdftext::redact::*`, `convert.rs:4125` uses `pdftext::converter`. Fix: correct comment; narrow allow(dead_code).

### A04-F14 — API-key comparison not constant-time — P3
`auth.rs:73-94` ordinary string equality; engine uses `subtle::ConstantTimeEq` (`rust/engine` middleware) — inconsistent.

### A04-F15 — Engine config cache: Unix-only import unconditional; non-constant-time HMAC — P3
`rust/engine/src/config_cache.rs:11` unconditional `use std::os::unix::fs::PermissionsExt` (used only under `#[cfg(unix)]`) → Windows engine build fails; `:157` `!=` on HMAC tag despite `subtle` dep.

### A04-F16 — Engine routes can return internal error details — P3
`rust/engine/src/error.rs:54-56,87-91`; `routes/agents.rs:45-47`; `routes/pdf_questions.rs:28,33-37`. `ApiError::internal(format!("...: {e}"))` leaks provider/agent internals; generic `From<anyhow::Error>` correctly hides. Fix: generic client detail + server-side `{e:#}` logging.

### A04-F17 — CLI `run_with_timeout` can deadlock on full pipes — P2
`cli/src/services/process.rs:45-97`: pipes created, `try_wait` polled without draining; child filling pipe blocks on write → always times out, output lost. Fix: `wait_with_output()` or reader threads (pattern exists in `convert::run`).

### A04-F18 — CLI `spawn_detached` unconditionally imports Unix `CommandExt` — P2 (Windows build break)
`cli/src/services/process.rs:108` unconditional `use std::os::unix::process::CommandExt` though branches are `#[cfg(unix)]`/`#[cfg(windows)]` (`:129-145`). Windows CLI cannot compile despite `platform/windows.rs` existing.

### A04-F19 — CLI component names unsanitized; systemd unit paths unquoted — P3
`cli/src/services/filesystem.rs:31-33`; `commands/runtime.rs:30-32,78-103`; `commands/install.rs:176-182`. `name` interpolated into `.run/{name}.pid`/`.cmd` (`../x` escapes); `ExecStart={}`/`WorkingDirectory={}` raw (spaces/`%` break units). Fix: charset-validate names; quote/`systemd-escape`.

### A04-F20 — Job IDs deterministic time+counter; collide across restarts — P3
`jobs.rs:157-169`; `jobstore.rs:75-83` `ON CONFLICT DO UPDATE` → restart can overwrite persisted job. Fix: CSPRNG UUID (dup of A06-F12/A07-F04).

### A04-F21 — Engine availability probes run `--version` twice per binary — P3
`endpointconfig.rs:62-80,485-511,707-747,807-813`: `engine_available` may run `--version`, then `probe_engine` runs it again. 11 engines × 2 sequential 5s spawns → slower startup, double timeout exposure. Fix: probe once, return availability+version together.

## Notes — assessed without confirmed defect

- Error mapping consistent (`AppError` → problem+json, hides internals — `error.rs:75-122`).
- `pdfdoc.rs` centralizes page-tree ops; handlers reuse `page_select`/`pdfdoc` — no commit-logic duplication.
- No unsafe in server/engine; only CLI setsid.
- Deps checked have source references except the A04-F12 unused set.
- Read-only audit; no build/test run — dep findings from source-reference search.
