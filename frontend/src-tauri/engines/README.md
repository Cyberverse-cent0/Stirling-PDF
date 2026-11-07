# engines/

Staged conversion-engine payloads for the desktop bundle, produced by
`task desktop:engines` (build-tools `stage-engines`). Everything except
this README is generated and gitignored.

Layout per platform:

- `engines/<name>[.exe]` — the Tier-1 binaries (`gs`, `qpdf`,
  `pdftohtml`, `rsvg-convert`, `inkscape`, `unrar`, `unar`, `bsdtar`,
  `7z`, `rar`, `tesseract`, `magick`) resolved from the build host's
  package manager; on Windows each lands flat with its DLL closure.
- `engines/lib/` — shared-library closure (Linux `.so`, macOS `.dylib`
  with `@loader_path` install names). The Tauri shell exports it as
  `LD_LIBRARY_PATH` / `DYLD_FALLBACK_LIBRARY_PATH` to the sidecar.
- `engines/share/ghostscript/` — Ghostscript `Resource/` tree when the
  build host has one; exported as `GS_LIB`.
- `engines/share/tessdata/` — `*.traineddata` language data when the
  build host has it; exported as `TESSDATA_PREFIX` (user settings still
  win — it is only the fallback the backend consults).

`bundle.resources` ships the tree as `<resource_dir>/engines`, which the
Rust backend finds via `STIRLING_ENGINES_DIR`. In `tauri dev` the same
tree is mirrored to `src-tauri/binaries/engines` and found through the
`<exe>/engines` convention. Engines missing on the build host simply
aren't staged — `endpointconfig` probes disable those tool groups.

Not bundled (too large or Python-based — they resolve from the user's
`PATH`/`customPaths` and their tool groups stay disabled when absent):
`soffice`/`unoconvert` (LibreOffice, ~1 GB), `weasyprint`, `ocrmypdf`
(Python), `ebook-convert` (Calibre), `verapdf` (needs a JVM), `ffmpeg`.
