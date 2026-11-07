//! `FormFillController` (`/api/v1/form/*`): field inspection
//! (`fields`, `fields-with-coordinates`), CSV/XLSX extraction, value
//! filling (`fill`), field deletion (`delete-fields`), and field
//! creation/editing (`add-fields`, `edit-fields`, `modify-fields`).
//!
//! The field walk mirrors `FormUtils`/`PDFieldFactory`: a node is
//! non-terminal when any `/Kids` entry carries a `/T` string; otherwise it
//! is terminal, typed by the nearest `/FT` up the `/Parent`-or-`/P` chain,
//! with `/Ff` resolved own-then-ancestor.
//!
//! Appearance divergence vs Java: `refresh_appearances` draws real `/AP`
//! streams for single-line text/combobox (Standard-14 metrics via
//! `fontmetrics`), checkbox/radio toggles and pushbutton captions — the
//! same inputs PDFBox's `AppearanceGeneratorHelper` uses. Listboxes,
//! multiline and comb fields still rely on `/NeedAppearances` plus the
//! `/DR` Helv resource for viewer-side regeneration.

use std::collections::{HashMap, HashSet};

use axum::extract::Multipart;
use axum::response::{IntoResponse, Response};
use axum::Json;
use lopdf::{decode_text_string, dictionary, Dictionary, Document, Object, ObjectId};
use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::error::{AppError, AppResult};
use crate::handlers::jobs::AppState;
use crate::handlers::merge::{file_response, pdf_response};
use crate::pdfdoc;
use crate::upload::Upload;

const T_TEXT: &str = "text";
const T_CHECKBOX: &str = "checkbox";
const T_COMBOBOX: &str = "combobox";
const T_LISTBOX: &str = "listbox";
const T_RADIO: &str = "radio";
const T_BUTTON: &str = "button";
const T_SIGNATURE: &str = "signature";

const FF_READ_ONLY: i64 = 1;
const FF_REQUIRED: i64 = 1 << 1;
const FF_MULTILINE: i64 = 1 << 12;
const FF_RADIO: i64 = 1 << 15;
const FF_PUSHBUTTON: i64 = 1 << 16;
const FF_COMBO: i64 = 1 << 17;
const FF_MULTI_SELECT: i64 = 1 << 21;

const SAME_LINE_THRESHOLD_PT: f32 = 10.0;

#[derive(Clone, Copy, PartialEq, Debug)]
enum FieldKind {
    Text,
    Checkbox,
    Combobox,
    Listbox,
    Radio,
    Button,
    Signature,
}

impl FieldKind {
    fn name(self) -> &'static str {
        match self {
            Self::Text => T_TEXT,
            Self::Checkbox => T_CHECKBOX,
            Self::Combobox => T_COMBOBOX,
            Self::Listbox => T_LISTBOX,
            Self::Radio => T_RADIO,
            Self::Button => T_BUTTON,
            Self::Signature => T_SIGNATURE,
        }
    }
    fn is_choice(self) -> bool {
        matches!(self, Self::Combobox | Self::Listbox | Self::Radio)
    }
}

/// A widget annotation as stored in the field's `/Kids` (or the field
/// dictionary itself when it has no kids), paired with its resolved form.
struct Widget {
    stored: Object,
    dict: Dictionary,
}

/// One terminal AcroForm field. `chain[0]` is the field dictionary and the
/// rest are its traversal ancestors — the chain `PDField.
/// getInheritableAttribute` walks.
struct TerminalField {
    /// The object the field tree referenced (for `/Annots` identity lookups).
    stored: Object,
    chain: Vec<Dictionary>,
    qualified: String,
    kind: FieldKind,
    /// `/Ff` flags resolved own-then-ancestor, like `PDField.
    /// getFieldFlags`' inheritable lookup.
    ff: i64,
    widgets: Vec<Widget>,
}

fn dict_get<'a>(dict: &'a Dictionary, key: &[u8]) -> Option<&'a Object> {
    dict.get(key).ok()
}

fn resolved<'a>(doc: &'a Document, object: &'a Object) -> Option<&'a Object> {
    doc.dereference(object).ok().map(|(_, o)| o)
}

fn dict_of(doc: &Document, object: &Object) -> Option<Dictionary> {
    pdfdoc::dict_owned(doc, object).ok().flatten()
}

fn text_of(doc: &Document, object: &Object) -> Option<String> {
    let object = resolved(doc, object)?;
    decode_text_string(object).ok()
}

fn name_of(doc: &Document, object: &Object) -> Option<String> {
    let object = resolved(doc, object)?;
    object
        .as_name()
        .ok()
        .map(|n| String::from_utf8_lossy(n).into_owned())
}

fn int_of(doc: &Document, object: &Object) -> Option<i64> {
    match resolved(doc, object)? {
        Object::Integer(n) => Some(*n),
        Object::Real(r) => Some(*r as i64),
        _ => None,
    }
}

fn num_of(doc: &Document, object: &Object) -> Option<f64> {
    match resolved(doc, object)? {
        Object::Integer(n) => Some(*n as f64),
        Object::Real(r) => Some(*r as f64),
        _ => None,
    }
}

fn ff_flag(ff: i64, bit: i64) -> bool {
    ff & bit != 0
}

/// `/Kids` entries that are dictionaries, as `(stored, dict)` pairs —
/// `kids.getObject(i)` resolution, keeping the stored object for identity.
fn kid_dicts(doc: &Document, dict: &Dictionary) -> Vec<(Object, Dictionary)> {
    let mut out = Vec::new();
    let Some(items) = dict_get(dict, b"Kids")
        .and_then(|o| resolved(doc, o))
        .and_then(|o| match o {
            Object::Array(items) => Some(items.clone()),
            _ => None,
        })
    else {
        return out;
    };
    for item in &items {
        if let Some(d) = dict_of(doc, item) {
            out.push((item.clone(), d));
        }
    }
    out
}

/// The `/T` partial name when it is a string object — `COSDictionary.
/// getString` semantics: non-string entries do not count.
fn partial_name(doc: &Document, dict: &Dictionary) -> Option<String> {
    let object = dict_get(dict, b"T")?;
    let object = resolved(doc, object)?;
    match object {
        Object::String(..) => decode_text_string(object).ok(),
        _ => None,
    }
}

/// `PDFieldFactory.findFieldType`: own `/FT`, else recurse through the
/// `/Parent`-or-`/P` dictionary chain (depth-capped like PDFBox's `seen`).
fn find_field_type(doc: &Document, dict: &Dictionary, depth: usize) -> Option<String> {
    if depth > 64 {
        return None;
    }
    if let Some(ft) = dict_get(dict, b"FT").and_then(|o| name_of(doc, o)) {
        return Some(ft);
    }
    for key in [b"Parent".as_slice(), b"P".as_slice()] {
        if let Some(parent) = dict_get(dict, key).and_then(|o| dict_of(doc, o)) {
            return find_field_type(doc, &parent, depth + 1);
        }
    }
    None
}

fn classify_terminal(ft: &str, ff: i64) -> Option<FieldKind> {
    match ft {
        "Tx" => Some(FieldKind::Text),
        "Sig" => Some(FieldKind::Signature),
        "Ch" => Some(if ff_flag(ff, FF_COMBO) {
            FieldKind::Combobox
        } else {
            FieldKind::Listbox
        }),
        "Btn" => Some(if ff_flag(ff, FF_RADIO) {
            FieldKind::Radio
        } else if ff_flag(ff, FF_PUSHBUTTON) {
            FieldKind::Button
        } else {
            FieldKind::Checkbox
        }),
        _ => None,
    }
}

/// Depth-first pre-order walk of the AcroForm field tree, mirroring
/// `PDFieldTree`'s iterator (node, then children recursively) and its
/// dictionary-identity cycle guard via reference ids.
fn collect_fields(
    doc: &Document,
    stored: &Object,
    dict: &Dictionary,
    prefix: &str,
    chain: &[Dictionary],
    visited: &mut HashSet<ObjectId>,
    out: &mut Vec<TerminalField>,
) {
    if let Ok(id) = stored.as_reference() {
        if !visited.insert(id) {
            return;
        }
    }

    let partial = partial_name(doc, dict);
    let qualified = match &partial {
        Some(p) if prefix.is_empty() => p.clone(),
        Some(p) => format!("{prefix}.{p}"),
        None => prefix.to_string(),
    };

    let kids = kid_dicts(doc, dict);
    // PDFieldFactory: any kid carrying a /T string makes this node
    // non-terminal; the kids are then classified on their own.
    if kids.iter().any(|(_, kid)| partial_name(doc, kid).is_some()) {
        let mut child_chain = Vec::with_capacity(chain.len() + 1);
        child_chain.push(dict.clone());
        child_chain.extend_from_slice(chain);
        for (kid_stored, kid_dict) in kids {
            collect_fields(
                doc,
                &kid_stored,
                &kid_dict,
                &qualified,
                &child_chain,
                visited,
                out,
            );
        }
        return;
    }

    let Some(ft) = find_field_type(doc, dict, 0) else {
        return;
    };
    // /Ff is inheritable: own dictionary first, then the ancestor chain.
    let ff = std::iter::once(dict)
        .chain(chain.iter())
        .find_map(|d| dict_get(d, b"Ff").and_then(|o| int_of(doc, o)))
        .unwrap_or(0);
    let Some(kind) = classify_terminal(&ft, ff) else {
        return;
    };

    // PDTerminalField.getWidgets: no /Kids → the field dict itself is the
    // widget; otherwise every kid dictionary, subtype unchecked. An
    // explicitly empty /Kids yields no widgets.
    let has_kids_key = dict_get(dict, b"Kids").is_some();
    let widgets = if !has_kids_key {
        vec![Widget {
            stored: stored.clone(),
            dict: dict.clone(),
        }]
    } else {
        kids.into_iter()
            .map(|(stored, dict)| Widget { stored, dict })
            .collect()
    };

    let mut chain_vec = Vec::with_capacity(chain.len() + 1);
    chain_vec.push(dict.clone());
    chain_vec.extend_from_slice(chain);
    out.push(TerminalField {
        stored: stored.clone(),
        chain: chain_vec,
        qualified,
        kind,
        ff,
        widgets,
    });
}

fn terminal_fields(doc: &Document) -> AppResult<Vec<TerminalField>> {
    let Some(form) = pdfdoc::acroform(doc) else {
        return Ok(Vec::new());
    };
    let Some(fields_array) = dict_get(&form, b"Fields")
        .and_then(|o| resolved(doc, o))
        .and_then(|o| match o {
            Object::Array(items) => Some(items.clone()),
            _ => None,
        })
    else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    let mut visited = HashSet::new();
    for item in &fields_array {
        if let Some(dict) = dict_of(doc, item) {
            collect_fields(doc, item, &dict, "", &[], &mut visited, &mut out);
        }
    }
    Ok(out)
}

// ---------- inheritable lookups ----------

/// `PDField.getInheritableAttribute`: own dictionary first, then the
/// traversal ancestor chain.
fn inheritable<'a>(field: &'a TerminalField, key: &[u8]) -> Option<&'a Object> {
    field.chain.iter().find_map(|dict| dict_get(dict, key))
}

// ---------- value / options ----------

/// `safeFieldValue`: choice fields join their selected values, signatures
/// have none, buttons read the inherited `/V` name, text reads inherited
/// `/V` string-or-stream.
fn safe_field_value(doc: &Document, field: &TerminalField) -> Option<String> {
    match field.kind {
        FieldKind::Signature => None,
        FieldKind::Combobox | FieldKind::Listbox => {
            // PDChoice.getValueFor reads the field's OWN /V and only direct
            // COSString array elements count.
            let v = resolved(doc, dict_get(&field.chain[0], b"V")?)?;
            let values: Vec<String> = match v {
                Object::String(..) => vec![decode_text_string(v).ok()?],
                Object::Array(items) => items
                    .iter()
                    .filter_map(|item| match item {
                        Object::String(..) => decode_text_string(item).ok(),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            };
            if values.is_empty() {
                None
            } else {
                Some(values.join(","))
            }
        }
        FieldKind::Checkbox | FieldKind::Radio | FieldKind::Button => button_value(doc, field),
        FieldKind::Text => {
            let v = resolved(doc, inheritable(field, b"V")?)?;
            match v {
                Object::String(..) => decode_text_string(v).ok(),
                Object::Stream(stream) => {
                    Some(String::from_utf8_lossy(&stream.content).into_owned())
                }
                _ => None,
            }
        }
    }
}

/// `PDButton.getExportValues`: inheritable `/Opt` — a string becomes a
/// singleton, an array contributes its direct string elements only
/// (`toCOSStringStringList` does not resolve references).
fn button_export_values(doc: &Document, field: &TerminalField) -> Vec<String> {
    let Some(opt) = inheritable(field, b"Opt") else {
        return Vec::new();
    };
    let Some(opt) = resolved(doc, opt) else {
        return Vec::new();
    };
    match opt {
        Object::String(..) => decode_text_string(opt)
            .ok()
            .filter(|s| !s.is_empty())
            .into_iter()
            .collect(),
        Object::Array(items) => items
            .iter()
            .filter_map(|item| match item {
                Object::String(..) => decode_text_string(item).ok(),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// `PDButton.getValue`/`getValueAsString`: inherited `/V`; a name becomes
/// its string (or an `/Opt` index when numeric), anything else is "Off".
fn button_value(doc: &Document, field: &TerminalField) -> Option<String> {
    let export_values = button_export_values(doc, field);
    // PDButton.getValue: absent or non-name /V means "Off".
    let v = inheritable(field, b"V").and_then(|o| resolved(doc, o));
    match v {
        Some(Object::Name(name)) => {
            let string_value = String::from_utf8_lossy(name).into_owned();
            if !export_values.is_empty() {
                if let Ok(idx) = string_value.parse::<i64>() {
                    if idx >= 0 && (idx as usize) < export_values.len() {
                        return Some(export_values[idx as usize].clone());
                    }
                }
            }
            Some(string_value)
        }
        _ => Some("Off".to_string()),
    }
}

/// `FieldUtils.getPairableItems` over the choice field's OWN `/Opt`:
/// `resolveIndirectChoiceOptions` has already replaced indirect entries
/// with their resolved objects, so top-level entries resolve here while
/// the elements inside a pair array must still be direct strings.
fn pairable_items(doc: &Document, field: &TerminalField, pair_idx: usize) -> Vec<String> {
    let Some(opt) = dict_get(&field.chain[0], b"Opt") else {
        return Vec::new();
    };
    let Some(opt) = resolved(doc, opt) else {
        return Vec::new();
    };
    match opt {
        Object::String(..) => decode_text_string(opt).ok().into_iter().collect(),
        Object::Array(items) => {
            let mut out = Vec::new();
            for entry in items {
                match resolved(doc, entry) {
                    Some(Object::String(..)) => {
                        if let Some(s) = text_of(doc, entry) {
                            out.push(s);
                        }
                    }
                    Some(Object::Array(pair)) if pair.len() > pair_idx => {
                        if let Some(s) = match pair.get(pair_idx) {
                            Some(Object::String(..)) => decode_text_string(&pair[pair_idx]).ok(),
                            _ => None,
                        } {
                            out.push(s);
                        }
                    }
                    _ => {}
                }
            }
            out
        }
        _ => Vec::new(),
    }
}

/// `resolveOptions`: choice fields union trimmed export+display values in
/// order; radio/checkbox pass their export values through unfiltered.
fn resolve_options(doc: &Document, field: &TerminalField) -> Vec<String> {
    match field.kind {
        FieldKind::Combobox | FieldKind::Listbox => {
            let mut allowed: Vec<String> = Vec::new();
            for s in pairable_items(doc, field, 0)
                .into_iter()
                .chain(pairable_items(doc, field, 1))
            {
                let t = s.trim();
                if !t.is_empty() && !allowed.iter().any(|x| x == t) {
                    allowed.push(t.to_string());
                }
            }
            allowed
        }
        FieldKind::Radio | FieldKind::Checkbox => button_export_values(doc, field),
        _ => Vec::new(),
    }
}

fn resolve_display_options(doc: &Document, field: &TerminalField) -> Vec<String> {
    if matches!(field.kind, FieldKind::Combobox | FieldKind::Listbox) {
        pairable_items(doc, field, 1)
    } else {
        Vec::new()
    }
}

/// `resolveTooltip`: per widget, `/NM` first then `/TU`, first non-blank
/// wins and moves to the next widget.
fn resolve_tooltip(doc: &Document, field: &TerminalField) -> Option<String> {
    for widget in &field.widgets {
        for key in [b"NM".as_slice(), b"TU".as_slice()] {
            if let Some(s) = widget
                .dict
                .get(key)
                .ok()
                .and_then(|o| text_of(doc, o))
                .filter(|t| !t.trim().is_empty())
            {
                return Some(s);
            }
        }
    }
    None
}

// ---------- labels ----------

fn punct_to_space(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_space = false;
    for c in s.chars() {
        if c.is_ascii_punctuation() {
            pending_space = true;
        } else if pending_space {
            if !out.ends_with(' ') && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.push(c);
        } else {
            out.push(c);
        }
    }
    out
}

fn normalize_label(s: &str) -> String {
    punct_to_space(s)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_hex_uuid(nospace: &str) -> bool {
    nospace.len() >= 32
        && nospace[..8].chars().all(|c| c.is_ascii_hexdigit())
        && nospace[8..].chars().all(|c| c.is_ascii_hexdigit())
}

const GENERIC_WORDS: [&str; 11] = [
    "field",
    "text",
    "checkbox",
    "radio",
    "button",
    "signature",
    "name",
    "value",
    "option",
    "select",
    "choice",
];

/// `^(field|text|…|choice)(\s*\d+)?$` case-insensitive.
fn matches_generic_name(s: &str) -> bool {
    let lower = s.to_lowercase();
    for word in GENERIC_WORDS {
        if let Some(rest) = lower.strip_prefix(word) {
            let rest = rest.trim_start();
            if rest.is_empty() || rest.chars().all(|c| c.is_ascii_digit()) {
                return true;
            }
        }
    }
    false
}

/// `^[A-Za-z]{1,2}\s*\d{1,3}$`.
fn is_simple_form_field(s: &str) -> bool {
    let b = s.as_bytes();
    if b.is_empty() {
        return false;
    }
    let mut i = 0;
    while i < b.len() && i < 2 && b[i].is_ascii_alphabetic() {
        i += 1;
    }
    if i == 0 {
        return false;
    }
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    let start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let digits = i - start;
    i == b.len() && (1..=3).contains(&digits)
}

/// `^(?:t\s*)?\d+$` case-insensitive.
fn is_optional_t_numeric(s: &str) -> bool {
    let mut t = s.trim();
    if let Some(rest) = t.strip_prefix('t').or_else(|| t.strip_prefix('T')) {
        t = rest.trim_start();
    }
    !t.is_empty() && t.chars().all(|c| c.is_ascii_digit())
}

fn uuid_like(simplified: &str) -> bool {
    let nospaces: String = simplified.chars().filter(|c| !c.is_whitespace()).collect();
    is_hex_uuid(&nospaces)
}

fn looks_generic(value: &str) -> bool {
    let simplified = normalize_label(value);
    if simplified.is_empty() {
        return true;
    }
    uuid_like(&simplified)
        || matches_generic_name(&simplified)
        || is_simple_form_field(&simplified)
        || is_optional_t_numeric(&simplified)
}

/// Same as `looks_generic` minus the generic-word list: human-typed
/// placeholders like "Choice" stay usable labels here.
fn looks_auto_generated(value: &str) -> bool {
    let simplified = normalize_label(value);
    if simplified.is_empty() {
        return true;
    }
    if uuid_like(&simplified) {
        return true;
    }
    let lower = simplified.to_lowercase();
    let is_field = lower == "field"
        || lower
            .strip_prefix("field")
            .map(|r| {
                let r = r.trim_start();
                r.is_empty() || r.chars().all(|c| c.is_ascii_digit())
            })
            .unwrap_or(false);
    is_field || is_simple_form_field(&simplified) || is_optional_t_numeric(&simplified)
}

fn humanize_name(name: &str) -> Option<String> {
    // \[[^\]]*\] → ' ', then '.'/'-'/'_' → ' ', then camel/digit
    // boundaries → ' ', whitespace collapsed.
    let chars: Vec<char> = name.chars().collect();
    let mut cleaned = String::with_capacity(name.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '[' {
            if let Some(close) = chars[i + 1..].iter().position(|&c| c == ']') {
                cleaned.push(' ');
                i += close + 2;
                continue;
            }
        }
        cleaned.push(chars[i]);
        i += 1;
    }
    let spaced: Vec<char> = cleaned
        .chars()
        .map(|c| {
            if c == '.' || c == '-' || c == '_' {
                ' '
            } else {
                c
            }
        })
        .collect();
    let mut out = String::with_capacity(spaced.len() + 8);
    for (i, &c) in spaced.iter().enumerate() {
        let boundary = i > 0
            && ((spaced[i - 1].is_lowercase() && c.is_uppercase())
                || (spaced[i - 1].is_alphabetic() && c.is_ascii_digit())
                || (spaced[i - 1].is_ascii_digit() && c.is_alphabetic()));
        if boundary {
            out.push(' ');
        }
        out.push(c);
    }
    let result = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

fn clean_label(label: Option<String>) -> Option<String> {
    let label = label?;
    let cleaned = label.trim().trim_end_matches(['.', ':']).trim();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned.to_string())
    }
}

fn fallback_label_for_type(kind: FieldKind, type_index: usize) -> String {
    let suffix = format!(" {type_index}");
    match kind {
        FieldKind::Checkbox => format!("Checkbox{suffix}"),
        FieldKind::Radio => format!("Option{suffix}"),
        FieldKind::Combobox => format!("Dropdown{suffix}"),
        FieldKind::Listbox => format!("List{suffix}"),
        FieldKind::Text => format!("Text field{suffix}"),
        _ => format!("Field{suffix}"),
    }
}

fn derive_display_label(
    doc: &Document,
    field: &TerminalField,
    name: &str,
    tooltip: &Option<String>,
    options: &[String],
    type_index: usize,
) -> String {
    let alternate = clean_label(dict_get(&field.chain[0], b"TU").and_then(|o| text_of(doc, o)));
    if let Some(a) = &alternate {
        if !looks_generic(a) {
            return a.clone();
        }
    }
    if let Some(t) = clean_label(tooltip.clone()) {
        if !looks_generic(&t) {
            return t;
        }
    }
    let humanized = clean_label(humanize_name(name));
    if let Some(h) = &humanized {
        if !looks_generic(h) {
            return h.clone();
        }
    }
    if field.kind.is_choice() && !options.is_empty() && looks_auto_generated(name) {
        if let Some(o) = clean_label(Some(options[0].clone())) {
            if !looks_generic(&o) {
                return o;
            }
        }
    }
    if let Some(h) = &humanized {
        if !looks_auto_generated(name) {
            return h.clone();
        }
    }
    fallback_label_for_type(field.kind, type_index)
}

// ---------- page index resolution ----------

/// `/Annots` reference entries mapped to their first page, matching
/// `buildAnnotationPageMap`'s `putIfAbsent` order. Java keys the map by
/// resolved `COSDictionary`; a reference id is equivalent because two
/// references can never resolve to different dictionaries.
fn annotation_page_map(doc: &Document, page_ids: &[ObjectId]) -> HashMap<ObjectId, usize> {
    let mut map = HashMap::new();
    for (idx, page_id) in page_ids.iter().enumerate() {
        let Ok(page) = doc.get_dictionary(*page_id) else {
            continue;
        };
        let Some(items) = dict_get(page, b"Annots")
            .and_then(|o| resolved(doc, o))
            .and_then(|o| match o {
                Object::Array(items) => Some(items.clone()),
                _ => None,
            })
        else {
            continue;
        };
        for item in &items {
            if let Ok(id) = item.as_reference() {
                map.entry(id).or_insert(idx);
            }
        }
    }
    map
}

/// Follows `/P` references to the final object id — `getDictionaryObject`
/// semantics where an inline dictionary can never be `==` to a page's
/// dictionary instance in Java.
fn page_ref_index(doc: &Document, page_ids: &[ObjectId], dict: &Dictionary) -> Option<usize> {
    let mut current = dict_get(dict, b"P")?.clone();
    let mut last_id = None;
    while let Object::Reference(id) = current {
        last_id = Some(id);
        current = doc.get_object(id).ok()?.clone();
    }
    if !matches!(current, Object::Dictionary(_)) {
        return None;
    }
    let id = last_id?;
    page_ids.iter().position(|pid| *pid == id)
}

/// `resolveWidgetPageIndex`: the prebuilt map, then the `/P`
/// back-reference. The annotation rescan is redundant — the map already
/// covers every `/Annots` reference, and an inline dictionary can never
/// share identity with a `/Kids` entry.
fn widget_page_index(
    doc: &Document,
    page_ids: &[ObjectId],
    annot_map: &HashMap<ObjectId, usize>,
    widget: &Widget,
) -> i64 {
    if let Ok(id) = widget.stored.as_reference() {
        if let Some(&idx) = annot_map.get(&id) {
            return idx as i64;
        }
    }
    page_ref_index(doc, page_ids, &widget.dict)
        .map(|i| i as i64)
        .unwrap_or(-1)
}

fn first_widget_page_index(
    doc: &Document,
    page_ids: &[ObjectId],
    annot_map: &HashMap<ObjectId, usize>,
    field: &TerminalField,
) -> i64 {
    field
        .widgets
        .iter()
        .map(|w| widget_page_index(doc, page_ids, annot_map, w))
        .find(|&idx| idx >= 0)
        .unwrap_or(-1)
}

// ---------- widget geometry ----------

fn rect_of(doc: &Document, dict: &Dictionary) -> Option<[f64; 4]> {
    let items = dict_get(dict, b"Rect")
        .and_then(|o| resolved(doc, o))
        .and_then(|o| match o {
            Object::Array(items) => Some(items.clone()),
            _ => None,
        })?;
    if items.len() < 4 {
        return None;
    }
    let mut r = [0.0; 4];
    for i in 0..4 {
        r[i] = num_of(doc, &items[i])?;
    }
    Some(r)
}

/// `/DA` font size: inheritable, must be a string, the number before `Tf`;
/// falls back to the AcroForm `/DA`.
fn extract_font_size(doc: &Document, field: &TerminalField) -> Option<f32> {
    let mut da = inheritable(field, b"DA")
        .and_then(|o| resolved(doc, o))
        .and_then(|o| match o {
            Object::String(..) => decode_text_string(o).ok(),
            _ => None,
        });
    if da.as_deref().map(str::trim).unwrap_or("").is_empty() {
        da = pdfdoc::acroform(doc)
            .and_then(|form| form.get(b"DA").ok().cloned())
            .and_then(|o| text_of(doc, &o));
    }
    let da = da?;
    let tokens: Vec<&str> = da.split_whitespace().collect();
    for (i, tok) in tokens.iter().enumerate() {
        if *tok == "Tf" && i > 0 {
            if let Ok(size) = tokens[i - 1].parse::<f32>() {
                return (size > 0.0).then_some(size);
            }
        }
    }
    None
}

/// Radio widgets get their `/Opt` index; other buttons fall back to the
/// first non-`Off` key of the widget's `/AP` `/N` subdictionary.
fn widget_export_value(doc: &Document, field: &TerminalField, index: usize) -> Option<String> {
    if field.kind == FieldKind::Radio {
        let exports = button_export_values(doc, field);
        if index < exports.len() {
            return Some(exports[index].clone());
        }
    }
    if matches!(
        field.kind,
        FieldKind::Checkbox | FieldKind::Radio | FieldKind::Button
    ) {
        let widget = field.widgets.get(index)?;
        let ap = dict_of(doc, widget.dict.get(b"AP").ok()?)?;
        let n = resolved(doc, ap.get(b"N").ok()?)?;
        if let Object::Dictionary(states) = n {
            for (key, _) in states.iter() {
                let name = String::from_utf8_lossy(key).into_owned();
                if name != "Off" {
                    return Some(name);
                }
            }
        }
    }
    None
}

/// Java `float` fields — coordinates serialize with f32 shortest-round-trip
/// formatting, matching Jackson's `Float` output.
#[derive(Serialize)]
struct WidgetCoords {
    #[serde(rename = "pageIndex")]
    page_index: i32,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    #[serde(rename = "exportValue", skip_serializing_if = "Option::is_none")]
    export_value: Option<String>,
    #[serde(rename = "fontSize", skip_serializing_if = "Option::is_none")]
    font_size: Option<f32>,
    #[serde(rename = "cropBoxHeight")]
    crop_box_height: f32,
}

/// `createWidgetCoordinates`: CropBox-relative, y flipped to the viewer's
/// top-left origin, finite-only. `export_value` is null on the
/// field-as-own-widget fallback.
fn make_widget_coords(
    doc: &Document,
    page_ids: &[ObjectId],
    page_index: i64,
    rect: [f64; 4],
    export_value: Option<String>,
    font_size: Option<f32>,
) -> Option<WidgetCoords> {
    if page_index < 0 || page_index as usize >= page_ids.len() {
        return None;
    }
    let crop = pdfdoc::form_view_box(doc, page_ids[page_index as usize]).ok()?;
    let crop_height = (crop[3] - crop[1]) as f32;
    let (llx, lly, urx, ury) = (
        rect[0] as f32,
        rect[1] as f32,
        rect[2] as f32,
        rect[3] as f32,
    );
    let width = urx - llx;
    let height = ury - lly;
    let rel_x = llx - crop[0] as f32;
    let rel_y = lly - crop[1] as f32;
    let final_y = crop_height - rel_y - height;
    if ![rel_x, final_y, width, height]
        .iter()
        .all(|v| v.is_finite())
    {
        return None;
    }
    Some(WidgetCoords {
        page_index: page_index as i32,
        x: rel_x,
        y: final_y,
        width,
        height,
        export_value,
        font_size,
        crop_box_height: crop_height,
    })
}

fn widget_coordinates(
    doc: &Document,
    page_ids: &[ObjectId],
    annot_map: &HashMap<ObjectId, usize>,
    field: &TerminalField,
) -> Vec<WidgetCoords> {
    let font_size = extract_font_size(doc, field);
    if field.widgets.is_empty() {
        // Fields with no resolvable widgets can still be their own
        // annotation: the field dictionary's /Rect is the geometry.
        if let Some(rect) = rect_of(doc, &field.chain[0]) {
            let widget = Widget {
                stored: field.stored.clone(),
                dict: field.chain[0].clone(),
            };
            let page_index = widget_page_index(doc, page_ids, annot_map, &widget);
            if let Some(coords) =
                make_widget_coords(doc, page_ids, page_index, rect, None, font_size)
            {
                return vec![coords];
            }
        }
        return Vec::new();
    }
    let mut out = Vec::new();
    for (i, widget) in field.widgets.iter().enumerate() {
        let Some(rect) = rect_of(doc, &widget.dict) else {
            continue;
        };
        let page_index = widget_page_index(doc, page_ids, annot_map, widget);
        let export_value = widget_export_value(doc, field, i);
        if let Some(coords) =
            make_widget_coords(doc, page_ids, page_index, rect, export_value, font_size)
        {
            out.push(coords);
        }
    }
    out
}

fn extract_max_length(doc: &Document, field: &TerminalField) -> Option<i64> {
    if field.kind != FieldKind::Text {
        return None;
    }
    let n = int_of(doc, dict_get(&field.chain[0], b"MaxLen")?)?;
    (n > 0).then_some(n)
}

/// `extractButtonAction`: the first widget `/A` dictionary decides;
/// unmatched subtypes keep scanning, a `Named` action that is not Print
/// ends the search with null.
fn extract_button_action(doc: &Document, field: &TerminalField) -> Option<String> {
    if field.kind != FieldKind::Button {
        return None;
    }
    for widget in &field.widgets {
        let Some(a) = widget.dict.get(b"A").ok().and_then(|o| dict_of(doc, o)) else {
            continue;
        };
        let Some(subtype) = a.get(b"S").ok().and_then(|o| name_of(doc, o)) else {
            continue;
        };
        match subtype.as_str() {
            "ResetForm" => return Some("reset".to_string()),
            "Named" => {
                let n = a.get(b"N").ok().and_then(|o| name_of(doc, o));
                return n
                    .filter(|n| n.eq_ignore_ascii_case("Print"))
                    .map(|_| "print".to_string());
            }
            "URI" => {
                let uri = a
                    .get(b"URI")
                    .ok()
                    .and_then(|o| text_of(doc, o))
                    .unwrap_or_default();
                return Some(format!("uri:{uri}"));
            }
            "SubmitForm" => {
                let f = a
                    .get(b"F")
                    .ok()
                    .and_then(|o| text_of(doc, o))
                    .unwrap_or_default();
                return Some(format!("submit:{f}"));
            }
            _ => {}
        }
    }
    None
}

// ---------- response shapes ----------

/// `FormFieldInfo` — NON_NULL members only.
#[derive(Serialize)]
struct FormFieldInfo {
    name: String,
    label: String,
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<Vec<String>>,
    required: bool,
    #[serde(rename = "pageIndex")]
    page_index: i64,
    #[serde(rename = "multiSelect")]
    multi_select: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tooltip: Option<String>,
    #[serde(rename = "pageOrder")]
    page_order: i64,
}

/// `FormFieldWithCoordinates` — NON_NULL members only.
#[derive(Serialize)]
struct FormFieldWithCoordinates {
    name: String,
    label: String,
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<Vec<String>>,
    #[serde(rename = "displayOptions", skip_serializing_if = "Option::is_none")]
    display_options: Option<Vec<String>>,
    required: bool,
    #[serde(rename = "readOnly")]
    read_only: bool,
    #[serde(rename = "multiSelect")]
    multi_select: bool,
    multiline: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tooltip: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    widgets: Vec<WidgetCoords>,
    #[serde(rename = "maxLength", skip_serializing_if = "Option::is_none")]
    max_length: Option<i64>,
    #[serde(rename = "buttonActionSpec", skip_serializing_if = "Option::is_none")]
    button_action_spec: Option<String>,
}

struct FieldEntry {
    info: FormFieldInfo,
    coords: FormFieldWithCoordinates,
    /// `(qualified name, kind, value, multi_select)` for the fill template.
    template_row: (String, FieldKind, Option<String>, bool),
}

/// Shared extraction behind `/fields`, `/fields-with-coordinates` and
/// `/extract-csv`. `sort_fields_order` applies `extractFormFields`'s
/// pageIndex/pageOrder/name ordering; the coordinates endpoint instead
/// keeps tree order for its own comparator to break ties against.
fn extract_entries(doc: &Document, sort_fields_order: bool) -> AppResult<Vec<FieldEntry>> {
    let fields = terminal_fields(doc)?;
    let page_ids = pdfdoc::page_ids(doc);
    let annot_map = annotation_page_map(doc, &page_ids);

    let mut type_counters: HashMap<&'static str, usize> = HashMap::new();
    let mut page_order_counters: HashMap<i64, usize> = HashMap::new();
    let mut entries = Vec::new();

    for field in &fields {
        if field.qualified.trim().is_empty() {
            continue;
        }
        let kind_name = field.kind.name();
        let type_index = {
            let c = type_counters.entry(kind_name).or_insert(0);
            *c += 1;
            *c
        };
        let value = safe_field_value(doc, field);
        let required = ff_flag(field.ff, FF_REQUIRED);
        let page_index = first_widget_page_index(doc, &page_ids, &annot_map, field);
        let options = resolve_options(doc, field);
        let display_options = resolve_display_options(doc, field);
        let display_opts = if !display_options.is_empty() && display_options != options {
            Some(display_options)
        } else {
            None
        };
        let tooltip = resolve_tooltip(doc, field);
        let label =
            derive_display_label(doc, field, &field.qualified, &tooltip, &options, type_index);
        let multi_select = field.kind == FieldKind::Listbox && ff_flag(field.ff, FF_MULTI_SELECT);
        let page_order = {
            let c = page_order_counters.entry(page_index).or_insert(0);
            let v = *c as i64;
            *c += 1;
            v
        };

        let info = FormFieldInfo {
            name: field.qualified.clone(),
            label: label.clone(),
            kind: kind_name,
            value: value.clone(),
            options: if options.is_empty() {
                None
            } else {
                Some(options.clone())
            },
            required,
            page_index,
            multi_select,
            tooltip: tooltip.clone(),
            page_order,
        };
        let coords = FormFieldWithCoordinates {
            name: field.qualified.clone(),
            label,
            kind: kind_name,
            value: value.clone(),
            options: if options.is_empty() {
                None
            } else {
                Some(options)
            },
            display_options: display_opts,
            required,
            read_only: ff_flag(field.ff, FF_READ_ONLY),
            multi_select,
            multiline: field.kind == FieldKind::Text && ff_flag(field.ff, FF_MULTILINE),
            tooltip,
            widgets: widget_coordinates(doc, &page_ids, &annot_map, field),
            max_length: extract_max_length(doc, field),
            button_action_spec: extract_button_action(doc, field),
        };
        entries.push(FieldEntry {
            info,
            coords,
            template_row: (field.qualified.clone(), field.kind, value, multi_select),
        });
    }

    if sort_fields_order {
        // extractFormFields ordering: pageIndex, pageOrder, name
        // (compareToIgnoreCase ≈ lowercase compare).
        entries.sort_by(|a, b| {
            a.info
                .page_index
                .cmp(&b.info.page_index)
                .then(a.info.page_order.cmp(&b.info.page_order))
                .then_with(|| a.info.name.to_lowercase().cmp(&b.info.name.to_lowercase()))
        });
    }
    Ok(entries)
}

/// `FieldCoordinateComparator`: first-widget page, then a 10pt
/// same-line y-band falling back to x, then y. Stable, so tree order
/// breaks remaining ties — matching Java's `List.sort`.
fn sort_coords_entries(entries: &mut [FieldEntry]) {
    fn first_geom(entry: &FieldEntry) -> (i32, f32, f32) {
        entry
            .coords
            .widgets
            .first()
            .map(|w| (w.page_index, w.x, w.y))
            .unwrap_or((-1, 0.0, 0.0))
    }
    entries.sort_by(|a, b| {
        let (pa, xa, ya) = first_geom(a);
        let (pb, xb, yb) = first_geom(b);
        pa.cmp(&pb).then_with(|| {
            if (ya - yb).abs() < SAME_LINE_THRESHOLD_PT {
                xa.partial_cmp(&xb).unwrap_or(std::cmp::Ordering::Equal)
            } else {
                ya.partial_cmp(&yb).unwrap_or(std::cmp::Ordering::Equal)
            }
        })
    });
}

/// `buildFillTemplateRecord`: checkbox → bool, listbox → array-or-string,
/// button/signature skipped, everything else the value or "".
fn fill_template(rows: &[(String, FieldKind, Option<String>, bool)]) -> Value {
    let mut map = Map::new();
    for (name, kind, value, multi_select) in rows {
        let entry = match kind {
            FieldKind::Checkbox => Some(json!(is_checked(value.as_deref()))),
            FieldKind::Listbox => {
                if *multi_select {
                    Some(json!([]))
                } else {
                    Some(json!(value.clone().unwrap_or_default()))
                }
            }
            FieldKind::Button | FieldKind::Signature => None,
            _ => Some(json!(value.clone().unwrap_or_default())),
        };
        if let Some(v) = entry {
            map.insert(name.clone(), v);
        }
    }
    Value::Object(map)
}

fn is_checked(value: Option<&str>) -> bool {
    let Some(v) = value else { return false };
    matches!(
        v.trim().to_lowercase().as_str(),
        "true" | "1" | "yes" | "on" | "checked"
    )
}

// ---------- endpoints ----------

struct FormUpload {
    file: Upload,
    /// `data`/`names`/`fields` file parts — `data` is applied as fill
    /// values on the extract endpoints; `names` drives `delete-fields`.
    data: Option<Upload>,
    names: Option<Upload>,
    /// `fields`/`edits`/`updates` JSON payloads for the mutation
    /// endpoints; text parts of the same names arrive via `params`.
    fields: Option<Upload>,
    /// Non-file text parts (`flatten`, or JSON sent without a filename).
    params: Vec<(String, String)>,
}

/// The form endpoints bind `@RequestParam("file")`, not the usual
/// `fileInput`; `data` is an optional file part on the extract endpoints.
async fn receive_form_multipart(multipart: Multipart) -> AppResult<FormUpload> {
    let mut file = None;
    let mut data = None;
    let mut names = None;
    let mut fields = None;
    let mut params = Vec::new();
    let mut multipart = multipart;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(format!("multipart error: {e}")))?
    {
        let name = field.name().unwrap_or_default().to_string();
        let is_file_part = field.file_name().is_some();
        match (name.as_str(), is_file_part) {
            ("file", true) => file = Some(Upload::from_field(field).await?),
            ("data", true) => data = Some(Upload::from_field(field).await?),
            ("names", true) => names = Some(Upload::from_field(field).await?),
            ("fields", true) => fields = Some(Upload::from_field(field).await?),
            _ => {
                // Every non-file part is a candidate `@RequestParam`
                // (`flatten`, `includeFields`, `async`, ...) — keep the
                // text so `param_bool` can see it.
                let text = field
                    .text()
                    .await
                    .map_err(|e| AppError::BadRequest(format!("failed to read {name}: {e}")))?;
                params.push((name, text));
            }
        }
    }
    let Some(file) = file else {
        return Err(AppError::BadRequest("file must be in PDF format".into()));
    };
    if file.bytes.is_empty() {
        return Err(AppError::BadRequest("file must be in PDF format".into()));
    }
    Ok(FormUpload {
        file,
        data,
        names,
        fields,
        params,
    })
}

/// `#[endpoint]` receiver for the form routes: the PDF arrives as `file`
/// (not `fileInput`) and the optional `data`/`names`/`fields` file parts
/// travel as named extras so the work fns keep the standard signature.
async fn receive_form_parts(
    multipart: Multipart,
) -> AppResult<(Vec<Upload>, Vec<(String, Upload)>, Vec<(String, String)>)> {
    let FormUpload {
        file,
        data,
        names,
        fields,
        params,
    } = receive_form_multipart(multipart).await?;
    let mut extras = Vec::new();
    if let Some(u) = data {
        extras.push(("data".to_string(), u));
    }
    if let Some(u) = names {
        extras.push(("names".to_string(), u));
    }
    if let Some(u) = fields {
        extras.push(("fields".to_string(), u));
    }
    Ok((vec![file], extras, params))
}

/// The `data`/`names`/`fields` extra part by field name, if the request
/// carried it as a file upload.
fn extra<'a>(extras: &'a [(String, Upload)], field: &str) -> Option<&'a Upload> {
    extras
        .iter()
        .find(|(name, _)| name == field)
        .map(|(_, u)| u)
}

fn load_document(upload: &Upload) -> AppResult<Document> {
    pdfdoc::load(&upload.name, &upload.bytes)
}

/// `POST /api/v1/form/fields` → `{fields, template}`.
#[stirling_macros::endpoint(receive = "receive_form_parts")]
fn list_fields(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    _extras: Vec<(String, Upload)>,
    _params: Vec<(String, String)>,
) -> AppResult<Response> {
    let upload = uploads.into_iter().next().unwrap();
    let document = load_document(&upload)?;
    let entries = extract_entries(&document, true)?;
    let fields: Vec<Value> = entries
        .iter()
        .map(|e| serde_json::to_value(&e.info).unwrap_or(Value::Null))
        .collect();
    let rows: Vec<_> = entries.iter().map(|e| e.template_row.clone()).collect();
    Ok(Json(json!({
        "fields": fields,
        "template": fill_template(&rows),
    }))
    .into_response())
}

/// `POST /api/v1/form/fields-with-coordinates` → field list with widget
/// geometry in the embedpdf viewer's coordinate space.
#[stirling_macros::endpoint(receive = "receive_form_parts")]
fn list_fields_with_coordinates(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    _extras: Vec<(String, Upload)>,
    _params: Vec<(String, String)>,
) -> AppResult<Response> {
    let upload = uploads.into_iter().next().unwrap();
    let document = load_document(&upload)?;
    let mut entries = extract_entries(&document, false)?;
    sort_coords_entries(&mut entries);
    let json: Vec<Value> = entries
        .iter()
        .map(|e| serde_json::to_value(&e.coords).unwrap_or(Value::Null))
        .collect();
    Ok(Json(json).into_response())
}

/// `POST /api/v1/form/extract-csv` → `name_extracted.csv`. A `data` part
/// carries `Map<String,String>` fill values applied before extraction,
/// matching the controller's pre-pass.
#[stirling_macros::endpoint(receive = "receive_form_parts")]
fn extract_csv(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    extras: Vec<(String, Upload)>,
    _params: Vec<(String, String)>,
) -> AppResult<Response> {
    let data = extra(&extras, "data");
    let mut upload = uploads.into_iter().next().unwrap();
    let mut document = load_document(&upload)?;
    apply_data_values(&mut document, data, false)?;
    let entries = extract_entries(&document, true)?;
    upload.name = csv_name(&upload.name);
    Ok(file_response(csv_bytes(&entries), &upload.name, "text/csv"))
}

/// `POST /api/v1/form/extract-xlsx` → `name_extracted.xlsx`, a minimal
/// OOXML workbook equivalent to POI's two-column "Form Fields" sheet.
#[stirling_macros::endpoint(receive = "receive_form_parts")]
fn extract_xlsx(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    extras: Vec<(String, Upload)>,
    _params: Vec<(String, String)>,
) -> AppResult<Response> {
    let data = extra(&extras, "data");
    let mut upload = uploads.into_iter().next().unwrap();
    let mut document = load_document(&upload)?;
    apply_data_values(&mut document, data, false)?;
    let entries = extract_entries(&document, true)?;
    upload.name = xlsx_name(&upload.name);
    Ok(file_response(
        xlsx_bytes(&entries),
        &upload.name,
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    ))
}

fn xlsx_name(original: &str) -> String {
    let base = original
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("document");
    let stem = if base.to_lowercase().ends_with(".pdf") {
        &base[..base.len() - 4]
    } else {
        base
    };
    format!("{stem}_extracted.xlsx")
}

fn csv_name(original: &str) -> String {
    let base = original
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("document");
    let stem = if base.to_lowercase().ends_with(".pdf") {
        &base[..base.len() - 4]
    } else {
        base
    };
    format!("{stem}_extracted.csv")
}

fn sanitize_cell(value: Option<&str>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    if value.is_empty() {
        return String::new();
    }
    let candidate = value.trim_start();
    if candidate.is_empty() || !"=+-@".contains(candidate.chars().next().unwrap()) {
        return value.to_string();
    }
    if is_plain_number(candidate) {
        return value.to_string();
    }
    format!("'{value}")
}

/// `[-+]?[0-9]+(?:[.,][0-9]+)*` — sign, digits, then digit groups joined
/// by `.` or `,`.
fn is_plain_number(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let digits_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == digits_start {
        return false;
    }
    while i < b.len() {
        if !(b[i] == b'.' || b[i] == b',') {
            return false;
        }
        i += 1;
        let group_start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == group_start {
            return false;
        }
    }
    true
}

fn csv_quote(cell: &str) -> String {
    if cell.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", cell.replace('"', "\"\""))
    } else {
        cell.to_string()
    }
}

/// OpenCSV `CSVWriter` defaults: `,` separator, minimal quoting, `\n` rows.
fn csv_bytes(entries: &[FieldEntry]) -> Vec<u8> {
    let mut out = String::from("Field Name,Value\n");
    for entry in entries {
        let (name, _, value, _) = &entry.template_row;
        out.push_str(&csv_quote(&sanitize_cell(Some(name))));
        out.push(',');
        out.push_str(&csv_quote(&sanitize_cell(value.as_deref())));
        out.push('\n');
    }
    out.into_bytes()
}

// ---------- mutation: fill / delete ----------

fn pdf_text(value: &str) -> Object {
    Object::string_literal(value.as_bytes().to_vec())
}

fn pdf_name(value: &str) -> Object {
    Object::Name(value.as_bytes().to_vec())
}

/// First non-`Off` key of a widget's `/AP` `/N` subdictionary — the state
/// name `PDAnnotationWidget.getAppearanceState`/export-value fallbacks use.
fn widget_on_state(doc: &Document, widget: &Widget) -> Option<String> {
    let ap = dict_of(doc, widget.dict.get(b"AP").ok()?)?;
    let n = resolved(doc, ap.get(b"N").ok()?)?;
    if let Object::Dictionary(states) = n {
        for (key, _) in states.iter() {
            let name = String::from_utf8_lossy(key).into_owned();
            if name != "Off" {
                return Some(name);
            }
        }
    }
    None
}

/// `collectCheckBoxStates`: on-states from every widget's normal
/// appearance plus the inheritable `/Opt` exports, deduplicated.
fn checkbox_states(doc: &Document, field: &TerminalField) -> Vec<String> {
    let mut states: Vec<String> = Vec::new();
    for widget in &field.widgets {
        if let Some(state) = widget_on_state(doc, widget) {
            let t = state.trim();
            if !t.is_empty() && !t.eq_ignore_ascii_case("off") && !states.iter().any(|s| s == t) {
                states.push(t.to_string());
            }
        }
    }
    for export in button_export_values(doc, field) {
        let t = export.trim();
        if !t.is_empty() && !t.eq_ignore_ascii_case("off") && !states.iter().any(|s| s == t) {
            states.push(t.to_string());
        }
    }
    states
}

fn set_field_value(doc: &mut Document, field: &TerminalField, value: Object) {
    let Some(id) = field.stored.as_reference().ok() else {
        return;
    };
    if let Ok(dict) = doc.get_dictionary_mut(id) {
        dict.set("V", value);
    }
}

fn set_widget_state(doc: &mut Document, widget: &Widget, state: &str) {
    let Ok(id) = widget.stored.as_reference() else {
        return;
    };
    if let Ok(dict) = doc.get_dictionary_mut(id) {
        dict.set("AS", pdf_name(state));
    }
}

/// `shouldCheckBoxBeChecked`: truthy literals or a case-insensitive match
/// against a candidate on-state.
fn should_check(value: Option<&str>, states: &[String]) -> bool {
    let Some(raw) = value else { return false };
    if is_checked(Some(raw)) {
        return true;
    }
    let normalized = raw.trim();
    if normalized.is_empty() || normalized.eq_ignore_ascii_case("off") {
        return false;
    }
    states.iter().any(|s| s.eq_ignore_ascii_case(normalized))
}

/// `filterChoiceSelections`: selections must match an `/Opt` value
/// case-insensitively; the matched *allowed* spelling is what `/V` stores.
fn filter_choice_selections(
    selections: &[String],
    allowed: &[String],
) -> Result<Vec<String>, String> {
    let clean: Vec<String> = selections
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if clean.is_empty() {
        return Ok(Vec::new());
    }
    if allowed.is_empty() {
        return Err("the /Opt array is missing".to_string());
    }
    let mut out = Vec::new();
    for sel in clean {
        if let Some(m) = allowed.iter().find(|a| a.eq_ignore_ascii_case(&sel)) {
            if !out.contains(m) {
                out.push(m.clone());
            }
        }
    }
    Ok(out)
}

fn apply_value_to_field(
    doc: &mut Document,
    field: &TerminalField,
    value: Option<&str>,
    strict: bool,
) -> AppResult<()> {
    let result = (|| -> Result<(), String> {
        match field.kind {
            FieldKind::Text => {
                set_field_value(doc, field, pdf_text(value.unwrap_or("")));
            }
            FieldKind::Checkbox => {
                let states = checkbox_states(doc, field);
                if should_check(value, &states) {
                    let on = states.first().cloned().unwrap_or_else(|| "Yes".into());
                    set_field_value(doc, field, pdf_name(&on));
                    for widget in &field.widgets {
                        let state = widget_on_state(doc, widget).unwrap_or_else(|| on.clone());
                        set_widget_state(doc, widget, &state);
                    }
                } else {
                    set_field_value(doc, field, pdf_name("Off"));
                    for widget in &field.widgets {
                        set_widget_state(doc, widget, "Off");
                    }
                }
            }
            FieldKind::Radio => {
                if let Some(v) = value.filter(|v| !v.trim().is_empty()) {
                    let exports = button_export_values(doc, field);
                    let matched = exports
                        .iter()
                        .find(|e| e == &&v.trim().to_string())
                        .cloned()
                        .unwrap_or_else(|| v.trim().to_string());
                    set_field_value(doc, field, pdf_name(&matched));
                    for (i, widget) in field.widgets.iter().enumerate() {
                        let on = widget_on_state(doc, widget).or_else(|| exports.get(i).cloned());
                        let export = exports.get(i).cloned().or_else(|| on.clone());
                        let state = match (on, export) {
                            (Some(on_state), Some(exp))
                                if exp == matched || on_state == matched =>
                            {
                                on_state
                            }
                            (Some(on_state), None) if on_state == matched => on_state,
                            _ => "Off".to_string(),
                        };
                        set_widget_state(doc, widget, &state);
                    }
                }
            }
            FieldKind::Combobox | FieldKind::Listbox => {
                let multi = field.kind == FieldKind::Listbox && ff_flag(field.ff, FF_MULTI_SELECT);
                let allowed = resolve_options(doc, field);
                match value {
                    None => {
                        set_field_value(doc, field, pdf_text(""));
                    }
                    Some(raw) => {
                        let selections: Vec<String> = if multi {
                            raw.split(',').map(|s| s.trim().to_string()).collect()
                        } else {
                            vec![raw.trim().to_string()]
                        };
                        let matched = filter_choice_selections(&selections, &allowed)
                            .map_err(|e| format!("field '{}': {e}", field.qualified))?;
                        if multi {
                            set_field_value(
                                doc,
                                field,
                                Object::Array(matched.iter().map(|s| pdf_text(s)).collect()),
                            );
                        } else {
                            set_field_value(
                                doc,
                                field,
                                pdf_text(matched.first().map_or("", String::as_str)),
                            );
                        }
                    }
                }
            }
            FieldKind::Button | FieldKind::Signature => {}
        }
        Ok(())
    })();
    match result {
        Ok(()) => Ok(()),
        Err(e) => {
            if strict {
                Err(AppError::BadRequest(format!(
                    "Failed to set value for field '{}': {e}",
                    field.qualified
                )))
            } else {
                Ok(())
            }
        }
    }
}

/// `applyFieldValues`: resolve each key against qualified then partial
/// names, apply per field kind, then flag `/NeedAppearances` so viewers
/// regenerate widget appearances — lopdf cannot run PDFBox's
/// `AppearanceGeneratorHelper`, so the flag stays set rather than
/// pretending appearances were rewritten.
fn apply_field_values(
    doc: &mut Document,
    values: &[(String, Option<String>)],
    flatten: bool,
    strict: bool,
) -> AppResult<()> {
    // Java's strict path (the /fill endpoint) throws on a missing
    // AcroForm before reading values; the non-strict path (CSV/XLSX data
    // pre-pass) logs and returns — and flattenEntireDocument no-ops
    // without an AcroForm, so neither path flattens here.
    if pdfdoc::acroform(doc).is_none() {
        return if strict {
            Err(AppError::Internal(anyhow::anyhow!(
                "No AcroForm present in document"
            )))
        } else {
            Ok(())
        };
    }
    let fields = terminal_fields(doc)?;
    let mut applied = false;
    for (key, value) in values {
        if key.trim().is_empty() {
            continue;
        }
        // Java's lookup prefers the fully-qualified name, then the
        // first field whose partial (/T) name matches.
        let partial = key.rsplit('.').next().unwrap_or(key);
        let field = fields.iter().find(|f| f.qualified == *key).or_else(|| {
            fields
                .iter()
                .find(|f| f.qualified.rsplit('.').next() == Some(partial))
        });
        let Some(field) = field else {
            continue;
        };
        apply_value_to_field(doc, field, value.as_deref(), strict)?;
        applied = true;
    }
    if applied {
        refresh_appearances(doc);
    }
    repair_widget_geometry(doc);
    if flatten {
        crate::forms::flatten(doc)?;
    }
    Ok(())
}

/// `/AcroForm /NeedAppearances` — same two-shape handling as
/// `unlock_pdf_forms`: indirect dict mutated in place, inline dict
/// rewritten through the catalog.
fn set_need_appearances(doc: &mut Document) {
    let Some(acro_slot) = doc
        .catalog()
        .ok()
        .and_then(|c| c.get(b"AcroForm").ok().cloned())
    else {
        return;
    };
    match &acro_slot {
        Object::Reference(id) => {
            if let Ok(dict) = doc.get_dictionary_mut(*id) {
                dict.set("NeedAppearances", Object::Boolean(true));
            }
        }
        Object::Dictionary(dict) => {
            let mut dict = dict.clone();
            dict.set("NeedAppearances", Object::Boolean(true));
            if let Ok(catalog_id) = doc.trailer.get(b"Root").and_then(|o| o.as_reference()) {
                if let Ok(catalog) = doc.get_dictionary_mut(catalog_id) {
                    catalog.set("AcroForm", Object::Dictionary(dict));
                }
            }
        }
        _ => {}
    }
}

// ---------- appearance regeneration ----------
//
// PDFBox's `ensureAppearances` runs `refreshAppearances()` over the whole
// field tree, then clears `/NeedAppearances` when every widget got an /AP.
// lopdf cannot typeset, but the Standard-14 metrics in `fontmetrics` cover
// the fonts form `/DA` strings actually name, so single-line text, toggle
// buttons and pushbuttons get real appearance streams here. Anything else
// (listboxes, multiline, comb) keeps the viewer-regeneration flag.

/// `/DA` resolved own-then-ancestor, then the AcroForm default, then
/// PDFBox's implicit `/Helv 0 Tf 0 g`.
fn field_da(doc: &Document, field: &TerminalField) -> String {
    inheritable(field, b"DA")
        .and_then(|o| text_of(doc, o))
        .or_else(|| acroform_da(doc))
        .unwrap_or_else(|| "/Helv 0 Tf 0 g".to_string())
}

fn acroform_da(doc: &Document) -> Option<String> {
    pdfdoc::acroform(doc)
        .and_then(|form| form.get(b"DA").ok().cloned())
        .and_then(|o| resolved(doc, &o).cloned())
        .and_then(|o| decode_text_string(&o).ok())
}

struct DaStyle {
    font_res: String,
    size: f64,
    rgb: [f64; 3],
    quadding: i64,
}

/// Token scan of a `/DA` content string: `/Name size Tf` fixes the font,
/// the last `g`/`rg` operator before it fixes the nonstroking colour.
fn parse_da(da: &str, quadding: i64) -> DaStyle {
    let tokens: Vec<&str> = da.split_whitespace().collect();
    let mut font_res = "Helv".to_string();
    let mut size = 0.0;
    let mut rgb = [0.0, 0.0, 0.0];
    let mut pending: Option<(String, f64)> = None;
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i];
        if t == "Tf" {
            if let Some((name, sz)) = pending.take() {
                font_res = name;
                size = sz;
            }
        } else if t == "g" && i >= 1 {
            if let Ok(v) = tokens[i - 1].parse::<f64>() {
                rgb = [v, v, v];
            }
        } else if t == "rg" && i >= 3 {
            let p = |k: usize| tokens[i - k].parse::<f64>().ok();
            if let (Some(r), Some(g), Some(b)) = (p(3), p(2), p(1)) {
                rgb = [r, g, b];
            }
        } else if let Some(name) = t.strip_prefix('/') {
            if i + 2 < tokens.len() && tokens[i + 2] == "Tf" {
                if let Ok(sz) = tokens[i + 1].parse::<f64>() {
                    pending = Some((name.to_string(), sz));
                }
            }
        }
        i += 1;
    }
    DaStyle {
        font_res,
        size,
        rgb,
        quadding,
    }
}

/// `/DR /Font /<res>` → the BaseFont name, defaulting to Helvetica for
/// the `/Helv` alias every AcroForm we create installs.
fn dr_font_base(doc: &Document, font_res: &str) -> String {
    let name = pdfdoc::acroform(doc)
        .and_then(|form| form.get(b"DR").ok().cloned())
        .and_then(|o| resolved(doc, &o).cloned())
        .and_then(|o| dict_of(doc, &o).and_then(|d| d.get(b"Font").ok().cloned()))
        .and_then(|o| resolved(doc, &o).cloned())
        .and_then(|o| dict_of(doc, &o))
        .and_then(|fonts| fonts.get(font_res.as_bytes()).ok().cloned())
        .and_then(|o| resolved(doc, &o).cloned())
        .and_then(|o| dict_of(doc, &o))
        .and_then(|font| font.get(b"BaseFont").ok().cloned())
        .and_then(|o| name_of(doc, &o))
        .unwrap_or_else(|| "Helvetica".to_string());
    // fontmetrics keys on the AFM family names.
    let lower = name.to_lowercase();
    if lower.contains("times") {
        "Times-Roman".to_string()
    } else if lower.contains("courier") {
        "Courier".to_string()
    } else {
        "Helvetica".to_string()
    }
}

/// `/MK` on the widget dictionary (merged field-widget: the field dict).
fn widget_mk(doc: &Document, widget: &Widget) -> Option<Dictionary> {
    dict_of(doc, &widget.stored)
        .and_then(|d| d.get(b"MK").ok().cloned())
        .and_then(|o| resolved(doc, &o).cloned())
        .and_then(|o| dict_of(doc, &o))
}

fn mk_color(doc: &Document, mk: Option<&Dictionary>, key: &[u8]) -> Option<[f64; 3]> {
    let arr = mk?.get(key).ok()?;
    let arr = resolved(doc, arr)?;
    let items = arr.as_array().ok()?;
    let comps: Vec<f64> = items.iter().filter_map(|o| num_of(doc, o)).collect();
    match comps.len() {
        1 => Some([comps[0], comps[0], comps[0]]),
        3 => Some([comps[0], comps[1], comps[2]]),
        _ => None,
    }
}

fn mk_caption(doc: &Document, mk: Option<&Dictionary>) -> Option<String> {
    let ca = mk?.get(b"CA").ok()?;
    text_of(doc, ca).filter(|s| !s.trim().is_empty())
}

fn font_resource_dict(base_font: &str) -> Dictionary {
    let base = match base_font {
        "Times-Roman" => "Times-Roman",
        "Courier" => "Courier",
        _ => "Helvetica",
    };
    dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => base,
        "Encoding" => "WinAnsiEncoding",
    }
}

fn ap_stream(
    doc: &mut Document,
    w: f64,
    h: f64,
    font_res: &str,
    base_font: &str,
    content: Vec<u8>,
) -> ObjectId {
    doc.add_object(lopdf::Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Form",
            "BBox" => Object::Array(vec![
                pdf_num(0.0), pdf_num(0.0), pdf_num(w), pdf_num(h),
            ]),
            "Resources" => dictionary! {
                "Font" => dictionary! {
                    font_res => Object::Dictionary(font_resource_dict(base_font)),
                },
            },
        },
        content,
    ))
}

fn rgb_ops(rgb: [f64; 3]) -> String {
    format!("{} {} {} rg\n", rgb[0], rgb[1], rgb[2])
}

fn rgb_vals(rgb: [f64; 3]) -> String {
    format!("{} {} {}", rgb[0], rgb[1], rgb[2])
}

/// Single-line text appearance: /MK background+border, /DA font size and
/// colour, /Q quadding measured with the Standard-14 metrics. Returns
/// None for field kinds this cannot draw faithfully.
fn text_ap_stream(
    doc: &mut Document,
    field: &TerminalField,
    widget: &Widget,
    rect: [f64; 4],
) -> Option<ObjectId> {
    if field.ff & (FF_MULTILINE | FF_COMB) != 0 {
        return None;
    }
    let (w, h) = (rect[2], rect[3]);
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let da = parse_da(&field_da(doc, field), {
        field
            .chain
            .iter()
            .find_map(|d| dict_get(d, b"Q").and_then(|o| int_of(doc, o)))
            .unwrap_or(0)
    });
    let base_font = dr_font_base(doc, &da.font_res);
    // DA size 0 means "auto": PDFBox fits the border box height.
    let size = if da.size > 0.0 {
        da.size
    } else {
        (h - 4.0).max(4.0)
    };
    let mk = widget_mk(doc, widget);
    let text = safe_field_value(doc, field).unwrap_or_default();
    let encoded = super::misc::winansi_safe(&text);
    let scale = size / 1000.0;
    let text_w = crate::fontmetrics::string_width(&base_font, &encoded) * scale / 1000.0;

    let mut content = String::new();
    if let Some(bg) = mk_color(doc, mk.as_ref(), b"BG") {
        content.push_str(&format!("{} 0 0 {w} {h} re f\n", rgb_ops(bg).trim_end()));
    }
    let bs_w = dict_of(doc, &widget.stored)
        .and_then(|d| d.get(b"BS").ok().cloned())
        .and_then(|o| resolved(doc, &o).cloned())
        .and_then(|o| dict_of(doc, &o))
        .and_then(|bs| bs.get(b"W").ok().cloned())
        .and_then(|o| num_of(doc, &o));
    let border_rgb = mk_color(doc, mk.as_ref(), b"BC");
    let border_w = match (border_rgb, bs_w) {
        (Some(_), Some(w)) => w.max(0.5),
        (Some(_), None) => 1.0,
        (None, Some(w)) if w > 0.0 => w,
        _ => 0.0,
    };
    if border_w > 0.0 {
        let rgb = border_rgb.unwrap_or([0.0, 0.0, 0.0]);
        content.push_str(&format!(
            "{} w {} 0 0 {w} {h} re S\n",
            border_w,
            rgb_ops(rgb).trim_end().replacen("rg", "RG", 1)
        ));
    }
    content.push_str(&format!("0 0 {w} {h} re W n\n"));

    let pad = if border_w > 0.0 { border_w + 1.0 } else { 2.0 };
    let inner_w = (w - 2.0 * pad).max(0.0);
    let tx = match da.quadding {
        1 => pad + ((inner_w - text_w) / 2.0).max(0.0),
        2 => (w - pad - text_w).max(pad),
        _ => pad,
    };
    let (asc, desc) = crate::fontmetrics::ascent_descent(&base_font);
    let em = (asc - desc) * scale;
    let inner_h = (h - 2.0 * pad).max(0.0);
    let baseline = pad + ((inner_h - em) / 2.0).max(0.0) + (-desc) * scale;

    let mut bytes = content.into_bytes();
    bytes.extend_from_slice(
        format!(
            "BT /{} {} Tf {} rg\n{} {} Td (",
            da.font_res,
            size,
            rgb_vals(da.rgb),
            tx,
            baseline
        )
        .as_bytes(),
    );
    // The literal is byte-oriented: emit the WinAnsi bytes directly.
    super::misc::push_pdf_literal(&mut bytes, &encoded);
    bytes.extend_from_slice(b") Tj ET\nQ\n");
    Some(ap_stream(doc, w, h, &da.font_res, &base_font, bytes))
}

/// `toggleStream` geometry: 10% inset, stroked frame/check or radio
/// circles; the on-state fills the check or the inner dot.
fn toggle_ap_stream(doc: &mut Document, w: f64, h: f64, on: bool, is_radio: bool) -> ObjectId {
    let inset = w.min(h) * 0.1;
    let line_w = (w.min(h) * 0.06).max(0.5);
    let mut content = format!("0 0 0 RG {line_w} w\n");
    if is_radio {
        let (cx, cy) = (w / 2.0, h / 2.0);
        let r = w.min(h) / 2.0 - inset;
        content.push_str(&format!("{cx} {cy} {r} 0 360 arc S\n"));
        if on {
            let r2 = w.min(h) / 4.0 - inset / 2.0;
            content.push_str(&format!("0 0 0 rg {cx} {cy} {r2} 0 360 arc f\n"));
        }
    } else {
        content.push_str(&format!(
            "{inset} {inset} {} {} re S\n",
            w - 2.0 * inset,
            h - 2.0 * inset
        ));
        if on {
            content.push_str(&format!("0 0 0 RG {} w\n0.25w 0.5h m\n", line_w));
            content.push_str(&format!("{w}*0.45 {h}*0.28 l\n{w}*0.78 {h}*0.72 l S\n"));
        }
    }
    doc.add_object(lopdf::Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Form",
            "BBox" => Object::Array(vec![
                pdf_num(0.0), pdf_num(0.0), pdf_num(w), pdf_num(h),
            ]),
            "Resources" => dictionary! {},
        },
        content.into_bytes(),
    ))
}

/// `applyPushButtonAppearance`: /MK /CA caption centred with Helvetica
/// metrics, /MK /BG//BC colours, font size min(12, h*0.6).
fn pushbutton_ap_stream(doc: &mut Document, widget: &Widget, rect: [f64; 4]) -> Option<ObjectId> {
    let (w, h) = (rect[2], rect[3]);
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let mk = widget_mk(doc, widget);
    let bg = mk_color(doc, mk.as_ref(), b"BG").unwrap_or([0.85, 0.85, 0.85]);
    let bc = mk_color(doc, mk.as_ref(), b"BC").unwrap_or([0.0, 0.0, 0.0]);
    let caption = mk_caption(doc, mk.as_ref());
    let size = 12.0f64.min(h * 0.6);

    let mut content = String::new();
    content.push_str(&format!("{}\n0 0 {w} {h} re f\n", rgb_ops(bg).trim_end()));
    content.push_str(&format!(
        "0.5 w {} 0.5 0.5 {} {} re S\n",
        rgb_ops(bc).trim_end().replacen("rg", "RG", 1),
        w - 1.0,
        h - 1.0
    ));
    let mut bytes = content.into_bytes();
    if let Some(caption) = caption {
        let encoded = super::misc::winansi_safe(&caption);
        let text_w = crate::fontmetrics::string_width("Helvetica", &encoded) * size / 1000.0;
        let tx = (w - text_w) / 2.0;
        let tx = if tx < 2.0 { 2.0 } else { tx };
        let baseline = (h - size) / 2.0 + size * 0.2;
        bytes.extend_from_slice(
            format!("BT /Helv {size} Tf 0 0 0 rg {tx} {baseline} Td (").as_bytes(),
        );
        super::misc::push_pdf_literal(&mut bytes, &encoded);
        bytes.extend_from_slice(b") Tj ET\n");
    }
    Some(ap_stream(doc, w, h, "Helv", "Helvetica", bytes))
}

/// Does the widget already carry an `/AP` with an `/N` entry?
fn widget_has_ap_n(doc: &Document, widget: &Widget) -> bool {
    let Some(ap) = dict_of(doc, &widget.stored)
        .and_then(|d| d.get(b"AP").ok().cloned())
        .and_then(|o| resolved(doc, &o).cloned())
    else {
        return false;
    };
    dict_of(doc, &ap)
        .and_then(|d| d.get(b"N").ok().cloned())
        .and_then(|o| resolved(doc, &o).cloned())
        .is_some_and(|n| matches!(n, Object::Dictionary(_) | Object::Stream(_)))
}

fn set_widget_ap(doc: &mut Document, widget: &Widget, ap: Dictionary) {
    let Ok(id) = widget.stored.as_reference() else {
        return;
    };
    if let Ok(dict) = doc.get_dictionary_mut(id) {
        dict.set("AP", Object::Dictionary(ap));
    }
}

fn remove_widget_as(doc: &mut Document, widget: &Widget) {
    let Ok(id) = widget.stored.as_reference() else {
        return;
    };
    if let Ok(dict) = doc.get_dictionary_mut(id) {
        dict.remove(b"AS");
    }
}

/// The field's current button value as a PDF name (`/V`), "Off" when the
/// stored value is not a name — `PDButton.getValue` semantics.
fn button_v_name(doc: &Document, field: &TerminalField) -> String {
    match inheritable(field, b"V").and_then(|o| resolved(doc, o)) {
        Some(Object::Name(n)) => String::from_utf8_lossy(n).into_owned(),
        _ => OFF_STATE.to_string(),
    }
}

/// Regenerates one widget's `/AP` where lopdf can draw it faithfully.
/// Returns whether the widget ends up with an `/AP /N`.
fn regenerate_widget_ap(
    doc: &mut Document,
    field: &TerminalField,
    widget: &Widget,
    widget_index: usize,
) -> bool {
    let Some(rect) = widget_rect(doc, widget) else {
        return widget_has_ap_n(doc, widget);
    };
    match field.kind {
        FieldKind::Text | FieldKind::Combobox => {
            if let Some(id) = text_ap_stream(doc, field, widget, rect) {
                set_widget_ap(doc, widget, dictionary! { "N" => Object::Reference(id) });
                true
            } else {
                widget_has_ap_n(doc, widget)
            }
        }
        FieldKind::Checkbox | FieldKind::Radio => {
            let is_radio = field.kind == FieldKind::Radio;
            let states = widget_on_states(doc, field);
            let on_state = states
                .get(widget_index)
                .cloned()
                .unwrap_or_else(|| "Yes".to_string());
            let on_id = toggle_ap_stream(doc, rect[2], rect[3], true, is_radio);
            let off_id = toggle_ap_stream(doc, rect[2], rect[3], false, is_radio);
            set_widget_ap(
                doc,
                widget,
                dictionary! {
                    "N" => dictionary! {
                        on_state.as_str() => Object::Reference(on_id),
                        OFF_STATE => Object::Reference(off_id),
                    },
                },
            );
            let v = button_v_name(doc, field);
            if v == on_state {
                set_widget_state(doc, widget, &on_state);
            } else {
                set_widget_state(doc, widget, OFF_STATE);
            }
            true
        }
        FieldKind::Button => {
            if let Some(id) = pushbutton_ap_stream(doc, widget, rect) {
                set_widget_ap(doc, widget, dictionary! { "N" => Object::Reference(id) });
                remove_widget_as(doc, widget);
                true
            } else {
                widget_has_ap_n(doc, widget)
            }
        }
        // Listbox scroll panes, signatures and anything unhandled keep
        // whatever appearance they shipped with.
        _ => widget_has_ap_n(doc, widget),
    }
}

/// `ensureAppearances`: regenerate what lopdf can draw, then clear
/// `/NeedAppearances` only when every widget carries an `/AP /N` — the
/// same post-condition Java reaches when `refreshAppearances` succeeds.
fn refresh_appearances(doc: &mut Document) {
    let fields = match terminal_fields(doc) {
        Ok(f) => f,
        Err(_) => {
            set_need_appearances(doc);
            return;
        }
    };
    let mut all_have_ap = true;
    for field in &fields {
        for (i, widget) in field.widgets.iter().enumerate() {
            if !regenerate_widget_ap(doc, field, widget, i) {
                all_have_ap = false;
            }
        }
    }
    set_need_appearances_value(doc, !all_have_ap);
}

fn set_need_appearances_value(doc: &mut Document, needed: bool) {
    let Some(acro_slot) = doc
        .catalog()
        .ok()
        .and_then(|c| c.get(b"AcroForm").ok().cloned())
    else {
        return;
    };
    match &acro_slot {
        Object::Reference(id) => {
            if let Ok(dict) = doc.get_dictionary_mut(*id) {
                dict.set("NeedAppearances", Object::Boolean(needed));
            }
        }
        Object::Dictionary(dict) => {
            let mut dict = dict.clone();
            dict.set("NeedAppearances", Object::Boolean(needed));
            if let Ok(catalog_id) = doc.trailer.get(b"Root").and_then(|o| o.as_reference()) {
                if let Ok(catalog) = doc.get_dictionary_mut(catalog_id) {
                    catalog.set("AcroForm", Object::Dictionary(dict));
                }
            }
        }
        _ => {}
    }
}

/// `repairWidgetGeometry`: widgets with a missing or degenerate /Rect get
/// the page's lower-left media-box corner and a 200x40 (clamped) box, and
/// are re-attached to the page's /Annots.
fn repair_widget_geometry(doc: &mut Document) {
    let page_ids = pdfdoc::page_ids(doc);
    if page_ids.is_empty() {
        return;
    }
    let fields = match terminal_fields(doc) {
        Ok(f) => f,
        Err(_) => return,
    };
    for field in fields {
        for widget in &field.widgets {
            let rect = widget_rect(doc, widget);
            let invalid = rect.is_none_or(|r| r[2] <= 0.0 || r[3] <= 0.0);
            if !invalid {
                continue;
            }
            let page_idx = find_widget_page(doc, widget).unwrap_or(0);
            let Some(&page) = page_ids.get(page_idx) else {
                continue;
            };
            let media = pdfdoc::page_boxes(doc, page)
                .map(|b| {
                    [
                        b.media[0] as f64,
                        b.media[1] as f64,
                        b.media[2] as f64,
                        b.media[3] as f64,
                    ]
                })
                .unwrap_or([0.0, 0.0, 612.0, 792.0]);
            let media_w = (media[2] - media[0]).max(0.0);
            let media_h = (media[3] - media[1]).max(0.0);
            let fallback = [media[0], media[1], media_w.min(200.0), media_h.min(40.0)];
            set_widget_rect(
                doc,
                widget,
                [fallback[0], fallback[1], fallback[2], fallback[3]],
            );
            if let Ok(id) = widget.stored.as_reference() {
                if let Ok(dict) = doc.get_dictionary_mut(id) {
                    dict.set("P", Object::Reference(page));
                }
            }
            // Java adds the widget back only when the page does not
            // already list it.
            let already_listed = doc
                .get_dictionary(page)
                .ok()
                .and_then(|d| d.get(b"Annots").ok().cloned())
                .and_then(|o| resolved(doc, &o).cloned())
                .and_then(|o| match o {
                    Object::Array(items) => Some(items),
                    _ => None,
                })
                .is_some_and(|items| {
                    items.iter().any(|item| {
                        item.as_reference()
                            .ok()
                            .zip(widget.stored.as_reference().ok())
                            .is_some_and(|(a, b)| a == b)
                    })
                });
            if !already_listed {
                push_page_annots(doc, page, widget.stored.clone());
            }
        }
    }
}

// ---------- payload parsing (FormPayloadParser) ----------

fn coerce_scalar(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(s) => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn normalize_field_value(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().filter_map(coerce_scalar).collect();
            Some(parts.join(","))
        }
        Value::Object(_) => Some(value.to_string()),
        _ => coerce_scalar(value),
    }
}

/// `extractName`: a string node, or an object with `name`/`targetName`/
/// `fieldName`, optionally nested under `field`.
fn extract_name(node: &Value) -> Option<String> {
    match node {
        Value::String(s) => {
            let t = s.trim();
            (!t.is_empty()).then(|| t.to_string())
        }
        Value::Object(map) => {
            for key in ["name", "targetName", "fieldName"] {
                if let Some(v) = map.get(key).and_then(coerce_scalar) {
                    if !v.trim().is_empty() {
                        return Some(v);
                    }
                }
            }
            map.get("field").and_then(extract_name)
        }
        _ => None,
    }
}

fn collect_names(node: &Value, out: &mut Vec<String>, seen: &mut HashSet<String>) {
    match node {
        Value::Array(items) => {
            for item in items {
                if let Some(name) = extract_name(item) {
                    if seen.insert(name.clone()) {
                        out.push(name);
                    }
                }
            }
        }
        Value::Object(map) => {
            if let Some(fields) = map.get("fields") {
                if fields.is_array() {
                    collect_names(fields, out, seen);
                    return;
                }
            }
            if let Some(name) = extract_name(node) {
                if seen.insert(name.clone()) {
                    out.push(name);
                }
            }
        }
        Value::String(s) => {
            let t = s.trim();
            if !t.is_empty() && seen.insert(t.to_string()) {
                out.push(t.to_string());
            }
        }
        _ => {}
    }
}

/// `parseNameList`: array of strings/objects, `{"fields":[...]}`, a
/// single name object, or a bare JSON string.
fn parse_name_list(text: &str) -> AppResult<Vec<String>> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let value: Value = serde_json::from_str(text)
        .map_err(|_| AppError::BadRequest("Invalid names payload format".into()))?;
    let mut names = Vec::new();
    let mut seen = HashSet::new();
    collect_names(&value, &mut names, &mut seen);
    Ok(names)
}

/// `extractFieldInfoArray`: `[{name, value|defaultValue}]` → record.
fn extract_field_info_array(node: &Value, out: &mut Vec<(String, Option<String>)>) {
    let Value::Array(items) = node else { return };
    for item in items {
        let Some(name) = extract_name(item) else {
            continue;
        };
        let value = item
            .get("value")
            .filter(|v| !v.is_null())
            .or_else(|| item.get("defaultValue").filter(|v| !v.is_null()));
        out.push((name, value.and_then(normalize_field_value)));
    }
}

fn object_to_value_map(map: &serde_json::Map<String, Value>) -> Vec<(String, Option<String>)> {
    map.iter()
        .map(|(k, v)| {
            let value = match v {
                Value::Null => None,
                Value::String(_) | Value::Number(_) | Value::Bool(_) => coerce_scalar(v),
                _ => Some(v.to_string()),
            };
            (k.clone(), value)
        })
        .collect()
}

/// `parseValueMap`: flat object, `{"template":{...}}`, `{"fields":[...]}`,
/// or a single-record array — all reduced to name→value pairs.
fn parse_value_map(text: &str) -> AppResult<Vec<(String, Option<String>)>> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let value: Value = serde_json::from_str(text)
        .map_err(|_| AppError::BadRequest("Invalid data payload format".into()))?;
    match &value {
        Value::Object(map) => {
            if let Some(template) = map.get("template").and_then(Value::as_object) {
                return Ok(object_to_value_map(template));
            }
            if let Some(fields) = map.get("fields") {
                if fields.is_array() {
                    let mut out = Vec::new();
                    extract_field_info_array(fields, &mut out);
                    if !out.is_empty() {
                        return Ok(out);
                    }
                }
            }
            Ok(object_to_value_map(map))
        }
        Value::Array(items) => {
            let Some(first) = items.first() else {
                return Ok(Vec::new());
            };
            if first.is_object()
                && ["name", "value", "defaultValue"]
                    .iter()
                    .any(|k| first.get(*k).is_some())
            {
                let mut out = Vec::new();
                extract_field_info_array(&value, &mut out);
                return Ok(out);
            }
            if let Some(map) = first.as_object() {
                return Ok(object_to_value_map(map));
            }
            Ok(Vec::new())
        }
        _ => Ok(Vec::new()),
    }
}

/// `data` part bytes → UTF-8 JSON text; empty/missing means no values.
fn data_text(data: Option<&Upload>) -> AppResult<Option<String>> {
    match data {
        Some(u) if !u.bytes.is_empty() => Ok(Some(String::from_utf8_lossy(&u.bytes).into_owned())),
        _ => Ok(None),
    }
}

fn apply_data_values(doc: &mut Document, data: Option<&Upload>, strict: bool) -> AppResult<()> {
    let Some(text) = data_text(data)? else {
        return Ok(());
    };
    // The extract endpoints read `Map<String,String>`; scalar coercion in
    // parse_value_map already yields strings.
    let values = parse_value_map(&text)?;
    apply_field_values(doc, &values, false, strict)
}

// ---------- delete ----------

fn stored_eq(a: &Object, b: &Object) -> bool {
    match (a.as_reference(), b.as_reference()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

/// Mutates the array stored under `key` of the dictionary object `holder`,
/// following one reference level for indirect arrays.
fn with_array_mut<R>(
    doc: &mut Document,
    holder: ObjectId,
    key: &[u8],
    f: impl FnOnce(&mut Vec<Object>) -> R,
) -> Option<R> {
    let slot = {
        let dict = pdfdoc::dict_of(doc.get_object(holder).ok()?)?;
        dict.get(key).ok()?.clone()
    };
    match slot {
        Object::Reference(arr_id) => {
            if let Some(Object::Array(items)) = doc.objects.get_mut(&arr_id) {
                Some(f(items))
            } else {
                None
            }
        }
        Object::Array(_) => {
            let obj = doc.objects.get_mut(&holder)?;
            let dict = pdfdoc::dict_mut(obj)?;
            match dict.get_mut(key).ok()? {
                Object::Array(items) => Some(f(items)),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Removes `target` from the `/Fields` array or the `/Kids` array of
/// whichever ancestor lists it, then clears the field dictionary —
/// `removeFieldFromDocument` minus the widget sweep (done by the caller).
fn detach_field(doc: &mut Document, target: &Object) -> AppResult<()> {
    // Locate the container: AcroForm /Fields, or a parent's /Kids.
    let mut container: Option<(ObjectId, &'static [u8])> = None;
    let acro = pdfdoc::acroform(doc);
    if let Some(form) = &acro {
        // /Fields may sit in an indirect AcroForm dict or inline in the
        // catalog; find which object id holds it.
        let acro_id = doc
            .catalog()
            .ok()
            .and_then(|c| c.get(b"AcroForm").ok())
            .and_then(|o| o.as_reference().ok());
        let fields_items = dict_get(form, b"Fields")
            .and_then(|o| resolved(doc, o))
            .and_then(|o| match o {
                Object::Array(items) => Some(items.clone()),
                _ => None,
            })
            .unwrap_or_default();
        if fields_items.iter().any(|i| stored_eq(i, target)) {
            if let Some(id) = acro_id {
                container = Some((id, b"Fields".as_slice()));
            } else {
                // Inline AcroForm: mutate through the catalog object.
                if let Ok(root) = doc.trailer.get(b"Root").and_then(|o| o.as_reference()) {
                    let removed = {
                        let Ok(catalog) = doc.get_dictionary_mut(root) else {
                            return Ok(());
                        };
                        let Ok(Object::Dictionary(af)) = catalog.get_mut(b"AcroForm") else {
                            return Ok(());
                        };
                        match af.get_mut(b"Fields") {
                            Ok(Object::Array(items)) => {
                                let before = items.len();
                                items.retain(|i| !stored_eq(i, target));
                                items.len() != before
                            }
                            _ => false,
                        }
                    };
                    let _ = removed;
                }
                container = None;
            }
        } else {
            // Depth-first search for the parent whose /Kids lists target.
            fn find_parent(
                doc: &Document,
                items: &[Object],
                target: &Object,
                depth: usize,
            ) -> Option<ObjectId> {
                if depth > 64 {
                    return None;
                }
                for item in items {
                    let Some(dict) = dict_of(doc, item) else {
                        continue;
                    };
                    let kids = dict_get(&dict, b"Kids")
                        .and_then(|o| resolved(doc, o))
                        .and_then(|o| match o {
                            Object::Array(items) => Some(items.clone()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    if kids.iter().any(|k| stored_eq(k, target)) {
                        return item.as_reference().ok();
                    }
                    if let Some(found) = find_parent(doc, &kids, target, depth + 1) {
                        return Some(found);
                    }
                }
                None
            }
            container =
                find_parent(doc, &fields_items, target, 0).map(|id| (id, b"Kids".as_slice()));
        }
    }
    if let Some((holder, key)) = container {
        with_array_mut(doc, holder, key, |items| {
            items.retain(|i| !stored_eq(i, target));
        });
    }
    if let Ok(id) = target.as_reference() {
        if let Some(obj) = doc.objects.get_mut(&id) {
            if let Some(dict) = pdfdoc::dict_mut(obj) {
                *dict = Dictionary::new();
            }
        }
    }
    Ok(())
}

#[derive(Debug)]
struct SkippedEdit {
    operation: &'static str,
    target: String,
    reason: String,
}

/// `deleteFormFields`: locate each named terminal field, strip its widget
/// annotations from every page, detach it from the field tree.
fn delete_fields(doc: &mut Document, names: &[String]) -> AppResult<Vec<SkippedEdit>> {
    let mut skipped = Vec::new();
    if names.is_empty() {
        return Ok(skipped);
    }
    if pdfdoc::acroform(doc).is_none() {
        for name in names {
            skipped.push(SkippedEdit {
                operation: "delete",
                target: name.clone(),
                reason: "the document has no form to delete from".into(),
            });
        }
        return Ok(skipped);
    }
    let page_ids = pdfdoc::page_ids(doc);
    for name in names {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            continue;
        }
        let fields = terminal_fields(doc)?;
        let Some(field) = fields
            .iter()
            .find(|f| f.qualified == trimmed || f.qualified.rsplit('.').next() == Some(trimmed))
        else {
            skipped.push(SkippedEdit {
                operation: "delete",
                target: trimmed.to_string(),
                reason: "no field with that name exists".into(),
            });
            continue;
        };
        let field = TerminalField {
            stored: field.stored.clone(),
            chain: field.chain.clone(),
            qualified: field.qualified.clone(),
            kind: field.kind,
            ff: field.ff,
            widgets: field
                .widgets
                .iter()
                .map(|w| Widget {
                    stored: w.stored.clone(),
                    dict: w.dict.clone(),
                })
                .collect(),
        };
        // Widget sweep: every stored widget object leaves each page's
        // /Annots. A merged field-widget is its own annotation too.
        let mut widget_objs: Vec<Object> = field.widgets.iter().map(|w| w.stored.clone()).collect();
        if field
            .widgets
            .iter()
            .all(|w| stored_eq(&w.stored, &field.stored))
        {
            widget_objs.push(field.stored.clone());
        }
        for page_id in &page_ids {
            with_array_mut(doc, *page_id, b"Annots", |items| {
                items.retain(|a| !widget_objs.iter().any(|w| stored_eq(a, w)));
            });
        }
        detach_field(doc, &field.stored)?;
    }
    refresh_appearances(doc);
    Ok(skipped)
}

// ---------- skipped-edit headers ----------

const MAX_REPORTED_SKIPS: usize = 20;
const MAX_SKIP_FIELD_CHARS: usize = 120;
const MAX_SKIP_HEADER_BYTES: usize = 4096;

fn abbreviate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_string()
    } else {
        format!("{}...", value.chars().take(max).collect::<String>())
    }
}

fn b64_encode(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// `withSkippedEdits`: base64 JSON header, capped at 20 entries / 4 KiB.
fn with_skipped_edits(mut response: Response, skipped: &[SkippedEdit]) -> Response {
    if skipped.is_empty() {
        return response;
    }
    let mut reported: Vec<Value> = Vec::new();
    let mut encoded = String::new();
    for edit in skipped {
        if reported.len() >= MAX_REPORTED_SKIPS {
            break;
        }
        reported.push(json!({
            "operation": edit.operation,
            "target": abbreviate(&edit.target, MAX_SKIP_FIELD_CHARS),
            "reason": abbreviate(&edit.reason, MAX_SKIP_FIELD_CHARS),
        }));
        let candidate = b64_encode(serde_json::to_string(&reported).unwrap().as_bytes());
        if candidate.len() > MAX_SKIP_HEADER_BYTES {
            reported.pop();
            break;
        }
        encoded = candidate;
    }
    if let Ok(v) = encoded.parse() {
        response
            .headers_mut()
            .insert("x-stirling-skipped-field-edits", v);
    }
    if let Ok(v) = skipped.len().to_string().parse() {
        response
            .headers_mut()
            .insert("x-stirling-skipped-field-edits-total", v);
    }
    response
}

// ---------- endpoints: fill / delete-fields ----------

/// `POST /api/v1/form/fill` → `name_filled.pdf`; `flatten=true` merges
/// the form afterwards.
#[stirling_macros::endpoint(receive = "receive_form_parts")]
fn fill_form(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    extras: Vec<(String, Upload)>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let flatten = crate::upload::param_bool(&params, "flatten");
    let data = extra(&extras, "data");
    let upload = uploads.into_iter().next().unwrap();
    let mut document = load_document(&upload)?;
    let values = match data_text(data)? {
        Some(text) => parse_value_map(&text)?,
        // Java treats an absent/empty `data` part as an empty map
        // and still runs the strict applyFieldValues pass.
        None => Vec::new(),
    };
    apply_field_values(&mut document, &values, flatten, true)?;
    pdfdoc::touch_loaded_info(&mut document);
    Ok(pdf_response(
        pdfdoc::save(&mut document)?,
        &pdf_filename(&upload.name, "filled"),
    ))
}

/// `POST /api/v1/form/delete-fields` → `name_updated.pdf` plus the
/// skipped-edits headers when names did not resolve.
#[stirling_macros::endpoint(receive = "receive_form_parts")]
fn delete_fields_endpoint(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    extras: Vec<(String, Upload)>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let names_text = match extra(&extras, "names") {
        Some(u) if !u.bytes.is_empty() => Some(String::from_utf8_lossy(&u.bytes).into_owned()),
        _ => params
            .iter()
            .rev()
            .find(|(n, _)| n == "names")
            .map(|(_, v)| v.clone()),
    };
    let name_list = match names_text {
        Some(text) => parse_name_list(&text)?,
        None => Vec::new(),
    };
    if name_list.is_empty() {
        return Err(AppError::BadRequest(
            "names payload must contain at least one value".into(),
        ));
    }
    let upload = uploads.into_iter().next().unwrap();
    let mut document = load_document(&upload)?;
    let skipped = delete_fields(&mut document, &name_list)?;
    pdfdoc::touch_loaded_info(&mut document);
    let bytes = pdfdoc::save(&mut document)?;
    let response = pdf_response(bytes, &pdf_filename(&upload.name, "updated"));
    Ok(with_skipped_edits(response, &skipped))
}

/// `buildBaseName`: simple filename, `.pdf` stripped, `_{suffix}` added;
/// the caller appends the real extension.
fn pdf_filename(original: &str, suffix: &str) -> String {
    let base = original
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("document");
    let stem = if base.to_lowercase().ends_with(".pdf") {
        &base[..base.len() - 4]
    } else {
        base
    };
    format!("{stem}_{suffix}.pdf")
}

// ---------- xlsx ----------

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Minimal OOXML workbook: inline strings, one "Form Fields" sheet —
/// what Apache POI's `XSSFWorkbook` produces for the two-column export.
fn xlsx_bytes(entries: &[FieldEntry]) -> Vec<u8> {
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    let mut sheet = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
         <worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">\
         <sheetData><row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>Field Name</t></is></c>\
         <c r=\"B1\" t=\"inlineStr\"><is><t>Value</t></is></c></row>",
    );
    for (i, entry) in entries.iter().enumerate() {
        let r = i + 2;
        let (name, _, value, _) = &entry.template_row;
        let cell_value = value.clone().unwrap_or_default();
        sheet.push_str(&format!(
            "<row r=\"{r}\"><c r=\"A{r}\" t=\"inlineStr\"><is><t>{}</t></is></c>\
             <c r=\"B{r}\" t=\"inlineStr\"><is><t>{}</t></is></c></row>",
            xml_escape(name),
            xml_escape(&cell_value),
        ));
    }
    sheet.push_str("</sheetData></worksheet>");

    let mut zip = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let parts: [(&str, String); 6] = [
        (
            "[Content_Types].xml",
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
             <Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
             <Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
             <Default Extension=\"xml\" ContentType=\"application/xml\"/>\
             <Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>\
             <Override PartName=\"/xl/worksheets/sheet1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>\
             <Override PartName=\"/xl/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml\"/>\
             </Types>"
                .into(),
        ),
        (
            "_rels/.rels",
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
             <Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
             <Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"xl/workbook.xml\"/>\
             </Relationships>"
                .into(),
        ),
        (
            "xl/workbook.xml",
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
             <workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" \
             xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\">\
             <sheets><sheet name=\"Form Fields\" sheetId=\"1\" r:id=\"rId1\"/></sheets></workbook>"
                .into(),
        ),
        (
            "xl/_rels/workbook.xml.rels",
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
             <Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
             <Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet1.xml\"/>\
             <Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/>\
             </Relationships>"
                .into(),
        ),
        (
            "xl/styles.xml",
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
             <styleSheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">\
             <fonts count=\"1\"><font><sz val=\"11\"/><name val=\"Calibri\"/></font></fonts>\
             <fills count=\"1\"><fill><patternFill patternType=\"none\"/></fill></fills>\
             <borders count=\"1\"><border/></borders>\
             <cellStyleXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\"/></cellStyleXfs>\
             <cellXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\"/></cellXfs>\
             </styleSheet>"
                .into(),
        ),
        ("xl/worksheets/sheet1.xml", sheet),
    ];
    for (name, body) in parts {
        zip.start_file(name, options).expect("zip entry");
        zip.write_all(body.as_bytes()).expect("zip write");
    }
    zip.finish().expect("zip finish").into_inner()
}

// ---------- field creation / modification (add/edit/modify-fields) ----------

use serde::Deserialize;

/// `NewFormFieldDefinition` / `ModifyFormFieldDefinition` share one JSON
/// shape; `targetName` is only read by the modify paths.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct FieldDefinition {
    name: Option<String>,
    label: Option<String>,
    #[serde(rename = "type")]
    field_type: Option<String>,
    page_index: Option<i64>,
    x: Option<f64>,
    y: Option<f64>,
    width: Option<f64>,
    height: Option<f64>,
    required: Option<bool>,
    multi_select: Option<bool>,
    options: Option<Vec<Option<String>>>,
    default_value: Option<String>,
    tooltip: Option<String>,
    font_size: Option<f64>,
    read_only: Option<bool>,
    multiline: Option<bool>,
    max_length: Option<i64>,
    button_action: Option<String>,
    option_gap: Option<f64>,
    option_size: Option<f64>,
    target_name: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct FieldEditBatch {
    add: Vec<FieldDefinition>,
    modify: Vec<FieldDefinition>,
    /// Kept as raw JSON: the delete section accepts every `parseNameList`
    /// shape, not just a string array.
    delete: Option<serde_json::Value>,
}

const FF_COMB: i64 = 1 << 24;
const OFF_STATE: &str = "Off";

fn sanitize_for_log(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// `FormUtils.invalidFieldNameReason`: null/blank names pass (the caller
/// substitutes a generated one); periods and control characters are the
/// only hard rejections.
fn invalid_field_name_reason(name: Option<&str>) -> Option<String> {
    let name = name?;
    if name.trim().is_empty() {
        return None;
    }
    if name.chars().any(|c| c.is_control()) {
        return Some("Field name cannot contain line breaks or control characters.".into());
    }
    if name.contains('.') {
        return Some(format!(
            "Field name '{}' cannot contain a period. PDF forms use '.' to separate a parent field from its children.",
            sanitize_for_log(name)
        ));
    }
    None
}

fn parent_prefix(qualified: &str) -> &str {
    match qualified.rfind('.') {
        Some(i) => &qualified[..=i],
        None => "",
    }
}

/// `FormUtils.leafName`: only a rename inside the target's own parent may
/// carry the qualified prefix; anything else is used verbatim.
fn leaf_name<'a>(target: &str, new_name: &'a str) -> &'a str {
    let prefix = parent_prefix(target);
    if !prefix.is_empty() && new_name.starts_with(prefix) {
        &new_name[prefix.len()..]
    } else {
        new_name
    }
}

/// `FormUtils.renameProblem`.
fn rename_problem(target: &str, new_name: Option<&str>) -> Option<String> {
    let new_name = new_name?;
    if new_name == target {
        return None;
    }
    let trimmed = new_name.trim();
    let leaf = leaf_name(target, trimmed);
    if !trimmed.is_empty() && leaf.trim().is_empty() {
        return Some(format!(
            "Field name '{new_name}' has no name after the parent prefix."
        ));
    }
    invalid_field_name_reason(Some(leaf))
}

fn normalize_field_type(field_type: Option<&str>) -> String {
    field_type
        .map(|t| t.trim().to_lowercase())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| T_TEXT.to_string())
}

fn sanitize_options(options: Option<&Vec<Option<String>>>) -> Vec<String> {
    options
        .map(|opts| {
            opts.iter()
                .flatten()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn detect_field_type_name(kind: FieldKind) -> &'static str {
    match kind {
        FieldKind::Text => "text",
        FieldKind::Checkbox => "checkbox",
        FieldKind::Combobox => "combobox",
        FieldKind::Listbox => "listbox",
        FieldKind::Radio => "radio",
        FieldKind::Button => "button",
        FieldKind::Signature => "signature",
    }
}

/// `getSupportedNewFieldTypes` minus creation support: signature fields
/// can be modified in place but never created.
fn creation_supported(field_type: &str) -> bool {
    matches!(
        field_type,
        "text" | "checkbox" | "combobox" | "listbox" | "radio" | "button"
    )
}

fn collect_existing_field_names(doc: &Document) -> HashSet<String> {
    terminal_fields(doc)
        .map(|fields| fields.into_iter().map(|f| f.qualified).collect())
        .unwrap_or_default()
}

/// `generateUniqueFieldName`: `existing` holds qualified names, so the
/// caller's parent prefix is part of the collision key.
fn generate_unique_field_name(
    base: Option<&str>,
    existing: &HashSet<String>,
    prefix: &str,
) -> String {
    let trimmed = base
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("field");
    let mut candidate = trimmed.to_string();
    let mut counter = 1;
    while existing.contains(&format!("{prefix}{candidate}")) {
        candidate = format!("{trimmed}_{counter}");
        counter += 1;
    }
    candidate
}

fn find_terminal<'a>(fields: &'a [TerminalField], name: &str) -> Option<&'a TerminalField> {
    let partial = name.rsplit('.').next().unwrap_or(name);
    fields.iter().find(|f| f.qualified == name).or_else(|| {
        fields
            .iter()
            .find(|f| f.qualified.rsplit('.').next() == Some(partial))
    })
}

// ---------- AcroForm / page mutation helpers ----------

fn catalog_id(doc: &Document) -> Option<ObjectId> {
    doc.trailer
        .get(b"Root")
        .ok()
        .and_then(|o| o.as_reference().ok())
}

fn acroform_slot(doc: &Document) -> Option<Object> {
    let root = catalog_id(doc)?;
    doc.get_dictionary(root)
        .ok()?
        .get(b"AcroForm")
        .ok()
        .cloned()
}

/// `withAcroForm` covering both storage shapes: an indirect dictionary
/// mutated in place, or an inline dictionary rewritten through the catalog.
fn with_acroform_mut(doc: &mut Document, f: impl FnOnce(&mut Dictionary)) {
    let Some(slot) = acroform_slot(doc) else {
        return;
    };
    match slot {
        Object::Reference(id) => {
            if let Ok(dict) = doc.get_dictionary_mut(id) {
                f(dict);
            }
        }
        Object::Dictionary(dict) => {
            let mut dict = dict;
            f(&mut dict);
            if let Some(root) = catalog_id(doc) {
                if let Ok(catalog) = doc.get_dictionary_mut(root) {
                    catalog.set("AcroForm", Object::Dictionary(dict));
                }
            }
        }
        _ => {}
    }
}

/// `getAcroFormSafely` + create: documents without an AcroForm get a new
/// indirect one, like `new PDAcroForm(document)`.
fn ensure_acroform(doc: &mut Document) -> AppResult<()> {
    if acroform_slot(doc).is_some() {
        return Ok(());
    }
    let id = doc.add_object(dictionary! {
        "Fields" => Object::Array(Vec::new()),
    });
    let Some(root) = catalog_id(doc) else {
        return Err(AppError::BadRequest("PDF has no catalog".into()));
    };
    let catalog = doc
        .get_dictionary_mut(root)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("catalog: {e}")))?;
    catalog.set("AcroForm", Object::Reference(id));
    Ok(())
}

fn push_acroform_field(doc: &mut Document, field: Object) {
    with_acroform_mut(doc, |acro| {
        let mut fields = match acro.get(b"Fields").ok().cloned() {
            Some(Object::Array(items)) => items,
            _ => Vec::new(),
        };
        fields.push(field);
        acro.set("Fields", Object::Array(fields));
    });
}

fn push_page_annots(doc: &mut Document, page: ObjectId, annot: Object) {
    if let Ok(dict) = doc.get_dictionary_mut(page) {
        let mut annots = match dict.get(b"Annots").ok().cloned() {
            Some(Object::Array(items)) => items,
            _ => Vec::new(),
        };
        annots.push(annot);
        dict.set("Annots", Object::Array(annots));
    }
}

/// `PDPage.getCropBox`: inherited through the page tree, clipped to the
/// media box, Letter fallback — `pdfdoc::page_boxes` already resolves all
/// of that.
fn page_crop_box(doc: &Document, page: ObjectId) -> [f64; 4] {
    pdfdoc::page_boxes(doc, page)
        .map(|b| {
            [
                b.crop[0] as f64,
                b.crop[1] as f64,
                b.crop[2] as f64,
                b.crop[3] as f64,
            ]
        })
        .unwrap_or([0.0, 0.0, 0.0, 0.0])
}

fn pdf_num(value: f64) -> Object {
    if value.fract() == 0.0 && value >= i64::MIN as f64 && value <= i64::MAX as f64 {
        Object::Integer(value as i64)
    } else {
        Object::Real(value as f32)
    }
}

fn pdf_rect(x: f64, y: f64, w: f64, h: f64) -> Object {
    Object::Array(vec![pdf_num(x), pdf_num(y), pdf_num(x + w), pdf_num(y + h)])
}

fn rect_parts(rect: &Object) -> Option<[f64; 4]> {
    let Object::Array(items) = rect else {
        return None;
    };
    if items.len() < 4 {
        return None;
    }
    let mut n = [0.0; 4];
    for (i, item) in items.iter().take(4).enumerate() {
        n[i] = match item {
            Object::Integer(v) => *v as f64,
            Object::Real(v) => *v as f64,
            _ => return None,
        };
    }
    Some([n[0], n[1], n[2] - n[0], n[3] - n[1]])
}

fn set_dict_value(doc: &mut Document, stored: &Object, key: &[u8], value: Object) {
    let Ok(id) = stored.as_reference() else {
        return;
    };
    if let Ok(dict) = doc.get_dictionary_mut(id) {
        dict.set(key, value);
    }
}

fn remove_dict_value(doc: &mut Document, stored: &Object, key: &[u8]) {
    let Ok(id) = stored.as_reference() else {
        return;
    };
    if let Ok(dict) = doc.get_dictionary_mut(id) {
        dict.remove(key);
    }
}

fn dict_value(doc: &Document, stored: &Object, key: &[u8]) -> Option<Object> {
    let id = stored.as_reference().ok()?;
    doc.get_dictionary(id).ok()?.get(key).ok().cloned()
}

/// `/DA` + `/DR` defaults every new variable-text field relies on, plus
/// the Helv font `ensureAcroFormDefaults` installs for PDFBox's
/// appearance generator. NeedAppearances stays on so viewers regenerate
/// appearances for fields whose /AP lopdf cannot draw faithfully.
fn ensure_acroform_defaults(doc: &mut Document) {
    with_acroform_mut(doc, |acro| {
        if acro.get(b"DA").is_err() {
            acro.set("DA", Object::string_literal("/Helv 0 Tf 0 g"));
        }
    });
    ensure_acroform_dr(doc);
}

/// The `/DR` font resource `ensureAcroFormDefaults` installs so a `/DA`
/// naming Helv resolves. Resolved through `dict_of` because `/DR` and
/// `/Font` may sit behind indirect references; the rewritten dict is
/// stored inline, which is legal PDF.
fn ensure_acroform_dr(doc: &mut Document) {
    let Some(slot) = acroform_slot(doc) else {
        return;
    };
    let acro = match &slot {
        Object::Reference(id) => doc.get_dictionary(*id).ok().cloned(),
        Object::Dictionary(d) => Some(d.clone()),
        _ => None,
    };
    let Some(acro) = acro else { return };
    let dr = acro.get(b"DR").ok().and_then(|o| dict_of(doc, o));
    let fonts = dr
        .as_ref()
        .and_then(|d| d.get(b"Font").ok())
        .and_then(|o| dict_of(doc, o));
    if fonts.as_ref().and_then(|d| d.get(b"Helv").ok()).is_some() {
        return;
    }
    let helv = Object::Dictionary(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
    });
    let mut fonts = fonts.unwrap_or_default();
    fonts.set("Helv", helv.clone());
    fonts.set("Helvetica", helv);
    let mut dr = dr.unwrap_or_default();
    dr.set("Font", Object::Dictionary(fonts));
    with_acroform_mut(doc, move |a| a.set("DR", Object::Dictionary(dr)));
}

// ---------- appearance streams for created button-family fields ----------

/// A minimal variable-text-free appearance: border plus, for the on
/// state, a check stroke or filled dot. Viewers that honour
/// /NeedAppearances regenerate richer versions on open.
fn checkbox_ap(doc: &mut Document, w: f64, h: f64) -> Dictionary {
    let on_id = toggle_ap_stream(doc, w, h, true, false);
    let off_id = toggle_ap_stream(doc, w, h, false, false);
    dictionary! {
        "N" => dictionary! {
            "Yes" => Object::Reference(on_id),
            "Off" => Object::Reference(off_id),
        },
    }
}

fn radio_ap(doc: &mut Document, w: f64, h: f64, on_state: &str) -> Dictionary {
    let on_id = toggle_ap_stream(doc, w, h, true, true);
    let off_id = toggle_ap_stream(doc, w, h, false, true);
    dictionary! {
        "N" => dictionary! {
            on_state => Object::Reference(on_id),
            OFF_STATE => Object::Reference(off_id),
        },
    }
}

/// `sanitizeOnState`: a PDF-name-safe unique on-state per radio option.
fn sanitize_on_state(raw: Option<&str>, index: usize, used: &mut HashSet<String>) -> String {
    let base = raw
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect::<String>()
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("Option{}", index + 1));
    let base = if base.eq_ignore_ascii_case(OFF_STATE) {
        format!("Option{}", index + 1)
    } else {
        base
    };
    let mut candidate = base.clone();
    let mut suffix = 1;
    while !used.insert(candidate.clone()) {
        candidate = format!("{base}_{suffix}");
        suffix += 1;
    }
    candidate
}

/// `radioOptionRects`: the option stack is top-down inside the base box;
/// overrides size the slot directly, otherwise a quarter-slot gap is
/// derived and the stack fills the drawn height.
fn radio_option_rects(
    box_: [f64; 4],
    count: usize,
    gap_override: Option<f64>,
    size_override: Option<f64>,
) -> Vec<[f64; 4]> {
    let n = count.max(1) as f64;
    let (x, y, w, h) = (box_[0], box_[1], box_[2], box_[3]);
    let slot = h / n;
    let size = match size_override {
        Some(s) if s > 0.0 => s,
        _ => match gap_override {
            Some(g) if g >= 0.0 => (h - (n - 1.0) * g) / n,
            _ => slot * 0.75,
        },
    }
    .min(w)
    .max(1.0);
    let gap = match gap_override {
        Some(g) if g >= 0.0 => g,
        _ => {
            if n > 1.0 {
                ((h - n * size) / (n - 1.0)).max(0.0)
            } else {
                0.0
            }
        }
    };
    let top = y + h;
    (0..count)
        .map(|i| {
            let iy = top - (i as f64 + 1.0) * size - i as f64 * gap;
            [x, iy, size, size]
        })
        .collect()
}

// ---------- creation ----------

struct NewFieldSpec<'a> {
    definition: &'a FieldDefinition,
    resolved_type: &'a str,
    unique_name: &'a str,
    page: ObjectId,
    rect: [f64; 4],
    options: &'a [String],
}

fn field_flags(definition: &FieldDefinition, extra: i64) -> i64 {
    let mut ff = extra;
    if definition.required == Some(true) {
        ff |= FF_REQUIRED;
    }
    if definition.read_only == Some(true) {
        ff |= FF_READ_ONLY;
    }
    ff
}

fn set_default_appearance(dict: &mut Dictionary, definition: &FieldDefinition) {
    if let Some(size) = definition.font_size {
        if size > 0.0 {
            dict.set("DA", Object::string_literal(format!("/Helv {size} Tf 0 g")));
        }
    }
}

fn base_widget_entries(
    dict: &mut Dictionary,
    page: ObjectId,
    rect: [f64; 4],
    name: &str,
    ft: &str,
    ff: i64,
) {
    dict.set("Type", "Annot");
    dict.set("Subtype", "Widget");
    dict.set("FT", ft);
    dict.set("T", Object::string_literal(name));
    dict.set("Rect", pdf_rect(rect[0], rect[1], rect[2], rect[3]));
    dict.set("F", 4);
    dict.set("P", Object::Reference(page));
    dict.set("Ff", ff);
    dict.set("DA", Object::string_literal("/Helv 0 Tf 0 g"));
}

/// `createNewField` + `registerNewField` for the single-widget types:
/// the field dictionary doubles as its only widget (merged /Kids-less
/// terminal), exactly the shape PDFBox writes.
fn create_single_widget_field(doc: &mut Document, spec: &NewFieldSpec) -> AppResult<()> {
    let definition = spec.definition;
    let mut dict = Dictionary::new();
    let ff_base = field_flags(definition, 0);
    match spec.resolved_type {
        T_TEXT => {
            let mut ff = ff_base;
            if definition.multiline == Some(true) {
                ff |= FF_MULTILINE;
            }
            if let Some(max_len) = definition.max_length {
                if max_len > 0 {
                    ff |= FF_COMB;
                }
            }
            base_widget_entries(&mut dict, spec.page, spec.rect, spec.unique_name, "Tx", ff);
            if let Some(max_len) = definition.max_length {
                if max_len > 0 {
                    dict.set("MaxLen", max_len);
                }
            }
            if let Some(value) = definition
                .default_value
                .as_deref()
                .filter(|v| !v.trim().is_empty())
            {
                dict.set("V", Object::string_literal(value));
            }
        }
        T_CHECKBOX => {
            base_widget_entries(
                &mut dict,
                spec.page,
                spec.rect,
                spec.unique_name,
                "Btn",
                ff_base,
            );
            let w = spec.rect[2];
            let h = spec.rect[3];
            let ap = checkbox_ap(doc, w, h);
            dict.set("AP", Object::Dictionary(ap));
            dict.set("AS", OFF_STATE);
            dict.set("V", OFF_STATE);
        }
        T_COMBOBOX | T_LISTBOX => {
            let mut ff = ff_base;
            if spec.resolved_type == T_COMBOBOX {
                ff |= FF_COMBO;
            }
            if spec.resolved_type == T_LISTBOX && definition.multi_select == Some(true) {
                ff |= FF_MULTI_SELECT;
            }
            base_widget_entries(&mut dict, spec.page, spec.rect, spec.unique_name, "Ch", ff);
            if !spec.options.is_empty() {
                dict.set(
                    "Opt",
                    Object::Array(
                        spec.options
                            .iter()
                            .map(|o| Object::string_literal(o.clone()))
                            .collect(),
                    ),
                );
            }
            if let Some(value) = definition
                .default_value
                .as_deref()
                .filter(|v| !v.trim().is_empty())
            {
                dict.set("V", Object::string_literal(value));
            }
        }
        T_BUTTON => {
            let ff = field_flags(definition, FF_PUSHBUTTON);
            base_widget_entries(&mut dict, spec.page, spec.rect, spec.unique_name, "Btn", ff);
            // Java stores the caption on /MK /CA and lets
            // applyButtonAppearances draw the /AP from it; the shared
            // refresh pass at the end of the batch does the same here.
            let caption = definition
                .label
                .as_deref()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .unwrap_or(spec.unique_name);
            dict.set(
                "MK",
                dictionary! {
                    "CA" => Object::string_literal(caption),
                },
            );
            if let Some(action) = button_action_dict(definition.button_action.as_deref()) {
                dict.set("A", action);
            }
        }
        _ => unreachable!("creation_supported gates this"),
    }
    if let Some(label) = definition.label.as_deref().filter(|l| !l.trim().is_empty()) {
        dict.set("TU", Object::string_literal(label));
    }
    if let Some(tooltip) = definition
        .tooltip
        .as_deref()
        .filter(|t| !t.trim().is_empty())
    {
        dict.set("TU", Object::string_literal(tooltip));
    }
    set_default_appearance(&mut dict, definition);
    let id = doc.add_object(Object::Dictionary(dict));
    push_acroform_field(doc, Object::Reference(id));
    push_page_annots(doc, spec.page, Object::Reference(id));
    Ok(())
}

/// `FormFieldTypeSupport.applyButtonAction`: reset/print/uri:/submit:
/// specs become the widget's /A; blank clears it; anything else is a
/// reported problem, not a silent drop.
fn button_action_dict(action: Option<&str>) -> Option<Object> {
    let spec = action?.trim();
    if spec.is_empty() {
        return None;
    }
    let lower = spec.to_lowercase();
    if lower == "reset" {
        return Some(Object::Dictionary(dictionary! { "S" => "ResetForm" }));
    }
    if lower == "print" {
        return Some(Object::Dictionary(dictionary! {
            "S" => "Named",
            "N" => "Print",
        }));
    }
    if let Some(rest) = lower.strip_prefix("uri:") {
        if rest.trim().is_empty() {
            return None;
        }
        return Some(Object::Dictionary(dictionary! {
            "S" => "URI",
            "URI" => Object::string_literal(spec[4..].trim()),
        }));
    }
    if let Some(rest) = lower.strip_prefix("submit:") {
        if rest.trim().is_empty() {
            return None;
        }
        return Some(Object::Dictionary(dictionary! {
            "S" => "SubmitForm",
            "F" => Object::string_literal(spec[7..].trim()),
        }));
    }
    None
}

fn valid_button_action_spec(action: &str) -> bool {
    let lower = action.trim().to_lowercase();
    lower == "reset"
        || lower == "print"
        || lower.starts_with("uri:")
        || lower.starts_with("submit:")
}

/// `createRadioField`: one field, one widget per option, top-down
/// option rects, PDF-name-safe unique on-states.
fn create_radio_field(doc: &mut Document, spec: &NewFieldSpec) -> AppResult<()> {
    let definition = spec.definition;
    let values: Vec<String> = if spec.options.is_empty() {
        vec!["1".to_string(), "2".to_string()]
    } else {
        spec.options.to_vec()
    };
    let mut used = HashSet::new();
    let on_states: Vec<String> = values
        .iter()
        .enumerate()
        .map(|(i, v)| sanitize_on_state(Some(v), i, &mut used))
        .collect();
    let rects = radio_option_rects(
        spec.rect,
        values.len(),
        definition.option_gap,
        definition.option_size,
    );

    let mut field = Dictionary::new();
    let ff = field_flags(definition, FF_RADIO);
    field.set("FT", "Btn");
    field.set("Ff", ff);
    field.set("T", Object::string_literal(spec.unique_name));
    field.set(
        "Opt",
        Object::Array(
            on_states
                .iter()
                .map(|s| Object::string_literal(s.clone()))
                .collect(),
        ),
    );
    if let Some(label) = definition.label.as_deref().filter(|l| !l.trim().is_empty()) {
        field.set("TU", Object::string_literal(label));
    }
    let field_id = doc.add_object(Object::Dictionary(field));

    let mut kids = Vec::new();
    for (i, state) in on_states.iter().enumerate() {
        let r = rects[i];
        let w = r[2];
        let h = r[3];
        let mut widget = Dictionary::new();
        widget.set("Type", "Annot");
        widget.set("Subtype", "Widget");
        widget.set("Rect", pdf_rect(r[0], r[1], w, h));
        widget.set("F", 4);
        widget.set("P", Object::Reference(spec.page));
        widget.set("Parent", Object::Reference(field_id));
        widget.set("AS", OFF_STATE);
        widget.set("AP", Object::Dictionary(radio_ap(doc, w, h, state)));
        if let Some(tooltip) = definition
            .tooltip
            .as_deref()
            .filter(|t| !t.trim().is_empty())
        {
            widget.set("TU", Object::string_literal(tooltip));
        }
        let widget_id = doc.add_object(Object::Dictionary(widget));
        kids.push(Object::Reference(widget_id));
        push_page_annots(doc, spec.page, Object::Reference(widget_id));
    }
    if let Ok(field_dict) = doc.get_dictionary_mut(field_id) {
        field_dict.set("Kids", Object::Array(kids));
        if let Some(value) = definition
            .default_value
            .as_deref()
            .filter(|v| !v.trim().is_empty())
            .filter(|v| on_states.iter().any(|s| s == v))
        {
            field_dict.set("V", Object::Name(value.as_bytes().to_vec()));
        }
    }
    push_acroform_field(doc, Object::Reference(field_id));
    Ok(())
}

fn add_fields_work(
    doc: &mut Document,
    definitions: &[FieldDefinition],
) -> AppResult<Vec<SkippedEdit>> {
    let mut skipped = Vec::new();
    let page_ids = pdfdoc::page_ids(doc);
    if page_ids.is_empty() {
        for definition in definitions {
            skipped.push(SkippedEdit {
                operation: "add",
                target: definition.name.clone().unwrap_or_default(),
                reason: "the document has no pages".into(),
            });
        }
        return Ok(skipped);
    }
    ensure_acroform(doc)?;
    ensure_acroform_defaults(doc);
    let mut existing = collect_existing_field_names(doc);

    for definition in definitions {
        if let Some(problem) = invalid_field_name_reason(definition.name.as_deref()) {
            skipped.push(SkippedEdit {
                operation: "add",
                target: definition.name.clone().unwrap_or_default(),
                reason: problem,
            });
            continue;
        }
        let resolved_type = normalize_field_type(definition.field_type.as_deref());
        let page_count = page_ids.len() as i64;
        let mut page_idx = definition.page_index.unwrap_or(0);
        if page_idx < 0 || page_idx >= page_count {
            page_idx = (page_count - 1).max(0);
        }
        let page = page_ids[page_idx as usize];
        let crop = page_crop_box(doc, page);
        let (x, y) = (
            definition.x.unwrap_or(0.0) + crop[0],
            definition.y.unwrap_or(0.0) + crop[1],
        );
        let (w, h) = (
            definition.width.unwrap_or(150.0),
            definition.height.unwrap_or(20.0),
        );
        let rect = if w <= 0.0 || h <= 0.0 {
            [100.0, 100.0, 100.0, 20.0]
        } else {
            [x, y, w, h]
        };
        let base_name = definition
            .name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("field");
        let unique_name = generate_unique_field_name(Some(base_name), &existing, "");
        existing.insert(unique_name.clone());
        let options = sanitize_options(definition.options.as_ref());

        if !creation_supported(&resolved_type) {
            skipped.push(SkippedEdit {
                operation: "add",
                target: unique_name.clone(),
                reason: format!("'{resolved_type}' fields cannot be created"),
            });
            existing.remove(&unique_name);
            continue;
        }
        let spec = NewFieldSpec {
            definition,
            resolved_type: resolved_type.as_str(),
            unique_name: &unique_name,
            page,
            rect,
            options: &options,
        };
        let result = if resolved_type == T_RADIO {
            create_radio_field(doc, &spec)
        } else {
            create_single_widget_field(doc, &spec)
        };
        if let Err(e) = result {
            skipped.push(SkippedEdit {
                operation: "add",
                target: unique_name.clone(),
                reason: format!("{e}"),
            });
            existing.remove(&unique_name);
        }
    }
    refresh_appearances(doc);
    Ok(skipped)
}

// ---------- in-place modification ----------

fn ff_set(doc: &mut Document, stored: &Object, bit: i64, on: bool) {
    let Ok(id) = stored.as_reference() else {
        return;
    };
    let ff = doc
        .get_dictionary(id)
        .ok()
        .and_then(|d| d.get(b"Ff").ok().cloned())
        .and_then(|o| int_of(doc, &o))
        .unwrap_or(0);
    let ff = if on { ff | bit } else { ff & !bit };
    if let Ok(dict) = doc.get_dictionary_mut(id) {
        dict.set("Ff", ff);
    }
}

fn widget_rect(doc: &Document, widget: &Widget) -> Option<[f64; 4]> {
    dict_value(doc, &widget.stored, b"Rect").and_then(|o| rect_parts(&o))
}

fn set_widget_rect(doc: &mut Document, widget: &Widget, rect: [f64; 4]) {
    let Ok(id) = widget.stored.as_reference() else {
        return;
    };
    if let Ok(dict) = doc.get_dictionary_mut(id) {
        dict.set("Rect", pdf_rect(rect[0], rect[1], rect[2], rect[3]));
    }
}

/// The page a widget annotates: `/P` first, then a scan of every page's
/// `/Annots` (the `repairMissingWidgetPageReferences` fallback).
fn find_widget_page(doc: &Document, widget: &Widget) -> Option<usize> {
    if let Some(id) = dict_value(doc, &widget.stored, b"P").and_then(|o| o.as_reference().ok()) {
        if let Some(i) = pdfdoc::page_ids(doc).iter().position(|p| *p == id) {
            return Some(i);
        }
    }
    let page_ids = pdfdoc::page_ids(doc);
    for (i, page_id) in page_ids.iter().enumerate() {
        let hit = doc
            .get_dictionary(*page_id)
            .ok()
            .and_then(|d| d.get(b"Annots").ok().cloned())
            .and_then(|o| resolved(doc, &o).cloned())
            .and_then(|o| match o {
                Object::Array(items) => Some(items),
                _ => None,
            })
            .is_some_and(|items| {
                items.iter().any(|item| {
                    item.as_reference()
                        .ok()
                        .zip(widget.stored.as_reference().ok())
                        .is_some_and(|(a, b)| a == b)
                })
            });
        if hit {
            return Some(i);
        }
    }
    None
}

/// On-state names a button-family field can take: every widget's /AP /N
/// keys minus Off, plus the inheritable /Opt exports.
fn widget_on_states(doc: &Document, field: &TerminalField) -> Vec<String> {
    let mut states = Vec::new();
    for widget in &field.widgets {
        if let Some(state) = widget_on_state(doc, widget) {
            if !states.contains(&state) {
                states.push(state);
            }
        }
    }
    for export in button_export_values(doc, field) {
        if !export.eq_ignore_ascii_case(OFF_STATE) && !states.contains(&export) {
            states.push(export);
        }
    }
    states
}

fn choice_kind(field: &TerminalField) -> bool {
    matches!(field.kind, FieldKind::Combobox | FieldKind::Listbox)
}

fn text_kind(field: &TerminalField) -> bool {
    field.kind == FieldKind::Text
}

fn button_kind(field: &TerminalField) -> bool {
    matches!(
        field.kind,
        FieldKind::Checkbox | FieldKind::Radio | FieldKind::Button
    )
}

/// `modifyFieldPropertiesInPlace`: property edits that never change the
/// field's type. Geometry moves widget 0 and shifts the rest by the same
/// delta, like `updateWidgetGeometry`.
fn modify_field_in_place(
    doc: &mut Document,
    field: &TerminalField,
    modification: &FieldDefinition,
    new_name: Option<&str>,
    skipped: &mut Vec<SkippedEdit>,
) -> AppResult<()> {
    let qualified = field.qualified.clone();
    if let Some(name) = new_name {
        if name != field.qualified.rsplit('.').next().unwrap_or("") {
            set_dict_value(doc, &field.stored, b"T", Object::string_literal(name));
        }
    }
    if let Some(label) = &modification.label {
        if label.trim().is_empty() {
            remove_dict_value(doc, &field.stored, b"TU");
        } else {
            set_dict_value(
                doc,
                &field.stored,
                b"TU",
                Object::string_literal(label.as_str()),
            );
        }
    }
    if let Some(required) = modification.required {
        ff_set(doc, &field.stored, FF_REQUIRED, required);
    }
    if let Some(value) = &modification.default_value {
        if value.trim().is_empty() {
            remove_dict_value(doc, &field.stored, b"V");
        } else if text_kind(field) || choice_kind(field) {
            set_dict_value(
                doc,
                &field.stored,
                b"V",
                Object::string_literal(value.as_str()),
            );
        } else if button_kind(field) {
            let states = widget_on_states(doc, field);
            if states.iter().any(|s| s == value.trim()) {
                set_dict_value(doc, &field.stored, b"V", pdf_name(value.trim()));
            }
        }
    }
    if let Some(options) = &modification.options {
        if choice_kind(field) {
            let clean = sanitize_options(Some(options));
            set_dict_value(
                doc,
                &field.stored,
                b"Opt",
                Object::Array(
                    clean
                        .iter()
                        .map(|o| Object::string_literal(o.clone()))
                        .collect(),
                ),
            );
        } else {
            skipped.push(SkippedEdit {
                operation: "modify",
                target: qualified.clone(),
                reason: "only dropdown and list fields have an editable option list".into(),
            });
        }
    }
    if let Some(multi) = modification.multi_select {
        if choice_kind(field) {
            ff_set(doc, &field.stored, FF_MULTI_SELECT, multi);
        }
    }
    if let Some(tooltip) = &modification.tooltip {
        for widget in &field.widgets {
            if tooltip.trim().is_empty() {
                remove_dict_value(doc, &widget.stored, b"TU");
            } else {
                set_dict_value(
                    doc,
                    &widget.stored,
                    b"TU",
                    Object::string_literal(tooltip.as_str()),
                );
            }
        }
    }
    if let Some(read_only) = modification.read_only {
        ff_set(doc, &field.stored, FF_READ_ONLY, read_only);
    }
    if let Some(multiline) = modification.multiline {
        if text_kind(field) {
            ff_set(doc, &field.stored, FF_MULTILINE, multiline);
        }
    }
    if let Some(action) = &modification.button_action {
        if field.kind == FieldKind::Button {
            if valid_button_action_spec(action) {
                for widget in &field.widgets {
                    match button_action_dict(Some(action)) {
                        Some(a) => set_dict_value(doc, &widget.stored, b"A", a),
                        None => remove_dict_value(doc, &widget.stored, b"A"),
                    }
                }
            } else {
                skipped.push(SkippedEdit {
                    operation: "modify",
                    target: qualified.clone(),
                    reason: format!("'{action}' is not a button action this editor understands"),
                });
            }
        }
    }
    if let Some(max_length) = modification.max_length {
        if text_kind(field) {
            if max_length > 0 {
                set_dict_value(doc, &field.stored, b"MaxLen", Object::Integer(max_length));
                let multiline = field
                    .chain
                    .iter()
                    .find_map(|d| dict_get(d, b"Ff"))
                    .and_then(|o| int_of(doc, o))
                    .is_some_and(|ff| ff_flag(ff, FF_MULTILINE));
                if !multiline {
                    ff_set(doc, &field.stored, FF_COMB, true);
                }
            } else {
                remove_dict_value(doc, &field.stored, b"MaxLen");
                ff_set(doc, &field.stored, FF_COMB, false);
            }
        }
    }
    if let Some(size) = modification.font_size {
        if size > 0.0
            && matches!(
                field.kind,
                FieldKind::Text | FieldKind::Combobox | FieldKind::Listbox
            )
        {
            set_dict_value(
                doc,
                &field.stored,
                b"DA",
                Object::string_literal(format!("/Helv {size} Tf 0 g")),
            );
            for widget in &field.widgets {
                remove_dict_value(doc, &widget.stored, b"AP");
            }
        }
    }
    let geometry = modification.x.is_some()
        || modification.y.is_some()
        || modification.width.is_some()
        || modification.height.is_some()
        || modification.option_gap.is_some()
        || modification.option_size.is_some();
    if geometry {
        update_widget_geometry(doc, field, modification, skipped)?;
    }
    Ok(())
}

/// `updateWidgetGeometry`: widget 0 takes the requested size, every
/// widget shifts by the anchor delta; a radio group reflows only when
/// spacing/size is explicitly given.
fn update_widget_geometry(
    doc: &mut Document,
    field: &TerminalField,
    modification: &FieldDefinition,
    skipped: &mut Vec<SkippedEdit>,
) -> AppResult<()> {
    let Some(anchor) = field.widgets.first() else {
        return Ok(());
    };
    let Some(anchor_rect) = widget_rect(doc, anchor) else {
        return Ok(());
    };
    let unusable = |v: Option<f64>| v.is_some_and(|n| !n.is_finite() || n <= 0.0);
    if unusable(modification.width) || unusable(modification.height) {
        skipped.push(SkippedEdit {
            operation: "modify",
            target: field.qualified.clone(),
            reason: "a width and height above zero are required, so the size was left as it was"
                .into(),
        });
        return Ok(());
    }
    if modification.x.is_some_and(|v| !v.is_finite())
        || modification.y.is_some_and(|v| !v.is_finite())
    {
        skipped.push(SkippedEdit {
            operation: "modify",
            target: field.qualified.clone(),
            reason: "the position is not a usable number, so the field was left where it was"
                .into(),
        });
        return Ok(());
    }
    let anchor_page = find_widget_page(doc, anchor).unwrap_or(0);
    let crop = pdfdoc::page_ids(doc)
        .get(anchor_page)
        .map(|id| page_crop_box(doc, *id))
        .unwrap_or([0.0, 0.0, 0.0, 0.0]);
    let new_x = modification.x.unwrap_or(anchor_rect[0]) + crop[0];
    let new_y = modification.y.unwrap_or(anchor_rect[1]) + crop[1];
    let (dx, dy) = (new_x - anchor_rect[0], new_y - anchor_rect[1]);

    if (modification.option_gap.is_some() || modification.option_size.is_some())
        && field.kind == FieldKind::Radio
        && field.widgets.len() > 1
    {
        let mut bounds = anchor_rect;
        for w in &field.widgets[1..] {
            if let Some(r) = widget_rect(doc, w) {
                bounds[0] = bounds[0].min(r[0]);
                bounds[1] = bounds[1].min(r[1]);
                bounds[2] = (bounds[0] + bounds[2]).max(r[0] + r[2]) - bounds[0];
                bounds[3] = (bounds[1] + bounds[3]).max(r[1] + r[3]) - bounds[1];
            }
        }
        let box_ = [
            bounds[0] + dx,
            bounds[1] + dy,
            modification.width.unwrap_or(bounds[2]),
            modification.height.unwrap_or(bounds[3]),
        ];
        let rects = radio_option_rects(
            box_,
            field.widgets.len(),
            modification.option_gap,
            modification.option_size,
        );
        for (widget, r) in field.widgets.iter().zip(rects) {
            set_widget_rect(doc, widget, r);
        }
        return Ok(());
    }

    for (i, widget) in field.widgets.iter().enumerate() {
        let Some(rect) = widget_rect(doc, widget) else {
            continue;
        };
        let w = if i == 0 {
            modification.width.unwrap_or(rect[2])
        } else {
            rect[2]
        };
        let h = if i == 0 {
            modification.height.unwrap_or(rect[3])
        } else {
            rect[3]
        };
        set_widget_rect(doc, widget, [rect[0] + dx, rect[1] + dy, w, h]);
    }
    Ok(())
}

// ---------- modify ----------

/// `modifyFormFields`: locate, rename-check, in-place property edits, or
/// delete-and-recreate on a type change (never for nested fields).
fn modify_fields_work(
    doc: &mut Document,
    modifications: &[FieldDefinition],
) -> AppResult<Vec<SkippedEdit>> {
    let mut skipped = Vec::new();
    if acroform_slot(doc).is_none() {
        for modification in modifications {
            skipped.push(SkippedEdit {
                operation: "modify",
                target: modification.target_name.clone().unwrap_or_default(),
                reason: "the document has no form to modify".into(),
            });
        }
        return Ok(skipped);
    }
    ensure_acroform_defaults(doc);
    let mut existing = collect_existing_field_names(doc);

    for modification in modifications {
        let lookup = modification
            .target_name
            .as_deref()
            .map(str::trim)
            .unwrap_or("");
        if lookup.is_empty() {
            skipped.push(SkippedEdit {
                operation: "modify",
                target: String::new(),
                reason: "the request named no field to change".into(),
            });
            continue;
        }
        if let Some(problem) = rename_problem(lookup, modification.name.as_deref()) {
            skipped.push(SkippedEdit {
                operation: "modify",
                target: lookup.to_string(),
                reason: problem,
            });
            continue;
        }
        let fields = terminal_fields(doc)?;
        let Some(field) = find_terminal(&fields, lookup) else {
            skipped.push(SkippedEdit {
                operation: "modify",
                target: lookup.to_string(),
                reason: "no field with that name exists".into(),
            });
            continue;
        };
        if field.widgets.is_empty() {
            skipped.push(SkippedEdit {
                operation: "modify",
                target: lookup.to_string(),
                reason: "the field has nothing drawn on a page".into(),
            });
            continue;
        }
        let Some(first_widget) = field.widgets.first() else {
            continue;
        };
        let Some(anchor_rect) = widget_rect(doc, first_widget) else {
            skipped.push(SkippedEdit {
                operation: "modify",
                target: lookup.to_string(),
                reason: "the field is not placed on a known page".into(),
            });
            continue;
        };
        let anchor_page_idx = find_widget_page(doc, first_widget).unwrap_or(0);
        let page = pdfdoc::page_ids(doc).get(anchor_page_idx).copied();

        let resolved_type = match modification.field_type.as_deref() {
            Some(t) => normalize_field_type(Some(t)),
            None => detect_field_type_name(field.kind).to_string(),
        };
        const SUPPORTED: [&str; 7] = [
            "text",
            "checkbox",
            "combobox",
            "listbox",
            "radio",
            "button",
            "signature",
        ];
        if !SUPPORTED.contains(&resolved_type.as_str()) {
            skipped.push(SkippedEdit {
                operation: "modify",
                target: lookup.to_string(),
                reason: format!(
                    "'{}' is not a supported field type",
                    abbreviate(&resolved_type, 60)
                ),
            });
            continue;
        }

        let current_type = detect_field_type_name(field.kind).to_string();
        let type_changing = current_type != resolved_type;
        let qualified = field.qualified.clone();
        let prefix = parent_prefix(&qualified).to_string();

        let desired_name = modification
            .name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .filter(|name| *name != lookup)
            .map(|name| leaf_name(lookup, name).to_string())
            .unwrap_or_else(|| field.qualified.rsplit('.').next().unwrap_or("").to_string());

        existing.remove(&qualified);
        let unique_desired = generate_unique_field_name(Some(&desired_name), &existing, &prefix);
        let reserved = format!("{prefix}{unique_desired}");
        existing.insert(reserved.clone());

        if !type_changing {
            // Java renames whenever desiredName differs from the current
            // partial name.
            let partial = field.qualified.rsplit('.').next().unwrap_or("").to_string();
            let renamed = if unique_desired != partial {
                Some(unique_desired.as_str())
            } else {
                None
            };
            match modify_field_in_place(doc, field, modification, renamed, &mut skipped) {
                Ok(()) => {
                    // The rename changed /T; keep the name set honest for
                    // later modifications in the same batch.
                    existing.remove(&qualified);
                    existing.insert(format!("{prefix}{unique_desired}"));
                    continue;
                }
                Err(e) => {
                    skipped.push(SkippedEdit {
                        operation: "modify",
                        target: lookup.to_string(),
                        reason: format!("{e}"),
                    });
                    existing.remove(&reserved);
                    existing.insert(qualified.clone());
                    continue;
                }
            }
        }

        if !prefix.is_empty() {
            skipped.push(SkippedEdit {
                operation: "modify",
                target: lookup.to_string(),
                reason: if type_changing {
                    "a field nested under a parent cannot have its type changed here".into()
                } else {
                    "this change needs the field rebuilt, which a nested field does not support"
                        .into()
                },
            });
            existing.remove(&reserved);
            existing.insert(qualified.clone());
            continue;
        }

        if !creation_supported(&resolved_type) {
            skipped.push(SkippedEdit {
                operation: "modify",
                target: lookup.to_string(),
                reason: format!(
                    "'{resolved_type}' cannot be rebuilt, so the field was left as it was"
                ),
            });
            existing.remove(&reserved);
            existing.insert(qualified.clone());
            continue;
        }

        // Recreate: build the replacement at the original widget geometry
        // first so a failure leaves the original untouched.
        let Some(page) = page else {
            skipped.push(SkippedEdit {
                operation: "modify",
                target: lookup.to_string(),
                reason: "the field is not placed on a known page".into(),
            });
            existing.remove(&reserved);
            existing.insert(qualified.clone());
            continue;
        };
        let crop = page_crop_box(doc, page);
        let replacement = FieldDefinition {
            name: Some(unique_desired.clone()),
            label: modification.label.clone(),
            field_type: Some(resolved_type.clone()),
            page_index: Some(anchor_page_idx as i64),
            x: Some(anchor_rect[0] - crop[0]),
            y: Some(anchor_rect[1] - crop[1]),
            width: Some(anchor_rect[2]),
            height: Some(anchor_rect[3]),
            required: modification.required,
            multi_select: modification.multi_select,
            options: modification.options.clone(),
            default_value: modification.default_value.clone(),
            tooltip: modification.tooltip.clone(),
            font_size: modification.font_size,
            read_only: modification.read_only,
            multiline: modification.multiline,
            max_length: modification.max_length,
            button_action: modification.button_action.clone(),
            option_gap: modification.option_gap,
            option_size: modification.option_size,
            target_name: None,
        };
        let options = sanitize_options(modification.options.as_ref());
        let spec = NewFieldSpec {
            definition: &replacement,
            resolved_type: &resolved_type,
            unique_name: &unique_desired,
            page,
            rect: [
                anchor_rect[0],
                anchor_rect[1],
                anchor_rect[2],
                anchor_rect[3],
            ],
            options: &options,
        };
        let created = if resolved_type == T_RADIO {
            create_radio_field(doc, &spec)
        } else {
            create_single_widget_field(doc, &spec)
        };
        if let Err(e) = created {
            skipped.push(SkippedEdit {
                operation: "modify",
                target: lookup.to_string(),
                reason: format!("{e}"),
            });
            existing.remove(&reserved);
            existing.insert(qualified.clone());
            continue;
        }
        let _ = delete_fields(doc, std::slice::from_ref(&qualified))?;
    }
    refresh_appearances(doc);
    Ok(skipped)
}

/// `applyFieldEdits`: modify, then delete, then add — so generated
/// names dedupe against the surviving set.
fn apply_field_edits_work(
    doc: &mut Document,
    batch: &FieldEditBatch,
) -> AppResult<Vec<SkippedEdit>> {
    let mut skipped = Vec::new();
    if !batch.modify.is_empty() {
        skipped.extend(modify_fields_work(doc, &batch.modify)?);
    }
    if let Some(delete) = &batch.delete {
        let names = parse_name_list(&delete.to_string())?;
        if !names.is_empty() {
            skipped.extend(delete_fields(doc, &names)?);
        }
    }
    if !batch.add.is_empty() {
        skipped.extend(add_fields_work(doc, &batch.add)?);
    }
    Ok(skipped)
}

// ---------- endpoints: add / edit / modify ----------

fn part_text(upload: Option<&Upload>, params: &[(String, String)], name: &str) -> Option<String> {
    if let Some(u) = upload {
        if !u.bytes.is_empty() {
            return Some(String::from_utf8_lossy(&u.bytes).into_owned());
        }
    }
    params
        .iter()
        .rev()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.clone())
}

fn require_usable_names(adds: &[FieldDefinition], modifies: &[FieldDefinition]) -> AppResult<()> {
    let problem = adds
        .iter()
        .filter_map(|d| invalid_field_name_reason(d.name.as_deref()))
        .chain(modifies.iter().filter_map(|m| {
            let lookup = m.target_name.as_deref().unwrap_or("").trim();
            rename_problem(lookup, m.name.as_deref())
        }))
        .next();
    if let Some(problem) = problem {
        return Err(AppError::BadRequest(problem));
    }
    Ok(())
}

fn parse_definitions(text: &str) -> AppResult<Vec<FieldDefinition>> {
    serde_json::from_str(text)
        .map_err(|e| AppError::BadRequest(format!("invalid field definitions JSON: {e}")))
}

/// The `?includeFields=true` bundle: `fields.json` (the coordinates
/// extraction) deflated, `document.pdf` stored with size+CRC — the same
/// layout `writeFieldBundle` produces.
fn field_bundle(pdf_bytes: &[u8], fields_json: &[u8]) -> AppResult<Vec<u8>> {
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    let mut zip = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    zip.start_file(
        "fields.json",
        SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
    )
    .map_err(|e| AppError::Internal(anyhow::anyhow!("zip: {e}")))?;
    zip.write_all(fields_json)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("zip: {e}")))?;
    let stored = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Stored)
        .last_modified_time(zip::DateTime::default());
    // The `zip` crate computes CRC/sizes itself for Stored entries.
    zip.start_file("document.pdf", stored)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("zip: {e}")))?;
    zip.write_all(pdf_bytes)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("zip: {e}")))?;
    let bytes = zip
        .finish()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("zip: {e}")))?
        .into_inner();
    Ok(bytes)
}

/// `POST /api/v1/form/add-fields` → `name_updated.pdf`.
#[stirling_macros::endpoint(receive = "receive_form_parts")]
fn add_fields_endpoint(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    extras: Vec<(String, Upload)>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let text = part_text(extra(&extras, "fields"), &params, "fields").unwrap_or_default();
    let definitions = parse_definitions(&text)?;
    if definitions.is_empty() {
        return Err(AppError::BadRequest(
            "fields payload must contain at least one definition".into(),
        ));
    }
    require_usable_names(&definitions, &[])?;
    let upload = uploads.into_iter().next().unwrap();
    let mut document = load_document(&upload)?;
    let skipped = add_fields_work(&mut document, &definitions)?;
    pdfdoc::touch_loaded_info(&mut document);
    let bytes = pdfdoc::save(&mut document)?;
    let response = pdf_response(bytes, &pdf_filename(&upload.name, "updated"));
    Ok(with_skipped_edits(response, &skipped))
}

/// `POST /api/v1/form/modify-fields` → `name_updated.pdf`.
#[stirling_macros::endpoint(receive = "receive_form_parts")]
fn modify_fields_endpoint(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    extras: Vec<(String, Upload)>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    // Java binds this part as `updates`; accept `fields` too so both
    // spellings of the JSON part work.
    let fields = extra(&extras, "fields");
    let text = part_text(fields, &params, "updates")
        .or_else(|| part_text(fields, &params, "fields"))
        .unwrap_or_default();
    let modifications = parse_definitions(&text)?;
    if modifications.is_empty() {
        return Err(AppError::BadRequest(
            "updates payload must contain at least one definition".into(),
        ));
    }
    require_usable_names(&[], &modifications)?;
    let upload = uploads.into_iter().next().unwrap();
    let mut document = load_document(&upload)?;
    let skipped = modify_fields_work(&mut document, &modifications)?;
    pdfdoc::touch_loaded_info(&mut document);
    let bytes = pdfdoc::save(&mut document)?;
    let response = pdf_response(bytes, &pdf_filename(&upload.name, "updated"));
    Ok(with_skipped_edits(response, &skipped))
}

/// `POST /api/v1/form/edit-fields` → `name_updated.pdf`, or the
/// fields.json+document.pdf ZIP bundle when `includeFields=true`.
#[stirling_macros::endpoint(receive = "receive_form_parts")]
fn edit_fields_endpoint(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    extras: Vec<(String, Upload)>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let include_fields = crate::upload::param_bool(&params, "includeFields");
    let text = part_text(extra(&extras, "fields"), &params, "edits").unwrap_or_default();
    let batch: FieldEditBatch = serde_json::from_str(&text)
        .map_err(|e| AppError::BadRequest(format!("invalid edits payload JSON: {e}")))?;
    let delete_names = match &batch.delete {
        Some(v) => parse_name_list(&v.to_string())?,
        None => Vec::new(),
    };
    if batch.add.is_empty() && batch.modify.is_empty() && delete_names.is_empty() {
        return Err(AppError::BadRequest(
            "edits payload must contain at least one edit".into(),
        ));
    }
    require_usable_names(&batch.add, &batch.modify)?;
    let mut upload = uploads.into_iter().next().unwrap();
    let mut document = load_document(&upload)?;
    let mut batch = batch;
    batch.delete = Some(json!(delete_names));
    let skipped = apply_field_edits_work(&mut document, &batch)?;
    pdfdoc::touch_loaded_info(&mut document);
    let bytes = pdfdoc::save(&mut document)?;
    if include_fields {
        let mut entries = extract_entries(&document, false)?;
        sort_coords_entries(&mut entries);
        let fields_json = serde_json::to_vec(
            &entries
                .iter()
                .map(|e| serde_json::to_value(&e.coords).unwrap_or(Value::Null))
                .collect::<Vec<_>>(),
        )
        .unwrap_or_default();
        let bundle = field_bundle(&bytes, &fields_json)?;
        upload.name = bundle_name(&upload.name);
        let response = file_response(bundle, &upload.name, "application/zip");
        return Ok(with_skipped_edits(response, &skipped));
    }
    let response = pdf_response(bytes, &pdf_filename(&upload.name, "updated"));
    Ok(with_skipped_edits(response, &skipped))
}

fn bundle_name(original: &str) -> String {
    let base = original
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("document");
    let stem = if base.to_lowercase().ends_with(".pdf") {
        &base[..base.len() - 4]
    } else {
        base
    };
    format!("{stem}_updated.zip")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit;
    use lopdf::dictionary;

    fn ap_states(states: &[&str]) -> Dictionary {
        let mut ap = Dictionary::new();
        let mut n = Dictionary::new();
        for s in states {
            n.set(*s, Object::Null);
        }
        ap.set("N", Object::Dictionary(n));
        ap
    }

    fn load(pdf: &mut testkit::Pdf) -> Document {
        Document::load_mem(&pdf.bytes()).unwrap()
    }

    fn field_v(doc: &Document, name: &str) -> Option<Object> {
        terminal_fields(doc)
            .unwrap()
            .into_iter()
            .find(|f| f.qualified == name)
            .and_then(|f| {
                f.stored
                    .as_reference()
                    .ok()
                    .and_then(|id| doc.get_dictionary(id).ok().cloned())
            })
            .and_then(|d| d.get(b"V").ok().cloned())
    }

    fn widget_as(doc: &Document, field_name: &str, index: usize) -> Option<String> {
        let field = terminal_fields(doc)
            .unwrap()
            .into_iter()
            .find(|f| f.qualified == field_name)?;
        let w = field.widgets.get(index)?;
        let id = w.stored.as_reference().ok()?;
        let d = doc.get_dictionary(id).ok()?;
        d.get(b"AS")
            .ok()
            .and_then(|o| o.as_name().ok())
            .map(|n| String::from_utf8_lossy(n).into_owned())
    }

    fn need_appearances(doc: &Document) -> bool {
        pdfdoc::acroform(doc)
            .and_then(|f| f.get(b"NeedAppearances").ok().cloned())
            .is_some_and(|o| matches!(o, Object::Boolean(true)))
    }

    #[test]
    fn fill_text_sets_v_and_regenerates_appearance() {
        let mut pdf = testkit::Pdf::new();
        let widget = pdf.add(dictionary! {
            "Type" => "Annot", "Subtype" => "Widget",
            "Rect" => Object::Array(vec![0.into(), 0.into(), 100.into(), 20.into()]),
            "P" => Object::Reference(pdf.page),
        });
        let field = pdf.add(dictionary! {
            "FT" => "Tx", "T" => Object::string_literal("name"),
            "Kids" => vec![Object::Reference(widget)],
        });
        let acro = pdf.add(dictionary! { "Fields" => vec![Object::Reference(field)] });
        pdf.edit_catalog(|c| c.set("AcroForm", Object::Reference(acro)));
        pdf.edit_page(|p| p.set("Annots", vec![Object::Reference(widget)]));

        let mut doc = load(&mut pdf);
        let values = parse_value_map(r#"{"name":"Alice"}"#).unwrap();
        apply_field_values(&mut doc, &values, false, true).unwrap();

        match field_v(&doc, "name") {
            Some(Object::String(bytes, _)) => {
                assert_eq!(String::from_utf8_lossy(&bytes), "Alice")
            }
            other => panic!("unexpected /V: {other:?}"),
        }
        // Single-line text gets a real /AP from refresh_appearances, so
        // the viewer-regeneration flag stays clear.
        assert!(!need_appearances(&doc));
    }

    #[test]
    fn fill_checkbox_checks_and_unchecks() {
        let mut pdf = testkit::Pdf::new();
        let widget = pdf.add(dictionary! {
            "Type" => "Annot", "Subtype" => "Widget",
            "Rect" => Object::Array(vec![0.into(), 0.into(), 20.into(), 20.into()]),
            "P" => Object::Reference(pdf.page),
            "AP" => ap_states(&["Yes", "Off"]),
        });
        let field = pdf.add(dictionary! {
            "FT" => "Btn", "T" => Object::string_literal("agree"),
            "Kids" => vec![Object::Reference(widget)],
        });
        let acro = pdf.add(dictionary! { "Fields" => vec![Object::Reference(field)] });
        pdf.edit_catalog(|c| c.set("AcroForm", Object::Reference(acro)));
        pdf.edit_page(|p| p.set("Annots", vec![Object::Reference(widget)]));

        let mut doc = load(&mut pdf);
        let values = parse_value_map(r#"{"agree":true}"#).unwrap();
        apply_field_values(&mut doc, &values, false, true).unwrap();
        assert!(matches!(field_v(&doc, "agree"), Some(Object::Name(n)) if n == b"Yes"));
        assert_eq!(widget_as(&doc, "agree", 0).as_deref(), Some("Yes"));

        let values = parse_value_map(r#"{"agree":""}"#).unwrap();
        apply_field_values(&mut doc, &values, false, true).unwrap();
        assert!(matches!(field_v(&doc, "agree"), Some(Object::Name(n)) if n == b"Off"));
        assert_eq!(widget_as(&doc, "agree", 0).as_deref(), Some("Off"));
    }

    #[test]
    fn fill_radio_selects_matching_widget() {
        let mut pdf = testkit::Pdf::new();
        let w1 = pdf.add(dictionary! {
            "Type" => "Annot", "Subtype" => "Widget",
            "Rect" => Object::Array(vec![0.into(), 0.into(), 20.into(), 20.into()]),
            "P" => Object::Reference(pdf.page),
            "AP" => ap_states(&["A", "Off"]),
        });
        let w2 = pdf.add(dictionary! {
            "Type" => "Annot", "Subtype" => "Widget",
            "Rect" => Object::Array(vec![30.into(), 0.into(), 50.into(), 20.into()]),
            "P" => Object::Reference(pdf.page),
            "AP" => ap_states(&["B", "Off"]),
        });
        let field = pdf.add(dictionary! {
            "FT" => "Btn", "Ff" => 1 << 15, "T" => Object::string_literal("choice"),
            "Opt" => Object::Array(vec![Object::string_literal("A"), Object::string_literal("B")]),
            "Kids" => vec![Object::Reference(w1), Object::Reference(w2)],
        });
        let acro = pdf.add(dictionary! { "Fields" => vec![Object::Reference(field)] });
        pdf.edit_catalog(|c| c.set("AcroForm", Object::Reference(acro)));
        pdf.edit_page(|p| p.set("Annots", vec![Object::Reference(w1), Object::Reference(w2)]));

        let mut doc = load(&mut pdf);
        let values = parse_value_map(r#"{"choice":"B"}"#).unwrap();
        apply_field_values(&mut doc, &values, false, true).unwrap();
        assert!(matches!(field_v(&doc, "choice"), Some(Object::Name(n)) if n == b"B"));
        assert_eq!(widget_as(&doc, "choice", 0).as_deref(), Some("Off"));
        assert_eq!(widget_as(&doc, "choice", 1).as_deref(), Some("B"));
    }

    #[test]
    fn fill_choice_filters_against_opt() {
        let mut pdf = testkit::Pdf::new();
        let widget = pdf.add(dictionary! {
            "Type" => "Annot", "Subtype" => "Widget",
            "Rect" => Object::Array(vec![0.into(), 0.into(), 100.into(), 20.into()]),
            "P" => Object::Reference(pdf.page),
        });
        let field = pdf.add(dictionary! {
            "FT" => "Ch", "Ff" => 1 << 17, "T" => Object::string_literal("pick"),
            "Opt" => Object::Array(vec![
                Object::Array(vec![Object::string_literal("e1"), Object::string_literal("d1")]),
                Object::Array(vec![Object::string_literal("e2"), Object::string_literal("d2")]),
            ]),
            "Kids" => vec![Object::Reference(widget)],
        });
        let acro = pdf.add(dictionary! { "Fields" => vec![Object::Reference(field)] });
        pdf.edit_catalog(|c| c.set("AcroForm", Object::Reference(acro)));
        pdf.edit_page(|p| p.set("Annots", vec![Object::Reference(widget)]));

        let mut doc = load(&mut pdf);
        let values = parse_value_map(r#"{"pick":"D2"}"#).unwrap();
        apply_field_values(&mut doc, &values, false, true).unwrap();
        match field_v(&doc, "pick") {
            Some(Object::String(bytes, _)) => {
                assert_eq!(String::from_utf8_lossy(&bytes), "d2")
            }
            other => panic!("unexpected /V: {other:?}"),
        }
        // A value outside /Opt yields the empty selection, not a write.
        let values = parse_value_map(r#"{"pick":"bogus"}"#).unwrap();
        apply_field_values(&mut doc, &values, false, true).unwrap();
        match field_v(&doc, "pick") {
            Some(Object::String(bytes, _)) => assert!(bytes.is_empty()),
            other => panic!("unexpected /V: {other:?}"),
        }
    }

    #[test]
    fn delete_fields_detaches_tree_and_annots() {
        let mut pdf = testkit::Pdf::new();
        let w1 = pdf.add(dictionary! {
            "Type" => "Annot", "Subtype" => "Widget",
            "Rect" => Object::Array(vec![0.into(), 0.into(), 20.into(), 20.into()]),
            "P" => Object::Reference(pdf.page),
        });
        let w2 = pdf.add(dictionary! {
            "Type" => "Annot", "Subtype" => "Widget",
            "Rect" => Object::Array(vec![30.into(), 0.into(), 50.into(), 20.into()]),
            "P" => Object::Reference(pdf.page),
        });
        let f1 = pdf.add(dictionary! {
            "FT" => "Tx", "T" => Object::string_literal("keep"),
            "Kids" => vec![Object::Reference(w1)],
        });
        let f2 = pdf.add(dictionary! {
            "FT" => "Tx", "T" => Object::string_literal("drop"),
            "Kids" => vec![Object::Reference(w2)],
        });
        let acro = pdf.add(dictionary! {
            "Fields" => vec![Object::Reference(f1), Object::Reference(f2)],
        });
        pdf.edit_catalog(|c| c.set("AcroForm", Object::Reference(acro)));
        pdf.edit_page(|p| {
            p.set("Annots", vec![Object::Reference(w1), Object::Reference(w2)]);
        });

        let mut doc = load(&mut pdf);
        let skipped = delete_fields(&mut doc, &["drop".to_string()]).unwrap();
        assert!(skipped.is_empty());
        let names: Vec<String> = terminal_fields(&doc)
            .unwrap()
            .iter()
            .map(|f| f.qualified.clone())
            .collect();
        assert_eq!(names, vec!["keep".to_string()]);
        let page = doc.get_dictionary(pdf.page).unwrap();
        let annots = page.get(b"Annots").unwrap();
        let items = annots.as_array().unwrap();
        assert_eq!(items.len(), 1);

        let skipped = delete_fields(&mut doc, &["missing".to_string()]).unwrap();
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].reason, "no field with that name exists");
    }

    #[test]
    fn no_acroform_is_a_noop() {
        let mut pdf = testkit::Pdf::new();
        let mut doc = load(&mut pdf);
        let values = parse_value_map(r#"{"x":"y"}"#).unwrap();
        // Java's applyFieldValues: strict callers (/fill) get the
        // "No AcroForm present" IOException; non-strict callers skip.
        apply_field_values(&mut doc, &values, false, false).unwrap();
        assert!(apply_field_values(&mut doc, &values, false, true).is_err());
        let skipped = delete_fields(&mut doc, &["x".to_string()]).unwrap();
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].reason, "the document has no form to delete from");
    }

    #[test]
    fn payload_parsers_match_java_shapes() {
        // template object wins over a flat map
        let v = parse_value_map(r#"{"template":{"a":"1"},"b":"2"}"#).unwrap();
        assert_eq!(v, vec![("a".to_string(), Some("1".to_string()))]);
        // fields array extracts name+value
        let v = parse_value_map(r#"{"fields":[{"name":"f","value":"v"}]}"#).unwrap();
        assert_eq!(v, vec![("f".to_string(), Some("v".to_string()))]);
        // defaultValue fallback, array join, number/bool coercion
        let v = parse_value_map(r#"{"a":["x","y"],"b":3,"c":true}"#).unwrap();
        assert_eq!(
            v,
            vec![
                ("a".to_string(), Some("[\"x\",\"y\"]".to_string())),
                ("b".to_string(), Some("3".to_string())),
                ("c".to_string(), Some("true".to_string())),
            ]
        );
        // name list: strings, objects, fields wrapper, bare string
        assert_eq!(
            parse_name_list(r#"["a",{"name":"b"},5]"#).unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(
            parse_name_list(r#"{"fields":["x","y"]}"#).unwrap(),
            vec!["x".to_string(), "y".to_string()]
        );
        assert_eq!(
            parse_name_list(r#""solo""#).unwrap(),
            vec!["solo".to_string()]
        );
        assert!(parse_name_list("").unwrap().is_empty());
    }

    fn def_json(v: &str) -> Vec<FieldDefinition> {
        serde_json::from_str(v).unwrap()
    }

    #[test]
    fn add_fields_creates_text_checkbox_and_radio() {
        let mut pdf = testkit::Pdf::new();
        let mut doc = load(&mut pdf);
        let defs = def_json(
            r#"[
            {"name":"t1","type":"text","pageIndex":0,"x":10,"y":20,"width":100,"height":20,"defaultValue":"hi"},
            {"name":"c1","type":"checkbox"},
            {"name":"r1","type":"radio","options":["a","b"],"defaultValue":"b"}
        ]"#,
        );
        let skipped = add_fields_work(&mut doc, &defs).unwrap();
        assert!(skipped.is_empty(), "skipped: {skipped:?}");
        let fields = terminal_fields(&doc).unwrap();
        let names: Vec<&str> = fields.iter().map(|f| f.qualified.as_str()).collect();
        assert!(names.contains(&"t1") && names.contains(&"c1") && names.contains(&"r1"));
        // text default landed in /V
        let t1 = fields.iter().find(|f| f.qualified == "t1").unwrap();
        let v = dict_value(&doc, &t1.stored, b"V").unwrap();
        assert!(matches!(v, Object::String(..)));
        // radio: field with two widget kids, /V = on-state name
        let r1 = fields.iter().find(|f| f.qualified == "r1").unwrap();
        assert_eq!(r1.widgets.len(), 2);
        let rv = dict_value(&doc, &r1.stored, b"V").unwrap();
        assert!(matches!(rv, Object::Name(_)));
        // every widget is on a page's /Annots
        for w in &r1.widgets {
            let id = w.stored.as_reference().unwrap();
            let on_page = pdfdoc::page_ids(&doc).iter().any(|p| {
                doc.get_dictionary(*p)
                    .ok()
                    .and_then(|d| d.get(b"Annots").ok().cloned())
                    .and_then(|o| resolved(&doc, &o).cloned())
                    .and_then(|o| match o {
                        Object::Array(a) => Some(a),
                        _ => None,
                    })
                    .is_some_and(|a| a.iter().any(|i| i.as_reference().ok() == Some(id)))
            });
            assert!(on_page);
        }
    }

    #[test]
    fn add_fields_rejects_bad_names_and_empty_pages() {
        let mut pdf = testkit::Pdf::new();
        let mut doc = load(&mut pdf);
        let defs = def_json(r#"[{"name":"bad.name","type":"text"},{"type":"signature"}]"#);
        let skipped = add_fields_work(&mut doc, &defs).unwrap();
        assert_eq!(skipped.len(), 2);
        assert!(skipped[0].reason.contains("cannot contain a period"));
        assert_eq!(skipped[1].reason, "'signature' fields cannot be created");
    }

    #[test]
    fn modify_fields_renames_and_reflags() {
        let mut pdf = testkit::Pdf::new();
        let widget = pdf.add(dictionary! {
            "Type" => "Annot", "Subtype" => "Widget",
            "Rect" => Object::Array(vec![0.into(), 0.into(), 100.into(), 20.into()]),
            "P" => Object::Reference(pdf.page),
        });
        let field = pdf.add(dictionary! {
            "FT" => "Tx", "T" => Object::string_literal("old"),
            "Kids" => vec![Object::Reference(widget)],
        });
        let acro = pdf.add(dictionary! { "Fields" => vec![Object::Reference(field)] });
        pdf.edit_catalog(|c| c.set("AcroForm", Object::Reference(acro)));
        pdf.edit_page(|p| p.set("Annots", vec![Object::Reference(widget)]));

        let mut doc = load(&mut pdf);
        let mods =
            def_json(r#"[{"targetName":"old","name":"new","required":true,"defaultValue":"v"}]"#);
        let skipped = modify_fields_work(&mut doc, &mods).unwrap();
        assert!(skipped.is_empty(), "skipped: {skipped:?}");
        let fields = terminal_fields(&doc).unwrap();
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].qualified, "new");
        assert!(ff_flag(fields[0].ff, FF_REQUIRED));
        assert!(matches!(
            dict_value(&doc, &fields[0].stored, b"V"),
            Some(Object::String(..))
        ));

        // type change recreates: text -> checkbox yields a Btn field
        let mods = def_json(r#"[{"targetName":"new","type":"checkbox"}]"#);
        let skipped = modify_fields_work(&mut doc, &mods).unwrap();
        assert!(skipped.is_empty(), "skipped: {skipped:?}");
        let fields = terminal_fields(&doc).unwrap();
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].kind, FieldKind::Checkbox);
    }

    #[test]
    fn edit_batch_orders_modify_delete_add() {
        let mut pdf = testkit::Pdf::new();
        let widget = pdf.add(dictionary! {
            "Type" => "Annot", "Subtype" => "Widget",
            "Rect" => Object::Array(vec![0.into(), 0.into(), 100.into(), 20.into()]),
            "P" => Object::Reference(pdf.page),
        });
        let field = pdf.add(dictionary! {
            "FT" => "Tx", "T" => Object::string_literal("victim"),
            "Kids" => vec![Object::Reference(widget)],
        });
        let acro = pdf.add(dictionary! { "Fields" => vec![Object::Reference(field)] });
        pdf.edit_catalog(|c| c.set("AcroForm", Object::Reference(acro)));
        pdf.edit_page(|p| p.set("Annots", vec![Object::Reference(widget)]));

        let mut doc = load(&mut pdf);
        // add reuses the soon-to-be-deleted name: modify+delete run first,
        // so "victim" is free and no _1 suffix appears.
        let batch: FieldEditBatch = serde_json::from_str(
            r#"{
            "add":[{"name":"victim","type":"text"}],
            "modify":[],
            "delete":["victim"]
        }"#,
        )
        .unwrap();
        let skipped = apply_field_edits_work(&mut doc, &batch).unwrap();
        assert!(skipped.is_empty(), "skipped: {skipped:?}");
        let names: Vec<String> = terminal_fields(&doc)
            .unwrap()
            .iter()
            .map(|f| f.qualified.clone())
            .collect();
        assert_eq!(names, vec!["victim".to_string()]);
    }

    /// Resolves a widget's `/AP /N` to stream content bytes. For toggle
    /// fields `/N` is a state dictionary, so pass the state name.
    fn widget_ap_stream(
        doc: &Document,
        field_name: &str,
        widget_index: usize,
        state: Option<&str>,
    ) -> Option<Vec<u8>> {
        let field = terminal_fields(doc)
            .unwrap()
            .into_iter()
            .find(|f| f.qualified == field_name)?;
        let w = field.widgets.get(widget_index)?;
        let id = w.stored.as_reference().ok()?;
        let d = doc.get_dictionary(id).ok()?;
        let ap = resolved(doc, d.get(b"AP").ok()?)?.clone();
        let ap_dict = dict_of(doc, &ap)?;
        let n = resolved(doc, ap_dict.get(b"N").ok()?)?.clone();
        let stream_obj = match state {
            Some(s) => {
                let states = dict_of(doc, &n)?;
                states.get(s.as_bytes()).ok()?.clone()
            }
            None => n,
        };
        match resolved(doc, &stream_obj)? {
            Object::Stream(stream) => Some(stream.content.clone()),
            _ => None,
        }
    }

    #[test]
    fn fill_text_ap_stream_draws_the_value() {
        let mut pdf = testkit::Pdf::new();
        let widget = pdf.add(dictionary! {
            "Type" => "Annot", "Subtype" => "Widget",
            "Rect" => Object::Array(vec![0.into(), 0.into(), 100.into(), 20.into()]),
            "P" => Object::Reference(pdf.page),
        });
        let field = pdf.add(dictionary! {
            "FT" => "Tx", "T" => Object::string_literal("name"),
            "DA" => Object::string_literal("/Helv 12 Tf 0 g"),
            "Kids" => vec![Object::Reference(widget)],
        });
        let acro = pdf.add(dictionary! { "Fields" => vec![Object::Reference(field)] });
        pdf.edit_catalog(|c| c.set("AcroForm", Object::Reference(acro)));
        pdf.edit_page(|p| p.set("Annots", vec![Object::Reference(widget)]));

        let mut doc = load(&mut pdf);
        let values = parse_value_map(r#"{"name":"Alice"}"#).unwrap();
        apply_field_values(&mut doc, &values, false, true).unwrap();

        let content = widget_ap_stream(&doc, "name", 0, None)
            .expect("text widget should carry a generated /AP");
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("(Alice)"), "AP content: {text}");
        assert!(text.contains("/Helv 12 Tf"), "AP content: {text}");
    }

    #[test]
    fn add_pushbutton_writes_mk_caption_and_ap() {
        let mut pdf = testkit::Pdf::new();
        let acro = pdf.add(dictionary! { "Fields" => Vec::<Object>::new() });
        pdf.edit_catalog(|c| c.set("AcroForm", Object::Reference(acro)));

        let mut doc = load(&mut pdf);
        let defs: Vec<FieldDefinition> = serde_json::from_str(
            r#"[{"name":"go","type":"button","pageIndex":0,"x":10,"y":10,"width":80,"height":20,"label":"Print"}]"#,
        )
        .unwrap();
        add_fields_work(&mut doc, &defs).unwrap();

        let field = terminal_fields(&doc)
            .unwrap()
            .into_iter()
            .find(|f| f.qualified == "go")
            .expect("pushbutton field");
        let w = &field.widgets[0];
        let d = doc
            .get_dictionary(w.stored.as_reference().unwrap())
            .unwrap();
        let mk = d.get(b"MK").unwrap();
        assert!(text_of(&doc, mk).is_none());
        let mk_dict = resolved(&doc, mk).unwrap();
        let mk_dict = dict_of(&doc, mk_dict).unwrap();
        let ca = mk_dict.get(b"CA").unwrap();
        assert_eq!(text_of(&doc, ca).as_deref(), Some("Print"));

        let content = widget_ap_stream(&doc, "go", 0, None).expect("pushbutton /AP /N stream");
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("(Print)"), "AP content: {text}");
        assert!(!need_appearances(&doc));
    }

    #[test]
    fn listbox_keeps_need_appearances() {
        let mut pdf = testkit::Pdf::new();
        let widget = pdf.add(dictionary! {
            "Type" => "Annot", "Subtype" => "Widget",
            "Rect" => Object::Array(vec![0.into(), 0.into(), 100.into(), 60.into()]),
            "P" => Object::Reference(pdf.page),
        });
        let field = pdf.add(dictionary! {
            "FT" => "Ch", "T" => Object::string_literal("pick"),
            "Ff" => 1 << 21,
            "Opt" => Object::Array(vec![
                Object::string_literal("a"), Object::string_literal("b"),
            ]),
            "Kids" => vec![Object::Reference(widget)],
        });
        let acro = pdf.add(dictionary! { "Fields" => vec![Object::Reference(field)] });
        pdf.edit_catalog(|c| c.set("AcroForm", Object::Reference(acro)));
        pdf.edit_page(|p| p.set("Annots", vec![Object::Reference(widget)]));

        let mut doc = load(&mut pdf);
        let values = parse_value_map(r#"{"pick":"a"}"#).unwrap();
        apply_field_values(&mut doc, &values, false, true).unwrap();
        // lopdf cannot draw a listbox scroll pane: the flag stays so the
        // viewer regenerates, matching Java's fallback contract.
        assert!(need_appearances(&doc));
    }

    #[test]
    fn repair_widget_geometry_restores_degenerate_rect() {
        let mut pdf = testkit::Pdf::new();
        let widget = pdf.add(dictionary! {
            "Type" => "Annot", "Subtype" => "Widget",
            "Rect" => Object::Array(vec![0.into(), 0.into(), 0.into(), 0.into()]),
            "P" => Object::Reference(pdf.page),
        });
        let field = pdf.add(dictionary! {
            "FT" => "Tx", "T" => Object::string_literal("name"),
            "Kids" => vec![Object::Reference(widget)],
        });
        let acro = pdf.add(dictionary! { "Fields" => vec![Object::Reference(field)] });
        pdf.edit_catalog(|c| c.set("AcroForm", Object::Reference(acro)));
        pdf.edit_page(|p| p.set("Annots", vec![Object::Reference(widget)]));

        let mut doc = load(&mut pdf);
        // No values still runs the geometry repair on the strict path.
        apply_field_values(&mut doc, &[], false, true).unwrap();

        let field = terminal_fields(&doc)
            .unwrap()
            .into_iter()
            .find(|f| f.qualified == "name")
            .unwrap();
        let rect = widget_rect(&doc, &field.widgets[0]).unwrap();
        assert_eq!(rect, [0.0, 0.0, 200.0, 40.0]);
    }
}
