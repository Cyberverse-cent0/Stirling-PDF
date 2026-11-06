# Agent 05 — Misc API: Automation, Scanning and Document Repair

## Controllers inspected

| Java controller | Route | Status |
|---|---|---|
| `AutoRenameController` | `POST /api/v1/misc/auto-rename` | Implemented natively |
| `AutoRotateController` (+ `AutoRotateDetection`, `AutoRotatePdfRequest`, `AutoRotateAnalysisResult`) | `POST /api/v1/misc/auto-rotate-pdf` | Implemented natively for `text`/`auto`; `osd` proxied to Java |
| `AutoSplitPdfController` | `POST /api/v1/misc/auto-split-pdf` | Not migrated — requires page rendering + QR decoding; falls through to Java proxy |
| `RepairController` | `POST /api/v1/misc/repair` | Implemented natively (gs → qpdf → lopdf round-trip) |
| `MobileScannerController` + `MobileScannerService` | `/api/v1/mobile-scanner/*` | Implemented natively (session/file-transfer API only) |

## Implementation notes

### auto-rename

- New positioned-text extractor (`rust/src/text.rs`) walks content streams, tracking `q`/`Q`, `cm`, `BT`/`ET`, `Tf`, `Td`/`TD`, `Tm`, `T*`, `TL`, `Tj`/`TJ`/`'`/`"` and form XObjects (depth-capped at 16), decoding strings through lopdf's per-font `Encoding`.
- Mirrors the Java `PDFTextStripper` subclass: groups runs into lines by baseline `y`, caps at the same effective 199 collected lines (`LINE_LIMIT = 200` starts a 200th line that is never flushed), merges adjacent equal-font-size lines, sorts descending, picks the largest heading.
- `useFirstTextAsFallback` preserved; it is dead code in Java too (the fallback branch is only reachable when the merged list is empty, which yields `null` either way).
- Filename rule preserved: sanitized header + `.pdf` when the UTF-16 length is `< 255`, stripping exactly `[/\\?%*:|"<>]`; otherwise `Filenames.toSimpleFileName(original)` (basename). PDF bytes pass through `pdfdoc::load`/`save` unchanged in content.

### auto-rotate-pdf

- `detectionMode` validated against `auto|text|osd`; anything else → 400 like Java's `INVALID_ARGUMENT`.
- `pageRotations` JSON: explicit rotations skip detection entirely; validates 1-based in-range pages, multiples of 90, no repeats; rotations are additive `floorMod(current + angle, 360)`.
- `detectionMode=osd` is forwarded verbatim to Java via `proxy::forward_multipart` — OSD needs PDF rendering at 150 DPI plus Tesseract, neither of which exists in the lopdf-only architecture.
- `auto`/`text`: per-page text-direction detection runs natively on the positioned extractor. Java's conclusive rule is reproduced: ≥30 glyphs at ≥95% dominance or ≥8 glyphs at ≥99%; `correction = floorMod(dominantDirection − currentRotation, 360)`; `confidence = dominance * 100`; `apply = correction != 0`.
- Inconclusive pages: `text` mode reports `tooFewGlyphs`/`noDominantDirection`; `auto` mode reports `tesseractUnavailable` (see Blockers).
- `inferUndetected` (default true) reproduces the consensus algorithm: per current-rotation agreed correction, conflicts disqualify, inferred pages get `method=inferred`, `confidence=null`, `note=inferredFromDocument`.
- `dryRun=true` returns the `AutoRotateAnalysisResult` JSON shape (`pages`, `totalPages`, `pagesToRotate`, `detectedByText`, `detectedByOsd`, `inferred`, `undetected`). Otherwise applied rotations are saved as `<base>_auto_rotated.pdf`.
- `confidenceThreshold` is validated (`@Min(0)` → 400) though only OSD consumes the value.
- `?async=true` supported via the shared `misc::dispatch` job framework.

### repair

- Tries `gs -o <out> -sDEVICE=pdfwrite <in>` first, then `qpdf --qdf --object-streams=disable <in> <out>` (exit 0 or 3 = success), in a `tempfile::tempdir`.
- Each tool is gated on `endpointConfiguration.isGroupEnabled("Ghostscript")`/`("qpdf")` via `endpointconfig::is_group_enabled` — a disabled group is skipped outright, matching `RepairController.isGhostscriptEnabled`/`isQpdfEnabled` (not merely binary presence).
- The lopdf load+save fallback only runs when *both* tool groups are disabled (the PDFBox-equivalent structural round-trip); if a group is enabled but its binary is missing or declines the file → 422 `"This document is damaged in a way the repair tools cannot fix."` (Java's `PdfUnrepairableException` mapping).
- Output name: `<base>_repaired.pdf` via `output_name`/`remove_extension` (Java `GeneralUtils.generateFilename`).

### mobile-scanner

- Full session API ported: `create-session`, `validate-session`, `upload`, `files`, `download`, `delete session` — same paths, methods, JSON shapes, and status codes as `MobileScannerController`.
- Sessions live in `std::env::temp_dir()/stirling-mobile-scanner/<sessionId>` behind a global `Mutex<ScannerState>`; a background sweeper (5 min interval) purges expired sessions and orphaned directories, mirroring `@Scheduled` cleanup.
- Limits preserved: 10 min expiry, 50 MB/file, 500 MB/session, 500 files/session, 2 GB total, 1000 live sessions; per-file/session caps → 413, server-capacity → 503, expired/unknown session → 400.
- Session IDs restricted to `[A-Za-z0-9-]`; filenames sanitized to `[A-Za-z0-9._-]` with `_` substitution and `upload-<millis>` fallback; collision rule `name-N.ext` using Java's `[.][^.]+$` extension split. All paths are `join`ed under the validated session dir — no raw user input reaches the filesystem.
- Upload streams multipart `files` parts into per-file `tempfile::NamedTempFile` spools with a running byte cap (Java's `DiskFileItem` behaviour — a batch never holds every file in RAM), then copies spool→session-dir under the lock. Capacity is reserved under the lock before writing (Java's `reserve`), with rollback of `fileCount`/`storedBytes` for unstored files.
- Download claims a file (`downloaded` map), streams it, deletes file + bookkeeping only after a complete read; early stream termination releases the claim via a `Drop` guard (Java's `releaseDownloadClaim`). Content type via extension map approximating `MediaTypeFactory`; `Content-Disposition: attachment`.
- Feature gate: `settings::mobile_features_enabled` resolves `system.enableMobileScanner`/`enableMobileSignature` from the loaded `settings.yml`/`custom_settings.yml` (both default true), with `SYSTEM_ENABLEMOBILESCANNER`/`SYSTEM_ENABLEMOBILESIGNATURE` env overrides — matching Java's `isEnableMobileScanner || isEnableMobileSignature`. Disabled → 403 JSON except download, which returns a bare 403 like Java.
- Camera capture and on-device image processing remain client-side; nothing server-side was required for them.

## Files modified

- `rust/src/handlers/automation.rs` — new: all endpoints above.
- `rust/src/text.rs` — new: positioned text extraction shared by auto-rename/auto-rotate.
- `rust/src/handlers/mod.rs` — `pub mod automation;` + route registrations.
- `rust/src/error.rs` — added `PayloadTooLarge`, `Unprocessable`, `Forbidden` variants (`Forbidden` also used by `passwords.rs`).
- `rust/src/settings.rs` — added `mobile_features_enabled` (shared Phase-7 file; no other changes).

## External dependencies

- **Ghostscript (`gs`)** and **qpdf** — optional; repair uses whichever exists, lopdf round-trip when neither does.
- **Tesseract + PDF renderer** — required for `detectionMode=osd` (proxied to Java) and for the `auto`-mode OSD fallback on text-inconclusive pages (not available in Rust).
- **QR decoder + renderer** — required for `auto-split-pdf` divider detection (not migrated).
- No new Cargo dependencies; uses existing `lopdf`, `axum`, `tokio`, `futures-util`, `bytes`, `tempfile`, `serde_json`.

## Blockers / unsupported functionality

- `auto-split-pdf`: divider detection renders pages (PDFBox `PDFRenderer` at 150/300 DPI) and decodes QR codes — no renderer or QR crate in the Rust architecture. Requests fall through to the Java proxy unchanged.
- `auto-rotate-pdf` `detectionMode=osd`: proxied to Java verbatim.
- `auto-rotate-pdf` `detectionMode=auto`: pages the text signal cannot decide are reported `tesseractUnavailable` and left unrotated; Java would run Tesseract OSD on them when available. Fully text-decidable documents still rotate natively.
- `repair`: gs/qpdf gating now follows settings endpoint-group enablement; when both groups are disabled the lopdf fallback runs. When a group is enabled but its binary is missing or declines the file the result is still a repair failure (matching Java, where an enabled-but-absent tool just fails its attempt).
- `text.rs` approximates PDFBox: per-operation baseline origins (not per-glyph advances), no Type3/CID metric detail, render mode ignored — sufficient for heading-size grouping and direction histograms, not a general text extractor.

## Required routes

```
POST /api/v1/misc/auto-rename
POST /api/v1/misc/auto-rotate-pdf
POST /api/v1/misc/repair
POST /api/v1/mobile-scanner/create-session/{sessionId}
GET  /api/v1/mobile-scanner/validate-session/{sessionId}
POST /api/v1/mobile-scanner/upload/{sessionId}
GET  /api/v1/mobile-scanner/files/{sessionId}
GET  /api/v1/mobile-scanner/download/{sessionId}/{filename}
DELETE /api/v1/mobile-scanner/session/{sessionId}
```

`POST /api/v1/misc/auto-split-pdf` intentionally has no Rust route — it proxies to Java.

## Formatting verification

- `rustfmt --edition 2021` run on `src/handlers/automation.rs`, `src/text.rs`, `src/handlers/mod.rs`, `src/error.rs`, `src/settings.rs` only (this session: `automation.rs`, `settings.rs`).
- No builds, compilation, Cargo tests, parity tests, or runtime execution were performed, per the task restrictions. Compile-correctness was reviewed manually against the lopdf/axum/tempfile APIs in the local cargo registry.
