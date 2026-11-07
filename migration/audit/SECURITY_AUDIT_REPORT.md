# SECURITY AUDIT REPORT

Source: agent-07 (primary), cross-checked with agent-03 (engines) and agent-06 (jobs). Defensive review only — no exploitation. Verified-against tree: 2026-11-07 working tree (mid-flight; re-verify after stabilization).

## 1. Posture summary

Faithful OSS-mode port with real controls: open-by-default `/api/**` (parity), optional `RUST_API_KEY`, endpoint/group enablement enforced pre-handler, url-to-pdf disabled by default, multipart streams to temp files with per-part cap, all `EngineSpec` invocations argv-only, static files reject traversal, mobile-scanner has real byte/file/session caps, parameterized SQL, linear-time regex, rustls-tls, TSA allowlist+size cap, no secret logging found.

Main risk clusters: (1) one confirmed filesystem-safety P0; (2) resource-exhaustion gaps vs Java (timeouts, size limits, zip budgets, job bounds); (3) security-control parity gaps (revocation fabrication, sanitization, predictable IDs); (4) fail-open deployment/auth model.

## 2. Confirmed findings

### P0

**A07-F01 — Path traversal → arbitrary file write** · CONFIRMED (coordinator-verified)
`BlankRasterizer::probe` (`rust/src/handlers/misc.rs:2751-2753`) writes `dir.path().join(format!("{name}.pdf"))` where `name` is the raw multipart filename (`upload.rs:167-170` stores `field.file_name()` unsanitized). `std::fs::write` follows `..`/absolute components → attacker PDF bytes written anywhere writable ending `.pdf`. Reachable via remove-blanks (`misc.rs:2682`), scanner-effect (`scanner.rs:156`), auto-split (`automation.rs:1279`), extract-image-scans (`image_scans.rs:177`), OCR raster fallback (`ocr.rs:303`). Unauthenticated in default OSS-open deployment.
**Fix:** fixed name (`input.pdf`) or `simple_name()` basename + reject `/`,`\`,`..`,control chars. Add regression test with `../../evil` filename.

### P1

| ID | Finding | Evidence | Mitigation |
|---|---|---|---|
| A07-F02 | Engine subprocesses without timeout + unbounded output: `misc.rs:717-783` (gs compress), `857-875` (qpdf), `automation.rs:441-486` (repair), `misc.rs:2741-2832` (gs probes/renders `.status()`), `ocr.rs:251-259`, `convert.rs:1547-1555`; `convert.rs:196-256` drains stdout/stderr unbounded | hostile PDF spins gs/qpdf → worker/process DoS | route all through `EngineSpec`; cap drained bytes; kill on cap |
| A07-F03 | `-dSAFER`/`--permit-file-*` inconsistent: absent at `misc.rs:717,2262,2817`, `automation.rs:441`, `ocr.rs:251`, `convert.rs:1547,1128,2893,3610,3623`; present at `convert.rs:668,893,3082,3152`, `misc.rs:349`, `security.rs:2139`. (Java never passed it; gs≥9.50 defaults SAFER — inconsistency is hardening defect, not parity regression) | uniform `-dSAFER` or explicit `--permit-file-*` on every gs call |
| A07-F04 | Predictable job/file IDs: `new_id()` timestamp+counter formatted as UUID (`jobs.rs:157-170`); lookups by ID with no ownership (`jobs.rs:339-505`) → enumerable IDOR; also collide across restarts | CSPRNG UUIDv4 (`rand`/`uuid`/`getrandom` already deps); owner binding or signed URLs if multi-user |
| A07-F05 | Signature revocation fabricated: `signature.rs:1336-1352` sets `revocationChecked:true`, `revocationStatus:"good"` when revocation mode enabled — **no OCSP/CRL/AIA lookup ever happens**. Revoked certs report "good" | implement OCSP/CRL with SSRF guardrails, or emit `revocationChecked:false`/`"not-checked"` |
| A07-F08 | No total request-size, part-count, or concurrency limits — per-part cap only (`upload.rs:14-27`); no `RequestBodyLimitLayer`/`ConcurrencyLimitLayer` (`main.rs:98-151`). Java had `maxFileSize`+`maxRequestSize` (`MultipartConfiguration.java:35-80`) — **regression** | tower-http limit layers; aggregate budgets |
| A07-F09 | Async jobs: unbounded spawn, `to_bytes(usize::MAX)`, results in RAM+SQLite forever, no ownership, no TTL — see A06 consolidated | semaphore+timeout+retention+disk-backed results (A06) |
| A07-F10 | ZIP decompression lacks size/entry limits in pipeline (`pipeline.rs:186-220`), CBZ (`convert.rs:1494-1530`), HTML-ZIP (`1960-2008`) — but Markdown ZIP HAS caps (`2691-2705`): guard exists, applied inconsistently. Java `FileToPdf` used `ZipBombGuard.Budget` | shared zip-bomb budget on all extraction paths |
| A07-F15 | Open-by-default auth; `RUST_API_KEY` single shared key, ordinary `==` compare (non-constant-time; engine uses `subtle` — inconsistent), no rate limiting; proprietary/SaaS markers → startup warning only | fail-closed when security markers present or non-loopback bind; constant-time compare; document Java-in-front requirement |
| A06-03/04/08 | Job cancel doesn't abort work (no JoinHandle/AbortHandle); completion path writes orphan `file_data` BLOB + RAM `StoredFile` unreachable by cleanup; panicking job leaves permanently incomplete record (no catch_unwind) | AbortHandle + existence-gate before storing results; catch_unwind; match Java mark-error cancel semantics |
| A03-F06 | Untrusted archive extraction via unrar/unar/bsdtar/7z with no post-extraction containment check; `unar -f` force-overwrite; binaries unpinned | verify extracted paths stay under dest; pin engine versions |

### P2

| ID | Finding | Mitigation |
|---|---|---|
| A07-F06 | PKIX-lite path building omits name/policy constraints, pathLenConstraint, critical-extension policy (`signature.rs:609-695`) — chains Java/PKIX rejects may validate | document subset; enforce pathLen + critical-ext policy; consider webpki |
| A07-F07 | url→pdf residual SSRF: WeasyPrint `--base-url` + `--allowed-protocols data,http,https` lets fetched HTML pull internal subresources never revalidated; `fetch_remote_html` unbounded body; DNS check→connect TOCTOU (`convert.rs:2135-2381`) | subresource filtering/proxy; body cap; keep disabled by default |
| A07-F11 | HTML sanitization not ported: WeasyPrint gets raw HTML (`convert.rs:1891-1947`), comrak `render.unsafe=true` (`2613-2664`); Java had OWASP allowlist + SSRF-safe image checks | port sanitizer or document accepted risk; `disableSanitize` admin-only |
| A07-F12 | SVG sanitization = lowercase substring markers (`convert.rs:1246-1281`) vs Java parser-based `SvgSanitizer` | real XML/SVG sanitizer or strict allowlist; verify renderer network behavior |
| A07-F13 | `frontendUrl` honors `X-Forwarded-Proto/Host/Port` + `Host` with no trusted-proxy check (`platform.rs:667-735`) → response poisoning | honor forwarded headers only behind trusted-proxy setting |
| A07-F14 | `RUST_API_KEY` sent plaintext HTTP on pipeline self-dispatch when bound non-loopback (`pipeline.rs:240-253,368-398`) | always loopback for internal dispatch or Unix socket/HTTPS |
| A04-F08 | Settings YAML failure → silent defaults (security flags silently reset); bad PORT→silent 8081; bad HOST→panic | warn/fail-fast; validate HOST/PORT returning Err |
| A06-09 | No ownership checks on job/file routes — acceptable only behind Java edge; IDOR if exposed directly | document "Java-in-front only" or implement owner claim |
| A04-F14 | API-key compare not constant-time (`auth.rs:73-94`) | `subtle::ConstantTimeEq` (already used in engine) |

### P3

| ID | Finding |
|---|---|
| A07-F16 | `/actuator/` prefix + health exemptions broader than needed (`auth.rs:44-51`) — currently 404, latent if actuators added; use exact-path matching |
| A07-F17 | `Content-Disposition` filenames raw-interpolated, no `filename*` UTF-8 (`jobs.rs:371,444`; `automation.rs:1198`; `merge.rs:105-113`) — strip `"`/`;`/controls or RFC 5987 encode |
| A07-F18 | Error responses reflect internals/params: pipeline echoes invalid op params (`pipeline.rs:479-487`); convert reveals engine binary names; signature errors include cert DNs/OIDs — redact secret-looking keys |
| A07-F19 | `timestamp.rs:336-339` `eprintln!("DEBUG nonce mismatch…")` on every nonce-mismatch in production → `tracing::debug!` |
| A07-F20 | Legacy/weak crypto retained for parity: RC4-40/AES-128 password options (`passwords.rs:197-223`), PKCS#12 MAC unverified (`certsign.rs:20-23`), des/rc2/sha1 crates — warn/deprecate; `cargo audit`/`deny` in CI |
| A07-F21 | qpdf level≥8 bug: literal `env`/`QPDF_ZOPFLI=silent` passed as qpdf argv (`misc.rs:857-863`) → level≥8 compression fails on Unix — also correctness bug (see A03) |
| A09 | No `cargo audit`/`deny` in CI; dependency vuln scan never run (UNVERIFIED — lopdf 0.47, zip 2.6.1, image 0.25.10, reqwest 0.12.28, rustls 0.23.46, hayro 0.9, mail-parser 1.0.1, cms 0.2.3, rsa 0.9.10, pkcs12 0.1) |
| A04-F15 | Engine `config_cache.rs:11` unconditional Unix `PermissionsExt` import → Windows engine build break; `:157` non-constant-time HMAC compare |
| A04-F18 | CLI `spawn_detached` unconditional Unix `CommandExt` import → Windows CLI compile break (`cli/src/services/process.rs:108`) |
| A04-F19 | CLI component names unsanitized into paths; systemd `ExecStart=`/`WorkingDirectory=` unquoted (`filesystem.rs:31-33`, `runtime.rs:30-103`) |

## 3. Java-parity security notes

- Java never used `-dSAFER` either (not a regression) but Rust is internally inconsistent.
- RC4-40/AES-128 password options mirror Java's PDFBox choices — inherited weakness, not new.
- Java `FileValidationService` not found; relied on per-controller extension checks + multipart limits — Rust's per-part cap is roughly parity, missing only aggregate/request caps.
- `OfficeDocumentSanitizer` exists in Java but no production call site found — Rust LibreOffice delegation may be parity; UNVERIFIED.

## 4. UNVERIFIED security items (need targeted tests)

- U01 `cargo audit`/`deny` on Cargo.lock.
- U02 lopdf 0.47 stream decompression bounds beyond the 64 MiB caps found.
- U04 Office sanitizer parity / LibreOffice external-resource fetching.
- U05 SVG renderer (rsvg-convert/inkscape) external-resource/`javascript:`/`data:` behavior.
- U06 Proprietary/SaaS authz parity for Rust-owned admin endpoints (topology decision).
- U07 WeasyPrint exploitability surface under each `--allowed-protocols` setting.

## 5. Verified-acceptable controls (do not regress)

Static-file traversal/symlink guard (`web.rs:133-148`); mobile-scanner caps + filename validation; ZIP *output* `simple_name()`; parameterized SQL; linear-time regex; TSA allowlist + redirect-off + ≤1 MiB + hash-only; RSA-PSS/ECDSA/Ed25519 explicit handling; no password/key logging; CORS mirrors Java incl. credentials+wildcard fallback; endpoint guards enforced pre-handler; url-to-pdf off by default; argv-array exec everywhere (no shell interpolation found).
