# TESTING GAPS

Source: agent-09 (primary), cross-checked with all agents. Audit date 2026-11-07; tree was mid-edit — test counts are a snapshot.

## 1. Current coverage inventory

| Component | Tests | Notes |
|---|---|---|
| `stirling-server` (`rust/src`) | ~458 `#[test]`/`#[tokio::test]` in 55 `mod tests` | every handler module has in-file tests on `*_work` fns; **1 failure observed mid-edit** (`endpointconfig::tests::unrouted_endpoints_report_not_supported` — likely fixed by in-flight UNSUPPORTED trim, re-verify) |
| `rust/tests/` | **zero** | `integration/.gitkeep` only — dir "reserved"; binary-crate structure (`lib.rs` empty) prevents importing handlers in integration tests |
| `rust/engine` | 53 in-src + 78 across 14 `tests/*.rs` files | contracts, chunking, config, auth, llm_client, vector stores, agent_runtime, tool_io_compat, etc. |
| `rust/cli` | 7 | large command surface (doctor/engines/db/service/migration) untested |
| `rust/macros` | 0 | proc-macro crate |
| Java `app/` | ~100+ files/module | compiles against stripped tree UNVERIFIED — some endpoint-enumeration tests may encode pre-deletion set (`ToolIODeclarationCoverageTest`, `EndpointConfigurationTest`) |
| Python `engine/` | ~45 test files | `task engine:check` = pyright+ruff+pytest |
| Frontend | Vitest + Playwright (stubbed/live/enterprise) + a11y | live specs target Java backend |
| Smoke | `stirling test runtime|general|security` ~12+ HTTP checks | **boots real server, real fixtures — but never run in CI or `task rust:check`** |

## 2. Critical gaps

### P1

| ID | Gap | Impact | Fix |
|---|---|---|---|
| A09-F01 | **Live E2E + cucumber target deleted Java endpoints.** `e2e-live.yml` boots Spring Boot; `nightly.yml` cucumber vs fat image; specs exercise add-password/remove-password/restore-pdf-protection/rotate/split/merge — all 404 on stripped Java. `playwright-e2e-live` is a required `all-checks-passed` job → CI red or coverage silently lost | required check failing / coverage illusion | repoint `e2e:live`/cucumber at `stirling-server` :8081 (or Rust+Java strangler); quarantine Java-only specs |
| A09-F02 | **Generated tool-model surface collapsed to 7 endpoints** — spec regenerated from stripped Java OpenAPI; `check-generated-models.yml` passes against degraded spec; `rust/engine` generated files stale AND ungated | AI engine + frontend toolIO lost ~95% tool coverage silently | decide spec source of truth (Rust route table or merged spec); regen rust/engine files; extend freshness check |
| A09-F04 | **No route-table or live-smoke coverage in CI.** Route regressions (dropped/mis-typed routes) merge green; `stirling test` exists but unwired | silent routing breakage | wire `stirling test runtime|general|security` into `rust:check`/CI; or promote lib+bin for in-process router tests |
| A09-F03 | **Release ships partial Java JARs** missing migrated endpoints; `rust-release` omits `stirling` CLI that DEPLOYMENT.md instructs operators to run | users get broken artifacts | gate/replace JAR publishing; add CLI to release matrix |

### P2

| ID | Gap |
|---|---|
| A09-F07 | `rust/src/pdftext/` pipeline modules (~21 of 28) have zero direct tests — markdown pipeline covered only indirectly via handler tests |
| A09-F07 | Unix-only `#[cfg(unix)]` engine-spawn tests → engine paths untested on Windows CI (rust-server.yml is ubuntu-only; tauri-build compiles, doesn't test) |
| A09-F08 | `stirling test` clippy gate diverges from `task rust:lint` (`--all-features` missing) — gates can drift |
| A04/A06/A07 | **No tests for the audit's worst findings**: path-traversal filename, engine-call timeouts, zip-bomb budgets, job cancel/timeout/retention, revocation truthfulness, request-size limits — the security/reliability gaps are precisely the untested paths |
| A09-F10 | Java suite compilation against deleted code unverified — run `task backend:check` |
| — | No coverage tooling (no tarpaulin/llvm-cov wiring); `stirling test coverage` is inventory-only |
| — | `rust/engine` parity harness (`parity_check.sh` vs Python engine) exists but not in check/CI |

### P3

- `rust/cli`: 7 tests for doctor/engines/db/service/migration/install/package surface.
- `rust/macros`, `rust/engine/src/routes/*` thin bindings untested.
- Docs drift means documented test counts (305/433) don't match reality (~458).

## 3. Parity/regression testing status

- `testing/parity.sh` + `scripts/rust_pdf_parity.py` — **BROKEN by design now**: require live Java endpoints that were deleted (Java 404s). `rust/MIGRATION.md` admits this; agents confirm.
- `rust/engine/scripts/parity_check.sh` — Rust-engine vs Python-engine contract parity; functional but unwired.
- Byte-level output parity (lopdf rewrites vs PDFBox/JPDFium) — **UNVERIFIED everywhere**; no corpus-diff harness remains after parity tooling died.
- Fixture generation exists (`testkit::fixture_tests`) but only feeds unit tests.

## 4. Verification commands (current state)

- `cd rust && cargo check --workspace` — passed at audit start; **`--all-targets` FAILS on current tree** (signature.rs/apitests.rs in-flight errors).
- `cargo test` — ~458 tests; 1 failure observed mid-edit.
- `task rust:check` / `task rust:engine:check` / `task check` — wired in Taskfile; `rust-server.yml` CI runs lint+unit only.
- `./gradlew build` / `task backend:check` — Java still fully built/tested in CI; current compile status UNVERIFIED.
- `task engine:check`, `task frontend:test` — standard.

## 5. Recommended test work (priority order)

1. Stabilize tree → green `cargo check --all-targets` + full `cargo test`.
2. Security regression tests for every SECURITY_AUDIT_REPORT P0/P1 (traversal filename, engine timeouts, zip budgets, request limits, revocation honesty, job ID randomness/ownership).
3. Route-table test: enumerate `router()` routes vs expected list; wire `stirling test` smoke into CI.
4. Behavioral parity spot-checks for the BROKEN-for-UI items (crop/flatten/merge defaults) — these need fixture PDFs + golden outputs, not just unit tests.
5. Job lifecycle tests: timeout, cancel-abort, retention sweep, restart hydration, crash-mid-save.
6. Engine-absence matrix: run endpoint suite with each sidecar removed → assert 403 (not 500) where gated.
7. Reinstate a parity harness against a **merge-base Java build** (per MIGRATION.md note) for the ~10 dual-implemented endpoints before deleting Java copies.
8. Java `task backend:check` to verify stripped tree still compiles/tests.
