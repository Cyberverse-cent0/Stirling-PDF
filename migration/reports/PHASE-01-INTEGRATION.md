# Phase 1 — Rust Backend Integration Report

Baseline established for the strangler-arch Rust backend (`rust/` crate,
`stirling-server`). All migrated endpoints compile, lint clean, and pass the
full test suite. Java fallback proxy remains the router default for every
unmigrated `/api/**` route.

## Verification results (all run from `rust/`)

| Command | Result |
|---|---|
| `cargo fmt --check` | PASS |
| `cargo check` | PASS, 0 warnings |
| `cargo build` | PASS |
| `cargo test` | PASS — 65 passed, 0 failed |
| `cargo clippy --all-targets --all-features -- -D warnings` | PASS |

## Files inspected

- `rust/src/main.rs`, `rust/src/handlers/mod.rs` (full route table)
- All of `rust/src/handlers/`: analysis, automation, convert, filter, form,
  general, jobs, layout, merge, misc, passwords, proxy, security, split,
  transform, visual
- Support modules: `pdfdoc.rs`, `upload.rs`, `page_select.rs`, `text.rs`,
  `fontmetrics.rs`, `convert.rs`, `attachments.rs`, `imgextract.rs`,
  `imgcomp.rs`, `forms.rs`, `guard.rs`, `config.rs`, `error.rs`, `testkit.rs`
- `rust/MIGRATION.md`, `migration/FINAL_MIGRATION_AUDIT.md`, all reports in
  `migration/reports/`
- Git working tree: 272 changed paths (48 modified, 43 untracked, 181 deleted
  — mostly Java controllers superseded by Rust). Concurrent-agent work was
  preserved throughout.

## Integration defects found and fixed

1. **`handlers/form.rs` undeclared** — complete module (6 endpoints) with no
   `pub mod form;`. Added to `handlers/mod.rs`; routes for `/api/v1/form/*`
   were already registered.
2. **`convert.rs` stream typing** — `ChildStdout`/`ChildStderr` cannot share
   one array; boxed each as `Box<dyn Read + Send>` before concurrent reads.
3. **`handlers/convert.rs` missing import** — `image::GenericImageView` for
   `DynamicImage::dimensions()`; also replaced `chunks_exact(4)` per clippy.
4. **`handlers/form.rs` ownership** — field/widget mutation helpers moved the
   `dict` out and then reused it; switched to `doc.get_dictionary_mut(id)`
   in-place mutation.
5. **Form radio widgets** — `apply_value_to_field` now selects the widget
   appearance state matching the field's export value (`/V` → widget `/AS`).
6. **JSON array coercion** — scalar array elements are joined
   Java-compatibly (`["x","y"]` → `x,y`) for form payloads.
7. **`delete-fields` no-AcroForm semantics** — Java records one skip per
   requested name when the document has no form; test assertion corrected to
   match.
8. **`FormUpload.fields` dead field** — parsed for the not-yet-ported
   add/edit/modify-fields endpoints; annotated `#[allow(dead_code)]` with a
   note until those endpoints consume it.
9. **Upload size cap plumbing** — concurrent agent added
   `configure_max_part_bytes` but never called it; wired in `main`.
10. **Test-only `upload::from_vec`** — gated to `#[cfg(test)]`.

Audit-flagged orphans (`filter.rs`, `passwords.rs`, `visual.rs` stamp/
watermark, misc routes, `text.rs`/`fontmetrics.rs` declarations) are now all
declared and routed. Dead duplicate `misc::auto_rotate_pdf`/`repair_pdf`
were removed by a concurrent agent; the live implementations in
`automation.rs` are routed.

## Routes

86 explicit `.route()` registrations plus `.fallback(proxy::proxy)` as the
last layer. Explicit routes take precedence; everything else proxies to
`JAVA_BACKEND_URL` (default `http://localhost:8080`; `off`/empty disables).

Registered families: health/version/ping, general (merge, rotate, remove/
split/overlay/scale/crop pages, jobs API), misc (decompress, metadata,
flatten, attachments, extract-images, compress, show-javascript, add-image,
add-comments, add-stamp, auto-rename, repair, etc.), analysis (page-count,
basic-info, form-fields, font-info, security-info, …), filter (4), form
(6: fields, fields-with-coordinates, extract-csv, extract-xlsx, fill,
delete-fields), convert (`img/pdf`), security (sanitize-pdf, get-info-on-pdf,
remove-cert-sign, add-watermark, add-password, remove-password,
prepare-pdf-session), jobs (status, result, cancel, download, cleanup).

## Still missing / proxied to Java

- `restore-pdf-protection` — requires session-permission semantics not yet
  portable (see `handlers/passwords.rs` header).
- Form `add-fields`, `edit-fields`, `modify-fields` — `fields` multipart part
  is already parsed; endpoints pending.
- Form `remove-blanks` — deliberately unrouted: needs a renderer; stays on
  Java per audit + agent-04.
- Engine-dependent families: OCR, LibreOffice conversions, signing, PDF/A,
  rasterization, and other advanced mutation endpoints (~78 core endpoints
  remain proxied per the audit snapshot).

## Known defects / caveats

- `handlers/passwords.rs` documents parity gaps: selective removal fields
  and some encryption-metadata edge cases; `prepare-pdf-session` reproduces
  `PdfSessionPermissions.requireOperation` gating. Passwords are never
  logged.
- `rust/engine/` is an untracked separate crate outside this workspace —
  not covered by these checks.
- `rust/f.pdf` is a stray untracked fixture at the crate root.
- `cargo test` exercises unit + fixture tests only; no live HTTP parity run
  was performed (Java backend not started).

## Recommended next phase

Phase 2: end-to-end route verification — boot the Rust service against a
running Java backend, exercise every registered route with real uploads,
confirm fallback proxying for unmigrated endpoints, then port the remaining
form field-mutation endpoints and `restore-pdf-protection`.
