# Agent 08 — Convert API: Java → Rust migration report

Scope: the `@ConvertApi` controllers under
`app/core/src/main/java/stirling/software/SPDF/controller/api/converters/`
(all routes live under `/api/v1/convert/**`) plus the proprietary
`ConvertPdfToPdfUa`. Work landed in `rust/src/handlers/convert.rs` on top of
the pre-existing sidecar runner `rust/src/convert.rs`
(`EngineSpec`/`run`/`require_success`/`find_executable`) and the
`img/pdf` handler that was already migrated.

## Controller inventory

| Java controller | Routes | Status |
|---|---|---|
| `ConvertImgPDFController` | `/convert/img/pdf` | Done (pre-existing, native lopdf+image) |
| | `/convert/pdf/img` | **Done — Ghostscript sidecar** (png/jpeg only; gif/webp proxy to Java) |
| | `/convert/cbz/pdf`, `/convert/pdf/cbz` | Not ported (ZIP-of-images → PDF feasible natively; PDF→CBZ needs renderer) |
| | `/convert/cbr/pdf`, `/convert/pdf/cbr` | Not ported (needs RAR tooling + renderer) |
| `ConvertSvgToPDF` | `/convert/svg/pdf` | **Done — rsvg-convert → PNG embed, inkscape fallback (vector PDF)** |
| `ConvertHtmlToPDF` | `/convert/html/pdf` | **Done — WeasyPrint sidecar** (html + zip-with-inlined-images) |
| `ConvertOfficeController` | `/convert/file/pdf` | **Done — unoconvert → soffice fallback** |
| `ConvertPDFToOffice` | `/convert/pdf/word` | **Done — soffice/unoconvert** (`writer_pdf_import`) |
| | `/convert/pdf/presentation` | **Done — soffice/unoconvert** (`impress_pdf_import`) |
| | `/convert/pdf/xml` | **Done — soffice/unoconvert** (`writer_pdf_import`) |
| | `/convert/pdf/text` | **Done — txt via lopdf `extract_text`, rtf via soffice** |
| `ConvertPDFToHtml` | `/convert/pdf/html` | **Done — pdftohtml sidecar, output dir zipped** |
| `PdfVectorExportController` | `/convert/vector/pdf` | **Done — gs pdfwrite** (ps/eps/epsf; pdf passthrough) |
| | `/convert/pdf/vector` | **Done — gs eps2write/ps2write/pxlcolor/xpswrite** |
| `ConvertMarkdownToPdf` | `/convert/markdown/pdf` | Not ported — needs the Java MD→HTML renderer + ZIP asset inlining before WeasyPrint |
| `ConvertWebsiteToPDF` | `/convert/url/pdf` | Not ported — needs an HTTP fetch inside the sync work fn (reqwest blocking not enabled) plus the file-scheme content scan |
| `ConvertEmlToPDF` | `/convert/eml/pdf` | Not ported — needs an .eml parser → HTML before WeasyPrint |
| `ConvertEbookToPDFController` | `/convert/ebook/pdf` | Not ported — ebook parsing (EPUB etc.) not available natively |
| `ConvertPDFToExcelController` | `/convert/pdf/xlsx` | Not ported — needs table extraction (PDFBox) or LibreOffice round-trip |
| `ConvertPDFToEpubController` | `/convert/pdf/epub` | Not ported — needs pdftohtml XML + Calibre tooling |
| `ExtractCSVController` | `/convert/pdf/csv` | Not ported — PDFBox table/coordinate extraction |
| `ConvertPdfToVideoController` | `/convert/pdf/video` | Not ported — needs a PDF rasterizer (none in Rust deps) + FFmpeg |
| `ConvertPDFToPDFA` | `/convert/pdf/pdfa*` | Not ported — veraPDF validation + GS/PDFBox fallback chain |
| `ConvertPdfToPdfUa` (proprietary) | `/convert/pdf/pdfua` | Not ported — PDF/UA tagging engine |

## Conversion capabilities added

- **PDF → raster** (`/convert/pdf/img`): page selection (`pageNumbers`),
  DPI (default 300, ceiling 500 like `maxDPI`), colour type → gs device
  (`png16m/pnggray/pngmono`, `jpeg`/`jpeggray`), single-image vertical
  stacking or per-page ZIP, `_convertedToImages.zip` naming.
- **SVG → PDF** (`/convert/svg/pdf`): dangerous-content rejection
  (script/foreignObject/iframe/object/embed/js+vbscript URLs), viewport
  sizing (width/height → viewBox → 595×842 default, units → pt at 0.75),
  `combineIntoSinglePdf` merge via `pdfdoc::import_pages`, separate-PDF ZIP.
- **Office → PDF** (`/convert/file/pdf`): extension validation
  (`^[a-zA-Z0-9]{2,4}$`), `unoconvert` preferred then `soffice
  --headless --convert-to pdf`, output discovery fallback, result
  round-tripped through `pdfdoc` like Java's factory reload.
- **PDF → Office** (`/convert/pdf/{word,presentation,xml,text}`):
  per-endpoint format allowlists + `--infilter` filters, content-type
  gate (`application/pdf`), single-file vs ZIP responses, txt via
  lopdf text extraction.
- **HTML → PDF** (`/convert/html/pdf`): `.html` and `.zip` inputs, zip
  main-entry selection (index.* → shallowest) and `<img>` inlining as
  data URIs, WeasyPrint with Java's arg set (`-e utf-8 -v --pdf-forms
  --allowed-protocols data`).
- **PDF → HTML** (`/convert/pdf/html`): `pdftohtml -c` in a workdir, whole
  output directory zipped as `{stem}ToHtml.zip`.
- **Vector** (`/convert/vector/pdf`, `/convert/pdf/vector`): gs
  `pdfwrite` (with `-dPDFSETTINGS=/prepress`) and the four vector devices.

## Sidecar requirements

| Engine | Used by | Binary names tried |
|---|---|---|
| Ghostscript `gs` | pdf/img, vector/pdf, pdf/vector | `gs` on PATH |
| LibreOffice | file/pdf, pdf/{word,presentation,xml,text} | `unoconvert` then `soffice` |
| WeasyPrint | html/pdf | `weasyprint` |
| Poppler `pdftohtml` | pdf/html | `pdftohtml` |
| librsvg / Inkscape | svg/pdf | `rsvg-convert`, then `inkscape` |

All invocations go through `crate::convert` (argv arrays, no shell,
timeouts, drained pipes). Missing engines surface as errors — no handler
pretends a conversion succeeded.

## Modified files

- `rust/src/handlers/convert.rs` — new handlers + helpers (~1,250 added
  lines); refactored `append_image_page` tail into shared `commit_page`.
- `rust/src/handlers/mod.rs` — 11 new `/api/v1/convert/**` routes.
- `rust/Cargo.toml` — added `base64 = "0.22"` (already in `Cargo.lock`
  transitively; lock will pick up the new dep edge on next build).

## Incomplete integrations / known divergences

- **`useStirlingOfficeConvert=true`** is accepted but ignored — Java's
  in-process Office Convert engine has no Rust port; LibreOffice/WeasyPrint
  paths always run.
- **HTML/SVG sanitisation** is not ported: `CustomHtmlSanitizer` and the
  DOM-based `SvgSanitizer` are approximated (WeasyPrint `data`-only
  protocols; SVG active-content rejection). Java *rewrites* bad SVG; Rust
  *rejects* it.
- **`/convert/pdf/img`** supports png/jpeg/jpg only — gif/webp forward to
  Java (Java's webp path needs its Python OpenCV script). TIFF single-image
  mode and `includeAnnotations` rendering semantics differ (gs renders
  annotation appearances; PDFBox filters differently).
- **`/convert/svg/pdf`** raster path (rsvg-convert) produces an image
  XObject, not Batik's vector paint — visually equivalent, structurally
  different. Page size uses the SVG viewport in pt.
- **`/convert/pdf/text` txt** uses lopdf `extract_system_text` ordering —
  text content parity is approximate vs `PDFTextStripper`.
- **unoconvert fallback policy**: Java only falls back on spawn failure;
  Rust also falls back on non-zero exit (more forgiving).
- **Office document sanitising** (`OfficeDocumentSanitizer`,
  `CustomHtmlSanitizer` for .html office input) is not ported — inputs are
  passed to LibreOffice verbatim.
- **Endpoint enablement**: `pdf-to-rtf` gate and Ghostscript/LibreOffice
  endpoint-group checks are handled by the shared `endpointconfig` guard,
  not re-implemented per handler.

## Route requirements (registered)

```
POST /api/v1/convert/pdf/img        POST /api/v1/convert/pdf/word
POST /api/v1/convert/svg/pdf        POST /api/v1/convert/pdf/presentation
POST /api/v1/convert/file/pdf       POST /api/v1/convert/pdf/xml
POST /api/v1/convert/html/pdf       POST /api/v1/convert/pdf/text
POST /api/v1/convert/pdf/html       POST /api/v1/convert/vector/pdf
POST /api/v1/convert/pdf/vector     (+ pre-existing /convert/img/pdf)
```

All accept `?async=true` via `handlers::misc::dispatch`, matching the
Java `@AutoJobPostMapping` contract.

## Formatting status

`rustfmt --edition 2021` run on `src/handlers/convert.rs` and
`src/handlers/mod.rs` — clean. No builds/tests run per task restrictions;
compile verification pending (`cargo check` on next available slot).
