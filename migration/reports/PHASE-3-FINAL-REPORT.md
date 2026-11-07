# Phase 3 — Final Engineering Report

Audit date: 2026-11-06. Scope: `rust/` workspace (`stirling-server` ~56k LOC, `stirling-macros`, `stirling` CLI) + workspace-excluded `rust/engine/`. **Caveat:** multiple agents were concurrently modifying this tree throughout the audit; findings reflect verified state at each checkpoint, and several transient compile breaks observed mid-audit were other agents' in-flight edits, not defects in audited code.

## 1. Overall assessment

The Rust backend is in **good health**: it compiles cleanly, all **461 workspace tests pass**, `cargo fmt --check` is clean, and `cargo clippy --workspace --all-targets -- -D warnings` passes with zero errors. Unported parameter modes now return explicit 400s instead of delegating. The codebase is already well-factored (shared `upload`/`pdfdoc`/`convert`/`page_select`/`endpointconfig` layers, a proc-macro for the uniform endpoint contract, typed `AppError` → problem+json). Remaining risks are concentrated in: (a) blanket `#![allow(dead_code)]` masking unused surface in `pdfdoc`/`convert`/`pdftext`, and (b) absence of Rust-level HTTP integration tests.

## 2. Testing results

| Gate | Initial | Final |
|---|---|---|
| `cargo fmt --check` | FAIL (diffs in concurrently-edited files) | **PASS** |
| `cargo check --workspace` | PASS w/ ~116 warnings (after transient concurrent-edit errors cleared) | **PASS, zero warnings** |
| `cargo clippy --workspace --all-targets -- -D warnings` | FAIL — ~137 errors, mostly `#[cfg(test)]` lints | **PASS — 0 errors** |
| `cargo test --workspace` / `--bin stirling-server` | 382→384 passing | **461 passed, 0 failed, 2 ignored** (458 server + 3 CLI; ignores: perf probe + doc-test) |
| Runtime HTTP (standalone) | — | **PASS** — see Test Report for the probe matrix |

Clippy note: the initial `-D warnings` failure was dominated by test-code lints and dead-code warnings that concurrent agents plus this audit's consolidation resolved; the final run on the settled tree is fully clean.

## 3. Logical defects

| Severity | Defect | Status |
|---|---|---|
| **P1** | `/api/v1/convert/pdf/text?outputFormat=txt` always HTTP 500 — `pdf_to_office_work` passed 0-based indices to `lopdf::extract_text`, which expects the 1-based `get_pages()` keys. Verified live (500 → 200 `text/plain` after fix). | **FIXED** (`handlers/convert.rs`) |
| P2 | Build break: `handlers/image_scans.rs` imported `crate::image_split` but `mod image_split;` was never declared (E0432); `SplitResult` lacked `Debug` for its own test (E0277). | **FIXED** (`main.rs`, `image_split.rs`) — unblocked workspace for all agents |
| P2 | Duplicate test `engine_spec_builder_collects_args` in `convert.rs` test modules. | **FIXED** |
| P3 | `jobs::submit` buffers entire async-job results in memory (`to_bytes(.., usize::MAX)`). | Documented; Java parity acceptable for now |
| P3 | Async-job cancellation removes the record but cannot interrupt running `spawn_blocking` work. | Documented limitation (matches Java executor semantics) |

No P0 (data-corruption/security/crash) defects found in audited paths. Page indexing, multipart caps, path-traversal guards, subprocess argv handling, and error-detail hiding all verified sound.

## 4. Duplicate-code elimination

Consolidated (verified by `cargo check` + 461 passing tests):

- `first_upload`: 4 identical copies → `upload::first_upload`.
- `simple_name`: 5 identical copies → `upload::simple_name` (incl. repointing `pipeline.rs` off `automation::simple_name`).
- `int_param`/`float_param` strict-parse bodies: added generic `upload::param_parse<T>`; `layout.rs` and `visual.rs` helpers now delegate (call sites and i32/i64 Java-binding distinction preserved).
- `convert.rs` duplicate test module content merged.

Intentionally retained: `load_pdf` variants (different return contracts), `transform::float_param` (`Option` semantics), `bool_param`/`parse_bool`/`param_bool` family (distinct Spring-binding contracts), `misc::dispatch` vs `#[endpoint]` (pre-dispatch inspection need), `tabula` vs `pdftext/ruled_tables` (different algorithms), pdftext internal helpers (port fidelity).

## 5. Dead code & dependencies

- Removed: the duplicate test and 9 redundant helper copies above.
- `#![allow(dead_code)]` on `pdfdoc.rs`, `convert.rs`, `pdftext` — retained deliberately (incremental migration surface) but flagged: it hides genuinely dead functions; recommend item-level audit post-migration.
- `TODO(jpdfium)` markers in `pdftext/form_values.rs`, `pdftext/page_images.rs` lack issue numbers (repo convention requires `TODO(#issue)`).
- `reqwest` `blocking` feature appears unused — candidate trim (P3, verify first).
- No `[workspace.dependencies]` table — three members pin deps independently; versions currently consistent.
- `tests/integration/` (crate-level dir) still empty, but `src/apitests.rs` now provides 11 in-process HTTP tests against the real router — the gap is substantially closed; remaining value would be live-socket tests (the `stirling test` smoke harness covers those).

## 6. Performance

- Verified good: uploads spool to disk under a streaming cap; all PDF work runs on `spawn_blocking`; engine subprocesses drain pipes on threads with hard timeouts; CORS/middleware minimal.
- Outstanding: whole-document `Vec<u8>` materialization per request (`lopdf` parses fully anyway — documented in `UploadBytes`); async-job results buffered fully in memory; no global concurrency semaphore (blocking pool is the de-facto cap); `endpointconfig`/engine probing runs once at boot (good).
- No speculative micro-optimizations applied; nothing measurable regressed.

## 7. Standalone operation

**Confirmed — total Java independence.** During the audit window the migration agents deleted the entire proxy layer: `proxy.rs`, `config::JavaBackend`, `JAVA_BACKEND_URL`, and every `forward_*` call site are gone (`grep` returns zero hits). Unmigrated `/api` routes 404; unported parameter modes return explicit `AppError::BadRequest`s naming what's missing (verified live). All migrated endpoints, the async job framework (SQLite-persisted), SPA serving, and external non-Java engines work with zero Java configuration. The only vestige is `AppState::new/persistent`'s ignored `Option<()>` parameter kept for compile compatibility.

## 8. Remaining issues

- **Known bugs:** none outstanding in audited code (the one found was fixed and verified).
- **Incomplete features:** unported parameter modes now fail honestly with 400s (text stamps/watermarks, flatten raster — native port landed in-flight, compress `lineArt`, replace-invert non-CMYK modes, extract-images codec gaps, office-convert branches, auto-rotate `osd`, eml `@`-markers); proprietary/saas surface intentionally out of scope.
- **Missing external engines (environment):** qpdf, unoconvert, weasyprint, tesseract, ocrmypdf, rar, calibre, OpenCV — endpoints auto-disable with warnings.
- **Environment limitations:** none blocking; all tests ran natively.
- **Architectural improvements:** workspace deps table; item-level dead-code audit post-migration; consider streaming job-result storage to disk for very large outputs.
- **Deferred work:** unported parameter modes (see §8); live-socket integration tests beyond the smoke harness.

## 9. Recommended next phase

**Phase 4 — HTTP integration test harness + unported-mode completion.** Build `tests/integration/` (axum `tower::ServiceExt` oneshot or live-server harness — the native `stirling test` smoke harness in `cli/src/commands/smoke.rs` is a working template) covering the verified probe matrix as automated tests; then burn down the remaining unported modes (text stamp/watermark font embedding, compress line-art, replace-invert raster modes, extract-images codec coverage, eml `@`-markers). The Java decommission itself is already done — `proxy.rs` is deleted.
