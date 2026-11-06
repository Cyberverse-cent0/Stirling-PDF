# Phase 3 — Dead Code & Implementation Completeness

## Removed during the audit window

By this audit's own hand:
- `convert.rs::engine_tests` duplicate test + merged unique assertions.
- 4× `first_upload`, 5× `simple_name` local copies → `upload.rs`.
- `int_param`/`float_param` duplicated parse bodies → `upload::param_parse` delegations.

By concurrent migration agents (verified present at end-state):
- **`src/handlers/proxy.rs` deleted** — the entire Java fallback proxy.
- `config::JavaBackend`, `JAVA_BACKEND_URL` env handling — removed from `config.rs`.
- All `forward_multipart`/`forward_parts`/`forward_all`/`path_and_query` helper plumbing in `convert.rs`, `misc.rs`, `visual.rs`, `automation.rs`, `security.rs` — removed; unported modes now return `AppError::BadRequest`.
- `rust/tests/scripts/*.sh` — all shell smoke scripts deleted; `stirling test runtime|general|security|coverage` are now native Rust (`cli/src/commands/smoke.rs` + `test.rs::coverage`).
- Dead test helpers `gif_1x1`/`bmp_1x1` in `convert.rs` tests — removed by agents.

## Findings — retained / requires investigation

| Item | Location | Classification |
|---|---|---|
| `#![allow(dead_code)]` module-level | `src/pdfdoc.rs:7`, `src/convert.rs`, `src/pdftext/mod.rs` | **Retained deliberately** — Java-parity helper surface for incremental migration; masks genuinely dead fns. Item-level audit recommended post-migration (P3). |
| `#[allow(dead_code)]` scattered | `error.rs`, `pipeline.rs`, `testkit.rs`, `pdftext/{glyph_stitcher,line_merger,page_stitcher}.rs`, `imgcomp.rs`, `text.rs`, `pdfdoc.rs`, `handlers/layout.rs`, `image_split.rs` (`SplitResult::name` — script-side name, only tests read it) | Mostly justified (test-only or not-yet-routed surface). `SplitResult::name` allow is honest. Retained. |
| `TODO(jpdfium)` markers | `pdftext/form_values.rs:19`, `pdftext/page_images.rs:5` | Incomplete-port notes; lack `TODO(#issue)` numbers per repo convention — flag for cleanup. |
| `AppState::new/persistent(_backend: Option<()>)` | `handlers/jobs.rs:44,56` | Compat shim for Java-proxy-era call sites; now that proxy.rs is gone, `Option<()>` could simplify to no arg — **safe to consolidate** once all callers settle (P3). |
| `EngineOutput.stdout` field | `convert.rs` | Drained but only `stderr` surfaces in errors; `stdout` retained for diagnostics. |
| `reqwest` `blocking` feature | `rust/Cargo.toml` | **Requires investigation** — no `reqwest::blocking` usage found in `src/`; likely trimmable (P3). |
| No `[workspace.dependencies]` | `rust/Cargo.toml` | All members pin deps independently; versions consistent today, drift risk (P3). |
| `tests/integration/` empty | `rust/tests/integration/` | **Gap** — no Rust HTTP integration tests in-crate; coverage is 430+ unit tests plus the native `stirling test` smoke harness (live-server checks). |
| `pdftext` module `#[allow(dead_code)]` | `src/pdftext/mod.rs` | Large ported surface; only routed subsets are live — intentional during migration. |

## Incomplete implementations (honest, verified)

These return explicit errors rather than proxying or faking success:

- `misc::flatten` without `flattenOnlyForms` (raster flatten) — **now has a native Ghostscript implementation** (`flatten_raster_work`, added during audit window; routed status per `REMAINING_WORK.md`).
- `security::add_watermark`/`add_stamp` text & rasterising modes — 400 "not implemented".
- `misc::compress_pdf?lineArt=true` — 400 (ImageMagick service unported).
- `misc::replace_invert_pdf` non-`COLOR_SPACE_CONVERSION` modes — 400.
- `misc::extract_images` with undecodable image types — 400.
- `convert` `useStirlingOfficeConvert`/`system.stirlingOfficeConversion` branches — 400.
- `automation::auto_rotate_pdf?detectionMode=osd` — 400 (no Tesseract OSD path).
- `eml.rs` `@`-marker extraction — noted in-code as unported.
- Proprietary/saas surface — intentionally out of scope.

## Java remnants

**None in executable code.** Remaining mentions are comments/docstrings explaining parity decisions and historical context (`REMAINING_WORK.md`, migration reports). `AppState`'s `Option<()>` parameter is the only compile-compat vestige.
