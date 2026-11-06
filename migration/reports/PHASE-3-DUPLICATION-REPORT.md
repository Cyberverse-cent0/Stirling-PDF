# Phase 3 — Duplication & Redundancy Report

## Consolidated during this phase

| Duplicate | Copies found | Resolution |
|---|---|---|
| `first_upload` (identical 6-line fn) | `handlers/convert.rs`, `filter.rs`, `layout.rs`, `passwords.rs` | Moved to `upload::first_upload`; all 4 call sites + imports updated. ~30 call sites in convert/filter/layout unaffected. |
| `simple_name` (`Filenames.toSimpleFileName` port) | `handlers/automation.rs` (`pub(crate)`), `filter.rs`, `misc.rs`, `ocr.rs`, `attachments.rs` — 5 identical copies | Moved to `upload::simple_name`; all copies deleted, imports updated incl. `pipeline.rs` which imported `automation::simple_name`. |
| `int_param`/`float_param` strict-parse helpers | `layout.rs` (i64/f64) and `visual.rs` (i32/f64) — identical bodies modulo int width | Added generic `upload::param_parse<T: FromStr>`; both modules' helpers now one-line delegations. Local wrappers kept because call sites (44) and the i32-vs-i64 Java-binding distinction are load-bearing. |
| Duplicate test `engine_spec_builder_collects_args` | `convert.rs` `mod tests` and `mod engine_tests` (exact duplicate) | Removed from `engine_tests`; merged the unique non-executable-file assertion into `find_executable_rejects_non_executable_files`. `run_*` engine tests retained (unique coverage). |

Files modified: `src/upload.rs`, `src/convert.rs`, `src/handlers/{layout,visual,filter,passwords,automation,misc,ocr,pipeline,convert}.rs`, `src/attachments.rs`.

## Functional duplication intentionally retained

- `load_pdf` in `analysis.rs` (returns `Document`) vs `security.rs` (returns `(name, size, Document)`) — different contracts; unifying would add a tuple most callers don't need.
- `transform.rs::float_param` returns `AppResult<Option<f64>>` (blank → `None`, not default) — different semantics from `param_parse`; kept local.
- `layout.rs::bool_param`/`bool_param`-style helpers vs `upload::param_bool` — the former returns `AppResult<bool>` with explicit default and blank→false Spring semantics; the latter is infallible last-wins. Contracts differ.
- `merge.rs::parse_bool` (`AppResult<bool>`, strict) vs `param_bool` (lenient) — Java's `Boolean.parseBoolean`-vs-binding distinction; retained.
- `misc::dispatch` vs `#[endpoint]` macro — same contract, different need (pre-dispatch param inspection for Java delegation). Documented in `misc.rs`.
- `handlers/merge.rs::pdf_response`/`file_response` vs per-handler inline `Response::builder()` calls (e.g. `filter::filter_miss`, `jobs::job_result`) — different shapes (204 empty, JSON metadata); not worth a shared abstraction.
- `pdftext/` internal near-duplicates (`line.rs`, `word_geometry.rs`, `shared.rs` edge/baseline helpers) — jpdfium port fidelity; merging risks subtle behavioural drift. Retained pending dedicated pdftext refactor.
- `tabula.rs` vs `pdftext/ruled_tables.rs`/`table_*` — two table pipelines with different algorithms (lattice vs stream); intentionally separate.

## Dependency duplication

- `rusqlite` now declared in both `stirling-server` (added during audit window for `jobstore`) and `stirling-engine` — appropriate: separate crates, no workspace inheritance for excluded `engine/`. Within the workspace, shared deps use per-crate declarations (no `[workspace.dependencies]` table exists — all three members pin their own versions; versions are consistent today but could drift; recommend a workspace deps table as P3).
- No duplicate-purpose crates found (single HTTP client `reqwest`, single PDF lib `lopdf`, single zip `zip`, single image `image`).
- `reqwest` features `blocking`, `stream`, `multipart`, `rustls-tls` — `blocking` appears unused in `src/` (verify before trimming; engine crate uses its own reqwest without `blocking`).
- `chrono-tz` + `iana-time-zone` both present — check whether both are needed (likely: chrono-tz for named zones, iana for local detection).
