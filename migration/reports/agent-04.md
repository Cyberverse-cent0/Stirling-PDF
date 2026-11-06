# Agent 04 — Misc API: Page Manipulation and Visual Additions

## Controller inventory

| Java controller | Route | Decision |
|---|---|---|
| `PageNumbersController` | `POST /api/v1/misc/add-page-numbers` | Native (pre-existing `misc::add_page_numbers`, routed this session) |
| `OverlayImageController` | `POST /api/v1/misc/add-image` | Native for PNG/JPEG (`misc::add_image`, pre-existing); SVG and other formats forwarded to Java |
| `StampController` | `POST /api/v1/misc/add-stamp` | `stampType=image` native (`visual::add_stamp`); `text`/`qr` forwarded to Java |
| `WatermarkController` | `POST /api/v1/security/add-watermark` | `watermarkType=image` without `convertPDFToImage` native (`visual::add_watermark`); text mode and rasterising conversion forwarded to Java |
| `BlankPageController` | `POST /api/v1/misc/remove-blanks` | **Not migrated.** Despite the name this endpoint classifies/removes *detected* blank pages: it renders each page with PDFBox `PDFRenderer` at 150 DPI and tests text/image/pixel whiteness. `lopdf` has no renderer, so a faithful port is impossible; it stays on the Java proxy. There is no separate "insert blank page" endpoint in this controller — insertion is covered by page-manipulation endpoints (`general/add-blank-page`-style flows already handled elsewhere). |
| `ScannerEffectController` | `POST /api/v1/misc/scanner-effect` | **Not migrated.** Renders every page to a bitmap, applies noise/blur/contrast/rotation transforms, rebuilds the document from raster pages — fundamentally renderer-dependent. Stays on the Java proxy; no placeholder was added. |

## Implementation details

### `rust/src/handlers/visual.rs` (new)

- `add_stamp` — multipart handler for `/api/v1/misc/add-stamp`. Resolves
  `stampType` exactly like `StampController.resolveStampType` (blank → image
  when `stampImage` is non-empty, else text). Only the `image` branch runs
  natively; everything else is re-serialised and forwarded via
  `proxy::forward_parts` so Java keeps its validation and error shapes.
  - Validates `..`/leading-`/` filenames (400, matching
    `IllegalArgumentException`), requires a non-empty `stampImage`, and
    forwards non-PNG/JPEG payloads (ImageIO's wider codec coverage stays
    on Java).
  - `stamp_image_work` mirrors `addImageStamp`/`enterStampBox`:
    `position` 3×3 grid (`position % 3`, `(position-1)/3` with Java's
    truncating division), `overrideX`/`overrideY` (both ≥ 0 replace
    position), margin factors small/medium/large/x-large =
    0.02/0.035/0.05/0.075 × (mediaW+mediaH)/2, unrotated-size clamping to
    the media box, desired height = `fontSize`, width = height × aspect.
  - `pageNumbers` goes through the shared `page_select` validator/parser
    (blank → `all`, matching `PDFWithPageNums`).
  - Emits the same operator sequence PDFBox writes: `/gsN gs`, then
    `q cm(translate) cm(rotate) q cm(scale) /ImN Do Q Q`.
- `add_watermark` — same shape for `/api/v1/security/add-watermark`.
  Forwards when `convertPDFToImage` is true, `watermarkType` isn't `image`,
  or the image isn't PNG/JPEG. Enforces `@DecimalMin(1.0)` on `fontSize`
  and `@Min(0)` on spacers; Java's `SecurityException` path maps to a 500.
- `watermark_image_work` mirrors `addImageWatermark`: tile height =
  `fontSize`, width = height × aspect, rows/cols =
  `min((int)((page+(spacer))/(tile+spacer)), 10000)`, exclusive `0..n`
  loops, per-tile `q T(cx,cy) R T(-w/2,-h/2) q S /Im Do Q Q`, all pages.
- Shared helpers:
  - `embed_image`/`image_stream` — flate-compressed Image XObjects;
    DeviceRGB + `/SMask` for alpha, DeviceGray for `L8`/`L16` sources,
    mirroring `LosslessFactory.createFromImage`.
  - `register_page_resource`/`resources_dict`/`next_resource_name` —
    replicates `PDResources.add`: mutates the *effective* (inheritable)
    `/Resources` dictionary in place, including indirect section dicts,
    and names entries with PDFBox `createKey` semantics (counter seeded by
    dict size, first free `prefix<n>` wins). `ExtGState` (`/ca` opacity)
    and `XObject` (`/Im<n>`) entries are registered per page.
  - `pdf_num` formats operands like PDFBox's `DecimalFormat` (≤5 fraction
    digits, integral values without a fraction).

### `rust/src/handlers/misc.rs`

- `append_page_content` visibility raised to `pub(crate)` so `visual.rs`
  can reuse it. (`overlay-image`'s SMask handling was re-checked: the mask
  is attached to the image stream dict at embed time — already correct.)

### `rust/src/handlers/mod.rs`

No edits by this agent — concurrent work had already registered all four
routes: `add-page-numbers` (misc), `add-image` (misc), `add-stamp`
(visual), `add-watermark` (visual).

## Modified files

- `rust/src/handlers/visual.rs` — new (image stamp + image watermark).
- `rust/src/handlers/misc.rs` — one-line visibility change.

## Limitations / deliberate divergences

- Text and QR stamps stay on Java: they embed NotoSans TTFs as CID fonts
  and do `@placeholder` expansion + QR encoding — outside lopdf's reach.
- Text watermarks and `convertPDFToImage=true` stay on Java (font
  embedding / full-page rasterisation).
- SVG overlays and non-PNG/JPEG stamp/watermark images stay on Java.
- `scanner-effect` and `remove-blanks` stay on Java (need a PDF renderer).
- Content is appended into the page's existing stream via
  `append_page_content` rather than PDFBox's separate APPEND stream with
  `q`/`Q` wrapping — visually identical, different byte layout.
- PDFBox embeds a fresh image XObject per page; the native path embeds
  once and references it from each page's resources (same rendered
  result).
- `float` params parse as f64; extreme values beyond f32 range diverge
  slightly from Java's `Float` binding.

## Dependencies

Existing only: `lopdf`, `image` (png/jpeg features), `axum`, plus the
shared `pdfdoc`, `page_select`, `upload`, `proxy`, `jobs` helpers. No new
crates.

## Route registration requirements

All satisfied in `rust/src/handlers/mod.rs`:

- `POST /api/v1/misc/add-page-numbers` → `misc::add_page_numbers`
- `POST /api/v1/misc/add-image` → `misc::add_image`
- `POST /api/v1/misc/add-stamp` → `visual::add_stamp`
- `POST /api/v1/security/add-watermark` → `visual::add_watermark`

Unrouted endpoints intentionally fall through to `.fallback(proxy::proxy)`.

## Formatting status

`rustfmt --edition 2021` run on `rust/src/handlers/visual.rs` (the only
file authored by this agent). `misc.rs` received a one-token visibility
edit already in canonical style. No builds, tests, or parity runs per the
task restrictions.

---

## Continuation audit (second pass)

Re-verified every contract against `git show HEAD:` sources (the Java
controllers are deleted in the working tree; HEAD is the contract of
record):

- `PageNumbersController` (`/misc/add-page-numbers`): line-by-line match
  confirmed — margin factors, 3×3 position grid with truncating
  `(pos-1)/3`, MediaBox anchors, ascent/descent vertical alignment,
  `{n}`/`{total}`/`{filename}` replacement order, `pageNumber++` per
  selected page (= Rust `starting_number..` zip), `zeroPad` via
  `String.format("%%0Nd")`, `Color.decode` fallback to black, output name
  `_page_numbers_added.pdf`. `PDFWithPageNums` contributes only
  `pageNumbers`, already handled by `page_select`.
- `OverlayImageController` = `/misc/add-image` (not `/overlay-image`) —
  native `misc::add_image` covers it.
- `BlankPageController` = `/misc/remove-blanks` only; there is **no**
  insert-blank-page endpoint anywhere in the codebase (checked
  `git ls-tree` + frontend constants). The unrouted `misc::remove_blanks`
  estimate stays unrouted: it can keep pages Java's 150-DPI rasteriser
  would drop, so it is not faithful and correctly remains on the proxy.
- `ScannerEffectController` dep chain re-confirmed: PDFRenderer →
  bitmap transforms → rebuilt raster document. No lopdf path; stays Java.
- `StampController`/`WatermarkController` reviewed again: text/QR paths
  require NotoSans TTF→CIDFont embedding plus QR generation — confirmed
  outside lopdf; proxied by type sniffing before dispatch.

`visual.rs` re-reviewed for defects: resource registration mutates the
effective (inheritable) `/Resources` in place like `PDResources.add`;
`ExtGState`/`ca` opacity, `Im<n>` naming via `createKey` semantics,
SMask attachment at embed time, i64 grid math avoiding `i32::MIN`
overflow, and Java's truncating division are all correct by inspection.
No changes were needed this pass.

### Status after audit

| Endpoint | Status |
|---|---|
| `add-page-numbers` | Native, contract-verified by source inspection |
| `add-image` (overlay) | Native PNG/JPEG, others proxied |
| `add-stamp` | Native image type only; text/QR proxied |
| `add-watermark` | Native image type only; text/`convertPDFToImage` proxied |
| `remove-blanks` | Java fallback (renderer-dependent classification) |
| `scanner-effect` | Java fallback (full rasterisation pipeline) |

No files modified this pass; no build/test/parity runs per restrictions.

---

## Stabilization pass (global directive)

Scope: repo-wide test suite was red (6 failures); triaged and fixed the
ones in scope, verified the rest were already resolved by concurrent
edits. No new migration work.

### Defects found and fixed

- `handlers/misc.rs` test `unlock_forms_removes_lock_and_readonly_flag`:
  the fixture put `/Lock` on the AcroForm dictionary, but Java's
  `UnlockPDFFormsController` only strips `/Lock` from **field**
  dictionaries (`acroForm.getFieldTree()`). Moved `/Lock` into the field
  dict in the fixture and assert removal there. The Rust implementation
  was already Java-correct; the test was wrong.
- `handlers/merge.rs` test `merge_combines_pages_in_upload_order`:
  `testkit::Pdf::new()` already creates one page; the test then called
  `add_page` on both fixtures, producing 2+3=5 pages instead of the
  expected 1+2=3. Removed the extra `add_page` calls. Implementation was
  correct — `pdfdoc::import_pages` produced exactly the requested pages.
- `auth.rs` test `phone_scanner_paths_match_java_regex`: asserted
  `SESSION_1` (underscore) is a valid phone-scanner session id, but
  Java's `MOBILE_SCANNER_PHONE_PATTERN` is `[A-Za-z0-9-]+` — underscores
  are rejected. Corrected the test to use a hyphenated id and added an
  explicit underscore-rejection assertion.

### Tests already green (concurrent fixes, verified by rerun)

- `handlers/jobs.rs::submit_stores_file_results_for_download`
- `handlers/platform.rs::java_split_drops_only_trailing_empties`
- `metrics.rs::normalize_path_collapses_dynamic_segments`

### Verification actually run

- `cargo test` — **167 passed, 0 failed**
- `cargo fmt --check` — clean (rustfmt applied to touched files)
- `cargo check` — clean
- `cargo clippy --all-targets` — clean, no warnings
- Module/route audit: every file in `src/handlers/` has a `pub mod`
  declaration in `mod.rs`; every top-level `src/*.rs` is declared in
  `main.rs`; `forms.rs`, `convert.rs` (engine runner) and all handler
  modules are reachable — no orphans found.

### Remaining Java dependencies (unchanged)

`remove-blanks`, `scanner-effect`, text/QR stamps, text watermarks,
`convertPDFToImage`, and all engine-dependent conversion endpoints
remain proxied by design.
