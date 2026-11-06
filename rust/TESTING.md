# Testing the Rust backend

Two crates live under `rust/`:

- **`stirling-server`** (`rust/`, binary crate) — the API gateway +
  migrated PDF endpoints.
- **`stirling-engine`** (`rust/engine/`, lib+bin crate) — the Rust port of
  the Python AI engine.

Both are **binary/lib crates without a published lib target for
`stirling-server`**, so `tests/*.rs` cannot import handler modules.
Tests therefore live in three layers:

## 1. Unit tests — `#[cfg(test)] mod tests` inside `src/`

The primary layer. Tests call the synchronous `*_work` functions and pure
helpers directly — no HTTP server, no manual fixture files. PDFs are built
with `lopdf` through `src/testkit.rs` (`testkit::Pdf`), which serialises
through the same `pdfdoc::save` path real uploads take.

Run them:

```bash
cd rust
cargo test --bin stirling-server                          # everything
cargo run -p stirling -- test unit handlers::split        # one module
cargo run -p stirling -- test unit testkit::fixture_tests # regenerate fixtures
```

`testkit::fixture_tests::writes_sample_fixtures`
regenerates `tests/fixtures/*.pdf` on every run — fixtures are generated,
not checked in (`tests/fixtures/.gitignore`).

## 2. Engine tests — `rust/engine/tests/*.rs`

`stirling-engine` *is* a library crate, so its integration tests import
`stirling_engine::` directly:

| File | Covers |
|---|---|
| `tests/chunking.rs` | paragraph/sentence packing, overlap tails, page metadata |
| `tests/document_store.rs` | document store trait behaviour |
| `tests/http_classify.rs` | request classification middleware |
| `tests/llm_client.rs` | LLM client request/response shapes |
| `tests/contracts.rs` | contract serde: camelCase fields, snake_case tags, internally-tagged unions, `deny_unknown_fields` |
| `tests/config_error.rs` | `DocumentsBackend` parsing, `ApiError` → `{"detail"}` bodies + status mapping, anyhow → 500 |

```bash
cargo run -p stirling -- test engine    # or: task engine:test
```

## 3. Live smoke tests — `stirling test`

The `stirling` CLI (`rust/cli`) boots the real server
(`JAVA_BACKEND_URL=off`, spawned directly — no shell) and POSTs the
generated fixtures over HTTP:

| Command | Scope |
|---|---|
| `stirling test runtime` | one endpoint per migrated family + fallback/404 behaviour (12 checks) |
| `stirling test general` | general unit tests + live `remove-image-pdf` |
| `stirling test security` | security unit tests + live `sanitize-pdf`/`get-info-on-pdf` |
| `stirling test` | fmt + clippy + workspace tests (the old `test-all.sh` gate) |
| `stirling test unit [filter]` | unit tests, optionally filtered |
| `stirling test engine` | engine crate tests |
| `stirling test coverage` | per-file test-count inventory |

```bash
cargo run -p stirling -- test runtime              # multi-endpoint smoke
SMOKE_PORT=18097 cargo run -p stirling -- test runtime
cargo run -p stirling -- test coverage             # file → test-count table
# or point at a prebuilt binary while the tree is mid-edit:
STIRLING_SERVER_BIN=target/debug/stirling-server cargo run -p stirling -- test runtime
```

## Per-file dispositions

Every `src/**/*.rs` file has a documented disposition in
`migration/reports/PHASE-03-AGENT-02-TESTING.md` (source-to-test
inventory). Summary:

- **Unit-tested in-file** — all handler modules (`analysis`,
  `automation`, `convert`, `eml`, `filter`, `form`, `general`,
  `image_scans`, `jobs`, `layout`, `merge`, `misc`, `ocr`, `passwords`,
  `pipeline`, `platform`, `proxy`, `scanner`, `security`, `split`,
  `transform`, `visual`), `pdfdoc`, `page_select`, `upload`, `imgcomp`,
  `imgextract`, `forms`, `attachments`, `text`, `settings`, `config`,
  `auth`, `error`, `metrics`, `web`, `fontmetrics`, `endpointconfig`,
  `convert`, `jobstore`, `pipeline_table`, `tabula`, `pdftext/model`,
  `pdftext/word_geometry`, `pdftext/line`, `pdftext/gfm_table`,
  `pdftext/column_ranges`, `pdftext/shared`, `pdftext/redact`,
  `fonts`, `guard` (`forbidden` body only).
- **Fixture infrastructure** — `testkit.rs` (the fixture factory itself).
- **Not independently testable** — `main.rs` (wiring only),
  `handlers/mod.rs` (route table; exercised by smoke tests).
- **Orphaned module** — `pdftext/common_converter.rs` exists on disk but
  is not declared in `pdftext/mod.rs` and is referenced nowhere; it has
  no tests and does not compile into the crate. Tracked as a disconnected
  component in `PHASE-03-AGENT-01-INTEGRATION.md` §7.
- **Thin/untested-by-design** — `main.rs`, `handlers/mod.rs` (wiring;
  covered by smoke), most of
  `pdftext/**` (geometry/table primitives have in-file tests —
  `line`, `gfm_table`, `column_ranges`, `shared`, `model`,
  `word_geometry`, `redact`; the markdown pipeline stages
  (`converter`, `heading`, `paragraph`, `page_rules`, `table_*`, …)
  are exercised through handler tests),
  engine `contracts/*`/`routes/*`/`documents/*` modules
  (shared derive patterns; representative coverage in
  `engine/tests/contracts.rs` + `config_error.rs`).
- **Engine** — contracts covered by `tests/contracts.rs`; chunking,
  stores, middleware, LLM client by the other `tests/` files; route
  handlers are thin bindings over tested services.

## Known limitations

- **Verification status** — `cargo test --bin stirling-server` was run
  during the stabilization phase: the crate compiles and reports
  **433 passed / 0 failed / 1 ignored**. `cargo clippy`,
  `cargo fmt --check`, the engine suite, and the live smoke scripts
  have not been run this pass.
- **No line-coverage tooling configured** — `stirling test coverage`
  counts test functions per file; `cargo llvm-cov`/`tarpaulin` can be
  added later.
- **Java-parity golden tests** live outside this tree:
  `scripts/rust_pdf_parity.py` + `testing/parity.sh` compare live
  Java↔Rust output and require both backends running.
- **Engine route handlers** (`engine/src/routes/*`) are thin axum
  bindings; behaviour is covered at the service/contract layer, not per
  route.
