# Phase 3 — Agent 1: Codebase Integration & Reconciliation

Date: 2026-11-06. Branch: `feature/rust-rewite`. Scope: `rust/` (the
`stirling-server` crate) plus the `rust/engine/` (`stirling-engine`)
crate. No build/test commands were run (phase restrictions); all findings
are from static inspection.

## 1. Files inspected

- `rust/src/main.rs`, `rust/src/handlers/mod.rs` — startup + route table.
- All 24 top-level `rust/src/*.rs` files and all 22
  `rust/src/handlers/*.rs` files are declared (the set grew during the
  phase: `eml`, `image_scans`, `ocr`, `pipeline`, `scanner`,
  `platform`, plus `pdftext/`, `tabula.rs`, `jobstore.rs`, `fonts.rs`,
  `pipeline_table.rs`).
- `rust/macros/src/lib.rs` — the `#[stirling_macros::endpoint]` proc macro.
- `rust/engine/src/**` (54 files) — module declarations in `lib.rs`,
  `routes/mod.rs`, per-directory `mod.rs`/`*.rs` pairs.
- `rust/Cargo.toml`, `rust/engine/Cargo.toml`, `rust/.cargo/config.toml`.
- `rust/tests/` (fixtures, scripts, README).
- Migration reports: `migration/FINAL_MIGRATION_AUDIT.md`,
  `migration/INTEGRATION_REPORT.md`, `migration/reports/agent-01..12.md`,
  `PHASE-01/02/03/04/05/06/08-*.md`, `misc-remainder.md`,
  `platform-hardening.md`, `migration/RUST_MIGRATION.md`, `rust/MIGRATION.md`.
- `git status` / `git log` — the whole migration is **uncommitted** on
  `feature/rust-rewite`; last commits are the Rust backend skeleton and
  `pdf-to-single-page`.

## 2. Integration verification results

### Module declarations — complete

Every `.rs` file under `rust/src/` is declared: `main.rs` declares all
modules (`attachments auth config convert endpointconfig error
fontmetrics fonts forms guard handlers imgcomp imgextract jobstore
metrics page_select pdfdoc pdftext pipeline_table settings tabula
(testkit cfg-test) text upload web`);
`handlers/mod.rs` declares all 22 handler modules. No orphaned source
files remain — the `handlers/images.rs` draft seen earlier in the phase
was removed by concurrent work (it duplicated the routed
`misc::extract_images`/`imgextract.rs` path).

`rust/engine`: `lib.rs` declares all 11 top-level modules; every
subdirectory (`agents`, `contracts`, `docparse`, `documents`, `llm`,
`models`, `routes`) has its `mod.rs`/`*.rs` pair wired, including the
newly-landed `agents/runtime.rs` and `documents/rag_capability.rs`.
`routes/mod.rs::router` merges every route module.

### Route registrations — complete

`handlers/mod.rs` registers **136 `.route()` calls** plus
`metrics::routes()` (6) and the `/api/**` fallback proxy, across **22
handler modules** (`analysis automation convert eml filter form general
image_scans jobs layout merge misc ocr passwords pipeline platform
proxy scanner security split transform visual`). Every routed handler
name resolves to a real function:

- `#[stirling_macros::endpoint]` functions (51 across analysis,
  automation×2, filter×6, general×6, images→n/a, layout×2, merge,
  misc×12, passwords×3, security×3, split×5, transform×2) expand to
  `pub async fn name(State, Query, Multipart)` delegating to a sync
  `name_work` — verified the macro output shape matches the work-fn
  signatures.
- Macro-generated route handlers (`pdf_to_word`, `pdf_to_presentation`,
  `pdf_to_xml`, `pdf_to_text` via `pdf_to_office_route!`) exist.
- Platform endpoints landed this phase are wired: `platform.rs` (11
  routes under `/api/v1/config/*`, `/api/v1/ui-data/*`,
  `/api/v1/settings/get-endpoints-status`, `update-enable-analytics`).
- New `endpointconfig` public API used by `platform.rs`
  (`is_endpoint_enabled`, `all_endpoints`, `endpoint_availability`,
  `disabled_endpoint_statuses`, `is_group_enabled`) and
  `settings::current()` all exist.

### Request lifecycle — traced and coherent

`main` → `Config::from_env` → `settings::load` →
`endpointconfig::init` → `metrics::init` → `auth::warn_if_secured_deployment`
→ `AppState::new(JavaBackend::from_env)` → `handlers::router(state)` →
layers (outermost→innermost): `TraceLayer` → `CorsLayer` (Java
`WebMvcConfig` parity) → `metrics::track` → `auth::auth_guard` →
`guard::api_guard` → explicit routes → `proxy::proxy` fallback →
`web::serve` for non-API paths. Middleware order is correct: auth before
enablement guard before handler; CORS/trace outermost.

`?async=true` flows through `jobs::submit` → `AppState.jobs` →
`/api/v1/general/job/{id}` status/result/files + files endpoints —
verified `jobs::submit`, `is_async`, `async_requested`, `AppState::new`
signatures match the macro's generated call sites.

## 3. Parallel-agent reconciliation

| Claim (report) | Verified state |
|---|---|
| `filter.rs`, `passwords.rs`, `visual.rs` orphaned → wired | Declared + routed; confirmed |
| `text.rs`, `fontmetrics.rs` undeclared in `main.rs` | `mod` declarations present |
| `misc.rs` dead duplicates (`auto_rotate_pdf`, `repair_pdf`) | **Deleted** — no longer present; INTEGRATION_REPORT accurate, FINAL_MIGRATION_AUDIT §B item 4 is stale |
| `handlers/images.rs` early extract-images draft | Removed from tree during this phase; `misc::extract_images` + `imgextract.rs` is the live path |
| `src/convert.rs` engine runner "currently unused" (agent-12) | Now used by `handlers/convert.rs` (`run_engine`, `find_executable`, `EngineSpec`) |
| Form endpoints partial (3/9, INTEGRATION_REPORT §E) | All 9 routed in `handlers/form.rs` |
| Platform endpoints proxied | Now native via `handlers/platform.rs` (11 routes) |

No unmerged agent work was found on other branches — everything lives in
the working tree on `feature/rust-rewite`. The working tree is **under
continuous modification** during this phase (platform.rs, settings.rs,
endpointconfig.rs, misc.rs, form.rs all changed while auditing); findings
reflect the latest snapshot.

## 4. Integration problems found

1. **97 Java files deleted, uncommitted** (`git status`: 97 `.java`
   deletions incl. many `SPDF/controller/api/*Controller.java`, plus
   docker/workflow removals). The `/api/**` proxy fallback depends on the
   Java backend for ~60 unmigrated endpoints. **If these deletions are
   committed before parity verification, every proxied endpoint 404s.**
   This predates Phase 3 and was not touched; flagged as the top
   integration risk. Recommend confirming the deletions correspond only
   to migrated controllers before any commit.
2. **`rust/f.pdf`** — stray 512-byte artifact at crate root, untracked.
   Not referenced by code; safe to delete, left in place.
3. **`rust/engine/` is a separate crate**, not a workspace member of
   `rust/` (no `[workspace]` in `rust/Cargo.toml`); built via
   `task engine:*`. Its own `target/` and `vendor/` dirs are untracked —
   confirm `.gitignore` coverage before committing.
4. **Compile status unverified** — phase restrictions forbade
   `cargo check/build/test/clippy`. Static inspection found no missing
   `mod`, route, or import, but the first real compile may still surface
   issues in the churned files (platform.rs, settings.rs landed mid-audit).

## 5. Changes implemented by this agent

None required — the integration layer was already coherent at snapshot
time. (Test additions are Agent 2's scope; see
`PHASE-03-AGENT-02-TESTING.md`.)

## 6. Duplicate / conflicting implementations

- `handlers/images.rs` — superseded duplicate of `misc::extract-images`;
  already removed from the tree.
- No other duplicates found; `misc.rs` dead handlers were previously
  removed.

## 7. Remaining disconnected components

- **`src/pdftext/common_converter.rs`** (1031 lines, landed late in the
  phase): a complete port of Java's `BasicPdfMarkdownExtractor` — the
  OSS/common-classpath variant of the PDF→Markdown seam — but it is
  **not declared** in `pdftext/mod.rs` and nothing references it. The
  live path is `pdftext/converter.rs` (`AdvancedPdfMarkdownConverter`
  port), called from `handlers/convert.rs`. Left undeclared deliberately
  rather than wired as dead code or deleted: the owning agent's intent
  (fallback extractor vs. superseded draft) is unclear, and the file is
  only hours old. Owner decision needed: declare + `#[allow(dead_code)]`,
  wire as a fallback, or remove.
- Everything else is reachable: handlers via the router, utilities via
  handler call sites (`pdfdoc`, `upload`, `error`, `page_select`,
  `testkit`, `pdftext/converter`, `tabula`, `jobstore`, …).

## 8. Features still requiring Java fallback

Per `FINAL_MIGRATION_AUDIT.md` §C, still proxied (~60 endpoints):
`/convert/**` remainder (sidecar-blocked), security signing family
(JPDFium-dependent: cert-sign, timestamp, redact×3, validate-*,
verify-pdf, prepare/restore-pdf-session, hardware signing), raster misc
(ocr-pdf, extract-image-scans, auto-split-pdf, remove-blanks,
scanner-effect), pipeline `handleData`, and miscellaneous platform GETs.
The proxy path itself (`proxy.rs` → `JAVA_BACKEND_URL`, `web.rs` SPA
serving) is intact and verified wired.

## 9. Verification status

- Static wiring audit: **complete** (this document).
- `cargo test --bin stirling-server`: **executed — compiles clean;
  433 passed / 0 failed / 1 ignored (434 tests).** Earlier in-flight
  failures in `handlers/pipeline.rs`, `tabula.rs`, and a `metrics.rs`
  name collision (`wau` accessor vs handler) were resolved by the
  concurrent owners during the pass.
- `cargo clippy` / `cargo fmt --check`: not run this pass.
- Live smoke: covered by `tests/scripts/smoke.sh`; requires a permitted
  run against a live server.
