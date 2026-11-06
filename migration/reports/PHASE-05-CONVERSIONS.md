# Phase 5 — Conversion Controllers → Rust

Scope: `app/core/src/main/java/stirling/software/SPDF/controller/api/converters/`
(16 controllers, ~6.5k LOC), base path `/api/v1/convert` (`@ConvertApi`).

## Endpoint → engine audit

| Java endpoint | Controller | Java engine | Rust status |
|---|---|---|---|
| `POST /convert/img/pdf` | ConvertImgPDFController | PDFBox `PDImageXObject`/`JPEGFactory`/`LosslessFactory` + ImageIO | **Migrated (native lopdf + `image`)** |
| `POST /convert/pdf/img` | ConvertImgPDFController | PDFBox `PDFRenderer` / Ghostscript group flag | Proxied (needs rasteriser) |
| `POST /convert/cbz/pdf`, `/cbr/pdf` | ConvertImgPDFController | CbzUtils/CbrUtils + image pipeline | Proxied |
| `POST /convert/pdf/cbz`, `/pdf/cbr` | ConvertImgPDFController | PdfToCbz/CbrUtils + renderer + archive | Proxied |
| `POST /convert/file/pdf` | ConvertOfficeController | LibreOffice `soffice --headless --convert-to pdf` (or StirlingOfficeConvert) | Proxied — engine foundation ready |
| `POST /convert/pdf/word` `/pdf/presentation` `/pdf/text` `/pdf/xml` | ConvertPDFToOffice | PDFBox + Apache POI (or LibreOffice w/ flag) | Proxied |
| `POST /convert/pdf/xlsx` | ConvertPDFToExcel | PDFBox table extraction + POI (or LibreOffice) | Proxied |
| `POST /convert/pdf/html` | ConvertPDFToHtml | PDFBox text/HTML extraction (or LibreOffice) | Proxied |
| `POST /convert/pdf/pdfa` | ConvertPDFToPDFA | Ghostscript + veraPDF validation (~72 ProcessExecutor sites) | Proxied — Ghostscript spec ready to wire |
| `POST /convert/pdf/video` | ConvertPdfToVideo | PDFBox render → FFmpeg | Proxied — FFmpeg spec ready to wire |
| `POST /convert/svg/pdf` | ConvertSvgToPDF | Batik/SVG toolkit (verify) | Proxied |
| `POST /convert/html/pdf` | ConvertHtmlToPDF | openhtmltopdf | Proxied — Chromium/OpenHTML engine candidate |
| `POST /convert/url/pdf` | ConvertWebsiteToPDF | URL fetch + openhtmltopdf | Proxied |
| `POST /convert/markdown/pdf` | ConvertMarkdownToPdf | Markdown→HTML→openhtmltopdf (or LibreOffice flag) | Proxied |
| `POST /convert/ebook/pdf` | ConvertEbookToPDF | External ebook engine (ProcessExecutor-heavy; verify binary) | Proxied |
| `POST /convert/eml/pdf` | ConvertEmlToPDF | javax.mail MIME parse → PDFBox render | Proxied |
| `POST /convert/pdf/csv` | ExtractCSVController | PDFBox table → CSV (distinct from form/extract-csv) | Proxied |
| `POST /convert/vector/pdf`, `/pdf/vector` | PdfVectorExportController | SVG↔PDF vector paths (Batik/PDFBox) | Proxied |

## What was built

### `rust/src/convert.rs` — reusable engine foundation

Process-level integration for external converters, shared by every future
engine-backed endpoint:

- `EngineSpec { program, args, timeout, path_dirs, cwd }` — argv-array
  execution, no shell interpolation; `path_dirs` supports Stirling-managed
  install prefixes before `PATH`; `cwd` covers engines that resolve
  relative outputs or write lockfiles next to inputs.
- `find_executable` — PATH + extra-dir lookup with exec-bit checks;
  Windows callers may omit the extension (`soffice` → `soffice.exe`/`.com`/
  `.bat`/`.cmd`).
- `run` — spawn with piped stdout/stderr drained on reader threads (no
  pipe-buffer deadlock), 25 ms `try_wait` poll, kill + `timed_out` flag on
  deadline expiry.
- `require_success` — non-zero exit/timeout → `AppError::Internal` with a
  2 KB stderr tail for diagnostics.
- Temp dirs: callers use `tempfile::tempdir()` (already a dependency);
  engines write outputs there and the handler reads/cleans up.

Not yet wired to a specific engine — the audit shows the highest-value
next integrations are LibreOffice (`/file/pdf`, PDF→Office group) and
Ghostscript (`/pdf/pdfa`, `/pdf/img` group flag), both of which map
cleanly onto `EngineSpec`.

### `rust/src/handlers/convert.rs` — `/api/v1/convert/img/pdf`

Faithful port of `PdfUtils.imageToPdf` for the codecs Rust covers:

- **Params**: `fileInput` (multi, via `receive_multipart`), `fitOption`
  (default `fillPage`; explicit empty normalizes like Java's `isEmpty`
  check), `colorType` (default `color`; `isBlank` normalizes;
  `greyscale`/`blackwhite` quantize like
  `ImageProcessingUtils.convertColorType`), `autoRotate`. Unknown
  `fitOption` → blank page, matching Java's empty content stream.
- **Page geometry**: A4 595.28×841.89 pt (PDRectangle.A4), swapped for
  landscape under `autoRotate`; `fitDocumentToImage`/`fitDocumentToPage`
  size the page to image pixels; `maintainAspectRatio` aspect-fits
  centered — all mirroring `addImageToDocument`.
- **JPEG**: original stream embedded with `/DCTDecode` (equivalent to
  `JPEGFactory`); component count parsed from SOF → `/DeviceGray` vs
  `/DeviceRGB`.
- **PNG**: decoded via `image` crate → unfiltered sample stream;
  `/SMask` emitted when alpha < 255 (matches `LosslessFactory` alpha
  handling).
- **Async**: hand-written handler + `misc::dispatch` → `?async=true` job
  support identical to `#[endpoint]` endpoints.
- **Delegation**: TIFF (any frame count), GIF/WebP/BMP, CMYK JPEG, and
  EXIF-rotated JPEG (orientation ≠ 1 — Java's `loadImageWithExifOrientation`
  rotates pixels) forward the whole multipart to Java, preserving
  `forward_multipart` param/header fidelity.

Route already registered: `POST /api/v1/convert/img/pdf` in
`handlers/mod.rs`; `mod convert;` added to `main.rs`.

## Dependencies

- Existing: `lopdf`, `image` (jpeg+png), `tempfile`, `zip`.
- **Not added**: `flate2`/`miniz_oxide` — decoded PNG samples are stored
  unfiltered (valid PDF, larger files). Adding a deflate dep is a
  follow-up optimization.
- External engines required for remaining groups: LibreOffice (`soffice`),
  Ghostscript (`gs`), FFmpeg (`ffmpeg`), plus whatever Batik/openhtmltopdf
  equivalents are chosen for SVG/HTML.

## Tests / verification

**Not run** — build/test restrictions from prior phases still apply;
no `cargo check`/`test`/runtime verification performed. Source-level
review only. Recommended once restrictions lift:

- JPEG/PNG → PDF: compare page count, MediaBox, rendered pixels vs Java.
- `fitOption` × `autoRotate` × `colorType` matrix.
- Proxy path: TIFF, CMYK JPEG, EXIF orientation 6.
- `?async=true` job round-trip for a passing conversion.
- Engine runner: unit-test `run` timeout/kill with `/bin/sleep`.

## Known divergences

- PNG samples uncompressed (no `/Filter`) — larger PDFs than PDFBox's
  FlateDecode output; visually identical.
- `blackwhite` threshold at luminance 128 approximates `TYPE_BYTE_BINARY`.
- JPEG passthrough preserves original DCT stream; Java's `JPEGFactory`
  re-encodes — both are DCTDecode, minor compression differences only.
- EXIF orientation ≠1 and CMYK JPEG proxy to Java rather than being
  re-encoded natively (correctness over coverage).
- TIFF forwards entirely even for single-frame files — Rust has no TIFF
  decoder in the current `image` feature set.

## Blockers / next steps

- No rasteriser in Rust → `/pdf/img`, `/pdf/cbz|cbr`, `/pdf/video` need
  Ghostscript/PDRenderer-equivalent engine wiring via `convert::run`.
- Office/HTML/SVG/ebook groups all need their external engines provisioned
  and mapped to `EngineSpec` (commands must be taken from the Java
  `ProcessExecutor` call sites before wiring).
- `extract_csv` name collision: Rust already has `form::extract_csv`
  (AcroForm); `/convert/pdf/csv` is table extraction — keep separate.

## Stabilization addendum (2026-11-06)

The convert surface has grown well past this report's original scope —
`handlers/convert.rs` now implements and routes 11 endpoints
(img/pdf, pdf/img via Ghostscript, svg/pdf, file/pdf, pdf→word/
presentation/xml/text, html/pdf, pdf/html, vector/pdf, pdf/vector)
through `convert::EngineSpec` sidecars plus the native lopdf paths
described above.

Compile/integration fixes landed in this file during stabilization:

- CBZ archive name collection rewritten as a loop — `ZipArchive::
  by_index` borrows the archive mutably per call, so the iterator
  chain could not return `ZipFile`s (E0501-family escape error).
- Unused `office_convert` return binding dropped (output is re-read
  from the engine outdir).
- Clippy: needless `&v` borrows in `svg_unit_to_pt` calls; `len() == 0`
  → `is_empty()` on the CBZ archive check.

Verified: `cargo check` clean and `cargo test` **168/168 pass** at the
snapshot taken during this phase; `rustfmt --check` clean on
`convert.rs`. Concurrent agents subsequently introduced new errors in
`automation.rs`/`platform.rs` (unrelated files, in-flight work) — the
convert module itself remained warning-free.

Test coverage for the convert handlers is still thin: no unit tests
exist yet for `img_pdf`/`pdf_to_img`/engine-spec paths; engine-backed
endpoints need `gs`/`soffice` fixtures or mock executables before
parity claims. Remaining Java-fallback convert endpoints (url/pdf,
markdown/pdf, ebook/pdf, eml/pdf, pdf/csv, pdf/epub, pdf/xlsx,
pdf/video, pdf/pdfa) are unchanged from the audit table above.
