# Java → Rust Migration Plan

Strangler-fig migration: a new Rust (axum) backend lives in `rust/` and
serves migrated endpoints while the Java/Spring backend keeps serving the
rest. The frontend keeps calling `/api/*`; routing decides who answers.

## Current state

- Java backend: `app/` (core/common/proprietary/saas modules), Spring Boot,
  ~2,500 files, ~110 controllers. Runs natively on `:8080`.
- Rust backend: `rust/` — axum server on `:8081` (`task rust:dev`),
  `/health`, `/api/v1/info/version`, `/api/v1/ping`. PDF ops are pure Rust
  on the `lopdf` object model via `src/pdfdoc.rs` — no native PDF library.
  Migrated so far:
  `general/merge-pdfs`, `general/rotate-pdf`, `general/remove-pages`,
  `general/rearrange-pages`, `general/split-pages`,
  `analysis/page-count`, `analysis/basic-info`, `analysis/page-dimensions`,
  `analysis/document-properties`, `analysis/form-fields`,
  `analysis/annotation-info`, `analysis/font-info`,
  `analysis/security-info`, `misc/decompress-pdf` (lopdf, object-level),
  `general/split-by-size-or-count`, `general/split-pdf-by-chapters`,
  `general/remove-image-pdf`, `misc/update-metadata`,
  `misc/show-javascript`, `security/sanitize-pdf`,
  `security/get-info-on-pdf`,
  `misc/list-attachments`, `misc/extract-single-attachment`,
  `misc/extract-attachments`, `misc/rename-attachment`,
  `misc/delete-attachment`, `general/pdf-to-single-page`,
  async job framework (`?async=true` on every migrated endpoint +
  `/api/v1/general/job/*`, `/api/v1/general/files/*`,
  `POST /api/v1/general/jobs/cleanup`; JSON results are stored inline like
  Java, file results via the files endpoints). Every other `/api/**` route falls through
  to the Java backend via the built-in proxy (`JAVA_BACKEND_URL`, default
  `http://localhost:8080`; `off` disables).
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
   └── everything else  → proxy to Java backend (:8080)
```

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
      option 2 topology — Rust is the single entry point)
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
- `remove-pages` skips `FormUtils.pruneOrphanedFormFields` — `pdfdoc::subset`
  rebuilds the page tree but does not prune orphaned AcroForm fields.
- `merge-pdfs` doesn't convert image inputs to PDF pages yet (Java does via
  `PdfUtils.imageToPdf`), rejects `generateToc`/`removeCertSign=false`, and
  uses page import, so document-level structures are not carried over.
- `misc/decompress-pdf` — done via `lopdf`; per-stream failures are skipped
  like Java, but lopdf's save may normalise the xref table/object streams
  differently.
- `general/split-by-size-or-count` — done; size mode probes real saved bytes
  like Java, but outline destinations are not retargeted and orphaned
  fields/widgets are not pruned.
- `general/split-pdf-by-chapters` — done via lopdf outline walk + `/Info`
  copy for `includeMetadata`; `split-pdf-by-sections` not ported (needs page
  content clipping, now assessed below).
- `general/scale-pages`/`general/crop` — done by wrapping each transformed
  page's content in `q … cm`/`re W n … Q` and rewriting its page box, not by
  PDFBox `LayerUtility` Form XObjects. Transformed pages lose annotations;
  `scale-pages` KEEP sizes every page to the first page's MediaBox like Java;
  `crop` is clip-only (`autoCrop` rejected, `removeDataOutsideCrop` does not
  shell to Ghostscript, so out-of-crop data remains in the stream).
- `misc/flatten` handles only `flattenOnlyForms=true` natively; every other
  flatten request is forwarded to Java. `misc/compress-pdf` uses
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
- Backend is pure Rust (`lopdf` object model in `pdfdoc.rs`); PDFium is
  gone from the server. Behavioural differences vs PDFBox are being ironed
  out per endpoint.
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
  `split-by-size-or-count`, and `split-pdf-by-chapters` rebuild the page tree
  with `pdfdoc::subset`, so they keep catalog extras, `/Info`, AcroForm and
  outlines — but outline destinations are not retargeted to the new page
  order and orphaned fields/widgets are not pruned. `merge-pdfs` still drops
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
| split-pdf-by-sections | Medium | Same page-as-Form-XObject + clip machinery as crop/scale; page selection and ZIP/PDF output already exist. Needs a lopdf page→form importer (content + resources + BBox/Matrix) and per-section page trees; watch rotation/crop-box offsets and annotation semantics. |
| pdf-to-single-page | Done | `pdfdoc::to_single_page` ports the `LayerUtility` Form XObject placement |
| scale-pages | Done | Implemented as in-place content wrap (`q translate/scale cm … Q`) + MediaBox rewrite; KEEP uses the first page's size for all pages like Java. Divergence: no Form XObject/resource import, annotations on transformed pages are dropped, CropBox/Rotate edge cases are not PDFBox-identical. |
| crop | Done (manual) | Manual crop clips selected pages (`re W n`) and rewrites MediaBox. `autoCrop` is rejected (needs rasterisation); `removeDataOutsideCrop=true` does not invoke Ghostscript, so it is clip-only rather than Java's GS data-removal path when enabled. |
| multi-page-layout, poster-pdf, booklet-imposition | Hard | N-up imposition = page cloning + transforms; lopdf Form XObjects/content ops make it possible, but matrix/rotation/crop math and document-structure preservation are substantial. |
| overlay-pdfs, edit-table-of-contents | Hard | Overlay needs resource-name collision handling and optional-content/layer semantics; TOC editing needs outline-tree writes. lopdf has outline/bookmark APIs, but parity is unproven. |

#### Batch C — MiscApi (21 controllers)

| Endpoint(s) | Feasibility | Notes |
|---|---|---|
| update-metadata | Done | lopdf `/Info` dictionary writes are in `misc.rs`; custom-key/date/trapped parity is covered by the parity harness. |
| auto-rename | Easy | Filename-only change driven by info reads; needs the same `/Info` + XMP fallbacks Java uses. |
| extract-images, extract-image-scans | Medium | lopdf exposes image XObjects and stream metadata; extraction is object walking, but scan detection and Java's naming/ordering parity still need porting. |
| flatten, remove-images, decompress-pdf | Done/Partial | `decompress-pdf` is object-level; `flatten` is native only for `flattenOnlyForms=true` and forwards other modes to Java; `remove-images` is covered by `general/remove-image-pdf`. Visual flatten parity still needs the parity harness. |
| blank-page (add/remove), page-numbers, add-stamp, add-watermark, overlay-image, scanner-effect | Medium-Hard | lopdf can author content streams/Form XObjects, so stamps/watermarks/page numbers are feasible. Blank-page add/remove is page-tree work; blank detection, scanner effect, and overlay-image compositing need rendering or careful resource math. |
| compress-pdf | Done | Uses Ghostscript/qpdf sidecars when present plus the lopdf image-recompression pass; `lineArt=true` still forwards to Java. Golden-file parity is required because sidecar versions change bytes. |
| auto-rotate-pdf, ocr-pdf, auto-split-pdf | Hard | Need text/layout heuristics or engine/Tesseract sidecar; lopdf has text extraction but not renderer-grade geometry. |
| attachments (list/extract/rename/delete) | Done | Name-tree walk + `/EF` stream resolution ported in `attachments.rs`; `add-attachments`, `batch-process-attachments` and PDF/A-3b conversion stay Java. |
| repair, show-javascript, print-file, mobile-scanner, portfolio, login-disclaimer, config | Mixed | Several are not pure PDF ops — evaluate individually; some may be thin proxies that stay Java. |

#### Batch D — SecurityApi (11 controllers)

| Endpoint(s) | Feasibility | Notes |
|---|---|---|
| get-info-on-pdf | Medium | Huge Java method (VeraPDF compliance, outlines, structure tree, permissions). Port the subset lopdf covers; compliance checks need veraPDF sidecar or stay Java |
| sanitize-pdf | Done | All six flags are dictionary surgery on lopdf; field `/AA` walks `/Kids` with a depth cap |
| add/remove-password, unlock-pdf-forms, validate, security-info extras | Medium | lopdf exposes `/Encrypt` metadata and decryption helpers; password *removal*/permission writes need the standard security handler — verify lopdf's encryption write path before porting |
| redact, timestamp-pdf, cert-sign, hardware-signing | Hard | Redaction = content removal (hard in lopdf); timestamp needs an RFC 3161 TSA client; cert-sign needs CMS signature generation — evaluate `cms`/`x509-cert` crates or keep Java |

#### Batch E — ConvertApi (8 controllers)

All need non-lopdf machinery; plan is sidecar processes, not pure Rust:

- img/pdf, svg/pdf, pdf/img, pdf/vector — `image` crate + lopdf page
  creation from images; vector export needs a renderer/SVG path or stays Java
- pdf/html, html/pdf, url/pdf, pdf/text, markdown/pdf, ebook/pdf, file/pdf,
  eml/pdf — weasyprint/chromium sidecar or engine; decide per endpoint
- pdf/word, pdf/xlsx, pdf/csv, extract-csv/xlsx — table extraction; engine
  or dedicated crates
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

- Frontend points at Rust only; remove proxy fallback.
- Delete `app/` Gradle modules, `build.gradle`, `gradlew`, `.taskfiles/backend.yml`.
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
