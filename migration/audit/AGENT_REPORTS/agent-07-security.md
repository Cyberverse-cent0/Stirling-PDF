# A07 — Security & Reliability Audit (round 2, 2026-11-07)

Scope: `rust/src` vs `app/` Java. Read-only source review; no exploitation.

## 1. Security posture summary

Faithful OSS-mode port. Present controls:
- `/api/**` open by default (OSS parity); `RUST_API_KEY` shared-key guard when set (`auth.rs:83-94`).
- Endpoint/group enablement enforced pre-handler via `api_guard`→`endpointconfig` (`guard.rs:37-50`, `mod.rs:384-388`).
- `url-to-pdf` disabled unless `system.enableUrlToPDF=true` (`settings.rs:48-52`, `endpointconfig.rs:474-476`).
- Multipart file parts stream to `NamedTempFile` with per-part cap (`upload.rs:38-60,191-211`).
- Engine invocations argv arrays, no shell (`convert.rs:183-195`).
- Static-file resolution rejects traversal/symlink escape (`web.rs:133-148`).
- Mobile-scanner uploads: explicit byte/file/session/global caps + filename validation (`automation.rs:514-520,794-816,1105-1203`).
- `reqwest` uses `rustls-tls` not `native-tls`.
- No password/API-key/token logging found in reviewed handlers.

Main risks: (1) filesystem safety — one confirmed path-traversal arbitrary-file-write; (2) resource exhaustion — engine calls lacking timeouts, incomplete upload/job/ZIP limits vs Java; (3) security-control parity — signature revocation false "good", HTML/SVG/Office sanitization gaps, predictable job IDs; (4) deployment/auth — proprietary/SaaS gets warning only.

## 2. Findings

### A07-F01 — Path traversal → arbitrary file write via uploaded filename — CONFIRMED · P0
`misc.rs:2739-2753` `BlankRasterizer::probe` writes uploaded PDF to `dir.path().join(format!("{name}.pdf"))` where `name` is unsanitized multipart `filename` (`upload.rs:167-170` stores `field.file_name()` raw). `std::fs::write` follows `..`/`/` → escapes tempdir. Callers passing raw `upload.name`: `misc.rs:2682` (remove-blanks), `scanner.rs:156` (scanner-effect), `automation.rs:1279` (auto-split-pdf), `image_scans.rs:177`, `ocr.rs:303`. Unauthenticated caller in default OSS-open deployment can write attacker PDF bytes to any writable path ending `.pdf` (configs/, install dirs, web-served dirs). **Coordinator-verified** sink + callers. Fix: fixed name `input.pdf` or `simple_name()` + reject `/`,`\`,`..`,control chars.

### A07-F02 — Engine subprocesses without timeout + unbounded output capture — P1
`misc.rs:717-783` (gs `.output()`), `misc.rs:857-875` (qpdf `.output()`), `automation.rs:441-486` (repair gs/qpdf `.output()`), `misc.rs:2741-2746,2817-2832` (gs probes/renders `.status()`), `convert.rs:196-256` (`EngineSpec` drains stdout/stderr unbounded `Vec<u8>`). Raw `Command::output()` buffers entire child output; no timeout on these call sites. Crafted PDF spinning gs/qpdf pins workers + spawns unbounded children → remote DoS. Fix: route all engine calls through `EngineSpec`/timeout wrapper; cap drained bytes; kill on cap.

### A07-F03 — Ghostscript without consistent sandbox flags — P1
Missing `-dSAFER`/`--permit-file-*`: `misc.rs:717-775` (compress gs), `misc.rs:2262-2275` (cmyk), `misc.rs:2817-2832` (`BlankRasterizer::render_gs`), `automation.rs:441-447` (repair), `ocr.rs:251-259` (post-OCR), `convert.rs:1547-1555` (CBZ ebook opt). Other sites do use flags → inconsistent. On gs builds where SAFER not default or PostScript file ops reachable, malicious PDF may read local files/execute via `%pipe%`. Java never passes `-dSAFER` (not parity regression; gs≥9.50 defaults SAFER) but inconsistency is a hardening defect. Fix: uniform `-dSAFER` or `--permit-file-*` on every gs invocation.

### A07-F04 — Predictable async job/file IDs enable IDOR — P1
`jobs.rs:157-170` `new_id()` timestamp+counter formatted as UUID; `jobs.rs:195,254` job/file IDs; `jobs.rs:339-505` status/result/files/download/metadata lookup by ID, no ownership. Java uses server-side job access checks + random UUIDs. Enumerable IDs → cross-tenant download of processed PDFs (shared `RUST_API_KEY` deployment) or unauthenticated (open deployment). Fix: CSPRNG UUIDv4 (`rand`/`uuid`/`getrandom`); bind jobs to principal or signed URLs if multi-user.

### A07-F05 — Signature validation reports revocation "good" without checking — P1
`signature.rs:8-10` documented divergence; `:1085-1090` `revocation_enabled`; `:1336-1352` `revocationChecked`/`revocationStatus`. When `security.validation.revocation.mode`≠none, Rust sets `revocationChecked=true` + `revocationStatus:"good"` for chain-valid sigs with NO OCSP/CRL/AIA lookup. Java `CertificateValidationService.java:181-213` real `PKIXRevocationChecker`. Revoked certs reported "good" → false assurance. Fix: implement OCSP/CRL with SSRF guardrails, or emit `revocationChecked:false`/`"not-checked"` when configured-but-unimplemented.

### A07-F06 — PKIX-lite path building omits several X.509 checks — P2
`signature.rs:609-695` `build_cert_path`: issuer/subject match, signature verify, CA bit, key usage, validity, AKID narrowing. No name constraints, policy constraints, `pathLenConstraint`, critical-extension handling, complete trust-anchor checks. Chains Java/PKIX rejects may validate `trustValid:true` in edge cases. Fix: document subset; consider webpki/fuller validation; enforce pathLenConstraint + critical-extension policy.

### A07-F07 — Residual SSRF + unbounded body in url-to-pdf — P2
`convert.rs:2135-2178` `url_to_pdf`; `:2180-2261` validation; `:2359-2381` `fetch_remote_html`. Top-level URL guarded (HTTP(S) only, no redirects, DNS private/reserved rejection, timeouts) BUT: WeasyPrint `--base-url <remote>` + `--allowed-protocols data,http,https` → fetched HTML pulls internal HTTP(S) subresources never revalidated; `resp.bytes()` no max body (20s timeout bounds time not bytes). Endpoint disabled by default. Fix: subresource filtering/proxying, body cap, keep disabled by default.

### A07-F08 — No total request-size, part-count, or concurrency limits — P1
`upload.rs:14-27` per-part cap only; `:63-80` text params buffered to same cap; `:191-211,217-240` unlimited field count; `main.rs:98-149` no `RequestBodyLimitLayer`/`ConcurrencyLimitLayer`. Java `MultipartConfiguration.java:35-80` sets `maxFileSize` AND `maxRequestSize`. Many near-cap parts or many small parts → disk/fd/memory exhaustion. Regression vs Java. Fix: total body cap, part-count cap, concurrency limits (`tower_http::limit`).

### A07-F09 — Async job results unbounded, retained indefinitely, no ownership — P1
`jobs.rs:191-288` unbounded `tokio::spawn`, `to_bytes(usize::MAX)`, `StoredFile{Vec<u8>}`; `:452-485` cleanup only via explicit endpoint; `jobstore.rs` SQLite blobs. No concurrency semaphore, no result-size cap, no TTL, no ownership. Repeated async jobs exhaust memory/disk/SQLite; sensitive PDFs persist until manual cleanup; enumerable IDs (F04). Duplicates A06-F01/F02/F04/F08/F09 — consolidated there.

### A07-F10 — ZIP decompression lacks size/entry limits in several paths — P1
`pipeline.rs:186-220` recursive ZIP extraction depth-cap only; `convert.rs:1494-1530` CBZ entries fully into memory; `convert.rs:1960-2008` HTML ZIP all entries into memory. Contrast `convert.rs:2691-2705` Markdown ZIP HAS caps (`ZIP_MAX_ENTRY_BYTES`, `ZIP_MAX_TOTAL_BYTES`, `ZIP_MAX_ENTRIES`) — intended guard exists, not applied consistently. Java `FileToPdf.java:119-136` uses `ZipBombGuard.Budget`. Zip bomb reachable via upload endpoints in open deployment. Fix: shared ZipBombGuard-equivalent budget on pipeline/CBZ/HTML-ZIP/EPUB/CBR paths.

### A07-F11 — HTML sanitization not ported for HTML/Markdown→PDF — P2
`convert.rs:1891-1924` comment "sanitisation is not ported"; `convert.rs:1936-1947` WeasyPrint `--allowed-protocols data`; `convert.rs:2613-2619,2641-2664` comrak `render.unsafe = true`. Java sanitizes via OWASP allowlist (`FileToPdf.java:60-115`, `CustomHtmlSanitizer.java:55-83`) + SSRF-safe image URL checks. Dangerous HTML/forms/links/data URIs reach renderer. `--allowed-protocols data` blocks remote subresources for html-to-pdf; markdown uses `data` unless `disableSanitize`. WeasyPrint exploitability untested. Fix: port sanitizer or document accepted risk; `disableSanitize` admin-only.

### A07-F12 — SVG sanitization weaker than Java's parser-based sanitizer — P2
`convert.rs:1246-1281` substring marker rejection (lowercase `<script`, `javascript:`, `<foreignobject`, `<iframe`, `<object`, `<embed`, `vbscript:`). Java `SvgSanitizer.java:42-52,84-99,136-168,189-255` parses XML securely, strips dangerous elements/attrs, normalizes URL encoding, removes event handlers, SSRF URL checks. Obfuscated/parser-level variants may bypass. Renderer external-resource behavior unverified. Fix: real XML/SVG sanitizer or strict allowlist; verify renderer network behavior.

### A07-F13 — `frontendUrl` trusts Host/X-Forwarded-* unconditionally — P2
`platform.rs:667-735` `resolve_frontend_url` honors `X-Forwarded-Proto/Host/Port` + `Host` with no trusted-proxy check. Direct client poisons `frontendUrl` in `/api/v1/config/app-config` → phishing/callback surfaces. Fix: honor forwarded headers only behind trusted-proxy setting; else configured `system.frontendUrl`/validated Host.

### A07-F14 — `RUST_API_KEY` sent plaintext HTTP to non-loopback bind — P2
`pipeline.rs:240-253` `self_base_url`; `:368-398` pipeline self-dispatch attaches `x-api-key`. If `HOST` is LAN/public IP, API key transmitted cleartext. Fix: always `127.0.0.1` for loopback dispatch, or Unix socket/HTTPS.

### A07-F15 — Open-by-default auth; `RUST_API_KEY` single shared key — P1 secured / P3 OSS
`auth.rs:1-16,44-51,73-94,98-117`. No users/teams/sessions/JWT/OAuth/SAML/license/per-endpoint authz. Proprietary/SaaS markers → startup warning only. Shared key, no rate limiting, non-constant-time compare. Fix: refuse non-loopback bind or require key when security markers present; constant-time compare; rate limiting; document Java-in-front.

### A07-F16 — `/actuator/` + health exemptions broader than needed — P3 (latent)
`auth.rs:44-51` exempts any `/actuator/` prefix + `/health`,`/healthz`,`/liveness`,`/readiness`. Currently 404 via SPA fallback — no present exploit; future actuator routes unauthenticated by default. Fix: exact-path matching.

### A07-F17 — `Content-Disposition` filenames not sanitized/RFC 5987 — P3
`jobs.rs:371,444`; `automation.rs:1198`; `merge.rs:105-113`. Raw interpolation into `filename="..."`; quotes/semicolons → malformed dispositions; no `filename*` UTF-8. CR/LF can't inject (HeaderValue rejects). Fix: strip `"`/`;`/controls or RFC 5987 encode.

### A07-F18 — Error responses reflect internals/params — P3
`pipeline.rs:479-487` echoes invalid operation parameters; `convert.rs:176-181` reveals engine binary names; `signature.rs` cert DNs/OIDs in errors. If params contain secrets → reflected. Fix: redact `password|secret|token|key` keys; don't echo full param maps.

### A07-F19 — Production debug output bypasses tracing — P3
`timestamp.rs:336-339` `eprintln!("DEBUG nonce mismatch…")` on every nonce-mismatch. Fix: `tracing::debug!` or remove.

### A07-F20 — Legacy/weak crypto paths retained for compatibility — P3
`passwords.rs:197-223` `keyLength` 40/128/256 incl. RC4-40 (Java parity); `certsign.rs:20-23` PKCS#12 MAC not verified; `des`/`rc2`/`sha1`/`pkcs12` legacy crates. Inherited compat risk. Fix: warn/deprecate 40-bit/RC4; verify PKCS#12 MAC; `cargo audit`/`deny` in CI.

### A07-F21 — Reliability bug: qpdf level ≥8 passes `env` as qpdf argument — P3
`misc.rs:857-863`: intends `env QPDF_ZOPFLI=silent qpdf ...` but builds `Command::new(qpdf)` then `.arg("env").arg("QPDF_ZOPFLI=silent")` → qpdf receives invalid leading args → level≥8 compression likely fails on non-Windows. Fix: `Command::new("env").arg("QPDF_ZOPFLI=silent").arg(qpdf)` or set env on child.

## 3. UNVERIFIED items

- **U01** Dependency vuln scan — run `cargo audit`/`cargo deny` on `rust/Cargo.lock` (lopdf 0.47.0, zip 2.6.1, image 0.25.10, reqwest 0.12.28, rustls 0.23.46, hayro 0.9.0, mail-parser 1.0.1, cms 0.2.3, rsa 0.9.10, pkcs12 0.1.0). Known-good: rustls-tls, no native-tls.
- **U02** lopdf stream decompression bounds — `pdfdoc.rs` caps object streams 64 MiB (`:63`), some page content 64 MiB (`:906-914`); full lopdf 0.47.0 audit not done.
- **U03** Java `FileValidationService` parity — class not found; Java relies on per-controller extension checks + `MultipartConfiguration` limits.
- **U04** Office sanitizer parity — Java `OfficeDocumentSanitizer` exists but no production call site found; Rust `office_to_pdf` delegates to LibreOffice without equivalent (`convert.rs:1631-1726`). Determine if Java invokes via DI/AOP; test LibreOffice external-resource fetching.
- **U05** SVG renderer external-resource behavior — marker check weaker than Java; verify whether rsvg-convert/inkscape fetch external hrefs or honor data:/javascript: URLs.
- **U06** Proprietary/SaaS authz parity for Rust-owned admin endpoints — map Java `SecurityConfiguration`/admin controllers to Rust routes; confirm intended topology.
- **U07** Unsanitized HTML exploitability in WeasyPrint — missing sanitizer confirmed; renderer-specific tests needed (forms, javascript:/data: links, embedded SVG, CSS url() under each `--allowed-protocols`).

## 4. Verified acceptable / lower risk

- Static files: `web.rs:133-148` rejects `..`, backslashes, dirs, non-canonical escapes; symlink escape blocked.
- Mobile scanner: session IDs + download filenames constrained; per-file/session/global byte caps.
- ZIP output naming: `simple_name()` basename handling (`attachments.rs:340-378`).
- SQL: `jobstore.rs` parameterized statements.
- Regex: linear-time `regex` crate; no ReDoS backtracking engine.
- Timestamping: TSA URLs allowlisted, redirects disabled, response ≤1 MiB, only doc hash sent (`timestamp.rs:44-140`).
- Signature primitives: RSA PKCS#1 v1.5, RSA-PSS, ECDSA P-256/P-384, Ed25519 with explicit algorithm handling.
- Password handling: no password logging found.
- CORS: mirrors Java method/header/origin-pattern incl. credentials + wildcard fallback (`main.rs:99-129`, `config.rs:35-98`).
- Endpoint guards: disabled endpoints/groups enforced pre-handler; url-to-pdf off by default.

## 5. Priority actions

1. **P0:** Fix `BlankRasterizer::probe` filename traversal (`misc.rs:2751-2753`) — verified.
2. **P1:** Timeouts + bounded output on all direct `Command` engine calls; route through `EngineSpec`.
3. **P1:** ZIP-bomb budgets consistently (pipeline, CBZ, HTML-ZIP, EPUB/CBR).
4. **P1:** Total request-size, part-count, concurrency limits.
5. **P1:** CSPRNG job/file IDs + ownership/retention controls.
6. **P1:** Signature revocation truthfulness (implement or report not-checked).
7. **P1:** Fail-closed for secured deployments without `RUST_API_KEY`.
8. **P2:** HTML/SVG/Office sanitization; bound url-to-pdf subresources + body size.
9. **P2:** Gate X-Forwarded-* trust; loopback-only internal dispatch.
10. **P3:** `cargo audit`/`deny`, remove `eprintln!`, sanitize Content-Disposition, redact echoed params, fix qpdf `env` bug.
