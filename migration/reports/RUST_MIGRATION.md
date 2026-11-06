# Rust Migration Docs

The Java/Spring backend is being strangler-fig migrated to a pure-Rust
(axum + `lopdf`) backend in `rust/`. Java stays the fallback for everything
not yet ported — see `rust/MIGRATION.md` for the full plan and per-endpoint
divergences.

## Layout

- `rust/` — the Rust backend (`cargo`). Owns migrated `/api/v1/**` routes.
- `migration/` — this directory: migration status, audits and
  parity-testing notes.
- Java sources under `app/` are **untouched** — they are the API contract
  and the fallback backend. Do not delete or move them until the Rust port
  is verified endpoint-by-endpoint.

## Running

```bash
task rust:dev     # Rust server on :8081 (proxies unmigrated /api/** to Java :8080)
task rust:check   # clippy -D warnings + fmt --check + cargo test
```

`JAVA_BACKEND_URL` points the fallback proxy at Java (default
`http://localhost:8080`, `off` disables). `task dev` runs the Java backend
on :8080 as usual — run both to test the strangler topology.

## Migrated endpoints (served by Rust)

Snapshot 2026-11-06; the authoritative per-endpoint list is the route
table in `rust/src/handlers/mod.rs`. See `migration/reports/FINAL_MIGRATION_AUDIT.md`
for the full Java↔Rust reconciliation.

| Area | Endpoints |
|---|---|
| general | `merge-pdfs`, `rotate-pdf`, `remove-pages`, `rearrange-pages`, `split-pages`, `split-by-size-or-count`, `split-pdf-by-chapters`, `split-pdf-by-sections`, `split-for-poster-print`, `remove-image-pdf`, `overlay-pdfs`, `edit-table-of-contents`, `extract-bookmarks`, `scale-pages`, `crop`, `pdf-to-single-page`, `multi-page-layout`, `booklet-imposition` |
| analysis | `page-count`, `basic-info`, `page-dimensions`, `document-properties`, `form-fields`, `annotation-info`, `font-info`, `security-info` |
| misc | `decompress-pdf`, `update-metadata`, `flatten`, `compress-pdf`, `show-javascript`, `add-attachments`, `extract-attachments`, `extract-single-attachment`, `list-attachments`, `rename-attachment`, `delete-attachment`, `batch-process-attachments`, `create-portfolio`, `flatten-portfolio`, `auto-rename`, `auto-rotate-pdf`, `repair`, `unlock-pdf-forms`, `add-page-numbers`, `add-comments`, `add-stamp`, `add-image` (raster only), `replace-invert-pdf` (CMYK mode only) |
| security | `sanitize-pdf`, `get-info-on-pdf`, `remove-cert-sign`, `add-password`, `remove-password`, `add-watermark` (image type only) |
| filter | `filter-contains-text`, `filter-contains-image`, `filter-page-count`, `filter-page-size`, `filter-file-size`, `filter-page-rotation` |
| mobile-scanner | `create-session`, `validate-session`, `upload`, `files`, `download`, `session` DELETE |
| jobs | `?async=true` on migrated endpoints; `GET /api/v1/general/job/{id}[/result|/result/files]`, `DELETE /job/{id}`, `GET /api/v1/general/files/{id}[/metadata]`, `POST /api/v1/general/jobs/cleanup` |

Everything else under `/api/**` is proxied to Java transparently.

## Parity testing (old Java vs new Rust)

The Java controllers are the spec. For each migrated endpoint:

1. **Request/response shape** — compare the Java controller's
   `@ModelAttribute`/`@RequestParam` fields and response headers
   (`Content-Disposition` filename suffixes, `_merged_unsigned.pdf` etc.)
   against the Rust handler. Golden rule: same multipart field names, same
   output filename rules (`GeneralUtils.generateFilename` parity lives in
   `rust/src/upload.rs`).

2. **Golden files** — run the same input through both backends with the
   parity harness. It posts every case to Java (`:8080`) and Rust (`:8081`),
   fingerprints PDF/ZIP outputs with `rust/examples/pdf_fingerprint.rs`
   (a `lopdf` object-model fingerprint: page boxes/rotation, annotations,
   fonts, content operators, Info/catalog flags, object/stream counts), and
   reports semantic diffs separately from structural ones:
   ```bash
   # Java on :8080, Rust on :8081
   python3 scripts/rust_pdf_parity.py --include-testing --out /tmp/parity.json
   python3 scripts/rust_pdf_parity.py --case rotate-pdf --limit 3 --strict-structure
   ```
   Byte-identical output is NOT expected (different writers); the harness
   compares page count, boxes, rotation, annotations, fonts, content
   operators, metadata flags, and object/stream structure instead. Input
   PDFs default to `app/**/src/test/resources` and `app/**/test/resources`;
   `--include-testing` adds `testing/test_pdf_*.pdf` and the other
   `testing/` fixtures.

3. **Async jobs** — `?async=true` returns `{async, jobId, result}` on both.
   Poll `/api/v1/general/job/{id}` then fetch `/result`; Rust stores JSON
   results inline and file results via `/files/{fileId}` like Java.

4. **Known divergences** — check the "Known divergences" section of
   `rust/MIGRATION.md` before filing a bug; the parity harness tags the
   documented ones as `known` (form-field pruning, AcroForm/outline
   handling on page import, date formats, page-count caps).

## What is NOT migrated yet

Content-authoring ops (watermark, stamp, page-numbers, blank-page),
render-dependent ops (auto-rename, OCR/auto-rotate), remaining
content transforms (layouts, booklet, sections, overlay; scale and manual
crop are migrated), full flatten/compress modes that still need Java's
renderer or ImageMagick, security writes (passwords, redact, sign, timestamp),
all of ConvertApi (needs sidecars), and platform services (auth, settings,
pipeline, admin). These still proxy to Java.
