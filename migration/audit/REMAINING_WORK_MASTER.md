# REMAINING WORK MASTER — Deduplicated Finding List

Every finding from `AGENT_REPORTS/agent-01…10` deduplicated and re-prioritized. IDs preserve agent prefixes (A0x-Fyy / A0x-NN). **Status note:** the working tree changed during the audit — items marked ✅FIXED-IN-TREE were resolved by uncommitted work observed at audit end; items marked 🔄VERIFY need re-check once the tree stabilizes.

Severity: P0 critical · P1 high · P2 medium · P3 low.

---

## P0 — must fix before anything else

| ID | Finding | Evidence | Status |
|---|---|---|---|
| A07-F01 | **Path traversal → arbitrary file write.** `BlankRasterizer::probe` joins raw multipart `upload.name` into temp path (`misc.rs:2751-2753`); callers pass unsanitized filename (remove-blanks `misc.rs:2682`, scanner-effect `scanner.rs:156`, auto-split `automation.rs:1279`, image-scans `image_scans.rs:177`, ocr `ocr.rs:303`). Unauthenticated in open deployment. | verified by coordinator | OPEN |
| — | **Working tree does not compile** (`cargo check --all-targets`): `signature.rs:22` bad `der::reader::Reader` import; `apitests.rs:254` type errors; earlier `passwords.rs:800` unawaited future. In-flight signature/pdfua/timestamp work. | coordinator run | OPEN — stabilize first |
| A05-F02 | **Java `InternalApiClient` loops tool dispatch to Java's own port** where migrated controllers are deleted → policy pipelines, folder pipelines, MCP tool execution, AI-workflow sub-steps 404 for every migrated tool. No Java→Rust client exists. | `InternalApiClient.java:57-61,121-124,239-248`; callers `PolicyExecutor.java:285`, `McpOperationExecutor.java:104` | OPEN — blocks proprietary automation |

## P1 — high

### Broken/missing functionality
| ID | Finding | Evidence |
|---|---|---|
| A02-F01 | `crop` 400s on every default UI request — absent `removeDataOutsideCrop` defaults to rejected-true; `autoCrop` rejected; frontend never sends the param | `transform.rs:215-236`; `useCropOperation.ts:25-40` |
| A02-F02 | `flatten` default raster mode unrouted dead code; frontend default `flattenOnlyForms:false` → 400 | `misc.rs:290-318`; `useFlattenParameters.ts:13` |
| A02-F06 | merge rejects `generateToc` + `removeCertSign=false`; frontend sends `removeCertSign:false` by default → default merge 400s | `merge.rs:50-62`; `useMergeOperation.ts:27` |
| A01-F03 | `fileId` request param unsupported on every Rust endpoint — async/pipeline chaining fails 400 across whole tool surface | `upload.rs:191-251`; zero `fileId` handling in handlers |
| A06-02 / A01 | `?async=true` honored by only 2 endpoints (`pipeline/handleData`, `update-enable-analytics`); ~140 POST handlers ignore it vs Java `@AutoJobPostMapping` on ~92 | `jobs.rs`, `pipeline.rs:737`, `platform.rs:1211` |
| A01-F01 / A02-F11 | `cert-sign` + 3 hardware-signing routes missing (403); frontend registers certSign tool | `endpointconfig.rs:372`; `CertSignController.java`, `HardwareSigningController.java` |
| A01-F02 / A08-F01 | ~~`restore-pdf-protection` missing → every op on encrypted PDF failed~~ | ✅FIXED-IN-TREE (`passwords.rs:616+`, `mod.rs:232`) — needs commit + verification |
| A08-F02 | ~~UNSUPPORTED_ENDPOINTS disabled client-side composed tools (automate/compare/remove-annotations/adjust-contrast/multi-tool/view-pdf/pdf-to-rtf)~~ | ✅FIXED-IN-TREE (list trimmed to cert-sign/ingest/orchestrate/tools/external-api-call/purview-*) — verify test now passes |
| A02-F04 | pdf→img, pdf→cbz, pdf→cbr require Ghostscript but aren't in probed groups → enabled endpoint → 500 without gs | `convert.rs:663-675,883-920`; `endpointconfig.rs:258-259,326-336` |
| A03-F03 | cbr→pdf extractors (unrar/unar/bsdtar/7z) unprobed/ungated → 500; pdf-to-cbr needs gs+rar, also ungated | `convert.rs:967-1224`; `endpointconfig.rs:319` |
| A09-F02 | Generated tool-model surface collapsed to 7 endpoints (Java OpenAPI source lost ~100); `toolIO.ts`/`toolApiTypes.ts`/`tool_models.py` shrank; Rust engine `tool_schemas.json`/`tool_params.rs` stale and NOT covered by freshness CI | `tool_models.py:250-302`; `check-generated-models.yml`; A10-F03 |
| A09-F01 | Live E2E (`e2e-live.yml`, `playwright-e2e-live` required check) + nightly cucumber still boot Java and exercise deleted endpoints → red CI or silently lost coverage; no Rust-backed live E2E | `.taskfiles/e2e.yml:50-163`; `nightly.yml:154-176` |
| A05-F01 | Default dev loop points frontend at Java :8080 (`task dev`, `dev:strangler`) which no longer serves migrated endpoints → every tool call 404s in default dev | `Taskfile.yml:72-87,216-243,465-474` |

### Security
| ID | Finding | Evidence |
|---|---|---|
| A07-F02 / A04-F01(part) | 8 direct `Command` engine call sites bypass `EngineSpec` timeouts (`misc.rs:717,857,2262,2817`, `automation.rs:441,466`) — hung gs/qpdf pins workers; unbounded output capture | agent-03/04 file:line |
| A07-F03 / A03-F11 | `-dSAFER` applied inconsistently across gs invocations (present in ~6 sites, absent in ~9) | `convert.rs`, `misc.rs`, `ocr.rs`, `automation.rs` |
| A07-F04 / A06-12 / A04-F20 | Predictable job/file IDs (timestamp+counter, UUID-shaped) → enumerable → IDOR on `/general/files/{id}`, job results; also collide across restarts | `jobs.rs:157-170` |
| A07-F05 / A01-F05 | Signature revocation reports `revocationChecked:true`/`"good"` with **no OCSP/CRL/AIA** — false assurance; Java did real PKIXRevocationChecker | `signature.rs:1336-1352` |
| A07-F08 | No total request-size/part-count/concurrency limits (Java had `maxFileSize`+`maxRequestSize`); per-part cap only | `upload.rs`; `main.rs` lacks limit layers |
| A07-F10 | ZIP decompression lacks byte/entry caps in pipeline, CBZ, HTML-ZIP paths (Markdown ZIP HAS caps — guard exists but inconsistent) | `pipeline.rs:186-220`; `convert.rs:1494-1530,1960-2008` vs `2691-2705` |
| A07-F09 / A06-01/02/04/08 | Jobs: no timeout, no concurrency bound, results fully buffered in RAM + SQLite BLOB, no retention sweeper (Java: 30-min/24-h expiry every 10 min), panics leave stuck records, cancel doesn't abort work and leaks orphan blobs | `jobs.rs`, `jobstore.rs` |
| A07-F15 | Open-by-default auth; `RUST_API_KEY` single shared key, non-constant-time compare; proprietary/SaaS markers warn-only — fail-open if exposed directly | `auth.rs:73-118` |

### Redaction (security-relevant divergence)
| A02-F03 / A01-F04 | Redaction never removes underlying content — always overlay; text remains extractable unless rasterisation also requested. Java preferred verified JPDFium removal | `security.rs:1990-1999,2580-2589`; `pdftext/redact.rs` |

## P2 — medium

### Functional gaps
| ID | Finding |
|---|---|
| A02-F05 | svg→pdf prefers rasteriser (rsvg-convert), unprobed/ungated, output raster vs Java vector |
| A02-F07 | get-info-on-pdf hardcodes all compliance standards false |
| A02-F08 / A03-F04 | veraPDF CLI unprobed/unbundled/unconfigurable; absent → non-strict silently skips, strict errors; PDF/A outputs unvalidated |
| A02-F09 | auto-rotate loses OSD path (`detectionMode=osd` rejected) |
| A02-F10 | password ops: signed PDFs refused, selective removal & preserveExisting rejected |
| A02-F11 | MISSING: print-file, pdf→ua (proprietary), form-detection; restore-pdf-protection ✅in-flight |
| A02-F12 | eml→pdf: `.msg` rejected, no FileAttachment annots |
| A02-F13 | extract-images/img→pdf codec subsets → whole-request 400s |
| A02-F14 | add-comments: only coordinate `/Text`; `anchorText` rejected |
| A02-F15 | auto-redact rejects Java-valid regex syntax |
| A02-F16 | pdf→pdfa level-A→B fallback; strict PDF/X rejected; OfficeConvert paths absent |
| A06-05/06/07/09/10/11/13/14/15 | Jobs: non-atomic persistence, silent memory-only degradation, ZIP results not exploded (divergent `/result` contract), no ownership checks, epoch-vs-ISO timestamps, malformed-JSON→`result:null`, full-byte clones per download, /tmp orphans on crash |
| A08-F03 | AI endpoints exposed in UI (`aiEngineEnabled` can be true) but Rust has no `/api/v1/ai/*` routes → 403/404; availability key `classify-and-label` ≠ guard key `tools` |
| A08-F04 | ~150 proprietary/saas endpoints 404 on Rust; processor portal still issues calls (mostly gated by app-config flags) |
| A03-F02 | Engine group/alternative table copied from Java mis-gates endpoints (pdf-to-markdown needs pdftohtml it doesn't use; auto-rotate gated on tesseract it doesn't need; html/url→pdf gated on python probe) |
| A05-F03/F04 | Two job stores (Java H2 vs Rust SQLite), ~10 endpoints dual-implemented with drift — needs single-owner decision |
| A05-F05 / A10-F13 | `task tool-models` + frontend/engine codegen requires booting Java for `/v3/api-docs` → deleting app/ breaks codegen chain |
| A05-F06 | Release pipeline still publishes `Stirling-PDF*.jar` (stripped backend 404s migrated ops) alongside rust-release binaries |
| A05-F07 | Desktop defaults Rust sidecar → all proprietary endpoints 404; ~553 proprietary call sites — needs product decision |
| A09-F04 | No route-table/smoke coverage in CI; `rust/tests/integration/` empty; binary-crate structure prevents in-process router tests |
| A09-F09 | `stirling` CLI not in release artifacts though DEPLOYMENT.md tells operators to use it |
| A10-F01/F02 | Dual AI engines (rust/engine live, Python engine/ parity oracle); Python retirement blocked by ~14 CI/workflow deps + Rust-engine codegen reading Python models |
| A10-F04 | `runtime/` declared "archived" but still load-bearing in ai-engine/nightly/build-enterprise/e2e compose |

### Security (continued)
| A07-F07 | url→pdf SSRF: subresources via WeasyPrint `--base-url`+`http,https` bypass host checks; unbounded body; DNS TOCTOU |
| A07-F11 | HTML sanitization not ported (WeasyPrint gets raw HTML; comrak `unsafe=true`) — Java had OWASP allowlist |
| A07-F12 | SVG sanitization is substring matching vs Java parser-based sanitizer |
| A07-F13 | `frontendUrl` trusts Host/X-Forwarded-* unconditionally → cacheable poisoning |
| A07-F14 | `RUST_API_KEY` sent plaintext HTTP on pipeline self-dispatch if bound non-loopback |
| A07-F06 | PKIX-lite path building omits name/policy constraints, pathLen, critical-extension policy |
| A04-F06 | Engine runner: timeout kills only direct child; grandchildren holding pipes → `recv()` blocks forever |
| A04-F07 | Engine SQLite vec store does sync rusqlite under tokio Mutex in async methods |
| A03-F06 | Untrusted archive extraction (unrar/unar/bsdtar/7z) — no post-extraction containment; unpinned binaries |
| A03-F14 | macOS universal desktop builds ship single-arch engines → Intel Macs silently lose engine features |

### Architecture/debt
| A04-F01 | Multipart: per-part cap only, no aggregate/part-count caps; sync spool I/O on Tokio workers; uncapped param Vecs |
| A04-F02 | Job results: full `to_bytes(usize::MAX)` + `Vec<u8>` in memory map AND SQLite BLOB; `load_all` hydrates all blobs at boot |
| A04-F04 | Job persistence non-atomic (no transaction, no WAL); tolerates inconsistent blob/ref state |
| A04-F05 | Hand-written async paths do sync FS/archive/SQLite on Tokio workers (pipeline ZIP, scanner, platform, jobstore) |
| A04-F08 | Settings/config load silently swallows YAML errors → security flags silently reset; bad PORT→8081, bad HOST→panic |
| A04-F09 | God files: form.rs 5.3k, convert.rs 5.2k, security.rs 4k, misc.rs 3.7k lines |
| A03-F09 | Rust drops Java's process-concurrency machinery (semaphores, UnoServer pool, Calibre/LibreOffice sandbox confinement) |

## P3 — low

| ID | Finding |
|---|---|
| A01-F07/F08/F09/F10 | Job timestamps epoch-vs-ISO, query-param binding gaps (`?tsaUrl` ignored), `eprintln!` debug left in timestamp.rs, missing `filename*` RFC-5987 encoding |
| A02-F17/F18/F19/F20 | compress lineArt, layout/booklet form fields, pdf→text fidelity, url→pdf subresources, stale "forwards to Java" comments |
| A03-F01 | `magick` probed/bundled/installable but never invoked — dead dependency |
| A03-F05/F07/F08/F10/F12 | Unpinned bundled engine versions; dead `--version` fallback; Windows `.bat` resolution; version-floor bypass via customPaths; CLI engine detector under-reports |
| A04-F10–F21 | Duplicated `last_param`×3 + boolean parsers; ad-hoc file validation; orphaned `certsign.rs` + unused crypto deps; stale dead-code comments; non-constant-time API-key compare; engine config-cache Unix-only import (Windows engine build break); engine error detail leaks; CLI `run_with_timeout` pipe deadlock; CLI Windows `CommandExt` import break; CLI unsanitized component names; engine `--version` probed twice per binary |
| A06-10/12/13 | epoch timestamps, fabricated UUIDs, malformed-JSON→null |
| A07-F16–F21 | actuator-prefix exemption, Content-Disposition sanitization, error reflection of params, eprintln, legacy crypto (RC4-40/DES/RC2/SHA1/PKCS12-MAC-unverified), qpdf env-arg bug |
| A08-F05/F06 | form-detection-model 404s; stale frontend preload/availability refs |
| A09-F05/F06/F07/F08 | Doc drift (MIGRATION.md/TESTING.md/runtime README/main.rs comments); stale Java EndpointConfiguration groups; pdftext/CLI/route coverage gaps; clippy flag drift between task and CLI gates |
| A10-F05–F14 | Dead parity tooling, testing/ orphans, scripts/ orphans (~20), devTools/, migration/reports archive, split_photos.py dead script, unused crypto deps, .gitignore gaps, SwaggerDoc linchpin, empty src/bin |

## Cross-cutting themes (dedup view)

1. **Snapshot instability** — audit ran against a moving tree; ~6 findings already fixed in-flight (restore-pdf-protection, UNSUPPORTED trim, timestamp/signature/accessibility routes). Re-verify all "OPEN" items after next commit.
2. **Java-retirement blockers** — InternalApiClient dispatch, SwaggerDoc codegen, dev/E2E/release wiring, proprietary/saas platform, dual job stores, ~10 dual-implemented endpoints.
3. **Jobs need a redesign pass** — persistence is real but semantics (timeout/cancel/retention/ownership/storage) lag Java.
4. **Security hardening pass** — one P0 traversal, inconsistent engine sandboxing/timeouts, missing request limits, zip budgets, sanitization gaps, fabricated revocation status.
5. **Generated-artifact governance** — 5 generated surfaces, only some freshness-gated; spec source is Java.
