# Java Execution Flow — Stirling-PDF backend (`app/`)

Reference trace of the Java/Spring Boot application, produced by reading the
sources (not assumed). This is the behavioral spec the Rust backend in `rust/`
must match.

## 1. Entry point

`app/core/src/main/java/stirling/software/SPDF/SPDFApplication.java`

`main(String[] args)`:

1. `new SpringApplication(SPDFApplication.class)`; `setAdditionalProfiles(getActiveProfile(args))`
   — profile selection from args/env (core / proprietary / saas flavors).
2. `new ConfigInitializer().ensureConfigExists()`
   (`app/common/.../configuration/ConfigInitializer.java`):
   - If `configs/settings.yml` is missing or has < 31 lines, it is moved to
     `settings.yml.<ts>.bak` and recreated from the packaged
     `settings.yml.template` (`app/core/src/main/resources/settings.yml.template`,
     ~426 lines, ~20 top-level sections: `security`, `premium`, `mail`,
     `telegram`, `legal`, `system`, `storage`, `autoPipeline`, `ui`,
     `endpoints`, `metrics`, `toolRecommendations`, `processExecutor`,
     `aiEngine`, `docparse`, `formDetection`, `policies`, `mcp`, `cluster`, …).
   - Otherwise the template is merged line-wise into the existing file,
     preserving comments/whitespace (`YamlHelper`), rewritten only on diff.
3. Spring config locations: `spring.config.additional-location` is set to
   `file:<settings.yml>` and, when present, `file:<custom_settings.yml>`
   (`InstallationPathConfig.getSettingsPath()/getCustomSettingsPath()`;
   root dir overridable via `STIRLING_BASE_PATH` / `-Dstirling.base-path`).
4. `app.run(args)` — component scan over
   `stirling.software.SPDF`, `stirling.software.common`,
   `stirling.software.proprietary`, `stirling.software.saas`
   (`@SpringBootApplication(scanBasePackages=…)`), `@EnableScheduling`.
5. After `run()` returns: creates `templates/` and `static/` dirs under the
   installation path, prints the startup URL.

`@PostConstruct init()`: resolves `backendUrl`/`contextPath`/`serverPort`
(`AppConfig`), honors `BROWSER_OPEN=true` (xdg-open/open/rundll32), logs
Tauri parent PID when `STIRLING_PDF_TAURI_MODE` is set.
`ApplicationReadyEvent` listener records the real bound port
(`local.server.port`, supports `server.port=0` "auto").

## 2. Dependency / service initialization (Spring context)

Everything is constructor-injected singletons. Notable families:

- **Config**: `ApplicationProperties` (`@ConfigurationProperties`, bound from
  settings.yml + env), `AppConfig`, `EndpointConfiguration` (per-endpoint
  enable/disable from `endpoints:` settings), `RuntimePathConfig`,
  `MultipartConfiguration` (~2 GB multipart limits, temp-file spilling).
- **Security** (proprietary module, always on classpath in the full build):
  `SecurityConfiguration` filter chain — `JwtAuthenticationFilter`,
  `UserAuthenticationFilter` (session login, `security.enableLogin`),
  `DatabaseLicenseFilter`, `EnterpriseEndpointFilter`,
  `UserBasedRateLimitingFilter`, API-key auth via
  `ApiKeyAuthenticationService`. Core also has
  `FingerprintBasedSessionFilter`, `MetricsFilter`, `WAUTrackingFilter`,
  `MobileScannerUploadFilter`. SaaS adds `SupabaseAuthenticationFilter`,
  `DeviceCredentialAuthenticationFilter`, `PaygResponseBodyWrapperFilter`.
- **Persistence**: Spring Data JPA — H2 embedded (default) or PostgreSQL
  (`app/proprietary/build.gradle`: `spring-boot-starter-data-jpa`,
  `h2`, `postgresql`). Entities cover users, teams, licenses, audit,
  storage, payg, policies.
- **External processes**: LibreOffice (`soffice`) for office conversions,
  Ghostscript + qpdf for compress/flatten, ImageMagick for some image ops,
  Tesseract for OCR, veraPDF for PDF/A compliance
  (`VeraPDFService`), pdfium/jpdfium for rendering-adjacent ops.
- **Python AI engine**: FastAPI service on `:5001` (`engine/`), reached
  through `AiEngineController`/proxy; Rust replacement in progress at
  `rust/engine/`.
- **Scheduling**: `@EnableScheduling` + `@Scheduled` jobs —
  `TempFileCleanupService`, `FileMonitor`, `MetricsAggregatorService`,
  `WeeklyActiveUsersService`, `PdfMetricsService`, pipeline
  `PipelineDirectoryProcessor`, plus proprietary schedulers
  (`ScheduledTasks`, `RateLimitResetScheduler`, `UsageSyncService`,
  `KeyPairCleanupService`, `PolicyAssetCleaner`, `ClusterNodeBootstrap`, …).

## 3. Request lifecycle

```
HTTP → servlet filters (metrics, WAU, mobile-scanner upload,
       fingerprint session; proprietary auth/license/rate-limit chain)
     → DispatcherServlet
     → HandlerInterceptors:
         EndpointInterceptor  — 403 when endpoints.<tool>.enabled=false;
                                sets Cache-Control: private, no-store on /api/*
         PdfMetricsInterceptor, CleanUrlInterceptor
     → @RestController (@RequestMapping /api/v1/<family>/<op>)
     → *Controller → service/util layer (PDFBox, Apache PDFBox, PDFium,
       iText?, Apache POI, commonmark, etc.)
     → external sidecars / JPA / file storage / engine proxy
     → ResponseEntity / StreamingResponseBody / file download
```

- Most PDF ops are `POST /api/v1/<family>/<op>` with `multipart/form-data`
  (`fileInput` + params), returning a PDF/ZIP attachment whose filename is
  generated by `GeneralUtils.generateFilename`.
- `AutoJobPostMapping`: endpoints also accept `?async=true` → job submitted
  to `JobExecutorService`, returns `{async, jobId, result}`; status via
  `GET /api/v1/general/job/{id}`, result via `/result` (`/result/files` for
  file outputs), cleanup via `POST /api/v1/general/jobs/cleanup`.
- Error shape: Spring `@ControllerAdvice`/`GlobalErrorResponseCustomizer`
  produce the frontend-consumed JSON error body.

## 4. Non-API surface

- `controller/web/`: `ReactRoutingController` (SPA fallback + share/mobile
  routes), `MetricsController` (`/metrics*`-style load/request stats),
  `RobotsController`, static resources from `static/` + frontend `dist/`.
- `pipeline/` controllers serve watched-folder automation.
- `UIDataController` / `ProprietaryUIDataController` feed the SPA bootstrap
  (version, enabled endpoints, licenses, login config).

## 5. Shutdown

Spring context close → `@PreDestroy`/scheduled-task cancellation;
`TempFileCleanupService` reaps `configs/temp/`; no explicit graceful-drain
logic beyond servlet container shutdown.
