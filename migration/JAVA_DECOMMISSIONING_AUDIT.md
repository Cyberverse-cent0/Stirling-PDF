# Java Decommissioning Audit — Phase 0

Audit date: 2026-11-06. Based on the working tree, including 97 uncommitted
Java deletions from the earlier strangler pass. All counts exclude `build/`
and `bin/` output.

## 1. Inventory totals

| Category | Count |
|---|---|
| Total `.java` files | **2,440** |
| — main sources | 1,466 |
| — test sources | 974 |
| `*Controller.java` files | **108** (34 core, 53 proprietary, 21 saas) |
| Files under `*controller*` paths (incl. non-suffixed endpoint classes) | 108 |
| `*Service*.java` files | ~303 |
| `*Repository.java` files | 70 (all proprietary/saas — Spring Data JPA) |
| Java resources (`application*.properties`, `settings.yml`, `messages*.properties`) | ~12 files + `configs/` |

Modules: `app/common` (385 files — PDFBox/JPDFium/utilities), `app/core`
(492 — the `/api/v1/**` backend), `app/proprietary` (1,221 — auth, billing,
teams, storage, policy, MCP, AI agents), `app/saas` (342 — SaaS billing,
account-link, payg, legal).

## 2. Runtime architecture today

- Rust (`rust/`, axum + lopdf) registers **112 explicit `/api/v1/**` routes**
  in `handlers/mod.rs` plus job/file/metrics/health routes.
- `handlers/proxy.rs` + `config::JavaBackend` forward **every unregistered
  `/api/**` request** to `JAVA_BACKEND_URL`. The variable now **defaults to
  off** — unmigrated routes 404 unless the operator opts in.
- `handlers/jobs.rs` forwards unknown job IDs to Java so both job
  namespaces coexist.
- In-handler fallbacks still call `proxy::forward_multipart` /
  `forward_parts` for unsupported parameter modes: `misc.rs` ×7,
  `convert.rs` ×2, `security.rs` ×1 (`redact` rasterise mode),
  `automation.rs` ×1, `visual.rs` ×1.
- Frontend Vite proxies `/api/*` to `BACKEND_URL` (default
  `http://localhost:8080`); production nginx/embedded mode expects the
  Java-served SPA + API on one port.

## 3. Controller classification — core (`SPDF/controller`)

### Fully replaced by Rust (routes registered)

| Java controller | Rust module | Notes |
|---|---|---|
| Analysis (8 endpoints) | `handlers/analysis.rs` | all `/analysis/*` |
| General page ops (merge, rotate, remove/rearrange/split pages, crop, scale, overlay, bookmarks, multi-page-layout, booklet, poster, pdf-to-single-page, remove-image) | `handlers/general.rs`, `merge.rs`, `split.rs`, `transform.rs`, `layout.rs` | ~20 routes |
| Filter (6) | `handlers/filter.rs` | `filter-*` |
| Form (9) | `handlers/form.rs` | all `/form/*` incl. add/edit/modify |
| Misc migrated (~24) | `handlers/misc.rs`, `automation.rs`, `visual.rs`, `images.rs`→`imgextract` | attachments, comments, stamps, page numbers, metadata, sanitize-adjacent, JS viewer, portfolio, compress/decompress, repair, auto-rotate/rename, batch-attachments |
| Security password (2 of 4) | `handlers/passwords.rs` | `add-password`, `remove-password` |
| `security/redact` (manual-rect mode) | `handlers/security.rs::redact` | 🔀 rasterise mode still proxies |
| `security/remove-cert-sign`, `get-info-on-pdf`, `add-watermark`, `sanitize-pdf` | `handlers/security.rs`, `visual.rs` | partial-mode fallbacks remain |
| `JobController` job/file routes | `handlers/jobs.rs` | unknown job IDs still proxy |
| `mobile-scanner/*` (6) | `handlers/` mobile-scanner routes | native |
| `convert/img/pdf`, `convert/pdf/img`, `svg/pdf`, `file/pdf`, `pdf/{word,presentation,xml,html,vector,text}`, `vector/pdf` | `handlers/convert.rs` + `src/convert.rs` engine runner | **new this pass** — sidecar engines (LibreOffice, GS, Chromium, FFmpeg, unoconv) |

### Partially replaced (route exists, some modes proxy)

| Java controller | Gap |
|---|---|
| `OCRController` (`/misc/ocr-pdf`) | no Rust route — needs Tesseract sidecar |
| `ExtractImageScansController` | rendered-page + OpenCV scan detection — Java only |
| `AutoSplitPdfController`, `BlankPageController` | no Rust routes |
| `PrintFileController` | no Rust route (server-side print spooler) |
| `ScannerEffectController` | no Rust route |
| `LoginDisclaimerController`, `ConfigController` (app-config, endpoint-enabled), `SettingsController`, `UIDataController` | platform/UI endpoints — Java only |
| `PipelineController` (`/pipeline/handleData`) | watch-folder/pipeline engine — Java services |
| `PasswordController` remainder | `prepare-pdf-session` ✅ ported; `restore-pdf-protection` still Java (JPDFium `copyEncryption`) |
| `RedactController` | `/redact` manual mode ✅; `/auto-redact`, `/redact-execute` Java (JPDFium `PdfRedactor`) |
| `CertSignController`, `TimestampController`, `HardwareSigningController`, `ValidateSignatureController`, `VerifyPDFController`, `ValidateComplianceController` | crypto stack — Java only (see §5) |
| Converter remainder | `/convert/ebook/pdf`, `/pdf/epub`, `/pdf/xlsx`, `/pdf/csv`, `/pdf/video`, `/cbz/pdf` (if distinct), `/pdf/pdfa`, eml/html/website/markdown→pdf variants not yet registered |

### Java-only by design (platform, not PDF API)

- `app/proprietary/**` — 53 controllers: auth (`AuthController`,
  `UserController`, `TeamController`, invites, licenses), billing/payg,
  storage (`FileStorage`, folders, encryption-at-rest), policy engine
  (161 files), audit, MCP server, AI agents, workflows/signing sessions,
  account-link, integrations (Purview), notifications, form-detection.
- `app/saas/**` — 21 controllers: PAYG wallet/invoices, procurement,
  account-link/instance handover, AI proxy, legal, team ownership.
- `app/core` web controllers — `MetricsController` (Micrometer),
  `ReactRoutingController` (SPA fallback), `RobotsController`,
  `SignatureImageController`, `AdditionalLanguageJsController`,
  `JobController` (partially superseded).

## 4. Java fallback dependencies (exact)

| Rust call site | Java functionality invoked |
|---|---|
| `proxy.rs` fallback | **everything unregistered** — converters remainder, OCR, security crypto, pipeline, all proprietary/saas APIs, platform endpoints |
| `jobs.rs:255` | job status/result/files for jobs created by Java |
| `misc.rs` ×7 | unsupported image formats in extract-images, unsupported add-image/add-comments modes, SVG/other stamp sources |
| `convert.rs` ×2 | engine-uncovered conversions |
| `security.rs` redact | `convertPDFToImage` rasterisation path |
| `automation.rs`, `visual.rs` | parameter modes without native equivalent |

`JAVA_BACKEND_URL`: read only in `config.rs::JavaBackend::from_env`;
defaults to **disabled**. `main.rs` logs the 404 behaviour.

## 5. External engines still required

| Capability | Java implementation | Rust path |
|---|---|---|
| Office↔PDF, HTML, ebook, eml, markdown, svg, vector | Apache POI/Tika/unsoffice/unoconv, openhtmltopdf, PDFBox renderer | `src/convert.rs` runner — **sidecar binaries required at runtime** (LibreOffice, Ghostscript, Chromium, FFmpeg) |
| OCR | Tesseract via PDFBox/tess4j | sidecar or `tesseract` crate + render (no PDF renderer in lopdf) |
| Redaction (text/regex/image) | JPDFium `PdfRedactor` native lib (`com.stirling:jpdfium`) | needs JPDFium-equivalent Rust binding or custom content-stream engine — **hard blocker** |
| Sign/timestamp/verify | BouncyCastle CMS, PKCS#12/JKS, PKCS#11, Windows cert store, RFC 3161 TSP | needs `cms`/`x509-cert`/`p11` crates — feasible but unimplemented |
| PDF/A, PDF→video, PDF→xlsx/csv/epub | veraPDF, PDFBox renderer + encoders, POI | engine or dedicated crates |
| Rendering (page→image) | PDFBox/JPDFium render | no lopdf renderer — **hard blocker** for OCR/scans/rasterise |
| Persistence | H2/PostgreSQL via Spring Data JPA (70 repositories) | proprietary/saas platform — out of PDF scope |

## 6. Cannot delete yet

- All of `app/proprietary`, `app/saas`, `app/common` JPDFium/PDFBox
  service layer, `app/core` config/security/platform classes — the Rust
  server has **no** replacement for auth, persistence, billing, policy,
  audit, storage, MCP, or the crypto/rendering engines above.
- `FormFillController` + `FormPayloadParser` — Rust port exists but keep
  until parity verified; likewise all partially-migrated controllers.
- `PasswordController` — `restore-pdf-protection` still Java.
- Build: `settings.gradle`, root `build.gradle`, `app/*/build.gradle`,
  `buildSrc/`, `.taskfiles/backend.yml`, `runtime/docker/**`.

## 7. Safe-to-remove-later candidates

- The 97 already-deleted controller/test files (uncommitted `D`) —
  commit them once the Rust routes are verified live.
- `FormFillController`/`FormPayloadParser` after form parity sign-off.
- Java model/DTO classes under `SPDF/model/api/**` for migrated
  endpoints (request/response records) once endpoints are confirmed dead.
- Corresponding `app/core` tests already deleted with controllers.

## 8. Recommended deletion order (Phases A–H)

- **A — API replacement**: finish converter remainder (sidecar engines),
  misc stragglers (auto-split, remove-blanks, print-file, scanner-effect,
  extract-image-scans, OCR), platform endpoints (config, settings,
  uidata, disclaimer) — decide Rust-native vs retire.
- **B — Service replacement**: port `SPDF/service`, `SPDF/utils`,
  `SPDF/pdf`, `common/util` pieces still referenced by live endpoints.
- **C — Platform replacement**: authN/Z, config, JPA persistence,
  startup (`SPDFApplication`), scheduling — largest block; only after A/B.
- **D — External engines**: stand up LibreOffice/GS/Chromium/FFmpeg
  sidecars (runner exists in `src/convert.rs`); evaluate JPDFium binding
  or custom engines for redaction/signing/rendering.
- **E — Fallback removal**: delete `proxy.rs`, `JavaBackend`,
  `JAVA_BACKEND_URL`, in-handler `forward_*` calls, `jobs.rs` proxy leg.
- **F — Java source deletion**: controllers → services → models →
  resources → tests, module by module (`core` → `common` →
  `proprietary`/`saas` last or never, if platform stays Java).
- **G — Build/deploy cleanup**: gradle files, `buildSrc`, backend
  taskfile, docker stages, Jib/native-image config.
- **H — Rust-only verification**: boot with no JDK/`JAVA_HOME`, no
  `JAVA_BACKEND_URL`, full API smoke.

## 9. Estimated remaining work

- Core PDF API: ~85–90% migrated by endpoint count; the residual is
  engine-bound (converters remainder ~6, OCR/scans/print ~5,
  security crypto ~8, pipeline ~1, platform ~8).
- Proprietary/SaaS: 0% — ~1,560 files, platform scope, not part of the
  PDF strangler unless the product decision is Rust-everything.
- Hard blockers requiring new native capability: PDF rendering, secure
  redaction, CMS/PKCS#11 signing, RFC 3161, PDF/A, OCR pipeline.

**Do not remove Java yet.** The proxy is already opt-in, but ~30 core
endpoints and the entire proprietary/saas platform still require it.
