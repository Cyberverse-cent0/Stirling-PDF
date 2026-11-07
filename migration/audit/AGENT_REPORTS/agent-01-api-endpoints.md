# A01 — API & Endpoint Parity Audit (round 2, 2026-11-07)

Read-only audit. Java route surface in scope is small: `app/common` holds only annotations; `app/core` retains ~10 endpoint classes (8 security controllers + `JobController` + `ConvertPDFToMarkdown`); proprietary/saas summarized as out-of-scope.

Rust routing is explicit in `rust/src/handlers/mod.rs:32-385` (`router()`); `#[stirling_macros::endpoint]` generates handler fns only. Unrouted `/api/**` → fallback 404 (`mod.rs:389-394`); disabled endpoint keys → 403 from `api_guard` (`guard.rs:37-51`) before routing. Auth: optional `RUST_API_KEY` only (`auth.rs:83-94`); proprietary/saas authz not ported (warn-only `auth.rs:98-118`).

## Parity table — Java endpoints (app/common + app/core)

| Java route | Method | Rust status | Evidence |
|---|---|---|---|
| `/api/v1/security/prepare-pdf-session` | POST multipart | COMPLETE (minor divergences) | Java `PasswordController.java:46`; Rust `passwords.rs:449-478`, routed `mod.rs:232` |
| `/api/v1/security/restore-pdf-protection` | POST multipart | MISSING → 403 via endpoint guard | Java `PasswordController.java:67`; Rust `passwords.rs:23-25`; `endpointconfig.rs:375` |
| `/api/v1/security/verify-pdf` | POST multipart | PARTIAL (veraPDF CLI dependency; WTPDF unvalidated) | Java `VerifyPDFController.java:43`; Rust `security.rs:3073-3085`, `verapdf.rs:1-17`; `mod.rs:197` |
| `/api/v1/security/validate-signature` | POST multipart | PARTIAL (no OCSP/CRL/AIA revocation; no AATL/EUTL anchors) | Java `ValidateSignatureController.java:84`; Rust `signature.rs:1053-1105`, `signature.rs:7-13`; `mod.rs:207` |
| `/api/v1/security/validate-compliance` | POST multipart | PARTIAL (same veraPDF CLI dependency) | Java `ValidateComplianceController.java:55`; Rust `security.rs:3191-3238`; `mod.rs:199` |
| `/api/v1/security/timestamp-pdf` | POST multipart | COMPLETE (minor: query `tsaUrl` ignored; 60s vs 30s read timeout; debug `eprintln!` left) | Java `TimestampController.java:84`; Rust `timestamp.rs:56-80`; `mod.rs:211` |
| `/api/v1/security/redact` | POST multipart | PARTIAL (overlay-only; matches Java overlay impl; `fileId` unsupported) | Java `RedactController.java:92`; Rust `security.rs:1838-1938`; `mod.rs:196` |
| `/api/v1/security/auto-redact` | POST multipart | PARTIAL (always overlay — Java prefers JPDFium verified removal; Java-valid regex rejected) | Java `RedactController.java:140`; Rust `security.rs:2000-2091`; `mod.rs:214` |
| `/api/v1/security/redact-execute` | POST multipart | PARTIAL (AUTO never removes covered content; `fileId` unsupported) | Java `RedactController.java:312`; Rust `security.rs:2590-2790`; `mod.rs:216` |
| `/api/v1/security/cert-sign` | POST multipart+form | MISSING → 403 | Java `CertSignController.java:92,182`; `endpointconfig.rs:372`; no route |
| `/api/v1/security/cert-sign/hardware/capabilities` | GET | MISSING → 403 | Java `HardwareSigningController.java:40`; `endpointconfig.rs:1010-1011` |
| `/api/v1/security/cert-sign/hardware/windows-certificates` | GET | MISSING → 403 | Java `HardwareSigningController.java:51` |
| `/api/v1/security/cert-sign/hardware/pkcs11-certificates` | POST JSON | MISSING → 403 | Java `HardwareSigningController.java:63` |
| `/api/v1/general/job/{jobId}` | GET | PARTIAL (no queue position; unix-second timestamps; extra `allResultFiles`) | Java `JobController.java:63`; Rust `jobs.rs:339-344,299-311`; `mod.rs:323` |
| `/api/v1/general/job/{jobId}/result` | GET | COMPLETE (no cluster 410/503 paths) | Java `JobController.java:98`; Rust `jobs.rs:346-382`; `mod.rs:324` |
| `DELETE /api/v1/general/job/{jobId}` | DELETE | COMPLETE (no queue; `wasQueued` always false) | Java `JobController.java:160`; Rust `jobs.rs:402-430`; `mod.rs:329` |
| `POST /api/v1/general/jobs/cleanup` | POST | COMPLETE (clears all finished; Java retention/user-scoped) | Java `JobController.java:222`; Rust `jobs.rs:455-485`; `mod.rs:335` |
| `/api/v1/general/job/{jobId}/result/files` | GET | COMPLETE | Java `JobController.java:246`; Rust `jobs.rs:384-400`; `mod.rs:325` |
| `/api/v1/general/files/{fileId}/metadata` | GET | COMPLETE | Java `JobController.java:281`; Rust `jobs.rs:487-505`; `mod.rs:330` |
| `/api/v1/general/files/{fileId}` | GET | COMPLETE (no `filename*` UTF-8 encoding) | Java `JobController.java:338`; Rust `jobs.rs:432-450`; `mod.rs:334` |
| `/api/v1/convert/pdf/markdown` | POST multipart | PARTIAL (`useStirlingOfficeConvert=true` → 400; default ported) | Java `ConvertPDFToMarkdown.java:38-51`; Rust `convert.rs:4110-4132`; `mod.rs:308` |

**Proprietary/saas (out-of-scope summary):** ~120 mappings under `app/proprietary` (auth/user/team/admin, storage, policies, signing sessions, workflow, audit, AI, integrations/Purview, `/api/v1/proprietary/ui-data/*`, `AccessibilityReportController` → `/api/v1/security/accessibility-report`, `ConvertPdfToPdfUa` → `/api/v1/convert/pdf/ua`, `EmailController` → `/api/v1/general/send-email`) and ~30 under `app/saas` (payg, procurement, account-link, ai proxy, teams). No Rust routes; `endpointconfig.rs:370-382` disables their keys (`ingest`, `orchestrate`, `tools`, `external-api-call`, `purview-*`, `cert-sign` family, `restore-pdf-protection`, `pdf-to-ua`, `form-detection`).

**Rust-only extras** (migrated tool surface): all `/api/v1/general/*`, `/api/v1/misc/*`, `/api/v1/security/{sanitize-pdf,get-info-on-pdf,remove-cert-sign,add-password,remove-password,add-watermark,add-stamp,accessibility-report}`, `/api/v1/analysis/*`, `/api/v1/filter/*`, `/api/v1/form/*`, `/api/v1/convert/*` (except pdf/markdown twin), `/api/v1/mobile-scanner/*`, `/api/v1/pipeline/handleData`, `/api/v1/config/*`, `/api/v1/ui-data/*`, `/api/v1/settings/*`, `/api/v1/info/version`, `/api/v1/ping`, `/metrics`. Internal gaps: `misc.rs:1179` (`convertToPdfA3b` rejected on attachment endpoints), `visual.rs:743,969` (text stamps/watermarks rejected), `automation.rs:169` (`detectionMode=osd` rejected), engine-gated branches in `convert.rs`.

## Findings

### A01-F01 — `cert-sign` family entirely missing (403, not 404) — P1
Java `CertSignController.java:92,182` (certTypes PEM/PKCS12/PFX/JKS/SERVER/WINDOWS_STORE/PKCS11, multipart+form) and `HardwareSigningController.java:31-84` (3 hardware routes). Rust: no routes; `endpointconfig.rs:371-372` `UNSUPPORTED_ENDPOINTS` → `api_guard` 403 (`guard.rs:40-42`). Frontend `useCertSignOperation.ts:19` and `hardwareSigningService.ts` fail on Rust. 403-disabled vs clean 404 indistinguishable to clients.

### A01-F02 — `restore-pdf-protection` missing — P1
Java `PasswordController.java:67` re-encrypts output with source key material via JPDFium `PdfSecurity.copyEncryption`. Rust `passwords.rs:23-25` defers to Java; `endpointconfig.rs:375` → 403. Frontend `pdfSessionUnlock.ts` calls it for every tool output on an encrypted input → whole operation fails.

### A01-F03 — `fileId` request parameter unsupported on every Rust endpoint — P1 (systemic)
Java `PDFFile.java:27-35`: every tool accepts `fileId` instead of `fileInput` for async chaining (`@AssertTrue isValid()` XOR). Rust `upload.rs:191-251` only understands `fileInput`; `fileId` swallowed as text param. Grep: zero `fileId` handling in handlers. Async/pipeline chaining passing a prior job's `fileId` fails 400 "fileInput is required" across nearly the whole tool surface.

### A01-F04 — Redaction endpoints never remove underlying content — P1 (security-relevant)
Java prefers JPDFium `PdfRedactor` verified removal (`RedactController.java:222-302`); Rust always paints cover rectangles (`security.rs:1990-1999,2580-2589,2235-2262`). Only `wipePages`/`convertToImage` paths alter content. Redacted text remains extractable. Java-valid regex (lookaround/backrefs/possessive) → 400 (`security.rs:2017-2029,2610-2612`).

### A01-F05 — `validate-signature` lacks revocation checking and AATL/EUTL anchors — P2
Java `CertificateValidationService` does OCSP/CRL/AIA when `security.validation.revocation.mode`≠none; AATL/EUTL trust lists. Rust `signature.rs:7-13,1336-1352` reports `revocationChecked`/`revocationStatus` from settings with no network lookups → revoked certs report "good".

### A01-F06 — `verify-pdf`/`validate-compliance` depend on external `verapdf` CLI — P2
Java `VeraPDFService` uses bundled library. Rust `verapdf.rs` shells to CLI; absent → per-flavour error results with `compliant=false` (`security.rs:3000-3008`), WTPDF declared-but-unvalidated (`security.rs:2985-2999`), CLI-text parsing loses `ruleId`/`location`. Missing CLI makes `validate-compliance` fail every document.

### A01-F07 — Job status timestamps/queue semantics diverge — P3
Java `JobResult.createdAt/completedAt` ISO-8601; `queueInfo{inQueue,position}` when queued. Rust `jobs.rs:299-311` unix seconds, hardcoded `notes:[]`/`metadata:{}`, non-Java `allResultFiles`, no queue/progress.

### A01-F08 — Multipart/query parameter binding gaps — P3
Java `@ModelAttribute`/`@RequestParam` bind query+form alike; Rust `#[endpoint]` handlers read `params` and mostly ignore `query`. Consequences: `timestamp-pdf?tsaUrl=` ignored (`timestamp.rs:82-95`); `redact` pageNumbers/redactions via query ignored; Rust accepts `?async=true` on endpoints where Java has no async path (harmless divergence).

### A01-F09 — `timestamp.rs` leftover debug output — P3
`timestamp.rs:336-339` `eprintln!("DEBUG nonce mismatch…")` fires on every nonce-mismatch in production.

### A01-F10 — Minor response-header divergences — P3
Java `createContentDispositionHeader` emits `filename*` UTF-8 (`JobController.java:470-479`); Rust only `filename="..."` (`jobs.rs:369-372,440-447`). TSA read timeout 60s vs Java 30s.
