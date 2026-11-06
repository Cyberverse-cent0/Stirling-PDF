# Agent 07 — Security API: Redaction, Timestamping and Digital Signatures

Scope: `app/core/.../controller/api/security/*` — redact, timestamp-pdf,
cert-sign, hardware-signing, and the remaining signing/integrity
controllers (validate-signature, verify-pdf, validate-compliance,
password). Already migrated before this agent: `sanitize-pdf`,
`get-info-on-pdf`, `remove-cert-sign`.

## Classification

| Java controller / endpoint | Status |
|---|---|
| `PasswordController` `/add-password` | **complete** (subset, divergences below) |
| `PasswordController` `/remove-password` | **complete** (subset, divergences below) |
| `PasswordController` `/prepare-pdf-session`, `/restore-pdf-protection` | still Java (JPDFium `PdfSecurity.copyEncryption`) |
| `RedactController` `/redact`, `/auto-redact`, `/redact-execute` | **blocked** — native JPDFium engine |
| `TimestampController` `/timestamp-pdf` | **blocked** — RFC 3161/CMS stack absent |
| `CertSignController` `/cert-sign` | **blocked** — CMS/PKCS#7 + keystore stack absent |
| `HardwareSigningController` `/cert-sign/hardware/*` | **blocked** — platform keystore/PKCS#11 |
| `ValidateSignatureController` `/validate-signature` | **blocked** — CMS verify + PKIX absent |
| `VerifyPDFController` `/verify-pdf` | **blocked** — veraPDF absent |
| `ValidateComplianceController` `/validate-compliance` | **blocked** — veraPDF absent |
| `WatermarkController`, `StampController` | out of scope (content ops, other agents) |

Unmapped routes keep proxying to Java via the `/api/**` fallback — no
fabricated success paths were added.

## Password endpoints — implemented

`rust/src/handlers/passwords.rs`, routes in `rust/src/handlers/mod.rs`
(`POST /api/v1/security/add-password`, `POST /api/v1/security/remove-password`).
lopdf APIs verified against the vendored 0.47.0 source:

- `Document::load_mem_with_password` → `Reader::authenticate_and_setup_encryption`
  tries `""` then the supplied password, `PasswordRole::Either` (user or
  owner), hard `Error::InvalidPassword` on failure; objects are decrypted
  eagerly during the read, so `save` emits plaintext.
- `Document::encrypt(&EncryptionState)` refuses `AlreadyEncrypted`
  (trailer `/Encrypt` still present after a password load — removed by
  `strip_encryption`, which mirrors `Document::decrypt_raw`'s trailer +
  object removal).
- `EncryptionVersion::{V1,V2,V4,V5}` → `EncryptionState::try_from`;
  AES filters from `lopdf::encryption::crypt_filters`
  (`Aes128CryptFilter`/AESV2, `Aes256CryptFilter`/AESV3); V5 takes a
  caller-supplied 32-byte file key generated with `rand` (already in the
  lockfile via lopdf).

### `/add-password` (Java `PasswordController.addPassword`)

- Params preserved: `password` (user), `ownerPassword`, `sourcePassword`,
  `preserveExisting`, `keyLength` {40,128,256} default 256, and the eight
  `prevent*` flags.
- Cipher mapping matches PDFBox `StandardProtectionPolicy`:
  40→RC4-40 (V1/R2), 128→AES-128 (V4/R4, PDFBox's AES default),
  256→AES-256 (V5/R6, random FEK).
- `prevent*` → `lopdf::Permissions` (identical PDF bit positions as
  `AccessPermission`).
- `requireOwner` reproduced: `security.honorOwnerPasswords` defaults to
  **true** in `ApplicationProperties.Security`; encrypted sources need
  owner-role auth (`Document::authenticate_owner_password` on the
  credential that actually opened the file — `""` wins when it
  authenticates, matching loader order). Signed PDFs
  (`pdfdoc::signature_dict_exists`, new helper mirroring
  `PDDocument.getSignatureDictionaries`) are refused with 403 in the same
  cases Java calls `requireOwner`.
- Validation reproduced: restrictions require a nonempty owner password
  distinct from the opening password; both empty → 400; bad keyLength →
  400. Output suffix `_passworded.pdf` / `_permissions.pdf` as Java.

### `/remove-password` (Java `PasswordController.removePassword`)

- Default path: any valid credential opens (no owner requirement, as
  Java); `AccessPermission.isOwnerPermission()` verified from PDFBox
  30.8 sources = all eight permission bits (`/P & 0xF3C == 0xF3C`).
  - Owner-level `/P` → full strip → unencrypted `_password_removed.pdf`.
  - Restricted `/P` → re-encrypt preserving `/P` verbatim
    (`Permissions::from_bits_retain`), `/Length` (default 40 like
    `PDEncryption.getLength`), `/V >= 4` → AES, random 32-byte-hex owner
    password, empty user password — PDFBox's re-`protect` branch.
    RC4 intermediate lengths map to V2; AES only at 128/256.
- `removeAll=true`: `sourcePassword` (else `password`) credential,
  owner auth + signed-PDF refusal via `requireOwner`, then full strip.
- Selective fields (`ownerPassword`, `sourcePassword`,
  `retainedOpeningPassword`) outside `removeAll` → 400, not approximated.

### Known Java/Rust differences (password endpoints)

- `preserveExisting=true` on an **encrypted** source → 400 (retaining
  passwords/key length needs a re-encryption Java delegates to PDFBox;
  lopdf cannot express "keep the old user password"). On unencrypted
  input it behaves like a normal add.
- Java's post-write verification that opening/permissions passwords stay
  distinct after PDF password-encoding truncation is not replicated.
- `/EncryptMetadata` is always written `true` on new protection;
  PDFBox's default matches for the paths exercised.
- `honorOwnerPasswords` is a server setting; the Rust port hardcodes the
  default `true` (no `ApplicationProperties` equivalent yet).
- Selective removal (remove one of two passwords) is rejected — see above.

## Redaction — blocked, precise boundary

Java (`RedactController` + `ManualRedactionService` +
`RedactExecuteService` + `TextRedactionService` + JPDFium `PdfRedactor`):

- **auto-redact / redact-execute text**: JPDFium native engine —
  glyph-aware, ligature/bidi/grapheme-safe word & regex removal with
  per-page post-removal verification (`requireVerified`); falls back to
  PDFBox only for *box* redaction, and verification failures reject the
  request rather than returning unverified output.
- **manual /redact**: `ManualRedactionService` draws filled rects
  (`PDPageContentStream`), removes overlapping annotations, wipes pages,
  cleans document/XMP metadata; `convertPDFToImage` rasterizes. Genuine
  region content removal is again JPDFium; the PDFBox path is
  overlay-only and would violate this task's "no black-box redaction"
  rule, so it was **not** ported.
- **incremental semantics**: Java saves via `document.save` /
  JPDFium `result.save` — full rewrite, not incremental; original
  content is not retained in prior revisions.

Rust blocker: no glyph-aware content-stream redactor exists on lopdf.
Required integration: either (a) a Rust binding to the JPDFium native
redactor (the same engine Java calls), or (b) a content-stream rewriter
that maps text-showing operators (`Tj`/`TJ`/`'`/`"`) through font
encodings + ToUnicode CMaps, removes matched glyph runs, and recursively
handles Form XObjects — a substantial engine in itself, not a handler.

## Timestamping — blocked

`TimestampController.timestampPdf`: SHA-256 over the `/ByteRange`
contents → RFC 3161 `TimeStampRequest` (BouncyCastle DER: `certReq`,
nonce) → POST `application/timestamp-query` to an allowlisted TSA
(preset + admin-configured URLs, redirects disabled, 1 MB cap) →
`TimeStampResponse.validate` → embed CMS `TimeStampToken` as a
`/DocTimeStamp` signature with `/SubFilter /ETSI.RFC3161`, incremental
save.

Missing in Rust: ASN.1/DER codec, TSP request/response handling, CMS
`SignedData` validation, TSA HTTP client (reqwest exists), and
incremental signature-dictionary writing (lopdf `IncrementalDocument`
covers the container mechanics only). Boundary: a `tsa` module
(DER `TimeStampReq` build + `TimeStampResp`/`SignedData` verify) plus a
`/ByteRange` signature injector — neither exists in the dependency tree.

## Certificate signing — blocked

`CertSignController.signPDFWithCert` (+ `PdfSigningServiceImpl`):
certType PEM / PKCS12 / PFX / JKS / WINDOWS_STORE / PKCS11 → `KeyStore`
+ private key → PDFBox `doc.addSignature` producing CMS/PKCS#7
`adbe.pkcs7.detached` over `/ByteRange`, optional visible signature
widget (`SignatureOptions`), `/M` sign date, reason/location/name.

Missing: CMS `SignedData` generation (DER), X.509 chain handling,
PKCS#12/JKS/PEM private-key parsing and signing primitives, PKCS#11 /
Windows-store access, incremental `/ByteRange` signature placement.
No `cms`/`pkcs7`/`x509`/`pkcs12`/`p12` crates in `rust/Cargo.toml`.

## Hardware signing — blocked

`HardwareSigningController` (`/api/v1/security/cert-sign/hardware/*`):
`GET /capabilities`, `GET /windows-certificates` (MSCAPI/Windows store),
`POST /pkcs11-certificates` (SunPKCS11 JCA provider, PIN in body,
loopback+desktop gating via `assertLocalDesktop`).

Missing: platform keystore access (CryptoAPI / PKCS#11 C calls) and the
desktop-loopback trust model. No Rust equivalent; also inherently
host-specific.

## Signature verification / compliance — blocked

- `ValidateSignatureController`: CMS signature extraction, PKIX path
  building, time-of-stamping semantics, `/ByteRange` coverage analysis,
  RFC 3161 (PAdES-LTV) timestamp handling. Dictionary/ByteRange parsing
  is lopdf-feasible but the contract is cryptographic verification —
  partial output would fabricate trust.
- `VerifyPDFController`, `ValidateComplianceController`: veraPDF engine
  (XMP-driven PDF/A, PDF/UA, WTPDF detection + rule validation). No Rust
  equivalent — same gap already documented for `Compliancy` in
  `security.rs`.

## Files changed

- `rust/src/handlers/passwords.rs` — **new** (both endpoints).
- `rust/src/pdfdoc.rs` — `load` split into `load` + `load_with_password`
  over shared `finish_load`; new `signature_dict_exists` (+
  `signature_value_in_field`) mirroring `getSignatureDictionaries`.
- `rust/src/error.rs` — new `Forbidden` variant → HTTP 403 (Java's
  `ResponseStatusException(FORBIDDEN)` parity).
- `rust/src/handlers/mod.rs` — `pub mod passwords;` + two routes
  (additive only; concurrent agents' routes untouched).
- `rust/Cargo.toml` — `rand = "0.10"` (already in `Cargo.lock`).
- `migration/reports/agent-07.md` — this report.

## Cryptographic dependencies

- **Available**: lopdf `encryption` (`EncryptionState`,
  `EncryptionVersion`, `Permissions`, `crypt_filters`,
  `Document::encrypt`/`decrypt`/`load_mem_with_password`,
  `authenticate_{owner,user,password}`), `rand` for key/password
  generation, `reqwest` (could carry TSA calls once a TSP/CMS layer
  exists).
- **Missing**: CMS/PKCS#7 SignedData (sign+verify), RFC 3161 TSP
  client, X.509 path validation, PKCS#12/JKS/PEM key parsing,
  PKCS#11/Windows keystore, veraPDF, JPDFium native redaction.

## Route registrations required

Already registered in `mod.rs`:

```
POST /api/v1/security/add-password
POST /api/v1/security/remove-password
```

All other security paths intentionally remain on the Java proxy.

## Formatting verification

`rustfmt --edition 2021` run on `passwords.rs`, `error.rs`,
`pdfdoc.rs` (and `mod.rs` earlier); `--check` clean. No builds, tests,
clippy, or parity runs per restrictions. Concurrent edits by other
agents in `mod.rs`/`misc.rs`/`automation.rs` were preserved — only
additive changes were made to shared files.
