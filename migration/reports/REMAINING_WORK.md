# Remaining Work — handoff task list for coding agents

Ordered by dependency.

> **Scope update (2026-11-06, third pass):** Java is no longer a
> fallback target. The migrated-endpoint Java controllers are deleted
> and stay deleted; acceptance is Rust unit tests, real conversion
> fixtures, runtime smoke tests, and output validation — not Java
> parity. Do not implement proxy-to-Java behavior when an external
> engine is missing: return a clear missing-dependency error instead.
> The ~74 proprietary/SaaS services remain out of scope.
>
> **Next phase — four agents**, shared-file rule: only one agent edits
> `rust/src/handlers/convert.rs` at a time (others add sibling modules
> or wait). Priorities: **01 EML→PDF**, **02 PDF→CSV**, **03 PDF→XLSX**
> (all high), **04 PDF→PDF/A** (medium). Per-endpoint engine/param
> audit: `migration/reports/P3-CONVERSION-READINESS.md`.
>
> **Status refresh (2026-11-06, third pass):** P0/P1 done; P2
> `app-config` routed. P3: `url/pdf`, `markdown/pdf`, `ebook/pdf`,
> `pdf/epub`, `html/pdf`, `file/pdf`, `cbz`/`cbr` both ways, `vector`
> both ways, **`eml/pdf`** (`handlers::eml`, mail-parser → WeasyPrint;
> MSG rejected, office-convert branch not ported), **`pdf/pdfa`**
> (`convert::pdf_to_pdfa`), **pipeline `handleData`**
> (`handlers::pipeline` + generated `pipeline_table.rs`), **`pdf/csv`**
> and **`pdf/xlsx`** (`src/tabula.rs` lattice extraction → CSV/zip or a
> hand-built `.xlsx` workbook) are routed.
> P4: `remove-blanks`, `auto-split-pdf`, `ocr-pdf`, `scanner-effect`
> and `extract-image-scans` are all routed. **Parity caveat:** migrated-endpoint Java controllers are
> deleted in the working tree — `testing/parity.sh` 404s; acceptance is
> Rust tests + fixtures + runtime smoke, not Java parity.

## P0 — Make the crate compile (BLOCKS EVERYTHING) — DONE

`cargo check`, `cargo clippy --all-targets -- -D warnings` (note: no
double `--`), and `cargo fmt --check` are all clean as of 2026-11-06.
The original 23 errors are resolved; details kept below for history.

- [x] `rust/src/handlers/convert.rs` — 9 errors:
  - `base64` imported (line 18) and used (line ~1461) but not in
    `Cargo.toml`. Either add `base64` (pin a version ≥7 days old) or
    replace the use with whatever encoding the call site needs.
  - Line ~696: unused `ImageFormat` import — remove.
  - Lines ~715, ~739, ~1144: argument type mismatches — read the
    callee signatures before editing.
  - Line ~825: `&Vec<u32>` passed where `&[usize]` expected — fix the
    call or the signature.
  - Line ~1174: `==` on `PdfToOffice` — derive/impl `PartialEq` or
    match on the enum.
  - Line ~1331: `ZipFile::read_to_end` — `use std::io::Read;` or call
    the zip crate's own read API.
- [x] `rust/src/handlers/form.rs` — 11 errors:
  - Lines ~3101, 3138, 3171, 3178, 3312, 3338: `.as_str()` on an
    existing `&str` hits unstable `str::as_str` — drop the call.
  - Line ~3664: `i64` where `Object` expected — wrap in
    `Object::Integer` (or the right `Object` variant).
  - Line ~3871: closure/fn arg type mismatch.
  - Line ~2856: returning a reference into parameter `font` — return
    an owned `Object`/clone instead.
  - Line ~3462: `*doc` borrowed mutably and immutably — reorder or
    clone the needed value first.
- [x] `rust/src/endpointconfig.rs:367` — iterator yields `(&&str, _)`
  but map wants `(&str, _)`: add `.copied()`/`.map(|(k,v)|(*k,v))` in
  the `alternatives` collect.
- [x] `rust/src/config.rs:139` — `settings` moved into `Self` then
  read for `metrics_enabled`: compute `metrics_enabled` before the
  struct literal or clone the field.
- [x] Acceptance: `cargo check` exits 0. `cargo fmt`/`clippy` sweep
  still open.

## P1 — Verify what already exists (after P0) — DONE except parity

- [x] `cargo test` — 280 tests pass, 0 failures.
- [x] `cargo clippy --all-targets -- -D warnings` — clean.
- [x] `#[endpoint]` convert fns confirmed routed in
  `handlers/mod.rs` (`pdf_to_word`, `pdf_to_presentation`,
  `pdf_to_xml`, `pdf_to_text` at ~lines 255–261).
- [x] Runtime smoke with `JAVA_BACKEND_URL=off` on :8081:
  `merge-pdfs` → 200 + valid 2-page PDF; `filter-page-count` →
  200 pass-through on `comparator=Equal`, 204 on miss, 400 on bad
  comparator; `convert/img/pdf` (PNG) → 200 PDF.
- [x] `testing/parity.sh` sweep — **dropped**: Java fallback is out of
  scope now; acceptance is Rust tests + fixtures + smoke + output
  validation.

## P2 — Small missing core endpoints

- [x] `POST /api/v1/config/app-config` — routed to
  `platform::app_config` (`handlers/mod.rs`, `platform.rs:616`).
- [x] Platform GETs — `ui-data/home`, `ui-data/licenses`,
  `ui-data/pipeline`, `ui-data/ocr-pdf`, `get-endpoints-status`,
  `group-enabled` are routed in `platform.rs`. LoginDisclaimer /
  additionalLanguageJs / SignatureImageController / ui-data-sign /
  MetricsController / the whole web layer are now ported to Rust
  (see P7 passes 3–4); only `JobController` remains Java-side, kept
  for proprietary/saas job ownership.
- [x] Pipeline `handleData` (`POST /api/v1/pipeline/handleData`) —
  **DONE**: `handlers/pipeline.rs` ported from `PipelineController` +
  `PipelineProcessor` + `InternalApiClient`. Loopback HTTP dispatch to
  the server's own bound address (`platform::bound_addr`, ephemeral-port
  safe) so steps hit the same router/guards/proxy as direct calls;
  `pipeline_table.rs` is generated from `toolIO.ts` by
  `rust/scripts/generate_pipeline_table.mjs` (multi-input arity +
  accepted extensions); `policies.maxSteps`/`maxFilesPerStep` and
  `internalApi.*` timeouts read via `settings::policy_limits` /
  `internal_api_timeouts` (env-overridable). Filter-empty drop, zip
  unpack (depth 10, `.cbz` exempt), `removeTrailingNaming`, and
  `output.zip` dedup all match Java. **Known approximations** (Java
  controllers are deleted, so no live parity check): operation
  validity is approximated by the `InternalApiClient` dispatch
  allowlist + a required-`@RequestParam` table rather than
  `apiDocService.isValidOperation` against live api-docs — unknown but
  dispatchable paths dispatch and 404 into the empty-200 path instead
  of Java's up-front 400; fatal step failures return the same empty
  200 Java's `return null` produces. `?async=true` rides the shared
  `jobs::submit`. (`ui-data/pipeline` GET already native.)
  Orchestration requirements from Java (`PipelineController` +
  `PipelineProcessor` + `InternalApiClient`):
  - Request: multipart `fileInput[]` + `json` field →
    `PipelineConfig{operations: [{operation, parameters}]}`.
  - Each operation is an `/api/v1/...` path re-invoked **over HTTP** via
    `InternalApiClient` (RestTemplate loopback) — i.e. Java pipelines can
    mix migrated and proxied endpoints. Rust equivalent: call the axum
    router in-process or loop back to `127.0.0.1:<port>`; in-process
    dispatch avoids a self-connection but must reproduce multipart
    semantics and per-endpoint enablement checks.
  - Per file × per operation: skip when the op's declared input
    extensions don't match; `filter-*` ops treat an empty body as
    "file filtered out" and drop it from the chain; non-200 → logged,
    `hasErrors`, file dropped, pipeline continues.
  - Zip responses are unpacked and their entries become the next
    stage's input files; single final output → raw file, multiple →
    zip. TempFile lifecycle and `AutomationRunContext` run-id scoping
    per execution.
  - Validation: `apiDocService.isValidOperation` rejects unknown
    op/param combos up front (400). `toolMetadataService` supplies
    per-op input extensions and multi-input flag — Rust needs an
    equivalent table (derivable from `endpointconfig` group table +
    `ToolIO` annotations if ported).

## P3 — Convert family completion (~12 endpoints on Java fallback)

For each: read the Java `ProcessExecutor`/engine call site FIRST,
then wire `convert::EngineSpec`. Absent engine must reproduce Java's
group-enable 403 (`endpointconfig`/`guard` already model this).

- [x] `/convert/url/pdf` — routed (`convert::url_to_pdf`); SSRF guards
  (`is_valid_url`, `is_sensitive_address`, `is_disallowed_network_location`)
  implemented with tests.
- [x] `/convert/markdown/pdf` — routed (`convert::markdown_to_pdf`);
  comrak GFM render + zip asset inlining + weasyprint.
- [x] `/convert/html/pdf` — routed (`convert::html_to_pdf`).
- [x] `/convert/ebook/pdf` — routed (`convert::ebook_to_pdf`); Calibre
  (`ebook-convert`) + optional Ghostscript ebook-preset optimisation.
- [x] `/convert/eml/pdf` — **DONE** (was AGENT 01, high). MIME→HTML→PDF pipeline;
  `mail-parser` crate for EML, EML-only scope for MSG (rejected with a
  clear 400, magic-byte + extension); HTML→PDF reuses the WeasyPrint
  `EngineSpec` path (`useStirlingOfficeConvert` ignored, same divergence
  as `html_to_pdf`). `downloadHtml=true` bypasses the engine.
  `includeAttachments` embeds files into the PDF `/EmbeddedFiles` name
  tree (lopdf); Java's page-level FileAttachment annotations are NOT
  ported — they need PDF text-position extraction. Sanitizer is
  `ammonia` with an OWASP-policy-shaped allowlist, not
  java-html-sanitizer.
- [x] `/convert/pdf/csv` — routed (`convert::pdf_to_csv`); `src/tabula.rs`
  ports Tabula ruling/region detection on `text.rs` positioned runs,
  plus a borderless text-edge pass (persistent x-gutters across aligned
  lines) for tables without rulings. Pure Rust, no engine, no group gate.
- [x] `/convert/pdf/xlsx` — routed (`convert::pdf_to_xlsx`); same
  `tabula.rs` extractor (ruled + borderless), workbook written
  natively (`xlsx_workbook`,
  no `rust_xlsxwriter` dependency). `useStirlingOfficeConvert` /
  `system.stirlingOfficeConversion` → 400 — the Office-Convert branch
  is not ported and does not proxy to Java.
- [x] `/convert/pdf/pdfa` — routed (`convert::pdf_to_pdfa`); Ghostscript
  `-dPDFA`/`PDFA_def.ps` + PDF/X via `EngineSpec`, qpdf pre-clean,
  embedded ICC profiles (`rust/assets/icc/`). `strict` (veraPDF is a
  Java library), `useStirlingOfficeConvert`, and Ghostscript failures
  all forward to Java — the PDFBox sanitizer chain is not ported, so
  gs-failed PDFs get Java's full fallback. Smoke-verified live: PDF/A-1b/2b/3b,
  level-A→B fallback, PDF/X-2008, 400 on non-PDF.
- [x] `/convert/pdf/epub` — routed (`convert::pdf_to_epub`); Calibre
  `ebook-convert` with chapter detection, output profile, EPUB/AZW3.
- [x] `/convert/pdf/video` — NOT PORTED, intentionally: Java commented
  the endpoint out ("ffmpeg disabled due to raised CVEs"); nothing to
  migrate.
- [x] `/convert/cbz/pdf`, `/pdf/cbz`, `/cbr/pdf`, `/pdf/cbr` — routed;
  extraction uses the first available archive CLI
  (`unrar`/`unar`/`bsdtar`/`7z`) and returns a clear missing-dependency
  error naming the required executables when none exist (no Java
  fallback), creation shells out to `rar` (`PdfToCbrUtils` shape) with
  endpoint probing for the `rar` group.

## P4 — Raster/OCR-dependent misc

Per endpoint: Ghostscript rasterisation (`misc::BlankRasterizer` is
now `pub(crate)` and shared) vs documented non-port.
- [x] `ocr-pdf` — routed (`handlers::ocr::ocr_pdf`): OCRmyPDF preferred
  when its group is enabled (full flag map, `--jobs 1` retry on the
  multiprocessing `ENOSYS` failure, exit 6 → prior-text 400,
  `-dFILTERIMAGE` post-pass), else per-page Tesseract at 300 DPI via
  `BlankRasterizer` + `text::page_items` skip-text check, merged with
  `pdfdoc::import_pages`. Language list filtered against tessdata
  `*.traineddata` stems like `getAvailableTesseractLanguages`.
  Divergence: no `system.maxDPI` clamp (no Rust setting), sequential
  page processing.
- [x] `extract-image-scans` — routed (`handlers::image_scans` +
  `image_split`): native Rust port of `split_photos.py` — no Python/cv2
  dependency (endpoint dropped from the `Python`/`OpenCV` groups).
  PDF pages render at `min(300, system.maxDPI)` via `BlankRasterizer`
  (Ghostscript), pages rasterising and splitting concurrently on scoped
  threads with page order preserved. Image inputs decode via the
  `image` crate (jpeg/png/gif/bmp/webp/tiff/pnm/hdr/tga/dds/exr/ico/
  qoi/ff — heic/avif/jp2 remain gaps vs cv2's imread). Zip or
  single-PNG naming matches Java. Script quirk
  preserved: `minArea`/`minContourArea` params are parsed but ignored —
  `split_photos.py` never forwarded them, so its 10000/500 defaults
  always applied.
- [x] `remove-blanks` — routed (`misc::remove_blanks`); text via the
  shared `text::page_items` extractor, image gate via resource-dict
  scan, white-ratio via Ghostscript `png16m` at 150 DPI (Java's
  `renderImageWithDPI` equivalent). Without gs, image-bearing pages are
  kept — documented divergence. ZIP entry names match Java
  (`*_nonBlankPages.pdf`, `*_blankPages.pdf`, `*_processed.zip`).
- [x] `scanner-effect` — routed (`handlers::scanner::scanner_effect`):
  full `ScannerEffectController` port — quality presets, safe-DPI
  resolution clamp (Java's 500 default; `system.maxDPI` unported),
  random gradient border, bilinear rotation (Java bicubic — output is
  stochastic anyway), edge feather, 2-pass box blur, and the
  brightness/contrast/yellowish/noise grade with Java's truncation
  semantics. Re-embedded as image-only pages via the shared
  `convert::commit_page`. Divergence: sequential page processing
  (Java uses a ForkJoinPool), no `system.maxDPI` setting.
- [x] `auto-split-pdf` — routed (`automation::auto_split_pdf`); divider
  detection tries embedded `/Image` XObjects (≤3, `imgcomp` decode →
  luma → `rqrr`), then Ghostscript `png16m` renders at 150 DPI with a
  300 DPI retry, matching `AutoSplitPdfController`'s order. Only the
  three Stirling QR URLs split; `duplexMode` skips divider backs.
  Divergence: `rqrr` does its own binarization where Java tries ZXing's
  Hybrid then GlobalHistogram binarizers.

## P5 — Security crypto family (architectural, do not half-port)

Blocked on missing crypto stack — see `migration/reports/
PHASE-06-SECURITY.md` §cryptographic dependency analysis. Options:
`rasn`+`rasn-cms` for CMS signing/timestamp/validation, `cryptoki`/
`pkcs11` for hardware, veraPDF binary for verify/validate-compliance.
No Java fallback exists anymore — this family stays unimplemented
until a dedicated engine project lands. Do NOT ship visual-only
approximations.

## P6 — Platform/deployment

- [x] Job persistence: `jobstore.rs` persists jobs to
  `configs/stirling-jobs.db` (SQLite); falls back to memory-only with a
  startup warning when the DB is unavailable.
- [x] `web.rs` static serving: SPA fallback + traversal rejection
  implemented and unit-tested (`web::tests`). Verify against the real
  frontend build layout remains a deployment-time check.
- [x] Graceful shutdown: `axum::serve(...).with_graceful_shutdown`
  wired in `main.rs` (ctrl_c + SIGTERM).
- [x] Engine presence detection parity: `endpointconfig::init` now
  probes every `ENGINE_PROBES` binary (`gs`, `qpdf`, `soffice`,
  `unoconvert`, `weasyprint`, `pdftohtml`, `tesseract`, `ocrmypdf`,
  `rar`, `magick`, `ebook-convert`) plus the Python/OpenCV check,
  mirroring `ExternalAppDepConfig` — missing or under-versioned
  (weasyprint <58, qpdf <12) engines disable their tool group and the
  guard 403s. Verified live: Weasyprint missing → `html-to-pdf` 403,
  `compress-pdf` survives via the Ghostscript alternative → 200.
- [x] Sidecars documented in `rust/DEPLOYMENT.md` (engine table,
  version floors, `system.customPaths.operations.*` overrides,
  `stirling doctor`/`engines list` reporting).

## P7 — Java decommission (post-cleanup passes)

Controller deletion is now driven by route coverage: a Java controller
is deleted only when every endpoint it maps is registered in
`rust/src/handlers/mod.rs`. Verified by `./gradlew compileJava
compileTestJava` on all modules after each pass.

- [x] Controller deletion pass 2 (2026-11-06): removed the 15 live
  converter controllers + `ConvertPdfToVideoController` (endpoint
  commented out), `FormFillController` + `FormPayloadParser`,
  `SettingsController`, `ConfigController`, `LoginDisclaimerController`,
  `OCRController`, `ScannerEffectController`, `AutoSplitPdfController`,
  `BlankPageController`, `PrintFileController`, `PipelineController` +
  `PipelineProcessor` + `PipelineDirectoryProcessor` — ~55 tests.
- [x] Dead model/util sweep: ~60 `model/api/**` request classes,
  pipeline models, `StringToMapPropertyEditor`, `AutoRotateDetection`,
  `SvgToPdf`, `SvgOverlayUtil`, `ConnectedInputStream`,
  `CacheUnavailableException`, swagger response classes — ~70 tests.
  Method: import/same-package-aware reachability from live roots;
  bean stereotypes, `*RepositoryCustomImpl`, filters/listeners and
  other framework-wired classes are never auto-deleted.
- [x] `ExtractImageScansController` + request/tests deleted —
  `handlers/image_scans.rs` route verified first.
- [x] Platform/web pass 3 (2026-11-06): ported and deleted
  `UIDataController` (`/ui-data/sign` — shared signatures from
  `customFiles/signatures/ALL_USERS` + font metadata; personal category
  needs proprietary auth, documented divergence), `SignatureImageController`
  (`/general/signatures/{fileName}`, filename validation matches
  `SharedSignatureService`), `SharedSignatureService`, `SignatureFile`,
  `AdditionalLanguageJsController` (`/js/additionalLanguageCode.js`),
  `LanguageService`, `MetricsController` (all 12 `/api/v1/info/*`
  endpoints — `metrics.rs` now covers load/unique/wau; "session" tag
  approximated by `X-Browser-Id`/User-Agent, WAU always on since Rust
  has no login mode), `WeeklyActiveUsersService`, `WAUTrackingFilter`,
  `RobotsController` (Rust `web.rs` static serving). New `fonts.rs`
  serves bundled `fonts/*.woff2` for `/fonts/*` and the sign-data font
  list. Java compile verified on common/core/proprietary.
- [x] Web-layer pass 4 (2026-11-06): `ReactRoutingController` fully
  ported into `web.rs` — SaaS-landing swap on `/`+`/index.html` only,
  prerendered `<route>.html` pages, desktop-mode `mobile-upload`/
  `mobile-sign`, the Tauri `/auth/callback/tauri` deep-link page,
  missing-index fallback page, `SYSTEM_ROOTURIPATH` context-path HTML
  processing, and `CleanUrlInterceptor`'s query-param allowlist
  redirect. Controller, interceptor, and tests deleted; Java compile
  verified.
- [x] Proxy removal (Phase E): `handlers/proxy.rs`,
  `config::JavaBackend`, and `JAVA_BACKEND_URL` are gone — the fallback
  404s unmigrated `/api/**` and serves the SPA/static surface for
  everything else. `AppState` no longer carries a backend; all
  `forward_*` call sites were deleted with it (unsupported modes return
  `BadRequest`).
- [ ] Platform/web endpoints still Java-only: `JobController` +
  `JobOwnershipCache` — every route is registered in Rust, but
  proprietary/saas `@AutoJobPostMapping` endpoints still create jobs in
  the Java job store, so the controller must stay until those modules
  are migrated or job ownership is unified.
- [ ] ~96 unreachable bean classes flagged by the sweep (servlet
  filters, listeners, `*RepositoryCustomImpl`, DI impls such as
  `AttachmentService`, `PortfolioService`, `ReplaceAndInvertColor*`,
  `FingerprintBasedSessionFilter`, `MetricsFilter`, MCP `Stirling*Tool`s)
  — need a DI-aware review (interface injection, servlet-container
  registration, Spring Data naming convention) before deletion; do not
  bulk-delete on name-reachability alone.
- [ ] Remaining Java-only API surface after the above: security crypto
  family (P5 — `CertSign`, `HardwareSigning`, `Timestamp`,
  `ValidateSignature`, `VerifyPDF`, `ValidateCompliance`,
  `PasswordController::restore-pdf-protection`) and all of
  proprietary/saas. `RedactController::auto-redact`/`redact-execute`
  are routed natively (`pdftext::redact` + overlay cover, matching
  Java's PDFBox fallback; `convertPDFToImage` rasterises through
  Ghostscript at 300 DPI like `PdfUtils.convertPdfToPdfImage`). The jpdfium markdown pipeline is fully ported in
  `rust/src/pdftext/` and serves `/convert/pdf/markdown`.
- [ ] Phase G build cleanup (only after F): `settings.gradle`,
  `app/*/build.gradle`, `buildSrc/`, `.taskfiles/backend.yml`,
  `runtime/docker/**`, Java jobs in `.github/workflows/*`.
- [x] Phase H verification (2026-11-06): `stirling-server` boots and
  serves with `JAVA_HOME`/`JAVA_BACKEND_URL` unset and no JVM linkage
  (`ldd` clean); `info/version` reports `runtime: rust`, and live
  requests verified — `merge-pdfs`, `convert/pdf/pdfa` (PDF/A-2b),
  `form/fields`, `security/sanitize-pdf` all 200 with valid PDFs.
  `stirling test runtime` smoke: 12/12 PASS. `testing/parity.sh` stays
  obsolete (Java controllers for migrated endpoints are deleted).

## P8 — Engine cutover and tooling

- [ ] `rust/engine` (stirling-engine) vs `engine/` (Python): resolve the
  known divergences in `rust/engine/README.md` (pydantic wire
  validation on request bodies, snake_case aliases, RFC 3339
  `expires_at`, generated `tool_models` parity), then cut the frontend
  engine proxy over and retire the Python service.
- [ ] Shell elimination phase 2 — tracked in
  `migration/reports/SHELL-ELIMINATION-PLAN.md`: port the smoke
  scripts (`test-general.sh`, `test-security.sh`, `smoke.sh`) to Rust
  integration tests or `stirling test runtime`, then delete the `.sh`;
  `backend:run/stop` stays on `scripts/run.sh` until Java retires.
- [x] Frontend backend target: `frontend/vite.config.ts` defaults
  `BACKEND_URL` to `http://localhost:8081` (Rust standalone); `:8080`
  remains an override for a Java sidecar. `VITE_DESKTOP_BACKEND_URL`
  stays empty so desktop uses the bundled backend.
- [ ] CI: Java build/test/package jobs in `.github/workflows/` still
  run on every PR — gate or remove as decommission phases land.

## P9 — Desktop (Tauri) Rust-backend migration

Audit: `migration/reports/desktop-phase-b-endpoint-audit.md`.

- [x] Phase A — sidecar wiring: `select_backend()` prefers
  `stirling-server[-<triple>]`, `STIRLING_DESKTOP_BACKEND` override,
  `run_stirling_server()` env contract verified live (port banner,
  `/health`, sentinel shutdown). `find_rust_server` probes packaged
  resources, `src-tauri/binaries/` (dev), and `STIRLING_SERVER_PATH`;
  `task desktop:rust-server` stages the binary and is wired into
  `prepare`. Tauri crate: check/clippy/test/fmt clean (90 tests).
- [x] Phase B — endpoint surface: `endpointconfig::UNSUPPORTED_ENDPOINTS`
  marks the no-route keys (`cert-sign`, `timestamp-pdf`,
  `validate-signature`, `verify-pdf`, `validate-compliance`, `ingest`,
  `restore-pdf-protection`, `orchestrate`, `tools`, `external-api-call`,
  `purview-*`) disabled with `DisableReason::NotSupported` →
  `NOT_SUPPORTED_LOCALLY`; path spellings translate to keys. Engine-gated
  endpoints degrade via `ENGINE_PROBES` group probes. Dead registry keys
  and frontend-composed tools deliberately left enabled (parity + they
  work through routed calls).
- [ ] Phase C — engine bundling: decide per-platform which of
  `gs`/`qpdf`/`pdftohtml`/`rsvg-convert`/`inkscape`/unrar-family/
  `tesseract`+`ocrmypdf` ship in the installer; `weasyprint`/`soffice`/
  `ebook-convert` likely stay off (group probes already degrade).
- [ ] Phase D — installer slimming: drop `libs/*.jar` +
  `runtime/jre/**` from `tauri.conf.json` resources and the
  `BackendKind::Java` fallback once the Rust path is proven on all
  desktop OSes (~100 MB+ per installer). Requires a real
  `tauri build` smoke test first — `cargo check` does not cover
  `externalBin` resource resolution.

## Explicitly out of scope (do not touch)

- ~74 proprietary/SaaS controllers (auth, billing, teams, audit,
  storage, MCP, policy, webhooks) — stay Java/proprietary; not part of
  this Rust PDF-engine migration.
- Java source deletion — confirmed intentional and now executing (see
  P7): migrated-endpoint controllers, their tests, and unreachable
  models/utils are being removed pass by pass. Do not restore deleted
  files, do not implement proxy-to-Java fallback for missing engines,
  and do not treat Java parity as an acceptance criterion.
