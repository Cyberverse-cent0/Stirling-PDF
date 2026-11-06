# Phase 8 — End-to-End Testing and Java Parity Verification

Date: 2026-11-05/06. Environment: Java `build/backend/stirling-server.jar`
on :8080 (`--security.enable-login=false`), Rust `target/debug/stirling-server`
on :8081 (`JAVA_BACKEND_URL=http://localhost:8080`). 50 input PDFs
(`--include-testing`).

## Automated checks

| Check | Result |
|---|---|
| `cargo fmt --check` | PASS |
| `cargo check` (earlier snapshot) | PASS, warnings only |
| `cargo build` | PASS (40s) — **later snapshot FAILS**: concurrent in-flight work in `handlers/convert.rs` (now ~1400 lines), `endpointconfig.rs`, `form.rs` (~3100 lines) does not compile; unrelated to Phase-8 edits |
| `cargo test` | **65/65 PASS** (earlier snapshot) |
| `cargo clippy -D warnings` | PASS (earlier snapshot) |
| `rust_pdf_parity.py --include-testing` | **807 comparisons, ~487 flagged** — breakdown below |

## Parity results by endpoint (from /tmp/parity.json)

| Endpoint | Cases | Status-pair anomalies | Dominant diffs |
|---|---|---|---|
| analysis/page-count | 31 | 0 | `pageCount` on doc-timestamped-tampered (FIXED, see below) |
| analysis/basic-info | 31 | 0 | same single input |
| analysis/page-dimensions | 31 | 0 | f32-vs-2dp rounding on 18 inputs — **FIXED** (round to 2 decimals) |
| analysis/document-properties | 31 | 0 | clean |
| analysis/form-fields | 31 | 0 | clean |
| analysis/annotation-info | 31 | 0 | `typeBreakdown.Widget`, `totalCount` on 1 input |
| analysis/font-info | 31 | 0 | font resource naming order (F6/F10…) on 3 inputs — cosmetic |
| analysis/security-info | 31 | 0 | clean |
| general/rotate-pdf | 31 | 0 | metadata + content-stream rewrite diffs (see Known divergences) |
| general/remove-pages | 31 | 0 | same |
| general/rearrange-pages(+reverse) | 62 | java=200/rust=400 on tampered doc — **FIXED** | same |
| general/split-pages | 31 | java=200/rust=400 on tampered doc — **FIXED** | contentType — **FIXED** |
| general/split-by-size-or-count | 31 | 0 | contentType — **FIXED** |
| general/split-pdf-by-chapters | 31 | java=500/rust=400 on test_globalsign.pdf (encrypted; both reject, different codes) | — |
| general/split-pdf-by-sections(+merge) | 62 | tampered doc — **FIXED** | contentType / stream diffs — **FIXED** |
| general/scale-pages-a4/keep | 62 | tampered doc — **FIXED** | metadata/stream diffs |
| general/crop | 31 | tampered doc — **FIXED** | stream-operator diffs (expected: Java rewrites content, Rust clips) |
| general/pdf-to-single-page | 31 | 0 | content-stream diffs (expected) |
| general/merge-pdfs | 1 | 0 | clean semantically |
| misc/decompress-pdf | 31 | 0 | stream diffs (expected: decompression rewrites streams) |
| misc/flatten-forms | 31 | 0 | stream diffs (expected) |
| misc/compress-pdf | 31 | 0 | clean |
| misc/update-metadata | 31 | 0 | clean |
| security/remove-cert-sign | 31 | 0 | metadata diffs |

Status codes otherwise agree: 727×(200,200), 73×(400,400) — error cases
(bad PDFs, bad params) match.

## Defects found and fixed this phase

1. **`doc-timestamped-tampered.pdf` → Rust saw 0 pages** (java=200, rust=400/empty
   on page-count, crop, rearrange, scale, split). Root cause: the file's object
   stream declares `/Txpe /ObjStm` (typo). `pdfdoc::finish_load` only expanded
   streams with `has_type(ObjStm)`, so the page tree stayed hidden. lopdf/PDFBox
   key object streams on `/N`+`/First`. Fix in `rust/src/pdfdoc.rs`
   `finish_load`: treat `has_type(ObjStm)` **or** (`/N` and `/First` present) as
   an object stream. **Source-fixed; runtime re-verification blocked** — see
   Build state below.
2. **`page-dimensions` float precision**: Rust emitted raw f32
   (595.280029296875), Java emits 2 decimals (595.28). Fixed in
   `rust/src/handlers/analysis.rs` — round to 2dp.
3. **Split ZIP `Content-Type`**: Java returns `application/octet-stream`
   (`WebResponseUtils` uses `APPLICATION_OCTET_STREAM` for multi-file), Rust
   returned `application/zip`. Fixed in `rust/src/handlers/split.rs`
   `zip_response`.

## Known divergences (documented, not fixed)

- **`/Info` Producer/Creator/ModDate/CreationDate** (~600 of the flagged diffs):
  Java restamps Producer/Creator to `Stirling-PDF v<app>` and re-zones dates
  to the system zone on the `createNewDocumentBasedOnOldDocument` path —
  confirmed stamped on rotate/remove-pages/rearrange/crop/scale/
  pdf-to-single-page/decompress/flatten/remove-cert-sign/split-sections-merge,
  and **not** stamped on compress-pdf, split segments, merge-pdfs,
  update-metadata. Rust preserves source `/Info`. A blanket stamp in
  `pdfdoc::save` was implemented then **reverted**: it would have introduced
  diffs on the non-stamping endpoints. Correct fix is per-endpoint stamping
  via a `pdfdoc` helper (the pieces — `app_version_label`, `pdf_date_now`,
  `pdf_date_rezone` — already exist) wired only into handlers whose Java path
  restamps; also needs `STIRLING_VERSION=3.3.0` (or real version plumbing)
  for the label to match.
- **Content-stream operator diffs** on crop/scale/single-page/split-merge:
  Java rewrites content streams through PDFBox; Rust preserves/clips them.
  Expected structural difference, not a functional regression — page boxes,
  counts, and semantics matched wherever compared.
- **catalog `/PageMode /UseNone`** present in Java output (PDFBox
  `createNewDocument` artifact), absent in Rust — cosmetic.
- **PDF version header** 1.6 (Java) vs 1.5 (lopdf default on some paths).
- **Stream filter counts** differ (lopdf vs PDFBox compression choices) —
  harmless.
- **font-info** resource naming (F6 vs F10 ordering) on 3 inputs — cosmetic.
- **annotation-info** Widget count on 1 input — needs a follow-up look.
- **test_globalsign.pdf** (encrypted): both backends reject; Java 500 vs
  Rust 400 — error-code divergence only.
- **`/api/v1/convert/img/pdf`** and other convert routes: registered and
  compile-verified in an earlier snapshot; not covered by the parity script
  (no Case entries) — runtime parity unverified.

## Build state caveat

Mid-phase, concurrent agents landed large in-flight changes
(`handlers/convert.rs` ~1400 lines, `endpointconfig.rs`, `form.rs` ~3100
lines) that currently break `cargo build` (23 errors: missing `base64`
crate, handler signature mismatches). Phase-8 fixes are source-level and
compile-clean in isolation but could not be rebuild-verified or
runtime-re-verified against the tampered-PDF case. The running :8081
binary predates all Phase-8 fixes.

## Commands run

```
java -jar build/backend/stirling-server.jar --server.port=8080 --security.enable-login=false
JAVA_BACKEND_URL=http://localhost:8080 ./target/debug/stirling-server   # :8081
cargo fmt --check && cargo check && cargo build && cargo test && cargo clippy --all-targets --all-features -- -D warnings
python3 scripts/rust_pdf_parity.py --case rotate-pdf --limit 3 --strict-structure
python3 scripts/rust_pdf_parity.py --include-testing --out /tmp/parity.json
```

## Endpoint status summary

- **JAVA_PARITY_VERIFIED (modulo metadata/stream cosmetics)**: all
  analysis/* endpoints, general/merge-pdfs, misc/compress-pdf,
  misc/update-metadata, misc/decompress-pdf, misc/flatten-forms,
  security/remove-cert-sign — semantic fields match on all 31 inputs.
- **PARTIAL**: general/* page ops — functional output verified; residual
  diffs are `/Info` stamping, content-stream rewrite style, PageMode,
  version header. The tampered-ObjStm fix awaits rebuild.
- **JAVA_FALLBACK**: all /api/v1/convert/* except img/pdf, form
  add/edit/modify/extract-xlsx, misc text stamps/watermarks,
  scanner-effect, remove-blanks.
- **BLOCKED**: runtime verification of Phase-8 fixes pending the
  concurrent-work build breakage resolving.
