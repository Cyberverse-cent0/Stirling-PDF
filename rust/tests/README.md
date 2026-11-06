# Rust backend tests

The Rust backend is a **binary crate**, so `tests/*.rs` integration tests
cannot import its modules. Tests therefore live in two places:

- **Unit tests** — `#[cfg(test)] mod tests` inside `src/` (handlers,
  `pdfdoc`, `page_select`, `upload`, `imgcomp`, `forms`, `attachments`).
  They build PDFs with `lopdf` via `src/testkit.rs` and call the endpoint
  work functions directly — no manual fixture files needed.
- **Smoke tests** — `stirling test runtime|general|security` (the
  `stirling` CLI in `rust/cli`) boot the real server
  (`JAVA_BACKEND_URL=off`) and POST the generated fixtures at
  `tests/fixtures/` over HTTP. These replaced the former
  `tests/scripts/*.sh` shell harnesses.

## Layout

```
tests/
├── fixtures/          # generated PDFs (written by testkit tests, not checked in)
├── integration/       # reserved for future HTTP-level integration tests
└── README.md
```

## Running

```bash
cd rust
cargo test --all-targets                 # unit tests only
cargo run -p stirling -- test             # fmt + clippy + workspace tests
cargo run -p stirling -- test runtime     # live multi-endpoint smoke
cargo run -p stirling -- test general     # general unit tests + live smoke
cargo run -p stirling -- test security    # security unit tests + live smoke
cargo run -p stirling -- test coverage    # per-file test-count inventory
SMOKE_PORT=18098 cargo run -p stirling -- test security   # override the smoke port
```

Fixtures regenerate on every `cargo test` run (the
`testkit::fixture_tests::writes_sample_fixtures` test), so they are always
in sync with the code that produces them.
