# runtime/

Archived Docker runtime assets. The project now builds and runs natively
(`task dev`, `./gradlew :stirling-pdf:bootRun`, `cargo run` for the Rust
service) and does not need Docker for day-to-day development.

Everything here is preserved so the containerized deployment path can be
restored later. Nothing in the active build references these files.

## Contents

- `docker/` — was `docker/`: base/embedded/backend/frontend/engine/unoserver
  Dockerfiles and compose stacks. Compose files use `context: ../../..`
  (repo root) with `dockerfile: runtime/docker/...`.
- `testing/compose/` — was `testing/compose/`: Keycloak, multinode, SaaS,
  security, and coverage compose fixtures plus their start/validate scripts.
- `testing/test.sh`, `testing/test2.sh` — was `testing/`: Docker-image test
  matrix runners. Path references inside were updated to `runtime/`.
- `taskfiles/docker.yml` — was `.taskfiles/docker.yml`: the `task docker:*`
  commands. To restore, move it back to `.taskfiles/docker.yml` and re-add the
  `docker:` include in `Taskfile.yml`.
- `workflows/` — was `.github/workflows/`: Docker-only CI workflows
  (image builds/pushes, compose tests, PR demo deploys, rollbacks).
- `dockerignore` — was `.dockerignore`.

## Restoring

1. `git mv runtime/docker docker`
2. `git mv runtime/testing/compose testing/compose` and
   `git mv runtime/testing/test*.sh testing/`
3. `git mv runtime/taskfiles/docker.yml .taskfiles/docker.yml`
4. `git mv runtime/dockerignore .dockerignore`
5. Move wanted files from `runtime/workflows/` back to `.github/workflows/`.
6. Revert path prefixes: `runtime/docker/` → `docker/`,
   `runtime/testing/compose` → `testing/compose`, and in compose files
   `context: ../../..` → `context: ../..`, `dockerfile: runtime/docker/` →
   `dockerfile: docker/`.
7. Re-add the `docker:` include to `Taskfile.yml`.
