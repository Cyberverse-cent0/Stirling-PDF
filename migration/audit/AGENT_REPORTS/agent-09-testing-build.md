# A09 — Testing, Build & Deployment (round 2, 2026-11-07)

Read-only audit; no builds/tests run — compile/test outcomes inferred from source, flagged where unverifiable.

## 1. Inventory

### Rust backend (`rust/`, workspace: `stirling-server` root bin+empty lib, `macros`, `cli`; `engine/` excluded own workspace)

- **`stirling-server` unit tests**: ~460 `#[test]`/`#[tokio::test]` across 55 `mod tests` in `rust/src/`. Every handler module has in-file tests except `handlers/mod.rs` (route table). `rust/src/pdftext/` tests in 7 of ~28 modules; markdown pipeline stages rely on indirect handler coverage (`rust/TESTING.md`).
- **Integration tests**: `rust/tests/` = `README.md`, `fixtures/.gitignore`, `tests/integration/.gitkeep` — **zero HTTP-level integration tests**; dir explicitly "reserved".
- **Fixtures**: generated at test time by `testkit::fixture_tests` into `tests/fixtures/` (gitignored).
- **Smoke tests**: `stirling test runtime|general|security` (`cli/src/commands/smoke.rs`) boots real server, POSTs fixtures — ~12 HTTP checks in `runtime`, 1–2 per domain. **Not wired into `task rust:check` or any CI job.**
- **`stirling` CLI**: 7 unit tests; `doctor`, `engines list/check/validate`, `test`, `start/stop/status`, `ports free`, `install`, `package`, `service`.
- **`rust/engine`**: lib+bin; 53 `#[test]` in `src/` + **78 tests across 14 files in `rust/engine/tests/`** (contracts, chunking, config, auth, llm_client, vector stores, agent_runtime, operation_shortlist, tool_io_compat, model_concurrency, http_classify, document_store, vec_store_bench). Parity script `rust/engine/scripts/parity_check.sh` via `task rust:engine:parity` (not in check/CI).
- **`rust/macros`**: proc-macro crate, no tests.

### Java (`app/{common,core,proprietary,saas}`)

- Large test tree: ~100+ files per module. Spot-checked imports resolve; tests for deleted migrated-endpoint controllers appear removed. **Compilation not verifiable without build.**
- `EndpointConfiguration.java` still lists migrated endpoints in "Java"/engine groups — stale grouping config.

### Python engine (`engine/`)

- ~45 test files (`engine/tests/`). `task engine:check` = pyright + ruff + format + pytest.
- **`tool_models.py` collapsed**: `OPERATIONS` now only **7 endpoints** (pdf/markdown, pdf/ua, accessibility-report, auto-redact, redact, redact-execute, timestamp-pdf) — exactly the endpoints whose Java controllers remain.

### Frontend

Vitest (`task frontend:test`→`test:editor`), Playwright stubbed/live/enterprise, Storybook a11y gates, oxlint/stylelint/theme-lint. `frontend-validation.yml`, `frontend-a11y.yml`, `e2e-stubbed.yml`, `e2e-live.yml`, `build-enterprise.yml` → `build.yml` → `all-checks-passed`.

### CI

- `build.yml` router + `dorny/paths-filter` (`.github/config/.files.yaml`); `all-checks-passed` aggregates ~18 jobs.
- `rust-server.yml` (reusable): `task rust:check` + `task rust:engine:check`, gated `rust/**`.
- `rust-lint.yml`: Tauri crates clippy, `tauri` filter (excludes `rust/engine` correctly).
- `rust-release.yml`: release binaries `stirling-server`+`stirling-engine` linux/win/macOS (x86_64+arm64). **Does not ship `stirling` CLI or `rust/macros`.**
- `tauri-build.yml`/`multiOSReleases.yml`: desktop bundles use `tauri.backend-rust.conf.json` (Rust sidecar); `DESKTOP_BACKEND=java` opt-in.
- `ai-engine.yml`: Python engine `task engine:check` + `runtime/docker/engine` images.
- `nightly.yml`: cucumber (`@nightly` + 10-shard full run) against **Java fat image**; a11y sweep; cache warmers.
- `e2e-live.yml`: Playwright live suite — **boots Spring Boot Java backend**.

### Taskfile wiring

`task check`/`check:all` → backend, frontend, engine, desktop, `rust:check`, `rust:engine:check`. `task test` → backend+frontend+engine+`rust:test`+`rust:engine:test`. `cargo clippy -D warnings` + `fmt --check` + `test` wired. No standalone `cargo check`/`build` in gate (clippy covers compilation).

## 2. Findings

### A09-F01 — Live E2E and Cucumber target Java backend that no longer serves migrated endpoints — P1
`.taskfiles/e2e.yml:50-163` (`live:backend` runs `./gradlew :stirling-pdf:bootRun`), `e2e-live.yml:3-5` ("boots Spring Boot"), `nightly.yml:154-176` (cucumber vs `runtime/docker/embedded` fat image), `frontend/src/editor/core/tests/live/*.spec.ts`, `testing/cucumber/features/*.feature`. Java controllers for migrated endpoints deleted (`app/core` retains only security controllers + `JobController`). Live specs directly exercise migrated endpoints: `pdf-protection-*.spec.ts` → `add-password`/`remove-password`/`restore-pdf-protection`; `pdf-permissions-matrix.spec.ts` → `rotate-pdf`/`split-pages`/`compress-pdf`; `e2e-pdf-operations.spec.ts` → merge etc. `PasswordController.java` now maps only `prepare-pdf-session`+`restore-pdf-protection` — `add-password`/`remove-password` have **no Java mapping**. Cucumber `general.feature`, `merge_overlay.feature`, `security_new.feature`, `convert_*.feature` same surface. `rust/MIGRATION.md:54-58` itself states Java 404s migrated endpoints. Impact: `playwright-e2e-live` (required `all-checks-passed` job) and nightly cucumber fail/404 — CI already red or coverage silently lost. No Rust-backed live E2E exists. Fix: repoint `e2e:live`/cucumber at `stirling-server` (:8081) or Rust+Java strangler; quarantine specs for endpoints Rust doesn't serve.

### A09-F02 — Generated tool-model surface collapsed to 7 Java-retained endpoints; Rust engine generated files diverge and are unchecked — P1
`engine/src/stirling/models/tool_models.py:250-302` (`OPERATIONS` = 7 endpoints only), `frontend/src/editor/core/types/toolIO.ts:135-187` (~17 entries, all Java-retained/proprietary), `toolApiTypes.ts` (0 matches for merge-pdfs/add-password), vs `rust/engine/src/models/tool_schemas.json` (still contains `/api/v1/general/merge-pdfs`, `/api/v1/security/add-password`, …) and `tool_params.rs`.
`task tool-models` regenerates from **Java** OpenAPI spec (`:stirling-pdf:copySwaggerDoc`). With migrated controllers deleted, spec lost ~100 endpoints → committed generated files shrank → `check-generated-models.yml` passes because committed files match the DEGRADED spec. Consumers: engine `OPERATIONS` (Python engine can't describe/plan migrated tools), frontend `toolIO`/`toolApiTypes` used by `ToolPicker.tsx`, `policyInput.ts`, `useToolOperation`, `toolAutomation` (pipeline/automation lose all migrated tools). `rust/engine`'s `tool_schemas.json`/`tool_params.rs` generated from pre-deletion spec and NOT covered by `check-generated-models.yml` (checks only `toolApiTypes.ts`, `toolIO.ts`, `tool_models.py`, `tool_io.py`). Impact: AI engine tool coverage silently collapsed ~95%; Rust engine holds stale-but-larger spec that collapses on next regen; two generated surfaces disagree about the API. Fix: decide spec source of truth post-migration (Rust route table or merged spec); regenerate `rust/engine` tool files; extend `check-generated-models` to cover `rust/engine/src/models/tool_{schemas,params}.*`; restore engine/frontend tool coverage or formally descope.

### A09-F03 — Release pipeline still publishes Java JARs missing all migrated endpoints — P1
`multiOSReleases.yml:120-197` (`build-jars` builds 3 variants → `Stirling-PDF*.jar`), `package-managers.yml:65-73` (publishes `Stirling-PDF-with-login.jar` SHA to Homebrew + Scoop), `rust-release.yml:86-114`. Released `Stirling-PDF.jar` = Java backend with migrated controllers deleted — partial backend where merge/rotate/split/password 404. `rust-release.yml` ships `stirling-server`/`stirling-engine` but NOT `stirling` CLI that `rust/DEPLOYMENT.md:58` instructs operators to run, nor sidecar engine bundling. Fix: gate/replace `build-jars`/package-managers JAR publishing until Java decommissioned, or ship combined artifact; add `stirling` CLI to release matrix.

### A09-F04 — No CI coverage for Rust route table / live smoke; `tests/integration` empty — P2
`rust/tests/integration/.gitkeep`, `handlers/mod.rs` (no route tests), `cli/src/commands/smoke.rs`, `.taskfiles/rust.yml:46-50` (`rust:check` = lint+test only), `rust-server.yml`. `stirling-server` is binary crate (`lib.rs` empty) → `tests/*.rs` can't import handlers; all coverage in-file unit tests on `*_work` fns. Axum route table exercised only by manual `stirling test runtime` (~12 checks) which nothing in `task check`/CI runs. Dropped/mis-typed route merges green. Fix: wire `cargo run -p stirling -- test runtime` (or general/security) into `rust:check`/CI — self-builds, ephemeral ports; or promote to lib+bin for in-process router tests.

### A09-F05 — Documentation drift across rust/*.md and runtime/README — P2
- `rust/MIGRATION.md:7-13` cites `tests/scripts/{test-unit,test-engine,smoke,coverage-report}.sh` — **deleted** (replaced by `stirling` CLI per `rust/tests/README.md`); "305 cargo test cases"/"116 routes" vs ~460+ today; Phase-4 engine section (`:453-458`) still lists agent runtime/routes/pytest port as "Remaining" though `rust/engine` implements them.
- `rust/TESTING.md`: engine test table lists 6 of 14 actual `rust/engine/tests/*.rs`; "Unit-tested in-file" includes `proxy` (deleted); references `JAVA_BACKEND_URL=off` (removed); "433 passed" predates current count.
- `rust/src/main.rs:20-23` comment claims `pdftext` "not yet routed to a handler" — stale: 31 `pdftext::` use sites.
- `runtime/README.md:8` claims "Nothing in the active build references these files" — false: `ai-engine.yml:106-110` builds `runtime/docker/engine/*`, `nightly.yml:155` uses `runtime/testing/compose/test_cicd.yml`, `build-enterprise.yml` uses `runtime/testing/compose/*`, `.files.yaml` gates `runtime/**`.
- `rust/DEPLOYMENT.md` otherwise accurate except CLI-not-shipped gap.

### A09-F06 — Stale endpoint-group config in Java `EndpointConfiguration` — P3
`app/common/.../EndpointConfiguration.java:398-539` still assigns migrated endpoints (add-password, merge-pdfs, rotate-pdf, compress-pdf…) to "Java"/"Security"/"Convert"/"Advance" groups. Java's own endpoints-status reporting claims endpoints available that now 404. Fix: prune groups to Java-retained or mark migration-frozen.

### A09-F07 — Test-coverage gaps — P3
- `rust/src/pdftext/` pipeline modules (`converter`, `heading`, `paragraph`, `page_rules`, `table_block/finder/grid/shape`, `ruled_*`, `page_stitcher`, `markdown_*`, `glyph_stitcher`, `line_merger`, `column_accumulator/layout`, `form_field(s)`, `pdf_form_reader`, `page_images`) have **no in-file tests** — only 7 of 28 modules; coverage indirect via handler tests.
- `rust/cli`: 7 tests for large command surface (doctor/engines/db/service/migration untested).
- `rust/engine/src/routes/*` thin bindings untested per-route (acknowledged TESTING.md); `rust/macros` untested; no line-coverage tooling.
- `#[cfg(unix)]`-gated tests in `convert.rs:364-410` → engine-spawn paths untested on Windows CI (rust-server.yml ubuntu only; tauri-build compiles not tests).

### A09-F08 — `stirling test` CLI gate diverges slightly from `task rust:lint` — P3
`cli/src/commands/test.rs:34-41` runs `cargo clippy --workspace --all-targets` (no `--all-features`); `.taskfiles/rust.yml:30` uses `--all-targets --all-features`. Harmless today (no features) but gates can drift.

### A09-F09 — `stirling` CLI absent from release artifacts — P2
`rust-release.yml` ships only `stirling-server`+`stirling-engine`; `rust/DEPLOYMENT.md:58-63` + `rust/tests/README.md` direct operators to `stirling doctor`/`engines list`/`test`. `rust/cli` has platform modules + install/package commands — intended for distribution. Fix: add `stirling` to release matrix (`cargo build --bin stirling`) or document build-from-source.

### A09-F10 — Java test suite compilation against deleted code — UNVERIFIED
Cannot run `./gradlew test` (read-only). Sampled test imports (`CertSignController`, `UploadLimitService`, `VeraPDFService`, `CertificateValidationService`, models) resolve. No test files reference deleted `controller/api/{general,misc,convert,filter,form,analysis,transform,layout,split,visual,automation}` packages. Tests enumerating endpoints (`ToolIODeclarationCoverageTest`, `ToolChainValidatorConformanceTest`, `EndpointConfigurationTest`) may encode pre-deletion endpoint set — **recommend running `task backend:check`**.

## 3. Verified consistent

- Workspace layout: `rust/Cargo.toml` members `[".","macros","cli"]`, `exclude=["engine"]`; engine own `[workspace]`.
- `task check`/`check:all`/`test`/`lint`/`build`/`fix`/`clean` all include rust + rust:engine (`Taskfile.yml:263-270,342-360`).
- CI gating: `rust-server` filter `rust/**` triggers both checks; `tauri` filter excludes `rust/engine`; `engine` filter Python-only.
- Desktop: `DESKTOP_BACKEND` defaults `rust`; `tauri.backend-rust.conf.json` exists; `desktop:rust-server`/`desktop:engines` staging implemented; multi-OS matrix incl. Windows ARM64, macOS universal, Linux.
- Cross-platform: `cfg(target_os)`/`cfg(unix|windows)` in `convert.rs`, `cli/src/platform/*` (non-linux/win/mac fallback), `engine/src/config_cache.rs` (see A04-F15 unix-import bug), `frontend/src-tauri`. Unix-only exec-bit tests main gap (F07).
- `rust/engine` parity harness + `stirling test coverage` inventory exist as documented.

## 4. Recommended priority order

1. **A09-F02** — spec source of truth; generated tool surface lost ~95% endpoints across Python engine + frontend toolIO. Highest functional impact.
2. **A09-F01** — live E2E/cucumber vs deleted Java endpoints; required CI job likely red or silently narrowed.
3. **A09-F03/F09** — release artifacts ship partial Java backend; CLI omitted.
4. **A09-F04** — wire smoke tests into CI before route-table regressions slip.
5. Docs sweep (F05/F06) + coverage gaps (F07/F08).
