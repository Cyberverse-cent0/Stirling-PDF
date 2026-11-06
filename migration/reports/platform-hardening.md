# Platform hardening — audit findings

Follow-up to the migration audit's "critical problems beyond endpoint migration". Status per finding:

## Implemented

### Error response parity
- `rust/src/error.rs`: `AppError` now renders RFC 7807 `application/problem+json` bodies matching Java's `GlobalExceptionHandler` — `type` (ErrorTypes URI per variant), `title` (reason phrase), `status`, `detail`, `timestamp` (ISO-8601 millis, like `Instant.now()`). Status mapping unchanged (400/404/413/422/403/500).
- Remaining gap: extractor-level rejections (malformed multipart boundary, invalid path params) still produce axum's plain-text defaults; Java routes those through `GlobalExceptionHandler` too. A `HandleErrorLayer` pass is the follow-up.

### CORS parity
- `rust/src/config.rs`: `cors_origin_patterns`/`cors_origin_allowed` port `WebMvcConfig.addCorsMappings` — `CORS_ALLOWED_ORIGINS` env (comma-separated, `*` glob semantics), automatic Tauri origins, `STIRLING_PDF_TAURI_MODE`, allow-all default.
- `rust/src/main.rs`: CorsLayer now mirrors Java's method set (adds PATCH), allowed/exposed header lists, `allow_credentials(true)`, `max_age(3600)`.

### Upload size cap
- `rust/src/upload.rs`: `read_part_bytes` streams each multipart part with a running byte count and rejects over `RUST_MAX_UPLOAD_BYTES` (default 2000 MB = `spring.servlet.multipart.max-file-size`). Applies to file parts and text params in both `receive_multipart` and `receive_files_multipart`.

### Endpoint-enable enforcement + cache headers
- `rust/src/guard.rs` + `handlers/mod.rs`: `api_guard` middleware on the whole router. `RUST_DISABLED_ENDPOINTS` (comma-separated exact paths or `prefix*` globs) → 403 with Spring Boot's error-dispatch body shape (`{timestamp,status,error,path}`, message excluded per `include-message=never`). Sets `Cache-Control: private, no-store` on every `/api/` response like `EndpointInterceptor`.

### Multipart disk spooling
- `rust/src/upload.rs`: `Upload.bytes` is now `UploadBytes` — parts stream to a `NamedTempFile` while parsing (Java `DiskFileItem` parity), capped by `RUST_MAX_UPLOAD_BYTES`. The `&[u8]` view materializes lazily on first access and is cached per `Upload`; `Deref`/`AsRef` keep all ~70 `&upload.bytes` call sites source-compatible. `proxy::forward_parts` re-streams the spool file to Java via `Part::stream_with_length` instead of re-buffering.
- Remaining gap: handlers that consume the bytes still materialize them once per document (`lopdf` needs the full object graph in memory regardless), so per-document peak is unchanged — the win is bounded parse-phase memory, early rejection, and zero-copy proxy forwarding. Migrating `pdfdoc::load` call sites to `Document::load(path)` is the follow-up for deeper savings.
- `form.rs`'s `receive_form_multipart` drains unknown text parts with unbounded `field.text()`; its file parts spool correctly via `Upload::from_field`.

## Not implemented (documented decisions)

- **Authentication**: OSS `/api/**` is unauthenticated in Java as well, so no middleware was added. For proprietary/saas deployments the Rust gateway must sit behind Java (or the deployment must accept that Rust routes bypass license/Supabase filters). Proxy forwarding of `Authorization`/`X-API-KEY`/cookies to Java is already intact (`forward_request_headers` passes everything except hop-by-hop headers).
- **Full endpoint/tool-group enablement**: `RUST_DISABLED_ENDPOINTS` covers explicit disables only; Java's functional-group and tool-group resolution from `settings.yml` is not ported.
- **Metrics/audit, startup config parity, static frontend hosting**: unchanged; Rust remains an API gateway, not a standalone app.

## Env vars added

| Var | Default | Meaning |
|---|---|---|
| `CORS_ALLOWED_ORIGINS` | unset (allow all) | Comma-separated origin patterns |
| `STIRLING_PDF_TAURI_MODE` | `false` | Restrict CORS to localhost/Tauri |
| `RUST_MAX_UPLOAD_BYTES` | 2000 MB | Per-part multipart cap |
| `RUST_DISABLED_ENDPOINTS` | empty | Comma-separated disabled `/api` paths/globs |

Formatting: `rustfmt` run on all touched files. No builds/tests run.
