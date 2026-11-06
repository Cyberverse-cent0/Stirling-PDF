# Java Dependencies — what still requires the JVM

Everything the Rust backend cannot yet do alone, and what must be replaced
or explicitly retired before Java can be removed. Ordered punch list:
`migration/REMAINING_WORK.md` (P0 compile — done; P1–P6 open).

## Hard blockers (functional)

| Dependency | Where used | Rust path forward |
|---|---|---|
| ~12 convert endpoints (`url/pdf`, `markdown/pdf`, `ebook/pdf`, `eml/pdf`, `pdf/csv|epub|xlsx|video|pdfa`, `cbz/cbr` families) | ConvertApi controllers | `convert.rs` EngineSpec per endpoint (P3); absent engine → Java's group-enable 403 semantics |
| Security crypto family: cert-sign create, timestamp-pdf, validate-signature, verify-pdf, validate-compliance, hardware signing | SecurityApi + proprietary signing | CMS stack (`rasn`+`rasn-cms`), PKCS#11 (`cryptoki`), veraPDF binary — dedicated engine project; keep Java fallback, do NOT ship approximations (P5) |
| Raster/OCR misc: ocr-pdf, extract-image-scans, remove-blanks, auto-split-pdf, scanner-effect | MiscApi + tesseract/ocrmypdf | Ghostscript rasterisation vs keep-Java decision per endpoint (P4) |
| `/api/v1/config/app-config` + pipeline `handleData` | ConfigController, pipeline | small ports (P2); proxy covers meanwhile |
| AuthN/Z proprietary depth: license enforcement, JWT/OAuth2/SAML sessions, invites, MFA, rate limits | proprietary `SecurityConfiguration`, saas filters | `auth.rs`/`guard.rs` cover core; proprietary stack stays Java (explicit non-goal — see decommissioning audit) |
| JPA persistence (H2/Postgres): users, teams, licenses, audit, storage, payg, policies | proprietary + saas modules | SQLx/SQLite port — large; or keep slim Java control plane (open decision) |
| Scheduled jobs | TempFileCleanup, FileMonitor, metrics/WAU aggregation, license checker, usage sync | tokio interval tasks in Rust server |
| External sidecars invoked by Java ops | soffice, gs, qpdf, magick, tesseract, veraPDF, ffmpeg, unrar | Rust spawns the same binaries via `convert.rs` (not Java deps); document required sidecars (P6) |
| Python AI engine :5001 | `/api/v1/ai-engine/*` via Java proxy | `rust/engine` (in progress) or keep Python service standalone |

## Resolved since first audit (no longer Java dependencies)

- `settings.yml` config system → `settings.rs` typed model + `endpointconfig.rs`
- Auth middleware on Rust routes → `auth.rs`/`guard.rs`
- Static SPA hosting → `web.rs`
- CORS parity incl. Tauri mode → `config.rs` + `main.rs`
- Multipart size cap → `max_upload_bytes` in `upload.rs`
- Graceful shutdown → `shutdown_signal()` in `main.rs`
- Operational metrics endpoints → `metrics.rs` (`/api/v1/info/*`)
- UI-data/config read endpoints → `platform.rs` (~10 routes)
- Mobile scanner session API → `mobile-scanner/*` routes

## Soft/partial Java dependencies inside migrated endpoints

- `compress-pdf` `lineArt=true`, `flatten` non-forms modes,
  `crop autoCrop`/`removeDataOutsideCrop` → forwarded to Java per-request
  (`proxy::forward_multipart`).
- Job endpoints forward unknown job IDs to Java.
- `misc::remove_blanks` exists but stays unrouted pending the P4
  raster decision.

## Java-only libraries with no Rust equivalent yet

- PDFBox features lopdf lacks: content-stream surgery at PDFBox fidelity,
  PDF/A, advanced AcroForm, render-dependent ops (auto-rotate, OCR geometry,
  scanner-effect).
- veraPDF compliance reporting.
- CMS/CADES signature creation, RFC 3161 timestamping (cert-sign,
  timestamp-pdf, hardware-signing).
- Redaction content removal.
- Apache POI / office document handling (remaining ConvertApi family).
- Spring Security OAuth2/SAML/OIDC client stack.
- jpdfium PDFium bindings (Java-side rendering).

## Runtime requirements to eliminate

- JDK + Gradle build (`./gradlew :stirling-pdf:bootRun`)
- H2 database files / Postgres for proprietary features
- `configs/settings.yml` bootstrap semantics — **largely ported**
  (`settings.rs`); residual per-key gaps tracked in `migration/`
- Scheduled executor, servlet container, Micrometer registry
- The opt-in `JAVA_BACKEND_URL` proxy itself — the last thing standing
  between Rust and standalone operation

## Explicit non-goals / stay-Java candidates

Per `migration/JAVA_DECOMMISSIONING_AUDIT.md`: proprietary/SaaS modules
(billing, storage, MCP, policy, account-link, ~74 controllers) may remain
Java longest — or move to a slim control plane. This is an **open
architectural decision**, documented but not yet resolved.
