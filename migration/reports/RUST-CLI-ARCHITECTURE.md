# Rust CLI Architecture

`rust/cli` is a new workspace member (`stirling` binary) that replaces the
repo's dev/ops shell scripts with a single cross-platform executable.

## Layout

```
rust/cli/src/
├── main.rs               # tracing init, repo-root resolution, dispatch
├── cli.rs                # clap command tree
├── commands/
│   ├── dev.rs            # build/check/lint/fmt/clean/watch  (cargo wrappers)
│   ├── test.rs           # test unit|integration|engine|fixtures|runtime
│   ├── runtime.rs        # start/stop/restart/status/logs/health
│   ├── diagnostics.rs    # doctor/diagnose/report
│   ├── engines.rs        # engines list/check/versions/validate/report
│   ├── database.rs       # db status (others: unsupported until Rust persistence)
│   ├── config.rs         # config init/validate/show (rust/configs/*.yml)
│   ├── install.rs        # install/uninstall/package/service
│   └── migration.rs      # migration audit/report (living script inventory)
├── platform/             # per-OS dirs, pid_alive, terminate
│   ├── linux.rs          # XDG dirs, systemd user units
│   ├── windows.rs        # APPDATA/LOCALAPPDATA, taskkill/tasklist
│   └── macos.rs          # ~/Library dirs, launchd plists
├── services/
│   ├── process.rs        # safe Command wrappers: run, run_with_timeout,
│   │                     # spawn_detached, PATH executable discovery
│   ├── engine_detector.rs# external tool probing + --version capture
│   ├── filesystem.rs     # repo_root, .run/ pid+log bookkeeping
│   ├── database.rs       # DB status seam
│   └── diagnostics.rs    # doctor report collection
└── output/               # human tables + --json emission
```

## Workspace

`rust/Cargo.toml` is now a workspace: members `.`, `macros`, `cli`;
`engine` is excluded (it keeps its own `Cargo.lock`/`target` as a separate
workspace — do not merge it without coordinating).

## Design rules enforced

- **No shell strings.** Every external invocation is
  `std::process::Command` + argv array. `stirling test runtime` is fully
  native: it builds `stirling-server` via cargo argv, spawns the binary
  detached with `JAVA_BACKEND_URL=off`, and runs all 12 smoke assertions
  over HTTP using `reqwest` (blocking + multipart) — no curl, no bash.
  `STIRLING_SERVER_BIN` overrides the binary path to skip the build.
- **Detached processes.** `spawn_detached` calls `setsid` on unix and
  `DETACHED_PROCESS|CREATE_NEW_PROCESS_GROUP|CREATE_NO_WINDOW` on Windows;
  stdout/stderr append to `.run/logs/<name>.log`.
- **Platform isolation.** OS differences live only in `platform/`;
  `cfg!` appears there and in `services/process.rs` spawn flags.
- **Honest capability.** `db migrate|backup|...` and Windows service
  management return explicit "not applicable/implemented" errors rather
  than pretending. `stirling doctor` reports missing engines; nothing is
  auto-installed.
- **`.run/` compatibility.** The pid/log layout matches `scripts/run.sh`,
  so `stirling status` sees components started by the old script (verified:
  it correctly reported a stale `java` pid written by run.sh).

## Dependencies added

`clap` (derive), `anyhow`, `serde`, `serde_json`, `serde_yml`, `tracing`,
`tracing-subscriber`, `libc` (unix-only, for `setsid`), `reqwest`
(blocking + multipart, for the native smoke harness). No `config`,
`indicatif`, `ratatui`, `walkdir` yet — added only when a command needs
them.

## Phase 2 additions

- `stirling ports free <ports...>` — internal port discovery replacing
  `scripts/find-free-port.sh` (and making the `.ps1` redundant): preferred
  port if bindable, else a distinct port in 20000-49999.
- `stirling wait <seconds> <url>` — replaces `run.sh wait-for`.
- `stirling stop`/`restart` with no names act on all managed components
  (replaces `run.sh stop-all`).
- `stirling start` gained `--cwd` and repeatable `--env KEY=VALUE`; the
  restart spec is stored as JSON in `.run/<name>.cmd`.
- `find_executable` on Windows now tries `.exe/.cmd/.bat/.com` so `npx`,
  `gradlew`-style shims resolve.
- `default-run = "stirling-server"` keeps bare `cargo run` working now
  that `rust/` is a workspace with two binaries.
- Taskfile rewired: port picking and the rust/engine/frontend/status/stop/
  wait lifecycle tasks call `stirling` via `cargo run -p stirling` (works
  before the binary is installed). The Java backend lifecycle stays on
  `scripts/run.sh` because `./gradlew` is a shell script.

- `test` subcommands now mirror every legacy script natively: `unit
  [FILTER]` (test-unit.sh), `engine` (--all-targets parity), `general`,
  `security`, `coverage`, `runtime`, `fixtures`, `conversions`.
  `runtime`/`general`/`security` share `commands/smoke.rs`'s harness —
  cargo argv for unit filters and the server build, detached spawn with
  `JAVA_BACKEND_URL=off`, reqwest HTTP/multipart assertions — no shell
  scripts remain in the test path. `coverage` counts `#[test]`/
  `#[tokio::test]` attributes per file under `src/` and `engine/src/`.
- `task rust:cli CLI_ARGS="..."` runs the CLI through cargo without
  installing it.

## Not yet implemented / next phase

- `stirling watch` requires `cargo-watch`.
- `db` subcommands beyond `status` await Rust persistence.
- `package --archive` shells out to `zip` when present, else leaves a
  staged bundle in `dist/`.
- HTTP client in `runtime::health`/`wait` is a minimal HTTP/1.1 GET (no
  TLS); fine for localhost health endpoints. The smoke harness uses
  `reqwest` and therefore supports TLS if ever pointed off-localhost.
- Java lifecycle (`backend:run/stop`) still needs `run.sh` or a
  `gradlew.bat`-aware path.
