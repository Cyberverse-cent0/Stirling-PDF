# Phase 2 — General PDF Operations: audit and gap fixes

Scope: all general PDF controllers under the Java source tree vs their Rust
equivalents — multi-page layout, booklet imposition, poster generation,
overlays, table-of-contents editing, split-by-sections, page scaling, crop,
single-page generation, page arrangement/splitting — plus the incomplete-
functionality list (widget annotations, AcroForm references, OCProperties,
page resources, rotation matrices, page boxes, font/resource preservation).

Most endpoints were already implemented by earlier migration agents
(`agent-01.md`, `agent-02.md`, and the Phase-1 work recorded in
`rust/MIGRATION.md`). This pass audited them against the Java sources
(recovered via `git show HEAD:` since the migrated controllers are deleted
in the working tree) and fixed the one real behavioural gap found.

## Endpoint audit

| Java controller / endpoint | Rust handler | Status |
|---|---|---|
| `MultiPageLayoutController` → `general/multi-page-layout` | `layout::multi_page_layout` | Implemented (agent-01) |
| `BookletImpositionController` → `general/booklet-imposition` | `layout::booklet_imposition` | Implemented (agent-01) |
| `PosterPdfController` → `general/split-for-poster-print` | `split::poster_pdf` | Implemented |
| `PdfOverlayController` → `general/overlay-pdfs` | `general::overlay_pdfs` | Implemented (agent-02) |
| `EditTableOfContentsController` → `general/edit-table-of-contents` | `general::edit_table_of_contents` | Implemented |
| `RearrangePagesPDFController` → `general/extract-bookmarks` companion | `general::extract_bookmarks` | Implemented |
| `SplitPDFController` → `general/split-pages` | `split::split_pages` | Implemented |
| `SplitPdfBySizeController` → `general/split-by-size-or-count` | `split` size/count modes | Implemented |
| `SplitPdfByChaptersController` → `general/split-pdf-by-chapters` | `split::split_by_chapters` | Implemented |
| split-by-sections → `general/split-pdf-by-sections` | `split` sections mode | Implemented |
| `ScalePagesController`-equivalent → `general/scale-pages` | `transform::scale_pages` | Implemented |
| `CropController` → `general/crop` | `transform::crop_pdf` | Implemented (manual crop; `autoCrop` rejected — needs rasterisation) |
| single-page generation → `general/pdf-to-single-page` | `pdfdoc::to_single_page` | Implemented |
| `RearrangePagesPDFController` → `general/rearrange-pages`, `general/remove-pages` | `general::rearrange_pages`, `general::remove_pages` | Implemented |
| `MergeController` → `general/merge-pdfs`, `RotationController` → `general/rotate-pdf` | `merge::merge_pdfs`, `general::rotate_pdf` | Implemented |

## Gap found and fixed: orphaned AcroForm pruning

Java calls `FormUtils.pruneOrphanedFormFields(document)` after page
removal/splitting in exactly four places (verified via `git grep` on HEAD —
the only production callers):

- `RearrangePagesPDFController.deletePages` → `/general/remove-pages`
- `SplitPDFController.writeSplitsViaPdfBox` → `/general/split-pages`
- `SplitPdfBySizeController` → `/general/split-by-size-or-count`
- `SplitPdfByChaptersController` → `/general/split-pdf-by-chapters`

`/general/rearrange-pages` does **not** prune in Java (pages are reordered,
not removed), so Rust correctly does not either.

### Rust implementation

`pdfdoc::prune_orphaned_form_fields` (`rust/src/pdfdoc.rs`) ports the Java
algorithm:

- Collects the live widget set: `/Annots` entries on every remaining page
  whose `/Subtype` is `/Widget` (indirect ids only — direct annots have no
  stable identity, same limit as Java's dictionary-identity check).
- Walks `/AcroForm /Fields` recursively with a visited set (drops repeated
  or cyclic references like Java's `IdentityHashMap`).
- Non-terminal nodes (kids that are fields) are kept only if a descendant
  survives; `/Kids` is rewritten to the surviving children.
- Terminal fields are kept only while at least one widget is live —
  including the merged field/widget case where the field dictionary itself
  is the page annotation — and `/Kids` is rewritten to the live widgets.
- `/AcroForm` is removed from the catalog when no fields remain.

Wired into:

- `general::remove_pages` — after `pdfdoc::subset`
- `split::segment_bytes` — covers `split-pages` and `split-by-size-or-count`
- `split_by_chapters` — per output segment, matching Java's per-doc call

The stale "pruneOrphanedFormFields is not ported" comment in `general.rs`
was removed.

## Verification

- `cargo test --bin stirling-server` — 55 tests pass, including two new
  `pdfdoc` tests: fields whose widgets were on removed pages are dropped
  (and `/AcroForm` removed when nothing survives); live widgets and their
  fields are preserved.
- `rustfmt --edition 2021` run on the touched files only.

## Remaining known divergences (unchanged, documented in MIGRATION.md)

- Outline destinations are not retargeted after page-tree rebuilds
  (remove/rearrange/split) — Java's PDFBox save does not retarget them
  either in these paths, but bookmark *page numbers* in split outputs can
  differ.
- `scale-pages`/`crop` wrap content streams rather than using PDFBox
  `LayerUtility` Form XObjects; transformed pages drop annotations.
- `crop` is clip-only — `autoCrop` rejected, `removeDataOutsideCrop` does
  not invoke Ghostscript.
- `merge-pdfs` uses page import (document-level structures not carried).
- `split-for-poster-print`, `multi-page-layout`, `booklet-imposition`:
  content-stream/Form-XObject placements differ structurally from Java's
  `LayerUtility.importPageAsForm` path — see `agent-01.md`.
- `edit-table-of-contents` writes the outline via `pdfdoc::write_outline`;
  deep outline-tree edge cases (named destinations, remote go-to actions)
  are approximations.

## Files changed this pass

- `rust/src/pdfdoc.rs` — `prune_orphaned_form_fields` + helpers
  (`live_widget_ids`, `is_pure_widget`, `prune_field_node`,
  `write_back_field`) + two unit tests.
- `rust/src/handlers/general.rs` — call site in `remove_pages`, stale
  comment removed.
- `rust/src/handlers/split.rs` — call sites in `segment_bytes` and
  `split_by_chapters`.
- `rust/MIGRATION.md` — statuses and divergence notes updated.
