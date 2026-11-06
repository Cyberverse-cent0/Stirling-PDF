# Java → Rust Migration Audit

Component-level ledger for the Java-to-Rust migration. Authoritative detail
now lives in `migration/` (`FINAL_COMPLETION_AUDIT.md`,
`FINAL_MIGRATION_AUDIT.md`, `REMAINING_WORK.md`, `reports/`); `rust/MIGRATION.md`
holds per-endpoint divergences. This file tracks the mission-level status.

Statuses: `NOT_AUDITED`, `MATCHED`, `PARTIAL`, `MISSING`, `DEFECT`, `PORTING`,
`TESTING`, `VERIFIED`, `JAVA_FALLBACK`.

## Application bootstrap

| Java component | Rust equivalent | Status | Evidence / action |
|---|---|---|---|
| `SPDFApplication.main` (Spring Boot) | `rust/src/main.rs` | PARTIAL | Independent entry: axum, settings.yml (`settings.rs`), endpoint config (`endpointconfig.rs`), auth guard (`auth.rs`/`guard.rs`), metrics (`metrics.rs`), SPA static hosting (`web.rs`), graceful shutdown (ctrl-c + SIGTERM), multipart part cap, Java-matching CORS. Missing: profile detection (default/security/saas), `@Scheduled` jobs, browser-open, template/static dir creation. |
| `ConfigInitializer` + `configs/settings.yml` + `custom_settings.yml` | `settings.rs` merged model | PARTIAL | Rust reads both YAML files; per-key coverage vs `ApplicationProperties` is tracked in `migration/` audits — verify exhaustively before cutover. |
| Profile detection (`default`/`security`/`saas`) | — | MISSING | No flavor concept; proprietary/SaaS stay Java (see JAVA_DEPENDENCIES.md). |
| `@EnableScheduling` (~20 `@Scheduled` classes) | — | MISSING | Core: TempFileCleanupService, FileMonitor, metrics aggregators; proprietary schedulers stay Java. |
| Component scan / DI | explicit `handlers::router` | PARTIAL | ~112 explicit `/api/v1/**` routes. **Java proxy is now opt-in**: `JAVA_BACKEND_URL` unset/`off` → unmigrated routes 404 (was: default `localhost:8080`). |

## HTTP routing — core API

Route families now registered in Rust (from `handlers/mod.rs`, ~112 paths):

| Family | Endpoints | Status | Notes |
|---|---|---|---|
| `general/*` | ~19: merge, rotate, remove/rearrange/split-pages, splits, scale, crop, pdf-to-single-page, multi-page-layout, booklet-imposition, overlay-pdfs, edit-table-of-contents, extract-bookmarks, split-for-poster-print, remove-image | PARTIAL | lopdf ports; divergences per endpoint in `rust/MIGRATION.md` (outlines, orphaned fields, annotations on transforms, clip-only crop) |
| `analysis/*` | all 8 | PARTIAL | Documented divergences (dates, font-info, security-info, form-field counting) |
| `filter/*` | 6 filter-* batch wrappers | TESTING | Wrappers over migrated ops |
| `form/*` | 9: fields, fill, add/delete/edit/modify-fields, extract-csv/xlsx, fields-with-coordinates | TESTING | New `form.rs` (~4k lines); AcroForm read/write port — verify vs Java field semantics |
| `misc/*` | ~24 incl. mobile-scanner ×6 | PARTIAL | decompress, update-metadata, flatten (forms-only + portfolio), compress, show-javascript, attachments (5), auto-rename, auto-rotate, add-image, add-stamp, add-page-numbers, add-comments, extract-images, create/flatten-portfolio, repair, replace-invert, unlock-forms, batch-process-attachments, add-attachments, remove-blanks | 
| `security/*` | sanitize, get-info-on-pdf, remove-cert-sign, add/remove-password, redact, add-watermark, prepare-pdf-session | PARTIAL | Crypto family (cert-sign, timestamp, validate-signature, verify-pdf, validate-compliance) stays Java per P5 — needs CMS/veraPDF stack |
| `convert/*` | 11: img/pdf, pdf/img (Ghostscript), svg, office, html both ways, vector both ways, pdf→word/presentation/xml/text | PARTIAL | `convert.rs` engine-runner; remaining ~12 on Java fallback per `REMAINING_WORK.md` P3 |
| `config/*`, `settings/*`, `ui-data/*` | endpoint-enabled(s), group-enabled, login-disclaimer, get-endpoints-status, update-enable-analytics, footer-info, home, licenses, ocr-pdf, pipeline | TESTING | `platform.rs`/`endpointconfig.rs`/`settings.rs`; `/config/app-config` still proxied (P2) |
| `mobile-scanner/*` | 6 session/upload/download routes | TESTING | New port in `platform.rs`/misc |
| `info/*` metrics | requests/version etc. | TESTING | `metrics.rs`, gated by `metrics.enabled` |

## Platform services (Spring-heavy)

| Area | Status | Notes |
|---|---|---|
| AuthN/Z (sessions, API keys, license) | PARTIAL | `auth.rs` + `guard.rs` middleware ported; proprietary auth/licensing stack stays Java — see `migration/JAVA_DECOMMISSIONING_AUDIT.md` |
| Jobs (`?async=true`) | PARTIAL | In-memory job table + files endpoints; no DB persistence (P6 decision pending) |
| Pipeline/workflow, folders | JAVA_FALLBACK | JPA-backed; `handleData` port is P2-last |
| Admin/audit/billing/teams/account-link/MCP/policy/SaaS | JAVA_FALLBACK | ~74 proprietary/SaaS controllers explicitly out of scope |
| AI engine | JAVA_FALLBACK | `engine/` Python service unchanged |

## Cross-cutting prerequisites

| Item | Status |
|---|---|
| Async job framework | PARTIAL (in-memory; persistence decision in P6) |
| AuthN/Z layer | PARTIAL (`auth.rs`/`guard.rs`; proprietary stack on Java) |
| Settings reader | DONE (`settings.rs` — coverage audit in `migration/`) |
| Multipart limits | DONE (`max_upload_bytes` mirrors `max-file-size` ~2 GB) |
| Graceful shutdown | DONE (ctrl-c + SIGTERM in `main.rs`) |
| Golden-file parity harness | DONE (`scripts/rust_pdf_parity.py`, `testing/parity.sh`, `pdfdiff`/`pdf_fingerprint` examples) |
| Live Java↔Rust parity run | PENDING — needs `task backend:dev` (:8080) + `task rust:dev`; `REMAINING_WORK.md` P1 |
