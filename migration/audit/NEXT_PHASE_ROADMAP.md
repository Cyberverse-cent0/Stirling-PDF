# NEXT PHASE ROADMAP

Ordered development phases with dependencies, priorities, and acceptance criteria. Derived from all 10 agent reports; deduplicated in `REMAINING_WORK_MASTER.md`.

**Prerequisite for everything:** the working tree is mid-flight and `cargo check --all-targets` currently fails. Phase 0 is stabilization, not new work.

---

## Phase 0 — Stabilize (BLOCKER)

**Goal:** committed, compiling, tested tree before any migration continues.

- Land or revert in-flight work: `signature.rs` (fix `der::reader::Reader` import → `der::Reader`), `apitests.rs:254`, `passwords.rs` restore-protection, `pdfua.rs`, `timestamp.rs`, `endpointconfig.rs` UNSUPPORTED trim, `handlers/*` edits.
- Remove `eprintln!` debug from `timestamp.rs:336-339`.
- Acceptance: `cargo check --workspace --all-targets` clean; `cargo clippy --all-targets --all-features -- -D warnings` clean; `cargo fmt --check` clean; `cargo test` 100% pass (incl. `unrouted_endpoints_report_not_supported`); commit.

## Phase 1 — P0 fixes (security + broken defaults)

| # | Work | IDs | Acceptance |
|---|---|---|---|
| 1.1 | Fix path-traversal filename in `BlankRasterizer::probe` + audit ALL uses of `upload.name`/filename for path joins | A07-F01 | `../../x` filename test writes inside tempdir only; grep shows no raw filename→path joins |
| 1.2 | Fix crop/flatten/merge default-request rejections (implement GS crop-remove-data path or fix defaults; route flatten raster or fix frontend default; implement or accept merge `generateToc`/`removeCertSign=false`) | A02-F01/F02/F06 | default UI calls for all three tools return 200 with correct output on fixture PDFs |
| 1.3 | Decide + fix Java `InternalApiClient` self-dispatch → retarget to Rust :8081 (configurable base + `RUST_API_KEY`/`X-API-KEY` compat) OR formally gate policy/MCP/AI-workflow features | A05-F02 | policy pipeline executing a migrated tool (e.g. merge) succeeds end-to-end |
| 1.4 | Add aggregate request limits: total body cap, part-count cap, `RequestBodyLimitLayer`/`ConcurrencyLimitLayer` | A07-F08, A04-F01 | oversized/many-part uploads rejected 413/400; limits configurable |

## Phase 2 — Core-tool functional parity

Close the divergences users hit on common tools:

- Redaction: implement real content removal path (or gate overlay-only behind explicit param + UI warning); fix Java-regex compatibility or document subset | A02-F03, A01-F04, A02-F15
- Engine gating: add pdf→img/cbz/cbr + cbr-extractors + svg engines to probe groups/alternatives; reconcile `group_table()`/`alternatives()` with actual Rust engine usage | A02-F04, A03-F02/F03, A03-F01
- Text stamps/watermarks; anchorText comments; selective password ops; pdf→text fidelity (use `pdftext/`); extract-images/img→pdf codec coverage | A02-F12–F18
- `fileId` chaining param across handlers (or formally descope async chaining) | A01-F03
- `?async=true` wiring decision: extend to all POST handlers or narrow contract | A06-02, A01
- qpdf env-arg bug (`misc.rs:857`); compress `lineArt`; layout/booklet form fields | A07-F21, A02-F17

**Acceptance:** parity spot-check harness (merge-base Java build per MIGRATION.md) passes for top-20 tools; no default-UI-request 400s remain.

## Phase 3 — Jobs & persistence hardening

- CSPRNG job/file IDs + ownership model decision | A07-F04, A06-12, A04-F20
- Job runner: semaphore+queue, ~20-min timeout, AbortHandle cancel, catch_unwind, completion gated on job existence | A06-02/03/08
- Retention sweeper (Java parity: 30-min results / 24-h pending, 10-min sweep) | A06-01
- Storage: move result payloads to disk (`<base>/job-files/`), metadata-only SQLite, WAL+transactions, lazy hydration, stream downloads | A06-04/05/14, A04-F02/F04
- ZIP result explosion parity (`TaskManager.setFileResult`) or documented contract | A06-07
- Sync→async boundary fixes: `spawn_blocking`/`tokio::fs` for pipeline/scanner/platform/jobstore paths | A04-F05, A04-F07

## Phase 4 — Security hardening

- Route all engine calls through `EngineSpec` (kill the 8 raw `Command` sites); uniform `-dSAFER`/`--permit-file-*`; bounded output capture | A07-F02/F03, A04-F01, A04-F06
- Zip-bomb budget on ALL extraction paths (pipeline, CBZ, HTML-ZIP, EPUB/CBR) | A07-F10
- Signature revocation truthfulness: implement OCSP/CRL w/ SSRF guardrails OR report `not-checked` | A07-F05, A01-F05
- HTML/SVG/Office sanitization parity or documented risk acceptance | A07-F11/F12, A07-U04/U05/U07
- `frontendUrl` trusted-proxy gating; loopback-only internal dispatch; constant-time API-key compare; fail-closed option for secured deployments | A07-F13/F14/F15, A04-F14
- `cargo audit`/`deny` in CI | A07-U01, A09

## Phase 5 — Platform & integration completeness

- `/v3/api-docs` port (unblocks SwaggerUI + future codegen off Java) | A05-F11
- AI proxy decision: port `/api/v1/ai/*` to Rust engine or force `aiEngineEnabled:false` in app-config | A08-F03
- `cert-sign` family: port (crypto deps already in Cargo.toml, orphaned `certsign.rs` exists) or formally descope + remove from frontend registry | A01-F01, A02-F11, A04-F12
- form-detection-model endpoints: implement or mark NotSupported | A08-F05
- Content-Disposition RFC-5987; error-message redaction; settings fail-fast | A07-F17/F18, A04-F08
- Desktop: product decision on proprietary endpoints (Rust sidecar 404s ~150 routes) | A05-F07

## Phase 6 — Testing, CI & release integrity

- Wire `stirling test` smoke into `rust:check`/CI; route-table regression test | A09-F04
- Fix live E2E/cucumber target (Rust backend) or formally quarantine | A09-F01
- Fix generated-artifact pipeline: spec source of truth post-Java, regen + freshness-gate Rust engine files, extend `check-generated-models` | A09-F02, A10-F03/F13
- Release: gate/replace JAR publishing; add `stirling` CLI to release matrix; sidecar bundling decision | A09-F03/F09, A03-F05/F14
- Coverage: pdftext direct tests, engine-spawn tests on Windows, security regression tests for Phase 1/4 fixes | A09-F07
- `task backend:check` on stripped Java tree | A09-F10

## Phase 7 — Debt & cleanup (per TECHNICAL_DEBT_REPORT §6)

Stabilize → freshness gate → docs/hygiene → dead tooling → dep prune → split_photos.py → runtime/ resolution → engine unification (blocked: codegen repoint + cucumber + board decision) → **Java retirement LAST**.

## Phase 8 — Java removal (gated on product decision)

See `JAVA_REMOVAL_READINESS.md` §5–6. Preconditions: proprietary/saas fate decided; InternalApiClient retargeted; codegen off Java; dev/E2E/release rewired; security-endpoint parity signed off; dual job stores resolved.

---

## Cross-phase dependencies

```
Phase 0 ──► everything
Phase 1.3 (InternalApiClient) ──► Phase 8 (Java removal) AND proprietary automation works at all
Phase 2 (fileId/async) ──► Phase 3 (jobs) — async contract must settle first
Phase 5 (/v3/api-docs) ──► Phase 6 (codegen) ──► Phase 8 (app/ deletion)
Phase 4 security ──► any public/exposed deployment
Phase 7 cleanup ──► independent, can interleave after Phase 0
Phase 8 ──► last, product-decision gated
```

## Explicit non-goals / do-not-do

- Do NOT delete Java files, `engine/` Python, `runtime/` live compose, or `png_to_webp.py`/`calibre_sandbox.py` during earlier phases.
- Do NOT mark endpoints COMPLETE on compilation alone — use the parity harness.
- Do NOT remove `-dSAFER` "for parity" — Java's omission is not a spec.
- Do NOT expose Rust directly to multi-user traffic until Phase 4 auth/IDOR work lands (Java-in-front is the current security boundary).
