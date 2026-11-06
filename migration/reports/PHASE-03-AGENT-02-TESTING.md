# Phase 3 — Agent 2: Test Infrastructure & Systematic Coverage

Date: 2026-11-06. Scope: `rust/` (`stirling-server`) and `rust/engine/
(`stirling-engine`). `cargo test --bin stirling-server` was executed
during the stabilization phase: the crate compiles and
`cargo test --bin stirling-server` reports **433 passed / 0 failed /
1 ignored**. `cargo clippy`, `cargo fmt --check`, and the engine suite
were not run.

## 1. Test architecture

Three layers, matching the binary-crate constraint (a binary crate's
modules cannot be imported from `tests/`):

1. **Unit tests** — `#[cfg(test)] mod tests` inside each `src/` file.
   Tests call the synchronous `*_work` functions and pure helpers
   directly. `src/testkit.rs` builds minimal PDFs with `lopdf` and
   serialises them through `pdfdoc::save` — the same path real uploads
   take — so no binary fixtures are needed. `testkit::fixture_tests`
   regenerates `tests/fixtures/*.pdf` (gitignored) for the shell smokes.
2. **Engine integration tests** — `rust/engine` is a lib crate, so
   `engine/tests/*.rs` import `stirling_engine::` directly.
3. **Live smoke tests** — `rust/tests/scripts/*.sh` boot the real
   server (`JAVA_BACKEND_URL=off`) and exercise endpoints over HTTP.

## 2. Tests implemented this phase

Test coverage was built across the whole crate during the phase —
~380 `#[test]`/`#[tokio::test]` annotations across 47 source files at
snapshot time (exact per-file counts in §3). Highlights by area:

- **Pure logic**: `page_select` range/`an+b` parsing, `settings` YAML
  merge + size parsing, `endpointconfig` URI→key mapping + enablement,
  `metrics` path normalization, `config` CORS/env parsing, `fontmetrics`
  width tables, `text` direction/line grouping, `fontmetrics`, `error`
  mapping, `upload` naming/param helpers.
- **Handler `*_work` functions**: merge ordering/metadata, split
  range/size/division/poster/chapter logic (incl. Java-faithful
  `merge_same_page_chapters` title folding), misc metadata/compression/
  portfolio helpers, general image-XObject removal, filter/security/
  form/visual/automation/pipeline/tabula/eml/ocr/scanner helpers,
  proxy header filtering, platform config endpoints, job store.
- **PDF round-trips via `testkit::Pdf`**: update-metadata info fields,
  decompress validity, page subset/reorder, rotation, AcroForm fill.
- **Engine contracts**: `engine/tests/contracts.rs` — serde shapes,
  tagged unions, `deny_unknown_fields`.

Representative earlier additions (superseded by the counts above):
`convert.rs` engine-runner tests, `handlers/convert.rs` SVG/CBZ/image
magic tests, `handlers/automation.rs` scanner-session tests,
`handlers/proxy.rs` header tests, `handlers/visual.rs` filename/raster
gates.

## 3. Source-to-test inventory

147 Rust source files inspected (`rust/src/**` + `rust/engine/src/**` +
`rust/macros`). Test-function counts are `#[test]`/`#[tokio::test]`
annotations per file; the tree grew during the phase (pdftext/,
pipeline, tabula, eml, ocr, scanner landed), so counts reflect the
latest snapshot.

| Source file | Test fns |
|---|---|
| `engine/src/agents/document_classifier.rs` | 9 |
| `engine/src/agents/pdf_comment.rs` | 4 |
| `engine/src/agents.rs` | 0 |
| `engine/src/agents/runtime.rs` | 0 |
| `engine/src/config.rs` | 0 |
| `engine/src/contracts/agent_drafts.rs` | 0 |
| `engine/src/contracts/agent_specs.rs` | 0 |
| `engine/src/contracts/comments.rs` | 0 |
| `engine/src/contracts/common.rs` | 0 |
| `engine/src/contracts/config.rs` | 0 |
| `engine/src/contracts/contradiction.rs` | 0 |
| `engine/src/contracts/docparse.rs` | 0 |
| `engine/src/contracts/document_classifier.rs` | 0 |
| `engine/src/contracts/documents.rs` | 0 |
| `engine/src/contracts/execution.rs` | 0 |
| `engine/src/contracts/health.rs` | 0 |
| `engine/src/contracts/ledger.rs` | 0 |
| `engine/src/contracts/mod.rs` | 0 |
| `engine/src/contracts/orchestrator.rs` | 0 |
| `engine/src/contracts/pdf_comments.rs` | 0 |
| `engine/src/contracts/pdf_create.rs` | 0 |
| `engine/src/contracts/pdf_edit.rs` | 0 |
| `engine/src/contracts/pdf_questions.rs` | 0 |
| `engine/src/contracts/pdf_review.rs` | 0 |
| `engine/src/contracts/progress.rs` | 0 |
| `engine/src/docparse/chunking.rs` | 0 |
| `engine/src/docparse.rs` | 0 |
| `engine/src/documents/chunker.rs` | 0 |
| `engine/src/documents/embedder.rs` | 0 |
| `engine/src/documents/pgvector_store.rs` | 0 |
| `engine/src/documents/rag_capability.rs` | 0 |
| `engine/src/documents.rs` | 0 |
| `engine/src/documents/service.rs` | 0 |
| `engine/src/documents/sqlite_vec_store.rs` | 0 |
| `engine/src/documents/store.rs` | 0 |
| `engine/src/error.rs` | 0 |
| `engine/src/lib.rs` | 0 |
| `engine/src/llm/client.rs` | 0 |
| `engine/src/llm/model.rs` | 0 |
| `engine/src/llm/mod.rs` | 0 |
| `engine/src/main.rs` | 0 |
| `engine/src/middleware.rs` | 0 |
| `engine/src/models.rs` | 0 |
| `engine/src/models/tool_io.rs` | 0 |
| `engine/src/models/tool_models.rs` | 0 |
| `engine/src/models/tool_params.rs` | 0 |
| `engine/src/models/tool_texts.rs` | 0 |
| `engine/src/routes/agents.rs` | 0 |
| `engine/src/routes/docparse.rs` | 0 |
| `engine/src/routes/document_classifier.rs` | 0 |
| `engine/src/routes/documents.rs` | 0 |
| `engine/src/routes/health.rs` | 0 |
| `engine/src/routes/mod.rs` | 0 |
| `engine/src/routes/pdf_comments.rs` | 0 |
| `engine/src/services/operation_shortlist.rs` | 0 |
| `engine/src/services.rs` | 0 |
| `engine/src/services/tool_io_compat.rs` | 0 |
| `engine/src/state.rs` | 0 |
| `engine/tests/agent_runtime.rs` | 2 |
| `engine/tests/chunking.rs` | 21 |
| `engine/tests/config_error.rs` | 5 |
| `engine/tests/contracts.rs` | 7 |
| `engine/tests/document_store.rs` | 2 |
| `engine/tests/http_classify.rs` | 1 |
| `engine/tests/llm_client.rs` | 7 |
| `engine/tests/vec_store_bench.rs` | 4 |
| `engine/tests/vector_store_audit.rs` | 12 |
| `macros/src/lib.rs` | 0 |
| `src/attachments.rs` | 6 |
| `src/auth.rs` | 3 |
| `src/config.rs` | 5 |
| `src/convert.rs` | 9 |
| `src/endpointconfig.rs` | 4 |
| `src/error.rs` | 3 |
| `src/fontmetrics.rs` | 6 |
| `src/fonts.rs` | 6 |
| `src/forms.rs` | 7 |
| `src/guard.rs` | 1 |
| `src/handlers/analysis.rs` | 10 |
| `src/handlers/automation.rs` | 16 |
| `src/handlers/convert.rs` | 42 |
| `src/handlers/eml.rs` | 12 |
| `src/handlers/filter.rs` | 9 |
| `src/handlers/form.rs` | 15 |
| `src/handlers/general.rs` | 8 |
| `src/handlers/image_scans.rs` | 8 |
| `src/handlers/jobs.rs` | 9 |
| `src/handlers/layout.rs` | 14 |
| `src/handlers/merge.rs` | 8 |
| `src/handlers/misc.rs` | 31 |
| `src/handlers/mod.rs` | 0 |
| `src/handlers/ocr.rs` | 1 |
| `src/handlers/passwords.rs` | 3 |
| `src/handlers/pipeline.rs` | 13 |
| `src/handlers/platform.rs` | 9 |
| `src/handlers/proxy.rs` | 6 |
| `src/handlers/scanner.rs` | 2 |
| `src/handlers/security.rs` | 17 |
| `src/handlers/split.rs` | 13 |
| `src/handlers/transform.rs` | 12 |
| `src/handlers/visual.rs` | 20 |
| `src/imgcomp.rs` | 8 |
| `src/imgextract.rs` | 8 |
| `src/jobstore.rs` | 2 |
| `src/main.rs` | 0 |
| `src/metrics.rs` | 5 |
| `src/page_select.rs` | 3 |
| `src/pdfdoc.rs` | 7 |
| `src/pdftext/column_accumulator.rs` | 0 |
| `src/pdftext/column_layout.rs` | 0 |
| `src/pdftext/column_ranges.rs` | 5 |
| `src/pdftext/common_converter.rs` | 0 |
| `src/pdftext/common_heading.rs` | 0 |
| `src/pdftext/converter.rs` | 0 |
| `src/pdftext/form_field.rs` | 0 |
| `src/pdftext/form_values.rs` | 0 |
| `src/pdftext/gfm_table.rs` | 6 |
| `src/pdftext/glyph_stitcher.rs` | 0 |
| `src/pdftext/heading.rs` | 0 |
| `src/pdftext/line_merger.rs` | 0 |
| `src/pdftext/line.rs` | 6 |
| `src/pdftext/markdown_blocks.rs` | 0 |
| `src/pdftext/markdown_text.rs` | 0 |
| `src/pdftext/model.rs` | 5 |
| `src/pdftext/mod.rs` | 0 |
| `src/pdftext/page_images.rs` | 0 |
| `src/pdftext/page_rules.rs` | 0 |
| `src/pdftext/page_stitcher.rs` | 0 |
| `src/pdftext/paragraph.rs` | 0 |
| `src/pdftext/pdf_form_reader.rs` | 0 |
| `src/pdftext/redact.rs` | 11 |
| `src/pdftext/ruled_rows.rs` | 0 |
| `src/pdftext/ruled_tables.rs` | 0 |
| `src/pdftext/rule_grid.rs` | 0 |
| `src/pdftext/shared.rs` | 2 |
| `src/pdftext/table_block.rs` | 0 |
| `src/pdftext/table_finder.rs` | 0 |
| `src/pdftext/table_grid.rs` | 0 |
| `src/pdftext/table_shape.rs` | 0 |
| `src/pdftext/word_geometry.rs` | 4 |
| `src/pipeline_table.rs` | 3 |
| `src/settings.rs` | 2 |
| `src/tabula.rs` | 10 |
| `src/testkit.rs` | 1 |
| `src/text.rs` | 7 |
| `src/upload.rs` | 4 |
| `src/web.rs` | 8 |

`macros/src/lib.rs` (proc-macro crate): no unit tests — it is a
token-rewriting macro whose output is exercised indirectly by every
`#[endpoint]` handler test and the live smoke suite.

## 4. Test infrastructure created

| Artifact | Purpose |
|---|---|
| `rust/tests/scripts/test-unit.sh [filter]` | unit tests, optional cargo name filter |
| `rust/tests/scripts/test-engine.sh` | `stirling-engine` suite (`cargo test --all-targets`) |
| `rust/tests/scripts/smoke.sh` | multi-endpoint live smoke: meta, analysis, general, misc async-job flow, security, form, filter, plus 404/fallback behaviour |
| `rust/tests/scripts/coverage-report.sh` | per-file test-count inventory table |
| `rust/engine/tests/contracts.rs` | contract serde regression suite |
| `rust/TESTING.md` | testing guide: layers, commands, dispositions, limitations |
| `rust/tests/README.md` | updated script index |

Fixture strategy is unchanged and already sound: `testkit::Pdf` generates
PDFs in-test; `testkit::fixture_tests` writes the three smoke fixtures
(`basic.pdf`, `images.pdf`, `sanitize-target.pdf`) on every `cargo test`
run. `tests/fixtures/.gitignore` keeps them out of git.

## 5. Missing coverage / limitations

- The tree changed substantially during the phase (pipeline.rs,
  tabula.rs, platform.rs, pdftext/, eml, ocr, scanner landed; test
  count grew past 400); the §3 inventory reflects the latest snapshot
  but counts will drift.
- `src/guard.rs`, `src/main.rs`, `src/handlers/mod.rs` have no unit tests
  by design (middleware ordering / wiring); covered by `smoke.sh`.
- `engine/src/routes/*` handlers are thin axum bindings — covered at the
  service/contract layer, not per-route.
- Engine contract coverage is representative (`common.rs` shapes + union
  mechanics in `contracts.rs`), not exhaustive across all 19 contract
  modules — they share the same derive pattern.
- PDF regression cases requiring real-world fixtures (encrypted PDFs,
  large files, rotated pages, forms/widgets, signatures) rely on
  `testkit`-built documents and the three generated fixtures; broader
  corpus coverage belongs to `scripts/rust_pdf_parity.py` /
  `testing/parity.sh` against `app/**/src/test/resources`.
- No line-coverage tooling configured; `coverage-report.sh` counts test
  functions per file (`cargo llvm-cov` can be added later).
- Security tests cover implemented behaviour only (passwords.rs
  encrypt/decrypt round-trips, sanitize flags); JPDFium-dependent
  signing/redaction stays on Java and is untested here by design.

## 5a. Late additions (integration pass, same phase)

Follow-up pass over the settled tree added:

- `src/fonts.rs` — 6 tests: bundled-font embedding, exact-name lookup,
  `format_for` extension map, `describe` last-dot splitting (incl.
  leading-dot edge), `custom_fonts` dir scanning via `tempfile`.
- `src/guard.rs` — 1 test: `forbidden()` emits the Spring-style 403 JSON
  body (`status`/`error`/`path`/`timestamp`, no `message` key).
- `src/pdftext/line.rs` — 6 tests: `Line` geometry (top/centre/edges,
  glyph edges skipping whitespace), merged-word ordering, text override.
- `src/pdftext/gfm_table.rs` — 6 tests: empty input, header+separator
  shape, column padding, inline-markdown escaping (pipes can't split
  columns), short-row padding, UTF-16-aware padding for astral chars.
- `src/pdftext/column_ranges.rs` — 5 tests: empty/degenerate input,
  two-band column detection, narrow-gap merging, support threshold,
  >2000pt span guard.
- `src/pdftext/shared.rs` — 2 tests: `Element` page range, `RowSource`
  confirmation levels.
- `engine/tests/config_error.rs` — 5 tests: `DocumentsBackend` parsing
  (incl. case-sensitivity rejection), `ApiError` → FastAPI-style
  `{"detail": ...}` bodies and status mapping, anyhow → 500.

## 6. Script usage

```bash
cd rust
cargo test --bin stirling-server            # all unit tests
./tests/scripts/test-unit.sh handlers::split
./tests/scripts/test-engine.sh              # engine crate
./tests/scripts/smoke.sh                    # live multi-endpoint smoke
SMOKE_PORT=18097 ./tests/scripts/smoke.sh
./tests/scripts/coverage-report.sh          # file → test-count table
./tests/scripts/test-all.sh                 # fmt+clippy+test+domain smokes
# or via Task: task rust:test / task rust:engine:test / task rust:check
```
