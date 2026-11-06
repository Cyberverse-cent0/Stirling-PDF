# P3 — Convert family migration readiness

Audit date: 2026-11-06. Sources: Java controllers at `HEAD:` (the working
tree deletes migrated controllers), `ProcessExecutor`/`OfficeConversionService`,
and the live Rust route table in `rust/src/handlers/mod.rs`. Engine presence
is gated by `endpointconfig::probe_engines` (mirrors
`ExternalAppDepConfig`); a missing engine disables the tool group and the
`api_guard` returns 403 before the handler runs.

## Status matrix

| Endpoint | Java controller | Engine(s) | Rust status |
|---|---|---|---|
| `POST /convert/url/pdf` | `ConvertWebsiteToPDF` | WeasyPrint (`ProcessExecutor.WEASYPRINT`) | **Routed** — `convert::url_to_pdf`, SSRF guards + tests |
| `POST /convert/markdown/pdf` | `ConvertMarkdownToPdf` | WeasyPrint (or office convert via `useStirlingOfficeConvert`) | **Routed** — `convert::markdown_to_pdf` (comrak → HTML → engine) |
| `POST /convert/html/pdf` | `ConvertHtmlToPDF` | WeasyPrint / office convert | **Routed** — `convert::html_to_pdf` |
| `POST /convert/ebook/pdf` | `ConvertEbookToPDFController` | Calibre `ebook-convert` → PDF, then optional Ghostscript optimize | **Routed** — `convert::ebook_to_pdf` (EBOOK_EXTENSIONS: epub/mobi/azw3/fb2/txt/docx) |
| `POST /convert/eml/pdf` | `ConvertEmlToPDF` | Jakarta Mail parse → HTML → WeasyPrint **or** office HTML→PDF | **Routed** — `handlers::eml::eml_to_pdf` (mail-parser → styled HTML → WeasyPrint; MSG rejected, office-convert branch not ported) |
| `POST /convert/pdf/csv` | `ExtractCSVController` | Tabula table detection + CSVPrinter (pure Java, no engine) | **Routed** — `convert::pdf_to_csv` (`src/tabula.rs`, pure Rust, no engine) |
| `POST /convert/pdf/epub` | `ConvertPDFToEpubController` | Calibre `ebook-convert` (`--pdf-engine pdftohtml`), or office convert | **Routed** — `convert::pdf_to_epub` (EPUB + AZW3/`targetDevice`) |
| `POST /convert/pdf/xlsx` | `ConvertPDFToExcelController` | Tabula + POI `XSSFWorkbook` (legacy) **or** `OfficeConversionService` xlsx | **Routed** — `convert::pdf_to_xlsx` (same `tabula.rs` extractor, native OOXML writer); office-convert branch → 400 |
| `POST /convert/pdf/video` | `ConvertPdfToVideoController` | `ffmpeg` rasterise→encode (mp4/gif…) | **NOT routed** — proxied; ffmpeg probe intentionally absent (Java disabled it over CVEs) |
| `POST /convert/pdf/pdfa` | `ConvertPDFToPDFA` | Ghostscript `-dPDFA` / office convert; veraPDF **Java lib** validation; PDF/X variant via `outputFormat` | **Routed** — `convert::pdf_to_pdfa` (Ghostscript + optional `verapdf` CLI validation; strict mode native) |
| `POST /cbr/pdf` | `ConvertImgPDFController.convertCbrToPdf` | **junrar** (pure-Java RAR decode) + optional Ghostscript optimize (`optimizeForEbook`) | **Routed** — `convert::cbr_pdf`; divergence: Rust shells `unrar`/`unar`/`bsdtar`/`7z` (no pure-Rust RAR reader) |
| `POST /pdf/cbr` | `ConvertImgPDFController.convertPdfToCbr` | `rar` CLI (`rar a -m5 -ep1`), dpi param default 300 | **Routed** — `convert::pdf_to_cbr`; `rar` group probe gates it |
| `POST /cbz/pdf`, `POST /pdf/cbz` | same controller | zip read/write, pure Java | **Routed** — `cbz_pdf`, `pdf_to_cbz` (zip crate) |

## Remaining work per endpoint

### `eml/pdf` — DONE (native)
- Status: routed at `/api/v1/convert/eml/pdf`; mail-parser EML→HTML→WeasyPrint.
  Divergences: MSG/OLE2 rejected with a clear 400 (no msg-parser port);
  `useStirlingOfficeConvert` ignored (always WeasyPrint); `includeAllRecipients`
  not ported; attachment size cap honors `maxAttachmentSizeMB` (default 10).
  `includeAttachments` embeds files via a PDF `/EmbeddedFiles` name tree.
- Original plan notes:
- Request: `EmlToPdfRequest extends PDFFile` — `useStirlingOfficeConvert`,
  `includeAttachments` (default false), `maxAttachmentSizeMB` (10),
  `downloadHtml` (returns `.html` instead of PDF), `includeAllRecipients`.
- Java flow (`EmlToPdf.convertEmlToHtml`): parse EML/**MSG** with Jakarta
  Mail, render a sanitized HTML document (attachments inlined/linked per
  flags), then either `officeToPdfService.convertHtml` (soffice path) or
  WeasyPrint.
- Rust plan: `mail-parser` crate covers RFC-822 EML; **MSG (OLE2) is not
  covered** — either `cfb`+`msg-parser` crates or document EML-only
  divergence. Reuse the existing WeasyPrint `EngineSpec` path from
  `html_to_pdf`; the office-convert branch maps to the `soffice` spec.
  Group gating: Weasyprint ∪ LibreOffice already probed.
- Watch: `downloadHtml=true` bypasses the engine entirely — cheap win.

### `pdf/csv` — DONE (pure Rust, no engine)
- `src/tabula.rs` ports Tabula's ruling-lattice detection plus a
  borderless text-edge pass (runs of >=2 lines splitting at persistent
  x-gutters; lattice wins on overlap) over `text.rs` positioned runs;
  `extract_table_fragments` in `convert.rs`
  maps each detected table to a `TableFragment { page, table_index, rows }`.
- `csv_fragment_bytes` reproduces `CSVFormat.EXCEL` + `QuoteMode.ALL`:
  every field double-quoted, inner quotes doubled, CRLF records, plus
  `CsvSanitizer` formula-injection prefixing (`= + - @` unless a plain
  number) and `TabulaTableParser.normaliseText` whitespace collapsing.
- Single fragment → `{base}_extracted.csv` (`text/csv`); multiple →
  `{base}_extracted.zip` of `{base}_p{N}_t{M}.csv`; none → **422**
  `NothingToExtractException` parity — scanned/image-only pages get the
  `NO_TEXT_LAYER` detail ("Run OCR on the PDF first"), text pages get
  `NO_TABLES_FOUND`. `pageNumbers` selection honoured via `page_select`.
- Verified live: `bordered-table-test_widget.pdf` → clean 4×3 CSV;
  `many-tables-test_stress.pdf` → 11-entry ZIP across 4 pages;
  `tables.pdf`/`bank_statement.pdf` (borderless, whitespace-aligned) →
  correct CSVs via the text-edge pass; `table-sample.pdf` /
  `merged-table-sample.pdf` → correct cells (spanning header cells
  become empty strings); `multi-column-test_lorem.pdf`,
  `justified-sample.pdf`, `classified_invoice.pdf` (prose) → 422;
  `images.pdf` (image-only) → 422 NO_TEXT_LAYER; malformed → 400.

### `pdf/xlsx` — DONE (Tabula-equivalent path only)
- Same `extract_table_fragments` pipeline; `xlsx_workbook` writes a
  minimal valid OOXML package by hand (inline strings, one sheet per
  fragment) — no `rust_xlsxwriter` dependency was needed.
- Sheet names follow Java's `writeTabula`: `Page N` for a lone table on a
  page, `Page N Table M` otherwise, POI-style ` (n)` dedupe.
- `useStirlingOfficeConvert` / `system.stirlingOfficeConversion` → **400**
  "the Rust backend does not implement" — no proxy to Java.
- Verified live: widget fixture → valid zip with `Page 1` sheet and
  correct cell values; stress fixture → 11 sheets; malformed → 400;
  no-table/scanned → same 422 contract as csv.

### `pdf/video` — FFmpeg, blocked upstream anyway
- Java: rasterise pages (PDFBox render) → `ffmpeg` image2pipe → x264 mp4
  (or gif). Params: `videoFormat` (mp4), `secondsPerPage` (3), `resolution`
  (ORIGINAL), `dpi` (150), `opacity` (watermark), `watermarkText`.
- FFmpeg is commented out of Java's dependency probe (raised CVEs), so the
  endpoint stays "enabled" and fails at runtime when ffmpeg is absent.
- Rust plan: Ghostscript rasterise (`gs` already probed) or `pdfium`-free
  page render is the gap — lopdf cannot rasterise. Realistic path: `gs`
  → PNG frames → `ffmpeg` `EngineSpec`. Keep proxied until both sidecars
  are acceptable ops-wise; low user value vs. cost.

### `pdf/pdfa` — Ghostscript + veraPDF, largest remaining
- Java: `useStirlingOfficeConvert` chooses office path; else Ghostscript
  (`gs -dPDFA=<part> -dPDFACompatibilityPolicy=1` + generated `PDFA_def.ps`
  pdfmark for OutputIntent/ICC). `outputFormat` selects PDF/A vs PDF/X;
  `pdfUa`, `strict` flags. `VeraPDFService` uses the **veraPDF Java
  library** (`org.verapdf.*`) for validation reporting — not a binary.
- Rust plan: the `gs` invocation is directly portable via `EngineSpec`
  (the `PDFA_def.ps` generation is string templating — mechanical). The
  veraPDF validation half has no Rust equivalent: options are the
  veraPDF CLI binary as an `EngineSpec`, or omit validation and document
  the divergence. PDF/X branch is a second gs profile.
- Group gating: Ghostscript probed; veraPDF group exists but Java does
  not probe it (library, always "present") — decide whether Rust should
  probe a `verapdf` binary if it goes the CLI route.

## Cross-cutting notes

- All endpoints are `@AutoJobPostMapping` — long conversions must run
  through the Rust job framework (`handlers/jobs.rs`) with `async=true`
  parity; check each Java endpoint's `resourceWeight`.
- `useStirlingOfficeConvert` appears on eml/pdf, pdf/xlsx, pdf/pdfa,
  markdown/pdf — it selects between the legacy engine and the
  `officeconvert` service. Rust has no officeconvert equivalent; default
  to whichever path Java takes when the flag is absent (verify per
  endpoint — `OfficeConversionService.legacy()` default).
- Engine-presence semantics now live in `endpointconfig::probe_engines`;
  any NEW engine a port introduces (unrar/7z for cbr, ffmpeg, verapdf)
  must be added to `ENGINE_PROBES` or the endpoint will 500 instead of 403.
