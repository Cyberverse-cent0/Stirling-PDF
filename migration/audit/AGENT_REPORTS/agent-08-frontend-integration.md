# A08 — Frontend ↔ Rust API Integration (round 2, 2026-10-07 → verified 2026-11-07)

Supersedes `agent-08-frontend.md` (earlier same-day draft).

## 1. Integration map

### How the frontend reaches a backend

| Layer | Client | Base URL resolution | Dev/build default |
|---|---|---|---|
| Core web (core/proprietary/saas builds) | Axios `apiClient` — `frontend/src/editor/core/services/apiClient.ts` | `window.STIRLING_PDF_API_BASE_URL` → `import.meta.env.VITE_API_BASE_URL` → relative `/` | Vite dev/preview proxies `/api`,`/oauth2`,`/saml2`,`/swagger-ui`,`/v1/api-docs` to `BACKEND_URL` — **`http://localhost:8081` (Rust) when unset** (`vite.config.ts:532,541-563`) |
| SaaS | `frontend/src/editor/saas/services/apiClient.ts` | `VITE_API_BASE_URL` | `.env.saas` |
| Desktop (Tauri) | `frontend/src/editor/desktop/services/apiClient.ts` | `window.STIRLING_PDF_API_BASE_URL` → `VITE_API_BASE_URL` → `operationRouter.getBaseUrl()` | Local bundled backend `http://127.0.0.1:{port}` via Tauri `get_backend_port` (`tauriBackendService.ts:287`) |
| Shared cloud layer | `frontend/src/editor/cloud/` | via `@editor/services/apiClient` seam | — |

**Dev defaults:** `task dev`/`task dev:portal` run **Java** backend, `BACKEND_URL=http://localhost:<java-port>` (`Taskfile.yml:72-105,442`). Plain `vite`/`task rust:dev` → Rust :8081 (`vite.config.ts:532`). `task dev:strangler` runs Rust :8081 + Java :8080 + frontend + engine (`Taskfile.yml:465-474`). No committed `frontend/.env*` in this checkout (produced by `task frontend:prepare`).

**Desktop backend selection:** `src-tauri/src/commands/backend.rs` prefers Rust `stirling-server` sidecar, falls back to bundled `stirling-pdf-*.jar` only when no Rust binary (`backend.rs:875-891`). Desktop "local" backend **is** Rust in current builds.

### Endpoint surface

**Rust routes** (`handlers/mod.rs:32-385`, `metrics.rs`): ~120 explicit routes — general page ops, misc, security, analysis, filter, form (`/api/v1/form/*` expects multipart field **`file`**, not `fileInput` — `form.rs:1295-1341`), convert, mobile-scanner, jobs, files download, config (`app-config`, `login-disclaimer`, `endpoint-enabled`, `endpoints-enabled`, `endpoints-availability`, `group-enabled`), ui-data (`home`,`licenses`,`ocr-pdf`,`footer-info`,`pipeline`,`sign`), `pipeline/handleData`, `settings/get-endpoints-status`, `settings/update-enable-analytics`, `/api/v1/info/*` metrics. Fallback 404s unknown `/api/**` (`mod.rs:389-394`).

**Frontend tool calls** via `useToolApiCalls`/`useToolOperation`/`defineSingleFileTool` POST `FormData` with file under **`fileInput`** — matches `upload.rs:186`. Correct exceptions: `/api/v1/form/*` uses `file`; `/api/v1/mobile-scanner/upload/{id}` uses `files`; `/api/v1/general/extract-bookmarks` accepts `file`. Attachment endpoints use `fileInput`+`attachmentName`/`newName`/`attachments`/`opsJson`/`convertToPdfA3b` — all matched.

**Feature flags:** `fetchAppConfig`/`fetchEndpointsAvailability`/`fetchEndpointEnabled`/`fetchGroupEnabled` (`core/api/config.ts`) map 1:1 to Rust `platform.rs:221-285`. Rust returns `{enabled, reason}` with `CONFIG|DEPENDENCY|NOT_SUPPORTED_LOCALLY` (`platform.rs:254-281`) — same contract frontend consumes (`useEndpointConfig.ts`, `useToolManagement.tsx:75-107`: tool unavailable only when ALL registry `endpoints` report `enabled:false`). `get-endpoints-status` exists in Rust but has **no production frontend caller**.

**Job API:** Rust supports `?async=true` → `{async,jobId}` + `/api/v1/general/job/*` routes. Frontend **never** sends `async=true` to tool endpoints and never calls `/api/v1/general/job/*`; only `jobId` consumers are proprietary policy/pipeline flows targeting `/api/v1/policies/run*` — Java/proprietary-only endpoints.

**Engine/AI:** Python engine reached **only through Java backend** (`AIENGINE_URL` on Java, `Taskfile.yml:230-237`; `rust/engine` separate binary via `engine:bg`). Frontend calls `/api/v1/ai/orchestrate/stream`, `/api/v1/ai/tools/classify-and-label`, `/api/v1/ai/status`, `/api/v1/account-link/linked` directly against API base — expects **Java** to proxy the engine. Rust has no `/api/v1/ai/*` routes.

**Desktop routing:** `operationRouter.ts` classifies `/api/v1/{policies,processing-folders,automation,pipeline}` + SaaS-only prefixes to connected server; tool endpoints (`general|convert|misc|security|filter|multi-tool|ui-data`) → local Rust **unless** `isEndpointSupportedLocally` says no → SaaS/self-hosted or sign-in error in local mode (`operationRouter.ts:185-381`).

## 2. Findings

### A08-F01 — `restore-pdf-protection` unrouted + guard-blocked: every operation on password-protected PDF fails — P1
Frontend `pdfSessionUnlock.ts:241-263` (`protectPdfOutput` POSTs `/api/v1/security/restore-pdf-protection` with `fileInput`,`sourceFile`,`password`); invoked for **every** non-password tool output when source opened via `prepare-pdf-session` (`protectUnlockedResults`, `:265-311`). Rust: no route; `endpointconfig.rs:375` UNSUPPORTED → 403. `passwords.rs:21-25` documents deferral to Java (JPDFium `PdfSecurity.copyEncryption`). `prepare-pdf-session` IS implemented (`mod.rs:231-234`) so encrypted inputs decrypt, tool runs, mandatory restore 403s → whole op fails. All tool ops on owner-password-protected PDFs unusable on Rust (web + desktop-local). Fix: port restore-pdf-protection (persist session key material from prepare-pdf-session, re-encrypt natively) — silently stripping protection is NOT acceptable.

### A08-F02 — `UNSUPPORTED_ENDPOINTS` disables frontend-composed/client-side tools needing no backend call — P1
`endpointconfig.rs:385-396` puts `adjust-contrast`,`automate`,`change-permissions`,`compare`,`extract-pages`,`json-to-pdf`,`multi-tool`,`pdf-to-json`,`pdf-to-rtf`,`pdf-to-video`,`remove-annotations`,`view-pdf` into UNSUPPORTED (NotSupported → `enabled:false`, guard 403). Contradicts doc comment `endpointconfig.rs:364-369` ("Deliberately absent … frontend-composed tools that decompose into routed calls — disabling the key would hide a working tool") and unit test `endpointconfig.rs:1013-1021` asserting `extract-pages`,`compare`,`pipeline` stay enabled — **test should currently fail** (verify `cargo test -p stirling-server endpointconfig`).
Frontend impact (all client-side):
- `automate` → registry `endpoints:["automate"]` (`useTranslatedToolRegistry.tsx:905`); Automate chains ops in-browser → **hidden**.
- `compare` → `:1135`; web worker + pdf.js → hidden.
- `remove-annotations` → `:834`; client-side → hidden.
- `adjust-contrast` → `:938`; `customProcessor` client-side → hidden.
- `multi-tool`,`view-pdf` → `:116,233,480` → hidden.
- `pdf-to-rtf` → `convertConstants.ts:28` maps RTF→`/api/v1/convert/pdf/text` (route EXISTS `mod.rs:304`), but `ConvertSettings.tsx:104-107` requires `endpointStatus["pdf-to-rtf"]===true` → **PDF→RTF option disabled** though backend call would succeed.
Fix: remove composed/client-side keys from UNSUPPORTED (keep real unrouted: `cert-sign`,`ingest`,`restore-pdf-protection`,`orchestrate`,`tools`,`external-api-call`,`purview-*`,`dev-*`); fix stale comment + failing test; `pdf-to-rtf` drop from list or frontend key→`pdf-to-text`.

### A08-F03 — AI/engine endpoints: frontend exposes them, Rust has no routes — P2
| Call | File | Rust result |
|---|---|---|
| `POST /api/v1/ai/orchestrate/stream` | ChatContext via `aiBaseUrl.ts` | 403 — `orchestrate` ∈ UNSUPPORTED (`endpointconfig.rs:377`) |
| `POST /api/v1/ai/tools/classify-and-label` | classify pipeline | 403 — `tools` ∈ UNSUPPORTED (`:378`) |
| `GET /api/v1/ai/status` | `useAiEngineStatus.ts:56` | 404 — `status` unknown → enabled → no route |
| `GET /api/v1/ai/health` | engine probes | 404 |
| `GET /api/v1/account-link/linked` | `useAiEngineStatus.ts:109` | 404 (handled gracefully) |

Enabler: `app-config` returns `aiEngineEnabled` from `settings.ai_engine.enabled` + `aiFeatures.*` (`platform.rs:1066-1081`). User enabling AI on Rust gets chat/classify UI that can only fail. Key mismatch: registry `classify` uses `endpoints:["classify-and-label"]` (`useTranslatedToolRegistry.tsx:1153`); availability lookup for that literal key returns enabled (not in any group/disabled set) while guard evaluates URI key `tools` → disabled. Availability and enforcement disagree. Fix: force `aiEngineEnabled`/`aiFeatures` false in `app_config` until Rust engine proxy exists (or proxy `/api/v1/ai/*` to `rust/engine`/`AIENGINE_URL` like Java); align `classify-and-label` availability key with `tools` guard key.

### A08-F04 — Proprietary/portal API surface has no Rust routes → 404 — P2 (expected; verify gating)
Frontend-referenced, absent from `mod.rs`: `/api/v1/policies*`, `/api/v1/policies/run*`, `/api/v1/processing-folders*`, `/api/v1/storage/*`, `/api/v1/notifications*`, `/api/v1/file-run-events*`, `/api/v1/proprietary/ui-data/*`, `/api/v1/user/*`, `/api/v1/auth/*`, `/api/v1/team/*`, `/api/v1/invite/*`, `/api/v1/admin/*`, `/api/v1/database/*`, `/api/v1/sources*`, `/api/v1/integrations*`, `/api/v1/automation/meter`, `/api/v1/workflow/participant/*`, `/api/v1/ownership/handover`, `/api/v1/payg/*`, `/api/v1/usage/*`, `/api/v1/procurement*`, `/api/v1/legal/*`, `/api/v1/webhooks/*`, `/api/v1/docparse/capabilities`, `/api/v1/ui-data/tessdata*`, `/api/v1/form/form-detection-model/*`.
Mitigations present: `app-config` hard-codes `enableLogin=false`, `premiumEnabled=false`, `storageEnabled=false`, `accountLinkAvailable=false` (`platform.rs:790-998,1060-1064`) → most surfaces unreachable in OSS. Failures swallowed (`notifications.ts:96-98`, `automationMeter.ts:11-16`; `ServerSelection.tsx:96-112` treats 404 as "security disabled").
Residual risk: processor portal (`/portal` routes, policies/sources/pipelines pages) + notification bell still issue requests that 404 on Rust; `FolderIngestionSettings.tsx:41` calls `/api/v1/docparse/capabilities` → 404 (sibling `ingest` correctly NOT_SUPPORTED_LOCALLY).
Fix: none required for OSS parity, but processor SPA should detect Rust backend (e.g. `app-config.runtime==="rust"` or capabilities flag) and hide portal features rather than per-call 404s.

### A08-F05 — Form-detection model endpoints 404 while detection endpoint 403s — P3
`useFormDetectionModelStatus.ts:36-39` polls `GET/POST/DELETE /api/v1/form/form-detection-model[/status|/install|/config]` — key `form-detection-model` unknown → enabled → 404. Tool `/api/v1/form/form-detection/detect` → `form-detection` → Dependency-disabled → 403 (`useAutoFormDetectionOperation.ts:21`). Inconsistent gating; admin model UI errors on Rust (AutoFormDetection tool correctly hidden via `endpointName:"form-detection"`). Fix: implement model endpoints or mark `form-detection-model` NotSupported.

### A08-F06 — Stale/misleading endpoint references (no runtime break) — P3
- `desktop/components/AppProviders.tsx:44` preloads availability for `/api/v1/misc/add-watermark` — no such route (watermark is `/api/v1/security/add-watermark` `mod.rs:219-222`). Harmless but misleading.
- `/api/v1/ui-data/tessdata-languages`+`/download` (`AdvancedProcessingCard.tsx:59,94,159`) → 404, gated by `loginEnabled`=false on Rust → unreachable.
- Rust implements routes frontend never calls: `/api/v1/analysis/*`, `/api/v1/filter/*`, `/api/v1/pipeline/handleData`, `/api/v1/general/job/*`, `/api/v1/settings/get-endpoints-status` — parity items, not bugs.

## 3. Verified-compatible areas

- **Multipart contract:** `fileInput` convention matches `upload.rs:186`; form `file`, mobile-scanner `files`, attachment params all match.
- **Config/UI-data:** all frontend calls have matching Rust GET routes + compatible shapes.
- **Mobile scanner lifecycle:** create/validate/upload/files/download/delete all match (`automation.rs:676-1139` vs `useMobileTransferSession.ts`, `MobileScannerPage.tsx:719-730`, `MobileSignPage.tsx`).
- **Job API:** internally consistent; unused by frontend's synchronous tool path.
- **Desktop routing:** `operationRouter` correctly diverts SaaS-only/unsupported-local endpoints; degrade to sign-in prompt not silent 404.
- **Unknown `/api/**`→404, disabled→403** (`mod.rs:389`, `guard.rs`): matches frontend expectations (`extractSingleAttachment` tolerates 404/501, `attachmentService.ts:144-149`).

## 4. Items for parent verification

- Run `cargo test -p stirling-server endpointconfig` — `unrouted_endpoints_report_not_supported` appears to contradict UNSUPPORTED_ENDPOINTS (A08-F02).
- Confirm whether processor SPA is served by Rust static fallback (`web.rs`) in production builds — determines A08-F04 exposure.
- `?async=true` end-to-end reviewed statically only.
