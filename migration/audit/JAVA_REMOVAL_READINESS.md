# JAVA REMOVAL READINESS

Source: agent-05 (primary), cross-checked with agent-01 (endpoints) and agent-08 (frontend). Audit date 2026-11-07.

## 1. Verified headline facts

- **No Java files are deleted in the working tree.** Deletions were *committed*: `67001f296e` ("replace Java/Spring backend") and `84f6bf24e7` ("retire migrated Java code"). `app/` is clean vs HEAD.
- **2,106 `.java` files remain at HEAD**: `app/core` (main ~150 + tests), `app/common` (~186), `app/proprietary` (~816), `app/saas` (~209) — counts include tests.
- **85 live endpoint classes** (not 83): 9 core (`*Controller`: 8 security + JobController) + 53 proprietary + 21 saas + **2 non-`*Controller`-suffixed**: `app/core/.../converters/ConvertPDFToMarkdown.java` (`/api/v1/convert/pdf/markdown`) and `app/proprietary/.../ConvertPdfToPdfUa.java` (`/api/v1/convert/pdf/ua`).
- Rust router covers the migrated OSS surface; Java proxy deleted — **no Rust→Java path exists**.
- Java still fully builds: `settings.gradle:89-98`, all `app/*/build.gradle`, `buildSrc/`, and CI (`backend-build.yml`, `multiOSReleases.yml`, `build-enterprise.yml`, `tauri-build.yml` jlink legs) intact.

## 2. Module classification

| Module | Files (main) | Controllers | Classification | Evidence |
|---|---|---|---|---|
| Migrated op controllers (merge/split/convert/general/misc/filter/form/analysis/transform/layout/visual/automation) | — | deleted in commits | **safe-to-remove (already removed)** | absent from `app/core`; Rust `handlers/mod.rs` covers |
| `ConvertPDFToMarkdown` (core, misplaced in `model/` pkg) | 1 | 1 | **verify → likely removable** | duplicates Rust `convert::pdf_to_markdown` (`mod.rs:307-310`) |
| Core security crypto: `CertSignController`, `HardwareSigningController`, `PasswordController::restore-pdf-protection` | few | 3 classes | **NOT MIGRATED — Java-only** | no Rust routes for cert-sign/hardware; restore landed in-flight (uncommitted) — re-check |
| Core security overlap: VerifyPDF, ValidateCompliance, ValidateSignature, Timestamp, Redact×3, prepare-pdf-session | few | 8 | **PARTIAL/DIVERGENT — dual live** | same paths routed in Rust AND compiled in Java; semantic drift (revocation, veraPDF lib-vs-CLI, redaction removal) |
| `JobController` + job framework (`AutoJobAspect`, `TaskManager`, `JobExecutorService`, `JobStore`, ownership services) | several | 1 | **PARTIAL — load-bearing while Java runs** | `@AutoJobPostMapping` on all 8 security controllers + proprietary admin/policy paths; two incompatible job namespaces (H2 vs SQLite) |
| `app/common` infra (cluster, FileStorage, ToolIORegistry, InternalApiClient, JPDFium/PDFBox utils, models) | ~186 | 0 | **PARTIAL — cannot remove while proprietary/saas stay** | ~208 proprietary + 26 saas importers; mix of live infra + dead tool-services |
| `app/proprietary` | ~816 | ~53 | **PROPRIETARY — retained by design** | auth/users/teams/licenses, account-link+entitlements, storage/folders/encryption-at-rest, policy engine+processing folders+sources+webhooks, audit, MCP, AI agents, Purview integrations, form-detection, notifications, signing workflows, portal/admin/usage/fleet. `rust/src/auth.rs` header documents Rust cannot replicate |
| `app/saas` | ~209 | ~21 | **PROPRIETARY — retained by design** | PAYG wallet/invoices/Stripe/procurement/account-link/AI-proxy/team-ownership/legal/fleet-usage |
| `ConvertPdfToPdfUa` (`/api/v1/convert/pdf/ua`) | 1 | 1 | **PROPRIETARY — retained** | premium PDF/UA tagging; no Rust route |
| Filters/interceptors/AOP (EndpointInterceptor, metrics, MobileScannerUploadFilter; PaygCharge, entitlement, auth, rate-limit, AuditAspect, AutoJobAspect, InitialSecuritySetup) | several | — | **PROPRIETARY/Java-only** | Rust has only guard/auth/metrics middleware |
| Repositories | ~70 | — | **PROPRIETARY — Java-only** | Spring Data JPA; H2/Postgres; Rust jobstore is jobs-only SQLite |
| Config (ApplicationProperties, EndpointConfiguration, settings.yml, profiles) | several | — | **PARTIAL** | settings.yml + endpoint enable/disable ported; security/OAuth2/SAML2/mail/legal props Java-only |
| Scheduling | ~36 `@Scheduled` sites | — | **PROPRIETARY — Java-only** | no Rust scheduler |
| Cluster/HA (Valkey, S3 FileStore, in-process) | several | — | **PROPRIETARY — Java-only** | `proprietary/cluster/valkey`, `common/cluster/inprocess` |
| Ops surface (Micrometer, Telegram, PostHog, AppUpdateService, fingerprint sessions, springdoc, legacy static) | several | — | **PARTIAL** | `/api/v1/info/*` metrics + SPA static ported; **no `/v3/api-docs`** → SwaggerUI 404s on Rust |
| Dead remnants | few | — | **safe-to-remove candidates** | `scripts/PropSync.java`, `find-free-port.{sh,ps1}`, `init*.sh`, `aot-diagnostics.sh`, unused migrated-endpoint DTOs (verify no proprietary/saas refs). Keep: `RestartHelper.java` (wired into build), `PaygCucumberThrowController` (profile-gated), `runtime/` (CI-load-bearing — see A10-F04) |

## 3. Runtime invocation of Java (confirmed live paths)

- `task dev`/`dev:all`/`dev:portal` → `./gradlew :stirling-pdf:bootRun`; frontend gets `BACKEND_URL=<java>` (`Taskfile.yml:72-105,216-243,442`).
- `task dev:strangler` runs Rust :8081 AND Java :8080 but frontend still defaults `BACKEND_URL=:8080` → **all UI traffic to Java**.
- `task backend:{run,stop,restart,build,test,check,swagger,licenses:*}`, `task build`, `task check`, `task test` — all include Gradle.
- `task e2e:live` boots `bootRun`; cucumber `BASE_URL=localhost:8080`; enterprise E2E + CI boot Java.
- Desktop: `DESKTOP_BACKEND` defaults `rust` (sidecar); `=java` opt-in keeps JAR+JRE.
- Releases: `multiOSReleases.yml` still builds/uploads `Stirling-PDF{,-with-login,-server}.jar`; `rust-release.yml` ships server+engine in parallel.
- **Codegen**: `task tool-models` → `backend:swagger` (boots Java) → `SwaggerDoc.json` → `toolApiTypes.ts`/`toolIO.ts` + engine `tool_models` + (indirectly) Rust engine schemas. `generate_tool_params.sh` hits `JAVA_URL=http://localhost:8080/v3/api-docs`.

## 4. What still NEEDS Java (removal blockers)

1. **Proprietary/SaaS platform** — auth, billing, JPA, policies, cluster, audit, MCP, AI proxy: ~1,025 files, ~74 controllers, ~250 frontend call sites. One Spring app — can't remove core/common while keeping them.
2. **`InternalApiClient` self-dispatch** — proprietary automation (policies, processing folders, MCP, AI workflows) posts tool calls to Java's own port → 404s for migrated tools. Needs retarget to Rust :8081 or formal retirement/gating. **P0 for proprietary scope.**
3. **SwaggerDoc codegen chain** — frontend types + both engines' tool models generated from Java `/v3/api-docs`. Needs Rust `/v3/api-docs` port or frozen merged spec before `app/` deletion.
4. **Dev/test/release wiring** — default dev → Java; live E2E/cucumber → Java; release ships JARs; `task backend:*`/`build`/`check` include Gradle.
5. **Core-only Java remnants** — cert-sign family (unless ported), dual-implemented security endpoints (until parity sign-off + Java copies deleted), `ConvertPDFToMarkdown` (post-parity), `JobController` job namespace (while Java serves requests).
6. **Java-only infra consumed by proprietary** — ~208 proprietary + 26 saas importers of `app/common`.

## 5. Removal readiness verdict

| Scenario | Verdict |
|---|---|
| Remove migrated OSS controllers only | **Already done** (committed) — but leaves dual-implemented security endpoints + InternalApiClient 404s |
| Remove all of `app/` today | **BLOCKED** — proprietary/saas product platform + codegen + dev/E2E/release wiring all depend on it |
| Remove `app/core`+`app/common` keeping proprietary/saas | **BLOCKED** — proprietary imports common infra pervasively; they're one Spring app |
| Full Java removal | **Not ready** — requires product decision on proprietary/saas fate FIRST, then codegen repoint, InternalApiClient retarget, dev/E2E/release rewiring, security-endpoint parity sign-off |

## 6. Recommended sequence

1. **Decide proprietary/saas fate** (retain as Java-in-front product vs port vs retire) — this gates everything.
2. If retained: keep Java; fix `InternalApiClient`→Rust dispatch; repoint default dev to Rust for core work; add Rust `/v3/api-docs` or freeze spec; resolve dual-implementation drift per endpoint; prune verified-dead Java (PropSync, find-free-port, init*, dead DTOs).
3. If retired: phased deletion saas→proprietary→core→common + Gradle/CI/Taskfile in separate commits, after codegen/E2E/dev rewiring.
4. Either way: `ConvertPDFToMarkdown` deletion post-parity; stale `EndpointConfiguration` groups cleanup; `split_photos.py` removal once `extract-image-scans` confirmed dead.

## 7. Could not verify

- `./gradlew build`/`bootRun` current compile/start (read-only audit; CI suggests green but stripped-tree test compilation unconfirmed — run `task backend:check`).
- Whether shipped `Stirling-PDF.jar` remains a supported artifact vs superseded by `rust-release`.
- Exact `@Scheduled`/`*Repository.java` counts (~36/~70 prior estimates not recounted).
- Whether `task dev` breakage is known/accepted mid-migration.
