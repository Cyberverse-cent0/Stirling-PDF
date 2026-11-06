# Rust Migration — Integration Report

Date: 2026-11-06. Integration pass over the parallel agent migration
work, followed by full compile/lint/test verification of `rust/`.

## A. Integration summary

### Agents inspected

Reports read: `migration/reports/agent-01.md`, `agent-02.md`,
`agent-07.md`, `agent-09.md`, `agent-10.md` (agents 03–06, 08 left no
reports; their work is visible directly in the tree).

### Orphaned work found and wired

| Contribution | State found | Resolution |
|---|---|---|
| `handlers/filter.rs` (6 filter endpoints) | file present, module undeclared | `pub mod filter` + 6 routes (registered by parallel agent during this pass) |
| `handlers/passwords.rs` (add/remove-password) | file present, module undeclared | `pub mod passwords` + 2 routes |
| `handlers/visual.rs` (add-stamp, add-watermark) | file present, module undeclared | `pub mod visual` + 2 routes |
| `handlers/form.rs` (form fields/csv) | landed mid-pass | `pub mod form` + 3 routes so far; remaining form endpoints still proxy |
| `handlers/automation.rs` (auto-rename/rotate/repair, mobile-scanner×6) | landed mid-pass | declared + 9 routes |
| `src/text.rs`, `src/fontmetrics.rs` | undeclared in `main.rs` | `mod` declarations added |
| `misc.rs` finished handlers (`unlock_pdf_forms`, `add_page_numbers`, `replace_invert_pdf`, `add_image`, `add_comments`) | implemented, unrouted | 5 routes registered |
| Dead duplicates `misc::auto_rotate_pdf`, `misc::repair_pdf`, `parse_page_rotations`, `auto_rotate_apply_work`, `sidecar_available` | superseded by `automation::*` | **deleted** (~250 lines) — they were the source of dead-code warnings |

### Conflicts resolved

- **`Upload.bytes` type migration** (`Vec<u8>` → disk-spooled `UploadBytes`,
  landed mid-pass by another agent): fixed all breakage —
  `misc.rs` compress fallback arm (`to_vec()`), three test helpers
  (`UploadBytes::from_vec`), `AsRef<[u8]>` impl, and wired
  `Config::max_upload_bytes` into `upload.rs` via
  `configure_max_part_bytes` called from `main`.
- **`main.rs` CORS rewrite** (Java `WebMvcConfig` parity, landed mid-pass):
  fixed `.map` on header slices → `.iter().copied().map(...)`.
- **`guard.rs`** (new endpoint-disable/auth-guard middleware, landed
  mid-pass): added missing `IntoResponse` import.

### Compile errors fixed (13 → 0)

| File | Error | Fix |
|---|---|---|
| `misc.rs` | `doc` vs `document` binding | corrected identifier |
| `misc.rs` | `&&Vec` iteration, moved `non_empty` | `for kid in items`, `for &file in &non_empty` |
| `general.rs` | overlay guide `Option` destructure | `*entry` pattern fix |
| `layout.rs` | `scale` fn shadowed by `f64` local | renamed formatter to `scale_cm` |
| `passwords.rs` | `trailer.remove` returns `Option` not `Result` | `let Some` |
| `attachments.rs` | `NaiveDate`/`NaiveDateTime` API + missing `Datelike`/`Timelike` | `and_then` chain + trait imports |
| `automation.rs` | `CONTENT_TOO_LARGE` doesn't exist on `http::StatusCode` | `PAYLOAD_TOO_LARGE` (×3) |
| `imgextract.rs` | zip error-type mismatch in `and_then` | `Ok(...?)` wrap |
| `visual.rs` | `DynamicImage::dimensions` needs trait | `use image::GenericImageView` |

### Clippy errors fixed (19 + 5 → 0)

`map_clone`, `manual_range_contains`, `explicit_counter_loop` (×2),
`needless_borrow` (×2), `manual_clamp` (×2), `byte_char_slices` (×12),
`int_plus_one` + `collapsible_match` (form.rs), `while_let_loop` (form.rs),
`redundant_closure`, `explicit_auto_deref`.

### Warnings fixed

`unused_mut` (×4), deprecated `load_mem_with_password` →
`load_mem_with_options` + `LoadOptions`, unused `page_content` binding,
dead-code fields (`BookletSide::is_back`, `TextItem::x` — kept for Java
parity, annotated), dead duplicate handlers deleted.

## B. Build results

| Check | Result |
|---|---|
| `cargo fmt --check` | clean |
| `cargo check --all-targets` | clean at snapshot (0 errors; transient dead-code warnings only in still-WIP `form.rs`/`convert.rs`) |
| `cargo clippy --all-targets --all-features -- -D warnings` | **green** at the settled snapshot; a later re-run shows 1 remaining `dead_code` warning on `FormUpload::fields` (form agent mid-flight, field reserved for `add-fields`) |
| `cargo build` | success (dev profile) |
| `cargo test` | **55 passed, 0 failed** |

Note: the tree was under **continuous active modification** throughout
this pass — `form.rs` (→96 KB, all 9 form endpoints), `handlers/convert.rs`
+ `src/convert.rs` (engine-runner + `img/pdf` route), `guard.rs`, the
`UploadBytes` spool refactor and the CORS rewrite all landed while
working. Errors those agents introduced were fixed where mechanical
(missing imports, type mismatches, API drift); genuinely unfinished
functions (`apply_data_values`, `xlsx_bytes`, engine specs) were left to
their owners. Results reflect the tree at the time of each run; re-run
`task rust:check` on the settled tree before release.

## C. API migration status (snapshot)

- Java controllers: 146 files; ~150 core `/api/v1/**` endpoint mappings.
- Rust routes registered: **85** in `handlers/mod.rs` (count grew during
  the pass as `form` and `convert` modules were wired).
- Fully native: ~75 endpoints (all general, analysis, filter,
  attachments/portfolio, passwords, sanitize/get-info/remove-cert-sign,
  form endpoints incl. extract-xlsx/fill, automation/mobile-scanner,
  visual image ops, `convert/img/pdf`).
- Partial/delegating: ~10 (modes that forward to Java: text watermark,
  raster stamp, non-CMYK replace-invert, non-raster add-image,
  anchored add-comments, layout widget/OCProperties gaps, non-native
  image formats in convert).
- Still proxied: ~65 — remaining `/convert/**` (~25, sidecar-blocked),
  security signing family (9, crypto-stack-blocked), raster misc
  (ocr/blanks/scanner-effect/extract-image-scans), pipeline, platform
  GETs.
- Blocked by external engines: ~30 endpoints (documented in
  `FINAL_MIGRATION_AUDIT.md` §D).

## D. System verification

Runtime smoke test against `target/debug/stirling-server`
(`JAVA_BACKEND_URL=off`, port 18081):

| Check | Result |
|---|---|
| `GET /health` | `{"status":"ok"}` |
| `POST /api/v1/analysis/page-count` | `{"pageCount":1}` on fixture |
| `POST /api/v1/general/rotate-pdf` | 200, valid `%PDF-1.7` output |
| `POST /api/v1/filter/filter-page-count` | 204 on non-match (correct pass/fail semantics) |
| `POST /api/v1/misc/decompress-pdf?async=true` | job accepted, `jobId` returned |
| `GET /job/{id}` + `GET /job/{id}/result` | status `complete:true`, result = valid PDF bytes |
| Unmigrated route (`/api/v1/convert/pdf/word`) | 404 with proxy disabled — correct fallback-off behaviour |

**Not verified**: live Java↔Rust proxy parity (`scripts/rust_pdf_parity.py`)
requires the Java backend on :8080; starting the full Gradle/Spring stack
was out of scope for this pass and risks colliding with other agents'
build usage. The proxy path itself is exercised by the 404 case above.

## E. Remaining issues

1. **Active-tree churn** — other agents were still landing code during
   verification; final state needs one more `task rust:check`.
2. **`/api/v1/form/*`** — 3 of 9 endpoints routed; `extract-xlsx`,
   `add/edit/modify/delete-fields`, `fill` still on Java (agent may be
   mid-flight).
3. **Compile is verified; runtime parity is not** — golden-file parity
   vs Java remains to be run once both backends are up.
4. **`UploadBytes` materialization** — `Deref` lazily reads the whole
   spool file into memory on first `&[u8]` access; per-document peak is
   unchanged vs before (documented in `upload.rs`), but proxy
   re-streaming to Java avoids buffering where used.
5. **Dead-code hygiene** — `#[allow(dead_code)]` on `BookletSide::is_back`
   and `TextItem::x` is intentional parity preservation, not suppression.

## F. Recommended next phase

1. Re-run `task rust:check` + `cargo test` on the settled tree.
2. Stand up Java :8080 + Rust :8081 and run
   `scripts/rust_pdf_parity.py` across all routed endpoints; triage diffs.
3. Finish `/api/v1/form/*` remaining 6 endpoints (AcroForm CRUD +
   appearance regeneration; `extract-xlsx` likely stays Java).
4. Decide the rasterisation story for `remove-blanks`, `auto-split-pdf`,
   `scanner-effect`, `extract-image-scans`, text watermarks/stamps
   (PDFium is banned — evaluate `hayro` or keep Java).
5. Keep the signing/redact/timestamp family on Java permanently unless a
   crypto sidecar is reintroduced.
6. Port or consciously defer platform GET endpoints (`/api/v1/config/*`,
   `/api/v1/ui-data/*`) — proxy is correct but adds a hop.
