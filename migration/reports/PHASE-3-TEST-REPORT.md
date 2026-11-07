# Phase 3 — Test Report

## Baseline gate results (audit start)

| Command | Result |
|---|---|
| `cargo fmt --check` | **FAILED** — formatting diffs present (in files under concurrent edit) |
| `cargo check --workspace` | Initially failed on transient concurrent edits (`tabula.rs`, `image_scans.rs`); on re-run **passed** with ~116 warnings (mostly dead-code) |
| `cargo clippy --workspace --all-targets -- -D warnings` | **FAILED** — ~137 errors, concentrated in `#[cfg(test)]` code (`err_expect`, `manual_range_contains`, etc.) |
| `cargo test --workspace` | **PASSED** — 382 tests (server bin) + cli/macros suites, 0 failures |

## Final gate results (audit end, settled tree)

Concurrent agents landed substantial work during the audit (new `image_split.rs`, `fonts.rs`, `pdftext::redact`, auto-redact routes, CLI test commands, native smoke harness replacing shell scripts, Java-proxy deletion). Final verified state:

| Command | Result |
|---|---|
| `cargo fmt --check` | **PASSED** — clean |
| `cargo check --workspace` | **PASSED** — zero errors, zero warnings |
| `cargo clippy --workspace --all-targets -- -D warnings` | **PASSED — 0 errors** (the ~137 baseline failures were resolved by concurrent work + this audit's consolidation) |
| `cargo test --workspace` | **PASSED — 461 tests, 0 failed, 2 ignored** at gate time; +11 in-crate HTTP tests (`src/apitests.rs`) added afterwards — all pass |

## In-crate HTTP integration tests (`src/apitests.rs`, 11 tests)

Added post-gate: the real `handlers::router(AppState::new())` exercised in-process via `tower::ServiceExt::oneshot` (the `metrics.rs` test pattern), with hand-built multipart bodies and `tests/fixtures/` PDFs. Covers: health/version, rotate→valid PDF (page count verified via lopdf), merge→2-page PDF, `pdf-to-text` with real extracted text ("Hello Stirling" — exercises the P1 off-by-one fix end-to-end), invalid angle/missing file/non-PDF → 400 problem+json, unsupported watermark mode → honest 400, unknown route/job → 404 problem+json, and the full async job lifecycle (submit → poll → result file download).

## Fixes made to keep the build green

1. `src/main.rs`: added missing `mod image_split;` — `handlers/image_scans.rs` imported `crate::image_split` but the module was never declared (E0432). The 20 KB `src/image_split.rs` existed untracked; wiring it restored compilation.
2. `src/image_split.rs`: added `#[derive(Debug)]` to `SplitResult` — the module's own test used `{results:?}` (E0277).

## Unit test coverage observed (461 tests)

Strong coverage in: `upload` (param semantics), `page_select` (selection syntax), `config` (CORS patterns), `error` (problem+json mapping), `convert` (EngineSpec, find_executable, run/timeout — real subprocess tests), `jobstore` (SQLite round-trip), `metrics`, `web` (traversal, clean-url), `attachments`, `auth`, `settings`, `endpointconfig`, `imgcomp`/`imgextract`, `tabula` (incl. adversarial ruling cap), `pdfdoc` (to-single-page edge cases), `pdftext::*` (redact, gfm_table, column_ranges, word_geometry, model), `handlers::*` (automation auto-split, image_scans split_photos, passwords encrypted round-trip, visual QR/stamp, layout params).

## Gaps

- `rust/tests/integration/` is **empty** — no Rust HTTP-level integration tests. HTTP behaviour is covered only by `tests/scripts/*.sh` smoke scripts (transitionally delegated by `stirling test general|security|runtime|coverage`) and this audit's manual curl probing.
- Tests that only assert 200: none found that are meaningless; handler tests assert content/structure (e.g. `auto_split_*`, `qr_stamp_produces_decodable_pdf` decodes the QR).
- Permanently ignored: `tabula::dense_grid_perf` (`#[ignore]` perf probe — justified, runs on demand).
- Java-dependent tests: none found; all tests run without Java.

## Runtime HTTP verification (standalone)

Two server boots were exercised. Early probes ran on the pre-removal binary with `JAVA_BACKEND_URL=off`; **end-state probes ran on the final binary with no Java configuration at all** (the proxy was deleted mid-audit). Verified:

| Probe | Result |
|---|---|
| `GET /health`, `GET /api/v1/info/version` | 200 `{"status":"ok"}`, `{"runtime":"rust"}` |
| `POST /general/rotate-pdf` valid | 200 `application/pdf`, valid 1-page PDF out |
| rotate missing `fileInput` / bad angle / invalid PDF / empty file | 400 problem+json each, correct `type` URIs |
| `POST /general/merge-pdfs` (2 files) | 200, merged PDF = 2 pages (`pdfinfo`) |
| `POST /general/split-pages` | 200 octet-stream, valid ZIP |
| `POST /general/remove-pages?pageNumbers=all` | 400 (Java's exact message) |
| `POST /general/remove-pages?pageNumbers=999` | 400 range error |
| `POST …?async=true` → job status → job result → file download | Full lifecycle verified; stored file served with correct name/type |
| `POST /security/sanitize-pdf` | 200 PDF |
| `POST /misc/compress-pdf`, `update-metadata`, `remove-blanks` | 200 (remove-blanks → ZIP, Java parity) |
| `POST /convert/pdf/text?outputFormat=txt` | **was 500 (off-by-one defect) → now 200 `text/plain` after fix** |
| `POST /convert/pdf/markdown` | 200 `text/markdown` |
| Unmigrated `/api/v1/general/nonexistent-thing` | 404 problem+json (no Java backend) |
| `POST /misc/flatten` raster mode, `POST /security/add-watermark` text mode (end-state) | 400 problem+json — explicit "not implemented" detail, no proxy |
| Unknown job id (end-state) | 404 problem+json |
| GET on POST-only route | 405 |
| 5 concurrent rotate requests | all 200 |

External-engine absence handled honestly: boot log warns and disables dependent groups (qpdf, unoconvert, weasyprint, tesseract, ocrmypdf, rar, calibre, OpenCV).
