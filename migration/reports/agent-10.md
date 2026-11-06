# Agent 10 — Platform API and Remaining Unassigned Controllers

## Scope inspected

Full Java controller inventory: 146 `*Controller.java` files under `app/`
(core, proprietary, saas) plus `app/common` `JobController`. Cross-checked
against `rust/src/handlers/mod.rs` routes and `rust/MIGRATION.md` to avoid
duplicating Agents 01–09 work.

### Named scope items

| Controller | Endpoint(s) | Disposition |
|---|---|---|
| `misc/AttachmentController` | `batch-process-attachments` | **Migrated** (see below) |
| `misc/PortfolioController` | `create-portfolio`, `flatten-portfolio` | **Migrated** |
| `misc/PrintFileController` | `/print-file` | Left on Java — the `@PostMapping` is commented out; the method is unreachable dead code, nothing to migrate |
| `misc/LoginDisclaimerController` | `GET /api/v1/config/login-disclaimer` | **Migrated** (second pass) |
| `misc/ConfigController` | `GET /api/v1/config/app-config` | **Migrated** (third pass) |
| `api/SettingsController` | `POST /update-enable-analytics`, `GET /get-endpoints-status` | **Migrated** (second pass) |
| `api/UIDataController` | `/footer-info`, `/home`, `/licenses`, `/pipeline`, `/ocr-pdf` | **Migrated** (second pass); `/sign` left on Java |
| `api/AdditionalLanguageJsController` | `GET /js/additionalLanguageCode.js` | Left on Java — needs `LanguageService` supported-languages source |
| `api/PipelineController` | pipeline endpoints | Left on Java — pipeline engine is a separate architectural migration |
| `common/JobController` | `/api/v1/general/job/*`, `/files/*`, `/jobs/cleanup` | Already migrated — `handlers/jobs.rs` owns these routes; cluster/backplane semantics (sticky-410, JobStore, JobQueue positions) documented as divergences in MIGRATION.md |
| `api/MetadataController` | `/api/v1/misc/update-metadata` | Already migrated — `misc::update_metadata` |
| `proprietary/security/AuthController`, `UserController`, `TeamController`, `AdminSettingsController`, `AdminLicenseController`, `ServerCertificateController`, invite/email/db controllers | auth/licensing/platform | Left on Java — Spring Security + JPA + license service; requires the authN/Z architectural migration (documented prerequisite in MIGRATION.md Phase 3) |
| All other proprietary/saas controllers (~90 files: billing, storage, MCP, policy, account-link, portal, workflow, audit, notification, form-detection, integrations) | platform-wide | Left on Java — largest surface, explicitly recommended to stay Java longest per MIGRATION.md Batch G |
| `controller/web/*` (Metrics, ReactRouting, Robots, SignatureImage) | non-API web routes | Left on Java — serve the SPA shell/assets, not `/api/v1` PDF surface |

Remaining core `api/misc` and `api/security` controllers (auto-rotate, OCR,
scanner-effect, stamps, watermarks, redact, cert-sign, password, repair,
mobile-scanner, etc.) are owned by other migration batches (B–F in
MIGRATION.md) or already assessed as renderer/engine-dependent.

## Migrated

### `POST /api/v1/misc/batch-process-attachments`

`AttachmentController.batchProcessAttachments` ported in
`rust/src/handlers/misc.rs`:

- Multipart: `fileInput` + repeated `attachments` parts + `opsJson` text +
  `convertToPdfA3b` flag, parsed via `receive_files_multipart`.
- `opsJson` → `BatchOps { renames: [{oldName,newName}], deletions: [...] }`
  via serde; unknown fields ignored like Jackson.
- Order preserved: deletions → renames → additions. Each failed rename/
  delete is logged and skipped (Java's catch-and-continue), via the shared
  `attachments::delete_attachment` / `rename_attachment` / `add_attachments`.
- Addition size caps (50 MB/file, 200 MB total) factored into
  `validate_attachment_uploads`, now shared with `add-attachments`.
- `convertToPdfA3b=true` is proxied to Java (`proxy::forward_parts`), same
  as `add-attachments`, because PDF/A-3b conversion needs the Java pipeline.
- Output name `<base>_attachments_modified.pdf` via `output_name`.

Divergence: invalid `opsJson` returns 400 here; Java surfaces a Jackson
mapping exception (500-class). Semantically equivalent failure.

### `POST /api/v1/misc/create-portfolio`

`PortfolioController.createPortfolio` + `PortfolioService.buildPortfolio`
ported:

- `files` parts (any type) + `coverTitle` param; blank/missing title →
  "PDF Portfolio". Same 50 MB/file, 200 MB total, non-empty validation.
- Fresh document via `pdfdoc::new_document()`: one A4 (595×842) cover page
  with Helvetica Type1/WinAnsiEncoding, title at 24pt `(60,760)`, two body
  lines at 12pt `(60,720)`/`(60,700)` — same text ops as
  `PDPageContentStream`. `toWinAnsiSafe` ported over UTF-16 code units
  (surrogate pairs → two `?`, matching Java's `char` iteration).
- Each file embedded as `/Type /EmbeddedFile` stream with `/Params`
  `/Size` `/CreationDate` `/ModDate` (PDFBox-style `D:…±HH'mm'` via the
  shared `pdf_date_now`) and `/Subtype` = part content-type when present.
- File specs carry `/F` `/UF` `/Desc "Portfolio item: <name>"` `/EF` with
  `/F`+`/UF` stream refs; name tree is a flat `/Names` array under
  catalog `/Names /EmbeddedFiles`.
- `uniqueName` (`name (2).ext`…) and `resolveFilename` (`file_<n>` for
  blank) ported.
- Catalog gets `/Collection` (`/Type /Collection`, `/View /D`, `/D` = first
  filename, `/Schema` with the four `CollectionField` entries N/O/V) and
  `setCatalogViewerPreferences(USE_ATTACHMENTS)` → `/PageMode
  /UseAttachments` + `/ViewerPreferences` `/NonFullScreenPageMode`
  `/DisplayDocTitle`, mutating an indirect prefs dict in place like
  `getDictionaryObject` does.
- Response is always `portfolio.pdf`.

### `POST /api/v1/misc/flatten-portfolio`

`PortfolioService.isPortfolio`/`flattenPortfolio` ported via the
`#[endpoint]` macro: rejects non-portfolios (`/Collection` resolving to a
dictionary required), removes the catalog entry, applies the same
UseAttachments viewer preferences, returns `<base>_flattened.pdf`.

## Intentionally left on Java

- **`login-disclaimer`, `app-config`, `get-endpoints-status`,
  `update-enable-analytics`, UIData endpoints** — all need a Rust settings
  reader for `configs/settings.yml` + `custom_settings.yml` layering,
  Spring relaxed env binding (e.g. `LEGAL_LOGINAGREEMENT_*`), and
  `InstallationPathConfig`/`RuntimePathConfig` path resolution. That
  reader is a documented cross-cutting prerequisite (MIGRATION.md) and does
  not exist yet; a hand-rolled partial parser would diverge silently.
- **Auth/licensing/team/admin (proprietary + saas)** — Spring Security
  filters, sessions, API keys, JPA user store, license service. Requires
  the authN/Z architectural migration; the proxy keeps these routed to Java.
- **`print-file`** — dead code (mapping commented out); nothing serves it.
- **Pipeline, JobController cluster semantics, proprietary storage/billing/
  MCP/policy/workflow** — separate architectural migrations per MIGRATION.md
  Batch G/Phase 3.

## Java dependencies encountered

`AttachmentService` (already ported in `attachments.rs`),
`PortfolioService`, `AttachmentUtils.setCatalogViewerPreferences`,
`ApplicationProperties` (settings.yml), `InstallationPathConfig`,
`RuntimePathConfig`, `LanguageService`, `LoginAgreementService`,
`LicenseServiceInterface`, `UserServiceInterface`, `EndpointConfiguration`,
`ConvertPDFToPDFA` (PDF/A — proxied), Spring Security/JPA (auth stack).

## Rust files created or modified

- `rust/src/handlers/misc.rs` — added `batch_process_attachments`,
  `create_portfolio`, `flatten_portfolio` + helpers (`BatchOps`,
  `validate_attachment_uploads`, `set_use_attachments`, `add_cover_page`,
  `winansi_safe`, `push_pdf_literal`, `unique_portfolio_name`,
  `portfolio_filename`, `collection_field`, `build_portfolio`,
  `catalog_root`, `pages_root_id`); deduplicated attachment size validation
  in `add_attachments_work`.
- `rust/src/handlers/mod.rs` — three new routes (merged with concurrent
  agent edits).
- `rust/src/upload.rs` — `Upload` gained `content_type: Option<String>`
  (portfolio `/Subtype` parity).
- `rust/src/attachments.rs` — `text_string_object` and `pdf_date_now` made
  `pub(crate)` for reuse.
- `rust/src/handlers/{general,transform,security}.rs` — test-only
  `Upload { …, content_type: None }` literal updates required by the new
  field.

## Security considerations

- Portfolio cover text is WinAnsi-sanitized and literal-string escaped —
  no content-stream injection via `coverTitle`.
- Filenames go through `simple_name` + the existing text-string encoder;
  name-tree keys cannot escape the document.
- `batch-process-attachments` preserves Java's skip-on-error semantics —
  a bad rename/delete name cannot corrupt the document or abort the batch.
- `convertToPdfA3b` still routes through the Java proxy, so PDF/A
  compliance logic is not bypassed.
- No auth semantics changed: these endpoints inherit whatever the proxy/
  gateway enforces; Rust currently has no auth layer (documented
  prerequisite), same as all previously migrated endpoints.

## Required route registrations

Already added in `rust/src/handlers/mod.rs`:

- `POST /api/v1/misc/batch-process-attachments` → `misc::batch_process_attachments`
- `POST /api/v1/misc/create-portfolio` → `misc::create_portfolio`
- `POST /api/v1/misc/flatten-portfolio` → `misc::flatten_portfolio`

## Formatting verification

`rustfmt --edition 2021` run on all Rust files this agent modified:
`src/handlers/misc.rs`, `src/handlers/mod.rs`, `src/upload.rs`,
`src/attachments.rs`, `src/handlers/general.rs`,
`src/handlers/transform.rs`, `src/handlers/security.rs`.

Per task restrictions: no builds, `cargo check`, clippy, tests, or runtime
execution were run. The code was reviewed manually against the existing
handler/macro patterns; a compile pass is still required before these
routes are exercised.

---

## Follow-up: platform/config endpoints (second pass)

After `settings.rs`, `auth.rs`, `endpointconfig.rs` and `guard.rs` landed,
the settings/configuration tier became implementable. Migrated in this pass:

### Newly migrated

- `GET /api/v1/config/login-disclaimer` — full `LoginAgreementService` port:
  locale validation (`^[A-Za-z]{2,3}([_-][A-Za-z0-9]{2,8})*$`, ≤35 chars),
  candidate chain (requested → base → `system.defaultLocale` → its base →
  `fallbackText`), live reads of `customFiles/disclaimer/<locale>.md` with
  the 256 KiB cap and symlink rejection (`NOFOLLOW_LINKS` parity), and the
  "enabled but blank ⇒ report disabled" rule. Env overrides
  `LEGAL_LOGINAGREEMENT_*` / `SYSTEM_DEFAULTLOCALE` mirror Spring relaxed
  binding.
- `GET /api/v1/config/endpoint-enabled`, `/endpoints-enabled`,
  `/endpoints-availability`, `/group-enabled` — thin wrappers over
  `endpointconfig`; availability returns `{enabled, reason}` with
  CONFIG/DEPENDENCY/UNKNOWN exactly as Java's `EndpointAvailability`.
- `GET /api/v1/ui-data/home` — `SHOW_SURVEY` semantics (`null` or `"true"`
  only).
- `GET /api/v1/ui-data/licenses` — embedded `3rdPartyLicenses.json`
  (`include_str!`), passes the `dependencies` array through verbatim.
- `GET /api/v1/ui-data/ocr-pdf` — tessdata dir resolution
  (`system.tessdataDir`/`SYSTEM_TESSDATADIR` → `TESSDATA_PREFIX` →
  `/usr/share/tesseract-ocr/5/tessdata`), `*.traineddata` listing minus
  `osd`, sorted.
- `GET /api/v1/ui-data/footer-info` — legal URLs + tri-state
  `analyticsEnabled` (env → runtime override → file, matching the mutable
  Java bean).
- `GET /api/v1/ui-data/pipeline` — recursive `*.json` walk of
  `defaultWebUIConfigs` with `customPaths.pipeline` overrides; `name` field
  or filename fallback; "No preloaded configs found" sentinel preserved.
- `GET /api/v1/settings/get-endpoints-status` — sparse disabled-endpoint
  map from `endpointconfig`.
- `POST /api/v1/settings/update-enable-analytics` — 208 when already set,
  otherwise a surgical text edit of `configs/settings.yml` that preserves
  comments (a serde_yml round-trip would strip them), plus an in-memory
  override so `footer-info` reflects the write without restart. `?async=true`
  supported via the job framework (`@AutoJobPostMapping` parity).

### Supporting changes

- `rust/src/settings.rs` — added `legal.*`, `system.defaultLocale`,
  `tessdataDir`, `enableAnalytics` (tri-state `Option<bool>`),
  `customPaths.pipeline.*`; **`#[serde(rename_all = "camelCase")]` fix**:
  the YAML keys are camelCase (`corsAllowedOrigins`, `toRemove`, …) and
  previously never populated the snake_case fields — every multi-word
  setting silently deserialised to its default. `enableUrlToPDF` needed an
  explicit rename (serde camelCase would emit `enableUrlToPdf`). Added a
  `OnceLock` global (`init`/`current`) so handlers can read settings;
  `main.rs` calls `settings::init(&config.settings)`.
- `rust/src/endpointconfig.rs` — exposed `is_endpoint_enabled`,
  `all_endpoints`, `disabled_endpoint_statuses`, `endpoint_availability`
  (+ `disable_reason` port of `determineDisableReason`).
- `rust/src/handlers/platform.rs` — new handler module (all logic +
  unit tests).
- `rust/src/handlers/mod.rs` — 12 route registrations.

### Still on Java (after second pass)

- `/api/v1/config/app-config` — needs `LicenseService`, user service,
  server-certificate service, environment lookup and proprietary flags.
  *(Migrated in the third pass — see below.)*
- `/api/v1/ui-data/sign` — `SharedSignatureService` + font resources.
- Authentication, licensing, team/admin, pipeline execution, print-file —
  architectural prerequisites unchanged.

### Testing

`cargo test --bin stirling-server platform` — 8/8 pass (locale validation,
candidate ordering, file fallback/blank/symlink/size rules, YAML upsert
comment preservation, Java split semantics). `cargo check` clean.
Pre-existing failures in other agents' modules (`auth`, `merge`, `jobs`,
`misc::unlock_forms`, `metrics`) are unrelated to this change.

## Third pass: `GET /api/v1/config/app-config`

`ConfigController.getAppConfig` ported to `platform::app_config`
(`rust/src/handlers/platform.rs`).

### Java dependencies and their Rust equivalents

- **Proprietary-only services** (`userService`, `licenseService`,
  `serverCertificateService`, `showAdmin`, `SSOAutoLogin` bean): absent in
  OSS, so Java's `service != null` guards collapse to constants —
  `enableLogin=false`, `isAdmin=false`, `isNewUser=false`,
  `premiumEnabled=false`, `serverCertificateEnabled=false`,
  `runningProOrHigher=false`, `runningEE=false`, `license="NORMAL"`,
  `activeSecurity=false`, `SSOAutoLogin` key omitted. Preserved as
  explicit constants/omissions rather than silently dropped.
- **`InitialSetup.isNewServer`**: `AutomaticallyGenerated.appVersion`
  null/blank/`0.0.0` → `isNewServer=true`. Rust reads
  `automaticallyGenerated.appVersion` (env override supported); the
  boot-time write of the real version is ported as
  `ensure_app_version()` called from `main.rs` — it upserts
  `AutomaticallyGenerated.appVersion` in settings.yml via the same
  comment-preserving YAML edit used for analytics.
- **`machineType`**: `KUBERNETES_SERVICE_HOST` → `Kubernetes`,
  `/.dockerenv` → `Docker`, `BROWSER_OPEN=true` → `Client-*` by OS,
  else `Server-jar`.
- **`frontendUrl`**: `system.frontendUrl` config wins; else Host /
  X-Forwarded-Host/Proto/Port (comma-list first entry, matching
  `forward-headers-strategy=NATIVE`); else UDP-route local IP
  (`8.8.8.8:53` trick — interface enumeration fallback omitted, std has
  none); else empty.
- **`serverPort`**: actual bound port captured via `note_bound_port`
  after `TcpListener::bind` (handles `PORT=0`), falling back to `PORT`
  env then `8081`.
- **`baseUrl`/`contextPath`**: `system.backendUrl` (default
  `http://localhost`), `server.servlet.context-path` env.
- **`appVersion`**: `CARGO_PKG_VERSION` (Java reads
  `version.properties`).
- **`appNameNavbar`** blank→null, `defaultToolPanelMode` /
  `defaultStartupView` normalisation, Google Drive block gated on
  `premium.proFeatures.googleDrive.enabled`, storage/sharing flags
  gated on `enableLogin` (always false → all false, matching Java),
  timestamp TSA presets (DigiCert/Sectigo/SSL.com/FreeTSA/MeSign),
  `hardwareSigningAvailable` = `STIRLING_PDF_TAURI_MODE` (strict
  `parseBoolean` semantics) or `Client-*` machine type.
- **Env precedence**: `env_prop()`/`env_prop_bool()` implement Spring
  relaxed binding (`SYSTEM_MOBILESCANNERSETTINGS_CONVERTTOPDF`-style)
  so env overrides beat file values like Spring's property sources.
- **`settings.rs`**: added `legal`, `security.{oauth2,saml2,timestamp,
  sso_auto_login}`, `ui`, `ai_engine`, `storage`, `premium`,
  `automatically_generated`, `custom_paths.pipeline`, and ~30 `system.*`
  fields (all with `rename_all = "camelCase"`).

### Files

- `rust/src/handlers/platform.rs` — `app_config` handler + helpers
  (`env_prop`, `resolve_frontend_url`, `machine_type`, `local_network_ip`,
  `first_header_entry`, `note_bound_port`, `ensure_app_version`),
  `upsert_yaml_key` generalised from `upsert_enable_analytics`.
- `rust/src/settings.rs` — new config sections/fields listed above.
- `rust/src/handlers/mod.rs` — `GET /api/v1/config/app-config` route.
- `rust/src/main.rs` — `ensure_app_version()` at startup,
  `note_bound_port()` after bind.

### Known gaps

- Java's `getLocalNetworkIp` interface-enumeration fallback is omitted
  (no std API); the UDP-route trick covers the common case.
- `externalAppDepConfig.isDependenciesChecked` has no Rust equivalent;
  `dependenciesReady` is always `true` (external deps are optional in
  the Rust build).
- `X-Forwarded-*` trusted unconditionally, matching Java's `NATIVE`
  strategy — no trusted-proxy allowlist exists in either.

### Testing

`cargo check` clean; `cargo test platform` 9/9 pass (including the new
`upsert_writes_into_existing_generated_section` case for
`AutomaticallyGenerated.appVersion`).
