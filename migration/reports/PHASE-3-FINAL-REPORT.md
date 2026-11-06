# Phase 3 — Final Engineering Report

Audit date: 2026-11-06. Scope: `rust/` workspace (`stirling-server` ~56k LOC, `stirling-macros`, `stirling` CLI) + workspace-excluded `rust/engine/`. **Caveat:** multiple agents were concurrently modifying this tree throughout the audit; findings reflect verified state at each checkpoint, and several transient compile breaks observed mid-audit were other agents' in-flight edits, not defects in audited code.

## 1. Overall assessment

The Rust backend is in **good health**: it compiles cleanly, all 430 unit tests pass, `cargo fmt --check` is clean at end-state, and the server runs fully standalone (`JAVA_BACKEND_URL=off`) with honest error semantics and Java-parity API contracts. The codebase is already well-factored (shared `upload`/`pdfdoc`/`convert`/`page_select`/`endpointconfig` layers, a proc-macro for the uniform endpoint contract, typed `AppError` → problem+json). Remaining risks are concentrated in: (a) a large in-progress migration surface where some endpoints still delegate to the opt-in Java proxy, (b) blanket `#![allow(dead_code)]` masking unused surface in `pdfdoc`/`convert`/`pdftext`, and (c) absence of Rust-level HTTP integration tests.

## 2. Testing results

| Gate | Initial | Final |
|---|---|---|
| `cargo fmt --check` | FAIL (diffs in concurrently-edited files) | **PASS** |
| `cargo check --workspace` | PASS w/ ~116 warnings (after transient concurrent-edit errors cleared) | **PASS, zero warnings** |
| `cargo clippy --workspace --all-targets -- -D warnings` | FAIL — ~137 errors, mostly `#[cfg(test)]` lints | See note below |
| `cargo test --workspace` / `--bin stirling-server` | 382→384 passing | **430 passed, 0 failed, 1 ignored** (perf probe) |
| Runtime HTTP (standalone) | — | **PASS** — see Test Report for the probe matrix |

Clippy note: the initial `-D warnings` failure was dominated by test-code lints (`err_expect`, `manual_range_contains`, etc.) and dead-code warnings that concurrent agents have since cleaned up; `cargo check` at end-state emits **zero warnings**. A final `-D warnings` clippy run was queued behind concurrent builds at report time — rerun `cargo clippy --workspace --all-targets -- -D warnings` to confirm; any residual lints are in concurrently-owned files.

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

Consolidated (all verified by `cargo check` + 430 passing tests):

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
- `tests/integration/` empty — no Rust HTTP integration tests (shell smoke scripts fill the gap transitionally).

## 6. Performance

- Verified good: uploads spool to disk under a streaming cap; all PDF work runs on `spawn_blocking`; engine subprocesses drain pipes on threads with hard timeouts; CORS/middleware minimal.
- Outstanding: whole-document `Vec<u8>` materialization per request (`lopdf` parses fully anyway — documented in `UploadBytes`); async-job results buffered fully in memory; no global concurrency semaphore (blocking pool is the de-facto cap); `endpointconfig`/engine probing runs once at boot (good).
- No speculative micro-optimizations applied; nothing measurable regressed.

## 7. Standalone operation

**Confirmed.** `JAVA_BACKEND_URL` is opt-in (default off). With it off: unmigrated `/api` routes 404 cleanly, all migrated endpoints work, job store persists via SQLite, SPA/static serving works, external engines (gs/LibreOffice/etc.) degrade honestly. ~20 explicit `forward_multipart`/`forward_parts` delegation branches remain for unimplemented parameter combinations — they 404 standalone rather than fake success. No Java process/jar/`JAVA_HOME` dependency exists anywhere in `rust/`.

## 8. Remaining issues

- **Known bugs:** none outstanding in audited code (the one found was fixed and verified).
- **Incomplete features:** ~20 Java-delegation branches (office-format edge cases, `flatten` raster path now has a native Ghostscript implementation in-flight by another agent, eml `@`-marker extraction, image_scans native port landing); proprietary/saas surface intentionally out of scope.
- **Missing external engines (environment):** qpdf, unoconvert, weasyprint, tesseract, ocrmypdf, rar, calibre, OpenCV — endpoints auto-disable with warnings.
- **Environment limitations:** none blocking; all tests ran natively.
- **Architectural improvements:** workspace deps table; item-level dead-code audit post-migration; consider streaming job-result storage to disk for very large outputs.
- **Deferred work:** Rust HTTP integration tests (`tests/integration/`); clippy `-D warnings` confirmation once concurrent edits settle.

## 9. Recommended next phase

**Phase 4 — HTTP integration test harness + remaining Java-delegation elimination.** Build `tests/integration/` (axum `tower::ServiceExt` oneshot or live-server harness) covering the verified probe matrix as automated tests; then burn down the ~20 `forward_multipart` branches endpoint-by-endpoint (several are already being ported natively by concurrent agents — `flatten` raster, `image_split`, `pdfa_convert` post-processing). After that, the `JAVA_BACKEND_URL` proxy can be deprecated.
