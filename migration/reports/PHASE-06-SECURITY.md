# Phase 06 — Java Security API migration audit

Scope: `app/core/.../controller/api/security/*` plus security-adjacent
endpoints elsewhere. Builds on agent-07's earlier pass
(`migration/reports/agent-07.md`), which implemented the password pair
and classified the crypto-heavy remainder. This phase adds two native
implementations and re-audits every endpoint against current `rust/`
dependencies.

## Endpoint classification

| Endpoint | Java controller | Classification |
|---|---|---|
| `POST /api/v1/security/add-password` | `PasswordController` | **Implemented** (agent-07; lopdf `EncryptionState`, RC4-40/AES-128/AES-256, `/P` permission bits) |
| `POST /api/v1/security/remove-password` | `PasswordController` | **Implemented** (agent-07; full strip vs. re-encrypt preserving `/P`, key length, cipher) |
| `POST /api/v1/security/prepare-pdf-session` | `PasswordController` | **Implemented but unverified** (this phase — see below) |
| `POST /api/v1/security/restore-pdf-protection` | `PasswordController` | **Requires external cryptographic integration** — JPDFium `PdfSecurity.copyEncryption` re-encrypts with the source's original key material (incl. unknown second password); lopdf cannot express "re-encrypt under a copied `/Encrypt` dictionary" |
| `POST /api/v1/security/redact` | `RedactController` + `ManualRedactionService` | **Implemented but unverified** (this phase — manual overlay subset; `convertPDFToImage=true` forwarded to Java) |
| `POST /api/v1/security/auto-redact` | `RedactController` + `TextRedactionService` | **Not safely implementable with current dependencies** — positioned text extraction + glyph-aware region removal; agent-05's `text.rs` covers extraction but the JPDFium `PdfRedactor` removal engine has no equivalent |
| `POST /api/v1/security/redact-execute` | `RedactController` + `RedactExecuteService` | **Not safely implementable with current dependencies** — text/regex/wipe/range/image-box strategies incl. JPDFium region removal and `IMAGE_FINALIZE` rasterisation |
| `POST /api/v1/security/timestamp-pdf` | `TimestampController` | **Requires external cryptographic integration** — RFC 3161 TSP + CMS `SignedData` validation + `/ByteRange` signature injection; no ASN.1/CMS crates in `Cargo.toml` (`rasn`/`cms`/`x509-*`/`pkcs7` all absent). `reqwest` covers only the TSA HTTP hop |
| `POST /api/v1/security/cert-sign` | `CertSignController` + `PdfSigningServiceImpl` | **Requires external cryptographic integration** — CMS/PKCS#7 `adbe.pkcs7.detached` generation, X.509 chain handling, PKCS#12/JKS/PEM/PKCS#11 key sources, incremental `/ByteRange` placement |
| `GET /api/v1/security/cert-sign/hardware/capabilities`, `GET .../windows-certificates`, `POST .../pkcs11-certificates`, hardware sign | `HardwareSigningController` | **Requires external cryptographic integration** — MSCAPI/Windows store and PKCS#11 token access; explicitly not simulated |
| `POST /api/v1/security/validate-signature` | `ValidateSignatureController` | **Requires external cryptographic integration** — CMS signature parse + PKIX chain validation + `/ByteRange` digest check |
| `POST /api/v1/security/verify-pdf` | `VerifyPDFController` | **Not safely implementable with current dependencies** — veraPDF validation profiles absent |
| `POST /api/v1/security/validate-compliance` | `ValidateComplianceController` | **Not safely implementable with current dependencies** — veraPDF absent |
| `POST /api/v1/security/sanitize-pdf` | (earlier migration) | **Implemented** (pre-existing `security::sanitize_pdf`) |
| `POST /api/v1/security/get-info-on-pdf` | `GetInfoOnPDF` | **Implemented** (pre-existing; `analysis::security_info` also exposes `/P` bits) |
| `POST /api/v1/security/remove-cert-sign` | (earlier migration) | **Implemented** (pre-existing) |
| `POST /api/v1/security/add-watermark` | `WatermarkController` | Partial — image mode native (`visual::add_watermark`, other agent); text/rasterising modes on Java |
| Signature *images* (`UIDataController` `/signatures`) | `UIDataController` | Out of scope — user signature images for forms, not PDF cryptography; Java-side regardless |

## What this phase implemented

### `prepare-pdf-session` (`passwords.rs`, route registered)

Java flow (`PasswordController.prepare`): load with the supplied
password → `PdfSessionPermissions.requireOperation(document, endpoint,
security.honorOwnerPasswords)` → `setAllSecurityToBeRemoved(true)` →
return the plaintext working copy under the original filename.

Port details:

- `endpoint` binds like Spring's `@RequestParam` — accepted from form
  fields or the query string; absent → 400.
- The endpoint allow-list and per-endpoint permission are reproduced
  exactly: rotate/remove-pages/rearrange/merge need `canAssembleDocument`
  (bit 11, `0x400`) except revision-2 encryption which substitutes
  `canModify` (bit 4, `0x8`); the five split endpoints plus
  `auto-split-pdf` need `canExtractContent` (bit 5, `0x10`);
  `compress-pdf` needs `canModify`. Bit masks verified against PDFBox
  3.0.8 `AccessPermission` (`1 << (bit-1)`). Unknown endpoints → 400
  before any decryption work matters.
- Signed protected PDFs → 403; user-authenticated documents lacking the
  required permission → 403 (`honorOwnerPasswords` defaults true, same
  as `ApplicationProperties.Security`).
- Output strips `/Encrypt` via the existing `strip_encryption` before
  `pdfdoc::save` — without it the trailer would keep naming a security
  handler over plaintext objects.
- Divergence: Java's `isOwnerAuthenticated` reflects which credential
  PDFBox recorded at load; the port re-tests owner role with
  `opening_credential` (the credential that actually opened the file),
  so a password valid as both roles counts as owner-authenticated.

### `redact` — manual subset (`security.rs`, route registered)

Java's `ManualRedactPdfRequest` path is, on inspection, **overlay-only**:
`ManualRedactionService.redactAreas`/`redactPages` append filled-rect
content (`re` + `f` under `q`/`Q`, `rg` non-stroking colour) — the
covered content is *not* removed by this endpoint (that promise belongs
to `/redact-execute`'s strategies). The port reproduces it faithfully:

- `redactions` JSON array (`RedactionArea`: `page` 1-based, `x`/`y`
  top-left origin, `width`/`height`, optional `color`); entries with
  missing page or non-positive size are skipped like Java; `page >
  pageCount` skipped; missing `x`/`y` → 400 (Java NPEs → 500).
- Coordinate flip uses `page.getBBox().getHeight()` semantics — the
  crop box height via `pdfdoc::page_boxes().crop` (crop clipped to
  media, media fallback — PDFBox does not clip; diverges only on
  malformed boxes exceeding the media box).
- `pageNumbers` uses the shared `page_select` port of
  `requireValidPageSelection`/`parsePageList`; absent/blank wipes
  nothing (the request model shadows the `"all"` default).
- `pageRedactionColor` and per-area `color` go through a
  `Color.decode`-equivalent parser (`#RRGGBB`/`#AARRGGBB`, alpha
  ignored, BLACK on null/parse failure).
- Output name `<base>_redacted.pdf`; `?async=true` flows through the
  shared job dispatch (`@AutoJobPostMapping`).
- `convertPDFToImage=true` → whole request forwarded verbatim to Java
  (rasterisation has no Rust renderer).

## Cryptographic dependency analysis

`rust/Cargo.toml` today: `lopdf 0.47` (standard security handler:
RC4/AES-128/AES-256 password algorithms, `EncryptionState`,
`Permissions`), `reqwest` (HTTP only), `rand`, `zip`, `image`. **No**
ASN.1/DER, CMS/PKCS#7, X.509, PKCS#12, PKCS#11, RSA/ECDSA, or SHA-2
direct dependencies (SHA-2 exists only transitively inside rustls).

- **Password/encryption**: fully covered by lopdf for the algorithms
  Java's `StandardProtectionPolicy` emits. Public-key security handlers
  (`/Encrypt` with `/SubFilter` other than `adbe.pkcs7.s3`/`adbe.pkcs7.s4`/
  `adbe.pkcs7.s5`) are not reachable through `EncryptionState` —
  `load_with_password` would fail on them, matching Java only when
  PDFBox also refuses.
- **Timestamping**: the only piece not needing a local private key —
  the TSA's CMS token is embedded opaquely. Still blocked on: DER
  `TimeStampReq`/`TimeStampResp` codec, `SignedData` validation, and
  incremental signature-dictionary + `/ByteRange` writing. Candidate
  path if pursued: `rasn` + `rasn-cms` family, or a minimal hand-rolled
  DER codec for the two small TSP messages — neither exists today.
- **Signing**: needs CMS `SignedData` *generation* plus private-key
  import from PEM/PKCS#12/JKS/PKCS#11 — the largest gap; no safe
  partial port exists.
- **Validation**: needs CMS parse + chain building + digest compare;
  veraPDF endpoints need the full validation profile engine — out of
  reach for lopdf.
- **Redaction**: the remaining gap is a glyph-aware content-stream
  rewriter (font encodings + ToUnicode + Form XObject recursion) or a
  JPDFium binding — a dedicated engine project, not a handler.
- **Hardware signing**: PKCS#11 (`cryptoki`/`pkcs11` crates) and
  Windows CNG/MSCAPI are platform integrations; the endpoints also
  carry a desktop-bundle locality check (`assertLocalDesktop`) that
  must be preserved if ever ported.

## Testing

`cargo test --bin stirling-server passwords` was attempted: the crate
currently fails to compile due to **33 errors, all in
`src/handlers/form.rs`** (another agent's in-progress work — `Widget`
Clone derives, borrow errors). Zero diagnostics were emitted for
`security.rs`, `passwords.rs`, `misc.rs`, or `fontmetrics.rs`, so the
new code type-checks, but no test binary could be produced and **no
runtime or parity claim is made**. `passwords.rs`'s existing unit tests
(encryption round-trips, permission bits, strip/re-encrypt) remain the
verification vehicle once the tree compiles.

Manual verification still owed: wrong-password 400s, owner-vs-user
403 paths on `prepare-pdf-session`, signed-PDF refusal, redaction
coordinate flip on crop-boxed pages, and `/redact` async job shape.

## Files changed this phase

- `rust/src/handlers/passwords.rs` — `prepare_pdf_session`,
  `require_operation`, `permission_allows`, `encryption_revision`,
  permission-bit constants; module doc updated.
- `rust/src/handlers/security.rs` — `redact`, `redact_work`,
  `RedactionArea`, `redact_color`, `last_param`; new imports.
- `rust/src/handlers/mod.rs` — routes for `prepare-pdf-session` and
  `redact`.

Formatting: `rustfmt --edition 2021 --check` clean on all touched
files.
