# A10 — Repository Cleanup & Technical Debt (round 2, 2026-11-07)

Supersedes `agent-10-cleanup.md` (earlier same-day draft).

**Method note:** read-only file tools — no shell. Git-tracking claims corroborated by `.gitignore` inspection + prior verified audit (`agent-10-cleanup.md` ran `git ls-files`: zero tracked build artifacts). **[VERIFY-GIT]** = parent should re-run `git ls-files` spot-checks.

## 1. Cleanup inventory

| # | Item | Type | Evidence | Recommended action | Order |
|---|---|---|---|---|---|
| 1 | `engine/` Python AI engine vs `rust/engine/` | Duplicate implementation (~90 files each) | `rust/engine/Cargo.toml:6` "replaces the engine/ Python service"; `rust/engine/README.md:5-7` cut-over done, Python = "parity oracle"; `engine/pyproject.toml`, `.taskfiles/engine.yml`, `Taskfile.yml:21-23,268,280,304,313,330,347` | **Do not delete yet** — see F01/F03: rust engine codegen still reads `engine/src/stirling/models/*.py`. Execute `MIGRATION_BOARD.md:38` decision first | Last (blocked) |
| 2 | `rust/engine` generated models have **no CI freshness check** | Generated-code drift | `pipeline_table.rs:4`, `tool_io.rs:1`, `tool_params.rs:1`, `tool_schemas.rs:2`, `tool_texts.rs:1` all "do not edit"; `check-generated-models.yml` + `tool-models:check` cover only `toolApiTypes.ts`, `toolIO.ts`, `tool_models.py`, `tool_io.py` | Add regen+`git diff --exit-code` for the 5 Rust generated files (or fold into `rust:engine:check`) | Early — cheap, prevents silent drift |
| 3 | `runtime/` "archived" in AGENTS.md but still load-bearing | Conflicting structure / stale doc | Live refs: `ai-engine.yml:106,110` (engine Dockerfiles), `nightly.yml:155,180,184` (`test_cicd.yml`), `build-enterprise.yml:151-410` (keycloak/saml/mcp compose), `.taskfiles/e2e.yml:211-284` | Either un-archive (update AGENTS.md + `runtime/README.md`) or migrate e2e to native. Not deletable today | Mid |
| 4 | `testing/parity.sh`, `scripts/rust_pdf_parity.py`, `rust/examples/{pdfdiff,pdf_fingerprint}` | Dead parity tooling | `testing/parity.sh:11` "Java↔Rust endpoint parity"; `MIGRATION_BOARD.md:3-5` "Java parity … not requirements"; scripts referenced only by each other + reports | Delete all four together (`qr_png.rs` only in reports — evaluate separately) | Early |
| 5 | `testing/` orphans (~10 files) | Dead fixtures | Prior audit A10-04: `endpoints.txt`, `failure-kind-codes.json`, `webpage_urls*.txt`, `allEndpointsRemovedSettings.yml`, `testdriver/test.yml`, `smoke_resource_endpoints.sh`, `test_disabledEndpoints.sh`, `test_webpages.sh` — zero refs outside reports **[VERIFY-GIT]** | Re-grep then delete; keep `cucumber/` (`.taskfiles/cucumber.yml`), audit `ledger/`+`test_pdf_*.pdf` first | After #4 |
| 6 | `scripts/` orphans (~20 files) | Dead helpers | Prior audit A10-05 list: `init*.sh`, `PropSync.java`, `RestartHelper.java`, `find-free-port.ps1`, `aot-diagnostics.sh`, `download-security-jar.sh`, `slim-onnxruntime.sh`, `stirling-diagnostics.sh`, `subset_cjk_fallback_fonts.py`, `sync_en_us_spelling.py`, `remove/replace_translation_*.sh`, `scripts/translations/*.py` (11) — no task/CI refs **[VERIFY-GIT]** | Triage: delete confirmed-dead; keep+document `export-ffdetr-onnx.py` (proprietary NOTICE), `db-migration/` (CI), `lint/` (pre-commit), `translations/` if manual | Mid |
| 7 | `devTools/` stylelint package | Dead package | `devTools/package.json` lints Java-tree CSS; only refs `dependabot.yml:62`, `labeler-config-srvaroa.yml:155`; frontend has own `stylelint.config.mjs` + `frontend.yml:389-396` | Delete dir + dependabot/labeler entries | Early (independent) |
| 8 | `migration/reports/` — 48 files, stale/self-contradictory | Stale docs | `FINAL_COMPLETION_AUDIT.md` claims "cargo check fails, 23 errors" vs `REMAINING_WORK.md` "clean"; `MIGRATION_BOARD.md` authoritative | Archive phase reports under `migration/reports/archive/`; keep BOARD + REMAINING_WORK + current-state docs | Early (docs-only) |
| 9 | `app/core/src/main/resources/static/python/split_photos.py` | Shipped-but-dead script | Rust port `rust/src/image_split.rs` + `handlers/image_scans.rs` live; grep `app/` shows **no Java caller** — only `GeneralUtils.java:49,1162` allowlist/error text, `EndpointConfiguration.java:444-491` group entries, `popularity.txt`, labeler config | Delete script; remove allowlist entry + group mappings + labeler refs once Java `extract-image-scans` config confirmed unreachable | Mid (with Java cleanup) |
| 10 | `png_to_webp.py`, `calibre_sandbox.py` | **Live** — do NOT delete | Java tests `ExtractScriptConcurrencyTest.java:45-94`, `CalibreSandboxScriptTest.java`, `CalibreSandboxScriptOffLinuxTest.java` still exercise them | Keep until Java/ebook endpoints retire | — |
| 11 | `rust/Cargo.toml` unused crypto deps | Dep bloat | Prior audit A10-08 + A04-F12: `pkcs12`, `pkcs8`, `cbc`, `des`, `rc2`, `hmac`, `const-oid`, `http-body-util` → 0 hits in compiled `rust/src/` **[VERIFY]** | `cargo machete`/`udeps`; remove or annotate as reserved for cert-sign work | Early |
| 12 | `rust/src/bin/` empty dir; `.gitignore:324` stale `rust/pdfium-native/target/`; missing `logs/`,`storage/` ignores | Cosmetic/hygiene | Dir empty & untracked; `rust/pdfium-native/` doesn't exist; root `logs/`(only `*.log` covered), `storage/` unignored **[VERIFY-GIT]** | `rmdir`; prune gitignore | Early |
| 13 | `SwaggerDoc.json` | Generated artifact, correctly ignored | `.gitignore:49`; absent on disk; consumed by `frontend.yml:600`, `engine.yml:136,148`, `generate_tool_params.sh` | No action; single-source-of-truth risk: spec comes from **Java** — must be preserved/repointed at Java retirement (see A05-F05, A09-F02) | Track |
| 14 | `pipeline/`, `customFiles/`, `configs/`, `.run/`, `rust/configs/` | Runtime dirs, all gitignored | Present on disk, correctly ignored; `pipeline/*.json` local-only **[VERIFY-GIT]** | No action — intentional runtime state | — |
| 15 | `node_modules`, `target/`, `build/`, `engine/.venv`, `dist/` | Build artifacts | All covered by `.gitignore`; prior audit confirmed 0 tracked **[VERIFY-GIT]** | No action | — |
| 16 | `buildSrc/` | Live Gradle build logic | `buildSrc/build.gradle` + license-filter Groovy used by root `build.gradle`; required while Java builds | Keep until Java retirement (Phase G) | Last |
| 17 | Cargo workspaces ×4 | Structure | `rust/` workspace (`.`,`macros`,`cli`; `exclude=["engine"]`), `rust/engine/` standalone, `frontend/src-tauri/` + 4 nested crates | Intentional; `rust/cli` (`stirling`) vs `stirling-server` naming overlap is dev-ops vs server — not duplication | Informational |
| 18 | Endpoint metadata tables ×5 generated artifacts | Conflicting structures | Chain: Java `@ToolIO` → `toolIO.ts`+`toolApiTypes.ts` (frontend) → `pipeline_table.rs` (server, via `generate_pipeline_table.mjs`); → `tool_io.py` → `tool_io.rs`/`tool_schemas.*`/`tool_texts.rs`/`tool_models.rs`/`tool_params.rs` (engine). `endpointconfig.rs` different concern (enablement) — not a dup | Consolidate freshness checks (#2); long-term single generator from SwaggerDoc | Mid |
| 19 | Frontend dead endpoint refs | — | Only `src-tauri/.../backend.rs:573,645` sets `JAVA_BACKEND_URL=off` (intentional); no calls to removed endpoints found | None — clean | — |
| 20 | `docs/` | Mostly current | `DeveloperGuide.md`, `BUILD_SYSTEM.md` describe Gradle/Java dev (accurate — Java still builds); `STYLELINT.md` points at live `frontend:lint:css` | Update only where contradicting Rust-first reality; low priority | Late |

## 2. Findings

### A10-F01 — Dual AI engines: `engine/` (Python) vs `rust/engine/` — DIVERGENT · P2
Two complete implementations of same HTTP contract/env/port 5001; every contract change made twice. `rust/engine/Cargo.toml:6`; `rust/engine/README.md:5-7`; `Taskfile.yml:230` (`dev:_all` Rust), `:424` (`engine:bg` Rust), `.taskfiles/engine.yml` (Python fully wired), `ai-engine.yml` (Python CI + Docker). Doubled maintenance + contract drift (partially mitigated by `rust/engine/tests/{contracts,tool_io_compat}.rs`). Fix: execute board decision. **Blocker:** `rust/engine/scripts/gen_tool_io.py:14`, `gen_tool_schemas.py:23`, `gen_tool_texts.py:22` read `engine/src/stirling/models/tool_{io,models}.py` — retiring `engine/` breaks Rust-engine codegen unless generators repointed to `toolIO.ts`/`toolApiTypes.ts` or SwaggerDoc first. Cucumber suite uses Python uv env.

### A10-F02 — Python engine retirement blocked by 4 dependency chains — PARTIAL · P2
`.taskfiles/engine.yml`, `.taskfiles/cucumber.yml:10-28`, `ai-engine.yml`, `multiOSReleases.yml:775-777,927-928`, `nightly.yml:146`, `build-enterprise.yml:373`, `sync_files_v2.yml:60`, `pre_commit.yml:35`, `frontend-validation.yml:117`, `e2e-live.yml:114`, `coverage-aggregate.yml:61`, `check_toml.yml:106`, `check-generated-models.yml:44`, `backend-build.yml:206`. ~14 workflows cache-key on `engine/pyproject.toml`+`uv.lock`; `check-generated-models` regenerates `tool_models.py`/`tool_io.py` feeding Rust engine's committed `.rs` files. Fix sequence: repoint rust-engine generators → migrate cucumber off Python env → remove `engine/`, `ai-engine.yml`, engine taskfile, uv groups, `runtime/docker/engine/` in one PR.

### A10-F03 — Rust generated files have no freshness gate — BROKEN (process) · P1
`rust/src/pipeline_table.rs`, `rust/engine/src/models/{tool_io.rs,tool_params.rs,tool_schemas.rs,tool_schemas.json,tool_texts.rs}`; generators `rust/scripts/generate_pipeline_table.mjs`, `rust/engine/scripts/{gen_tool_io.py,gen_tool_schemas.py,gen_tool_texts.py}`, `rust/engine/examples/generate_tool_params.rs`+`.sh`. `tool-models:check` + `check-generated-models.yml` verify only TS+Python artifacts. Five committed Rust artifacts can silently drift from `toolIO.ts`/`tool_models.py`/SwaggerDoc — nothing in `rust:check`/`rust:engine:check` regenerates them either. Headers demand manual two-step regen (`pipeline_table.rs:4`) with no task wrapping it. Impact: stale endpoint arity/extensions in pipeline engine + stale tool schemas for AI engine — wrong runtime behaviour, no CI signal. Fix: `rust:tool-models:check` running all generators + `git diff --exit-code`; wire into `check-generated-models.yml`.

### A10-F04 — `runtime/` "archived" but still live in CI/e2e — DIVERGENT · P2
AGENTS.md ("archived… does not need Docker") vs `ai-engine.yml:106,110`, `nightly.yml:11,155,180,184`, `build-enterprise.yml:5,151-410`, `.taskfiles/e2e.yml:211-284`. `runtime/docker/engine/*`, `runtime/docker/embedded/compose/test_cicd.yml`, `runtime/testing/compose/*` actively used by scheduled/enterprise CI + `task e2e:*`. `runtime/workflows/` genuinely archived (superseded by `.github/workflows/`). Fix: split — keep `runtime/testing/compose` + `runtime/docker/engine` live (update AGENTS.md), archive/delete `runtime/docker/{backend,base,frontend,unoserver,embedded}`, `runtime/workflows/`, `runtime/taskfiles/` if unused **[VERIFY]**.

### A10-F05 — Java↔Rust parity tooling obsolete — DEPRECATED · P2
`testing/parity.sh`, `scripts/rust_pdf_parity.py`, `rust/examples/pdfdiff.rs`, `rust/examples/pdf_fingerprint.rs` all require live Java backend serving deleted endpoints (`MIGRATION_BOARD.md:3-5`). `pdfdiff`/`pdf_fingerprint` exist solely for these scripts. Fix: delete all four; `qr_png.rs` evaluate separately.

### A10-F06 — Orphaned `testing/` fixtures — DEPRECATED · P3
`testing/{endpoints.txt,failure-kind-codes.json,webpage_urls.txt,webpage_urls_full.txt,allEndpointsRemovedSettings.yml,testdriver/,smoke_resource_endpoints.sh,test_disabledEndpoints.sh,test_webpages.sh}`; `crop_test.pdf`, `test_pdf_*.pdf`, `ledger/` tied to dead parity scripts. Prior audit basename-grep = 0 refs in Taskfile/.taskfiles/workflows/pre-commit **[VERIFY-GIT]**. `testing/cucumber/` IS live. Fix: delete orphans after F05; audit `ledger/` first.

### A10-F07 — Orphaned `scripts/` helpers (~20) — DEPRECATED/UNVERIFIED · P3
`scripts/{aot-diagnostics.sh,download-security-jar.sh,init.sh,init-without-ocr.sh,installFonts.sh,PropSync.java,RestartHelper.java,remove_translation_keys.sh,replace_translation_line.sh,rust_pdf_parity.py,slim-onnxruntime.sh,stirling-diagnostics.sh,subset_cjk_fallback_fonts.py,sync_en_us_spelling.py,find-free-port.ps1}` + `scripts/translations/*.py` (11). Prior audit: zero refs **[VERIFY-GIT]**. Live exceptions: `scripts/lint/*` (pre-commit), `scripts/db-migration/*` (CI + proprietary README), `scripts/export-ffdetr-onnx.py` (proprietary NOTICE + model-catalog.json), `scripts/run.sh` (`Taskfile.yml:410,415`), `scripts/find-free-port.sh` (`Taskfile.yml:12` via CLI), `scripts/pre-commit/*`. Fix: triage per prior audit; PropSync/RestartHelper prime deletion candidates.

### A10-F08 — `devTools/` dead stylelint package — DEPRECATED · P3
`devTools/{package.json,package-lock.json,.stylelintrc.json}`; refs `dependabot.yml:62`, `labeler-config-srvaroa.yml:155`, `runtime/dockerignore:78`. Lints `app/**` CSS only; frontend uses own `stylelint.config.mjs`. Fix: delete dir + 3 config refs. Verify proprietary static CSS out of frontend lint scope first.

### A10-F09 — `migration/reports/` stale archive — DEPRECATED · P2
48 files; authoritative: `MIGRATION_BOARD.md`, `REMAINING_WORK.md`. `FINAL_COMPLETION_AUDIT.md`/`PHASE-3-*` describe superseded states ("cargo check fails", "Java parity required", ~78–104 routes); mutual "supersedes" banners. Fix: move historical reports to `migration/reports/archive/`; keep board + remaining-work + verified-current docs.

### A10-F10 — `split_photos.py` shipped but unreferenced — DEPRECATED · P2
`app/core/src/main/resources/static/python/split_photos.py`; refs `GeneralUtils.java:49,1162`, `EndpointConfiguration.java:444,471,485,491`, `popularity.txt:49`, `labeler-config-srvaroa.yml:101,121`, `.taskfiles/pre-commit.yml:13`. Native Rust port live (`image_split.rs`+`image_scans.rs`); grep `app/` finds **no Java invocation** — only config/test/labeler mentions. Contrast `png_to_webp.py`/`calibre_sandbox.py` have live Java tests — do not touch. Fix: remove script + allowlist entry + group mappings once Java `extract-image-scans` reachability confirmed dead.

### A10-F11 — Suspected-unused crypto deps in `rust/Cargo.toml` — UNVERIFIED · P3
`rust/Cargo.toml:47-64` (`pkcs12`,`pkcs8`,`cbc`,`des`,`rc2`,`hmac`,`const-oid`,`http-body-util`) — 0 hits in compiled `rust/src/` (A04-F12: orphan `certsign.rs` imports pkcs8 but isn't compiled). Possibly staged for cert-sign work (MIGRATION_BOARD #5). Fix: `cargo machete`/`udeps`; remove confirmed-unused or annotate reserved.

### A10-F12 — `.gitignore` gaps & stale entries — PARTIAL · P3
`.gitignore:324` ignores `rust/pdfium-native/target/` (doesn't exist); root `logs/` covered only by `*.log`, `storage/` not ignored **[VERIFY-GIT]**. Fix: add `logs/`,`storage/`; drop pdfium-native entry.

### A10-F13 — `SwaggerDoc.json` linchpin generated artifact — PARTIAL · P3
`.gitignore:49` correctly untracked, currently absent; produced by `:backend:swagger` (Java), consumed by `frontend.yml:600`, `engine.yml:136,148`, `generate_tool_params.sh`, `generate_pipeline_table.mjs` (indirectly). Single spec source is Java — at Java retirement spec must regenerate from Rust routes or freeze, else all five downstream generators lose input. Fix: record as Java-retirement prerequisite alongside A05-F05/A09-F02.

### A10-F14 — `rust/src/bin/` stray empty dir — BROKEN (cosmetic) · P3
Empty on disk, untracked, no `[[bin]]` in `rust/Cargo.toml`. Fix: `rmdir`.

### A10-F15 — `rust/cli` (`stirling`) vs `stirling-server` — COMPLETE (informational)
`cli/Cargo.toml` = dev-ops CLI; `src/main.rs` = server. `Taskfile.yml:11` uses CLI for port/process mgmt. No functional duplication; `cli/src/services/engine_detector.rs` overlaps conceptually with server probing but different layers.

### A10-F16 — Multiple text-extraction implementations — COMPLETE (by design)
`rust/src/pdftext/` (34 files) = port of Java PDFTextStripper → markdown/HTML; `rust/src/text.rs` = positioned-run extractor for auto-rename/tabula; `rust/engine/src/agents/page_text.rs` = engine-side page text; `engine/src/stirling/agents/_page_text.py` = Python original. Distinct consumers — only true duplication is engine pair (F01).

### A10-F17 — Docs staleness — PARTIAL · P3
AGENTS.md "archived" claim contradicted by live `runtime/` usage (F04); `docs/developer/*` Java-centric but accurate while Java builds; `rust/MIGRATION.md`, `TESTING.md`, `DEPLOYMENT.md` present but contain stale claims (A09-F05); `migration/reports/*` covered by F09. Fix: update AGENTS.md runtime section; refresh DeveloperGuide when cut-over completes.

## 3. Recommended safe ordering

1. **Stabilize:** commit in-flight Rust work; `task rust:check` + `task rust:engine:check` green. *(Prior audit flagged uncommitted `timestamp.rs`/`pdfua.rs`/`signature.rs` — re-verify.)*
2. **F03 freshness gate** for 5 Rust generated files — prevents worst silent-drift risk.
3. **Docs/hygiene (zero risk):** F09 archive reports, F12 gitignore, F14 rmdir, F08 devTools removal.
4. **F05+F06+F07 dead tooling** deletion (parity scripts → testing orphans → scripts triage).
5. **F11** cargo dep prune.
6. **F10** `split_photos.py` removal (verify Java endpoint dead first).
7. **F04** resolve runtime/ archive-vs-live contradiction.
8. **F01/F02/F13** engine unification — blocked on codegen repointing + cucumber migration + board decision.
9. **Java retirement** (buildSrc, app/, gradle CI) — last, per MIGRATION_BOARD Phase G.

## 4. Could not perform

- `git ls-files`/`git status`/`git check-ignore` — no shell; tracking claims rely on `.gitignore` + prior agent-10 audit's verified results. Re-verify before deletion.
- `cargo machete`/`udeps`, `task` runs — need build environment.
- Confirm `rust/src/bin/` still exists on disk.
