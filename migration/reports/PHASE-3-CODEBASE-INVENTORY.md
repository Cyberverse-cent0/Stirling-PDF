# Phase 3 — Rust Codebase Inventory

Audited 2026-11-06. Scope: `rust/` workspace (`stirling-server`, `stirling-macros`, `stirling` CLI) plus the workspace-excluded `rust/engine/` crate. Note: the tree was under concurrent modification by other agents during the audit; line counts and findings reflect the state at audit time (~55.8k LOC in `rust/src`).

## Workspace layout

| Crate | Path | Members | Role |
|---|---|---|---|
| `stirling-server` | `rust/` (root package, bin) | — | Axum HTTP backend + all PDF logic |
| `stirling-macros` | `rust/macros` | proc-macro | `#[endpoint]` attribute: multipart parse + `?async=true` job dispatch + `spawn_blocking` |
| `stirling` | `rust/cli` | bin | Dev/ops CLI (`install`, `test`, `smoke`, `migration`, …) |
| `stirling-engine` | `rust/engine` | lib+bin | Rust AI engine (replaces `engine/` Python); **excluded from workspace**, own lockfile/build |

`frontend/src-tauri/` also contains Cargo projects (desktop app), out of backend scope.

## `stirling-server` module map

| Module | LOC | Responsibility | State |
|---|---|---|---|
| `main.rs` | ~140 | Entry point: config, CORS (Java `WebMvcConfig` parity), middleware stack, graceful shutdown | Complete |
| `config.rs` | 262 | `Config::from_env`, CORS origin patterns (`allowedOriginPatterns` semantics), opt-in `JavaBackend` | Complete |
| `settings.rs` | 672 | `settings.yml`/`custom_settings.yml` merge, engine program lookup, base dir | Complete |
| `endpointconfig.rs` | 943 | Endpoint enable/disable: `toRemove`/`groupsToRemove`, `ENDPOINTS_*` env, dependency probing (gs, tesseract, LibreOffice…) | Complete |
| `upload.rs` | ~370 | Multipart spool-to-tempfile (`UploadBytes`), `param`/`param_f64`/`param_bool`/`param_parse`, `first_upload`, `simple_name`, `output_name`, `remove_extension` | Complete — **consolidation target executed** |
| `error.rs` | 168 | `AppError` → RFC7807 `application/problem+json` matching Java `GlobalExceptionHandler` | Complete |
| `pdfdoc.rs` | 3283 | lopdf wrapper: load/save/subset/import_pages/rotation/boxes/annots/outline/encrypt/info/forms helpers | Complete (`#![allow(dead_code)]` masks some unused surface) |
| `page_select.rs` | 490 | `parse_page_list`/`require_valid_page_selection` — port of `GeneralUtils` page syntax (`1,3,5-9`, `all`, `an+b`) | Complete, well-tested |
| `convert.rs` | 373 | `EngineSpec`/`run`/`require_success`/`find_executable` — argv-only subprocess runner, timeout, pipe draining | Complete; **had duplicate test module — consolidated** |
| `jobstore.rs` | 300 | SQLite (rusqlite, bundled) write-through job/file store at `configs/stirling-jobs.db` | Complete |
| `text.rs` | 762 | Positioned text extraction for auto-rename/auto-rotate | Complete |
| `tabula.rs` | 2240 | Lattice (ruling-based) table extraction port of Tabula | Complete |
| `imgcomp.rs` | 995 | Image recompression inside PDFs | Complete |
| `imgextract.rs` | 788 | Image extraction from PDF | Complete |
| `forms.rs` | 1010 | AcroForm field model helpers | Complete |
| `attachments.rs` | 864 | Embedded-file (portfolio/attachment) operations | Complete |
| `pipeline_table.rs` | 778 | Generated endpoint table for pipeline `handleData` | Generated, complete |
| `fontmetrics.rs` | — | Font ascent/descent metrics (PDFBox parity) | Complete |
| `auth.rs` | 185 | OSS-parity auth gate: open by default, `RUST_API_KEY` opt-in, public-path exemptions | Complete |
| `guard.rs` | 75 | `api_guard`: endpoint-enabled 403 + `Cache-Control` on `/api/` | Complete |
| `metrics.rs` | 508 | `/api/v1/info/*` operational endpoints + request tracking | Complete |
| `web.rs` | 835 | SPA static serving, `CleanUrlInterceptor` parity, React routing, path-traversal guards | Complete (under active edit) |
| `image_split.rs` | ~500 | Native port of `split_photos.py` (OpenCV) — added during audit by another agent | New, in-flight |
| `fonts.rs` | — | New module added during audit | In-flight |
| `pdftext/` | ~10k | jpdfium text-model + markdown pipeline port (`common_converter`, `heading`, `page_rules`, `ruled_tables`, `redact`, `gfm_table`, …) | Complete for routed endpoints; `mod pdftext` is `#[allow(dead_code)]` (not all surface routed) |
| `testkit.rs` | — | `#[cfg(test)]` PDF builder + fixture writer | Test-only |

## `src/handlers/` — API layer

22 modules, ~54 `#[endpoint]` handlers + hand-written routes in `mod.rs` (~110 explicit routes + `/api/**` fallback proxy). Largest files: `form.rs` (5383), `convert.rs` (5206), `misc.rs` (3615), `security.rs` (2566), `automation.rs` (1803), `visual.rs` (1565), `platform.rs` (1397), `eml.rs` (1264), `pipeline.rs` (1079), `split.rs` (962), `general.rs` (931), `layout.rs` (893), `transform.rs` (644), `filter.rs` (640), `passwords.rs` (611), `scanner.rs` (608), `ocr.rs` (416), `analysis.rs` (410), `jobs.rs` (796), `proxy.rs` (365), `merge.rs`, `image_scans.rs` (405).

Patterns observed:
- `#[stirling_macros::endpoint]` on synchronous work fns → uniform multipart parse, `?async=true` → `jobs::submit`, inline path via `spawn_blocking`. Hand-written handlers needing pre-dispatch inspection use `misc::dispatch` (same contract).
- Response helpers centralized: `merge::pdf_response`/`file_response`, `split::zip_response` (octet-stream, Java parity).
- Java delegation is explicit and opt-in: `proxy::forward_multipart`/`forward_parts` for parameter combinations with no native implementation; returns 404 when `JAVA_BACKEND_URL` is unset (the default).

## Dependencies (stirling-server)

axum 0.8 (multipart), tokio, tower-http (cors/trace), serde/serde_json/serde_yml, lopdf 0.47, image 0.25, zip 2, reqwest 0.12 (rustls, stream, multipart, **blocking** feature — check usage), tempfile, chrono/chrono-tz/iana-time-zone, rand, comrak, qrcode, rqrr, mail-parser, unicode-normalization, ammonia, base64, tracing, anyhow/thiserror, stirling-macros, http-body-util, futures-util, tokio-util, bytes. `rusqlite` was added to `[dependencies]` during the audit window (jobstore).

Notable: `reqwest` `blocking` feature enabled — no `reqwest::blocking` usage found in `src/`; candidate for feature trim (verify before removing).

## Testing infrastructure

- 430 unit tests inside `src/**` `#[cfg(test)]` modules (all passing at audit time).
- `rust/tests/fixtures/`: `basic.pdf`, `images.pdf`, `sanitize-target.pdf`.
- `rust/tests/scripts/`: shell smoke scripts (`smoke.sh`, `test-general.sh`, `test-security.sh`, `test-unit.sh`, `test-engine.sh`, `coverage-report.sh`) — invoked via `stirling test` CLI and `task`.
- `rust/tests/integration/`: **empty** — no Rust HTTP integration tests exist.
- `rust/examples/`: `pdfdiff.rs`, `pdf_fingerprint.rs`, `qr_png.rs` dev tools.

## Implementation state summary

- Core PDF ops (rotate/remove/rearrange/merge/split/scale/crop/layout/metadata/attachments/forms/security redact/sanitize): complete, unit-tested, runtime-verified.
- Engine-backed conversions (LibreOffice, Ghostscript, Chromium, qpdf, tesseract, calibre, unoconvert, weasyprint): implemented via `EngineSpec`; endpoints honestly disable/error when the engine binary is absent (`endpointconfig` probes at boot — verified in server log).
- Java fallback: opt-in proxy only (`JAVA_BACKEND_URL`), default off.
