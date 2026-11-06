# Phase 3 — Standalone / Java-Independence Audit

## Verdict

**The Rust backend is fully standalone — the Java proxy has been removed entirely.** `src/handlers/proxy.rs` is deleted, `config::JavaBackend`/`JAVA_BACKEND_URL` no longer exist, `AppState` carries no backend, and every former `forward_multipart`/`forward_parts` delegation site now either runs natively or returns an explicit 400/404. Verified live: the server boots and serves migrated endpoints with zero Java configuration.

## Evidence

- `git status`: `D rust/src/handlers/proxy.rs`; `main.rs` constructs `AppState::persistent(None)` unconditionally.
- `grep -rn "JavaBackend\|JAVA_BACKEND_URL\|forward_multipart\|forward_parts\|proxy::" src/` → **zero hits** in executable code.
- `jobs.rs`: unknown job/file IDs → `AppError::NotFound` (`job_not_found`/`file_not_found`); the file header documents "there is no second backend owning" them.
- `handlers/mod.rs`: fallback is now a local fn — `/api/**` → 404 problem+json, everything else → `web::serve` (SPA). No proxy leg remains.
- Unported parameter modes return honest errors, verified live:
  - `POST /misc/flatten` (raster mode) → 400 "…the Rust backend does not implement"
  - `POST /security/add-watermark` (`watermarkType=text`) → 400 "text watermarks are not implemented…"
  - `POST /misc/compress-pdf?lineArt=true`, `replace-invert-pdf` non-CMYK modes, `add-stamp` text type, `auto-rotate-pdf?detectionMode=osd`, `extract-images` unsupported codec sets, `useStirlingOfficeConvert` office branches → all `AppError::BadRequest` rejections (per handler diffs).
- `AppState::new/persistent` keep an ignored `Option<()>` parameter "for the Java-proxy era compiling" — a deliberate, documented compat shim for in-flight callers; harmless.

## External (non-Java) engines — allowed and verified

Ghostscript, LibreOffice, Tesseract/OCRmyPDF, qpdf, unoconvert, weasyprint, calibre, rar/unrar, Chromium, FFmpeg — argv-only `EngineSpec`/`Command` execution, `endpointconfig` boot probes disable dependent groups with warnings when binaries are absent (observed: qpdf, unoconvert, weasyprint, tesseract, ocrmypdf, rar, calibre, OpenCV all warned+disabled on this host).

## Runtime proof (end-state binary)

```
PORT=18124 ./target/debug/stirling-server
→ health 200, version {"runtime":"rust"}
→ rotate-pdf 200 application/pdf (valid PDF)
→ convert/pdf/text?outputFormat=txt 200 text/plain  (P1 fix verified)
→ unmigrated /api route → 404 problem+json
→ unknown job id → 404 problem+json
→ unported modes → 400 problem+json with explicit "not implemented" detail
```

Job persistence: `configs/stirling-jobs.db` (SQLite via `jobstore.rs`), survives restarts — verified 4 persisted jobs loaded on boot.

## Historical note

During the audit window the codebase transitioned from "opt-in `JAVA_BACKEND_URL` proxy" to "proxy deleted". Earlier checkpoints in this phase's other reports describe the proxy state; this document reflects the final state. `MIGRATION.md`, `REMAINING_WORK.md`, and `SHELL-ELIMINATION-PLAN.md` were updated by the migration agents accordingly.
