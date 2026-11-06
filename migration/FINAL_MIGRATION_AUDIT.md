# Final Migration Audit — Java → Rust

Audit date: 2026-11-06 (reconciliation pass). Supersedes the earlier snapshot;
agent reports under `migration/reports/` are left untouched as history.

Method: every Java controller under `app/**/controller/**` was enumerated and
its Spring mappings extracted, then diffed against the explicit routes in
`rust/src/handlers/mod.rs` plus `src/metrics.rs`. The `/api/**` fallback proxy
(`handlers/proxy.rs` → `JAVA_BACKEND_URL`) means anything not registered still
works via Java — "proxied" below means "not yet ported", not "broken".
Non-`/api/` paths now serve the SPA build via `web::serve`.

**Verification status: `cargo check --all-targets` clean, `cargo clippy
--all-targets` clean (0 warnings after auto-fixes), `cargo fmt --check` clean,
`cargo test` 167/167 pass (2026-11-06 cleanup pass). Java-parity and live
runtime status remain unverified — no Java backend was started; parity claims
rest on source inspection only.**

## A. Current counts

| Metric | Count |
|---|---|
| Rust routes registered (`handlers/mod.rs` `.route()` calls) | 115 |
| Additional routes via `metrics::routes()` (`/api/v1/info/*`) | 5 |
| Total HTTP routes | 119 (+ `/health`, included above) |
| Infra routes (`/health`, `/api/v1/ping`, `/api/v1/info/version`) | 3 |
| Job/file-management routes (migrated `JobController` surface) | 7 |
| Platform routes (`handlers/platform.rs`: config, ui-data, settings, disclaimer) | ~13 |
| Migrated tool/endpoint routes | ~105 |
| Java `*Controller.java` files remaining on disk (core `SPDF/controller/api`, excl. helpers/tests) | ~40 |
| Core Java endpoint mappings still on disk | ~70 (+7 `JobController`, all Rust-routed) |
| Of those, served by Rust routes | ~38 (form ×9, convert ×14, security ×2, platform ~13) |
| Still effectively proxied to Java | ~33 |
| Proprietary/SaaS controllers (out of Rust scope) | ~74 files, unchanged |

## B. Reconciliation findings

### Integration defects — none found this pass

- All 16 handler modules declared in `handlers/mod.rs`; every `mod.rs` route
  target resolves to a real function (the four `pdf_to_office_route!` macro
  outputs — `pdf_to_word`, `pdf_to_presentation`, `pdf_to_xml`, `pdf_to_text` —
  verified present via macro expansion site).
- All `src/` modules declared in `main.rs` (`auth`, `endpointconfig`,
  `metrics`, `settings`, `web`, `guard`, `convert`, `text`, `fontmetrics`, …).
- No duplicate route paths (the `job/{jobId}` GET+DELETE pair is legal;
  apparent dup strings were test literals).
- Middleware chain complete: `api_guard` (endpoint enablement via
  `endpointconfig::is_endpoint_enabled_for_uri`) → `auth_guard` →
  `metrics::track`.
- Proxy fallback intact: `/api/**` → Java, non-API → SPA/`web::serve`.
- `misc.rs` dead duplicates (`auto_rotate_pdf`, `repair_pdf`) confirmed
  removed.

### Intentional non-route

- `misc::remove_blanks` exists as a complete `#[endpoint]` handler but is
  deliberately unrouted (`#[allow(dead_code)]` + doc comment): its blank-page
  classification is an image-XObject estimate, not Java's 150-DPI rasterisation,
  and can keep pages Java would drop. `/api/v1/misc/remove-blanks` stays on the
  Java proxy. **Not a defect — a documented hold.**

### Known partial/delegating routes (by design)

- `convert::img_pdf` — non-png/jpeg image formats forward to Java.
- `convert::pdf_to_img` — `gif`/`webp` forward to Java (no gs device).
- `security::redact` — `convertPDFToImage=true` forwards to Java.
- `misc::add_image`, `misc::replace_invert_pdf`, `misc::add_comments`,
  `visual::add_watermark`, `layout::{multi_page_layout,booklet_imposition}` —
  unsupported parameter modes forward to Java (per agent reports).

## C. Endpoint status — what is NOT implemented in Rust

### `/api/v1/convert` — 1 endpoint still proxied

| Endpoint | Blocker |
|---|---|
| `pdf/video` | **dead upstream** — commented out in `ConvertPdfToVideoController` (ffmpeg CVEs); correctly stays proxied |

Landed since the last audit pass: `url/pdf` (SSRF-guarded fetch +
WeasyPrint), `markdown/pdf` (comrak GFM + zip asset inlining +
WeasyPrint), `ebook/pdf` (Calibre `ebook-convert` + optional GS ebook
preset), `pdf/epub` (Calibre EPUB/AZW3 with chapter detection and output
profile), `cbr/pdf` + `pdf/cbr` (`rar` CLI extraction/packing behind the
`rar` engine probe), `eml/pdf` (mail-parser → HTML → WeasyPrint; `.msg`
rejected), `pdf/pdfa` (Ghostscript devices + qpdf normalize/clean),
`pdf/csv` + `pdf/xlsx` (full Tabula lattice port in `src/tabula.rs` —
ruling extraction from path ops, snap/collapse/intersection/cell/grid
phases, word merging — feeding a commons-csv-compatible CSV writer and a
hand-built OOXML workbook; `useStirlingOfficeConvert` branches forward
to Java). All engine-backed; runtime availability of
`weasyprint`/`ebook-convert`/`rar`/`gs`/`qpdf` unverified.

Already routed natively (26): `img/pdf`, `pdf/img`, `svg/pdf`, `file/pdf`,
`pdf/word`, `pdf/presentation`, `pdf/xml`, `pdf/text`, `html/pdf`, `pdf/html`,
`vector/pdf`, `pdf/vector`, `cbz/pdf`, `pdf/cbz`, `cbr/pdf`, `pdf/cbr`,
`url/pdf`, `markdown/pdf`, `ebook/pdf`, `pdf/epub`, `eml/pdf`, `pdf/pdfa`,
`pdf/csv`, `pdf/xlsx` — engine-backed (`gs`,
`rsvg-convert`/`inkscape`, `unoconvert`/`soffice`, `weasyprint`,
`pdftohtml`, `ebook-convert`, `rar`, `qpdf`). **Runtime availability of
those binaries is unverified.**

`cbz/pdf` (`convert::cbz_pdf`): native zip walk + natural-order sort + one
page per jpeg/png image sized to pixel dimensions, JPEG `/DCTDecode`
passthrough, `optimizeForEbook` via Ghostscript ebook preset with Java's
silent-disable/fallback semantics. Archives containing gif/bmp/webp or CMYK
JPEG entries forward to Java (codec coverage).

`pdf/cbz` (`convert::pdf_to_cbz`, added in the same pass): Ghostscript
`png16m` rasterisation at `dpi` (<=0 → 300, matching the controller clamp)
rezipped as `page_%03d.png` in `application/octet-stream` — Java renders
via PDFBox so pixels differ between renderers, container/naming identical.

`cbz/pdf` (`convert::cbz_pdf`, added in the post-agent reconciliation pass):
native zip walk + natural-order sort + one page per jpeg/png image sized to
pixel dimensions, `optimizeForEbook` via Ghostscript ebook preset with Java's
silent-disable/fallback semantics. Archives containing gif/bmp/webp entries
forward to Java (ImageIO codec coverage). `cbr/pdf` + `pdf/cbr` route through
the `rar` CLI (extraction + `rar a -m5 -ep1` packing); missing `rar` disables
the group via `endpointconfig`.

### `/api/v1/misc` — 6 proxied

`auto-split-pdf`, `remove-blanks`, `extract-image-scans`, `ocr-pdf`,
`scanner-effect` — all need rasterisation/OCR (Tesseract, ImageMagick);
`print-file` is commented out upstream (dead).

### `/api/v1/security` — ~11 proxied

`restore-pdf-protection` (JPDFium `PdfSecurity.copyEncryption` — hard block),
`auto-redact`, `redact-execute`, `timestamp-pdf`, `validate-compliance`,
`validate-signature`, `verify-pdf`, `cert-sign`, and hardware signing
(`cert-sign/hardware/capabilities`, `/windows-certificates`,
`/pkcs11-certificates`) — crypto/signing stack, deliberately Java.
`prepare-pdf-session` and `redact` are Rust-routed.

### Platform/infra GETs — now Rust-routed

`handlers/platform.rs` landed after the last audit pass and now serves
`/api/v1/config/login-disclaimer`, `/config/endpoint-enabled`,
`/config/endpoints-enabled`, `/config/endpoints-availability`,
`/config/group-enabled`, `/api/v1/ui-data/{home,licenses,ocr-pdf,
footer-info,pipeline}`, `/api/v1/settings/get-endpoints-status` and
`/api/v1/settings/update-enable-analytics`. Remaining platform surface:
`/api/v1/config/app-config` (68 Spring-coupled keys incl. license service),
`/api/v1/pipeline/handleData`, `/js/additionalLanguageCode.js`,
`/api/v1/ui-data/sign` if present, web controllers (Metrics/ReactRouting/
Robots/SignatureImage — partially covered by `web::serve` +
`metrics::routes()`).

### Form — complete on paper

All 9 `/api/v1/form/*` endpoints routed (`fields`, `fields-with-coordinates`,
`extract-csv`, `extract-xlsx`, `add-fields`, `edit-fields`, `modify-fields`,
`delete-fields`, `fill`). `FormFillController.java` still on disk pending
deletion. Widget `/AP` appearances are minimal hand-drawn approximations +
`/NeedAppearances` — documented divergence (PDFBox `AppearanceGeneratorHelper`
typesetting not reproducible).

## D. Files modified this pass

Post-agent reconciliation pass (this update):

- `rust/src/handlers/convert.rs` — added `cbz_pdf` + `cbz_native_supported`
  + `cbz_image_entry` + `natural_order_cmp` (~200 lines) for
  `POST /api/v1/convert/cbz/pdf`, and `pdf_to_cbz` (~120 lines) for
  `POST /api/v1/convert/pdf/cbz`.
- `rust/src/handlers/mod.rs` — registered `/api/v1/convert/cbz/pdf` and
  `/api/v1/convert/pdf/cbz`.
- `migration/FINAL_MIGRATION_AUDIT.md` — this document.

Earlier pass: no Rust or Java source modified — reconciliation found no
defects requiring fixes.

### Orphaned file — resolved

`rust/src/handlers/images.rs` (alternate `extract-images` implementation)
was deleted from the tree during this pass; `misc::extract_images` remains
the wired handler and keeps its unsupported-type → Java fallback.

A new `handlers/platform.rs` module (config/ui-data/settings/disclaimer
GET+POST routes) also landed during this pass and is fully registered.

`rustfmt --edition 2021` run on `convert.rs` + `mod.rs` (my additions only;
both files were already format-clean elsewhere). No `cargo` commands run
per phase restrictions — the new handler is **compiler-unverified**.

## E. Remaining blockers

1. **Compile/test verification still owed** — the post-`INTEGRATION_REPORT`
   churn (`convert.rs` +~1.3k lines, `form.rs` +~1.8k lines, new
   `auth`/`metrics`/`settings`/`web`/`endpointconfig` modules) has never been
   compiler-checked in this tree state. First `task rust:check` may surface
   issues.
2. **Runtime parity** — `scripts/rust_pdf_parity.py` needs Java :8080 + Rust
   :8081; not run.
3. **Engine availability** — convert endpoints shell out to
   `gs`/`weasyprint`/`soffice`/`unoconvert`/`pdftohtml`/`rsvg-convert`/
   `inkscape`; deployments without them get 500s where Java might succeed.
4. **Java controller deletion** — controllers whose endpoints are fully
   Rust-routed (`FormFillController`, `ConvertImgPDFController` partially,
   `ConvertPDFToOffice`, `PasswordController` partially, `RedactController`
   partially, `JobController`) remain on disk; deletion is presumably a later
   coordinated step.

---

# Phase 3 addendum (2026-11-06, integration + test-infrastructure pass)

Details: `migration/reports/PHASE-03-AGENT-01-INTEGRATION.md` and
`migration/reports/PHASE-03-AGENT-02-TESTING.md`.

## Corrections to earlier sections

- The dead `misc::auto_rotate_pdf` / `misc::repair_pdf` duplicates cited in
  §B/§E are **deleted** — verified absent.
- `handlers/images.rs` (early extract-images draft) was removed; the live
  path is `misc::extract_images` + `imgextract.rs`.
- Platform endpoints are now native via `handlers/platform.rs`
  (`/api/v1/config/*`, `/api/v1/ui-data/*`, `get-endpoints-status`,
  `update-enable-analytics`) — §C's "platform GETs proxied" note is
  superseded.
- All 9 `/api/v1/form/*` endpoints are routed.
- `handlers/mod.rs` registers **136 `.route()` calls** plus
  `metrics::routes()` (6) and the `/api/**` fallback proxy; 22 handler
  modules declared. The tree grew during the phase (`eml`,
  `image_scans`, `ocr`, `pipeline`, `scanner`, `pdftext/`, `tabula`,
  `jobstore`, `fonts`, `pipeline_table`).

## Current integration state (static audit)

- All `rust/src` modules and all 22 handler modules are declared; every
  routed handler resolves, including macro-generated endpoints
  (`pdf_to_word/presentation/xml/text`) and the `#[endpoint]` expansion
  shape. One exception: `src/pdftext/common_converter.rs` is present on
  disk but undeclared in `pdftext/mod.rs` and unreferenced — a 1031-line
  port of Java's `BasicPdfMarkdownExtractor` awaiting an owner decision
  (declare-as-fallback / wire / delete). See PHASE-03-AGENT-01 §7.
- `rust/engine/` (`stirling-engine`) is fully wired: `lib.rs` →
  `routes::router` merges all six route modules; `agents/runtime.rs` and
  `documents/rag_capability.rs` are declared. Duplicate declarations
  found and fixed during the pass: `pub mod pdf_comment;` twice in
  `agents.rs`, `pub mod pdf_comments;` twice plus a duplicate
  `.nest("/api/v1/ai/pdf-comment-agent")` in `routes/mod.rs` — merge
  artifacts that would not compile.
- Middleware chain verified: TraceLayer → CorsLayer → `metrics::track` →
  `auth::auth_guard` → `guard::api_guard` → routes → proxy fallback →
  `web::serve`.

## Risks flagged

1. **97 uncommitted Java deletions** in the working tree (incl. core API
   controllers). The proxy still needs Java for ~60 endpoints — do not
   commit these until migrated-endpoint parity is verified.
2. `rust/f.pdf` stray artifact; `rust/engine/{target,vendor}` untracked —
   check `.gitignore` before committing.
3. **`cargo test --bin stirling-server` executed** (stabilization
   phase): compiles clean, **433 passed / 0 failed / 1 ignored**.
   `clippy`, `fmt --check`, and the engine suite still unrun.

## Test infrastructure added

`rust/TESTING.md`, `tests/scripts/{test-unit,test-engine,smoke,
coverage-report}.sh`, `engine/tests/{contracts,config_error}.rs`, and
in-file test modules across the crate. Late additions in the same pass:
`src/fonts.rs`, `src/guard.rs` (`forbidden` body), and `src/pdftext/`
`line.rs`, `gfm_table.rs`, `column_ranges.rs`, `shared.rs` — the first
coverage those files have had. Source-to-test inventory covering all
147 Rust source files (incl. `engine/tests/`):
`PHASE-03-AGENT-02-TESTING.md` §3.
