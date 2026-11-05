# Unified Build System Plan

Goal: one entry point (`task`) that **builds, tests, lints, runs, restarts, and stops** every component — frontend, Java backend, Rust server, Python engine, desktop — into standard, predictable output directories, with per‑component and per‑module targeting.

---

## Current state

| Component | Build tool | Output today | Check command |
|---|---|---|---|
| Frontend | Vite | `frontend/dist` | `task frontend:check` |
| Java backend | Gradle | `app/*/build/` | `task backend:check` |
| Rust server | Cargo | `rust/target/` | `task rust:check` |
| Python engine | uv | `engine/.venv` | `task engine:check` |
| Desktop (Tauri) | cargo + npx tauri | `frontend/src-tauri/target/` | `task desktop:*` |

Everything is already reachable via `task` — the gap is **standardised artifacts**, **orchestration**, and **lifecycle commands** (run / restart / stop), not missing build commands.

---

## Proposed layout

```
build/                  # single artifact root (gitignored)
  frontend/             # vite build → build/frontend/
  backend/              # bootJar    → build/backend/stirling-server.jar
  rust/                 # release    → build/rust/stirling-server
  engine/               # not built; runtime only
  desktop/              # bundles    → build/desktop/

.run/                   # runtime state (gitignored): pids, logs, sockets
  java.pid
  rust.pid
  engine.pid
  frontend.pid
  logs/
    java.log
    rust.log
    ...
```

Rule: **no component writes artifacts outside `build/`** (except caches: `node_modules`, `.venv`, `target/` stay per‑component for incremental builds). Runtime state (pids/logs) lives in `.run/`.

---

## Command surface

### Build / check

| Command | Purpose |
|---|---|
| `task build:all` | frontend + backend:bootJar + rust:build --release |
| `task check` / `check:all` | all component checks |
| `task clean:all` | clean every component + `build/` + `.run/` |

### Per‑module build (development targeting)

Every `build:*` and `check:*` task accepts an optional **module filter** so you only rebuild what you touched. Convention: `task <component>:build -- <module>` or a dedicated `:module` task, e.g.:

```
task backend:build:module -- :app:core          # single Gradle subproject
task frontend:build:module -- src/routes/chat   # vite scoped build (or full rebuild if not supported)
task rust:build:module -- stirling-pdf         # cargo -p <crate>
task engine:check:module -- py/parsers         # pytest path
```

Add a helper `task build:changed` that computes changed paths (git diff) and dispatches to the right per‑module build — low risk, big DX win.

### Run / restart / stop

Per‑component lifecycle tasks, all writing pid + logs to `.run/`:

| Command | Purpose |
|---|---|
| `task rust:run` | start Rust server (:8081), background, pid → `.run/rust.pid`, log → `.run/logs/rust.log` |
| `task backend:run` | start Java (:8080) |
| `task engine:run` | start Python engine |
| `task frontend:run` | vite dev server (foreground by default; `--detach` to background) |
| `task desktop:run` | tauri dev |
| `task rust:restart` | stop if running → run |
| `task backend:restart` | stop → run |
| `task engine:restart` | stop → run |
| `task rust:stop` | kill pid from `.run/rust.pid`, clean pid file |
| `task backend:stop` / `engine:stop` / `frontend:stop` | same |
| `task dev:all` | **strangler topology default**: rust:run (:8081) → backend:run (:8080) → frontend:run → engine:run |
| `task restart:all` | restart every running component in dependency order |
| `task stop:all` | stop everything, clear `.run/` pids |
| `task status` | show running components (pid alive?), ports, last log line |

Implementation notes for run/restart/stop:

- Use a small `scripts/run.sh` (or `scripts/run.py`) helper: `start <name> <cmd>`, `stop <name>`, `status`. `task` calls it — avoids duplicating pid‑file logic in Taskfile.
- **Restart = stop (graceful SIGTERM, 5s timeout → SIGKILL) then start.** Never leave orphan ports; `status` should flag stale pids.
- `dev:all` must wait for readiness (health‑check `:8081` / `:8080`) before starting dependents — a `wait-for` helper in the same script.
- `clean:all` also removes `.run/` (but `stop:all` first to avoid killing nothing).

---

## Work items

### 1. Root‑level task aliases + lifecycle (highest value, lowest risk)

- `build:all`, `clean:all`, `check:all` (kept).
- New: `run:*`, `restart:*`, `stop:*`, `status`, `dev:all` (now includes `rust:run` so the strangler topology — Rust :8081 → Java :8080 — is the default dev loop), `restart:all`, `stop:all`.
- Shared `scripts/run.sh` helper for pid/log/health logic.

### 2. Per‑module build targeting

- Gradle: `task backend:build:module -- :app:core`.
- Cargo: `task rust:build:module -- <crate>` (`cargo build -p`).
- pytest: `task engine:check:module -- <path>`.
- Frontend: `task frontend:build:module -- <src path>` (full vite rebuild if scoping unsupported — document this).
- `task build:changed` dispatching from `git diff --name-only`.

### 3. Standardise Java output

- Add a `bootJar` destination override or copy task so the runnable jar lands at `build/backend/stirling-server.jar`. Keep `app/*/build/` for incremental compile — only final artifacts relocate. `backend:run` consumes the `build/` jar.

### 4. Rust release profile

- `[profile.release]` tuning (lto, codegen-units=1, strip) in `rust/Cargo.toml`; `task rust:build` → `cargo build --release` → copy binary to `build/rust/stirling-server`. The Rust backend is pure Rust (`lopdf`); no native libraries to bundle.

### 5. Frontend

- `vite build --outDir ../../build/frontend` (or post‑build copy) so `build/frontend/` is servable by either backend's static handler.

### 6. Docker / runtime packaging (later, when topology settles)

- `runtime/` compose/Dockerfiles reference `build/` artifacts; single context for image builds.

### 7. CI alignment

- `rust-server.yml` already builds Rust; add artifact‑upload from `build/rust/` so releases ship prebuilt binaries per platform.

---

## Status

Done:

- Lifecycle: `scripts/run.sh` + `run/stop/restart` per component,
  `status`, `stop:all`, `restart:all`, `dev:strangler` (Rust :8081 → Java
  :8080 → frontend + engine). Engine lifecycle is `engine:bg*` because
  `engine:run` already exists.
- Global tasks now include Rust: `build`, `test`, `lint`, `fix`, `check`,
  `check:all`, `clean` all cover `rust:*`; `build` also runs
  `backend:dist` → `build/backend/stirling-server.jar`; `rust:build`
  copies the release binary to `build/rust/stirling-server`.
- `build:all` / `clean:all` aliases; `clean` also stops dev processes and
  removes `build/` + `.run/`.

- `build/` artifacts verified: `backend:dist` → `build/backend/stirling-server.jar`,
  `rust:build` → `build/rust/stirling-server` (release, lto; smoke-tested `/health`).
- `frontend:dist` copies `frontend/dist` → `build/frontend/` (wired into
  `task build`; `dist/` stays canonical because CI/Tauri consume it).

Remaining:

- Per-module build targeting + `build:changed`.
- Docker/CI consumption of `build/` artifacts.

## Suggested order

1. ~~Root aliases + lifecycle (`run/restart/stop/status`, `dev:strangler`)~~ done.
2. ~~`build/` dir convention + Java jar copy task~~ done for backend + rust; frontend pending.
3. Per‑module build targeting + `build:changed`.
4. ~~Rust release profile~~ already present (`lto`, `codegen-units=1`).
5. Frontend outDir → `build/frontend`.
6. Docker/CI consumption of `build/`.

---

## Non‑goals

- Replacing Gradle/Cargo/npm/uv — each stays the component's build tool; `task` orchestrates, doesn't reimplement.
- Unifying version numbers — components version independently.
- No process supervisor (systemd/pm2) — `scripts/run.sh` pid files are enough for a dev loop; production packaging is Docker's job.
