# A05 — Java Migration & Removal Readiness (round 2, 2026-11-07)

## 1. Verification of headline claims

| Claim | Verdict | Evidence |
|---|---|---|
| ~83 controllers: 9 core / 53 proprietary / 21 saas | Confirmed with nuance | `*Controller.java` glob: 8 security + `JobController` = 9 "core"; 53 proprietary; 21 saas = 83. **Plus 2 endpoint classes not matching `*Controller` suffix**: `app/core/.../model/api/converters/ConvertPDFToMarkdown.java` (`@ConvertApi`, `/api/v1/convert/pdf/markdown`) and `app/proprietary/.../ConvertPdfToPdfUa.java` (`/api/v1/convert/pdf/ua`) → **85 live endpoint classes**. |
| Migrated controllers deleted | Confirmed | `app/core` only the 9 endpoint classes above; all general/misc/convert(except markdown)/filter/form/analysis controllers gone. |
| Rust router is the migrated surface | Confirmed | `rust/src/handlers/mod.rs:32-385` ~150 `/api/v1/**` routes; fallback 404s unmigrated `/api/**`; Java proxy gone — no Rust→Java path (`rust/MIGRATION.md:76-83,114-116`). |
| Java still builds/runs | Structurally yes; not executed | `settings.gradle:89-98`, `app/*/build.gradle`, `buildSrc/` intact; CI still builds/tests/packages it (`backend-build.yml`, `multiOSReleases.yml:176`, `db-migration-test.yml:75`, `build-enterprise.yml:181,256,288`, `tauri-build.yml` jlink legs). |

## 2. Java inventory table

| Area | Status | Evidence |
|---|---|---|
| Migrated op controllers (merge, split, convert, form, filter, analysis, misc, platform GETs) | safe-to-remove (already removed) | Deleted from `app/core`; Rust covers them. |
| `ConvertPDFToMarkdown` (`app/core/.../model/api/converters/`) | verify → likely safe-to-remove | Live `@ConvertApi` duplicating Rust `convert::pdf_to_markdown` (`mod.rs:307-310`); misplaced in `model/` package. |
| Core security crypto: `CertSignController`, `HardwareSigningController`, `PasswordController::restore-pdf-protection` | not-migrated (Java-only) | No Rust routes; `endpointconfig.rs:370-380` UNSUPPORTED. |
| Core security overlap: `VerifyPDF`, `ValidateCompliance`, `ValidateSignature`, `Timestamp`, `Redact` (+auto-redact, redact-execute), `prepare-pdf-session` | partial / divergent | Same paths routed in Rust AND still compiled in Java → two live implementations; drift risk. |
| `JobController` + job framework (`AutoJobAspect`, `TaskManager`, `JobExecutorService`, `JobStore`, `JobOwnershipCache`, `JobOwnershipServiceImpl`) | partial — load-bearing while Java runs | `@AutoJobPostMapping` on all 8 security controllers + proprietary `AdminJobController`/policy paths; serves `/api/v1/general/job/*` which Rust also serves from `jobstore.rs` (SQLite `stirling-jobs.db`). Two job namespaces, no forwarding. |
| `app/common` (infra: cluster, FileStorage, ToolIORegistry, InternalApiClient, models, JPDFium/PDFBox utils) | partial — cannot remove while proprietary/saas stay | ~208 proprietary + 26 saas importers; contains both dead tool-services and live infra. |
| `app/proprietary` (~53 controllers + services) | proprietary-retained | Auth/users/teams/invites/licenses, account-link + entitlement gating, storage/folders/encryption-at-rest, policy engine + processing folders + sources + webhook receiver, audit, MCP server, AI agents, integrations (Purview), form-detection, notifications, workflows/signing sessions, portal/admin/usage/fleet. No Rust counterpart by design (`rust/src/auth.rs` header). |
| `app/saas` (21 controllers) | proprietary-retained | PAYG wallet/invoices/payment-method/billing/pricing, Stripe usage reporting, procurement, account-link/connect/instance/ownership-handover, AI create/proxy/internal, team ownership, user-role webhook, legal, fleet usage, OpenAI apps challenge. `settings.gradle:95-98` conditional include. |
| `ConvertPdfToPdfUa` (`/api/v1/convert/pdf/ua`) | proprietary-retained | No Rust route; premium PDF/UA tagging. |
| Filters/interceptors/AOP | proprietary-retained (Java-only) | `EndpointInterceptor`, `PdfMetricsInterceptor`, `MobileScannerUploadFilter`, `MetricsFilter` (core); `PaygChargeInterceptor`, `InstanceEntitlementInterceptor`, `UserAuthenticationFilter`, `ParticipantRateLimitInterceptor`, `DeviceCredentialAuthenticationFilter`, `AuditAspect`, `AutoJobAspect`, `InitialSecuritySetup` (proprietary/saas). Rust has only `guard.rs`/`auth.rs`/`metrics.rs`. |
| Repositories | proprietary-retained (Java-only) | ~70 Spring Data JPA `*Repository.java`, proprietary/saas; H2/Postgres. Rust `jobstore.rs` is jobs-only SQLite. |
| Config (`ApplicationProperties`, `EndpointConfiguration`, `settings.yml`, profiles) | partial | `settings.yml` + endpoint enable/disable ported (`settings.rs`, `endpointconfig.rs`); security/OAuth2/SAML2/mail/legal/system props Java-only. |
| Scheduling | proprietary-retained | ~36 `@Scheduled` sites — no Rust scheduler. |
| Cluster/HA (Valkey, S3 file store) | proprietary-retained | `proprietary/cluster/valkey/*`, `common/cluster/inprocess/*`. |
| Ops surface (Micrometer, Telegram bot, PostHog, AppUpdateService, fingerprint sessions, springdoc `/v3/api-docs`, legacy static) | partial / verify | `metrics.rs` covers `/api/v1/info/*`; `web.rs` SPA/static; **no `/v3/api-docs`** → frontend `SwaggerUI.tsx` 404s on Rust. |
| Dead/obsolete remnants | safe-to-remove candidates | `scripts/PropSync.java`, `scripts/find-free-port.{sh,ps1}`, `scripts/init*.sh`, `aot-diagnostics.sh`, unused `SPDF/model/api/**` DTOs (verify no proprietary/saas refs), `runtime/` (archived but see A10-F04 — still CI-load-bearing). `PaygCucumberThrowController` profile-gated — keep. `RestartHelper.java` wired into `build` — keep while JAR builds exist. |

## 3. Runtime-path invocation of Java (confirmed)

- `task dev`/`dev:all` → `backend:dev` → `dev:proprietary` → `./gradlew :stirling-pdf:bootRun` (`.taskfiles/backend.yml:22-55`); frontend gets `BACKEND_URL=<java port>` (`Taskfile.yml:72-87,216-243`).
- `task dev:strangler` starts Rust :8081 AND Java :8080, but `frontend:run` defaults `BACKEND_URL=http://localhost:8080` → frontend→Java (`Taskfile.yml:391-474`).
- `task backend:{run,stop,restart,build,test,check,swagger,licenses:*}`, `task build`, `task check`, `task fix`, `task format`, `task tool-models` (via `backend:swagger`) — all Gradle.
- `task e2e:live` boots `:stirling-pdf:bootRun`, waits `/api/v1/info/status` (`.taskfiles/e2e.yml:50-163`); cucumber `BASE_URL=localhost:8080` (`.taskfiles/cucumber.yml:13`); enterprise E2E + CI boot Java.
- Desktop: `DESKTOP_BACKEND` defaults `rust` (Rust sidecar only); `DESKTOP_BACKEND=java` keeps JAR+JRE/jlink (`.taskfiles/desktop.yml:43-51,92-104`; `src-tauri/commands/backend.rs:340-356`).
- Releases: `multiOSReleases.yml` still builds & uploads `Stirling-PDF{,-with-login,-server}.jar`; `rust-release.yml` ships `stirling-server`/`stirling-engine` in parallel.
- **Codegen dependency**: `task tool-models` → `backend:swagger` → `generateOpenApiDocs` (boots Java) → `SwaggerDoc.json` → `toolApiTypes.ts`/`toolIO.ts` AND engine `tool_models` (`Taskfile.yml:326-336`, `frontend.yml:596-607`, `rust/engine/scripts/generate_tool_params.sh:11-14` hits `JAVA_URL=http://localhost:8080/v3/api-docs`).

## 4. Findings

### A05-F01 — broken-wiring — P1
`Taskfile.yml:72-87,216-243,465-474`; `.taskfiles/backend.yml:22-55`; `.taskfiles/e2e.yml:50-163`; `.taskfiles/cucumber.yml:13`. Default dev/test loop points frontend at Java backend which no longer serves migrated endpoints → every tool call 404s. `dev:strangler` same flaw. Fix: repoint core dev to Rust :8081; restrict `backend:dev`/`e2e:live` to proprietary/saas or start Rust; update docs (`DeveloperGuide.md:109-122` still Java :8080).

### A05-F02 — broken-feature — P0 (for proprietary automation)
`app/common/.../InternalApiClient.java:57-61,121-124,239-248`; callers `PolicyExecutor.java:285`, `McpOperationExecutor.java:104`, `AiWorkflowService`, `PolicyEngine`. Internal dispatch loops to **Java server's own port** posting `/api/v1/{general,misc,security,convert,filter,form,…}` — endpoints whose controllers are deleted → 404. `getBaseUrl()` = `localhost:<local.server.port>`; no Java→Rust client exists. Policy pipelines, folder pipelines, MCP tool execution, AI-workflow sub-steps fail for every migrated tool on any Java deployment. Fix: retarget dispatch to configurable backend URL (Rust :8081) with `RUST_API_KEY`/`X-API-KEY` compat, or formally retire/gate these features.

### A05-F03 — divergent — P1
Two independent job stores/namespaces (Java `JobStore`/H2 vs Rust SQLite `stirling-jobs.db`); neither resolves the other's job IDs (`MIGRATION.md:246-249`). Mixed deployments produce orphaned job handles; `JobOwnershipCache` only covers Java side. Acceptable single-backend; document or cross-lookup before hybrid.

### A05-F04 — divergent — P1
~10 endpoints implemented in BOTH Java and Rust with known semantic gaps (`signature.rs` lacks OCSP/CRL/AIA, AATL/EUTL; `MIGRATION.md` parity caveats). Results differ by which backend answers. Fix: single-owner decision per endpoint; delete Java copies after parity sign-off.

### A05-F05 — load-bearing — P1
`Taskfile.yml:326-336`; `.taskfiles/backend.yml:260-273`; `app/core/build.gradle:472-506`; `generate_tool_params.sh:11-14`. Tool-model codegen for frontend AND both engines depends on booting Java for `/v3/api-docs` → `SwaggerDoc.json`. Deleting `app/` breaks `task tool-models`, `tool-models:check` CI, regeneration of `toolApiTypes.ts`/`toolIO.ts`/engine `tool_models`. Fix: generate spec from Rust (port `/v3/api-docs`) or commit frozen spec + migration path.

### A05-F06 — release-risk — P2
`multiOSReleases.yml:120-197`; `package-managers.yml:65-73`. Release pipeline ships `Stirling-PDF*.jar` built from stripped Java tree — cannot perform migrated ops (only security/job/proprietary endpoints answer). Fix: confirm JAR releases deprecated vs `rust-release`; if not, release blocker.

### A05-F07 — scope-verify — P2
`.taskfiles/desktop.yml:43-51,92-104`; `src-tauri/commands/backend.rs:340-356,877-897`. Desktop defaults Rust sidecar; all proprietary-tier endpoints 404 in desktop builds; frontend still calls proprietary namespaces (~553 call sites). Needs product confirmation: intended desktop scope or default `DESKTOP_BACKEND=java`.

### A05-F08 — cleanup — P3
`scripts/PropSync.java`, `find-free-port.{sh,ps1}`, `init*.sh`, `aot-diagnostics.sh`, unused `SPDF/model/api/**` DTOs. Delete after reference sweep.

### A05-F09 — inventory — (no priority)
Java-only functional areas confirmed retained: authN/Z, license enforcement, JPA + ~36 schedulers, cluster (Valkey/S3), storage + encryption-at-rest, audit, webhooks, MCP server, AI agents/engine proxy, policy engine + processing folders + integrations + form-detection + notifications + signing workflows, PAYG/Stripe/procurement/legal/account-link, cert-sign/hardware-sign/restore-protection, Swagger/OpenAPI, ops telemetry. **Not migration gaps — the proprietary/SaaS platform.** Retain until product decision; do not schedule deletion under PDF strangler.

### A05-F10 — count-fix — P3
Two live endpoint classes don't match `*Controller.java` (85 not 83); core `ConvertPDFToMarkdown` misplaced in `model/` package duplicating a Rust route. Fix: rename/move or delete post-parity.

### A05-F11 — frontend-coupling — P2
`toolApiTypes.ts:375-439` (generated) still lists cert-sign family; ~553 proprietary/saas call sites; `SwaggerUI.tsx` expects `/v3/api-docs` (404 on Rust). Agent 08 deep-dive; align generated types or keep Java for those flavors.

## 5. Answers

- **Java functionality still necessary?** OSS/core PDF surface: only `cert-sign`, `cert-sign/hardware/*`, `restore-pdf-protection` lack Rust routes; ~10 security endpoints exist in both (drift). Everything else migrated already deleted. Proprietary/saas: ALL of `app/proprietary`+`app/saas`+consumed `app/common` infra load-bearing.
- **JobController/JobOwnershipCache:** needed only while Java serves requests (8 security controllers `@AutoJobPostMapping` + proprietary `AdminJobController`/policy). Rust parallel incompatible job store.
- **Dead/obsolete removable now:** PropSync, find-free-port.*, init*.sh family, unused migrated-endpoint DTOs (verify imports), `ConvertPDFToMarkdown` (post-parity). Keep: `RestartHelper.java`, `PaygCucumberThrowController`, `runtime/` (but see A10-F04).
- **Frontend→Java-only calls:** yes — generated types list cert-sign family; ~553 proprietary/saas call sites; Swagger UI `/v3/api-docs`.

## 6. Could not verify

- `./gradlew build`/`bootRun` current compile/start (read-only; CI suggests yes).
- Whether `task dev` breakage is known/accepted mid-migration.
- Whether shipped `Stirling-PDF.jar` still a supported artifact vs superseded by `rust-release`.
- Exact `@Scheduled`/`*Repository.java` counts (prior ~36/~70 not recounted).
