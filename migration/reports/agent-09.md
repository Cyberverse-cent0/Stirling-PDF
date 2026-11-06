# Agent 09 — Filter API migration report

Date: 2026-11-06. Branch: `feature/rust-rewite`.

## Inventory

The complete Java Filter API surface is a single controller:
`app/core/src/main/java/stirling/software/SPDF/controller/api/filters/FilterController.java`
(`@FilterApi` → base path `/api/v1/filter`). No other `@FilterApi`-annotated
controllers exist. All six endpoints are `@AutoJobPostMapping` multipart POSTs
(support `?async=true`), `ResourceWeight.SMALL_WEIGHT`, and answer either
**200 + PDF** or **204 No Content** — they are pass/fail gates, not
transformations.

| Route | Java handler | Request model | Pass response | Fail response |
|---|---|---|---|---|
| `/api/v1/filter/filter-contains-text` | `containsText` | `ContainsTextRequest` (`fileInput`, `text`, `pageNumbers`) | re-saved PDF, original filename | 204 |
| `/api/v1/filter/filter-contains-image` | `containsImage` | `PDFWithPageNums` (`fileInput`, `pageNumbers`) | re-saved PDF, original filename | 204 |
| `/api/v1/filter/filter-page-count` | `pageCount` | `PDFComparisonAndCount` (`pageCount`, `comparator`) | original upload bytes | 204 |
| `/api/v1/filter/filter-page-size` | `pageSize` | `PageSizeRequest` (`standardPageSize`, `comparator`) | original upload bytes | 204 |
| `/api/v1/filter/filter-file-size` | `fileSize` | `FileSizeRequest` (`fileSize`, `comparator`) | original upload bytes | 204 |
| `/api/v1/filter/filter-page-rotation` | `pageRotation` | `PageRotationRequest` (`rotation`, `comparator`) | original upload bytes | 204 |

Out of scope but related: `PipelineProcessor` (`/api/v1/pipeline/*`) calls the
`filter-*` paths internally to decide whether a file continues through a
pipeline. That controller stays on Java (pipeline family is a separate
migration); once these routes are live in Rust, Java-side pipeline HTTP calls
to the filters will reach the Rust handlers through the same ingress, which is
the intended strangler behaviour. Direct in-process pipeline calls bypass HTTP
entirely and are unaffected.

## Dependency mapping (Java → Rust)

| Java dependency | Rust equivalent | Status |
|---|---|---|
| `CustomPDFDocumentFactory.load` | `pdfdoc::load` (lopdf) | reused |
| `PdfUtils.hasText` → `PDFTextStripper` per page | `text::page_items` per selected page (shared positioned extractor: per-font `Encoding`/ToUnicode decode, form-XObject `Do` descent) | ported, divergence noted |
| `PdfUtils.hasImages` → `containsImage` XObject walk | `filter::resources_have_image` over `/Resources` → `/XObject`, recursing `/Subtype /Form`, `HashSet<ObjectId>` visited set | ported |
| `GeneralUtils.requireWellFormedPageSelection` + `parsePageList` | `page_select::require_well_formed_page_selection` (new) + `parse_page_list` | ported |
| `document.getNumberOfPages()` | `pdfdoc::page_ids().len()` | reused |
| `firstPage.getMediaBox()` area | `pdfdoc::media_box` + LETTER fallback | ported |
| `PdfUtils.textToPageSize` (A0–A6, LETTER, LEGAL) | `filter::standard_page_size`, PDFBox `PDRectangle` constants re-derived from pdfbox-3.0.8 sources (`POINTS_PER_MM = 1/(10*2.54f)*72`, LETTER 612×792, LEGAL 612×1008) | ported |
| `firstPage.getRotation()` | `pdfdoc::page_rotation` (inheritable `/Rotate`, %90 normalised) | reused |
| `FilterController.compare` Greater/Equal/Less | `filter::compare` on `Ordering` (`f32::total_cmp` for areas) | ported |
| `WebResponseUtils.multiPartFileToWebResponse` | `merge::file_response` with `Upload.content_type` (part-declared type, `application/pdf` fallback) | reused |
| `WebResponseUtils.pdfDocToWebResponse` | `pdfdoc::save` + `merge::pdf_response` | reused |
| `Filenames.toSimpleFileName` | `filter::simple_name` (basename) | ported |
| `ExceptionUtils.createInvalidArgumentException` / `createInvalidPageSizeException` | `AppError::BadRequest` (Java maps `BaseValidationException` → 400) | mapped |
| `@AutoJobPostMapping` / `?async=true` | `#[stirling_macros::endpoint]` → `jobs::submit` | reused |

## Migrated operations

All six endpoints are implemented in `rust/src/handlers/filter.rs` and routed
in `rust/src/handlers/mod.rs`. Filter ordering/execution semantics preserved:
per-page early-exit scans for contains-*, first-page-only reads for
page-size/page-rotation, no document parse at all for file-size (matching
Java), original-bytes vs re-saved-PDF response split preserved.

## Modified files

- `rust/src/handlers/filter.rs` — new module, all six endpoints + helpers +
  unit tests for `compare`/`standard_page_size`.
- `rust/src/handlers/mod.rs` — `pub mod filter;` + six `.route(...)` entries
  (additive only; other agents' routes untouched).
- `rust/src/page_select.rs` — added `require_well_formed_page_selection`;
  `require_valid_page_selection` and the new function share a private
  `check_page_selection(selection, total, require_match)` implementing Java's
  `checkPageSelection` `requireMatch` flag exactly.
- `rust/src/main.rs` — added `mod fontmetrics;` and `mod text;`. Both modules
  already existed as files and were referenced by `misc.rs` /
  `handlers/automation.rs` but never declared, so the crate could not compile
  without them; `filter.rs` uses `text::page_items` for contains-text.

## Blockers / known divergences

- **Text extraction**: `text::page_items` decodes through font `Encoding`s
  (WinAnsi/MacRoman/Standard/Differences/ToUnicode CMaps) and descends into
  form XObjects like PDFBox, but glyph advances inside `TJ` arrays and
  Type3/CID metrics are approximated (per text.rs's own notes). Page text is
  rebuilt by joining runs with a single space; PDFBox inserts whitespace
  only where glyph positions warrant it, so multi-word phrases spanning runs
  can disagree in both directions.
- **Missing `text` param**: Java binds `null` → NPE → 500; Rust returns 400.
- **Missing/invalid `comparator`**: Java NPEs on `null` (500) or throws
  `StepConfigurationException` (400) on bad values; Rust returns 400 for both.
- **`page-size` MediaBox fallback**: PDFBox falls back to US Letter when the
  MediaBox is absent/not an array; `pdfdoc::media_box` errors, so the
  fallback is applied on any resolution failure (slightly broader than
  Java's, which only falls back for the absent/non-array case).
- **Area precision**: Java computes `float` areas from `PDRectangle` floats;
  Rust casts the f64 box to f32 and multiplies in f32 — same precision class.
  `Equal` uses `f32::total_cmp`, matching `Float.compareTo` incl. -0.0/NaN.
- **204 + async jobs**: the miss response carries `Content-Type:
  application/json` with an empty body so `jobs::submit` records a null
  inline result (job completes, no file) rather than a zero-byte "result.pdf"
  artefact. Sync clients see a plain 204 either way.
- **Pass-through content type**: echoes the multipart part's declared
  `Content-Type` via `Upload.content_type`; falls back to `application/pdf`
  when absent or not a valid header value (Java would 500 on an unparseable
  media type — edge case only reachable via `filter-file-size` on non-PDFs).
- **Error message text** for bad numbers/comparator/page-size differs from
  Java's i18n'd messages; status codes match (400).

## Route requirements

Routes are registered in `rust/src/handlers/mod.rs`:

```
POST /api/v1/filter/filter-contains-text
POST /api/v1/filter/filter-contains-image
POST /api/v1/filter/filter-page-count
POST /api/v1/filter/filter-page-size
POST /api/v1/filter/filter-file-size
POST /api/v1/filter/filter-page-rotation
```

All support `?async=true` through the shared job framework
(`/api/v1/general/job/{id}` etc. already routed). No new dependencies; no
shared build artifacts touched; no other agents' modules modified
(`page_select.rs` and `handlers/mod.rs` edits are additive).

## Re-verification against updated codebase (2026-11-06, second pass)

Concurrent agents landed a temp-file upload spool (`UploadBytes`), new handler
modules (`automation`, `form`, `layout`, `passwords`, `visual`), `guard.rs`,
`imgextract.rs`, `text.rs`, `fontmetrics.rs`, and declared `mod fontmetrics;` /
`mod text;` / `mod guard;` in `main.rs`. Checklist re-run:

- `Upload { name, bytes: UploadBytes, content_type }` — `pdfdoc::load` calls
  unchanged (deref coercion `&UploadBytes → &[u8]`); pass-through switched to
  `upload.bytes.to_vec()`; `filter-file-size` now uses `byte_len()` so the
  comparison never materializes the spooled upload (Java's `getSize()` parity).
- `#[endpoint]` macro, `jobs::submit`/`async_requested`, `AppError` variants,
  `merge::{pdf_response, file_response}`, `page_select`, `text::page_items`,
  and all `pdfdoc` helpers used (`load`, `save`, `page_ids`, `page_resources`,
  `resolve_owned`, `dict_of`, `peek_dict`, `media_box`, `page_rotation`) —
  signatures unchanged.
- All six routes still registered in `handlers/mod.rs` (lines ~210–222)
  alongside the new agents' routes; `pub mod filter;` intact.
- `rustfmt --check` clean on `filter.rs`, `page_select.rs`, `main.rs`
  (`skip_children=true` for crate-root files).

## Formatting status

`rustfmt --edition 2021` run on `rust/src/handlers/filter.rs` and
`rust/src/page_select.rs`; `rustfmt --check` (with `skip_children=true` for
crate-root files) passes on `filter.rs`, `page_select.rs`, and `main.rs`.
`handlers/mod.rs` additions conform to rustfmt (pre-existing unformatted code
in `misc.rs` belongs to another agent and was not touched). No
builds/tests/clippy run per task restrictions — compile verification is
pending a shared `cargo check` once concurrent agent work lands.

## Stabilization addendum (2026-11-06, builds now authorized)

### Verification actually run

- `cargo check`: **passed** at the time this addendum's code was verified
  (later concurrent edits to `automation.rs`/`platform.rs` broke the tree
  again — unrelated to this module).
- `cargo test filter::` — **9/9 pass**: the pre-existing comparator/
  page-size unit tests plus 7 new `endpoint_tests` driving the real
  `*_work` functions through `testkit::Pdf` fixtures:
  - `page_count` — Equal pass returns the untouched upload bytes; miss → 204 empty.
  - `file_size` — non-PDF bytes pass on length alone (Java parity: no parse).
  - `contains_text` — positioned-text hit/miss; missing `text` → 400.
  - `contains_image` — page XObject hit; blank page → 204.
  - `page_size` — LETTER Equal passes on the 612×792 fixture, A4 misses,
    unknown size name → 400.
  - `page_rotation` — `/Rotate 90` Equal passes; unrotated → 204.
- `rustfmt --check` clean on `filter.rs`.

### Java–Rust classification (Filter API)

| Endpoint | Class | Notes |
|---|---|---|
| filter-contains-text | PARTIAL | Positioned-text extraction ported (`text::page_items`); run-join whitespace can differ from PDFBox `PDFTextStripper` on multi-run phrases — parity harness check owed |
| filter-contains-image | COMPLETE* | Recursive XObject walk incl. Form XObjects w/ visited set; *runtime parity unverified |
| filter-page-count | COMPLETE* | Comparator + original-bytes pass-through verified by unit test |
| filter-page-size | COMPLETE* | PDFBox `PDRectangle` constants re-derived; LETTER fallback for missing MediaBox |
| filter-file-size | COMPLETE* | `byte_len()` on the spooled upload; non-PDF inputs pass like Java |
| filter-page-rotation | COMPLETE* | First page only, inherited `/Rotate`, %90 normalization |

`*` = unit-tested against fixtures; Java-runtime parity still owed via
`testing/parity.sh` once the tree compiles again.

### Files modified this phase

- `rust/src/handlers/filter.rs` — added `endpoint_tests` module (7 tests).
- No other files touched; concurrent agents own `automation.rs`,
  `platform.rs`, `settings.rs`.
