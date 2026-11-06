# Shell Script Inventory

Audit of every `*.sh` / shell-dependent workflow in the repository, as of 2026-11-06.
49 shell scripts discovered. Classification: A=runtime, B=dev/build, C=install/deploy,
D=testing, E=database, F=CI/packaging, G=container/service, H=obsolete.

## scripts/ (repo root)

| File | Class | Purpose | Key externals | Env/args | Verdict |
|---|---|---|---|---|---|
| init.sh | A | OCR entrypoint; copies mounted tessdata, delegates to init-without-ocr.sh | cp, ls, mkdir | none | Keep — container entrypoint, low value to port |
| init-without-ocr.sh | A | Main container init: env, paths, launches app | many (env, java) | DOCKER_* env | Keep — container entrypoint |
| run.sh | B | Dev-loop process manager: pid files + logs in `.run/` | bash, kill, curl | RUN_DIR | **Replace → `stirling start/stop/status/logs`** |
| find-free-port.sh | B | Print free TCP ports for preferred ports | bash /dev/tcp | port args | **Replace → used by Taskfile via `sh:`** |
| find-free-port.ps1 | B | Windows equivalent of above | PowerShell | port args | Keep until Taskfile drops shell |
| stirling-diagnostics.sh | B | Collects diagnostic bundle to /configs | many | CONFIG_FILE, args | **Replace → `stirling diagnose`/`report`** |
| aot-diagnostics.sh | B | JVM Leyden AOT cache diagnostics | java | --test, --cache | Java-side; obsolete as Rust is authoritative |
| db-migration/run-migration-test.sh | E | Boots JAR against H2 fixtures, tests login | java, curl | STIRLING_JAR | Java-side; re-scope for Rust or retire |
| db-migration/run-postgres-migration-test.py | E | Postgres migration test (already Python) | psql | — | Keep (not shell) |
| download-security-jar.sh | C | Downloads proprietary JAR from files.stirlingpdf.com | curl | VERSION_TAG, DISABLE_ADDITIONAL_FEATURES | Java/proprietary flow; keep until packaging replaced |
| installFonts.sh | C | apk add font packages (Alpine container) | apk | $1 langs | Container-only; keep |
| slim-onnxruntime.sh | F | Strip multi-arch ONNX natives from jar | zip, dpkg/uname | target, arch | Packaging; candidate for `stirling package` later |
| remove_translation_keys.sh | B | Remove i18n keys from messages_*.properties | sed loop | key arg | Dev tool; low priority |
| replace_translation_line.sh | B | One-off sed replace in properties | sed | hardcoded key | Obsolete one-off |
| PropSync.java, RestartHelper.java | — | Java helper classes (not scripts) | — | — | Not shell |

## rust/tests/scripts/

| File | Class | Purpose | Verdict |
|---|---|---|---|
| test-all.sh | D | fmt + clippy + test + smoke gates | **Replace → `stirling check` / `stirling test`** |
| test-unit.sh | D | cargo test unit suite | **Replace → `stirling test unit`** |
| test-general.sh | D | GeneralApi smoke vs fixtures | **Replace → `stirling test smoke`/fixtures** |
| test-security.sh | D | SecurityApi smoke | same |
| test-engine.sh | D | stirling-engine test suite | **Replace → `stirling test engine`** |
| smoke.sh | D | Multi-endpoint smoke, boots server | **Replace → `stirling test runtime`** |
| coverage-report.sh | D | Per-file test disposition table | Candidate `stirling report coverage` |

## rust/engine/scripts/

| File | Class | Purpose | Verdict |
|---|---|---|---|
| generate_tool_params.sh | B | Regenerate tool_params.rs from OpenAPI | Keep (dev codegen; Java spec source) |
| parity_check.sh | D | Python↔Rust engine response parity | Keep until Python engine retired |

## testing/ (cucumber/e2e)

| File | Class | Purpose | Verdict |
|---|---|---|---|
| parity.sh | D | Java↔Rust endpoint parity (jq/pdfdiff) | Keep while Java backend exists for parity |
| smoke_resource_endpoints.sh | D | Resource-endpoint smoke vs bootRun | Java-side; retire with Java |
| test_disabledEndpoints.sh | D | Disabled-endpoint checks | Java-side |
| test_webpages.sh | D | Webpage reachability checks | Low value; candidate port |
| cucumber/run-parallel.sh | D | Behave shards runner | Keep (test infra) |

## runtime/ (archived container runtime)

| File | Class | Purpose | Verdict |
|---|---|---|---|
| docker/frontend/entrypoint.sh | G | nginx envsubst for backend URL | Keep (container) |
| docker/unoserver/entrypoint.sh | G | LibreOffice UNO server launcher | Keep (container) |
| docker/unoserver/healthcheck.sh | G | RPC ping healthcheck | Keep (container) |
| testing/compose/*.sh (14 files) | G/D | Multinode/OAuth/SAML/MCP compose harnesses | Keep — archived runtime, container-coupled |
| testing/test.sh, test2.sh | D | Legacy runtime test entrypoints | Likely obsolete — verify |

## .devcontainer/

| File | Class | Purpose | Verdict |
|---|---|---|---|
| git-init.sh | B | Seed git identity in devcontainer | Keep (devcontainer-only) |
| init-setup.sh | B | Devcontainer post-start banner/setup | Keep |

## frontend/scripts/

| File | Class | Purpose | Verdict |
|---|---|---|---|
| build-universal-mac-jre.sh | F | jlink×2 + lipo universal JRE | Packaging; candidate for `stirling package` |
| dev-update-test/*.sh (4 files) | F | Tauri updater dev signing/e2e | Keep — Tauri-specific, invokes tauri tooling |

## Shell embedded elsewhere

- `Taskfile.yml`: ~10 `sh:` task bodies call `scripts/find-free-port.sh` and `scripts/run.sh` (lifecycle tasks). These are the highest-value replacement targets — `stirling` covers both.
- `rust/src/convert.sh`? No — `rust/src/convert.rs` contains one `sh -c`-style string? Verified: only `convert.rs` matched the grep for shell invocation patterns; needs review but is process spawning for external tools (gs/qpdf), legitimate.

## Summary

- Total scripts: **49** (+1 PowerShell)
- Actively referenced by Taskfile/CI/dev workflow: ~12 (`run.sh`, `find-free-port.sh`, `init*.sh`, `test-*.sh`, `smoke.sh`, `parity*.sh`, compose scripts, frontend packaging)
- Obsolete/one-off: `replace_translation_line.sh`, `aot-diagnostics.sh` (Java), `test2.sh` (verify)
- First-wave Rust replacements: `run.sh` → runtime commands; `find-free-port.sh` → internal port picker; `test-all.sh`/`test-*.sh`/`smoke.sh` → `stirling test/check`; `stirling-diagnostics.sh` → `stirling diagnose/report`.
