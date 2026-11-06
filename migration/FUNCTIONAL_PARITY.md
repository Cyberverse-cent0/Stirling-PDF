# Functional Parity — Java ↔ Rust

Evidence-based parity status per subsystem. Statuses:
`MATCHED` / `PARTIAL` / `MISSING` / `DEFECT` / `VERIFIED` / `JAVA_FALLBACK`.
Authoritative per-endpoint divergences: `rust/MIGRATION.md` +
`migration/FINAL_COMPLETION_AUDIT.md`.

## PDF operation endpoints (`/api/v1/{general,analysis,misc,security,form,filter,convert,layout}`)

| Endpoint group | Java impl | Rust impl | Status | Evidence / notes |
|---|---|---|---|---|
| general (~19): merge, rotate, page ops, splits, scale, crop, layouts, TOC, bookmarks | PDFBox + utils | `lopdf` via `pdfdoc.rs`, `page_select.rs`, `transform.rs`, `split.rs`, `layout` modules | PARTIAL→TESTING | ~167 unit tests; `rust_pdf_parity.py` fingerprint harness; divergences in `rust/MIGRATION.md` |
| analysis (8) | PDFBox inspectors | `analysis.rs` on lopdf | PARTIAL | date format, fieldCount=widgets, font list semantics |
| filter (6) | per-tool batch wrappers | `filter` handlers over migrated ops | TESTING | thin wrappers; verify param parity per tool |
| form (9): fields, fill, add/delete/edit/modify, extract-csv/xlsx | PDFBox AcroForm | `form.rs` (~4k lines) AcroForm port | TESTING | new; verify field semantics vs Java before cutover claims |
| misc (~24): decompress, metadata, flatten, compress, show-js, attachments, auto-rename/rotate, stamps/images/comments/page-numbers, portfolios, repair, invert, unlock-forms | PDFBox + sidecars | `misc.rs`, `attachments.rs`, `forms.rs`, `convert.rs` | PARTIAL | per-endpoint divergences documented; raster-dependent ops need engine decision (P4) |
| security: sanitize, get-info-on-pdf, remove-cert-sign, passwords, redact, watermark, prepare-session | PDFBox + crypto | `security.rs`, `passwords.rs` | PARTIAL | crypto family (cert-sign create, timestamp, validate, verify, compliance) stays Java — P5 |
| convert (11 of ~23): img/pdf, pdf/img, svg, office, html, vector, word/presentation/xml/text | ProcessExecutor + engines | `convert.rs` engine runner | PARTIAL | ~12 convert endpoints remain on Java fallback (P3) |

## Async job framework

| Capability | Java | Rust | Status |
|---|---|---|---|
| `?async=true` submit | AutoJobPostMapping→JobExecutorService | jobs.rs in-memory table | PARTIAL |
| status/result/files endpoints | `/api/v1/general/job/*` | same paths | PARTIAL |
| unknown job → Java fallback | n/a | `proxy::forward` | JAVA_FALLBACK (good) |
| persistence across restart | Java in-memory too? | lost on restart | MATCHED-ish; divergences documented |
| ISO datetimes, progress %, retention, queue weighting | yes/no per Java | epoch secs, none | DEFECT (documented) |

## Platform services

| Capability | Java | Rust | Status |
|---|---|---|---|
| AuthN/Z (login, API keys, JWT, license, rate limits, invites, MFA) | proprietary+saas filter chains, JPA | `auth.rs`/`guard.rs` on Rust routes | PARTIAL — core ported; proprietary depth stays Java |
| Settings (`settings.yml`, custom_settings, endpoints.enabled, admin settings API) | ConfigInitializer + ApplicationProperties + AdminSettingsController | `settings.rs` + `endpointconfig.rs` + `platform.rs` settings endpoints | PARTIAL — `/config/app-config` still proxied (P2); coverage audit in `migration/` |
| UI data (footer/home/licenses/ocr-pdf/pipeline), login-disclaimer, endpoint status | UIDataController etc. | `platform.rs` handlers | TESTING |
| Mobile scanner sessions | MobileScannerController | `mobile-scanner/*` routes | TESTING |
| DB (users, teams, audit, storage, payg, policies) | JPA/H2/Postgres | none | MISSING — proxied |
| Pipeline/watch-folder automation | pipeline controllers + scheduler | `ui-data/pipeline` only; `handleData` pending (P2-last) | MISSING — proxied |
| Static UI / SPA | ReactRoutingController + static | `web.rs` SPA serving | MATCHED (asset-path verification pending, P6) |
| Scheduled jobs (temp cleanup, metrics, WAU, license check…) | @Scheduled | none | MISSING |
| Observability (Micrometer, audit log, correlation IDs) | filters/interceptors | `metrics.rs` + TraceLayer | PARTIAL |
| Engine proxy (`/api/v1/ai-engine/*`) | AiEngineController→Python :5001 | proxied to Java; `rust/engine` not wired | JAVA_FALLBACK |
| CORS | Spring config | Java-matching origin patterns + Tauri mode | MATCHED |
| Multipart limits | ~2 GB + temp spill | same cap, in-memory | PARTIAL |
| Graceful shutdown | Spring context close | ctrl-c + SIGTERM | MATCHED |

## AI engine (`engine/` Python → `rust/engine/`)

| Area | Python | Rust engine | Status |
|---|---|---|---|
| HTTP scaffold/auth/errors/CORS | FastAPI | axum port | PARTIAL (implemented per README) |
| Contracts/DTOs | pydantic | serde port | PARTIAL |
| Chunking/docparse | python impl | char-exact port, regression-tested | TESTING |
| Embedder/doc stores | openai/voyage/ollama; sqlite-vec/pgvector | same wire formats; vendored sqlite-vec | PARTIAL |
| Agents, orchestrator streaming, routers, config cache, PostHog | full | not ported | MISSING |
| Test suite | ~47 pytest files | none ported | MISSING |

## Verdict

Rust independently serves: ~112 route paths across general/analysis/
filter/form/misc/security/convert/layout/config/ui-data/mobile-scanner,
with auth middleware, settings enforcement, CORS parity, SPA hosting,
metrics, and graceful shutdown — **without JPA persistence, schedulers,
crypto signing stack, or several convert/raster endpoints**. Remaining
Java dependency: the opt-in proxy fallback for unmigrated routes plus
the proprietary/SaaS surface (explicitly out of scope). See
`migration/REMAINING_WORK.md` for the ordered punch list (P0 done;
P1 verify, P2 small endpoints, P3 convert family, P4 raster, P5 crypto,
P6 platform).
