# Rust Execution Flow — `rust/` (stirling-server) and `rust/engine/` (stirling-engine)

Trace of the Rust backend as it exists in the working tree. Sources:
`rust/src/main.rs`, `rust/src/config.rs`, `rust/src/settings.rs`,
`rust/src/endpointconfig.rs`, `rust/src/auth.rs`, `rust/src/guard.rs`,
`rust/src/metrics.rs`, `rust/src/web.rs`, `rust/src/handlers/mod.rs`,
`rust/src/handlers/proxy.rs`, `rust/engine/README.md`.

## stirling-server (`rust/`, axum, default `:8081`)

`src/main.rs::main`:

1. `tracing_subscriber` init (`RUST_LOG`/env-filter, default `info`).
2. `config::Config::from_env()` — `HOST` (default `0.0.0.0`), `PORT`
   (default `8081`), plus a much larger surface than env alone:
   - `settings.rs` loads `configs/settings.yml` + `configs/custom_settings.yml`
     into a typed `Settings` model (merged; custom overrides base).
   - `cors_origin_patterns(&config)`: `system.corsAllowedOrigins` from
     settings, `STIRLING_PDF_TAURI_MODE` → Tauri origin allow-list, else
     allow-all — mirrors Java `WebMvcConfig.addCorsMappings`.
   - `max_upload_bytes` mirrors `spring.servlet.multipart.max-file-size`
     (~2 GB default).
   - `metrics_enabled` gates `/api/v1/info/*` operational endpoints.
3. `endpointconfig::init(&settings)` — endpoint/group enable-disable rules.
   `settings::init(&settings)` — shared settings snapshot.
   `metrics::init`, `auth::warn_if_secured_deployment()`.
4. `handlers::jobs::AppState::new(config::JavaBackend::from_env())`:
   - `JAVA_BACKEND_URL` → `reqwest::Client` (10 s connect timeout).
     **Opt-in now**: unset/`off`/unparseable → `None`; unmigrated
     `/api/**` then 404s (a startup warning is logged). Previously it
     defaulted to `http://localhost:8080`.
   - In-memory async-job table (tokio tasks; lost on restart).
5. `handlers::router(state)` + `CorsLayer` (Java header/method lists,
   `allow_credentials`, 1 h preflight) + `TraceLayer` + `web.rs` SPA
   static serving.
6. `axum::serve` on the bind addr **with graceful shutdown** (ctrl-c +
   SIGTERM via `shutdown_signal()`). No scheduler, no DB.

### Routing (`src/handlers/mod.rs`, ~112 explicit routes)

Explicit routes win; `.fallback(proxy::proxy)` forwards every other
`/api/**` request to Java verbatim (method, headers minus hop-by-hop,
streamed body; response headers minus hop-by-hop). Non-`/api/` paths are
handled by `web.rs` static/SPA serving.

Registered families:

- meta: `/health`, `/api/v1/info/version`, `/api/v1/ping`, `/api/v1/info/*`
  metrics (gated)
- general (~19): merge, rotate, remove/rearrange/split-pages,
  split-by-size-or-count, split-by-chapters/sections,
  multi-page-layout, booklet-imposition, overlay-pdfs, scale, crop,
  pdf-to-single-page, remove-image, edit-table-of-contents,
  extract-bookmarks, split-for-poster-print, job/file endpoints
- analysis (8): page-count, basic-info, page-dimensions,
  document-properties, form-fields, annotation-info, font-info,
  security-info
- filter (6): filter-contains-image/text, filter-file-size,
  filter-page-count/rotation/size
- form (9): fields, fields-with-coordinates, fill, add/delete/edit/
  modify-fields, extract-csv/xlsx
- misc (~24): decompress, update-metadata, flatten, compress,
  show-javascript, attachments (5), auto-rename, auto-rotate,
  add-image, add-stamp, add-page-numbers, add-comments, extract-images,
  create/flatten-portfolio, repair, replace-invert-pdf,
  unlock-pdf-forms, batch-process-attachments, add-attachments,
  remove-blanks (routed?), remove-cert-sign-adjacent ops
- security: sanitize, get-info-on-pdf, remove-cert-sign, add/remove-
  password, redact, add-watermark, prepare-pdf-session
- convert (11): img/pdf, pdf/img (Ghostscript), svg/pdf, pdf/svg-adjacent,
  office↔pdf, html↔pdf, vector↔pdf, pdf→word/presentation/xml/text
- layout/visual/automation/transform families per `mod.rs`
- config/settings/ui-data: endpoint-enabled(s), endpoints-availability,
  group-enabled, login-disclaimer, get-endpoints-status,
  update-enable-analytics, footer-info, home, licenses, ocr-pdf, pipeline
- mobile-scanner (6): session create/upload/files/download/validate
- jobs: `GET /api/v1/general/job/{id}[/result|/result/files]`,
  `DELETE /job/{id}`, `GET /api/v1/general/files/{id}[/metadata]`,
  `POST /api/v1/general/jobs/cleanup` — unknown job IDs fall back to Java
  via `proxy::forward`; `forward_multipart`/`forward_parts` re-POST parsed
  multipart to Java for parameter combos Rust can't serve natively.

### Request handling internals

- `#[stirling_macros::endpoint]` (`rust/macros/`): wraps a synchronous work
  fn `(AppState, query, Vec<Upload>, params) -> AppResult<T>` into an axum
  multipart handler; uploads buffered in memory via `src/upload.rs`
  (filename generation mirrors `GeneralUtils.generateFilename`, part cap
  mirrors `max-file-size`).
- `guard.rs` + `auth.rs`: authN/Z middleware mirroring Java's filter
  chain semantics for migrated routes (API keys, sessions where the
  settings model supports them).
- `endpointconfig.rs`: `endpoints.*.enabled` / group enable-disable
  enforcement; absent-engine group → Java's 403 semantics.
- PDF work runs on the `lopdf` object model via `src/pdfdoc.rs` plus
  domain modules: `page_select.rs` (Java `pageNumbers` syntax incl.
  `all`/`an+b`), `imgcomp.rs`, `imgextract.rs`, `forms.rs`,
  `attachments.rs`, `fontmetrics.rs`, `text.rs`, `convert.rs`
  (external-engine runner for sidecars: Ghostscript, soffice, …).
- Async jobs: every migrated endpoint accepts `?async=true`; JSON results
  stored inline, file results behind `/files/{id}`; cleanup deletes *all*
  finished jobs.
- Errors: `src/error.rs` `AppError` → JSON body; shape parity with Java's
  error responses is only approximate (see `rust/MIGRATION.md`).
- Tests: ~167 unit tests across `src/**` (`src/testkit.rs` builds PDFs
  with lopdf); smoke scripts `rust/tests/scripts/`; Java↔Rust harness
  `scripts/rust_pdf_parity.py` + `testing/parity.sh`.

## stirling-engine (`rust/engine/`, axum, default `:5001`)

Rust replacement for the Python `engine/` (FastAPI + pydantic-ai) AI
service — in progress, tracked in `rust/engine/README.md` and
`rust/MIGRATION.md` §"AI engine migration".

Implemented: HTTP scaffold (`/health`, `X-Engine-Auth` shared-secret
middleware, FastAPI-style `{"detail"}` errors, CORS), full serde port of
`stirling.contracts`, character-exact chunking/docparse packing,
OpenAI-wire embedder (Voyage/OpenAI/Ollama), sqlite-vec (vendored
amalgamation) + pgvector document stores, `DocumentService`, and routes
`/api/v1/health`, `/api/v1/documents`, `/api/v1/docparse`.

Not yet: model provider layer, agent runtime, orchestrator NDJSON
streaming, most agents/routers, config push/cache, PostHog, pytest port,
parity harness. Python `engine/` remains the production engine.

## Gap summary vs Java

| Concern | Java | Rust server | Rust engine |
|---|---|---|---|
| Entry point | `SPDFApplication.main` | `main.rs` axum | `main.rs` axum |
| Config | settings.yml + env + profiles | settings.yml + env (HOST/PORT/JAVA_BACKEND_URL/metrics) | STIRLING_* env (Python parity) |
| AuthN/Z | full filter chain + JPA | `auth.rs`/`guard.rs` middleware on Rust routes | X-Engine-Auth shared secret |
| Persistence | JPA/H2/Postgres | none (jobs in-memory) | sqlite-vec/pgvector (docs only) |
| Scheduler | @EnableScheduling jobs | none | none |
| Static/UI | ReactRoutingController + static | `web.rs` SPA serving | n/a |
| External sidecars | LibreOffice/GS/qpdf/Magick/Tesseract/veraPDF | `convert.rs` engine runner + forwards | LLM HTTP APIs only |
| Fallback | — | proxy → Java (opt-in via `JAVA_BACKEND_URL`) | — |
