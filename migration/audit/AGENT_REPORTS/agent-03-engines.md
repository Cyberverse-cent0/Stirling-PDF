# A03 — PDF Engines & External Dependencies (round 2, 2026-11-07)

## 1. Engine inventory

| Engine | Used by | Invocation | Discovery | Bundled? | Status |
|---|---|---|---|---|---|
| `gs` (Ghostscript) | Rust: pdf→img, pdfa/pdfx, vector↔pdf, cbr/cbz render, flatten, rasterize, blank-page detect, optimize, repair, cmyk, ocr post-pass, tesseract fallback rasterizer, ebook optimize | `EngineSpec`/`Command::new`, argv, timeouts 120–600 s | probe `ENGINE_PROBES`→"Ghostscript"; `find_executable` | Yes — tier-1 + `share/ghostscript` + `engines/lib` + `GS_LIB` | Required for many endpoints; no customPaths (parity w/ Java) |
| `qpdf` | Rust: repair, compress, pdfa pre-clean, optimize (`--linearize` etc.), `QPDF_ZOPFLI` env on unix | `EngineSpec` | probe→"qpdf"; min ver 12.0.0 | Yes — tier-1 | Required-ish (best-effort fallbacks) |
| `soffice` | Rust: office→pdf / pdf→office (`file-to-pdf` family) | `EngineSpec --headless`, per-call `-env:UserInstallation` temp profile | probe→"LibreOffice"; `customPaths.operations.soffice` | No (too big) | Optional; LibreOffice group |
| `unoconvert` | Rust: tried before soffice for office conversions | `EngineSpec --convert-to` | probe→"Unoconvert"; `customPaths.operations.unoconvert` | No | Optional |
| `weasyprint` | Rust: html/url/markdown/eml→pdf | `EngineSpec -e utf-8 --pdf-forms --allowed-protocols` | probe→"Weasyprint"; `customPaths.operations.weasyprint`; min ver 58.0 | No | Optional |
| `pdftohtml` | Rust: pdf→html only | `EngineSpec -c` in tempdir | probe→"Pdftohtml" | Yes — tier-1 | Optional |
| `tesseract` | Rust: per-page OCR fallback in ocr-pdf; tessdata dir resolution | `EngineSpec <img> <base> -l <langs> pdf` | probe→"tesseract"; `system.tessdataDir`/`SYSTEM_TESSDATADIR`/`TESSDATA_PREFIX`/`share/tessdata` | Yes — tier-1 + `share/tessdata` | Optional (OCRmyPDF preferred) |
| `ocrmypdf` | Rust: ocr-pdf primary | `EngineSpec` full flag mapping + `--jobs 1` retry | probe→"OCRmyPDF"; `customPaths.operations.ocrmypdf` | No | Optional |
| `ebook-convert` (Calibre) | Rust: ebook→pdf, pdf→epub/azw3 | `EngineSpec`, 30-min timeout | probe→"Calibre" via `customPaths.operations.calibre` | No | Optional; **Java sandbox-confinement check not ported** |
| `verapdf` | Rust: PDF/A `strict` validation only (`validate_pdfa`, `run_verapdf`) | `EngineSpec --format text [--flavour]` | `find_executable` only — **not probed, not in customPaths, not bundled** | No | Optional validator; strict→error, non-strict→skip |
| `magick` (ImageMagick) | **Nothing in Rust** — `compress-pdf lineArt=true` rejected (`misc.rs:467-477`) | — | probed→"ImageMagick" | Yes — tier-1 (wasted) | **Dead dependency** |
| `rar` | Rust: pdf→cbr creation (`rar a -m5 -ep1`) | `EngineSpec` | probe→"rar" | Yes — tier-1 | Required for pdf-to-cbr |
| `unrar`,`unar`,`bsdtar`,`7z` | Rust: cbr→pdf extraction (first success wins) | `EngineSpec` | `find_executable` per spec; **none probed** | Yes — tier-1 | Needed for cbr-to-pdf; ungated |
| `rsvg-convert`,`inkscape` | Rust: svg→pdf (raster then vector fallback) | `EngineSpec` | `find_executable`; **not probed, no group** | Yes — tier-1 | Endpoint always enabled; runtime 500 if both absent |
| `ffmpeg` | Nothing (Rust); Java has dead `FFMPEG` executor + probe; pdf-to-video commented out both sides | — | deliberately unprobed | No | Dead/obsolete both sides |
| `hayro` (crate) | Rust: PDF rasteriser, primary in `BlankRasterizer` | in-process | n/a | n/a | Active |
| `lopdf` (crate) | Rust: nearly all PDF manipulation | in-process | n/a | n/a | Core |
| `@embedpdf/engines` pdfium (WASM) | Frontend editor only | in-browser worker | npm | n/a | Client-side |
| Python engine `engine/` | Java `AiEngineClient` → `aiEngine.url` (default `http://localhost:5001`) | HTTP | — | Docker image (archived `runtime/`) | **Alive — parity oracle** |
| `stirling-engine` `rust/engine` | Same contract/port; `task dev:*`/`engine:bg` run it | HTTP | — | `rust-release.yml` tarballs | **Alive — cut-over default** |

Discovery order (all engines): `customPaths.operations` (5 keys: weasyprint, unoconvert, calibre→ebook-convert, ocrmypdf, soffice) → per-spec `path_dirs` (unused) → `STIRLING_ENGINES_DIR` → `<exe>/engines` → `PATH`. Implemented `rust/src/convert.rs:98-170`, `settings.rs:410-456`, probed `endpointconfig.rs:485-570`.

## 2. Findings

### A03-F01 — `magick` probed/bundled/CLI-installable but never invoked — P2 dead dependency
Locations: `endpointconfig.rs:72,337`; `frontend/src-tauri/build-tools/src/main.rs:339` (TIER1); `engines/README.md:11`; `rust/DEPLOYMENT.md:18,45`; `cli/src/services/engine_install.rs:69,79,89,118,162,176`; `engine_detector.rs:46,92,107`. No `EngineSpec`/`Command::new` for magick anywhere in `rust/src`. Java only uses it in proprietary `ImageMagickLineArtConversionService.java`. Fix: remove from probes/TIER1/install specs/docs, or implement lineArt.

### A03-F02 — Tool-group/alternative table copied from Java disables/enables endpoints the Rust code doesn't need — P2
`endpointconfig.rs` `group_table()`/`alternatives()` transcribed from Java but Rust handlers differ:
- pdf-to-markdown alternative `Pdftohtml` (`endpointconfig.rs:393`) yet handler is pure `pdftext` — missing pdftohtml → 403 on working endpoint.
- pdf-to-html lists `LibreOffice` alternative (`:392`) but handler only runs `pdftohtml` (`convert.rs:3212-3218`); pdftohtml absent + soffice present → enabled → runtime 500.
- auto-rotate-pdf ∈ `tesseract` group (`:338`) but Rust detection is pure text-extraction; `detectionMode=osd` rejected anyway → missing tesseract → 403 on working endpoint.
- html-to-pdf/url-to-pdf ∈ `Python` group (`:236`), no alternatives → `probe_python` disables them without `python3` though Rust uses only weasyprint+reqwest (`convert.rs:1936,2160`).
- extract-image-scans: Java maps to `Python`+`OpenCV`; Rust `group_table` has no OpenCV entry — divergent gating (probably benign; unverified intent).
Fix: reconcile group membership/alternatives with actual Rust engine usage, or add missing fallbacks.

### A03-F03 — CBR extractor engines ungated; `cbr-to-pdf`/`pdf-to-cbr` fail at runtime instead of dependency-403 — P2
`convert.rs:967-1014` (`RAR_EXTRACTORS = unrar|unar|bsdtar|7z`), `1039-1056` (→ `AppError::Internal` 500), `1153-1224` (pdf-to-cbr uses gs + `rar`); `endpointconfig.rs:319` (`("rar", &["pdf-to-cbr"])` only). None of the four extractors probed; `cbr-to-pdf` in no group; `pdf-to-cbr` also needs gs but isn't in Ghostscript group → enabled endpoint → 500.

### A03-F04 — veraPDF: Java bundled library; Rust external CLI unprobed/unbundled/unconfigurable — P2
`convert.rs:3442-3513,3520-3578`; `settings.rs:421-443` (no verapdf field); Java `VeraPDFService.java:10-20`. Absent CLI → non-strict silently skips validation (`tracing::info`), strict errors; `customPaths.operations` cannot point at wrapper. PDF/A outputs on typical Rust installs never actually validated.

### A03-F05 — Bundled engine versions are unpinned host-PM binaries — P2
`build-tools/src/main.rs:327-340,403-421,531-592` copies whatever `find_on_path` resolves; CI installs via package manager (`tauri-build.yml:239-259`, `multiOSReleases.yml:283`); only weasyprint≥58/qpdf≥12 enforced (`endpointconfig.rs:78`). Non-reproducible bundles; vulnerable engine version ships to all users.

### A03-F06 — Untrusted-archive extraction via external tools, no post-extraction containment — P2
`convert.rs:978-1014` (`unrar x -o+`, `unar -o <dest> -f`, `bsdtar -xf -C`, `7z x -y -o`), then `collect_files_recursive` (`1019-1037`). No validation extracted paths stayed under dest; bsdtar/7z have traversal CVE histories; binaries unpinned (F05); `unar -f` forces overwrite.

### A03-F07 — `engine_available` `--version` fallback is unreachable dead code — P3
`endpointconfig.rs:705-718` vs `convert.rs:175-181`: `find_executable` miss → runs `EngineSpec --version`, but `convert::run` itself calls `find_executable` and errs before spawning → fallback can never succeed.

### A03-F08 — Windows `.bat`/`.cmd` candidates can resolve to non-CreateProcess-spawnable files — P3
`convert.rs:86-96` adds `exe|com|bat|cmd` on Windows; `is_exe` only checks `is_file()` (`convert.rs:123-126`). Batch files need `cmd.exe` → probe reports Broken → group disabled; a `.bat` in an earlier PATH dir wins over a real `.exe` later.

### A03-F09 — Rust drops Java's process-concurrency/confinement machinery for soffice/Calibre — P3
Java `ProcessExecutor` semaphores, `UnoServerPool` auto endpoints, `libreOfficeSandboxed`, `CalibreSandboxPolicy`; Rust `convert.rs:1696-1737` fresh soffice per call (no pooling → slower under concurrency), `endpointconfig.rs:480-484` Calibre confinement "skipped".

### A03-F10 — `MIN_ENGINE_VERSIONS` substring match lets renamed custom paths bypass version floors — P3
`endpointconfig.rs:523-535` `cmd.contains(name)`; Java compares against configured path itself (`ExternalAppDepConfig.java:150-179,208-215`). `customPaths.operations.weasyprint=/opt/wp/bin/convert` → 58.0 floor skipped.

### A03-F11 — `-dSAFER` applied inconsistently across Ghostscript invocations — P3
Present: `convert.rs:668,893,3082,3152`, `misc.rs:349`, `security.rs:2139`. Absent: `convert.rs:1128,1547,2893,3610,3623`, `misc.rs:717,2262,2817`, `ocr.rs:251`. Java never passes `-dSAFER` (0 hits) — not a parity regression; gs≥9.50 defaults SAFER. Low risk via argv (no shell).

### A03-F12 — `stirling engines` CLI under-reports real engine set — P3
`cli/src/services/engine_detector.rs:15-21,87-108`: `EngineKind` lacks weasyprint, ocrmypdf, pdftohtml, ebook-convert, verapdf, rar-archivers, rsvg-convert, inkscape. `PDF_ENGINES` = gs, qpdf, tesseract, libreoffice, imagemagick only.

### A03-F13 — Dual AI engines (`engine/` Python vs `rust/engine`) — P3 tracked divergence
`rust/engine/Cargo.toml:6` "replaces"; `rust/engine/README.md:5-7` cut-over done, Python = parity oracle. `Taskfile.yml:229-237` wires Rust; Python still fully wired (`.taskfiles/engine.yml`, `ai-engine.yml`). Frontend never calls engine directly; Java `AiEngineClient`/`AiProxyService` → `aiEngine.url` default :5001. Tracked at `MIGRATION_BOARD.md:38`, `REMAINING_WORK.md:383`.

### A03-F14 — macOS universal desktop builds ship single-arch engines — P2
`.taskfiles/desktop.yml:70-90` (universal stirling-server via lipo), `build-tools/src/main.rs:596-689` (`stage_engines_macos` copies host-PATH binaries). arm64 Mac stages arm64-only engines → universal .app ships x86_64-incompatible engines → Intel Macs lose all engine features (graceful degradation, silent).

## 3. Positive confirmations

- All engine invocations use argv arrays via `Command`/`EngineSpec` — **no shell interpolation** in `rust/src`.
- Hard timeouts on every spec (default 120 s; 300 s conversions; 30 min calibre/ocrmypdf/pdfa), output drained on reader threads, timeout kills child.
- Unix exec-bit enforced; Windows PATHEXT candidates; wrong-arch/missing-dylib → `Broken` → group disabled.
- Probe set otherwise matches Java restricted to implemented call sites; `ffmpeg` deliberately unprobed both sides; min-version floors match Java.
- `customPaths.operations` parity (same 5 keys).
- Desktop staging thorough: per-OS lib closure (Linux ldd, macOS otool+install_name_tool+codesign, Windows DLL flattening + gswin64c→gs.exe), share trees, `.keep`, dev-mirror; Tauri shell exports `STIRLING_ENGINES_DIR`, `LD_LIBRARY_PATH`/`DYLD_FALLBACK_LIBRARY_PATH`, `GS_LIB`, `TESSDATA_PREFIX`; `main.rs:75-87` re-exports `engines/lib`.
- `tauri.conf.json` `externalBin: binaries/stirling-server`; `tauri.backend-rust.conf.json` ships only `engines/**/*`; `DESKTOP_BACKEND=java` legacy preserved.

## 4. Unverified

- Whether bundled bsdtar/7z/unrar/unar builds sanitize `..`/absolute paths (extractor-version dependent; F06 fix recommended regardless).
- Java-side per-endpoint argv construction not fully audited (Rust is audited backend).
- Whether `extract-image-scans` intentionally dropped Java's Python/OpenCV group gating — needs owner confirmation.
