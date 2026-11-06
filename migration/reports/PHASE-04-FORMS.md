# Phase 4 — Form API migration to Rust

## Inventory verified

`FormFillController` (`app/core/.../api/form/`) exposes exactly 9
endpoints, all now implemented natively in `rust/src/handlers/form.rs`
and registered in `rust/src/handlers/mod.rs`:

| Endpoint | Rust handler | Status |
|---|---|---|
| `POST /form/fields` | `list_fields` | ✅ (pre-existing) |
| `POST /form/fields-with-coordinates` | `list_fields_with_coordinates` | ✅ (pre-existing) |
| `POST /form/extract-csv` | `extract_csv` | ✅ + native `data` pre-pass |
| `POST /form/extract-xlsx` | `extract_xlsx` | ✅ hand-rolled OOXML workbook |
| `POST /form/fill` | `fill_form` | ✅ text/checkbox/radio/choice, `flatten` |
| `POST /form/delete-fields` | `delete_fields_endpoint` | ✅ tree + `/Annots` detach |
| `POST /form/add-fields` | `add_fields_endpoint` | ✅ new this phase |
| `POST /form/modify-fields` | `modify_fields_endpoint` | ✅ new this phase |
| `POST /form/edit-fields` | `edit_fields_endpoint` | ✅ batch add/modify/delete + `includeFields` ZIP |

`src/forms.rs` still owns `flatten()` used by `misc/flatten` and
`fill?flatten=true`.

## What was ported this phase

- **Payload parsing** — `FieldDefinition` (serde, camelCase) covers both
  `NewFormFieldDefinition` and `ModifyFormFieldDefinition` (`targetName`
  read only by modify paths); `FieldEditBatch` keeps `delete` as raw
  JSON so every `parseNameList` shape works. Multipart part names
  `fields`/`updates`/`edits` are accepted as file parts or text parts.
- **Name validation** — `invalidFieldNameReason` (blank ok, ISO-control
  and `.` rejected), `renameProblem`, `leafName`/`parentPrefix`,
  `requireUsableFieldNames` → 400 before the document is touched.
- **add-fields** — AcroForm creation when absent, `/DA`+`/DR` Helv
  defaults, page-count guard, page-index clamp, cropbox-relative
  lower-left geometry (inherited CropBox via `pdfdoc::page_boxes`),
  unique-name generation against qualified names, per-type creation:
  - text → merged field/widget dict, `/Ff` ReadOnly/Required/Multiline/Comb, `/MaxLen`, `/V`
  - checkbox → `/FT Btn`, `/AP /N` Yes/Off streams, `/AS`+`/V` Off
  - combobox/listbox → `/FT Ch`, Combo/MultiSelect flags, `/Opt`, `/V`
  - button → pushbutton flag, captioned `/AP`, `/A` actions
    (`reset`/`print`/`uri:`/`submit:` specs; invalid specs reported)
  - radio → field + N widget kids, `radioOptionRects` port (gap/size
    overrides), sanitized unique on-states, `/Opt` exports, `/V` default
  - signature → skipped `'signature' fields cannot be created`
- **modify-fields** — locate by qualified/partial name, widget/page
  resolution (`/P` then `/Annots` scan), in-place property edits
  (rename, label→`/TU`, required/read-only/multiline/comb/maxLen flags,
  `/Opt`+multiSelect on choice only, tooltip, `/V` default, font size →
  `/DA` + `/AP` strip, button actions, geometry: widget0 resized,
  siblings shifted by anchor delta, radio reflow on explicit
  optionGap/optionSize). Type changes and nested-field rebuilds follow
  Java's delete-and-recreate path with the same refusal reasons.
- **edit-fields** — Java's order (modify → delete → add), skipped-edit
  tracking into the existing Base64 `X-Stirling-Skipped-Field-Edits*`
  headers, `includeFields=true` → ZIP with deflated `fields.json`
  (coordinates extraction) + stored `document.pdf`.
- **Bug fix** — field flags were read from `/FF`; the PDF key is `/Ff`,
  so radio/choice/required/read-only/multiline detection never fired.
  Now read own-then-inherited like `PDField.getFieldFlags`.

## Known divergences (do not treat as full parity)

- **Typesetting fidelity.** `refresh_appearances` uses Standard-14
  metrics and WinAnsi only: no font substitution fallback, no RTL or
  complex-script shaping, no comb-cell layout, no listbox scroll pane,
  no multiline wrapping, approximate `/DA` colour space handling.
  Fields it cannot draw keep `/NeedAppearances=true`.
- **Toggle geometry** matches `toggleStream` numerically but stroke
  caps/joins are PDF defaults, not PDFBox's exact graphics state.
- **`repairMissingWidgetPageReferences`** pre-pass is not ported; the
  same `/P`-then-`/Annots` fallback runs inline where needed.
- **XLSX** is a minimal OOXML workbook (inline strings, one
  "Form Fields" sheet) — cell-for-cell equivalent to POI's output but
  not byte-identical.
- **Flatten** uses `crate::forms::flatten` (lopdf content merge), not
  PDFBox's `acroForm.flatten()`; the render-to-image fallback for
  pathological documents does not exist.

## Parity hardening (second stabilization pass)

Java's `ensureAppearances` was previously approximated by only setting
`/NeedAppearances`. Now `refresh_appearances` regenerates real `/AP`
streams wherever lopdf can draw them, then clears the flag only when
every widget carries `/AP /N` — the same post-condition Java reaches:

- **Single-line text + combobox** — `/MK` background/border, `/DA`
  font size (0 → auto-fit) and colour, `/Q` quadding measured with
  `fontmetrics` Standard-14 widths, WinAnsi-encoded literal.
- **Checkbox/radio** — `toggleStream` geometry ported exactly (10%
  inset, check polyline, radio circle + dot); per-widget on-states and
  `/AS` selection from `/V`.
- **Pushbutton** — `/MK /CA` caption (now written at creation, matching
  `applyNewFieldDefinition`), `/MK /BG`/`/BC` colours honoured,
  font size `min(12, h*0.6)`, caption centred via Helvetica metrics,
  `/AS` removed.
- **Listbox, multiline, comb** — not drawable; their widgets keep
  `/NeedAppearances=true` so viewers regenerate (Java parity for the
  fallback contract, not the glyph output).

Other contract fixes:

- **`/fill` strict mode** — Java calls `applyFieldValues(…, strict=true)`
  which throws `IOException("No AcroForm present")` on formless PDFs,
  even with empty `data`. Rust now returns 500 under strict and stays a
  no-op for the non-strict CSV/XLSX pre-pass; `flatten` no longer runs
  on a formless document (Java's `flattenEntireDocument` early-returns).
- **`repairWidgetGeometry`** — ported: degenerate/missing `/Rect`
  widgets get a 200×40 (media-clamped) box at the media lower-left,
  `/P` is set, and the widget is re-added to `/Annots` only if absent.
  Runs on every `applyFieldValues` call like Java.
- **Appearance refresh on delete/modify/add** — Java calls
  `ensureAppearances` after each mutation; all paths now call
  `refresh_appearances`.
- **Dead code removed** — old `button_ap_stream`/`push_button_ap`
  superseded by `toggle_ap_stream`/`pushbutton_ap_stream`.

## Tests — stabilization phase results

`cargo test form::` **executed**: 17 `handlers::form::tests` pass,
0 fail. `cargo check --tests` clean for `form.rs`/`misc.rs` (warnings
elsewhere belong to other agents' modules).

Coverage: text fill + AP content (`(Alice)` in stream, `/Helv 12 Tf`),
`/NeedAppearances` cleared when all widgets have `/AP`, pushbutton
`/MK /CA` + centred caption AP, listbox keeps the flag, degenerate-rect
repair, checkbox/radio/choice fill, delete tree+annots, no-AcroForm
strict/non-strict split, payload parsers, add/modify/edit-batch.

## Endpoint parity classification (Java vs Rust)

| Endpoint | Class | Notes |
|---|---|---|
| `POST /form/fields` | COMPLETE | `{fields, template}` shape verified against `FormFieldExtraction`; coords use cropbox-relative lower-left like `FormUtils`. |
| `POST /form/fields-with-coordinates` | COMPLETE | Bare JSON array, matches `List<FormFieldWithCoordinates>` return. |
| `POST /form/extract-csv` | COMPLETE | CSV rows/quoting match; `data` pre-pass native, non-strict (silent on formless PDFs like Java). |
| `POST /form/extract-xlsx` | PARTIAL | Cell-for-cell equivalent minimal OOXML; not byte-identical to POI (no styles/shared strings). Filename `*_extracted.xlsx` matches. |
| `POST /form/fill` | PARTIAL | Strict AcroForm error, geometry repair, real AP generation for text/choice-toggle/button now match Java's pipeline. Remaining gap: listbox/multiline/comb glyph output and PDFBox's exact typesetting (font fallback, RTL, comb cells) still defer to viewer regeneration. |
| `POST /form/delete-fields` | COMPLETE | Qualified/partial lookup, tree detach, `/Annots` removal, appearance refresh, `*_updated.pdf`. |
| `POST /form/add-fields` | PARTIAL | Full type coverage + validation + geometry + `/MK /CA` + real APs. Gap: listbox/multiline AP fidelity. |
| `POST /form/modify-fields` | PARTIAL | In-place edits + delete/recreate + appearance refresh match Java's structure; same AP-fidelity caveat. |
| `POST /form/edit-fields` | PARTIAL | Modify→delete→add ordering, skipped-edit Base64 headers, `includeFields` ZIP all match; same AP-fidelity caveat. |

No Form endpoint proxies to Java; `JAVA_BACKEND_URL` fallback is not
involved. All are multipart `file` + optional part names as in Java.

## Stabilization defects fixed this pass

- Stale doc comment on `TerminalField.ff` claiming own-only flags —
  corrected to own-then-ancestor (`PDField.getFieldFlags` semantics).
- Confirmed `/Ff` (not `/FF`) flag reads and inherited lookup are in
  place; radio/choice/required/multiline detection verified by tests.

## Unverified behavior

- Real-world PDF viewers rendering created/mutated fields (no golden
  files yet); malformed AcroForm trees beyond test fixtures; encrypted
  input PDFs; very large field counts.
- Multipart-level handler tests (axum `Multipart` request path) —
  tests exercise the `*_work` functions directly, same pattern as the
  rest of the suite.

## Files

- `rust/src/handlers/form.rs` — all 9 endpoints + mutation engine (~1500 new lines)
- `rust/src/handlers/mod.rs` — 3 new route registrations
- `migration/reports/FINAL_MIGRATION_AUDIT.md` — form section updated to ✅
