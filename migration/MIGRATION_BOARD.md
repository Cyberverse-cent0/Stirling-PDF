# Migration Board — remaining work

Authoritative list of what is left after the Rust cut-over. Java fallback,
Java parity and proxying are **not** requirements; acceptance is Rust tests,
real fixtures, runtime smoke tests and output validation. Proprietary/SaaS
services (~74 controllers) stay out of scope.

## Done (do not re-open)

- All `/api/v1/**` PDF-engine endpoints routed natively except the blocks
  below; `handlers/proxy` and `JAVA_BACKEND_URL` are deleted.
- Conversions: html/markdown/url/eml/ebook→pdf, img/cbz/cbr→pdf,
  pdf→word/presentation/xml/text/epub/csv/xlsx/pdfa/markdown/json/rtf/cbz.
- Tables: `src/tabula.rs` ruling-lattice **+ borderless text-edge**
  detection → `pdf/csv`, `pdf/xlsx`.
- Pipeline `handleData`, app-config, engine probing/group gates, jobs
  (SQLite write-through), web/SPA layer, forms, security redact/sanitize/
  passwords, ocr-pdf, extract-image-scans, scanner-effect, remove-blanks,
  auto-split.

## Open — implementable now

| # | Feature | Blocker / notes |
|---|---------|-----------------|
| 1 | `POST /security/validate-compliance` | veraPDF CLI — **in progress** (this branch) |
| 2 | `POST /security/verify-pdf` | veraPDF CLI auto-detect — **in progress** |
| 3 | `POST /security/validate-signature` | needs CMS/PKCS7 verify (`cms`+`x509-cert` crates); fixtures exist (`test_globalsign.pdf`, `test_irs_signed.pdf`) |
| 4 | `POST /security/timestamp-pdf` | RFC 3161 TSA client + CMS timestamp embed — largest crypto piece |
| 5 | `POST /security/cert-sign`, `/security/sign`, `hardware-signing`, `restore-pdf-protection` | PKCS#12 + CMS signing / PIV — needs `cryptoki` or software-PKCS11; dedicated crypto project |
| 6 | `pdf-to-ua`, `accessibility-report` | check Java controllers — may be veraPDF-flavoured or metadata surgery |
| 7 | `pdf/video` | **upstream-disabled**: Java commented the endpoint out over ffmpeg CVEs — do not port unless policy changes |
| 8 | `useStirlingOfficeConvert` branches (pdf/xlsx, pdf/epub, pdf/markdown, pdf/pdfa) | needs the Stirling Office Convert engine — out of scope, returns 400 |

## Open — not code

- ~96 unreachable bean classes: deletion pending DI-aware review
  (`BEAN-REACHABILITY-AUDIT.md`).
- `rust/engine` vs `engine/` duplication decision.
- Phase G build cleanup + CI Java-job removal (after F).
- Deployment docs: sidecar list (`gs`, `soffice`, `weasyprint`, `tesseract`,
  `ocrmypdf`, `rar`, `magick`, `ebook-convert`, optional `verapdf`, `ffmpeg`
  intentionally absent).
