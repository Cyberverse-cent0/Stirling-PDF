# Bean Reachability Audit — non-proprietary Java tree

Date: 2026-11-06. Scope: `app/common`, `app/core` main sources (605 files;
proprietary/saas excluded from candidacy but included as referrers).

## Method

1. Extracted the primary type name of every main-source `.java` file
   (class/interface/enum/record/@interface).
2. Counted word-boundary references to each name across **all** of `app/`
   (`.java`, `.xml`, `.yml`, `.properties`), including test sources,
   proprietary, and saas — so "zero references" means unreferenced
   anywhere in the repo.
3. Classified each zero-reference type by its DI registration mechanism.

Raw scan: 285 zero-reference names, of which ~260 are `*Test` classes —
test classes are never referenced by name, so they are false positives
by construction. **22 real candidates** remain.

## Verdicts

### DELETE candidates (12) — no stereotype, zero references — **deleted 2026-11-07**, `:common:compileJava`, `:stirling-pdf:compileJava`, `compileTestJava` all clean

| File | Why dead |
|---|---|
| `common/annotations/api/AccountSecurityApi.java` | Composed `@RestController`+`@Tag` annotation; no controller applies it (its controller family is deleted). |
| `common/annotations/api/AdminServerCertificateApi.java` | Same — unused composed annotation. |
| `common/annotations/api/AnalysisApi.java` | Same. |
| `common/annotations/api/ConfigApi.java` | Same. |
| `common/annotations/api/DatabaseManagementApi.java` | Same. |
| `common/annotations/api/FilterApi.java` | Same. |
| `common/annotations/api/InfoApi.java` | Same. |
| `common/annotations/api/MiscApi.java` | Same. |
| `common/annotations/api/PipelineApi.java` | Same. |
| `common/annotations/api/SettingsApi.java` | Same. |
| `core/config/fingerprint/FingerprintBasedSessionFilter.java` | Entire class body commented out; only a comment block remains. |
| `core/model/api/misc/AutoRotateAnalysisResult.java` | Response model for `auto-rotate-pdf` `dryRun`; the Java controller is deleted and the endpoint is native Rust (`automation::auto_rotate_pdf`). |

The sibling `*Api` annotations that ARE still applied (`AdminApi`,
`ConvertApi`, `GeneralApi`, `SecurityApi`, etc.) have live references and
stay. `PasswordController` uses `@SecurityApi` and is **not** a candidate.

### KEEP — live beans despite zero name references (10)

Class-name references are not how these are reached; Spring classpath
scanning registers them:

- `@Configuration`: `LocalDiskFileStoreConfiguration` (conditional on
  property), `TempFileConfiguration`, `PostHogConfig`, `SchedulingConfig`,
  `MetricsConfig`, `TelegramBotConfig` (conditional on
  `telegram.enabled`)
- `@Component`: `ToolIOOperationCustomizer` (springdoc `Global*Customizer`)
- `@Service`: `BasicPdfMarkdownExtractor` (injected via
  `PdfMarkdownExtractor`; 7 files reference the interface)
- `@RestController`: `HardwareSigningController`, `PasswordController` —
  both serve live P5 security endpoints that remain Java until the crypto
  engine lands

## Caveats / Phase-2 follow-up

- This is a **name-absence** audit, not a full DI graph. Dead *clusters*
  (class A only referenced by dead class B) would not appear. A transitive
  reachability pass from the `@SpringBootApplication` scan roots +
  servlet registrations is the correct Phase-2 check before bulk deletion.
- `AutoConfiguration.imports`/`spring.factories`/`web.xml`: none exist in
  the tree (verified), so classpath scanning is the only registration path.
- `SPDFApplication.isClassPresent` uses `Class.forName` only for
  security/saas feature-jar detection — unrelated to these candidates.
- Deletion should be one commit for the 12 candidates, followed by
  `task backend:check` (or equivalent javac sweep) to confirm nothing
  referenced them reflectively.

## Phase-2 — transitive DI-graph pass (2026-11-07)

Name-absence reachability was extended to a fixpoint over all 1,320
main-source files (common+core+proprietary+saas as referrers), rooted at
stereotype annotations, composed `@*Api` controller annotations,
framework supertypes (filters/listeners/repositories/entities), and any
class named in non-Java config resources. Result: 26 unreachable files —
18 in the OSS tree, 8 in proprietary/saas.

The 8 proprietary hits are `*RepositoryCustomImpl` classes — Spring Data
names them into service by convention, so they are framework-wired, not
dead (analysis false positive, correctly excluded from deletion).

The 18 OSS files were deleted with their dedicated tests (44 files
total): orphaned utils (`CbrUtils`, `CbzUtils`, `PdfToCbrUtils`,
`PdfToCbzUtils`, `PDFToFile`, `EmlToPdf`, `ChecksumUtils`, `ErrorUtils`,
`PropertyConfigs`, `UIScaling`, `WidthCalculator`), dead request models
(`AddPasswordRequest`, `AddWatermarkRequest`, `RemovePasswordRequest`,
`SanitizePdfRequest`, `PosterPdfRequest`, `ReplaceAndInvertColorRequest`,
`PipelineResult`). `SecurityRequestsTest` and
`PasswordRequestLoggingTest` were edited to drop dead-class cases while
keeping live P5 coverage (`SignPDFWithCertRequest`, `RedactPdfRequest`,
`PDFPasswordRequest`). `:common`/`:stirling-pdf` compileJava +
compileTestJava verified; the two edited test classes pass.

Remaining caveat: reflection/`Class.forName` lookups and SpEL strings
are invisible to name scanning; none were found in the audited tree, but
a runtime bean dump (`/actuator/beans`) on the proprietary build remains
the definitive check before the Java tree is retired.
