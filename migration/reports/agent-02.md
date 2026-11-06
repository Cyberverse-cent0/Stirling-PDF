# Agent 02 — General API: Sections, Overlays and Document Structure

## Status

All in-scope endpoints are served by the `lopdf` backend.
`split-pdf-by-sections` and `split-pdf-by-chapters` were already migrated before
this run (verified against `SplitPdfBySectionsController` /
`SplitPdfByChaptersController`); no changes were needed there.

## Migrated controllers

| Endpoint | Java controller | Rust handler | Status |
|---|---|---|---|
| `POST /api/v1/general/split-pdf-by-sections` | `SplitPdfBySectionsController` | `split::split_by_sections` (pre-existing) | Already migrated |
| `POST /api/v1/general/split-pdf-by-chapters` | `SplitPdfByChaptersController` | `split::split_by_chapters` (pre-existing) | Already migrated — verified: full outline walk, chapter ranges, ZIP output |
| `POST /api/v1/general/overlay-pdfs` | `PdfOverlayController` | `general::overlay_pdfs` | Migrated |
| `POST /api/v1/general/edit-table-of-contents` | `EditTableOfContentsController` | `general::edit_table_of_contents` | Migrated |
| `POST /api/v1/general/extract-bookmarks` | `EditTableOfContentsController` | `general::extract_bookmarks` | Migrated |
| `POST /api/v1/general/split-for-poster-print` | `PosterPdfController` | `split::poster_pdf` | Migrated |

## Files

### Java (reference, unmodified)

- `app/core/.../controller/api/PdfOverlayController.java`
- `app/core/.../controller/api/EditTableOfContentsController.java`
- `app/core/.../controller/api/PosterPdfController.java`
- `app/core/.../controller/api/SplitPdfBySectionsController.java`
- `app/core/.../controller/api/SplitPdfByChaptersController.java` (verified only)
- `app/core/.../model/api/general/OverlayPdfsRequest.java`
- `app/core/.../model/api/EditTableOfContentsRequest.java`
- `app/core/.../model/api/general/PosterPdfRequest.java`
- PDFBox 3.0.8 sources consulted: `multipdf/Overlay.java`, `multipdf/LayerUtility.java`,
  `PDResources.java`, `PDOutlineNode.java`, `PDOutlineItem.java`, `PDPageXYZDestination.java`

### Rust

- `rust/src/pdfdoc.rs` — new shared helpers (see below); `page_as_form` refactored to
  share the new `form_view_box` helper.
- `rust/src/handlers/general.rs` — `overlay_pdfs`, `edit_table_of_contents`,
  `extract_bookmarks` + guide builders and JSON shaping.
- `rust/src/handlers/split.rs` — `poster_pdf` + page-size/factor validation.
- `rust/src/handlers/mod.rs` — route registrations (additive only).

## Implementation details

### `overlay-pdfs`

- Multipart: base PDF as `fileInput`, overlays as repeated `overlayFiles` parts,
  parsed via the existing `receive_files_multipart` + `misc::dispatch` (keeps
  `?async=true` job support). The `#[endpoint]` macro cannot express repeated
  file fields, so these are hand-written handlers following the `add_attachments`
  pattern.
- Validation mirrors Java order: overlay files required/non-empty → base load →
  `overlayMode` required (`null` → "Invalid overlay mode format: null") →
  `overlayPosition` int (0 = foreground, else background) → `counts` eagerly
  parsed (Spring binds it in every mode).
- The three modes are reduced to a per-base-page `(overlay file, page)` guide,
  matching PDFBox `Overlay`'s `LayoutPage` (which always reads page 0 — Java's
  sequential mode emulates page selection with single-page temp files; the Rust
  port keeps the page index directly):
  - `SequentialOverlay`: file cursor advances before the first page (multiple
    files start at the *second* file) and wraps on exhaustion.
  - `InterleavedOverlay`: base page `i` → file `(i-1) % n`, page 0 (the Java
    page-count check is provably always true).
  - `FixedRepeatOverlay`: `counts[i]` repeats × overlay page count consecutive
    base pages per file, page 0; length mismatch and negative counts handled.
- `pdfdoc::overlay_page` replicates `Overlay.overlayPage`: the overlay page's
  decoded content becomes a Form XObject (`/BBox` retranslated to origin,
  `/Resources` deep-copied with remapped ids, `/Matrix` rotation compensation via
  the same `Affine` translate/quadrantRotate pairs as Java), registered in the
  base page's effective `/Resources` (inheritable, like `PDPage.getResources`)
  under the next free `OL<n>` name (`PDResources.createKey` semantics).
  `/Contents` is rewritten as an array splicing the *original* stream objects:
  foreground = `q, orig…, Q, overlay`; background = `overlay, orig…`.
  The overlay stream is `q q 1 0 0 1 h v cm /OLn Do Q Q` with the media-box-centred
  shift, including the 90/270 rotation box transposition.

### `edit-table-of-contents` / `extract-bookmarks`

- `extract-bookmarks`: file arrives as `file` (Java `@RequestParam("file")`, not
  `fileInput`); returns the nested `{title, pageNumber, children}` JSON.
  `pdfdoc::outline_bookmarks` mirrors `processChildren`: shared visited set over
  the whole walk (dedupe by indirect object id), depth cap 64, raw titles
  (`/` preserved, `None` when absent), `pageNumber` 1-based with `1` for
  non-local destinations (Dest arrays, `/A` GoTo actions and named destinations
  resolved via the existing `outline_dest_page`).
- `edit-table-of-contents`: `bookmarkData` JSON → `{title, pageNumber, children}`
  tree; page numbers clamped to `[1, n]` like `createOutlineItem`; the whole
  `/Outlines` tree is replaced (old outline objects become unreachable, as in
  Java). `pdfdoc::write_outline` emits PDFBox-equivalent structure: `/XYZ`
  destinations `[page /XYZ null null null]`, `Parent`/`Prev`/`Next`/
  `FirstChild`/`LastChild` links, and PDFBox's count bookkeeping — nodes are
  created closed, so each parent's `/Count` is minus its direct-child count and
  the root's is minus the top-level count. Titles encode as literal ASCII or
  UTF-16BE hex like `COSString`.

### `split-for-poster-print`

- Ports `PosterPdfController`: each source page is imported as a Form XObject
  with MediaBox=CropBox normalization (`pdfdoc::poster_page_form` →
  `page_form_stream` with media=view=crop box), then an `xFactor × yFactor` grid
  of target-size pages is emitted, each drawing the form through the same
  `q / cm / cm / cm /Fm Do / Q` transform sequence (uniform fit-scale, centred,
  RTL column ordering, top-row-first cropping, 90/270 dimension swap).
- Page sizes match `PDRectangle` constants (mm→pt via `72/25.4`, LETTER/LEGAL/
  Tabloid in points). `xFactor`/`yFactor` enforce the `@Min(1)`/`@Max(10)`
  messages; the 100 000-page cap matches `MAX_OUTPUT_PAGES`.
- Output is a ZIP with a single `<base>_poster.pdf`, `/Info` carried over via
  `pdfdoc::carry_info` (approximates `createNewDocumentBasedOnOldDocument`).

## Limitations / divergences

- **Overlay**: per-page resource cloning matches Java (fresh clone per overlaid
  page). Content streams are preserved as objects rather than re-encoded —
  semantically identical, byte-different from PDFBox output. `OL<n>` naming
  follows `createKey`; collision behaviour is equivalent.
- **TOC**: `bookmarkData` parse/validation failures return 400 (Java surfaces
  Jackson errors as 500). `title: null` becomes an empty title (Java may NPE).
  `/Count` values replicate PDFBox's closed-by-default bookkeeping, which is
  spec-quirky (counts direct children, not total descendants) but matches the
  Java output.
- **Poster**: `/Info` carry-over is `carry_info`'s approximation (standard fields
  + re-zoned dates + app Producer/Creator), not the full
  `PdfMetadataService.extractMetadataFromPdf` copy. Content streams are
  uncompressed where Java uses Flate — valid PDF, slightly larger.
- Malformed PDFs surface through `pdfdoc::load` as 400s (Java: 500 for some
  overlay/TOC paths) — consistent with existing Rust handlers.
- `extract-bookmarks` also accepts a `fileInput` upload as a fallback; Java only
  binds `file`.

## Route registrations added (`rust/src/handlers/mod.rs`)

```rust
.route("/api/v1/general/split-for-poster-print", post(split::poster_pdf))
.route("/api/v1/general/overlay-pdfs", post(general::overlay_pdfs))
.route("/api/v1/general/edit-table-of-contents", post(general::edit_table_of_contents))
.route("/api/v1/general/extract-bookmarks", post(general::extract_bookmarks))
```

`split-pdf-by-sections` and `split-pdf-by-chapters` were already registered to
`split::split_by_sections` / `split::split_by_chapters`.

## Formatting

`rustfmt --edition 2021` run on `src/pdfdoc.rs`, `src/handlers/general.rs`,
`src/handlers/split.rs`, and `src/handlers/mod.rs` (the latter with
`skip_children=true` so concurrently-edited sibling handler modules are not
reformatted); `--check` passes clean on all four.
No builds/tests run per task restrictions.
