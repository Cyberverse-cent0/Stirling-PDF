# FUNCTIONALITY PARITY MATRIX — Java vs Rust

Compiled from agent-01 (endpoint surface) and agent-02 (behavioral verification), re-checked against the working tree on 2026-11-07. "Java" column refers to the pre-deletion implementation (commit `67001f296e^`) plus surviving controllers at HEAD.

Classifications: COMPLETE / PARTIAL / STUB / MISSING / BROKEN / DIVERGENT / DEPRECATED / PROPRIETARY / UNVERIFIED.

## A. Endpoint surface parity

| Java endpoint family | Java status | Rust status | Class |
|---|---|---|---|
| general page ops (rotate, remove/rearrange pages, split family, scale, layout, booklet, TOC, bookmarks, page-numbers, images, comments, replace/invert) | ~30 endpoints | routed, native lopdf | mostly COMPLETE; crop/flatten/text-stamp divergences below |
| misc ops (compress, decompress, metadata, JS, blanks, attachments/portfolio, unlock-forms, image ops, scanner-effect, mobile-scanner ×6, repair, cmyk, auto-rename) | ~25 endpoints | routed | COMPLETE/PARTIAL per-feature below |
| analysis (8) / filter (6) | 14 endpoints | routed | COMPLETE |
| form (9) | 9 endpoints | routed, `file` field (not `fileInput`) | COMPLETE (minor gaps) |
| security password family (add/remove-password, prepare/restore session) | 4 endpoints | routed incl. restore (in-flight) | COMPLETE |
| security crypto: cert-sign + 3 hardware-signing | 4 endpoints | **no routes**, guard-403 | **MISSING** |
| security: verify-pdf, validate-compliance, validate-signature, timestamp-pdf, redact ×3, sanitize, get-info, remove-cert-sign, add/remove-watermark, add-stamp, accessibility-report | ~14 endpoints | all routed (timestamp/signature/accessibility in-flight) | PARTIAL — see divergences |
| convert family (~25): img/pdf, office, html/markdown/url/eml, vector, pdf→word/presentation/xml/text/html/csv/xlsx/epub, cbz/cbr, ebook | ~25 endpoints | routed via EngineSpec | COMPLETE/PARTIAL per engine deps |
| convert/pdf/markdown | 1 endpoint | routed (pure pdftext) | PARTIAL (`useStirlingOfficeConvert` rejected) |
| convert/pdf/ua | proprietary only | no route | PROPRIETARY |
| misc/print-file | 1 endpoint | no route | **MISSING** |
| convert/pdf/video | commented-out Java (ffmpeg CVEs) | no route | DEPRECATED |
| jobs/files framework (`/general/job/*`, `/general/files/*`, `/jobs/cleanup`) | JobController | routed, SQLite-backed | PARTIAL (semantics diverge — A06) |
| platform GETs (app-config, ui-data/*, group/endpoint enabled, settings) | various | routed, shapes verified vs frontend | COMPLETE |
| pipeline handleData | proprietary pipeline | routed, loopback dispatch | PARTIAL (documented divergences) |
| proprietary/saas (~74 controllers, ~250 frontend call sites) | full platform | **zero Rust routes** | PROPRIETARY — Java-only by design |
| `/v3/api-docs` (Swagger) | springdoc | **absent** → SwaggerUI 404 | MISSING |

## B. Feature-level behavioral parity (verified, not just routed)

| Feature | Rust impl | vs Java | Class | Priority |
|---|---|---|---|---|
| crop | `transform.rs:215-236` rejects `autoCrop`, `removeDataOutsideCrop`; absent param → 400 | Java GS clip+remove-data, frontend never sends the param | **BROKEN for UI default** | P0-functional |
| flatten | `misc.rs:290-311` rejects unless `flattenOnlyForms`; raster impl dead code | Java rasterised all pages by default; frontend defaults flattenOnlyForms=false | **BROKEN for UI default** | P1 |
| merge-pdfs | `merge.rs:50-62` rejects `generateToc`, `removeCertSign=false` | Java generated TOC, kept signatures; frontend sends `removeCertSign:false` default | **BROKEN for UI default** | P1 |
| redaction (manual/auto/execute) | overlay rects only, `security.rs` | Java preferred JPDFium verified content removal; text stays extractable | DIVERGENT (security) | P1 |
| get-info-on-pdf | `security.rs:473-480` hardcodes all compliance flags false | Java populated via veraPDF lib | DIVERGENT | P2 |
| verify-pdf / validate-compliance | veraPDF **CLI** (`verapdf.rs`); absent → all non-compliant / 422 | Java bundled veraPDF library always present | PARTIAL | P2 |
| validate-signature | CMS/PKCS#7 verify real; **no OCSP/CRL/AIA**; reports revocation "good" | Java PKIXRevocationChecker + AATL/EUTL | PARTIAL (security) | P1 |
| timestamp-pdf | native RFC 3161 + incremental DocTimeStamp (`timestamp.rs`, in-flight) | Java equivalent | COMPLETE-ish | — |
| restore-pdf-protection | native re-encryption (`passwords.rs:616+`, in-flight, uncommitted) | JPDFium copyEncryption | UNVERIFIED→likely COMPLETE | — |
| cert-sign / hardware signing | none | full Java impl (PEM/PKCS12/JKS/PKCS11/Windows store) | **MISSING** | P1 |
| add/remove-password | lopdf RC4-40/AES-128/AES-256; signed PDFs refused; selective ops rejected | PDFBox broader | PARTIAL | P2 |
| compress-pdf | GS pdfwrite+qpdf+imgcomp; `lineArt` rejected; **qpdf env-arg bug at level≥8** (`misc.rs:857`) | Java + ImageMagick lineArt | PARTIAL/BROKEN edge | P2 |
| pdf→img / pdf→cbz | Ghostscript only, no hayro fallback, **ungated** (not in probed groups) | Java bundled JPDFium | PARTIAL | P1 |
| svg→pdf | rsvg-convert raster preferred, inkscape fallback; unprobed/ungated | Java Batik vector | DIVERGENT | P2 |
| cbr→pdf | unrar/unar/bsdtar/7z — none probed; enabled→500 | Java junrar in-process | PARTIAL | P2 |
| eml→pdf | mail-parser+WeasyPrint; `.msg` rejected; no FileAttachment annots | Java fuller MIME+PDFBox | PARTIAL | P2 |
| ebook↔pdf/epub | Calibre; `.docx` ebook & StirlingOfficeConvert rejected | Java + OfficeConvert engine | PARTIAL | P2 |
| auto-rotate-pdf | text-direction native; `detectionMode=osd` rejected | Java ran Tesseract OSD | PARTIAL | P2 |
| extract-images | native; JPX/CCITT/JBIG2/Indexed/ICCBased/SMask → whole-request 400 | Java ImageIO full codecs | PARTIAL | P2 |
| img→pdf | `image`-crate codecs only | Java ImageIO | PARTIAL | P2 |
| add-comments | `/Text` annots at coords only; `anchorText` rejected | Java richer | PARTIAL | P2 |
| add-stamp / add-watermark | image+QR real; **text variants rejected** | Java text stamps/watermarks | PARTIAL | P2 |
| auto-redact regex | Rust `regex` crate — Java-valid lookaround/backrefs → 400 | Java `java.util.regex` | PARTIAL | P2 |
| pdf→pdfa/pdfx | GS pdfwrite+ICC; level-A→B fallback; strict PDF/X rejected | Java same-ish + veraPDF validate | PARTIAL | P2 |
| pdf→text | `lopdf::extract_text` (weaker fidelity) | Java PDFBox | UNVERIFIED→PARTIAL | P3 |
| ocr-pdf | OCRmyPDF→per-page Tesseract@300dpi via hayro/GS | Java OCRmyPDF/JPDFium | COMPLETE | — |
| remove-blanks / scanner-effect / extract-image-scans / auto-split QR | hayro primary, GS fallback | Java JPDFium/ZXing | COMPLETE (QR lib differs rqrr vs ZXing) | — |
| mobile-scanner ×6 | full session API | Java equivalent | COMPLETE | — |
| pipeline handleData | loopback dispatch; fatal step → empty 200 | Java TaskManager richer | PARTIAL | P2 |
| `?async=true` jobs | honored on **2 endpoints only** (pipeline, update-enable-analytics) | Java `@AutoJobPostMapping` on ~92 endpoints | **BROKEN contract** | P1 |
| `fileId` chaining param | **unsupported everywhere** — only `fileInput` | Java every tool accepts fileId XOR fileInput | **MISSING systemic** | P1 |
| proprietary/saas suite | n/a | no Rust routes; ~250 frontend call sites | PROPRIETARY | — |

## C. Items verified as genuinely absent (no Rust equivalent anywhere)

`cert-sign` + hardware signing (4) · `restore-pdf-protection` *(now routed in-flight)* · `print-file` · `pdf→video` (dead both sides) · `pdf→ua` (proprietary) · form-detection model endpoints · Stirling Office Convert engine paths · OLE2 `.msg` · Tesseract OSD auto-rotate · text stamps/watermarks · merge `generateToc`/`removeCertSign=false` · crop `autoCrop`/`removeDataOutsideCrop` · selective password ops · `anchorText` comments · strict PDF/X · compress `lineArt` · flatten raster default · JPDFium verified redaction · veraPDF library (CLI approximation) · ImageIO codec coverage · ZXing (→rqrr) · `/v3/api-docs` · metrics/telegram/PostHog/app-update ports.

## D. Frontend-exposed dead ends on Rust backend

- `/api/v1/security/cert-sign*` (~10 paths) — frontend registers certSign tool; 403.
- `/api/v1/convert/pdf/ua` — 403 (proprietary key).
- `/api/v1/form/form-detection-model*` — 404 (key unknown → "enabled" but unrouted).
- `/api/v1/ai/*` (orchestrate/stream, tools/classify-and-label, status, health) — 403/404; `app-config` can still report `aiEngineEnabled:true` → UI exposes failing features (A08-F03).
- `/api/v1/security/restore-pdf-protection` — was 403; **fixed in-flight** (uncommitted).
- Proprietary namespaces (~150 endpoints: auth/user/team/storage/policies/payg/admin/notifications…) — 404; mostly gated by `enableLogin:false`/`premiumEnabled:false` in app-config, but processor portal pages still issue calls.
