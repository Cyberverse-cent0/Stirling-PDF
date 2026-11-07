# TECHNICAL DEBT REPORT

Sources: agent-04 (Rust architecture), agent-10 (cleanup), agent-09 (docs/build drift), agent-03 (engines), agent-06 (jobs).

## 1. Architectural assessment

The Rust architecture is **fundamentally sound**: layered handlers→shared libs, `#[endpoint]` macro centralizes the blocking/async boundary, `EngineSpec` centralizes subprocess execution (argv-only, timeouts, kill-on-timeout, output draining), temp files are RAII (`NamedTempFile`/`TempDir`), error mapping is consistent (`AppError`→`application/problem+json`, hides internals). Zero `unsafe` in server/engine.

Debt concentrates in five areas: god files, bypassed shared infrastructure, unbounded resources, silent failure modes, and generated-artifact sprawl.

## 2. Structural debt

| Item | Evidence | Class | Pri |
|---|---|---|---|
| God files mixing HTTP+domain+tests | `form.rs` ~5.3k, `convert.rs` ~5.2k, `security.rs` ~4k, `misc.rs` ~3.7k LOC | DIVERGENT (maintainability) | P2 |
| Duplicated multipart param helpers | `last_param`/`param_opt`/bool parsers reimplemented in misc/security/automation/passwords/layout vs canonical `upload.rs:254-310` | DEPRECATED (duplication) | P3 |
| Ad-hoc file validation | empty-check+message variants across upload/pdfdoc/misc/security/form | PARTIAL | P3 |
| `certsign.rs` orphaned (~386 lines, `unreachable!()`), not declared in main.rs | crypto deps `pkcs12/pkcs8/cbc/des/rc2/hmac/const-oid/http-body-util` unused in compiled code | DEPRECATED | P3 |
| Stale comments/docs | `main.rs:20-23` claims pdftext "not yet routed" (31 use sites); `endpointconfig.rs:365-369` doc lists composed keys now removed from UNSUPPORTED; `imgextract.rs:18`/`pdftext/redact.rs:18`/`JAVA_DEPENDENCIES.md` claim Java-proxy forwarding that no longer exists | DIVERGENT (docs) | P3 |
| `migration/reports/` 48 files, self-contradictory/superseded | e.g. "cargo check fails 23 errors" vs "clean"; `MIGRATION_BOARD.md` is authoritative | DEPRECATED | P2 |
| `runtime/` "archived" but live | `ai-engine.yml`, `nightly.yml`, `build-enterprise.yml`, `.taskfiles/e2e.yml` still use `runtime/docker/engine`, `runtime/testing/compose` | DIVERGENT | P2 |
| `testing/parity.sh`, `scripts/rust_pdf_parity.py`, `rust/examples/{pdfdiff,pdf_fingerprint}` | require Java endpoints that no longer exist | DEPRECATED | P2 |
| `testing/` orphans (~10), `scripts/` orphans (~20), `devTools/` | zero refs outside reports/labeler configs | DEPRECATED/UNVERIFIED | P3 |
| `app/core/.../split_photos.py` | shipped but no Java caller; Rust port live | DEPRECATED | P2 |
| `png_to_webp.py`, `calibre_sandbox.py` | **live** — Java tests exercise them; do NOT delete | — | — |
| `.gitignore` gaps | `rust/pdfium-native/target/` stale; root `logs/`, `storage/` uncovered | PARTIAL | P3 |
| `rust/src/bin/` empty dir | cosmetic | BROKEN (cosmetic) | P3 |
| `EndpointConfiguration.java` stale groups | still assigns migrated endpoints to Java groups | DEPRECATED | P3 |

## 3. Resource-management debt

| Item | Evidence | Pri |
|---|---|---|
| Job results: full body buffered (`to_bytes(usize::MAX)`), stored as `Vec<u8>` in RAM map AND SQLite BLOB, `load_all` hydrates all at boot, full clone per download | `jobs.rs:218,35`; `jobstore.rs:173-207`; `jobs.rs:373,438` | P1 |
| Job persistence non-atomic; no WAL/busy_timeout; blob-write failure warn-only | `jobstore.rs:72-118` | P2 |
| Upload: per-part cap only; no aggregate/part-count/total-body caps; sync `write_all`+`sync_all` on Tokio workers; uncapped param Vecs; scanner spools before limits checked | `upload.rs:38-80,164-212`; `form.rs:1316-1324`; `automation.rs:776-816` | P1/P2 |
| Engine runner: timeout kills direct child only; grandchildren holding pipes → `recv()` blocks forever; output drained unbounded; no process-group kill | `convert.rs:196-256` | P2 |
| 8 direct `Command` sites bypass `EngineSpec` timeouts entirely | `misc.rs:717,857,2262,2817`; `automation.rs:441,466` | P1 |
| Sync FS/archive/SQLite inside async handlers (pipeline ZIP extract/write, scanner mutex-held dir ops, platform read_dir, jobstore rusqlite) | `pipeline.rs:186-720`; `automation.rs:852-1228`; `platform.rs:329-561` | P2 |
| Engine (rust/engine) sqlite vec store: sync rusqlite under tokio Mutex | `documents/sqlite_vec_store.rs:295-972` | P2 |
| Settings/config: YAML read/parse failure → silent defaults (security flags silently reset); bad PORT→silent 8081; bad HOST→bind panic | `settings.rs:591-705`; `config.rs:101-145` | P2 |
| No global body/concurrency limits (`RequestBodyLimitLayer`/`ConcurrencyLimitLayer` absent) | `main.rs:98-151` | P1 |
| Unbounded `tokio::spawn` job fan-out; no semaphore/queue | `jobs.rs:214` | P2 |

## 4. Cross-layer duplication & drift

| Item | Detail | Pri |
|---|---|---|
| Dual AI engines | `rust/engine` (live default) vs `engine/` Python (parity oracle, fully wired in ~14 workflows). Contract changes made twice. Retirement blocked by Rust-engine codegen reading Python models (`gen_tool_io.py`, `gen_tool_schemas.py`, `gen_tool_texts.py`) | P2 |
| Dual job stores | Java `JobStore`/H2/TaskManager vs Rust SQLite `stirling-jobs.db` — no cross-resolution; ~10 endpoints dual-implemented (security family) with semantic drift | P1 |
| Generated-artifact sprawl | 5+ generated surfaces (toolApiTypes.ts, toolIO.ts, tool_models.py, tool_io.py, pipeline_table.rs, tool_{io,params,schemas,texts}.rs/json) fed by Java SwaggerDoc; only TS+Python freshness-gated — **Rust generated files have NO CI check** | P1 (process) |
| Engine group/alternative table | `endpointconfig.rs` `group_table()`/`alternatives()` transcribed from Java but Rust handlers use different engines → wrong enable/disable gating | P2 |
| Engine probe duplication | `engine_available` may run `--version`, then `probe_engine` runs it again — 11 engines × 2 spawns at startup | P3 |
| CLI vs server engine detection | `cli/src/services/engine_detector.rs` `EngineKind` lacks weasyprint/ocrmypdf/pdftohtml/ebook-convert/verapdf/rar-archivers/rsvg/inkscape | P3 |
| Text extraction ×4 | `pdftext/` (markdown), `text.rs` (positioned runs), engine `page_text.rs`, Python `_page_text.py` — distinct consumers, only engine pair is true duplication | informational |

## 5. Dead/unused inventory (verify before delete — see agent-10 report for grep evidence)

- `magick` engine: probed, bundled (tier-1), CLI-installable, documented — **zero invocations** in rust/src.
- `ffmpeg`: dead/obsolete both sides (pdf→video commented out).
- Unused Rust deps: `pkcs12, pkcs8, cbc, des, rc2, hmac, const-oid, http-body-util` (verify `cargo machete`).
- `certsign.rs` orphan module.
- `testing/parity.sh`, `scripts/rust_pdf_parity.py`, `rust/examples/pdfdiff.rs`, `pdf_fingerprint.rs`.
- ~10 `testing/` orphans, ~20 `scripts/` helpers, all of `devTools/`.
- `split_photos.py` (Java resources) — Rust port live, no Java caller.
- Dead Java remnants: `scripts/PropSync.java`, `find-free-port.*`, `init*.sh`, unused `SPDF/model/api/**` DTOs (verify imports), stale `EndpointConfiguration` groups.
- `python`/`cv2` vestigial engine probe (no Rust python call sites).

## 6. Safe cleanup sequence (recommended, NOT executed)

1. **Stabilize**: land/commit in-flight Rust work; green `task rust:check` + `task rust:engine:check`.
2. **Freshness gate** for the 5 ungated Rust generated files (cheap, prevents silent drift).
3. **Docs/hygiene (zero risk)**: archive `migration/reports/` history; fix `.gitignore`; `rmdir rust/src/bin`; remove `devTools/` + config refs.
4. **Dead tooling**: parity scripts → testing orphans → scripts triage (keep `lint/`, `db-migration/`, `export-ffdetr-onnx.py`, `run.sh`, `find-free-port.sh`).
5. **Dep prune**: `cargo machete`/`udeps`; remove confirmed-unused or annotate reserved-for-cert-sign.
6. **`split_photos.py`** removal after confirming Java `extract-image-scans` unreachable.
7. **Resolve `runtime/`** archive-vs-live contradiction (split live compose from archived docker/workflows).
8. **Engine unification** — blocked on codegen repointing + cucumber migration + board decision.
9. **Java retirement** — last (buildSrc, app/, gradle CI, Taskfile backend tasks).

## 7. What is NOT debt

- `rust/src/pdftext/` (34 files) — single port of Java PDFTextStripper, used by markdown/security/convert.
- `lopdf`/`hayro`/`pdfdoc.rs` layering — complementary, no duplication.
- `rust/cli` vs `stirling-server` — dev-ops CLI vs server, intentional.
- Cargo workspaces ×4 — intentional isolation.
- Proprietary/saas Java modules — product platform, not migration debt (see JAVA_REMOVAL_READINESS).
