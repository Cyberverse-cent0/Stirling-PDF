# Architecture Comparison — Java (`app/`) vs Rust (`rust/`)

Side-by-side structural map. "Java" = Spring Boot modular monolith;
"Rust" = axum strangler backend + in-progress Rust AI engine.
Detail ledgers: `migration/reports/FINAL_COMPLETION_AUDIT.md`,
`migration/reports/FINAL_MIGRATION_AUDIT.md`, `migration/reports/REMAINING_WORK.md`.

## Modules

| Java module | Role | Rust counterpart | Status |
|---|---|---|---|
| `app/core` | OSS API: ~96 controllers (general/analysis/misc/security/converters/pipeline/form/filters), web controllers, config, services | `rust/src/handlers/*` + `pdfdoc.rs` object-model library | PARTIAL — ~112 route paths registered; convert remainder + crypto family + raster ops still fall back |
| `app/common` | shared config (`settings.yml`), models, utils, temp files, InternalApiClient | `rust/src/settings.rs`, `config.rs`, `upload.rs`, `error.rs` | PARTIAL — settings.yml + custom_settings.yml merged; env parity for key knobs |
| `app/proprietary` | auth (users/teams/API keys/JWT), license, JPA/H2/Postgres, admin, audit, storage, MCP, policies | `rust/src/auth.rs`, `guard.rs`, `endpointconfig.rs`, `settings.rs`, `platform.rs` (partial) | PARTIAL — auth/settings/endpoint-config ports exist; JPA-backed features stay Java |
| `app/saas` | Supabase auth, billing/wallet, account-link, teams, procurement | none | MISSING — proxied to Java (explicit non-goal) |
| `engine/` (Python) | FastAPI AI engine :5001 | `rust/engine/` | PARTIAL — scaffold+stores+docparse done; agents/routers pending |
| frontend (`frontend/`) | Vite/React SPA | unchanged — calls `/api/*`; `web.rs` can serve built SPA | unaffected |

## Cross-cutting services

| Concern | Java implementation | Rust implementation | Parity |
|---|---|---|---|
| HTTP server | embedded Jetty/Tomcat via Spring Boot | axum/hyper | equivalent for API |
| Multipart | Spring `MultipartConfiguration` ~2 GB, temp spill | `upload.rs` `Vec<Upload>` in memory, `max_upload_bytes` mirrors `max-file-size` | PARTIAL — same cap; still memory-buffered (no temp spill) |
| Routing | `@RequestMapping` + `EndpointInterceptor` enable/disable | explicit axum routes + `endpointconfig.rs` enable/disable + opt-in `/api/**` fallback proxy | MATCHED for migrated set |
| Auth | filter chain (session, API key, JWT, license, rate limit) | `auth.rs` + `guard.rs` middleware on Rust routes | PARTIAL — core auth ported; proprietary license/JWT depth stays Java |
| Config | `settings.yml` + `custom_settings.yml` + env, ~20 sections | `settings.rs` typed model + env (HOST/PORT/JAVA_BACKEND_URL/metrics/CORS) | PARTIAL — per-key coverage audited in `migration/` |
| Errors | `@ControllerAdvice` JSON | `AppError` → JSON | PARTIAL — shape approximated |
| Async jobs | `AutoJobPostMapping` + `JobExecutorService` | in-memory tokio table | PARTIAL — no DB persistence (P6 decision) |
| PDF engine | PDFBox (+PDFium/jpdfium for some ops) | `lopdf` object model (`pdfdoc.rs`) | PARTIAL per endpoint |
| Sidecars | soffice, gs, qpdf, magick, tesseract, veraPDF | `convert.rs` external-engine runner + per-request forwards | PARTIAL |
| DB | JPA → H2/Postgres | none (server); rusqlite/sqlx (engine docs) | MISSING for server |
| Scheduler | `@Scheduled` services | none | MISSING |
| Observability | Micrometer metrics endpoints, WAU filter, audit | `metrics.rs` `/api/v1/info/*` + `TraceLayer` | PARTIAL |
| Static files/SPA | `static/`, ReactRoutingController | `web.rs` SPA static serving | MATCHED (verify asset paths vs real build — P6) |
| Logging | Logback + `LogbackPropertyLoader` | `tracing` + EnvFilter | equivalent |
| Graceful shutdown | Spring context close | ctrl-c + SIGTERM in `main.rs` | MATCHED |

## Data flow for a migrated endpoint

```
Browser → frontend /api/* → Rust :8081
  → guard/auth middleware → endpointconfig enable check
  → explicit route → #[endpoint] → Vec<Upload> (memory, capped)
  → lopdf Document → handler → PDF/JSON/ZIP bytes → response
Unmigrated /api/* → proxy.rs → Java (only when JAVA_BACKEND_URL set;
otherwise 404 — the proxy is opt-in, no longer default-on)
```

## Architectural risks / decisions already taken

- Strangler topology retained; the Java fallback proxy is now **opt-in**
  (`JAVA_BACKEND_URL`), so Rust can run standalone — unmigrated routes
  404 rather than silently depending on Java.
- Rust is pure-Rust `lopdf`; no native PDF libs — deliberate, documented
  in `rust/MIGRATION.md`.
- `convert.rs` shells out to the same external engines Java uses
  (Ghostscript, soffice, …) — sidecars are shared, not reimplemented.
- Some migrated handlers still forward to Java for unsupported parameter
  combos (`forward_multipart`) — explicit `JAVA_FALLBACK` per request.
- `rust/engine` is a *separate* crate/process, not wired into the server;
  Python `engine/` remains production.
- Proprietary/SaaS (~74 controllers) stays Java by design — see
  `migration/reports/JAVA_DECOMMISSIONING_AUDIT.md` for the end-state options.
