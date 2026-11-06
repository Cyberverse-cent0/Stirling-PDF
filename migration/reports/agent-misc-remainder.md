# Misc API remainder — agent report

Scope: the `MiscApi` controllers not already migrated and not claimed by
other agents — page numbers, overlay image, comments, form unlock,
colour conversion, portfolios, blank-page classification, plus the
remainder that stays on Java (OCR, auto-split, scanner-effect,
print-file, config, login-disclaimer). Sibling agents' routes
(`add-stamp`, `add-watermark`, `auto-rename`, `auto-rotate-pdf`,
`repair`, mobile-scanner) were left untouched.

## Controllers inspected

| Java controller | Route | Status |
|---|---|---|
| `PageNumbersController` (+ `AddPageNumbersRequest`) | `POST /api/v1/misc/add-page-numbers` | Implemented natively (`misc::add_page_numbers`) |
| `OverlayImageController` (+ `OverlayImageRequest`) | `POST /api/v1/misc/add-image` | Partial: PNG/JPEG native; SVG/other formats forwarded to Java |
| `AddCommentsController` (+ `AddCommentsRequest`) | `POST /api/v1/misc/add-comments` | Partial: coordinate specs native; `anchorText` specs forwarded to Java |
| `UnlockPDFFormsController` | `POST /api/v1/misc/unlock-pdf-forms` | Implemented natively (`misc::unlock_pdf_forms`) |
| `ReplaceAndInvertColorController` (+ `ReplaceAndInvertColorRequest`) | `POST /api/v1/misc/replace-invert-pdf` | Partial: `COLOR_SPACE_CONVERSION` native via Ghostscript; other modes forwarded |
| `PortfolioController` | `POST /api/v1/misc/create-portfolio`, `/flatten-portfolio` | Implemented natively |
| `BlankPageController` (+ `RemoveBlankPagesRequest`) | `POST /api/v1/misc/remove-blanks` | Implemented natively **but not routed** — approximation of a renderer-bound check; see Divergences |
| `StampController`/watermark | `POST /api/v1/misc/add-stamp`, `/api/v1/security/add-watermark` | Agent-03/04 (`visual.rs`) — routes registered, untouched |
| `AutoRenameController` | `POST /api/v1/misc/auto-rename` | Agent-05 (`automation.rs`) — route registered, untouched |
| `AutoRotateController` | `POST /api/v1/misc/auto-rotate-pdf` | Agent-05 — `text`/`auto` native, `osd` forwarded |
| `RepairController` | `POST /api/v1/misc/repair` | Agent-05 (`automation.rs`) — route registered, untouched |
| `MobileScannerController` | `/api/v1/mobile-scanner/*` | Agent-05 (`automation.rs`) — session/upload/download/delete routes registered |
| `OCRController` | `POST /api/v1/misc/ocr-pdf` | Left on Java — Tesseract sidecar |
| `AutoSplitPdfController` | `POST /api/v1/misc/auto-split-pdf` | Left on Java — page rendering + QR decode |
| `ScannerEffectController` | `POST /api/v1/misc/scanner-effect` | Left on Java — full-page rasterisation + image effects |
| `PrintFileController` | `POST /api/v1/misc/print-file` | Left on Java — OS print pipeline |
| `ConfigController`, `LoginDisclaimerController` | `/api/v1/config/*`, `/api/v1/misc/login-disclaimer` | Left on Java — server config/disclaimer state, not PDF work |
| `ExtractImageScansController` | `POST /api/v1/misc/extract-image-scans` | Excluded per assignment (GEN 03); stays on Java — PDFBox rendering |

## Implementation notes

### add-page-numbers

- Java params reproduced: `fontType` (Helvetica/Times-Roman/Courier),
  `position` 1–9, `customText` with `{n}`/`{total}`/`{filename}`,
  `pagesToNumber` (`all` or list), `startingNumber`, `fontSize`,
  `fontColor` (#rgb/#rrggbb/#cmyk), `customMargin` (small/medium/large),
  `zeroPad`.
- New `rust/src/fontmetrics.rs`: WinAnsi-indexed advance tables generated
  from the same Adobe AFM files PDFBox loads for `PDType1Font`, plus
  ascent/descent descriptor values.
- Placement mirrors PDFBox standard-14 behaviour: 3×3 grid anchors,
  margin factors, text width via `string_width`, baseline from
  ascent/descent. Content is appended as a new stream per page; page
  `/Resources` `/Font` entries are copied to page level before mutation
  so inherited/shared dictionaries are never touched.

### add-image (overlay)

- Hand-written handler (not `#[endpoint]`) because the second file field
  `imageFile` must be inspected before dispatch; the raster check reads
  only the first bound part, matching Spring's first-part semantics.
- PNG/JPEG decode via the `image` crate; alpha becomes an `/SMask` gray
  image XObject (no `/Matte`, matching PDFBox's soft-mask default).
- Placement: PDF user-space `(x, y)`, `q 1 0 0 1 x y cm /ImN Do Q`,
  first page or every page per `everyPage`.
- `/Resources` `/XObject` handled like `/Font`: resolved dict inlined at
  page level before merging; `Im<n>` names allocated around existing
  entries.
- SVG and every other codec `image` doesn't cover are rebuilt verbatim
  and forwarded via `proxy::forward_parts` (two file fields).

### add-comments

- Parses the JSON comment-spec array; each spec yields a `/Text`
  (sticky-note) annotation with `/Rect`, `/Contents`, `/T`, `/Subj`,
  `/C`, `/CA`, `/Name /Comment`, `/M`, `/CreationDate`.
- UTF-16BE text with BOM via the shared `attachments::text_string_object`.
- Specs carrying `anchorText` need positioned text extraction and are
  forwarded to Java together with the whole request (no partial mixing).

### unlock-pdf-forms

- Finds `/AcroForm`, walks `/Fields` and nested `/Kids` (cycle-guarded,
  depth-capped), removes `/Lock`, clears flag bit 1 (`/Ff` read-only) and
  sets `/NeedAppearances true`.
- XFA: resolves `/XFA` (single stream and `[name, stream, …]` array
  forms), rewrites each stream's bytes replacing `access="readOnly"`
  (any ASCII-whitespace variant around `=`) with `access="open"`, then
  re-stores the stream.

### replace-invert-pdf

- Only `COLOR_SPACE_CONVERSION` runs natively: Ghostscript
  `pdfwrite` + `-sProcessColorModel=DeviceCMYK` /
  `-sColorConversionStrategy=CMYK`, output `<base>_inverted.pdf` like
  Java's naming.
- `HIGH_CONTRAST_COLOR`, `CUSTOM_COLOR`, `FULL_INVERSION` all need
  rendered pixels → forwarded to Java via `proxy::forward_multipart`.

### portfolios (create / flatten)

- `create-portfolio`: fixed-position standard-14 cover text plus an
  embedded-file name tree, matching `PortfolioController`'s output
  shape.
- `flatten-portfolio`: resolves the portfolio dictionary structure and
  merges embedded files back as page content/attachments per Java's
  semantics.

### remove-blanks (implemented, **unrouted**)

- `BlankPageController` renders each page at 150 DPI and applies
  `isBlankImage` (blue-channel ≥ `255−threshold` counts as white;
  blank when white% ≥ `whitePercent`). No renderer exists in the
  lopdf-only architecture, so this is an estimate:
  - Text: content streams (page + nested Form XObjects, depth-capped)
    are decoded with `Stream::decode_content`; `Tj`/`TJ`/`'`/`"` string
    operands and `/ActualText` property lists with non-whitespace bytes
    mark the page non-blank.
  - Image presence: `/Image` XObjects reachable through `/Resources`
    (the same resource walk as `PdfUtils.hasImagesOnPage`), independent
    of whether content actually draws them.
  - White ratio: drawn images are decoded via `imgcomp::decode_image`,
    weighted by painted area (`|det(CTM)|` tracked through `q`/`Q`/`cm`
    and form `/Matrix`, divided by media-box area). Any vector painting
    operator or undecodable image marks the page non-blank.
- Divergence vs Java (documented on the function): the estimate errs
  toward **keeping** pages — a mostly-white page with a stray painted
  path, or content Java's renderer would show that the estimate can't
  see (annotations, shading detail, SMask edges), stays. Vector-only
  pages are still classified blank, matching Java's image-gated logic.
  Residual risk: a page Java keeps via text whose glyphs my byte-level
  whitespace test misses would be the only false-blank path; standard
  text ops are all covered.
- Output contract preserved: ZIP with `<base>_nonBlankPages.pdf` (or
  `_allBlankPages.pdf` when nothing survives) plus `_blankPages.pdf`
  when both sets are non-empty, named `<base>_processed.zip`, via the
  shared `split::zip_response`.

## Java files inspected

- `controller/api/misc/`: `BlankPageController`, `AutoSplitPdfController`,
  `ScannerEffectController`, `OCRController`, `PrintFileController`,
  `ConfigController`, `LoginDisclaimerController`,
  `ExtractImageScansController` (excluded)
- `controller/api/` (misc-tagged, other packages): `PageNumbersController`,
  `OverlayImageController`, `AddCommentsController`,
  `UnlockPDFFormsController`, `ReplaceAndInvertColorController`,
  `PortfolioController`
- Models: `AddPageNumbersRequest`, `RemoveBlankPagesRequest`,
  `ReplaceAndInvertColorRequest`, `OverlayImageRequest`,
  `AddCommentsRequest`, `PDFFile`
- `app/common`: `PdfUtils.hasImagesOnPage`, `GeneralUtils`,
  `Filenames.toSimpleFileName`, `WebResponseUtils`

## Rust files created or modified

- `rust/src/fontmetrics.rs` — **new**: standard-14 WinAnsi width tables +
  ascent/descent (AFM-derived, PDFBox-compatible).
- `rust/src/handlers/misc.rs` — added `unlock_pdf_forms`, `add_page_numbers`
  (+ helpers `ensure_page_font`, `upsert_font_entry`, `append_page_content`,
  `append_to_stream`, `push_content_stream`, `hex_bytes`, `parse_pdf_color`,
  `pdf_date_now`), `replace_invert_pdf` (+ `replace_invert_cmyk_work`),
  `add_image` (+ `add_image_work`, `embed_rgb_image`, `embed_gray_image`,
  `register_page_xobject`, `upsert_xobject`), `add_comments`
  (+ `add_comments_work`, `append_annotation`), and `remove_blanks`
  (+ `PageScan`, `page_is_blank`, `resources_contain_image`,
  `scan_page_contents`, `content_streams`, `scan_content_stream`,
  `scan_form_xobject`, `record_image`, `xobject_named`, `stream_of`,
  `dict_name`, `pdf_num`, `operands_have_text`, `operands_have_actual_text`,
  `object_has_visible_text`, `object_has_visible_text_bytes`,
  `subset_pdf_bytes`, `page_area`).
- `rust/src/main.rs` — added `mod fontmetrics;` (one line; the rest of
  the diff belongs to other agents).

## Required route registrations

Already present in `rust/src/handlers/mod.rs` (verified):

```rust
.route("/api/v1/misc/unlock-pdf-forms", post(misc::unlock_pdf_forms))
.route("/api/v1/misc/add-page-numbers", post(misc::add_page_numbers))
.route("/api/v1/misc/replace-invert-pdf", post(misc::replace_invert_pdf))
.route("/api/v1/misc/add-image", post(misc::add_image))
.route("/api/v1/misc/add-comments", post(misc::add_comments))
```

Sibling agents' routes also present: `add-stamp`, `add-watermark`
(`visual.rs`), `auto-rename`, `auto-rotate-pdf`, `repair`, and the
`/api/v1/mobile-scanner/*` set (`automation.rs`).

**Not registered** — `remove_blanks` exists but has no route, so
`/api/v1/misc/remove-blanks` falls through to the Java proxy. To enable
the native path, add:

```rust
.route("/api/v1/misc/remove-blanks", post(misc::remove_blanks))
```

Left unregistered deliberately: the endpoint rasterises at 150 DPI in
Java and the native version is an approximation whose divergence (it can
keep pages Java would drop) should be an explicit integrator decision,
not a silent route addition.

## Dependencies and architectural blockers

- **No PDF renderer**: blocks faithful `remove-blanks`, all of
  `scanner-effect`, `extract-image-scans`, `ocr-pdf`, rasterising
  `flatten`, `FULL_INVERSION`/`CUSTOM_COLOR`/`HIGH_CONTRAST_COLOR`.
- **Tesseract**: `ocr-pdf`, auto-rotate `osd` mode (agent-05 forwards).
- **QR decoding**: `auto-split-pdf` divider detection.
- **Ghostscript**: required at runtime for `COLOR_SPACE_CONVERSION`;
  handler returns an error when `gs` is absent (same failure surface as
  Java's sidecar path).
- **ImageMagick**: `compress-pdf` `lineArt` mode stays forwarded.
- **OS print subsystem**: `print-file`.
- **Server config state**: `config/*`, `login-disclaimer` are not PDF
  operations.

## Formatting results

`rustfmt --edition 2021 --check src/handlers/misc.rs src/fontmetrics.rs`
→ clean. No `cargo build`/`check`/`test` was run per the migration-phase
restrictions; compilation is unverified.

## Remaining integration work

- Decide whether to route `remove-blanks` natively (see above).
- `add-comments` `anchorText` path and `add-image` non-raster formats
  still depend on the Java proxy being reachable.
- `fontmetrics.rs` tables cover only the three fonts Java's
  `fontType` enum exposes; any new font choice must extend the tables.

---

## Stabilization addendum (2026-11-06)

### Tests added and executed

`rust/src/handlers/misc.rs` gained a `#[cfg(test)] mod tests` with 12
tests driving the real `*_work` functions through `testkit::Pdf`
fixtures: metadata set/delete-all, page numbers on every page, comment
annotation append + out-of-range skip, AcroForm `/Lock`+`/Ff` unlock,
remove-blanks ZIP split (mixed and all-blank), portfolio create/flatten
round-trip + non-portfolio rejection, and PNG image-XObject embedding.

**Executed**: `cargo test --bin stirling-server handlers::misc` →
**30 passed, 0 failed** (12 new + 18 `util_tests` added by a concurrent
agent for the same helpers). Full suite at time of writing: 259–261
passed; the only failures are 2 in `handlers::form::tests`
(`fill_text_sets_v_and_need_appearances`, `no_acroform_is_a_noop`),
which belong to the in-progress form module, not this one.

### Defects found and fixed

1. **Stale output filenames (6 endpoints).** Java commit `1a146993a8`
   ("Keep the input's filename across tools…") changed ~50 endpoints to
   return `<input-base>.pdf` via `generateFilename(name, ".pdf")`. The
   Rust ports still emitted tool suffixes. Fixed to
   `output_name(Some(simple_name(..)), ".pdf")`:
   `unlock-pdf-forms`, `add-page-numbers`, `add-image`, `add-comments`,
   `replace-invert-pdf` (CMYK path), `flatten-portfolio`.
   `create-portfolio` already returned `portfolio.pdf` — matches.
2. **`remove-blanks` ZIP contract.** Java names entries `<base>.pdf`
   (kept pages), `<base>_blankPages.pdf`, `<base>_allBlankPages.pdf`,
   archive `<base>.zip` — not the `_nonBlankPages`/`_processed.zip`
   names first written. Corrected (still unrouted).
3. **`winansi_safe` surrogate-pair bug.** It iterated `encode_utf16`
   units, emitting two `?` for one non-BMP char. Rewritten over
   `chars()` with a Windows-1252 table for the 0x80–0x9F punctuation
   range (WinAnsiEncoding = CP1252; C1 controls stay `?`).
4. **`merge_same_page_chapters` (split.rs, owner agent landed the same
   fix concurrently).** Java `mergeBookmarksThatCorrespondToSamePage`
   *replaces* the next real chapter's title with the folded
   zero-length titles verbatim (trailing space survives, real title
   discarded, 255-char cap) — verified against
   `SplitPdfByChaptersController` source. The port now matches; its
   test asserts `"Intro "`.
5. **Unparseable numeric params (split.rs).** Briefly changed
   `division_param`/`poster_factor` to fall back to defaults, then
   reverted: the split agent's tests assert Spring int-binding failure
   is a 400, which the restored error paths satisfy.

### Cross-agent fixes needed to unblock the shared test build

- `handlers/convert.rs`: `cbz` entry-name collection borrowed `archive`
  through an `FnMut` closure (E0599-class escape) — restructured to
  extract names inside one `filter_map`.
- `handlers/layout.rs`: renamed a duplicate second `mod tests` to
  `mod unit_tests`.

### Java–Rust parity notes (verified against git history — the Java
sources for most misc controllers are deleted on this branch; compared
at commit `1a146993a8`)

- `add-page-numbers`: `Color.decode` is `#AARRGGBB` — alpha first; the
  port takes the last 6 hex digits. `fontType`→standard-14 mapping,
  margin factors, `{n}/{total}/{filename}`, zero-pad all match.
- `unlock-pdf-forms`: Java walks `acroForm.getFieldTree()` only —
  `/Lock` on the AcroForm dict itself is *not* removed by either side;
  `/Ff` bit 0 cleared; `NeedAppearances` set; XFA `access="readOnly"`→
  `"open"` in stream and array forms. Output name now `<base>.pdf`.
- `remove-blanks`: still an approximation (no 150-DPI renderer); errs
  toward keeping pages. Unrouted pending integrator decision.
- `blank-page` classification detail: Java gates on *images present in
  resources* then measures rendered whiteness; the port decodes drawn
  image XObjects weighted by CTM coverage.

### Unverified behavior

- No runtime/parity harness run; Ghostscript-dependent
  `COLOR_SPACE_CONVERSION` path untested end-to-end (`gs` invocation is
  code-reviewed only).
- `add-image` alpha→`/SMask` path tested for presence, not visual
  correctness.
- Async (`?async=true`) job wrapping for these endpoints relies on the
  shared `dispatch`/`#[endpoint]` machinery tested elsewhere.
