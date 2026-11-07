# A02 — Core Business Logic & Feature Completeness (round 2, 2026-11-07)

Architecture: page/object work is native `lopdf`; rasterisation is `hayro` (pure-Rust) with Ghostscript fallback in `misc::BlankRasterizer`; everything else shells out via `convert::EngineSpec`/`EngineSpec::run` to `gs`, `qpdf`, `soffice`/`unoconvert`, `weasyprint`, `pdftohtml`, `tesseract`, `ocrmypdf`, `ebook-convert`, `rar`/`unrar`/`unar`/`bsdtar`/`7z`, `rsvg-convert`/`inkscape`, `verapdf`. Engine availability probed in `endpointconfig.rs:62-74`.

## Feature status table

| Feature | Status | Evidence |
|---|---|---|
| merge-pdfs | PARTIAL | `merge.rs:23-88` real lopdf merge; rejects `generateToc`, `removeCertSign=false` (merge.rs:53-62) |
| rotate / remove-pages / rearrange-pages | COMPLETE | `general.rs:21-103`, lopdf `/Rotate` + `pdfdoc::subset` |
| split-pages / by-size-count / chapters / sections / poster | COMPLETE | `split.rs:60-614` |
| multi-page-layout / booklet-imposition | COMPLETE (minor gap) | `layout.rs:119-470`; form fields not copied (`layout.rs:123-124`) |
| scale-pages / crop / pdf-to-single-page / remove-image-pdf / overlay / TOC edit / extract-bookmarks | COMPLETE except crop | `transform.rs:53-283`; crop rejects `autoCrop` & `removeDataOutsideCrop=true`, absent param defaults to rejected-true → 400 (`transform.rs:215-236`) |
| compress-pdf | COMPLETE (one rejected mode) | `misc.rs:600-883`: GS pdfwrite + qpdf + imgcomp; `lineArt=true` rejected (`misc.rs:475-477`) |
| repair | COMPLETE | `automation.rs:414-…`: GS → qpdf → lopdf fallback |
| decompress-pdf / update-metadata / show-javascript | COMPLETE | `misc.rs:211-283`, `misc.rs:34-…` |
| flatten | PARTIAL — default path dead | `misc.rs:290-311` rejects unless `flattenOnlyForms=true`; raster path exists but `#[allow(dead_code)]` unrouted (`misc.rs:317-318`) |
| extract-images | PARTIAL | `imgextract.rs` native; JPX/CCITT/JBIG2/Indexed/ICCBased/SMask-incompatible → 400 (`misc.rs:443-452`) |
| attachments / portfolio (all ops) | COMPLETE | `misc.rs:885-1670`, `attachments.rs`; `convertToPdfA3b` rejected (`misc.rs:1179`) |
| unlock-pdf-forms / add-page-numbers / add-image / replace-invert-pdf | COMPLETE | `misc.rs:1671,1923,2293,2225` |
| remove-blanks | COMPLETE | `misc.rs:2651-2864`: text + hayro/GS white-pixel ratio |
| add-comments | PARTIAL | `misc.rs:2500-2611`: only `/Text` annots at coordinates; `anchorText` rejected (`misc.rs:2527-2531`) |
| add-stamp / add-watermark | PARTIAL | `visual.rs:707-969`: image + QR real; **text stamp & text watermark rejected** (`visual.rs:743,969`) |
| sanitize-pdf | COMPLETE | `security.rs:1405-1448` |
| get-info-on-pdf | DIVERGENT | `security.rs:1-19,473-480`: `Compliancy` hardcodes all standards `false` |
| remove-cert-sign | COMPLETE | `security.rs:1450-1471` via `forms::remove_signature_fields` |
| redact (manual) | COMPLETE | `security.rs:1826-1938`: overlay rects — same as Java PDFBox path |
| auto-redact / redact-execute | PARTIAL — security-relevant | `security.rs:1990-…,2580-…`: always overlay; text remains extractable; Java-regex → 400 |
| verify-pdf / validate-compliance | PARTIAL | `verapdf.rs` drives `verapdf` CLI; absent → all standards reported non-compliant (`security.rs:3000-3008`) |
| accessibility-report (PDF/UA audit) | PARTIAL | `pdfua.rs:1-11`: veraPDF + lopdf facts; `figuresNeedingDescription` always empty |
| pdf-to-ua (conversion) | MISSING | proprietary tagger; `endpointconfig.rs:469`; no route |
| validate-signature | COMPLETE (minus revocation) | `signature.rs:1-13`: real CMS/PKCS#7 verify; no OCSP/CRL/AIA |
| timestamp-pdf | COMPLETE | `timestamp.rs:1-12`: native RFC 3161 + lopdf incremental `/DocTimeStamp` |
| add-password / remove-password / prepare-pdf-session | COMPLETE (subset) | `passwords.rs:1-25`: lopdf RC4-40/AES-128/AES-256; selective removal & `preserveExisting` rejected; signed PDFs refused |
| restore-pdf-protection | MISSING | `passwords.rs:23-25`; `endpointconfig.rs:375` |
| cert-sign + hardware signing (4 endpoints) | MISSING | no routes; `endpointconfig.rs:372` |
| ocr-pdf | COMPLETE | `ocr.rs:1-330`: OCRmyPDF preferred else per-page Tesseract@300dpi via `BlankRasterizer` |
| auto-rotate-pdf | PARTIAL | `automation.rs:147-355`: text-direction native; `detectionMode=osd` rejected (`automation.rs:165-172`); auto-mode undecided pages → `tesseractUnavailable` |
| auto-split-pdf | COMPLETE (minor divergence) | `automation.rs:1257-1341`: QR divider via `rqrr` (vs Java ZXing) |
| extract-image-scans | COMPLETE | `image_scans.rs` + `image_split.rs`: native port of `split_photos.py` |
| scanner-effect | COMPLETE | `scanner.rs:1-140` |
| mobile-scanner session API (6 routes) | COMPLETE | `automation.rs:676-1256` |
| analysis (8) / filter (6) | COMPLETE | `analysis.rs`, `filter.rs` |
| form (9 endpoints) | COMPLETE (minor gaps) | `form.rs:1-16`: AcroForm walk + `/AP` regen; listbox/multiline/comb rely on `/NeedAppearances`; signature fields not creatable |
| pipeline handleData | COMPLETE (divergences documented) | `pipeline.rs:1-22`: loopback dispatch; validity approximated; fatal step failures → empty 200 |
| jobs framework + file store | COMPLETE (reliability gaps — see A06) | `jobs.rs:1-16` |
| img→pdf | COMPLETE (codec subset) | `convert.rs:33-404`; non-`image`-crate formats 400 |
| pdf→img | PARTIAL — ungated GS dependency | `convert.rs:591-720`: Ghostscript only, no hayro fallback; not in any probed group |
| pdf→cbz / cbz→pdf | COMPLETE (GS dep for pdf→cbz) | `convert.rs:788-853,1465-1574` |
| cbr↔pdf | COMPLETE (sidecars) | `convert.rs:969-1240` |
| ebook→pdf / pdf→epub | PARTIAL | `convert.rs:2762-3040`: Calibre; `.docx` ebook & `useStirlingOfficeConvert` rejected |
| eml→pdf | PARTIAL | `eml.rs:1-17`: mail-parser + WeasyPrint; `.msg` rejected; no FileAttachment annots |
| file→pdf / pdf→word / pdf→presentation / pdf→xml | COMPLETE (engine paths) | `convert.rs:1626-1885`: unoconvert→soffice |
| pdf→text | PARTIAL | `convert.rs:1812-1825`: `lopdf::extract_text` vs Java PDFBox fidelity unverified |
| pdf→html | COMPLETE | `convert.rs:3182-3244`: `pdftohtml -c` → ZIP |
| pdf→csv / pdf→xlsx / pdf→markdown | COMPLETE | `convert.rs:3818-4176` + `tabula.rs` + `pdftext/` |
| html→pdf / url→pdf / markdown→pdf | COMPLETE | `convert.rs:1897-2645`: WeasyPrint; url SSRF host checks but subresources unchecked |
| svg→pdf | PARTIAL/DIVERGENT | `convert.rs:1249-1380`: `rsvg-convert` preferred → raster (Java Batik → vector); unprobed/ungated |
| vector↔pdf (ps/eps) | COMPLETE | `convert.rs:3047-3180`: Ghostscript |
| pdf→pdfa / pdfx | PARTIAL | `convert.rs:3324-3578`: GS pdfwrite + ICC; level-A→B fallback; strict PDF/X rejected; veraPDF optional |
| pdf→video | MISSING (intentionally — Java commented out, ffmpeg CVEs) | no route |
| print-file | MISSING | no route |
| remove-annotations / extract-pages / compare / adjust-contrast / sign / multi-tool / view-pdf | N/A — client-side or composed | frontend decomposes into routed calls |

## Findings

### A02-F01 — `crop` rejects every UI-initiated request (absent `removeDataOutsideCrop` defaults to rejected-true) — DIVERGENT · P0
Java `CropController` (deleted `67001f296e`): `removeDataOutsideCrop` default true via GS data removal. Rust `transform.rs:220-236`: absent param → `unwrap_or(true)` → BadRequest; `autoCrop` also rejected (`transform.rs:215-218`). Frontend `useCropOperation.ts:25-40` never sends `removeDataOutsideCrop`; default `autoCrop=false` → every normal crop 400s. **Coordinator-verified:** rejection path confirmed at `transform.rs:223-233`. Fix: implement GS clip+remove-data path or default absent param to clip-only; gate `autoCrop` in frontend.

### A02-F02 — `flatten` default mode (rasterise every page) is unrouted dead code; frontend default 400s — DIVERGENT · P1
Java rasterised all pages via JPDFium by default. Rust `misc.rs:290-311` BadRequest unless `flattenOnlyForms=true`; complete `flatten_raster_work` at `misc.rs:317-416` marked `#[allow(dead_code)]`. Frontend `useFlattenParameters.ts:13` defaults `flattenOnlyForms:false` → default tool call 400s.

### A02-F03 — Redaction never removes content — always overlay — DIVERGENT (security) · P1
Java primary path jpdfium `PdfRedactor` verified removal; Rust `security.rs:2584-2589,1990-1995`, `pdftext/redact.rs:1-18` always overlay. Text remains extractable unless `convertPDFToImage`/rasterisation also requested.

### A02-F04 — `pdf-to-img`, `pdf-to-cbz`, `pdf-to-cbr` require Ghostscript but availability never reflects it — DIVERGENT · P1
Java used bundled JPDFium. Rust `convert.rs:663-675,883-920` GS-only; `hayro` exists but unused here. `endpointconfig.rs` Ghostscript group (`:326-336`) omits them; they sit in always-on `Java` group (`:258-259`) → reported enabled, 500 without gs.

### A02-F05 — `svg→pdf` prefers rasteriser; engines unprobed/ungated — DIVERGENT · P2
`convert.rs:1330-1380`: `rsvg-convert` first (raster PNG page), `inkscape` fallback (vector). Neither probed; `svg-to-pdf` in no group → always "enabled", 500 when both absent; output raster where Java emitted vector.

### A02-F06 — `merge-pdfs` rejects `generateToc` and `removeCertSign=false` — DIVERGENT · P2
`merge.rs:50-62` BadRequest for both. Java generated TOC outline and could keep signatures. **Coordinator-verified:** rejection confirmed; frontend `useMergeOperation.ts:27` sends `removeCertSign: parameters.removeDigitalSignature ?? false` → default merge 400s.

### A02-F07 — `get-info-on-pdf` reports every standard non-compliant — DIVERGENT · P2
`security.rs:7-9,473-480` hardcodes `IsPDF/ACompliant:false` etc.; only `IsPDF/SECCompliant` computed. Java populated via veraPDF library. Contradicts `verify-pdf` which does call veraPDF CLI.

### A02-F08 — veraPDF-CLI-dependent endpoints degrade to "non-compliant" when CLI absent — PARTIAL · P2
`verapdf.rs:4-17`; `security.rs:3000-3008` missing CLI → `compliant:false`+error; `validate_compliance` (`security.rs:3205-3216`) fails whole pipeline step. Java: bundled library always available.

### A02-F09 — `auto-rotate-pdf` loses OSD path — PARTIAL · P2
`automation.rs:165-172` rejects `detectionMode=osd`; auto-mode undecided pages get `tesseractUnavailable` (Java ran Tesseract OSD).

### A02-F10 — Password protection: signed PDFs refused; selective removal & `preserveExisting` rejected — PARTIAL · P2
`passwords.rs:92-99,16-20`. Java `PasswordController` supported via PDFBox.

### A02-F11 — `restore-pdf-protection`, `cert-sign`, hardware signing, `print-file`, `pdf/video`, `pdf-to-ua` have no Rust implementation — MISSING · P1/P2
No routes; `UNSUPPORTED_ENDPOINTS` `endpointconfig.rs:370-382`. Java sources survive: `PasswordController.java:67`, `CertSignController.java:187`, `HardwareSigningController.java`, proprietary `ConvertPdfToPdfUaController`, `FormDetectionController`.

### A02-F12 — `eml→pdf` rejects `.msg`; attachment annotations not emitted — PARTIAL · P2
`eml.rs:96-100,154-160` (OLE2 → 400); `eml.rs:12-14` (name tree only, no page-level FileAttachment annots).

### A02-F13 — `extract-images` rejects out-of-subset image types; `img→pdf` codec subset — PARTIAL · P2
`imgextract.rs:11-23,70` + `misc.rs:443-452`: JPXDecode/CCITTFax/JBIG2/ImageMask/Indexed/ICCBased/Separation/`/Decode`/colour-key Mask → whole request 400. `convert.rs:123-149` img→pdf limited to `image`-crate codecs. Stale comments claim "forwarded to Java" (`imgextract.rs:18`) — proxy is gone.

### A02-F14 — `add-comments` supports only coordinate `/Text` annots; `anchorText` rejected — PARTIAL · P2
`misc.rs:2500-2531`. `text::page_items`/`pdftext` provide positioned text — feasible port.

### A02-F15 — `auto-redact`/`redact-execute` reject Java-valid regex syntax — PARTIAL · P2
`security.rs:2007-2017,2598-2611`: `regex` crate can't compile lookaround/backrefs/possessive → 400 vs Java `java.util.regex`.

### A02-F16 — `pdf→pdfa` level-A emits B-conformance; strict PDF/X rejected; Office-Convert paths absent — PARTIAL · P2
`convert.rs:3269-3296` (A→B fallback, matches OSS Java), `convert.rs:3527-3536` (strict PDF/X 400), `useStirlingOfficeConvert` rejected/ignored.

### A02-F17 — `compress-pdf lineArt=true` rejected; layout/booklet don't copy form fields — PARTIAL · P3
`misc.rs:475-477`; `layout.rs:123-124`.

### A02-F18 — `pdf→text` uses `lopdf::extract_text`, not PDFBox-fidelity stripper — UNVERIFIED→PARTIAL · P3
`convert.rs:1812-1825`. `pdftext/` and `text::page_items` exist and are more faithful — likely cheap win.

### A02-F19 — `url→pdf` WeasyPrint subresources bypass SSRF checks — UNVERIFIED · P3
`convert.rs:2160-2169` `--allowed-protocols data,http,https` lets fetched page pull arbitrary hosts incl. internal IPs; only top-level URL validated (`convert.rs:2218-2260`). `fetch_remote_html` reads unbounded body (`convert.rs:2377-2380`).

### A02-F20 — Stale docs/comments claim Java-proxy forwarding that no longer exists — DIVERGENT (docs) · P3
`migration/reports/JAVA_DEPENDENCIES.md:33-40`; `imgextract.rs:18`; `pdftext/redact.rs:18`. Proxy removed per `MIGRATION.md:76`; behaviour is 400.

## Java features with no Rust equivalent (consolidated)

cert-sign + 3 hardware-signing endpoints · restore-pdf-protection · print-file · pdf/video · pdf-to-ua (proprietary) · form-detection · ingest · orchestrate/tools/external-api-call/purview-* · Stirling Office Convert engine (`useStirlingOfficeConvert`) · OLE2 `.msg` · Tesseract OSD auto-rotate · text stamps/watermarks · merge `generateToc`/`removeCertSign=false` · crop `autoCrop`/`removeDataOutsideCrop` · selective password ops · `anchorText` comments · strict PDF/X · compress `lineArt` · flatten raster default · jpdfium verified redaction · veraPDF *library* (CLI approximation) · ImageIO full codec coverage · ZXing (→`rqrr`) · OOXML docx sanitizer · proprietary/SaaS suite (~50 controllers).

## Could not verify

Byte-level output parity of lopdf rewrites vs PDFBox; whether surviving Java controllers still boot/serve; `pdf→text`/`pdf→markdown` fidelity on corpora; `hayro` rendering correctness vs JPDFium for complex PDFs.
