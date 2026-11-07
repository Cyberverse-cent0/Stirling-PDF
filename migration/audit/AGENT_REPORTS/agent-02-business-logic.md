# A02 — Core Business Logic & Feature Completeness (business-logic audit, round 3)

Read-only audit of whether migrated endpoints **do the work**, not merely
return 200. Every row below was traced route → handler → native `lopdf`
mutation or `convert::EngineSpec`/`EngineSpec::run` external-engine call,
then compared against the replaced Java implementation.

## Audit basis

- Rust HEAD: `b1ad75e39afb881f544483d12129c380750e3759` ("wip(rust):
  cert-sign API fixes and merge/transform handler updates"), plus one
  uncommitted cosmetic diff in `rust/src/handlers/transform.rs`.
  This commit landed the previously-uncommitted cert-sign wiring,
  `removeCertSign=false` merge support, GS-backed crop data removal, and
  the routed flatten raster path described in the round-2 report.
- Java baseline (pre-migration common snapshot):
  `f233e7c89fed4a568c2f4c860316f14ada1515f6`. Controllers deleted by the
  migration (`MergeController`, `CropController`, `FlattenController`,
  `StampController`, `WatermarkController`, `AddCommentsController`,
  `CompressController`, `OCRController`, `AutoRotateController`,
  `AutoSplitPdfController`, `ExtractImagesController`, `FormFillController`,
  security controllers, …) are cited from that snapshot; controllers that
  still exist at HEAD (`PasswordController`, `CertSignController`,
  `TimestampController`, `ValidateSignatureController`,
  `ValidateComplianceController`, `VerifyPDFController`, `GetInfoOnPDF`,
  `RedactController`, `SanitizeController`, `RemoveCertSignController`)
  are cited from both.
- Supersedes `agent-02-feature-completeness.md` (round 2). Findings below
  are renumbered `A02-B##`; round-2 `A02-F##` IDs are referenced where a
  finding carried over unchanged.

## ⛔ Build status — read first

**The Rust crate does not compile at HEAD.** `cargo test -p stirling-server`
fails with **26 errors, all in `rust/src/certsign.rs`** (verified
2026-11-07, full log retained):

- `use cms::builder::{SignedDataBuilder, SignerInfoBuilder}` —
  `cms::builder` not found (`certsign.rs:33`); the `cms` 0.2.3 dependency
  does not expose `builder` without a feature this crate does not enable.
- `aes::Aes128/Aes192/Aes256`, `TdesEde3`, `TdesEde2`, `Des`, `Rc2`:
  `KeyIvInit`/`InnerIvInit` trait not satisfied — the `cbc 0.1`/`cipher
  0.4`/`crypto-common 0.1` stack disagrees with the `aes 0.8`/`des 0.8`/
  `rc2 0.8` versions resolved (multiple `crypto_common`/`aes` versions in
  the graph; `certsign.rs:1180-1205` and friends).
- `ed25519-dalek`/`rsa`/`p256`/`p384` `signature`/`pkcs8` trait-bound
  mismatches (`certsign.rs:288-360, 646-736`).
- `der::Any::tag()` used without `der::Tagged` in scope
  (`certsign.rs:1336`); a `'a` lifetime error in `decode_sequence_of`
  (`certsign.rs:748-757`); assorted `E0308` mismatches.

Because `mod certsign;` is wired into `main.rs:5` and routed at
`handlers/mod.rs:207-209`, **the entire `stirling-server` binary fails to
build** — no endpoint in this report can serve until the PKCS#12/PBES2
crypto code is repaired or feature-gated off. Treat every "COMPLETE"
below as *source-verified, not runtime-verified*.

## Feature status table

| Feature | Status | Evidence |
|---|---|---|
| merge-pdfs | PARTIAL | `handlers/merge.rs:28-88` real lopdf merge; `generateToc` still rejected (`merge.rs:54-60`); `removeCertSign=false` now re-registers signature field-widget annotations under a fresh `/AcroForm` (`merge.rs:82,96-137`) — hierarchical `/FT /Sig` fields and all non-signature form fields are still dropped |
| rotate / remove-pages / rearrange-pages / remove-images / pdf-to-single-page / overlay-pdfs / edit-table-of-contents / extract-bookmarks | COMPLETE | `handlers/general.rs:22-103,259-305,609-634,634-780,819-897`; lopdf `/Rotate`, `pdfdoc::subset`, resource `/XObject` cleaning, content-stream overlays |
| split-pages / by-size-or-count / by-chapters / by-sections / poster | COMPLETE | `handlers/split.rs:65-614`; bookmark-driven chaptering, section filters, poster tiling |
| multi-page-layout / booklet-imposition | COMPLETE (minor gap) | `handlers/layout.rs:126-351,473-470`; real N-up/saddle-stitch imposition; form fields not copied onto laid-out pages (`layout.rs` page push path) |
| scale-pages | COMPLETE | `handlers/transform.rs` scale branch; annotation rects transformed (`transform.rs` tests ~492-523) |
| crop | PARTIAL→mostly ported | `transform.rs:209-298`: `removeDataOutsideCrop` (Java default true) now distils selected pages through `gs -dUseCropBox` (`crop_with_ghostscript`, `transform.rs:304-377`) with clip-only `pdfdoc::wrap_page_content` fallback when gs is absent; `autoCrop` still rejected (`transform.rs:218-222`). **Stale test** `crop_rejects_remove_data_outside_crop` (`transform.rs:457-472`) still asserts the old rejection and will fail once the crate compiles |
| compress-pdf | COMPLETE (one rejected mode) | `handlers/misc.rs:465-477,596-700`: Ghostscript pdfwrite + qpdf + native `imgcomp` recompression loop with `expectedOutputSize` auto-level; `lineArt=true` rejected (`misc.rs:471-474`) |
| repair | COMPLETE | `handlers/automation.rs:415-540`: gs → qpdf → lopdf fallback, gated on endpoint *groups* like Java |
| decompress-pdf / update-metadata / show-javascript | COMPLETE | `misc.rs:255,42,214` |
| flatten | COMPLETE (corrects round-2 F02) | `misc.rs:289-330`: `flattenOnlyForms=true` → native `forms::flatten`; default → `flatten_raster_work` rasterises every page via Ghostscript (`misc.rs:314-…`), matching Java `FlattenController`'s PDFRenderer→JPEG rebuild (`FlattenController.java:52-156`) |
| extract-images | PARTIAL | `imgextract.rs:1-90` + `misc.rs:421-452`: native decode subset only (DCTDecode JPEG, 8-bit Gray/RGB/CMYK + `/SMask`); JPX/CCITT/JBIG2/masks/Indexed/ICCBased/Separation/`/Decode`/colour-key Mask → whole request 400; format limited to `image`-crate codecs |
| attachments / portfolio (add, list, extract single/all, rename, delete, batch, create-portfolio, flatten-portfolio) | COMPLETE except one flag | `misc.rs:1017-1207` + `attachments.rs`; `convertToPdfA3b=true` rejected (`misc.rs:1175`) |
| unlock-pdf-forms / add-page-numbers / add-image / replace-invert-pdf / remove-blanks | COMPLETE | `misc.rs:1671,1924,2293,2225,2658`; remove-blanks uses text + hayro/Ghostscript raster white-ratio (`misc.rs:2658-2760`, `BlankRasterizer::probe` now takes bytes only, fixed `input.pdf` name — `misc.rs:2735-2750`) |
| add-comments | PARTIAL | `misc.rs:2501-2531`: coordinate `/Text` annots only; any spec with `anchorText` → 400 (`misc.rs:2519-2526`) |
| add-stamp / add-watermark | PARTIAL | `visual.rs:711-940,946-1090`: image + QR stamps and image watermarks real; **text stamp, text watermark, and `convertPDFToImage` rejected** (`visual.rs:743-745,969-972,951-955`) |
| sanitize-pdf | COMPLETE | `security.rs:1410-1448`: JS/embedded-files/XMP/metadata/links/fonts removal mirrors `SanitizeController.java:58-226` |
| get-info-on-pdf | DIVERGENT | `security.rs:1360` + `compliancy()` `security.rs:473-480`: `IsPDF/ACompliant`, `/UA`, `/E`, `/VT`, `/B` hardcoded `false`; only `IsPDF/SECCompliant` computed. Java populated the array via `VeraPDFService` (`GetInfoOnPDF.java:137-147,1117-1132`) |
| remove-cert-sign | COMPLETE | `security.rs:1457-1471` via `forms::remove_signature_fields` (widget APs drawn, fields dropped, `/SigFlags` cleaned) |
| redact (manual `/redact`) | COMPLETE-equivalent | `security.rs:1839-1940`: filled-rect overlays — same approach as Java `ManualRedactionService.java:45-120` (PDFBox append-only fills) |
| auto-redact / redact-execute | PARTIAL — **security-relevant** | `security.rs:2001-2075,2591-2771`: always overlay cover-rects; text remains extractable. Java's primary path is jpdfium `PdfRedactor` *verified content removal* (`RedactController.java:239-282`, `TextRedactionService.java:139-202`) with overlay only as fallback. `convert_to_image` rasterisation exists for `/redact`; page wipes drop content natively. Java-regex syntax → 400 (`security.rs:2017-2029, regex_patterns_unsupported`) |
| verify-pdf / validate-compliance | PARTIAL | `verapdf.rs:201-330` drives `verapdf` CLI (not the Java library); absent CLI → `compliant:false` + error (`security.rs:3000-3008`); WTPDF detected but not validated; strict PDF/X rejected (`convert.rs:3527-3536`) |
| accessibility-report (PDF/UA audit) | PARTIAL | `pdfua.rs:147-320`: veraPDF + lopdf facts; `figures_needing_description` always empty (`pdfua.rs:6-9,664`) — Java fills it from tagged-content analysis with no Rust port |
| pdf-to-ua (conversion) | MISSING | proprietary tagger; `endpointconfig.rs` disables `pdf-to-ua` unconditionally; no route |
| validate-signature | COMPLETE (minus revocation) | `signature.rs:1128-1434`: real CMS/PKCS#7 verify, RFC 3161 timestamp-token imprint check, PKIX-lite path building; **no OCSP/CRL/AIA** (`signature.rs:8-13`), no AATL/EUTL download, no `serverAsAnchor` |
| timestamp-pdf | COMPLETE | `timestamp.rs:57-100,182-350,498-583`: native RFC 3161, SHA-256-only to allowlisted TSA, `/DocTimeStamp` via lopdf incremental update, 0x2500 `/Contents` reserve, 1 MiB response cap |
| cert-sign | **CODE PRESENT, DOES NOT COMPILE** | route `handlers/mod.rs:207-209` → `certsign.rs:122`; `certsign.rs` fails with 26 errors (see build status). When fixed: real CMS `adbe.pkcs7.detached` signing via `cms`/`x509-cert` + lopdf incremental embed; PEM (PKCS#8/PKCS#1/SEC1) and PKCS#12 (PBE + PBES2) identities; JKS/SERVER/WINDOWS_STORE/PKCS11 rejected (`certsign.rs:491-510`); visible-signature appearance + logo supported (`certsign.rs:1219-1320`) |
| cert-sign hardware/session subpaths (`/cert-sign/hardware/*`, `/cert-sign/sessions*`) | MISSING | no routes; disabled by path pattern `endpointconfig.rs:471-475`; Java `HardwareSigningController` (`@RequestMapping /api/v1/security/cert-sign/hardware`) has no port |
| add-password / remove-password / prepare-pdf-session | COMPLETE (subset) | `passwords.rs:286-448,450-485`: lopdf RC4-40/AES-128/AES-256 `EncryptionState`; owner-auth required, signed PDFs refused (`passwords.rs:92-99`); selective removal fields and `preserveExisting` on encrypted input rejected (`passwords.rs:320,395`) vs Java `PasswordController` snapshot lines 64-373 |
| restore-pdf-protection | COMPLETE (corrects round-2 F11) | `passwords.rs:553-695`: re-encrypts processed file with source's version/revision/permissions/key-length via `EncryptionState`, then reloads and compares `/P`, `/Length`, `/R` — same verification as Java `PasswordController.java:67-115` (HEAD) / `PdfSecurity.copyEncryption`. Possible divergence: both owner and user passwords are set to the single supplied `password` (`passwords.rs:560-566`), so a source with *distinct* user/owner passwords may not round-trip — Java's "unknown second password" comment (`PasswordController.java:62-64`) suggests JPDFium preserves it; **unverified** |
| ocr-pdf | COMPLETE | `ocr.rs:35-158,159-287,289-380`: OCRmyPDF sidecar path (all Java flags mapped) else per-page Tesseract@300dpi via `BlankRasterizer`; language data validated |
| auto-rotate-pdf | PARTIAL | `automation.rs:153-172,176-…`: text/auto modes native; `detectionMode=osd` rejected (Java ran Tesseract OSD, `AutoRotateController.java:267-271`) |
| auto-split-pdf | COMPLETE (minor divergence) | `automation.rs:1266-1345`: Stirling-QR divider detection via direct image decode + raster fallback, `rqrr` decoder (Java used ZXing); `duplexMode` drops divider backs |
| extract-image-scans / scanner-effect / mobile-scanner session API (6 routes) | COMPLETE | `image_scans.rs:100-160`, `scanner.rs:137-…`, `automation.rs:678-1256`; routes `handlers/mod.rs:144-182` |
| analysis (8) / filter (6+) | COMPLETE | `analysis.rs:24-193`, `filter.rs:123-337` |
| form endpoints (fields, fields-with-coordinates, extract-csv/xlsx, fill, add/edit/modify/delete-fields) | COMPLETE (appearance gaps) | `form.rs:1-16,2908-2935,4650-4720`: real AcroForm walk + `/AP` regen for text/combobox/checkbox/radio/pushbutton; **listbox, multiline, comb rely on `/NeedAppearances`**; signature fields not creatable |
| pipeline handleData | COMPLETE (divergences) | `pipeline.rs:81-159,467-640`: JSON pipeline → loopback dispatch to live routes; invalid/non-dispatchable op → 400 (Java catch-all → empty 200); fatal step failure → empty 200 (`pipeline.rs:531-532,588-589`); dispatchable-but-unrouted paths (`/integration/*`, `/docparse/ingest`, `/ai/tools/*`) pass validation then 404→empty 200 |
| jobs framework + file store | COMPLETE (reliability gaps — see A06 report) | `jobs.rs`, `jobstore.rs` |
| img→pdf | COMPLETE (codec subset) | `convert.rs:40-404`: `image`-crate codecs + manual JPEG/PNG EXIF paths; others 400 |
| pdf→img | PARTIAL — ungated GS dependency | `convert.rs:591-720`: Ghostscript-only rasterisation, **no hayro fallback**; `gs` not probed for this endpoint (see B10) |
| pdf→cbz / cbz→pdf / pdf→cbr / cbr→pdf | COMPLETE (GS dep for pdf→cbz) | `convert.rs:793-853,883-927,1045-1240,1159-1240` |
| ebook→pdf / pdf→epub | PARTIAL | `convert.rs:2767-2915,2921-3040`: Calibre `ebook-convert`; `.docx` ebook input and `useStirlingOfficeConvert` rejected (`convert.rs:2773-2780,2927-2935`) |
| eml→pdf | PARTIAL | `eml.rs:65-127,145-190,974-1040`: mail-parser + WeasyPrint real; `.msg`/OLE2 → 400 (`eml.rs:92-99,154-160`); attachments embedded in name tree only, no page-level FileAttachment annots |
| file→pdf / pdf→word / pdf→presentation / pdf→xml | COMPLETE (engine paths) | `convert.rs:1631-1780,1798-1890`: unoconvert → soffice fallback; `useStirlingOfficeConvert` accepted-but-ignored on input, rejected on pdf→* office outputs |
| pdf→text | PARTIAL | `convert.rs:1812-1828`: `lopdf::extract_text` only when `outputFormat=txt`; fidelity vs Java PDFBox stripper unverified |
| pdf→html | COMPLETE | `convert.rs:3182-3244`: `pdftohtml -c` → ZIP |
| pdf→csv / pdf→xlsx / pdf→markdown | COMPLETE | `convert.rs:3936-4176` + `tabula.rs` + `pdftext/`; Office-Convert variants rejected |
| html→pdf / url→pdf / markdown→pdf | COMPLETE | `convert.rs:1897-2645`: WeasyPrint; `useStirlingOfficeConvert` ignored/rejected; url SSRF note below (B20) |
| svg→pdf | PARTIAL/DIVERGENT | `convert.rs:1250-1380`: `rsvg-convert` preferred → **raster** page (Java Batik emitted vector), `inkscape` fallback; neither binary probed/gated |
| vector↔pdf (ps/eps) | COMPLETE | `convert.rs:3050-3180`: Ghostscript |
| pdf→pdfa / pdfx | PARTIAL | `convert.rs:3324-3578`: GS pdfwrite + ICC; PDF/A level-A→B fallback; `strict` PDF/A via veraPDF CLI; **strict PDF/X rejected** (`convert.rs:3527-3536`); `useStirlingOfficeConvert` rejected |
| pdf→video | MISSING (intentional) | no route; Java commented it out over ffmpeg CVEs; `ffmpeg` deliberately unprobed (`endpointconfig.rs` comment ~line 61) |
| print-file | MISSING | no route, no Java OSS controller found |
| remove-annotations / extract-pages / compare / adjust-contrast / sign / multi-tool / view-pdf / automate | N/A — client-side or composed | frontend decomposes into routed calls; deliberately absent from `UNSUPPORTED_ENDPOINTS` (`endpointconfig.rs:366-368` comment) |

## Findings

### A02-B01 — `certsign.rs` does not compile; whole crate unbuildable at HEAD — P0

`cargo test -p stirling-server` → 26 errors, all in `certsign.rs`
(`cms::builder` unresolved import; `aes`/`des`/`rc2`/`tdes` `KeyIvInit`
bound failures from mixed `cipher 0.4`/`crypto-common 0.1` vs `0.2`
versions; `ed25519-dalek`/`rsa`/`p256`/`p384`/`pkcs8` trait mismatches;
missing `der::Tagged` import; `'a` lifetime error in `decode_sequence_of`
at `certsign.rs:748-757`). Because `mod certsign` is in `main.rs:5` and
the route is live in `handlers/mod.rs:207-209`, **no Rust endpoint can
serve** until this is fixed. The round-2 report's "cert-sign MISSING"
was stale, but the replacement is currently worse than missing: it breaks
the build. The WIP commit `b1ad75e39a` also left
`crop_rejects_remove_data_outside_crop` (`transform.rs:457-472`)
asserting the pre-change rejection — a second, latent test failure once
compilation is repaired.

### A02-B02 — Redaction never removes content — always overlay — DIVERGENT (security) — P1

`/redact` manual overlays match Java's PDFBox path
(`ManualRedactionService.java:45-120` — append-only `re f` fills), so that
endpoint is parity. But `/auto-redact` (`security.rs:2001-2075`) and
`/redact-execute` (`security.rs:2591-2771`) **always** paint cover
rectangles; Java's primary path is jpdfium `PdfRedactor` verified content
removal (`RedactController.java:239-282`,
`TextRedactionService.java:139-202`), overlay only on failure. Underlying
text remains extractable in the Rust output unless the caller also passes
`convertPDFToImage`. Security-relevant: a user who believes text was
redacted can leak it. Compounded by `regex`-crate rejections of valid
Java regex syntax (`security.rs:2017-2029`).

### A02-B03 — `get-info-on-pdf` reports every standard non-compliant — DIVERGENT — P2

`security.rs:473-480` hardcodes `IsPDF/ACompliant:false`,
`IsPDF/UACompliant:false`, `IsPDF/ECompliant:false`, `IsPDF/VTCompliant:
false`, `IsPDF/BCompliant:false`; only `IsPDF/SECCompliant` is computed.
Java `GetInfoOnPDF.java:137-147` populated the `Compliance` array from
`VeraPDFService` results. The endpoint still returns rich page/document
metadata (`security.rs:113-1359`), so this is a wrong-data bug, not a
missing feature — and it contradicts `verify-pdf`, which does run veraPDF.

### A02-B04 — veraPDF-dependent endpoints degrade to "non-compliant"/error when the CLI is absent — PARTIAL — P2

`verapdf.rs:201-330` shells out to `verapdf` CLI and parses its text
report; Java used the veraPDF *library* (always present). Missing CLI →
`verify_pdf` reports `compliant:false` + error string
(`security.rs:3000-3008`); `validate_compliance` turns that into a
pipeline-stopping `ComplianceNotMet` (`security.rs:3192-3225`);
`accessibility_report`/`pdfua::audit` surface validation errors. WTPDF is
detected (`verapdf.rs:161`) but has no CLI flavour and is never actually
validated. Failure-detail granularity differs from Java's structured
`PDFAFlavour`/clause results.

### A02-B05 — `pdf-to-img`, `pdf-to-cbz` need Ghostscript but availability never reflects it — DIVERGENT — P1

`convert.rs:644-676` (`pdf_to_img`) and `convert.rs:883-927`
(`gs_render_pages_png`, used by `pdf_to_cbz`) are Ghostscript-only —
`hayro`, the native rasteriser used elsewhere, is not consulted. The
`Ghostscript` tool group (`endpointconfig.rs` ~lines 326-336) lists only
`repair`, `compress-pdf`, `crop`, `replace-invert-pdf`, `scanner-effect`,
`pdf-to-vector`, `vector-to-pdf` — **not** `pdf-to-img`/`pdf-to-cbz`/
`pdf-to-cbr`. Those keys sit in the always-on `Java` group
(`endpointconfig.rs:253-309`), so `/api/v1/convert/pdf/img` reports
enabled and then 500s without `gs`. Java used bundled JPDFium.

### A02-B06 — `svg→pdf` prefers the rasteriser; engines unprobed/ungated — DIVERGENT — P2

`convert.rs:1337-1378`: `rsvg-convert` first (PNG page sized to the SVG
viewport — raster where Java's Batik path emitted vector PDF),
`inkscape` fallback. Neither binary is in `ENGINE_PROBES`
(`endpointconfig.rs:62-74`) and `svg-to-pdf`/`vector-to-pdf` keys are not
consistently grouped, so the endpoint reports enabled and 500s when both
are absent.

### A02-B07 — `merge-pdfs`: `generateToc` rejected; `removeCertSign=false` only partially honoured — PARTIAL — P2

`merge.rs:54-60` still rejects `generateToc` (Java built a bookmark tree,
`MergeController.java:425-482`). `removeCertSign=false` is now accepted
and `preserve_signature_fields` (`merge.rs:96-137`) re-registers orphan
`/FT /Sig` widget annotations under a fresh `/AcroForm` — but only
field-widget dictionaries that survived page import as annotations;
hierarchical signature fields and **all non-signature form fields are
dropped**. Java's jpdfium merge + `removeCertSign` flatten
(`MergeController.java:303-325`) preserves the document structure
faithfully. Net: default merges work; signature preservation is
best-effort, not equivalent.

### A02-B08 — `crop`: `removeDataOutsideCrop` now works via Ghostscript; `autoCrop` still rejected; stale test — PARTIAL — P2

`transform.rs:218-233` parses `autoCrop`/`removeDataOutsideCrop`
(absent → true, matching Java). `transform.rs:261-277` runs
`crop_with_ghostscript` (`transform.rs:304-377`) — selected pages get a
`/CropBox`, are distilled through `gs -dUseCropBox`, then merged back —
when `removeDataOutsideCrop=true` **and gs is probed present**; otherwise
the clip-only `pdfdoc::wrap_page_content` path runs (Java's PDFBox
fallback). `autoCrop` → 400 (`transform.rs:218-222`). Residual issues:
(a) gs-absent deployments silently get clip-only output where Java would
still remove data via its own renderer — a data-remanence divergence,
not just a missing feature; (b) `crop_rejects_remove_data_outside_crop`
(`transform.rs:457-472`) still asserts rejection and contradicts the new
code — will fail once B01 is fixed.

### A02-B09 — Password protection: selective ops rejected; signed PDFs refused; restore-password collapse unverified — PARTIAL — P2

`passwords.rs:320` rejects `preserveExisting` on already-encrypted input;
`passwords.rs:395` rejects selective `password`/`ownerPassword`/`sourcePassword`
removal fields — Java `PasswordController` (snapshot lines 86-373)
implements all of them via PDFBox permission analysis. `refuse_signed`
(`passwords.rs:92-99`) blocks protection changes on signed PDFs; Java's
`prepare-pdf-session`/`restore-pdf-protection` pair exists precisely to
handle protected *inputs* to other tools, which Rust does implement
(`passwords.rs:450-485,623-695`) including the reload-and-compare
verification. `restore_state` (`passwords.rs:553-600`) maps V1/V2/V4/R5/V5
encryption versions; unsupported versions error. Open question: both
owner and user passwords are set to the single `password` argument —
Java's JPDFium `copyEncryption` claims to preserve "an unknown second
password" (`PasswordController.java:62-64`); whether Rust's collapse
diverges is **unverified** without a two-password fixture.

### A02-B10 — `cert-sign`: real implementation, wrong dependency versions, format/backend gaps — P0/P1

See B01 for the build break. Once compiling, the design is genuine:
`certsign.rs` builds CMS SignedData (`adbe.pkcs7.detached`), signs the
ByteRange-covered hash, and embeds via `timestamp::prepare_signed_revision`
+ `embed_signature_token` — the same incremental-update machinery as
`/timestamp-pdf`. Supported: `certType=PEM` (PKCS#8, encrypted PKCS#8,
unencrypted PKCS#1 RSA, SEC1 EC; OpenSSL-legacy `Proc-Type`/`DEK-Info`
rejected), `certType=PKCS12|PFX` (PKCS#12-KDF PBE 3DES/RC2 + PBES2
PBKDF2/DES/3DES/AES-CBC; RC4 PBE and RC2-in-PBES2 rejected; MAC
unverified). Rejected vs Java `CertSignController.java:236-317`: `JKS`,
`SERVER` (proprietary bean absent — same error text as Java's
no-bean path), `WINDOWS_STORE`, `PKCS11` (desktop-only, no port).
Hardware/session subpaths are path-disabled (`endpointconfig.rs:471-475`).
`showSignature`/`showLogo`/`pageNumber`/`reason`/`location`/`name` are
honoured (`certsign.rs:147-165`). RSA-PSS/ECDSA/Ed25519 signing paths
exist but currently don't compile (B01).

### A02-B11 — `extract-images` / `img→pdf` codec subsets; stale "forwarded to Java" comment — PARTIAL — P2

`imgextract.rs:1-23,70` + `misc.rs:443-452`: any image XObject outside
`imgcomp::decode_image`'s subset (JPXDecode, CCITTFaxDecode, JBIG2Decode,
ImageMask, Indexed/ICCBased/Separation colour spaces, `/Decode` remapping,
colour-key `/Mask`, non-matching `/SMask`) makes `supported()` return
false and the **whole request 400s** — Java/ImageIO extracted all of
them. `convert.rs:123-149` limits img→pdf to `image`-crate-decodable
uploads. `imgextract.rs:18` still claims unsupported requests are
"forwarded to Java" — the proxy is gone; behaviour is 400. Same stale
claim pattern in `pdftext/redact.rs` and `migration/reports/
JAVA_DEPENDENCIES.md` (round-2 F20, still present).

### A02-B12 — Text stamps/watermarks and `convertPDFToImage` rejected — PARTIAL — P2

`visual.rs:743-745` rejects `stampType=text` (Java `StampController.java:
104-274` embeds NotoSans TTFs and draws rotated multi-line stamps with
`@command` expansion); `visual.rs:969-972` rejects `watermarkType=text`
(Java `WatermarkController.java:69-199`); `visual.rs:951-955` rejects
`convertPDFToImage` rasterisation. Image and QR paths are real
(`visual.rs:786-940,1010-1090`). Note the rasterisation machinery now
exists (`flatten_raster_work`, `BlankRasterizer`) — these rejections are
port gaps, not architectural ones.

### A02-B13 — `add-comments` supports only coordinate `/Text` annots — PARTIAL — P2

`misc.rs:2501-2531`: any spec carrying `anchorText` → 400
(`misc.rs:2519-2526`). Java `AddCommentsController.java:72-180` resolves
`anchorText` via `pdfTextLocator.findOnPage` with coordinate fallback;
`pdftext::page_items` already provides positioned text, so this is a
feasible port, not a missing dependency.

### A02-B14 — `compress-pdf lineArt=true` rejected — PARTIAL — P3

`misc.rs:471-474`: needs Java's `LineArtConversionService` (ImageMagick
edge detection); the rest of the compression pipeline — GS levels 6-9,
qpdf `--normalize-content`/`--linearize`, native image recompression,
`expectedOutputSize` auto-level loop (`misc.rs:596-700`) — is a faithful
port of `CompressController.java:945-1384`.

### A02-B15 — `auto-rotate-pdf` loses the OSD path — PARTIAL — P2

`automation.rs:165-172` rejects `detectionMode=osd`; Java ran Tesseract
OSD (`AutoRotateController.java:267-271`). `auto`/`text` modes and the
`pageRotations`/`dryRun` JSON report are native (`automation.rs:176-412`).
Tesseract is probed for the `tesseract` group, so OSD is a missing
feature, not a hidden dependency.

### A02-B16 — `eml→pdf` rejects `.msg`; attachments are name-tree only — PARTIAL — P2

`eml.rs:92-99,154-160` rejects OLE2 `.msg` (Java used msg-parser);
`eml.rs:974-1040` embeds attachments as `/EmbeddedFiles` name-tree
entries — Java additionally emitted page-level FileAttachment annotations
(baseline claim; the name-tree part is confirmed). HTML sanitisation is
`ammonia`-based (`eml.rs:535-760`), not Java's sanitizer — output HTML
differs in edge cases.

### A02-B17 — `pdf→pdfa`/`pdfx`, office-Convert, ebook gaps — PARTIAL — P2

`convert.rs:3345-3350` rejects `useStirlingOfficeConvert` everywhere it
appears (Stirling Office Convert engine has no Rust port); PDF/A level-A
requests emit B-conformance like OSS Java (`convert.rs` GS args);
`strict` PDF/A runs veraPDF CLI and fails honestly when absent
(`convert.rs:3518-3565`); **strict PDF/X is rejected outright**
(`convert.rs:3527-3536`) — Java had a separate PDF/X validation path.
`pdf_to_epub`/`pdf_to_xlsx`/`pdf_to_markdown` reject the Office-Convert
variant (`convert.rs:2927-2935,3990-3995,4117-4122`); `ebook_to_pdf`
rejects `.docx` and Office-Convert (`convert.rs:2767-2780`).

### A02-B18 — `url→pdf` WeasyPrint subresources bypass SSRF checks — UNVERIFIED→PARTIAL — P3

`convert.rs:2166` passes `--allowed-protocols data,http,https` to
WeasyPrint, so a fetched page can pull arbitrary http(s) subresources —
including internal IPs — while only the top-level URL is validated
(`convert.rs:2218-2260`). `fetch_remote_html` reads the body unbounded
(`convert.rs:2377-2380`). Java had the same WeasyPrint behaviour, so this
is parity-with-a-shared-flaw rather than a regression — flagged because
the Rust host-check code *looks* like it closes the hole and doesn't.

### A02-B19 — `pdf→text` uses `lopdf::extract_text`, not the PDFBox-fidelity stripper — UNVERIFIED — P3

`convert.rs:1812-1828`: only the `outputFormat=txt` branch extracts text,
via lopdf. `pdftext/` and `text::page_items` exist and are more faithful
(baseline suggestion stands). `pdf→markdown`/`pdf→csv`/`pdf→xlsx` go
through `pdftohtml`/`tabula`/`pdftext` and are structurally real, but
output fidelity on representative corpora is unverified (see below).

### A02-B20 — Pipeline divergences from Java `HandleData` — PARTIAL — P3

`pipeline.rs:81-159` ports the dispatchable-path allowlist and required-
param slice; `is_valid_operation` rejects bad ops with 400 where Java's
catch-all returned empty-200. Fatal step failures (transport error)
return empty-200 like Java (`pipeline.rs:531-532,588-589`), but
non-200 step responses only set `has_errors` and continue
(`pipeline.rs:542-549`) — Java's per-file error semantics differ subtly.
Dispatchable paths with no Rust route (`/api/v1/integration/*`,
`/api/v1/docparse/ingest`, `/api/v1/ai/tools/*`) pass validation, 404 via
the fallback, and surface as empty-200 — matching Java's shape but
silently dropping work the frontend may expect (ingest is also in
`UNSUPPORTED_ENDPOINTS`).

### A02-B21 — Layout/booklet drop form fields; `flatten-portfolio`/attachments `convertToPdfA3b` gaps — PARTIAL — P3

`layout.rs` `push_output_page` rebuilds pages from content streams and
does not carry widget annotations/AcroForm — Java's N-up/booklet had the
same class of loss but via PDFBox; net effect: forms break after
imposition in both, but the Rust path also drops the `/AcroForm` dict
entirely. `misc.rs:1175` rejects `convertToPdfA3b` on all attachment
endpoints (Java ran a PDF/A-3 conversion pass). `flatten-portfolio`
(`misc.rs:1211`) drops `/Collection` natively.

### A02-B22 — `auto-split-pdf` QR decoding differs from Java ZXing — MINOR — P3

`automation.rs:1427-1451` uses `rqrr` on luma pixels from direct image
decode or 150/300 DPI raster; Java used ZXing with different preprocessing.
Detection of the fixed Stirling divider strings is equivalent; edge-case
QR images (low contrast, rotated, dense) may diverge. `duplexMode`
back-page skipping matches (`automation.rs:1290-1296`).

### A02-B23 — Endpoint availability vs implementation mismatches — DIVERGENT — P1/P2

`endpointconfig.rs` is otherwise faithful (probe → group disable →
endpoint-key translate → `guard.rs:40` 403), but: (a) `pdf-to-img`/
`pdf-to-cbz`/`pdf-to-cbr` are GS-dependent yet sit in the always-on `Java`
group (B05); (b) `svg→pdf` engines are unprobed (B06); (c)
`UNSUPPORTED_ENDPOINTS` (`endpointconfig.rs:~370-382`) covers only
`ingest`, `orchestrate`, `tools`, `external-api-call`, `purview-*` —
while `pdf-to-ua`/`form-detection` are separately dependency-disabled and
`cert-sign/*` subpaths are path-disabled; (d) `url-to-pdf` can be
config-disabled (`endpointconfig.rs:476-478`) but WeasyPrint-dependent
siblings (`html-to-pdf`, `markdown-to-pdf`, `eml-to-pdf`) share the same
binary and are not independently gated beyond the `Weasyprint` group.

### A02-B24 — Form appearance regeneration is partial — PARTIAL — P3

`form.rs:1-16,1777-1860,3419-3475`: `/fill`, add/edit/modify/delete-fields
walk the AcroForm tree faithfully and regenerate `/AP` for single-line
text, combobox, checkbox/radio, pushbutton; **listbox, multiline and comb
fields keep `/NeedAppearances=true`** for viewer-side regeneration —
visually correct in conforming viewers, wrong for consumers that read
`/AP` directly (e.g. flatten-then-extract pipelines). Signature fields
cannot be created (`/FT /Sig` widgets unsupported).

### A02-B25 — Stale docs/comments claim Java-proxy forwarding that no longer exists — DIVERGENT (docs) — P3

`imgextract.rs:18` ("the handler forwards the whole request to Java"),
`pdftext/redact.rs` header, `migration/reports/JAVA_DEPENDENCIES.md` —
the Java proxy was removed; actual behaviour is 400 or native fallback.
Carried over from round-2 F20; still present at HEAD.

## Java features with no Rust equivalent (consolidated)

- **Build-blocking**: `certsign.rs` PKCS#12/PBES2 crypto stack (26 compile
  errors) — cert-sign route is wired but the crate does not build.
- Proprietary/AI/integration: `pdf-to-ua` conversion (tagger),
  `form-detection` (model manager), `ingest`, `orchestrate`, `tools`,
  `external-api-call`, `purview-apply-label`, `purview-read-label`,
  Stirling Office Convert (`useStirlingOfficeConvert` paths), SaaS suite.
- Hardware signing: `cert-sign` `WINDOWS_STORE`/`PKCS11`/`JKS`/`SERVER`
  and all `/cert-sign/hardware/*`, `/cert-sign/sessions*` subroutes.
- PDF features: text stamps, text watermarks, `convertPDFToImage`
  watermark rasterisation, `anchorText` comments, crop `autoCrop`,
  merge `generateToc`, compress `lineArt`, selective/preserveExisting
  password ops, `.msg` email, strict PDF/X validation, `convertToPdfA3b`
  on attachments, auto-rotate OSD, jpdfium verified redaction, veraPDF
  *library* fidelity (CLI approximation), Tesseract OSD.
- Engine/codec coverage: ImageIO full codec set (extract-images/img→pdf
  subsets), ZXing (→`rqrr`), Batik vector SVG (→rsvg-convert raster +
  inkscape), msg-parser, Java regex syntax in redaction patterns,
  Stirling PDF/UA figure-description analysis.

## Could not verify

- Byte-level output parity of `lopdf` rewrites vs PDFBox/JPDFium across
  merge/split/crop/flatten/compress corpora.
- Whether surviving Java controllers still boot/serve alongside Rust
  (strangler state) — out of scope for source audit.
- `pdf→text`/`pdf→markdown`/`pdf→csv`/`pdf→xlsx` fidelity on representative
  documents.
- `hayro` rendering correctness vs JPDFium for complex PDFs (used by
  remove-blanks, OCR fallback, scanner paths).
- External-engine behaviour across installed versions (`gs`, `qpdf`,
  `soffice`/`unoconvert`, `weasyprint`, `pdftohtml`, `tesseract`,
  `ocrmypdf`, `ebook-convert`, `verapdf`, `rsvg-convert`/`inkscape`,
  archive tools) — probes only check presence/version floor.
- `restore-pdf-protection` with distinct source user/owner passwords
  (B09).
- Runtime behaviour of anything in this report until B01 (the
  `certsign.rs` compile failure) is fixed — `cargo test` cannot even
  build the test harness.
