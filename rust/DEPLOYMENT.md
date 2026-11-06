# Deploying the Rust backend

`stirling-server` is a single binary. Most PDF work is pure Rust (`lopdf`);
a set of external engine binaries is probed at startup and the endpoints
depending on a missing engine are disabled (the API guard answers 403,
the same behaviour Java's `ExternalAppDepConfig` produces).

## Required sidecars

| Binary | Used for | Minimum version |
|---|---|---|
| `gs` (Ghostscript) | compress, pdfa/pdfx, rasterisation, auto-split QR decode, scanner-effect | — |
| `qpdf` | structural PDF checks/repairs in several ops | 12.0 |
| `soffice` / `unoconvert` (LibreOffice) | office ↔ PDF conversions | — |
| `weasyprint` | html/markdown/url/eml → PDF | 58.0 |
| `pdftohtml` | pdf → html | — |
| `tesseract` + `ocrmypdf` | ocr-pdf (OCRmyPDF preferred, per-page Tesseract fallback) | — |
| `magick` (ImageMagick) | image conversion paths | — |
| `ebook-convert` (Calibre) | ebook → pdf, pdf → epub | — |
| `rar` | cbr → pdf / pdf → cbr | — |
| `verapdf` (optional) | PDF/A `strict=true` validation only — warn-and-serve when absent | — |

A missing binary disables its whole tool group, not just one endpoint —
e.g. no `gs` takes down compress-pdf, pdf-to-pdfa, scanner-effect and the
Ghostscript fallbacks. `ffmpeg` is deliberately not probed (Java disabled
pdf-to-video over CVEs).

## Configuration

- `system.customPaths.operations.*` in `configs/settings.yml` overrides
  the binary path for `weasyprint`, `unoconvert`, `calibre`
  (`ebook-convert` resolves through it), `ocrmypdf` and `soffice`; blank
  means the bare `PATH` name. Other engines always probe `PATH`.
- `system.enableUrlToPDF` must be `true` for url-to-pdf (default off,
  matching Java).
- `endpoints.toRemove` / `endpoints.groupsToRemove` disable endpoints or
  whole groups administratively.
- `policies.maxSteps` / `policies.maxFilesPerStep` cap pipeline runs
  (defaults 50 / 10000; ≤0 is unlimited). `internalApi.connectTimeoutSeconds`
  / `readTimeoutSeconds` tune the pipeline's loopback dispatch.

## Verifying a deployment

- `stirling doctor` / `stirling engines list` (the `rust/cli` binary)
  report which engines were found.
- `GET /api/v1/settings/get-endpoints-status` lists disabled endpoints
  and groups with reasons.
- Startup logs warn `Missing dependency: <bin> - Disabling group: <group>`
  for each absent engine.

## Notes

- The pipeline (`POST /api/v1/pipeline/handleData`) dispatches each step
  back to this server over loopback HTTP, so a step that needs an engine
  honours the same enablement/403 rules as a direct call.
- Async jobs (`?async=true`) persist to `configs/stirling-jobs.db`
  (SQLite) under the base path, so results survive restarts; jobs that
  were still running when the process died come back marked
  "Job interrupted by server restart". If the store cannot be opened the
  server logs a warning and falls back to memory-only jobs.
