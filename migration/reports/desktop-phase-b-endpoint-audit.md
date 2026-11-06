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

## Phase B mechanism — implemented

The Java proxy is gone entirely (`handlers/proxy.rs` deleted; the
fallback 404s every unrouted `/api/**`), so unrouted endpoints are dead
on **every** `stirling-server` deployment — the disable is unconditional,
not desktop-gated.

- `endpointconfig::UNSUPPORTED_ENDPOINTS` lists the keys Java implements
  but Rust has no route for (crypto family + proprietary/AI surface);
  they are disabled with a new `DisableReason::NotSupported`.
- `reason_value` serializes it as the frontend's existing
  `NOT_SUPPORTED_LOCALLY`, which `endpointAvailability.ts` already
  renders.
- `endpoint_availability`/`is_endpoint_enabled` now translate
  `/api/v1/...` paths to endpoint keys, so path-spelling callers get the
  same answer as key-spelling ones.
- Deliberately **not** disabled: group-table keys with no Java controller
  (`extract-pages`, `compare`, `multi-tool`, `adjust-contrast`,
  `automate`, `pipeline`, `change-permissions`, `remove-annotations`,
  `pdf-to-json`, `json-to-pdf`, `pdf-to-rtf`, `pdf-to-video`, `sign`) —
  several are frontend-composed tools that decompose into routed calls,
  and Java reports them enabled too (parity).

Engine-gated endpoints need no listing here: `ENGINE_PROBES` already
disables each tool group when its binary is absent.

## Phase C/D notes

- Engine bundling should follow `endpointconfig` groups so each engine
  is independently optional; `find_executable` probes already degrade.
- Installer win: removing `libs/*.jar` + `runtime/jre/**` from
  `tauri.conf.json` resources and dropping `BackendKind::Java` once the
  Rust path is proven on all three desktop OSes.
- `pdf/html` has a real decision: Rust uses `pdftohtml` only, while
  Java's alternative was LibreOffice. Bundling `pdftohtml` (small)
  covers it.

### Phase C — recommended per-platform engine set

Tier 1 — bundle everywhere (small, high-value):
`gs`, `qpdf`, `pdftohtml`, `rsvg-convert` (or `inkscape` fallback),
`unrar`/`unar`/`bsdtar`/`7z` + `rar`. Covers pdf→img/vector/pdfa/epub,
compress/repair, pdf→html, svg→pdf, cbr/cbz both ways, redact
`convertPDFToImage`, flatten, remove-blanks, extract-image-scans.

Tier 2 — bundle if size permits: `tesseract` + `ocrmypdf` + traineddata
(~50 MB) for ocr-pdf/auto-rotate; `verapdf` for pdfa validation.

Tier 3 — do not bundle (each dominates installer size for narrow
coverage): `weasyprint` (html/url/markdown/eml→pdf — Python+GTK),
`soffice`/`unoconvert` (file→pdf, pdf→word/presentation/xml/rtf —
~400 MB LibreOffice), `ebook-convert` (ebook↔epub — Calibre).
These groups stay disabled on desktop; the endpoints report
`DEPENDENCY`-disabled through the same availability surface, so the UI
hides them consistently.

Phase D prerequisite: a real `tauri build` on each OS — `cargo check`
does not exercise `externalBin` resource resolution or the
`binaries/stirling-server-<triple>` staging path.

## Addendum — engine resolution for bundled desktop installs

`convert::find_executable` now searches, after per-spec `path_dirs` and
before `PATH`: `STIRLING_ENGINES_DIR` (path-separated) then
`<dir-of-stirling-server>/engines/`. The Tauri sidecar exports
`STIRLING_ENGINES_DIR=<resource_dir>/engines`, so installers can drop the
tier-1 binaries into a resources `engines/` folder without touching
`PATH`. All `Command::new` engine call sites that bypassed `EngineSpec`
(`misc.rs` gs/qpdf helpers, `automation.rs` repair, `BlankRasterizer`)
now resolve through `convert::resolve`, which honours the same search
order and passes qualified `customPaths` entries through untouched.
Group probes (`ENGINE_PROBES`, `engine_available`) inherit the bundled
dirs for free, so a bundled `gs` enables its groups exactly like a
system-installed one.
