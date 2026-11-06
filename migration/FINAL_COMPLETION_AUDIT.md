# Phase 9 — Final Java→Rust Migration Gap Analysis

Audit date: 2026-11-06 (second pass; supersedes `FINAL_MIGRATION_AUDIT.md`,
which was written mid-flight at 78 routes). Method: mechanical extraction
of Java Spring mappings vs `rust/src/handlers/mod.rs` route table, plus a
real `cargo check` run (authorized this phase — earlier phases prohibited
builds).

## 1. Executive summary

**The Rust backend does not compile.** `cargo check` fails with 23 errors
across four files — `handlers/convert.rs` (9 errors: missing `base64`
dependency, type mismatches), `endpointconfig.rs` (1), `handlers/form.rs`
(11: unstable `str::as_str`, borrow errors, type mismatches), `config.rs`
(1: moved `settings`). Every route below is therefore **registered but
unverifiable** — no endpoint can be runtime-tested until the tree
compiles. This is the single highest-priority item; it blocks all
parity work.

Coverage has otherwise advanced substantially since the last audit:
~104 route paths are registered (was 78), the entire `/api/v1/form`
family (9 endpoints) and most of `/api/v1/convert` (11 endpoints,
including new Ghostscript/LibreOffice sidecar handlers in
`convert.rs`) are implemented and routed.

## 2. Inventory

| Metric | Count | Evidence |
|---|---|---|
| Java `*Controller.java` files (all modules) | 146 | `find app -name "*Controller.java"` |
| Java controllers with Spring mappings | 100 | grep `@*Mapping` |
| Java core API endpoint mappings (`/api/v1/**`) | ~150 | prior mechanical extraction (`FINAL_MIGRATION_AUDIT.md` §A); ~99 method-level mapping annotations recounted this pass — meta-annotations and class prefixes account for the difference |
| Rust handler modules | 18 files, ~18.8k LOC | `rust/src/handlers/` |
| Rust endpoint-style functions | ~90 | `#[endpoint]` attrs + `pub async fn` handlers |
| Registered route paths | ~104 | 99 unique `"/api/..."` literals in `mod.rs` + 5 `metrics::routes()` (`/api/v1/info/{status,health,uptime,requests,requests/all}`); `/health`, `/api/v1/ping`, `/api/v1/info/version` included in the 99 |
| Compiling | **NO — 23 errors** | `cargo check` output below |
| Unit tests present | 113 `#[test]`/`#[tokio::test]` | grep; none runnable while compile fails |
| Runtime-tested | 0 | no service started this phase |
| Java-parity-verified | 0 | unverifiable without a build |

## 3. `cargo check` failure inventory (verbatim, current tree)

| File:line | Error | Owner area |
|---|---|---|
| `src/handlers/convert.rs:18` | `unresolved import base64` — crate not in `Cargo.toml` | convert (sidecar work) |
| `src/handlers/convert.rs:1461` | `cannot find module base64` | convert |
| `src/handlers/convert.rs:696` | unused import `ImageFormat` (warning) | convert |
| `src/handlers/convert.rs:1144,715,739` | argument type mismatches | convert |
| `src/handlers/convert.rs:825` | expected `&[usize]`, found `&Vec<u32>` | convert |
| `src/handlers/convert.rs:1174` | `==` not implemented for `PdfToOffice` | convert |
| `src/handlers/convert.rs:1331` | `ZipFile::read_to_end` missing (needs `Read` trait or `std::io::Read` in scope) | convert |
| `src/endpointconfig.rs:367` | `HashMap<&str, HashSet<&str>>` built from `(&&str, _)` iterator | platform |
| `src/config.rs:139` | use of moved `settings` | platform |
| `src/handlers/form.rs:3101,3138,3171,3178,3312,3338` | `str::as_str` unstable (6 sites — `.as_str()` on an existing `&str`) | form |
| `src/handlers/form.rs:3664` | expected `Object`, found `i64` | form |
| `src/handlers/form.rs:3871` | type mismatch in fn args | form |
| `src/handlers/form.rs:2856` | returns value referencing parameter `font` | form |
| `src/handlers/form.rs:3462` | `*doc` borrowed mutable + immutable | form |

`cargo fmt --check`: 22 files with diffs (formatting drift across
concurrent agents; not a functional defect).

## 4. Reconciliation matrix (by area)

Legend: ✅ registered+implemented · 🔀 registered, partial modes proxy to
Java · ⬜ not implemented (Java fallback) · 🛠 implemented but **unregistered or
non-compiling**.

### general — ~19 endpoints: ✅ all routed
merge-pdfs, rotate-pdf, remove-pages, rearrange-pages, split-pages,
split-by-size-or-count, split-pdf-by-chapters, split-pdf-by-sections,
split-for-poster-print, scale-pages, crop, booklet-imposition,
multi-page-layout, overlay-pdfs, edit-table-of-contents,
extract-bookmarks, pdf-to-single-page, remove-image-pdf,
pdf-to-single-page… Partial-mode notes from prior audit still apply:
booklet/multi-page-layout don't port `/OCProperties` merge or
widget-annotation form-field copy (🔀).

### analysis — 8/8 ✅ (page-count, basic-info, page-dimensions,
document-properties, form-fields, annotation-info, font-info,
security-info).

### misc — ~24 endpoints: ✅ routed
All attachment/portfolio ops, compress/decompress, metadata, flatten,
show-javascript, auto-rename, auto-rotate, repair, mobile-scanner ×6,
add-stamp, unlock-pdf-forms, add-page-numbers, add-comments,
add-image(🔀 non-png/jpeg), replace-invert-pdf(🔀 raster modes),
extract-images, create/flatten-portfolio, batch-process-attachments.
⬜ remaining: ocr-pdf, extract-image-scans, auto-split-pdf,
remove-blanks, scanner-effect (all need rasterisation/OCR);
print-file is commented out in Java (dead upstream).

### security — 8 routed, ~10 Java endpoints
✅ sanitize-pdf, get-info-on-pdf, remove-cert-sign, redact (manual
overlay subset — `convertPDFToImage` proxies), add/remove-password,
prepare-pdf-session, add-watermark (🔀 image-only).
⬜ cert-sign, timestamp-pdf, validate-signature, verify-pdf,
validate-compliance, auto-redact, redact-execute,
restore-pdf-protection, hardware signing — need CMS/PKCS#/veraPDF/
JPDFium-class dependencies that do not exist in `Cargo.toml`
(PHASE-06-SECURITY.md §crypto analysis).

### filter — 6/6 ✅.

### form — 9/9 ✅ routed (fields, fields-with-coordinates, extract-csv,
extract-xlsx, fill, add-fields, edit-fields, modify-fields,
delete-fields). ⚠ All non-compiling until form.rs errors are fixed;
`/AP` appearance streams are approximations (documented divergence).

### convert — 11 routed, ~15 Java endpoints
✅ routed: img/pdf, pdf/img (Ghostscript), svg/pdf, file/pdf
(LibreOffice), pdf/word, pdf/presentation, pdf/xml, pdf/text,
html/pdf, pdf/html, vector/pdf, pdf/pdf-vector… (`mod.rs` lines
248–262). 🛠→⬜ unresolved: url/pdf, markdown/pdf, ebook/pdf,
eml/pdf, pdf/csv, pdf/epub, pdf/xlsx, pdf/video, pdf/pdfa,
cbz/pdf, cbr/pdf, pdf/cbz, pdf/cbr — Java fallback (engines or
codec gaps; see §5).

### platform / non-PDF
- ✅ native: `/health`, `/api/v1/ping`, `/api/v1/info/version`,
  `/api/v1/info/{status,health,uptime,requests,requests/all}`
  (metrics.rs), mobile-scanner session endpoints.
- 🔧 in progress: `auth.rs` (auth_guard middleware), `guard.rs`
  (endpoint enable/disable via `endpointconfig`), `settings.rs`
  (settings.yml load), `web.rs` (SPA static serving in proxy for
  non-`/api` paths), `config.rs` (settings merge).
- ⬜ proxied: UIDataController (footer/home/licenses/signature images),
  ConfigController app-config (feature flags — worth porting),
  LoginDisclaimer, additionalLanguageJs, SignatureImageController,
  pipeline handleData.

### proprietary/SaaS (~74 controllers)
Auth/users/teams/billing/audit/storage/MCP/policy/webhooks — out of
scope for the PDF gateway; stays Java by design.

## 5. Architectural blockers (dependency classification)

| Dependency | Needed by | Class | Note |
|---|---|---|---|
| Compile fix (23 errors) | everything | 1 — native | blocks all verification |
| Ghostscript (`gs`) | pdf/img, compress modes, pdfa, replace-invert CMYK | 2 — engine | already invoked via `convert::run`; needs ops packaging + graceful absent-engine UX (currently hard-fails vs Java's group-enable checks) |
| LibreOffice/unoserver | file/pdf, pdf→office, html?, markdown flag | 2 — engine | `office_to_pdf` wired; needs `soffice` presence detection parity with Java's `CheckProgramInstall` |
| HTML renderer (WeasyPrint/Chromium) | html/pdf, url/pdf | 2 — engine | convert.rs doc mentions WeasyPrint; verify which engine the in-flight code targets |
| Markdown→PDF | markdown/pdf | 2 — engine | pulldown-cmark + HTML path, or LibreOffice flag parity |
| Comic archive (cbz/cbr) | cbz/pdf, cbr/pdf, pdf/cbz, pdf/cbr | 2 — engine + codec | zip side exists (`zip` crate); cbr needs unrar/7z |
| Ebook (epub) | ebook/pdf, pdf/epub | 2 — engine | Java uses external converter — identify binary before porting |
| EML | eml/pdf | 3 — Java service or 1 — native mail parser | `mail-parser` crate is the natural Rust path |
| CSV table extraction | pdf/csv | 1 — native | positioned-text table reconstruction; hard, low value vs form/extract-csv already native |
| xlsx output | pdf/xlsx, extract-xlsx partially | 2/1 | `rust_xlsxwriter` or keep Java |
| Video | pdf/video | 2 — engine | FFmpeg + frame renderer |
| PDF/A | pdf/pdfa | 2 — engine | Ghostscript + veraPDF validation profiles |
| Rasteriser (PDF→bitmap) | remove-blanks, auto-split-pdf, scanner-effect, ocr-pdf, extract-image-scans, pdf/img fallback modes, text watermarks | 2 — engine | no pure-Rust renderer in tree; Ghostscript covers rasterisation where licensed/available |
| OCR (Tesseract/ocrmypdf) | ocr-pdf, extract-image-scans | 2 — engine | sidecar pattern applies |
| CMS/PKCS#7/X.509 signing, timestamp, signature validation | cert-sign, timestamp-pdf, validate-signature | 2 — engine or 4 — retain Java | `rasn-cms` family is the Rust path; large project |
| veraPDF | verify-pdf, validate-compliance | 2 — engine | external validator process |
| Redaction execute (glyph-aware removal) | auto-redact, redact-execute | 1 — native (hard) | needs text-geometry rewrite or raster fallback |
| JPDFium `PdfSecurity.copyEncryption` | restore-pdf-protection | 4 — retain Java or reintroduce native lib | deliberately removed dependency |
| PKCS#11/MSCAPI | hardware signing | 2 — engine / platform | `cryptoki`/`pkcs11` crates; desktop-locality check must port |
| AuthN/Z, users, teams, billing, audit, storage, MCP, policy, webhooks | proprietary controllers | 5 — out of scope | control-plane decision; proxy meanwhile |
| DB persistence, scheduled tasks, file storage beyond tempdir | platform | 3/5 | jobs are in-memory (restart loses jobs); settings now file-based |
| Logging/metrics | ops | 1 — native | `metrics.rs` covers request counts; tracing already present |
| Static frontend | SPA | 1 — native | `web.rs` serves build when configured |
| Graceful shutdown | ops | 1 — native | verify `axum::serve` shutdown signal wiring in main.rs |

## 6. Prioritized remaining work

### P0 — Critical correctness
1. **Fix the 23 compile errors** (convert.rs ×9, form.rs ×11,
   endpointconfig.rs ×1, config.rs ×1). Files are owned by concurrent
   agents — coordinate or assign one fixer. Acceptance: `cargo check`
   clean. Everything below is blocked on this.
2. `cargo fmt` the tree (22 files drifted) once edits settle.

### P1 — Security
3. No *new* security defects found in Rust code this pass; the exposure
   is that **nothing can be verified** while P0 stands. After compile:
   run the 113 unit tests + `cargo clippy --all-targets -- -D warnings`.
4. `auth.rs`/`guard.rs`/`endpointconfig.rs` are new middleware — need
   runtime verification that disabled endpoints 403 correctly and that
   `prepare-pdf-session` permission gates hold (PHASE-06 lists the
   manual checks owed).

### P2 — Missing core API implementations
5. Register-or-delete audit for convert.rs: 11 routes now registered;
   confirm `pdf_to_word/presentation/xml/text` macro-generated fns exist
   (line 1140 macro) — they do not appear in the `pub async fn` list,
   verify they're `#[endpoint]` fns.
6. `/api/v1/config/app-config` — frontend feature-flag dependency;
   small handler reading `settings.rs`.
7. Pipeline `handleData` — thin orchestrator once endpoint set freezes.

### P3 — External engine integrations (each = EngineSpec + endpoint)
8. Verify which engines `convert.rs` actually shells out to (gs seen;
   check for soffice/weasyprint/pdftohtml/rsvg in the file) and that
   absent-engine failures map to Java's group-enable 403s, not 500s.
9. Remaining convert endpoints: url/pdf, markdown/pdf, ebook/pdf,
   eml/pdf, pdf/csv, pdf/epub, pdf/xlsx, pdf/video, pdf/pdfa,
   cbz/cbr family (12 endpoints).
10. Raster-dependent misc: remove-blanks, auto-split-pdf,
    scanner-effect, ocr-pdf, extract-image-scans (5 endpoints) — decide
    Ghostscript-raster vs keep-Java per endpoint.

### P4 — Platform services
11. Job persistence (in-memory → restart loses running jobs; Java uses
    DB) — decide acceptable or port storage.
12. Auth/session parity for proprietary routes — out of scope unless
    standalone deployment is a goal.

### P5 — Testing & parity
13. Once compiling: `cargo test`, then `testing/parity.sh` Java-vs-Rust
    sweeps per area; record divergences in `rust/MIGRATION.md`.
14. Port the Python `engine/` pytest suite status into the audit
    (rust/engine is a separate crate with its own remaining work —
    agent runtime, orchestrator streaming, PostHog).

### P6 — Documentation/deployment
15. Update `rust/MIGRATION.md` verified-status section (done this pass).
16. Deployment: document required sidecars (`gs`, `soffice`,
    weasyprint/chromium, tesseract, ffmpeg) and their feature-flag
    interplay with `endpointconfig` group checks.

## 7. Parallel vs sequential

- **Parallel-safe**: P0 fixes are per-file (convert.rs, form.rs,
  endpointconfig.rs, config.rs are disjoint); engine integrations P3.8–9
  are per-endpoint once the runner API is frozen; platform items P4/P6.
- **Sequential**: P0 must land before ANY testing/parity (P5);
  `endpointconfig`/`guard` middleware correctness gates whether
  engine-absent endpoints should 403 or 500; `Cargo.toml` additions
  (base64 or replacement) serialize with other dep changes.

## 8. Bottom line

**What exactly remains before Rust can replace Java without losing
functionality?**

1. **One compile fix** — 23 errors, ~4 files, all in-flight agent work.
   Until then: 0/104 routes verifiable.
2. **~46 core endpoints still on Java fallback**: ~12 convert
   (url/markdown/ebook/eml/pdf-csv/epub/xlsx/video/pdfa/cbz/cbr),
   ~10 security crypto (cert-sign/timestamp/validate×3/redact-execute/
   auto-redact/restore-protection/hardware), ~5 raster/OCR misc,
   ~6 platform GETs (mostly optional proxy), pipeline handleData,
   config/app-config. Plus ~74 proprietary/SaaS controllers that are
   out of scope by design.
3. **Engine provisioning** — gs/libreoffice/html-renderer/ffmpeg/
   tesseract must be deployed and their absence must map to Java's
   enable/disable semantics.
4. **Verification** — 113 unit tests + parity sweeps have never run
   against a compiling tree; parity status is UNKNOWN for all 104
   routes, not "verified".
5. **Job persistence + auth** decisions for standalone deployment.
