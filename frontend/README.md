# Frontend

All frontend commands are run from the repository root using [Task](https://taskfile.dev/):

- `task frontend:dev` — start Vite dev server (localhost:5173)
- `task frontend:build` — production build
- `task frontend:test` — run tests
- `task frontend:test:watch` — run tests in watch mode
- `task frontend:lint` — run linting
- `task frontend:typecheck` — run TypeScript type checking
- `task frontend:check` — run typecheck + lint + test
- `task frontend:install` — install npm dependencies

For desktop app development, see the [Tauri](#tauri) section below.

## Layout

`frontend/` is a workspace containing one or more apps. Today it holds the
PDF editor under `frontend/`; new apps (the developer portal, etc.)
will sit alongside it as siblings. Shared tooling — `package.json`, `node_modules`,
`.storybook/`, oxlint, oxfmt — lives at `frontend/` so every app installs
once and lints with the same config.

## Environment Variables

The editor's environment variables live in committed `.env` files at
`frontend/`:

- `.env` — used by all builds (core, proprietary, and as the base for desktop/SaaS)
- `.env.desktop` — additional vars loaded in desktop (Tauri) mode
- `.env.saas` — additional vars loaded in SaaS mode

These files contain non-secret defaults and are checked into Git, so most dev work needs no further setup.

To override values locally (API keys, machine-specific settings), create an uncommitted sibling `.env.local` / `.env.desktop.local` / `.env.saas.local`. Vite automatically layers these on top of the committed files.

## Docker Setup

For Docker deployments and configuration, see the [Docker README](../docker/README.md).

## Tauri

All desktop tasks are available via [Task](https://taskfile.dev). From the root of the repo:

### Dev

```bash
task desktop:dev
```

This builds and stages the `stirling-server` sidecar plus bundled engines, then starts Tauri in dev mode.

### Build

```bash
task desktop:build
```

This rebuilds the `stirling-server` sidecar and stages the bundled engines, then builds the Tauri app for production.

Platform-specific dev builds are also available:

```bash
task desktop:build:dev           # No bundling
task desktop:build:dev:mac       # macOS .app bundle
task desktop:build:dev:windows   # Windows NSIS installer
task desktop:build:dev:linux     # Linux AppImage
```

### Clean

```bash
task desktop:clean
```

Removes all desktop build artifacts including staged engines, the sidecar binary, Cargo build, and dist/build directories.
