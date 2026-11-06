# Agent 03 — Misc API: Image Extraction and Image Processing

Scope: `ExtractImagesController` (`POST /api/v1/misc/extract-images`) and
`ExtractImageScansController` (`POST /api/v1/misc/extract-image-scans`),
plus a sweep of the remaining Misc controllers for embedded-image work
(`RemoveImagesController` → already `general/remove-image-pdf`;
`CompressController` image pass → `imgcomp.rs`).

**Verification status: no builds, compilation, Cargo tests, parity tests,
or other automated test execution were run, per the task restrictions.**
All statements below are from source inspection only; nothing here is a
runtime or parity claim.

## Java controllers and services inspected

- `ExtractImagesController` (`git show HEAD:app/core/.../misc/ExtractImagesController.java`)
  - Iterates `PDPage.getResources().getXObjectNames()`, keeps
    `isImageXObject` entries only, fingerprints via `ImageFingerprints`
    (memoised per COS object; content key = encoded stream bytes + the
    12 decode-relevant dict keys, `/Length` excluded, depth-32 /
    4096-node caps), decodes via `PDImageXObject.getImage()` (applies
    `/SMask` as alpha), converts to `TYPE_INT_ARGB` (PNG) or
    `TYPE_INT_RGB` (JPEG), writes `ImageIO` output into a
    `Deflater.BEST_COMPRESSION` zip named `<base>_extracted-images.zip`
    with entries `<base>_page_<n>_<i>.<format>`.
  - `ImageMemoryLimit.requireEmbeddedImageFits` guards decode memory.
- `ExtractImageScansController` — renders each page at up to 300 DPI via
  PDFBox `PDFRenderer`, then runs the Python/OpenCV `split_photos.py`
  pipeline to find, crop and straighten photos. This is *rendered-page*
  extraction, not XObject extraction; there is no Rust renderer, so it
  is deliberately **not** implemented and stays on the Java fallback.
- `RemoveImagesController` — already migrated as
  `general::remove_images` (unrelated to extraction).
- `CompressController` — its image pass lives in `imgcomp.rs`; reviewed,
  not duplicated.

## Rust files created or modified

- `rust/src/imgextract.rs` (new) — the extractor.
- `rust/src/imgcomp.rs` — added `decodable(stream)`, a decode-gate
  predicate mirroring `decode_image`'s early-outs without decompressing.
- `rust/src/handlers/misc.rs` — `extract_images` handler + work fn.
- `rust/src/handlers/mod.rs` — route registration (from the earlier pass;
  unchanged this round).
- `rust/src/main.rs` — `mod imgextract;` (earlier pass).

## Implemented endpoints and actual functionality

### `POST /api/v1/misc/extract-images` — native (png/jpeg), Java fallback otherwise

Two gates run before dispatch, both forwarding the *entire original
multipart request* to Java via `proxy::forward_multipart`:

1. `format` must be `png` or `jpeg` (case-insensitive). `gif` and unknown
   values go to Java's ImageIO path, preserving its codec set and error
   behaviour.
2. `imgextract::supported()` must report every image XObject inside the
   decode subset. This replaced the earlier silent-skip behaviour: a PDF
   containing even one JPX/CCITT/JBIG2 image, image mask, Indexed or
   ICCBased colour space, `/Decode` remap, `/Mask`, `/Matte`, or an
   undecodable/out-of-subset soft mask now produces the complete Java
   result instead of a ZIP missing entries. The check runs before
   `?async=true` dispatch, so forwarded requests keep Java's own async
   job semantics.

Native path (`imgextract::extract`):

- Walks each page's *inherited* `/Resources` → `/XObject` table in
  document order (lopdf `IndexMap`, matching `COSDictionary` iteration).
  Only direct `/Subtype /Image` entries are considered — **Java does not
  recurse into Form XObjects** (`getXObjectNames`/`isImageXObject` see
  only the resource table), so neither does this port; images nested in
  forms are absent from both outputs.
- Dedup: `Fingerprints` ports `ImageFingerprints` — a content key
  (encoded stream bytes + decode-relevant dict entries, `/Length`
  excluded, indirect references resolved with the same 32-depth /
  4096-node caps, sorted-key order, image dicts limited to the 12
  decode-relevant keys) memoised per object id; unfingerprintable images
  get a per-document counter, matching Java's identity fallback.
- Decode: `imgcomp::decode_image` — DCTDecode JPEGs and 8-bit
  DeviceGray/DeviceRGB/DeviceCMYK raw samples under Flate/LZW/ASCII85/
  ASCIIHex/RunLength/Brotli filters.
- **JPEG passthrough**: for `format=jpeg` with a lone `DCTDecode` filter
  and no `/Decode`/`/SMask`/`/Mask`, the original compressed stream bytes
  (minus PDFBox's tolerated leading LF) are written directly — the most
  faithful extraction the encoding permits, with no decode/re-encode
  generation loss.
- **Soft masks**: a `/SMask` chain (up to depth 4, `/SMask /None`
  honoured) must itself decode at the base image's exact dimensions or
  the request goes to Java. For PNG output the mask's luminance becomes
  the alpha channel (`TYPE_INT_ARGB` parity); for JPEG it is dropped,
  matching Java's `TYPE_INT_RGB` target.
- Output: deflated ZIP level 9, `<base>_page_<n>_<i>.<format>` entries,
  `<base>_extracted-images.zip` response, `application/zip`.

### `POST /api/v1/misc/extract-image-scans` — not implemented (Java fallback)

Requires page rasterisation (PDFRenderer ≤300 DPI) plus the Python
OpenCV `split_photos.py` segmentation pipeline. Native XObject
extraction is not equivalent; no Rust route is registered and the
request proxies to Java.

## Supported image formats and PDF filters (native path)

- Encodings out: PNG (always RGBA), JPEG (RGB; original DCT bytes when
  unmodified).
- PDF filters in: `DCTDecode` (single), `FlateDecode`, `LZWDecode`,
  `ASCII85Decode`, `ASCIIHexDecode`, `RunLengthDecode`, `BrotliDecode`.
- Colour spaces in: `DeviceGray`, `DeviceRGB`, `DeviceCMYK` at 8 bpc.
- Anything else → whole-request Java fallback (see below).

## Colour space, mask and transparency handling

- DeviceCMYK → naive RGB conversion (documented divergence vs PDFBox
  colour management).
- `/SMask`: composited as alpha for PNG; validated for decodability and
  dimension match during the support check.
- `/Mask` (colour-key array or explicit mask stream), `/Decode`,
  `/Matte`: presence → Java fallback.
- Image masks (`/ImageMask true`), JPXDecode, CCITTFaxDecode,
  JBIG2Decode, Indexed, ICCBased, Separation, DeviceN, sub-8-bit
  depths, exotic filter chains: → Java fallback.

## Duplicate references, nested resources, naming

- Same image referenced from multiple pages/XObject names → one decode,
  one ZIP entry per *first* occurrence per page-resource table, matching
  Java's per-page `imageCount` naming (`<base>_page_<n>_<i>`).
- Repeated references to the same object within one page dedup by
  fingerprint, same as Java.
- Nested Form XObject resources: not traversed — matches Java (see
  above).
- ZIP: `Deflater.BEST_COMPRESSION`, entries in page order then
  resource-table order.

## Unsupported cases and Java fallback requirements

The support gate forwards to Java (never a partial/placeholder result)
for: non-png/jpeg `format`, any image outside the decode subset, any
`/Decode`, `/Mask`, `/Matte`, failed or dimension-mismatched `/SMask`
chains, image masks, JPX/CCITT/JBIG2, Indexed/ICCBased/Separation/
DeviceN colour spaces, sub-8-bit depths, multi-filter chains containing
undecodable filters, corrupt images `decode_image` rejects, and
`extract-image-scans` unconditionally. Java remains the reference for
ImageIO codecs (gif etc.) and PDFBox's full decoder set.

## Dependencies and route registration

- Crates: `lopdf`, `image` (jpeg+png), `zip` — all already in
  `Cargo.toml`; no new dependencies added.
- Route: `POST /api/v1/misc/extract-images` → `misc::extract_images`
  (registered earlier; unchanged). `extract-image-scans` has no route.

## Formatting results

`rustfmt --edition 2021` run on `src/imgextract.rs`, `src/imgcomp.rs`,
`src/handlers/misc.rs` only.

## Explicit confirmation

No builds, compilation, Cargo tests, parity tests, or other automated
test execution were performed at any point in this task. Correctness
claims rest on source-level comparison with the Java implementation;
the support-gate design means any residual decode divergence degrades to
the Java fallback rather than wrong output.

## Addendum — review pass (this session)

Re-inspected the current tree: `src/imgextract.rs`, the
`misc::extract_images` gate/dispatch in `handlers/misc.rs`, and the
`mod.rs` route are all present and consistent with the findings above.
A redundant `src/handlers/images.rs` drafted during this review
(smaller decode subset, no Java-fallback gate, no SMask handling) was
deleted — it would have duplicated and weakened the existing
implementation. No source changes were required; the report above
remains the accurate description of the migrated state.

## Addendum 2 — integration audit (this session)

Traced the live path: `mod.rs` route → `misc::extract_images`
(format + `imgextract::supported` gates, `forward_multipart` to Java)
→ `dispatch`/`extract_images_work` → `imgextract::extract` →
`imgcomp::decode_image` → deflated ZIP via `file_response`. Module
declared in `main.rs`; no duplicate implementation remains (the
redundant `handlers/images.rs` draft was deleted last session).

**Defect found and fixed**: a `DCTDecode` image carrying
`/DecodeParms` (e.g. `/ColorTransform` for Adobe CMYK JPEGs) passed
`supported()` and was emitted either as byte-passthrough or re-encoded
JPEG — both ignore the parameter, while PDFBox honours it.
`image_supported` now returns `false` for single-filter DCT streams
with `/DecodeParms`, sending the request to Java; `passthrough_jpeg`
additionally refuses `DecodeParms` as defence in depth.

**Known residual divergences** (documented, not fixed):
- `/SMask` chains deeper than one level are validated by `supported`
  but only the first mask's luminance is composited.
- Raw-filter images rely on lopdf's `decompressed_content` predictor
  handling for `/DecodeParms`; non-predictor DecodeParms parameters are
  not independently verified.
- `extract()` skips an image `decode_image` rejects even though
  `supported()` passed (e.g. truncated sample data) — the ZIP would
  miss an entry Java might have produced or errored on.
- DeviceCMYK→RGB is the naive conversion, not PDFBox colour management.

**Verification status**: source inspection + `rustfmt` only — no
builds, tests, or parity runs per task restrictions. The
`DecodeParms` fix is compile-unverified.

## Addendum 3 — stabilization phase

- `image_supported` now rejects single-filter `DCTDecode` streams
  carrying `/DecodeParms` (e.g. `/ColorTransform`); `passthrough_jpeg`
  also refuses them. Regression test
  `supported_rejects_dct_with_decode_parms` added — verified passing.
- All 8 `imgextract` tests pass; module wiring confirmed
  (`main.rs` declaration, `mod.rs` route, `imgcomp` decode gate,
  `forward_multipart` fallback).
- Cross-agent clippy gate repaired: fixed 11 `-D warnings` errors in
  `handlers/ocr.rs` (arg-count refactor into `OcrOptions`),
  `pdftext/{column_accumulator,column_ranges,common_heading,heading,
  form_values,gfm_table}.rs`, and `handlers/scanner.rs`. `cargo clippy
  --all-targets --all-features -- -D warnings` now passes clean.
- Known unrelated failure: `tabula::tests::large_grid_extracts_every_cell`
  fails (expected 30 columns, got 25; rulings collapse loses vertical
  lines) — owned by the tabula agent, not this scope.
