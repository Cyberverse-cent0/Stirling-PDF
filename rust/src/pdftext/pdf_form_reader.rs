//! AcroForm widget reader, the Rust counterpart of jpdfium's
//! `doc/PdfFormReader`. Where PDFium walks `FPDFPage_GetAnnot` and the
//! form-fill environment, this reads the page's `/Annots` widget
//! dictionaries through lopdf and resolves field properties up the
//! `/Parent` chain the way PDF field inheritance works.

use lopdf::{Dictionary, Document, Object};

use super::form_field::{FormField, FormFieldType, Rect};

/// Field-flag bits from the PDF spec's `Ff` entry.
const FF_READ_ONLY: i64 = 1 << 0;
const FF_REQUIRED: i64 = 1 << 1;
const FF_PUSHBUTTON: i64 = 1 << 16;
const FF_RADIO: i64 = 1 << 17;
const FF_COMBO: i64 = 1 << 18;

/// All form fields on one page — jpdfium `PdfFormReader.readPage`.
/// Returns an empty list for a malformed AcroForm rather than failing the
/// surrounding conversion.
pub fn read_page(doc: &Document, page_index: usize) -> Vec<FormField> {
    let pages = crate::pdfdoc::page_ids(doc);
    let Some(&page_id) = pages.get(page_index) else {
        return Vec::new();
    };
    let Ok(page) = doc.get_dictionary(page_id) else {
        return Vec::new();
    };
    let Some(annots) = dict_array(doc, page, b"Annots") else {
        return Vec::new();
    };
    let mut fields = Vec::new();
    for annot in annots {
        let resolved = doc
            .dereference(&annot)
            .map(|(_, o)| o.clone())
            .unwrap_or(annot);
        let Object::Dictionary(dict) = &resolved else {
            continue;
        };
        if !is_name(doc, dict, b"Subtype", b"Widget") {
            continue;
        }
        fields.push(read_widget(doc, dict, page_index));
    }
    fields
}

/// All form fields in the document — jpdfium `PdfFormReader.readAll`.
pub fn read_all(doc: &Document) -> Vec<FormField> {
    crate::pdfdoc::page_ids(doc)
        .iter()
        .enumerate()
        .flat_map(|(i, _)| read_page(doc, i))
        .collect()
}

/// Walks the widget's `/Parent` chain collecting the first defined value
/// of each inheritable key, widget dict first.
fn inherited<'a>(doc: &'a Document, dict: &'a Dictionary, key: &[u8]) -> Option<Object> {
    let mut current = Some(dict);
    while let Some(d) = current {
        if let Ok(v) = d.get(key) {
            if !matches!(v, Object::Null) {
                return doc
                    .dereference(v)
                    .ok()
                    .map(|(_, o)| o.clone())
                    .or(Some(v.clone()));
            }
        }
        current = parent_dict(doc, d);
    }
    None
}

fn parent_dict<'a>(doc: &'a Document, dict: &'a Dictionary) -> Option<&'a Dictionary> {
    let v = dict.get(b"Parent").ok()?;
    let (_, obj) = doc.dereference(v).ok()?;
    match obj {
        Object::Dictionary(d) => Some(d),
        _ => None,
    }
}

fn read_widget<'a>(doc: &'a Document, dict: &'a Dictionary, page_index: usize) -> FormField {
    let field_type = field_type(doc, dict);
    let flags = inherited(doc, dict, b"Ff")
        .and_then(|o| int_value(&o))
        .unwrap_or(0);
    let name = inherited(doc, dict, b"T")
        .and_then(|o| text_value(doc, &o))
        .unwrap_or_default();
    let value = inherited(doc, dict, b"V").and_then(|o| text_value(doc, &o));
    let tooltip = inherited(doc, dict, b"TU")
        .and_then(|o| text_value(doc, &o))
        .unwrap_or_default();
    let checked = check_state(doc, dict);
    let (export_value, options, selected) = choice_data(doc, dict);
    let export_value = export_value.or_else(|| {
        (checked && matches!(field_type, FormFieldType::Checkbox | FormFieldType::Radio))
            .then(|| on_state_name(doc, dict))
            .flatten()
    });
    FormField {
        page_index,
        name,
        field_type,
        value,
        export_value,
        checked,
        read_only: flags & FF_READ_ONLY != 0,
        required: flags & FF_REQUIRED != 0,
        tooltip,
        options,
        selected_option_indices: selected,
        rect: rect_value(doc, dict),
    }
}

fn field_type<'a>(doc: &'a Document, dict: &'a Dictionary) -> FormFieldType {
    let Some(ft) = inherited(doc, dict, b"FT") else {
        return FormFieldType::Unknown;
    };
    let flags = inherited(doc, dict, b"Ff")
        .and_then(|o| int_value(&o))
        .unwrap_or(0);
    match name_str(&ft) {
        Some(n) if n == "Tx" => FormFieldType::Text,
        Some(n) if n == "Ch" => {
            if flags & FF_COMBO != 0 {
                FormFieldType::Combobox
            } else {
                FormFieldType::Listbox
            }
        }
        Some(n) if n == "Btn" => {
            if flags & FF_PUSHBUTTON != 0 {
                FormFieldType::Pushbutton
            } else if flags & FF_RADIO != 0 {
                FormFieldType::Radio
            } else {
                FormFieldType::Checkbox
            }
        }
        Some(n) if n == "Sig" => FormFieldType::Signature,
        _ => FormFieldType::Unknown,
    }
}

/// Checkbox/radio checked state: `/AS` names the active appearance state;
/// anything but `/Off` counts as on, matching PDFium's `IsChecked`.
fn check_state<'a>(doc: &'a Document, dict: &'a Dictionary) -> bool {
    let Some(as_obj) = dict.get(b"AS").ok().and_then(|v| doc.dereference(v).ok()) else {
        return false;
    };
    name_str(as_obj.1).is_some_and(|n| n != "Off")
}

/// The appearance-state name that turns a checkbox on (first `/AP /N` key
/// that isn't `/Off`) — jpdfium's export-value fallback.
fn on_state_name<'a>(doc: &'a Document, dict: &'a Dictionary) -> Option<String> {
    let ap = dict.get(b"AP").ok()?;
    let (_, ap_obj) = doc.dereference(ap).ok()?;
    let Object::Dictionary(ap_dict) = ap_obj else {
        return None;
    };
    let n = ap_dict.get(b"N").ok()?;
    let (_, n_obj) = doc.dereference(n).ok()?;
    let Object::Dictionary(states) = n_obj else {
        return None;
    };
    states
        .iter()
        .filter_map(|(k, _)| std::str::from_utf8(k).ok().map(str::to_string))
        .find(|n| n != "Off")
}

/// `/Opt` entries plus the `/I` selected indices for choice fields.
fn choice_data<'a>(
    doc: &'a Document,
    dict: &'a Dictionary,
) -> (Option<String>, Vec<String>, Vec<usize>) {
    let Some(opt) = inherited(doc, dict, b"Opt") else {
        return (None, Vec::new(), Vec::new());
    };
    let Object::Array(entries) = opt else {
        return (None, Vec::new(), Vec::new());
    };
    let mut options = Vec::with_capacity(entries.len());
    for entry in entries {
        match doc.dereference(&entry) {
            Ok((_, Object::Array(pair))) if pair.len() >= 2 => {
                // [export value, display label] — keep the label.
                options.push(text_value(doc, &pair[1]).unwrap_or_default());
            }
            _ => options.push(text_value(doc, &entry).unwrap_or_default()),
        }
    }
    let selected = inherited(doc, dict, b"I")
        .and_then(|o| match o {
            Object::Array(items) => Some(
                items
                    .iter()
                    .filter_map(|i| int_value(i).map(|v| v.max(0) as usize))
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_default();
    (None, options, selected)
}

fn dict_array<'a>(doc: &'a Document, dict: &'a Dictionary, key: &[u8]) -> Option<Vec<Object>> {
    let v = dict.get(key).ok()?;
    let (_, obj) = doc.dereference(v).ok()?;
    match obj {
        Object::Array(items) => Some(items.clone()),
        _ => None,
    }
}

fn is_name(doc: &Document, dict: &Dictionary, key: &[u8], want: &[u8]) -> bool {
    dict.get(key)
        .ok()
        .and_then(|v| doc.dereference(v).ok())
        .and_then(|(_, o)| name_str(o))
        .is_some_and(|n| n.as_bytes() == want)
}

fn rect_value(doc: &Document, dict: &Dictionary) -> Option<Rect> {
    let v = dict.get(b"Rect").ok()?;
    let (_, obj) = doc.dereference(v).ok()?;
    let Object::Array(a) = obj else {
        return None;
    };
    if a.len() < 4 {
        return None;
    }
    let nums: Option<Vec<f64>> = a[..4].iter().map(num).collect();
    let [x0, y0, x1, y1] = nums?.try_into().ok()?;
    Some(Rect {
        x: x0,
        y: y0,
        width: x1 - x0,
        height: y1 - y0,
    })
}

fn num(o: &Object) -> Option<f64> {
    match o {
        Object::Integer(i) => Some(*i as f64),
        Object::Real(r) => Some(f64::from(*r)),
        _ => None,
    }
}

fn int_value(o: &Object) -> Option<i64> {
    match o {
        Object::Integer(i) => Some(*i),
        Object::Real(r) => Some(*r as i64),
        _ => None,
    }
}

/// `/FT`-style name objects and PDFDocEncoding strings both surface as
/// names here.
fn name_str(o: &Object) -> Option<String> {
    match o {
        Object::Name(n) => Some(String::from_utf8_lossy(n).into_owned()),
        _ => None,
    }
}

/// PDF text string (literal or hex) or name → UTF-8-ish text.
fn text_value(doc: &Document, o: &Object) -> Option<String> {
    let (_, resolved) = doc.dereference(o).ok()?;
    match resolved {
        Object::String(bytes, _) => Some(String::from_utf8_lossy(bytes).into_owned()),
        Object::Name(n) => Some(String::from_utf8_lossy(n).into_owned()),
        _ => None,
    }
}

/// Java-style facade over this module's free functions; the ported pipeline
/// calls the Java `PdfFormReader` helper as a static class.
#[allow(dead_code)]
pub struct PdfFormReader;

#[allow(dead_code)]
impl PdfFormReader {
    pub fn read_page(doc: &Document, page_index: usize) -> Vec<FormField> {
        read_page(doc, page_index)
    }
    pub fn read_all(doc: &Document) -> Vec<FormField> {
        read_all(doc)
    }
}
