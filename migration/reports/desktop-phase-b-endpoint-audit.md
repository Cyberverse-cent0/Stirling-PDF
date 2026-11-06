# Desktop migration — Phase B endpoint classification audit

Scope: what the Tauri desktop shell gains/loses when `stirling-server`
replaces the `java -jar` sidecar (`JAVA_BACKEND_URL=off`, so every
unrouted `/api/**` request 404s). Compiled from `rust/src/handlers/mod.rs`
route table, `EngineSpec`/`engine_program`/`Command::new` call sites in
`rust/src/handlers/`, the frontend `ToolEndpoint` union in
`frontend/src/editor/core/types/toolApiTypes.ts`, and the Java
controller inventory.

## Phase A status — landed

`frontend/src-tauri/src/commands/backend.rs` implements the sidecar
switch: `select_backend()` prefers a `stirling-server[-<triple>][.exe]`
binary (resources dir, `src-tauri/binaries/` in dev, or
`STIRLING_SERVER_PATH`), `STIRLING_DESKTOP_BACKEND=rust|java` overrides,
and `run_stirling_server()` sets `PORT=0`, `HOST=127.0.0.1`,
`STIRLING_PDF_TAURI_MODE=true`, `JAVA_BACKEND_URL=off`,
`STIRLING_BASE_PATH`, and the shutdown sentinel. Verified live: banner
parse, `/health` 200, sentinel shutdown. `tauri.conf.json` declares
`bundle.externalBin: ["binaries/stirling-server"]`; `task
desktop:rust-server` stages it and is wired into `prepare`.

Remaining Phase A/D work is packaging only: `resources` still ships
`libs/*.jar` + `runtime/jre/**/*`, and `build-tools` still stages the
JAR. Dropping those (and `BackendKind::Java`) is the installer-size win
(~100 MB+), deferred until the Rust path is proven on all desktop OSes.

## Classification

Legend: **NATIVE** = pure Rust (`lopdf`/`image`/`zip`/`mail-parser`), no
external process. **ENGINE** = routed, but shells out to a binary that
must be bundled or feature-detected. **ABSENT** = no Rust route → 404
on desktop.

### NATIVE — works on desktop today, zero dependencies

- **general**: merge-pdfs, rotate-pdf, remove-pages, rearrange-pages,
  split-pages, split-by-size-or-count, split-pdf-by-chapters,
  split-pdf-by-sections, split-for-poster-print, pdf-to-single-page,
  remove-image-pdf, scale-pages, crop, add-page-numbers, overlay-pdfs,
  multi-page-layout, booklet-imposition, edit-table-of-contents,
  extract-bookmarks, create-portfolio, flatten-portfolio, auto-rename
- **misc**: decompress-pdf, update-metadata, show-javascript,
  list/add/extract/rename/delete/batch-process-attachments,
  extract-images, extract-image-scans (native splitter), add-image,
  add-comments, add-stamp, auto-split-pdf, auto-rotate-pdf,
  unlock-pdf-forms
- **security**: sanitize-pdf, get-info-on-pdf, redact, auto-redact,
  redact-execute, remove-cert-sign, prepare-pdf-session
- **analysis**: all 8 (page-count, basic-info, page-dimensions,
  document-properties, form-fields, annotation-info, font-info,
  security-info)
- **filter**: all 6
- **form**: all 9 (fields, fields-with-coordinates, extract-csv,
  extract-xlsx, fill, add/edit/modify/delete-fields)
- **convert (native subset)**: img/pdf, pdf/img (jpeg/png),
  pdf/csv + pdf/xlsx (tabula lattice + borderless detection),
  pdf/markdown (pdftext pipeline), pdf/text (`outputFormat=txt`),
  pdf/cbz (image extraction → cbz), eml/pdf (mail-parser → render;
  see ENGINE note for the render step)
- **platform/jobs**: `/api/v1/ping`, info/version, job + files
  framework, config/app-config, endpoint-enabled(s),
  endpoints-availability, group-enabled, login-disclaimer, ui-data/*,
  settings/*, mobile-scanner/*, pipeline/handleData, metrics `/info/*`

### ENGINE — routed, but need a bundled binary

| Engine | Endpoints | Desktop story |
|---|---|---|
| `gs` (Ghostscript) | pdf/img (non-jpeg/png), pdf/vector, vector/pdf, pdf/pdfa, pdf/epub (optimize pass), cbr/pdf, pdf/cbz raster path, misc/flatten, misc/remove-blanks (optional probe), misc/compress-pdf (gs optimize levels), security redact `convertPDFToImage`, misc/extract-image-scans raster, misc/repair (fallback) | Small-ish (~30 MB); bundleable per-platform |
| `qpdf` | misc/compress-pdf, misc/repair | Small; bundleable |
| `pdftohtml` | pdf/html | Small; bundleable |
| `rsvg-convert` / `inkscape` | svg/pdf | Small; bundleable |
| `unrar`/`unar`/`bsdtar`/`7z` + `rar` | cbr/pdf, pdf/cbr | Small; bundleable |
| `tesseract` + `ocrmypdf` | misc/ocr-pdf | ~50 MB w/ traineddata; optional |
| `weasyprint` | html/pdf, url/pdf, markdown/pdf, eml/pdf (render step) | Python+GTK stack — heavy; likely desktop-off |
| `soffice`/`unoconvert` | file/pdf, pdf/word, pdf/presentation, pdf/xml, pdf/text (`rtf`) | LibreOffice headless ~400 MB — probably stays off |
| `ebook-convert` (Calibre) | ebook/pdf, pdf/epub | Large; optional |
| `verapdf` | pdf/pdfa validation path (optional) | Small Java jar today; Rust probes the binary |
| ImageMagick `magick` | misc/compress-pdf `lineArt=true` | Already rejected natively — no loss |

`endpointconfig` group probes (`ENGINE_PROBES`) already disable each
tool group when its binary is absent, so desktop can ship engines
incrementally and the UI hides what isn't there — same mechanism as
server deployments.

### ABSENT — no Rust route, 404 on desktop

**Signing / crypto family** (the hard casualty — CMS/PKCS#7, keystores,
HSM, RFC 3161):

- `security/cert-sign` (+ `capabilities`, `pkcs11-certificates`,
  `windows-certificates`, `cert-sign/sessions*` — core
  `CertSignController`, `HardwareSigningController`, proprietary
  `SigningSessionController`)
- `security/timestamp-pdf` (RFC 3161 TSA)
- `security/validate-signature` (CMS validation)
- `security/verify-pdf` (CMS + veraPDF)
- `security/validate-compliance` (veraPDF)
- `security/restore-pdf-protection` (JPDFium `copyEncryption`)
- `security/accessibility-report` (proprietary + veraPDF)
- `proprietary/signatures/*` (cloud signature workflows — SaaS-only
  anyway)

**Proprietary / AI / cloud-coupled** (not expected on desktop, but
worth listing since the desktop build uses the proprietary cascade):

- `convert/pdf/ua` — PDF/UA tagger is proprietary-only; already
  dependency-disabled in `endpointconfig`
- `form/form-detection/detect` (+ model status) — proprietary model;
  already dependency-disabled
- `docparse/ingest` (+ capabilities) — proprietary DocParse pipeline
- `ai/orchestrate`, `ai/orchestrate/stream`, `ai/tools/classify-and-label`
- `integration/external-api-call`, `integration/purview-*`
- `account-link/*`, `team/*`, `payg/*`, `notifications`, `invite/*`,
  `policies/*`, `processing-folders`, `storage/*`, `admin/*`,
  `proprietary/ui-data/*`, `user/*` — SaaS/account surface; the desktop
  layer already routes cloud calls to the SaaS host, not the local
  backend, so most of these never hit `stirling-server`

**Engine-gated Java endpoints with no Rust port** (subset of ABSENT
that a future port could recover):

- `convert/pdf/video` — dead upstream (ffmpeg CVEs, commented out in
  Java); nothing to port
- `misc/print-file` — dead upstream

**Group-table keys with no backing controller** (frontend-composed or
dead registry entries — not real losses): extract-pages, view-pdf,
multi-tool, adjust-contrast, pdf-to-json, json-to-pdf, pdf-to-rtf,
change-permissions, automate. The frontend decomposes these into routed
calls (e.g. extract-pages → split-pages machinery) or marks them
link-only.

## Recommended Phase B mechanism

`STIRLING_PDF_TAURI_MODE=true` + `JAVA_BACKEND_URL=off` is already set
by the sidecar. Two options for hiding ABSENT endpoints in the UI:

1. **Config-level (preferred)**: extend `endpointconfig` with a
   `desktop`/`tauri` profile that reports the crypto family +
   proprietary/AI groups as disabled through the existing
   `/api/v1/config/endpoints-availability` + `endpoint-enabled`
   endpoints the frontend already polls. Data-driven, testable, and the
   same surface Java's `EndpointInterceptor` uses.
2. **Env-level (zero code)**: `RUST_DISABLED_ENDPOINTS` already exists —
   the sidecar could export the crypto paths. Downside: hardcodes the
   list in the Tauri shell and bypasses the group mechanism.

Option 1 keeps desktop behaviour data-driven and consistent with how
engine-absence is already reported.

## Phase C/D notes

- Engine bundling should follow `endpointconfig` groups so each engine
  is independently optional; `find_executable` probes already degrade.
- Installer win: removing `libs/*.jar` + `runtime/jre/**` from
  `tauri.conf.json` resources and dropping `BackendKind::Java` once the
  Rust path is proven on all three desktop OSes.
- Open question for Phase C: whether `weasyprint`/`soffice`/
  `ebook-convert` ship at all — they dominate installer size; the
  endpointconfig groups can mark them unavailable on desktop.
- `pdf/html` has a real decision: Rust uses `pdftohtml` only, while
  Java's alternative was LibreOffice. Bundling `pdftohtml` (small)
  covers it.
