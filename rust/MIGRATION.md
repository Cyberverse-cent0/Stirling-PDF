# Java → Rust Migration Plan

Strangler-fig migration: a new Rust (axum) backend lives in `rust/` and
serves migrated endpoints while the Java/Spring backend keeps serving the
rest. The frontend keeps calling `/api/*`; routing decides who answers.

> **Phase 3 status (2026-11-06):** integration audit complete — all modules
> declared, 116 routes registered, no orphaned handlers
> (`migration/reports/PHASE-03-AGENT-01-INTEGRATION.md`). Test
> infrastructure in place: `rust/TESTING.md`, per-file unit tests,
> `tests/scripts/{test-unit,test-engine,smoke,coverage-report}.sh`,
> `engine/tests/contracts.rs` (`PHASE-03-AGENT-02-TESTING.md`). Nothing was
> compiled this phase — first `task rust:check` is the acceptance gate.

## Current state

- Java backend: `app/` (core/common/proprietary/saas modules), Spring Boot,
  ~2,500 files, ~110 controllers. Runs natively on `:8080`.
- Rust backend: `rust/` — axum server on `:8081` (`task rust:dev`),
  `/health`, `/api/v1/info/version`, `/api/v1/ping`. PDF ops are pure Rust
  on the `lopdf` object model via `src/pdfdoc.rs` — no native PDF library.
  Platform layer in progress: `settings.rs` (settings.yml),
  `endpointconfig.rs` (endpoint enable/disable), `auth.rs` +
  `guard.rs` middleware, `metrics.rs` (`/api/v1/info/*`), `web.rs`
  (SPA static serving), `convert.rs` (external-engine runner).
  Registered endpoint families (~110 route paths + 5 metrics routes):
  all of `general` (~19), `analysis` (8), `filter` (6), `form` (9),
  most of `misc` (~25, incl. mobile-scanner ×6), `security` password +
  sanitize/info/redact-subset, `convert` (20: img/pdf, pdf/img via
  Ghostscript, svg, office, html/markdown/url both ways, vector both
  ways, pdf→word/presentation/xml/text, cbz + cbr both ways,
  ebook/pdf, pdf/epub, eml/pdf, pdf/pdfa, pdf/csv, pdf/xlsx),
  `transform`, `layout`, `split`,
  `visual`, `automation`, job/file framework, plus platform GETs
  (`app-config`, `ui-data/*`, `group-enabled`, endpoint status),
  `convert/pdf/markdown` and `security/auto-redact` +
  `security/redact-execute`. The jpdfium text/layout/Markdown surface
  is ported in `src/pdftext/` (structured text model, glyph/line
  stitching, column layout, ruled + word-grid table detection, GFM
  rendering, heading/paragraph assembly, image placeholders, AcroForm
  values, text-match redaction) — no PDFium native dependency remains
  on these paths; verified surgical removal is replaced by the overlay
  path Java itself falls back to.
  Engine presence is probed at startup (`endpointconfig::probe_engines`,
  mirroring Java's `ExternalAppDepConfig`) — a missing sidecar disables
  its tool group and the guard returns 403. Verified status and the
  remaining-work list live in `migration/reports/FINAL_COMPLETION_AUDIT.md`,
  `migration/reports/REMAINING_WORK.md`, and the per-endpoint engine/param
  audit in `migration/reports/P3-CONVERSION-READINESS.md`.
  **As of 2026-11-06 the crate compiles clean** — `cargo check`,
  `cargo clippy --all-targets --all-features -- -D warnings`,
  `cargo fmt --check`, and 305 `cargo test` cases all pass. The
  earlier 23-error state is resolved; see `migration/reports/REMAINING_WORK.md`.
  **Parity caveat:** the Java controllers for migrated endpoints are
  deleted in the working tree, so `testing/parity.sh` can no longer
  diff them (Java 404s). Migrated endpoints now rely on Rust unit
  tests + smoke checks; use a merge-base Java build if a live diff is
  needed.
- Runtime sidecars are probed at startup (`endpointconfig::ENGINE_PROBES`)
  and a missing or under-versioned binary disables its tool group
  (guard → 403): `gs` (pdf/img, compress, pdfa), `qpdf` (optional
  normalisation), `soffice`/`unoconvert` (office↔pdf), `weasyprint`
  (≥58; html/markdown/url/eml→pdf), `pdftohtml`, `tesseract` +
  `ocrmypdf` (ocr-pdf), `rar` (cbr), `magick`, `ebook-convert`
  (ebook/pdf, pdf/epub).
  `verapdf` is an optional validator, not a
  converter — it is deliberately not probed/gated; the PDF/A endpoint
  uses it for `strict` enforcement and warn-only checks when present.
  `get-info-on-pdf` compliance fields are computed locally (no veraPDF);
  the Java compliance service is gone with the proxy.
- The Java proxy is removed: `handlers/proxy.rs`, `JAVA_BACKEND_URL`,
  and `AppState.backend` are deleted. Unmigrated `/api/**` routes return
  404; modes needing a PDF renderer or an unported engine return
  explicit `400`/`500` errors instead of forwarding. Remaining
  unimplemented surface: the security crypto family (cert-sign,
  hardware-signing, timestamp-pdf, validate-signature, verify-pdf,
  validate-compliance, restore-pdf-protection) and all proprietary/SaaS
  controllers.
- The PDFium toolchain is fully removed: no `pdfium-render` dependency,
  no `libpdfium` binary, no C headers, no standalone bindings crate.
  All PDF work runs on `lopdf` via `src/pdfdoc.rs`.
- Docker assets archived under `runtime/` (see `runtime/README.md`).

## Target architecture

```
frontend (Vite :5173)
   │ /api/*
   ▼
Rust gateway (:8081 or :8080)
   ├── migrated endpoints → rust handlers (lopdf via pdfdoc.rs)
   └── everything else  → 404 for /api/**, SPA/static for the rest
```

The strangler proxy (option 2 below) was used during migration and has
been removed: unmigrated `/api/**` paths now 404 rather than forwarding
to Java.

Two cutover options, decide early:

1. **Frontend proxy split** — Vite/nginx routes migrated paths to Rust,
   rest to Java. Zero changes to Rust; simplest while both run.
2. **Rust reverse proxy** — Rust axum app proxies unmigrated routes to
   Java (`reqwest` + `tower-http` proxy). Single entry point, matches the
   final state where Java is deleted.

Option 2 is recommended: the proxy layer is ~50 lines and becomes the
production topology later.

## Phases

### Phase 0 — Foundations (this branch)

- [x] Java backend builds and runs natively (`./gradlew :stirling-pdf:bootRun`)
- [x] Rust skeleton compiles, clippy-clean, serves `/health`
- [x] Docker assets archived to `runtime/`
- [x] Drop the native PDF dependency — all server handlers run on `lopdf`
      (`src/pdfdoc.rs`); `pdfium-render` and `src/pdf.rs` are gone
- [x] Add Rust→Java fallback proxy for `/api/v1/**` (`handlers/proxy.rs`,
      option 2 topology — Rust is the single entry point); **removed** —
      proxy deleted, unmigrated `/api/**` 404s
- [x] Add `task rust:*` to CI (`rust-server` job in `build.yml`, gated by
      the `rust-server` paths filter)

### Phase 1 — Pilot endpoints (pure PDF ops, no auth/state)

Pick 3–5 stateless endpoints that map cleanly onto the `lopdf` object model:

- `POST /api/v1/general/merge-pdfs` — done (subset: `generateToc` and
  `removeCertSign=false` rejected; image inputs not yet converted)
- `POST /api/v1/general/rotate-pdf` — done
- `POST /api/v1/general/remove-pages` — done (`pageNumbers` syntax ported
  in `page_select.rs`, incl. `all` and `an+b` n-functions)
- `POST /api/v1/general/rearrange-pages` — done (all `customMode` presets)
- `POST /api/v1/general/split-pages` — done (ZIP output)
- `POST /api/v1/analysis/*` — all eight endpoints done

Known divergences so far:

- `document-properties` returns raw PDF date strings
  (`D:YYYYMMDDHHmmSSOHH'mm'`) where Java prints `Calendar.toString()`.
- `merge-pdfs` doesn't convert image inputs to PDF pages yet (Java does via
  `PdfUtils.imageToPdf`), rejects `generateToc`/`removeCertSign=false`, and
  uses page import, so document-level structures are not carried over.
- `misc/decompress-pdf` — done via `lopdf`; per-stream failures are skipped
  like Java, but lopdf's save may normalise the xref table/object streams
  differently.
- `general/split-by-size-or-count` — done; size mode probes real saved bytes
  like Java, but outline destinations are not retargeted.
- `general/split-pdf-by-chapters` — done via lopdf outline walk + `/Info`
  copy for `includeMetadata`; `split-pdf-by-sections` not ported (needs page
  content clipping, now assessed below).
- `general/scale-pages`/`general/crop` — done by wrapping each transformed
  page's content in `q … re W n … cm … Q` and rewriting its page box. For
  `scale-pages` the `cm` is `translate·scale` composed with the same
  `/Matrix` `LayerUtility.importPageAsForm` computes (shared
  `pdfdoc::form_matrix`, also used by pdf-to-single-page), so `/Rotate` is
  baked into the content and the view box is clipped like the form's
  BBox; the output page gets explicit `CropBox == MediaBox` and
  `Rotate == 0` like Java's fresh `PDPage`. `scale-pages` KEEP sizes each
  page to its own MediaBox (mixed-size documents stay mixed), matching
  `ScalePagesController.ownSize`. `crop` is clip-only (`autoCrop`
  rejected, `removeDataOutsideCrop` does not shell to Ghostscript, so
  out-of-crop data remains in the stream).
- `misc/flatten` handles only `flattenOnlyForms=true` natively; other
  flatten modes rasterise every page and are rejected. `misc/compress-pdf`
  uses
  Ghostscript/qpdf sidecars when present plus the lopdf image pass, so bytes
  will differ from Java even when size targets match.
- `general/pdf-to-single-page` — done on lopdf via `pdfdoc::to_single_page`:
  each source page becomes a Form XObject with the same `/Matrix` math as
  PDFBox's `LayerUtility.importPageAsForm` (raw crop box as viewBox, no
  media clipping), placed by `LayerN` OCG marked content, plus the `/Info`
  carry-over of `createNewDocumentBasedOnOldDocument` (dates re-zoned to
  the system zone like Java's `Calendar` round-trip). Producer/Creator
  stamp `Stirling-PDF v<ver>` from `STIRLING_VERSION` (fallback: the crate
  version, not the app version — set it for byte-level metadata parity).
  Divergences: annotations are dropped, matching Java — `importPageAsForm`
  wraps only the content stream, so widget/link annots never reach the
  output; the source catalog's `/OCProperties` is not merged in (Java's
  `importOcProperties` is), only the fresh `LayerN` groups are written;
  form streams are stored uncompressed where PDFBox flate-encodes them.
- `convert/pdf/pdfa` — done on the Ghostscript path. `outputFormat`
  accepts Java's full token set (`pdfa`, `pdfa-1/2/2b/3/3b`, `*-a`
  variants, `pdfx*`); unknown values fall back to PDF/A-2b like Java's
  `orElse`. The handler writes `PDFA_def.ps` + the same sRGB2014/Gray
  ICC profiles Java ships, runs an optional qpdf pre-clean, then gs with
  Java's exact argv (`-dPDFA`, `-dPDFACompatibilityPolicy=1`,
  `-dCompatibilityLevel`, `pdfwrite`, output intents, 30-min timeout).
  Divergences: level-A requests produce level-B output under a `_b`
  suffix, matching OSS Java which lacks the proprietary tagger;
  Java's ~1,500-line PDFBox sanitizer before gs is not ported (gs does
  its own repair); `strict=true` is native — the Ghostscript output is
  validated by the `verapdf` CLI (`--flavour <part><level>`, resolved via
  `customPaths.operations`): non-compliant → 400 with the report
  (Java `verifyStrictCompliance`), CLI absent + strict → 500,
  non-strict → warn-and-serve (Java `validateAndWarnPdfA`). PDF/X
  strict validation is not ported (400). `useStirlingOfficeConvert=true`
  and DOCX inputs are rejected (Office Convert / OOXML external-reference
  sanitising have no Rust port); a gs failure is an internal error —
  the PDFBox/LibreOffice fallback chain is gone with the proxy. Output
  naming/Content-Disposition match Java (`_PDFA-<part><conf>.pdf`,
  `_PDFX.pdf`). Requires the `gs` binary; without it (and without a
  Java backend) the endpoint 404s rather than 403s, because
  `pdf-to-pdfa` sits in Java's LibreOffice/Convert groups, not the
  Ghostscript group.
- Backend is pure Rust (`lopdf` object model in `pdfdoc.rs`); PDFium is
  gone from the server. Behavioural differences vs PDFBox are being ironed
  out per endpoint.
- `security/get-info-on-pdf` — done; all nine sections ported. Divergences:
  no veraPDF (every `IsPDF/*Compliant` is false, no `Compliance` array or
  per-standard keys; `IsPDF/SECCompliant` is computed locally), text counts
  come from lopdf's content reader not `PDFTextStripper`, `XMPMetadata` is
  the raw stream not XMP-reserialised, AcroForm `FormFields` walks `/Fields`
  with `/V` values approximated per type.
- `security/sanitize-pdf` — done; all six flags ported (JS actions incl.
  catalog/page/field `/AA` and widget `/A`, embedded files + FileAttachment
  annots, catalog `/Metadata`, empty `/Info`, Launch/URI link actions,
  page `/Font` removal via nearest resources holder).
- `security/remove-cert-sign` — done; root-level `/FT /Sig` fields are
  flattened like `PDAcroForm.flatten(fields, false)` (widget appearances
  drawn, annots removed), dropped from `/Fields`, `/XFA` removed, and
  `/SigFlags` cleared only when no `/V` signature dictionary remains in
  the tree. Divergence: nested (non-root) signature fields are kept, same
  as Java's `getFields()` filter; direct-dict widgets can't match page
  annots (same identity limit as `flatten()`).
- Async jobs are in-memory only (lost on restart), have no queue/resource
  weighting, no progress %, no timeout, `createdAt`/`completedAt` are epoch
  seconds rather than Java's ISO datetimes, and cleanup removes *all*
  finished jobs rather than applying a retention policy.
- `form-fields.fieldCount` counts widget annotations, not top-level fields;
  `hasXFA` reads the AcroForm `/XFA` entry directly.
- `font-info.fonts` lists declared `/BaseFont` names (including unused
  fonts, excluding form-XObject fonts), not resource keys or used fonts.
- `security-info` omits `keyLength`; permissions come from the raw `/P`
  bits of the `/Encrypt` dictionary.
- `rearrange-pages`, `remove-pages`, `split-pages`,
  `split-by-size-or-count`, `split-pdf-by-chapters`, and
  `split-pdf-by-sections` rebuild the page tree with `pdfdoc::subset`, so
  they keep catalog extras, `/Info`, AcroForm and outlines. The four
  endpoints Java runs `FormUtils.pruneOrphanedFormFields` on
  (remove-pages, split-pages, split-by-size-or-count,
  split-pdf-by-chapters) call `pdfdoc::prune_orphaned_form_fields`;
  rearrange-pages matches Java in not pruning. Outline destinations are
  not retargeted to the new page order. `merge-pdfs` still drops
  document-level structures because it uses page import. Page cap is Java's
  100,000.

For each: port request/response DTOs exactly (check
`app/core/.../controller/api/*Controller.java` + frontend caller in
`frontend/src`), implement handler in `rust/src/handlers/<domain>.rs`,
add route, verify semantic/structural parity vs Java on the parity harness
(`scripts/rust_pdf_parity.py`) before flipping the route.

### Phase 2 — Core PDF operation families

Group the ~110 controllers into families and migrate family-by-family.
Ordering principle: cheapest parity wins first; anything needing content-stream
surgery, rasterisation, OCR, or external binaries is flagged and scheduled
last within its family.

**Done:** merge-pdfs, rotate-pdf, remove-pages, rearrange-pages (all
customMode presets), split-pages, split-by-size-or-count, split-pdf-by-chapters,
scale-pages, crop, remove-image-pdf; all eight analysis endpoints;
misc/decompress-pdf, misc/update-metadata, misc/show-javascript,
misc/flatten (forms-only), misc/compress-pdf; security/sanitize-pdf.

#### Batch B — GeneralApi remainder

| Endpoint(s) | Feasibility | Notes |
|---|---|---|
| split-pdf-by-sections | Done | Page selection + ZIP/PDF output via `pdfdoc::subset`; per-section clipping shares the crop/scale content-wrap machinery |
| pdf-to-single-page | Done | `pdfdoc::to_single_page` ports the `LayerUtility` Form XObject placement |
| scale-pages | Done | In-place content wrap (`q clip+cm … Q`) + MediaBox rewrite; the `cm` reuses `pdfdoc::form_matrix`, the `LayerUtility.importPageAsForm` matrix, so rotation/crop-box geometry matches Java. KEEP resizes each page to its own size. Divergence: no Form XObject/resource import, Link/Widget annotations are kept and transformed (Java drops all annots except flattened stamp appearances), other annotation subtypes are dropped. |
| crop | Done (manual) | Manual crop clips selected pages (`re W n`) and rewrites MediaBox. `autoCrop` is rejected (needs rasterisation); `removeDataOutsideCrop=true` does not invoke Ghostscript, so it is clip-only rather than Java's GS data-removal path when enabled. |
| multi-page-layout, poster-pdf, booklet-imposition | Done | `layout.rs` ports the N-up/saddle-stitch math; poster splitting in `split.rs`. See agent-01 report for divergences (OCG/layer naming, form XObject import path) |
| overlay-pdfs, edit-table-of-contents | Done | `general::overlay_pdfs` + `edit_table_of_contents` (outline write via `pdfdoc::write_outline`). See agent-02 report for resource-collision and outline-parity notes |

#### Batch C — MiscApi (21 controllers)

| Endpoint(s) | Feasibility | Notes |
|---|---|---|
| update-metadata | Done | lopdf `/Info` dictionary writes are in `misc.rs`; custom-key/date/trapped parity is covered by the parity harness. |
| auto-rename | Easy | Filename-only change driven by info reads; needs the same `/Info` + XMP fallbacks Java uses. |
| extract-images | Partial | Native for the `image` crate's codec set (png, jpeg/jpg, tiff, bmp, webp, gif); PDF image types outside `imgextract`'s decode subset (JPX/CCITT/JBIG2, Indexed/ICCBased colour spaces, `/Decode`, soft masks) are rejected with a 400 rather than silently dropped. extract-image-scans is separately ported in `handlers::image_scans` (OpenCV script, not XObject extraction) |
| flatten, remove-images, decompress-pdf | Done/Partial | `decompress-pdf` is object-level; `flatten` is native only for `flattenOnlyForms=true` — other modes rasterise every page and are rejected; `remove-images` is covered by `general/remove-image-pdf`. Visual flatten parity still needs the parity harness. |
| blank-page (add/remove), page-numbers, add-stamp, add-watermark, overlay-image | Medium-Hard | lopdf can author content streams/Form XObjects, so stamps/watermarks/page numbers are feasible. Blank-page add/remove is page-tree work; blank detection and overlay-image compositing need rendering or careful resource math. |
| scanner-effect | Done | `handlers::scanner`: Ghostscript raster + full pixel pipeline (gradient border, rotation, feather, box blur, grade) ported from `ScannerEffectController`; sequential vs Java's ForkJoinPool, `system.maxDPI` unported (500 default). |
| compress-pdf | Done | Uses Ghostscript/qpdf sidecars when present plus the lopdf image-recompression pass; `lineArt=true` (ImageMagick line-art service) is rejected. Golden-file parity is required because sidecar versions change bytes. |
| auto-rotate-pdf, auto-split-pdf | Done | `auto-rotate-pdf` uses the positioned text extractor; `detectionMode=osd` (Tesseract OSD over rasters) is rejected; `auto-split-pdf` decodes Stirling QR dividers from embedded images or Ghostscript rasters. |
| ocr-pdf | Done | `handlers::ocr`: OCRmyPDF sidecar preferred, per-page Tesseract fallback at 300 DPI; tessdata language filtering and exit-code mapping match `OCRController`. |
| extract-image-scans | Done | `handlers::image_scans` + `image_split`: native port of `split_photos.py` (background median, inRange mask, 5×5 dilate, 8-conn components + Moore-traced contour area, Canny→Hough median-angle deskew, bicubic replicate warp). No Python/cv2 needed; endpoint removed from the `Python`/`OpenCV` groups. PDF pages still rasterise via Ghostscript at `min(300, system.maxDPI)` DPI, with per-page rasterisation and per-image splitting fanned out on scoped threads (page order preserved). Image codecs: jpeg/png/gif/bmp/webp/tiff/pnm/hdr/tga/dds/exr/ico/qoi/ff — heic/avif/jp2 are the remaining gap vs cv2's `imread`. Known approximations: `minArea`/`minContourArea` request params are ignored (the Python script never forwarded them — its 10000/500 defaults always applied, reproduced here); Canny skips cv2's internal smoothing; `contourArea` is a traced-polygon area. |
| attachments (list/add/extract/rename/delete) | Done | Name-tree walk + `/EF` stream resolution ported in `attachments.rs`; `batch-process-attachments` and `convertToPdfA3b` (PDF/A-3b attachment embedding) are rejected. |
| repair, show-javascript, print-file, mobile-scanner, portfolio, login-disclaimer, config | Mixed | Several are not pure PDF ops — evaluate individually; some may be thin proxies that stay Java. |

#### Batch D — SecurityApi (11 controllers)

| Endpoint(s) | Feasibility | Notes |
|---|---|---|
| get-info-on-pdf | Medium | Huge Java method (VeraPDF compliance, outlines, structure tree, permissions). Ported; veraPDF `IsPDF/*Compliant` checks are computed locally and report false rather than consulting a validator |
| sanitize-pdf | Done | All six flags are dictionary surgery on lopdf; field `/AA` walks `/Kids` with a depth cap |
| add/remove-password, unlock-pdf-forms, validate, security-info extras | Medium/Done | `add-password`, `remove-password`, `prepare-pdf-session` are native in `handlers::passwords` (lopdf encrypt/decrypt + permission flags); `restore-pdf-protection` (jpdfium `copyEncryption`) and `unlock-pdf-forms`' Java-only paths are unrouted → 404 |
| redact, timestamp-pdf, cert-sign, hardware-signing | Partial/Hard | `redact`, `auto-redact`, `redact-execute` are native (`pdftext::redact` matcher + overlay rectangles; `convertPDFToImage` rasterises via Ghostscript at 300 DPI like Java). `timestamp-pdf` (RFC 3161 TSA), `cert-sign` (CMS), `hardware-signing`, `validate-signature`, `verify-pdf`, `validate-compliance` are unrouted → 404 |

#### Batch E — ConvertApi (8 controllers)

All need non-lopdf machinery; plan is sidecar processes, not pure Rust:

- img/pdf, svg/pdf, pdf/img, pdf/vector — `image` crate + lopdf page
  creation from images; vector export needs a renderer/SVG path or stays Java
- pdf/html, html/pdf, url/pdf, pdf/text, markdown/pdf, ebook/pdf, file/pdf,
  eml/pdf — weasyprint/chromium sidecar or engine; decide per endpoint
- pdf/word — officeconvert/LibreOffice sidecar; pdf/xlsx, pdf/csv —
  done natively (`src/tabula.rs` ruling detection → CSV/zip or a
  hand-built OOXML workbook, no engine); extract-csv/xlsx still open
- pdf/video — ffmpeg sidecar
- office conversions (docx/pptx/xlsx → pdf) — LibreOffice headless sidecar
  (already a project dependency for the Java backend)

#### Batch F — FilterApi + pipeline/job infrastructure

- filter-* endpoints are batch wrappers over the ops above — cheap once the
  underlying ops exist
- **`?async=true` job framework is a prerequisite for UX parity**:
  `AutoJobPostMapping` routes long ops through `JobExecutorService` with
  `/api/v1/general/job/{id}` status polling. Rust needs an in-process job
  table (SQLite or in-memory + tokio tasks) before heavy endpoints feel
  equivalent. Do this *before* Batch C/D, not after.

#### Batch G — Platform services (Spring-heavy, decide per service)

Auth (API keys, sessions, license), settings (`configs/settings.yml`
parity), pipeline/workflow engine, admin, UI-data bootstrap, notifications,
audit. Candidates: Rust handlers over SQLite/SQLx, or keep Java behind the
proxy indefinitely for the self-hosted core. Proprietary/SaaS modules
(billing, storage, MCP, policy, account-link) are the largest surface —
recommend they stay Java longest; the strangler proxy already covers them.

#### Cross-cutting prerequisites (do once, early)

1. **Async job framework** (see Batch F) — blocks parity on heavy endpoints.
2. **AuthN/Z layer** — API keys, session cookies, license gating currently
   enforced by Spring filters; Rust must replicate or the proxy must keep
   forwarding auth'd routes.
3. **Settings reader** — `configs/settings.yml` + env parity.
4. **Multipart limits** — mirror `MultipartConfiguration` (~2GB, temp-file
   spilling; current handlers buffer uploads in memory).
5. ~~Per-platform libpdfium binaries~~ — resolved: backend is pure Rust,
   no native libs to bundle per platform.
6. [x] **Golden-file parity tests** — `scripts/rust_pdf_parity.py` drives both
   backends over `app/**/src/test/resources` PDFs (plus `testing/` with
   `--include-testing`), fingerprints outputs with
   `rust/examples/pdf_fingerprint.rs` (`lopdf` object model), and reports
   semantic vs structural diffs. Byte equality is not expected; run it before
   flipping each route.

Each migrated endpoint still follows: DTO parity → handler → golden-file
test → route flip. The proxy makes every flip safe and reversible.

### Phase 3 — Platform services

Auth, settings, pipeline/workflow engine, admin, billing/SAAS, audit,
notifications. These are Spring-heavy (JPA/H2, security filters) — evaluate
per-service: some may stay Java longest or be reimplemented in Rust with
SQLite/SQLx.

### Phase 4 — Cutover and cleanup

- [x] Frontend points at Rust only; proxy fallback removed (`handlers/proxy.rs`
      deleted, unmigrated `/api/**` → 404).
- [ ] Delete `app/` Gradle modules, `build.gradle`, `gradlew`, `.taskfiles/backend.yml`.
- Keep `engine/` (Python) — it proxies through whichever backend serves
  `/api/v1/ai-engine/*`.

## Cross-cutting concerns

- **API contract**: Java controllers are the spec. Generate Rust DTOs from
  the OpenAPI doc (`task backend:swagger` → `SwaggerDoc.json`) instead of
  hand-porting types — `engine/` already does this for Python via
  `tool_models.py`; do the same for Rust (e.g. `typify` or `openapi-generator`).
- **File handling**: Java uses multipart + temp files with size limits
  (`MultipartConfiguration`, ~2GB). Mirror limits in axum (`DefaultBodyLimit`,
  `multer`).
- **Config**: Java reads `configs/settings.yml` + env vars. Rust side needs
  a settings reader that understands the same keys during transition.
- **Error shape**: match Java's error JSON so the frontend doesn't fork.
- ** Licensing/flavors**: proprietary/saas modules gate features; decide
  whether Rust serves all flavors or only core first.

### Parity harness

`testing/parity.sh` compares migrated endpoints Java↔Rust live: JSON
responses via `jq -S`, PDFs via `rust/examples/pdfdiff` (structural
compare — page count, MediaBox/CropBox/Rotate, /Info keys, outline and
annotation counts, encryption flag; byte equality is not expected across
writers). Requires `task backend:dev` (:8080) and a Rust instance
(`PORT=8082 task rust:dev` to avoid clobbering :8081);
`JAVA_URL`/`RUST_URL` env vars override. Run `testing/parity.sh [filter]`
after every migrated batch — a divergence is a decision to record here,
not silently accept.

## Risks

- lopdf covers the object model but NOT everything PDFBox does (content-stream
  surgery, PDF/A, advanced forms, rendering). Gap list must be produced per
  endpoint before porting — fallback: keep that endpoint on Java until a Rust
  path exists.
- Performance: lopdf is pure Rust, but Stirling's Java code has tuned
  workarounds; benchmark merge/split early.
- Native sidecars (Ghostscript/qpdf/LibreOffice/Tesseract) still carry their
  own licensing and ops burden where an endpoint needs them.

## AI engine migration (`engine/` → `rust/engine/`)

`rust/engine/` is an independent crate replacing the Python FastAPI AI
engine. Same HTTP contract, same `STIRLING_*` env vars, same default port
5001. Tracked phases live in the migration plan; per-crate status and
known divergences are in `rust/engine/README.md`.

Done so far: HTTP scaffold + auth middleware, full serde contract port
(all contracts also derive `schemars::JsonSchema` for the capabilities
manifest), character-exact chunking/docparse packing (regression-tested
against Python outputs), OpenAI-wire embedder, sqlite-vec + pgvector
document stores, DocumentService, and the `/health`, `/api/v1/documents`,
`/api/v1/docparse`, `/api/v1/ai/pdf-comment-agent`,
`/api/v1/documents/classify`, `/api/v1/agents/capabilities` and
`/api/v1/agents/next-action` routes. sqlite-vec is vendored
(`vendor/sqlite-vec.c`) because the crates.io package is broken. Tool
parameter types generate via typify:
`rust/engine/scripts/generate_tool_params.sh`.

Remaining: agent runtime (tool-calling loop, `NativeOutput`/`ToolOutput`
modes, pydantic-ai message history, `MockModel`), remaining agents and
routers (orchestrator NDJSON streaming, pdf-edit/questions, agent-drafts,
execution, ledger/math-auditor, contradiction, config push/cache),
PostHog capture, the pytest port, and the parity harness before Python
removal.
