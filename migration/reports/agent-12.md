# Agent 12 — Conversion API kickoff: img/pdf + engine runner

## Scope

First endpoint of the Conversion API family (`/api/v1/convert/*`, 27 endpoints
in `app/core/.../converters/`). Chose `/img/pdf` as the only endpoint that is
pure `lopdf` + `image` work with no external engine.

## What exists now

- `rust/src/convert.rs` — shared external-engine runner (`EngineSpec`,
  `run`, `require_success`, `find_executable`). Runs engine binaries as argv
  child processes inside a tempdir with a hard timeout and drained pipes.
  Currently unused (`dead_code` warnings) — staged for the LibreOffice /
  Ghostscript / Chromium / FFmpeg endpoints that follow.
- `rust/src/handlers/convert.rs` — `img_pdf` handler for
  `POST /api/v1/convert/img/pdf`, ported from `PdfUtils.imageToPdf` +
  `ConvertImgPDFController.convertToPdf`:
  - one PDF page per input image; multi-file `fileInput` parts;
  - `fitOption` (`fillPage` default, `fitDocumentToImage`,
    `fitDocumentToPage`, `maintainAspectRatio`, unknown → blank page),
    `colorType` (`color`/`greyscale`/`blackwhite`, blank → color),
    `autoRotate` (swaps A4 page dims for landscape sources);
  - JPEG stays `/DCTDecode` passthrough when color + no transform needed;
    PNG and transformed JPEGs embed FlateDecode RGB/Gray with `/SMask`
    for alpha;
  - forwards the whole request to Java when any part is unsupported —
    TIFF (multi-frame), GIF/WebP, CMYK JPEG, EXIF orientation ≠ 1 — so
    ImageIO semantics and error shapes stay identical.
- Route registered: `/api/v1/convert/img/pdf` in `rust/src/handlers/mod.rs`.

## Known divergences (deliberate)

- Native JPEG passthrough keeps original encoding; Java's
  `JPEGFactory.createFromImage` re-encodes the decoded raster. Pixels are
  equivalent; bytes differ.
- `blackwhite` thresholds luminance at 128 into 8-bit gray; Java emits a
  1-bit `TYPE_BYTE_BINARY` image (ImageMask). Visual result identical.

## Remaining conversion endpoints (all still proxied)

Engine-bound, need explicit engine choice before porting:

- LibreOffice/Stirling-Office: `/file/pdf`, `/pdf/word`, `/pdf/presentation`,
  `/pdf/text`, `/pdf/xml`, `/pdf/xlsx`
- Ghostscript: `/pdfa`, `/pdf/video` (with ffmpeg), CBZ ebook optimization
- Headless browser: `/url/pdf`, `/html/pdf`
- Calibre/ebook: `/ebook/pdf`, `/pdf/epub`
- Rasterizer: `/pdf/img` (needs Ghostscript/pdfium, not lopdf)
- Typesetting: `/markdown/pdf` (LaTeX/typst), `/svg/pdf` (resvg candidate)
- `/eml/pdf`, `/pdf/csv`, `/pdf/html`, `/pdf/vector`, `/vector/pdf`,
  `/pdf/cbz`, `/pdf/cbr`, `/cbz/pdf`, `/cbr/pdf`

`src/convert.rs` (`EngineSpec`) is the intended shell for the process-based
engines; decide per-endpoint whether Java's engine path or a native path is
authoritative before implementing.

## Verification

`cargo check` clean (warnings only: staged engine runner is unused until the
first engine-backed handler lands). `rustfmt --edition 2021` applied to
`src/convert.rs`, `src/handlers/convert.rs`, `src/handlers/mod.rs`.
