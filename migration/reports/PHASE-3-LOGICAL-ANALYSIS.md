# Phase 3 — Logical Analysis

Method: source inspection of PDF paths, HTTP handlers, async boundaries, error paths, and file handling, plus live HTTP probing of the standalone server (early probes ran with `JAVA_BACKEND_URL=off`; the proxy was later deleted entirely — end-state probes ran with no Java configuration at all).

## A. PDF processing logic

- `pdfdoc::load`/`save`/`subset`/`import_pages`/`rotation`/`set_rotation`/`page_dimensions`/`media_box`/`set_page_box`/`clear_annots`/`wrap_page_content`/`scale_annots`/`clip_annots` — coherent lopdf wrapper; page indexing is consistently **0-based internally** (`page_ids` order, `subset(&[u32])` indices) which matches `page_select` (0-based after parse).
- **DEFECT FIXED (P1): `lopdf::Document::extract_text` is 1-based.** `get_pages()` keys are `(i+1) as u32`. `handlers/convert.rs::pdf_to_office_work` passed `(0..pages)` — page 0 never exists → `PageNumberNotFound(0)` → every `POST /api/v1/convert/pdf/text?outputFormat=txt` returned HTTP 500. Verified live (500 before, 200 `text/plain` after fix to `(1..=pages)`). Other `extract_text*` callers (`security.rs:267,1305`) correctly use `get_pages()` keys.
- Empty-document handling: `pdfdoc::to_single_page_*` tests cover empty docs; `remove_pages` rejects removing all pages (verified: `pageNumbers=all` → 400 with Java's message).
- `page_select` mirrors `GeneralUtils` semantics incl. `an+b`, dedup, max-size guard; validation split (`require_valid` vs `require_well_formed`) matches Java's filter-vs-transform distinction.
- Temp files: uploads spool to `NamedTempFile` under the multipart cap; handler workdirs use `tempfile::tempdir()` (auto-cleanup). `BlankRasterizer` keeps its tempdir alive for the struct's lifetime — correct.
- Resource cleanup: `UploadBytes` deletes the spool on last-clone drop; `jobs::submit` buffers whole result bodies into memory (`axum::body::to_bytes(.., usize::MAX)`) — a large-result memory spike, noted under Performance.

## B. HTTP API logic

- Router: ~110 explicit routes in `handlers/mod.rs`; the fallback 404s unmigrated `/api/**` paths and serves the SPA/static surface otherwise — verified.
- Multipart: single `receive_multipart`/`receive_files_multipart` in `upload.rs` — no per-handler re-implementation found.
- Error contract: `AppError` → `application/problem+json` with `type` URIs matching Java `ErrorTypes`; internal details hidden (verified: invalid PDF → 400 `invalid-argument`; missing `fileInput` → 400; bad angle → 400; out-of-range page → 400; wrong method → 405).
- Honest failure: missing engines produce 4xx/5xx with real errors, not fake success (verified `pdf-to-text` pre-fix 500 came from a real lopdf error, now fixed; engine-dependent groups disabled at boot with warnings).
- Unported modes: at audit end-state every former Java-delegation branch returns an explicit `AppError::BadRequest` naming the unimplemented mode (verified live: `flatten` raster mode, text watermark → 400 with clear detail). No silent fallbacks remain — the Java proxy itself was deleted during the audit window.
- Async jobs: `?async=true` verified end-to-end — submit → `jobId` → `GET job/{id}` complete → `GET job/{id}/result` returns stored PDF bytes (200, `application/pdf`, correct `Content-Disposition` filename `basic_rotated.pdf`).
- File download: `GET /api/v1/general/files/{fileId}` serves stored bytes (verified 200).
- Disabled endpoints: `guard::api_guard` → 403 + `endpointconfig` enablement (unit-tested).

## C. Async/concurrency

- `#[endpoint]` macro wraps **all** work fns in `tokio::task::spawn_blocking` (sync and async-job paths) — blocking lopdf/image/subprocess work never occupies async workers. Hand-written `dispatch` in `misc.rs` duplicates this contract for handlers needing pre-dispatch param inspection.
- `AppState` uses `std::sync::Mutex` (not tokio) for `jobs`/`files` maps — held only for map ops, never across `.await`; no deadlock path found. `jobstore` uses `Mutex<Connection>` similarly.
- `tokio::spawn` per async job; no cancellation token — `DELETE job/{id}` removes the record but cannot kill the running `spawn_blocking` work (Java parity: its executor also can't interrupt mid-PDFBox). Documented limitation, not a defect vs. the contract.
- Graceful shutdown: SIGINT/SIGTERM → `axum::serve` drain (`with_graceful_shutdown`). In-flight `spawn_blocking` PDF work is not awaited — acceptable, matches Java's shutdown semantics loosely.
- Unbounded concurrency: no semaphore on endpoint work; `spawn_blocking` pool (default 512 threads) is the effective cap. Large uploads are capped per-part (`max_upload_bytes`, default 2 GB — same as Java).

## D. Error handling

- `unwrap()`/`expect()` in non-test code: ~850 total occurrences but overwhelmingly inside `#[cfg(test)]` modules and `Mutex::lock().unwrap()` (poisoning). Non-test, non-mutex unwraps are guarded (`Option::is_empty` checks precede `chars().next().unwrap()`; `Vec::last().unwrap()` after non-empty checks). `zip.start_file/write_all/finish().expect()` in xlsx generation writes to an in-memory `Cursor` — infallible in practice.
- `AppError::Internal`/`Pdf` log the real error and return generic detail — no leakage (unit-tested).
- `param_f64` silently falls back on unparsable input (Spring relaxed binding parity) while `param_parse` 400s — **intentional contract difference**, now shared via `upload::param_parse`.
- `is_async` requires exact `"true"` (Java `request.getParameter("async")` semantics) vs `param_bool`'s wider spellings — intentional.

## E. Security/file handling

- Upload cap enforced **while streaming** (`spool_part`), aborting oversized parts — no pre-buffering DoS.
- `web.rs::resolve` rejects path traversal (unit-tested); `CleanUrlInterceptor` param filtering ported with tests.
- Subprocess execution is argv-array only (`EngineSpec`, `Command::new`), never shell interpolation — verified in `convert.rs`, `BlankRasterizer`, `image_scans` script runner.
- `forward_request_headers` strips hop-by-hop + `host`; `content-length`/`content-type` correctly removed on multipart rebuild.
- Filename handling: `simple_name` basename extraction, `invalid_file_name` rejects `..`/leading `/` for stamp/watermark (visual.rs).
- Auth: OSS-parity open default; `RUST_API_KEY` guard; startup warning when proprietary/saas markers present. No secrets in logs observed (split_photos stderr deliberately kept out of API responses).
- Zip-slip: `pipeline.rs` unzip has `MAX_UNZIP_DEPTH`; attachment extraction caps at 50 MB.

## F. Findings not fixed (see other reports)

- `reqwest` `blocking` feature likely unused.
- `#![allow(dead_code)]` on `convert.rs`, `pdfdoc.rs`, `pdftext` masks real unused surface.
- `jobs::submit` buffers entire results in memory.
- No Rust-level HTTP integration tests (`tests/integration/` empty); coverage is unit + the native `stirling test` smoke harness.
- `AppState::new/persistent` keep a dead `Option<()>` parameter as a Java-proxy-era compat shim — simplifiable once callers settle.
