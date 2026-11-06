# Phase 03 — Miscellaneous PDF Controller Migration

Audit of every miscellaneous endpoint against the Rust gateway. Verdict up
front: **everything that can run on `lopdf` + `image` + external binaries is
already migrated; every remaining endpoint needs page rasterisation, OCR, or
the Python/OpenCV pipeline, none of which the Rust architecture provides.**
They correctly stay on the Java fallback proxy.

## Endpoint status

| Java endpoint | Rust status | Notes |
|---|---|---|
| `POST /api/v1/misc/extract-images` | **Native** | `misc.rs` — embedded-image XObject extraction to ZIP. `png`/`jpeg` run natively; `gif`/unknown formats forward to Java (ImageIO codec set). |
| `POST /api/v1/misc/extract-image-scans` | **Java** | Renders every page (`PDFRenderer`, capped DPI) then runs `split_photos.py` (OpenCV) to find/crop/straighten photos → PNG or ZIP. Needs rasteriser + Python. |
| `POST /api/v1/misc/ocr-pdf` | **Java** | Tesseract pipeline: `languages`, `sidecar`, `deskew`, `rotatePages`, `clean`, `cleanFinal`, `ocrType`, DPI — needs render + OCR engine. |
| `POST /api/v1/misc/remove-blanks` | **Java** | Renders pages, binarises at `threshold`, classifies by `whitePercent`. An estimate-only `lopdf` implementation exists in `misc.rs` but is deliberately unrouted (`#[allow(dead_code)]`) — it can keep pages Java would drop. |
| `POST /api/v1/misc/auto-split-pdf` | **Java** | Divider detection = QR decode on rendered pages (150 DPI, retry 300 DPI, direct-image fast path), divider/duplex page removal, `<base>_N.pdf` ZIP. Needs rasteriser + QR decoder. |
| `POST /api/v1/misc/scanner-effect` | **Java** | Image-space effects on rendered pages: `quality` preset or `advancedEnabled` brightness/contrast/blur/noise, `rotation`/`rotate`/`rotateVariance`, `colorspace`, `resolution`, `border`, `yellowish`. Pure raster. |
| `POST /api/v1/misc/auto-rename` | **Native** | `automation.rs` — full parity (see `agent-05.md`). |
| `POST /api/v1/misc/auto-rotate-pdf` | **Native + partial proxy** | Text-direction detection, explicit rotations, inference, dry-run native; `detectionMode=osd` and the `auto`-mode OSD fallback proxy to Java (needs render + Tesseract). |
| `POST /api/v1/misc/repair` | **Native** | gs → qpdf → lopdf round-trip; same tool/exit-code semantics as `RepairController`. |
| `POST /api/v1/misc/add-page-numbers` | **Native** | `misc.rs`. |
| `POST /api/v1/misc/add-stamp` | **Native** | `visual.rs`. |
| `POST /api/v1/security/add-watermark` | **Native** | `visual.rs`. |
| `POST /api/v1/misc/add-image` | **Native** | `misc.rs`. |
| `POST /api/v1/misc/replace-invert-pdf` | **Native + partial proxy** | CMYK/inversion modes native; modes needing renderer/text layout forward to Java. |
| `POST /api/v1/misc/add-comments` | **Native** | `misc.rs`. |
| Attachment ops (`add-attachments`, `list-attachments`, `extract-attachments`, `extract-single-attachment`, `rename-attachment`, `delete-attachment`, `batch-process-attachments`) | **Native + partial proxy** | `misc.rs`/`attachments.rs`; PDF/A-related sub-modes forward to Java. |
| `POST /api/v1/misc/create-portfolio`, `flatten-portfolio` | **Native** | `misc.rs`/`attachments.rs`. |
| `POST /api/v1/mobile-scanner/*` (6 routes) | **Native** | `automation.rs` — full session API parity. |
| `POST /api/v1/misc/flatten` | **Native + partial proxy** | `flattenOnlyForms=true` native; full-page rasterisation proxies. |
| `POST /api/v1/misc/compress-pdf` | **Native + partial proxy** | gs/qpdf levels native; ImageMagick line-art mode proxies. |
| `POST /api/v1/misc/decompress-pdf`, `update-metadata`, `show-javascript`, `extract-images` etc. | **Native** | `misc.rs`. |
| `GET /api/v1/config/*` (`app-config`, `endpoint-enabled`, `endpoints-enabled`, `endpoints-availability`, `group-enabled`), `GET /api/v1/config/login-disclaimer` | **Java** | Config/state endpoints owned by `ConfigController`/`LoginDisclaimerController` under `/api/v1/config` — they report Java's settings model; proxying is correct until Rust owns configuration. |
| `POST /api/v1/misc/print-file` | **N/A** | Mapping is commented out in Java. |

"Blank-page insertion" has no dedicated Java endpoint — inserting pages is
covered by the general page-operation endpoints already routed.

## Required external engines (blockers)

| Capability | Needed by | Java uses | Rust path forward |
|---|---|---|---|
| Page rasterisation | extract-image-scans, ocr-pdf, remove-blanks, auto-split-pdf, scanner-effect, auto-rotate OSD | PDFBox `PDFRenderer` at capped DPI | **`hayro` 0.9** — the only maintained pure-Rust PDF rasteriser (no unsafe, ~1000-file regression suite; lacks encrypted files, blending/isolation, knockout groups). The project constraint is "no PDFium", so `hayro` is the candidate — a spike should validate output parity on scanned/image-only fixtures first. |
| OCR | ocr-pdf, auto-rotate OSD | Tesseract (`osd.traineddata`, language packs, hOCR/TSV) | `tesseract` crate (Leptonica/Tesseract FFI) — still needs a renderer for input. |
| QR decode | auto-split-pdf | ZXing via rendered page images | `rqrr` on `image`-crate rasters once `hayro` lands; the embedded-image fast path could use `rqrr` today but would miss vector-drawn QRs, so it stays proxied until rendering exists. |
| Photo detection/crop | extract-image-scans | Python `split_photos.py` + OpenCV | No Rust equivalent worth porting; keep Python call or accept Java. |
| Image codecs | extract-images (gif etc.), scanner-effect | ImageIO / ImageMagick | `image` crate covers png/jpeg; extend features or keep Java for the rest. |

## Partial-implementation policy applied

- `remove-blanks`: an `lopdf` content-stream heuristic exists in `misc.rs`
  but is intentionally unrouted — object inspection is not visual blank-page
  classification.
- `auto-rotate-pdf`: native for text-decidable pages; OSD-dependent pages
  report `tesseractUnavailable` (auto) or proxy (`osd` mode) rather than
  guessing.
- `extract-images`/`compress-pdf`/`replace-invert-pdf`/`flatten`/
  `add-attachments`: native where `lopdf`/`image` suffice; every
  renderer/ImageMagick/PDF-A-dependent parameter combination forwards to
  Java with field names intact.

## Verification status

Static audit only — endpoint inventory diffed against `handlers/mod.rs`
routes and Java controllers/request models. No builds, tests, or runtime
parity runs were performed in this phase; the raster endpoints were verified
by reading their Java pipelines (renderer DPI caps, Tesseract flags,
OpenCV script) rather than executing them. Recommended next step before
un-proxying anything: a `hayro` spike rendering the same fixtures Java uses
(scanned, image-only, text, corrupted PDFs) to measure parity.
