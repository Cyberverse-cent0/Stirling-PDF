# GLOBAL AUDIT REPORT — Stirling-PDF Java→Rust Migration

Date: 2026-11-07 · Scope: full repository · Method: 10 parallel evidence-driven workstreams (see `AGENT_REPORTS/`)

---

## 1. Executive summary

The Rust backend (`rust/`, axum, ~102k LOC across server + engine + CLI) is a **real, substantially complete port** of the OSS PDF tool surface — not a skeleton. The `#[endpoint]` macro forces PDF work onto `spawn_blocking`, multipart spools to temp files, engine invocations go through a timeout-guarded `EngineSpec` runner, and the route table (`rust/src/handlers/mod.rs:32-385`) registers ~150 `/api/v1/**` routes verified against the deleted Java controllers.

However, the audit found the migration is **not yet replaceable** for three structural reasons:

1. **The working tree is mid-flight and currently does not compile.** Uncommitted work (`signature.rs`, `apitests.rs`, `pdfua.rs`, `timestamp.rs`, handler edits) is actively landing — `restore-pdf-protection`, `timestamp-pdf`, `validate-signature`, and `accessibility-report` were routed *during* the audit. As of audit end, `cargo check --workspace --all-targets` fails (bad `der` import in `signature.rs:22`, type errors in `apitests.rs:254`). Several agent findings were fixed in-flight; the tree is a moving target.

2. **One confirmed P0 security defect is live**: path traversal → arbitrary file write via unsanitized multipart filename in `BlankRasterizer::probe` (`rust/src/handlers/misc.rs:2751-2753`), reachable from remove-blanks, scanner-effect, auto-split, OCR fallback, extract-image-scans — unauthenticated in the default open deployment.

3. **Java cannot be removed yet — and removing "just the migrated parts" is already broken.** `app/proprietary` + `app/saas` (~1,025 files, ~74 controllers) are the actual product platform (auth, billing, JPA, policies, cluster) and `app/common` infra is load-bearing for them. Meanwhile `InternalApiClient` in Java loops tool dispatch back to **Java's own port**, where the migrated controllers no longer exist → every policy/MCP/AI-workflow tool call 404s (A05-F02, P0 for proprietary automation). Dev tasks, live E2E, cucumber, and the release pipeline all still target the stripped Java backend.

## 2. Repository health snapshot

| Area | State | Evidence |
|---|---|---|
| Rust server compiles (lib/bin, non-test) | Was clean at audit start; **--all-targets now fails** on in-flight edits | `cargo check` runs by coordinator + agent-09 |
| Rust tests | ~458 tests, all handler modules covered; **1 failure observed mid-edit**; zero HTTP-level integration tests (`rust/tests/integration/` empty) | agent-09 |
| Route registration | ~150 explicit routes + metrics; fallback 404s unknown `/api/**`; guard 403s disabled endpoints | `handlers/mod.rs`, `guard.rs` |
| Java codebase | 2,106 `.java` files remain at HEAD; 85 live endpoint classes (9 core security/job + 53 proprietary + 21 saas + 2 non-`*Controller`) | agent-05 |
| Frontend↔backend | Core tool contract verified compatible (multipart `fileInput`, ProblemDetail errors, config shapes); dev defaults split between Java :8080 and Rust :8081 | agent-08 |
| External engines | 15+ sidecars probed at startup; argv-only exec with timeouts on the `EngineSpec` path; **8 direct `Command` call sites bypass timeouts** | agent-03/04/07 |
| Persistence | Jobs persisted to SQLite `configs/stirling-jobs.db` — real, but blobs duplicated in RAM+DB, no retention, no timeout, cancel doesn't stop work | agent-06 |
| CI | `rust-server.yml` lint+unit only; smoke tests never run in CI; live E2E/cucumber still target Java; release still ships Java JARs alongside Rust binaries | agent-09 |
| Generated artifacts | `SwaggerDoc.json` (gitignored) is produced by **booting Java** and feeds frontend types, Python engine models, AND Rust engine schemas — single point of failure for Java retirement | agent-05/09/10 |

## 3. Migration readiness verdict

**Endpoint parity (counted):** ~145 OSS Java endpoints vs ~150 Rust routes — near-complete *coverage* of the OSS surface. Missing: `cert-sign` family (4 routes), `print-file`, `pdf→video` (intentionally dead both sides), `pdf→ua` (proprietary). ~10 endpoints implemented in **both** Java and Rust with known semantic drift.

**Functional parity (verified behavior): materially lower than endpoint parity.** Confirmed divergences include: crop rejects every default UI request (P0 functional), flatten default mode unrouted/dead, redaction never removes content (security-relevant), text stamps/watermarks rejected, `?async=true` honored on only 2 of ~140 POST endpoints, merge rejects `generateToc`/`removeCertSign=false`, several engine-gated endpoints report wrong availability, and signature validation reports revocation "good" without checking.

**Do not state a single completion percentage.** By endpoint registration: ~95% of OSS routes exist. By verified functional parity: lower — the audit confirmed ~20 PARTIAL/DIVERGENT features and ~6 MISSING families (see `FUNCTIONALITY_PARITY_MATRIX.md`).

## 4. Top risks (cross-workstream deduplicated)

| # | Risk | Pri | Sources |
|---|---|---|---|
| 1 | Path traversal arbitrary file write via upload filename | **P0** | A07-F01 (verified by coordinator) |
| 2 | Working tree doesn't compile (--all-targets); in-flight signature/pdfua work unverified | **P0** | coordinator `cargo check`, A09-F02 |
| 3 | Java `InternalApiClient` dispatches migrated tools to Java port → 404 in policy/MCP/AI workflows | **P0** (proprietary scope) | A05-F02 |
| 4 | `crop` 400s on every normal UI request; `flatten` default 400s; merge rejects defaults the frontend sends | **P1** | A02-F01/F02/F06 |
| 5 | Redaction is overlay-only — text remains extractable (Java preferred verified removal) | **P1** security | A01-F04, A02-F03 |
| 6 | Engine calls without timeouts (8 sites) + no `-dSAFER` consistency + zip-bomb gaps + no request-size/concurrency limits | **P1** | A07-F02/F03/F08/F10, A04-F01/F06, A03 |
| 7 | Job framework: no timeout/concurrency/retention; cancel leaks storage; results fully in RAM; predictable IDs → IDOR | **P1** | A06, A07-F04/F09, A04-F02/F03 |
| 8 | Revocation status fabricated "good"; no OCSP/CRL | **P1** security | A07-F05, A01-F05 |
| 9 | Default dev/E2E/release flows still point at stripped Java → broken or silently narrowed coverage | **P1** | A05-F01/F05/F06, A09-F01/F03 |
| 10 | Generated tool-model surface collapsed to 7 endpoints (Java spec source); Rust engine schemas stale & ungated | **P1** | A09-F02, A10-F03 |

## 5. What is genuinely complete and verified

- Page ops: rotate, remove/rearrange/split (pages/size/chapters/sections/poster), scale, multi-page-layout/booklet — native lopdf, real implementations.
- Analysis (8) + filter (6) + form (9) endpoint families.
- OCR (OCRmyPDF→Tesseract fallback), ocr-pdf raster path via hayro.
- Most conversions via `EngineSpec` sidecars: office↔pdf, html/markdown/url/eml→pdf, pdf→html/csv/xlsx/markdown/text, vector↔pdf, cbz/cbr↔pdf, ebook↔pdf/epub.
- Password add/remove/prepare-session/**restore-protection** (restore landed in-flight, uncommitted).
- Native RFC 3161 timestamping (`timestamp.rs`), CMS/PKCS#7 signature verification (`signature.rs`, minus revocation).
- Job/file API shape largely frontend-compatible; SQLite persistence real.
- Platform layer: settings.yml, endpoint enable/disable, app-config/ui-data shapes match frontend.
- Mobile-scanner session API (6 routes), pipeline handleData, attachments/portfolio ops, sanitize, get-info, remove-cert-sign, verify/validate-compliance (veraPDF CLI).

## 6. Assessment

The Rust service is **architecturally sound and ~90% ported by surface area**, held back less by missing endpoints than by: (a) a handful of high-impact behavioral divergences on common tools (crop/flatten/merge/redact), (b) job/persistence reliability gaps, (c) security hardening gaps inherited-or-worse vs Java, (d) the unresolved Java-retirement blockers (proprietary platform, SwaggerDoc codegen, dev/E2E/release wiring), and (e) an unstable working tree that must be stabilized before any further migration work.

**Recommended order:** stabilize tree → fix P0s → close functional divergences on core tools → harden jobs/security → repoint dev/E2E/codegen off Java → decide proprietary/Java fate → then Java removal.

See `REMAINING_WORK_MASTER.md` for the deduplicated finding list, `NEXT_PHASE_ROADMAP.md` for sequencing, and `JAVA_REMOVAL_READINESS.md` for the removal plan.
