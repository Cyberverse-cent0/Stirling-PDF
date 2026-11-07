# Shell Elimination Plan

Phased replacement of shell scripts by the `stirling` Rust CLI
(`rust/cli`). Scripts are only deleted after their replacement is verified;
see `SHELL-SCRIPT-INVENTORY.md` for the per-script audit.

## Phase 1 — Done (this change)

| Shell script | Replacement | Status |
|---|---|---|
| `scripts/run.sh` (rust/engine/frontend/status/stop-all/wait-for) | `stirling start/stop/restart/status/logs/wait` | Implemented and Taskfile-rewired; java lifecycle stays on run.sh (gradlew is a shell script) |
| `scripts/find-free-port.sh` + `.ps1` | `stirling ports free` | Implemented; Taskfile port vars rewired (both OS branches collapse to one) |
| `rust/tests/scripts/test-all.sh` | `stirling check` + `stirling test` | Implemented (fmt+clippy+test; smoke scripts remain separate) |
| `rust/tests/scripts/test-unit.sh` | `stirling test unit` | Implemented |
| `rust/tests/scripts/test-engine.sh` | `stirling test engine` | Implemented |
| `rust/tests/scripts/test-general.sh` | `stirling test general` | Implemented natively (unit filters + detached server + remove-image-pdf check) |
| `rust/tests/scripts/test-security.sh` | `stirling test security` | Implemented natively (unit filters + sanitize-pdf + get-info-on-pdf) |
| `rust/tests/scripts/coverage-report.sh` | `stirling test coverage` | Implemented natively (per-file `#[test]`/`#[tokio::test]` counts) |
| `rust/tests/scripts/smoke.sh` | `stirling test runtime` | Implemented natively (reqwest HTTP + multipart, in-process server lifecycle); script retained as reference until deletion is approved |
| `scripts/stirling-diagnostics.sh` | `stirling diagnose` / `stirling doctor` | Implemented (Rust-backend scoped; the old script targeted the Java/proprietary deployment) |
| script census | `stirling migration audit` / `report` | Implemented |

## Phase 2 — In progress

1. ~~Taskfile rewiring~~ — done for port picking and rust/engine/frontend/
   status/stop/wait lifecycle. Remaining: `backend:run/stop` (Java) needs
   either a gradlew.bat-aware path or stays on run.sh until Java retires.
2. ~~Port the smoke scripts~~ — done. `smoke.sh`, `test-general.sh`,
   `test-security.sh`, `coverage-report.sh` are replaced by verified
   native `stirling test runtime|general|security|coverage` and have been
   deleted, along with `test-all.sh`, `test-unit.sh`, `test-engine.sh`
   (all replaced earlier). `rust/tests/scripts/` is now empty;
   `rust/tests/README.md` and `rust/TESTING.md` document the CLI commands.
3. ~~**`stirling-diagnostics.sh` parity**~~ — done for the Rust
   deployment: `stirling diagnose` now writes diagnostics table,
   engines JSON, system info (os-release/uname/tool+engine versions),
   redacted settings copies, and configs/.run trees. The shell script's
   proprietary-deployment extras (live metrics endpoints, /customFiles,
   /pipeline volumes, tar.gz packaging) have no Rust counterpart and
   retire with the Java deployment.
4. ~~**Engine install UX**~~ — done: `stirling engines install <name>`
   resolves dnf/apt/apk/pacman (os-release), brew and winget package
   specs, prints the command, prompts (or `--yes`), and re-detects
   afterwards. Engines without a known package print manual-install
   guidance.

## Phase 3 — Later

- `slim-onnxruntime.sh` → `stirling package --slim` (zip manipulation via
  the `zip` crate already in the workspace).
- `build-universal-mac-jre.sh` → `stirling package --target universal-macos`.
- Container entrypoints (`init*.sh`, `runtime/docker/*/entrypoint.sh`,
  `healthcheck.sh`) stay shell — they run inside minimal images before any
  Rust toolchain exists. Revisit only if a static `stirling` binary is
  shipped into those images.
- Cucumber/compose harnesses under `runtime/testing/` and `testing/` stay
  until the archived runtime is formally retired.
- Java-side scripts (`aot-diagnostics.sh`, `db-migration/*.sh`,
  `testing/*.sh` against bootRun) retire with the Java backend, not with
  this CLI.

## Rules carried forward

- No `sh -c` / `bash -c` / PowerShell strings anywhere in `rust/cli`.
- Old scripts stay until replacement verified; `stirling migration audit`
  tracks the shrinking count.
- Platform code only under `platform/`; everything else is portable.
