//! Recovers AcroForm values that live only in a field's `/V`, as pseudo
//! lines at their widget rectangles so they land in reading order.
//! Port of `FormValues.java`.

use std::rc::Rc;

use lopdf::Document;

use super::form_field::{FormField, FormFieldType};
use super::line::{Line, SharedLine};
use super::markdown_text::MarkdownText;
use super::model::{PdfChar, PdfLine, PdfWord};
use super::pdf_form_reader::PdfFormReader;

/// Pseudo text lines for `/V`-only AcroForm values, placed at their widget
/// rectangles; values already in the content stream are skipped.
pub fn lines(doc: &Document, page_index: usize, existing: &[SharedLine]) -> Vec<SharedLine> {
    // A malformed AcroForm must not sink the whole conversion; body text still stands.
    // TODO(jpdfium): PdfFormReader.readPage's RuntimeException catch becomes an
    // empty result here — confirm the Rust reader reports parse failures the same way.
    let fields = PdfFormReader::read_page(doc, page_index);
    let mut out = Vec::new();
    for f in &fields {
        let Some(value) = field_text(f) else {
            continue;
        };
        if value.trim().is_empty() {
            continue;
        }
        let Some(r) = &f.rect else {
            continue;
        };
        if r.width <= 0.0 || r.height <= 0.0 {
            continue;
        }
        let (rx, ry, rw, rh) = (r.x as f32, r.y as f32, r.width as f32, r.height as f32);
        if already_in_content(existing, &value, rx, ry, rw, rh) {
            continue;
        }
        out.push(synthetic_line(&value, rx, ry, rw, rh));
    }
    out
}

/// The text a filled field contributes, or `None` when the field contributes nothing.
fn field_text(f: &FormField) -> Option<String> {
    match &f.field_type {
        FormFieldType::Pushbutton | FormFieldType::Signature | FormFieldType::Unknown => None,
        FormFieldType::Checkbox | FormFieldType::Radio => {
            if f.checked {
                Some("[x]".to_string())
            } else {
                None
            }
        }
        _ => {
            let value = f.value.as_deref()?;
            if value == "Off" {
                return None;
            }
            Some(value.replace(['\r', '\n'], " ").trim().to_string())
        }
    }
}

/// True when the extractor already found this value inside the widget's own rectangle.
fn already_in_content(
    lines: &[SharedLine],
    value: &str,
    rx: f32,
    ry: f32,
    rw: f32,
    rh: f32,
) -> bool {
    let needle = MarkdownText::normalise_space(value);
    for l in lines {
        let overlaps = l.x < rx + rw && l.x + l.width > rx && l.y < ry + rh && l.y + l.height > ry;
        if overlaps && MarkdownText::normalise_space(&l.text).contains(&needle) {
            return true;
        }
    }
    false
}

/// Wraps a field value as a one-word-per-token [`PdfLine`] at the widget
/// rectangle, so downstream stages treat it like any other text.
fn synthetic_line(value: &str, rx: f32, ry: f32, rw: f32, rh: f32) -> SharedLine {
    // Java split("\\s+"): ASCII whitespace only, no leading empty since the
    // value is already stripped.
    let tokens: Vec<&str> = value
        .split([' ', '\t', '\n', '\x0B', '\x0C', '\r'])
        .filter(|t| !t.is_empty())
        .collect();
    let height = rh.min(14.0);
    let advance = if tokens.is_empty() {
        rw
    } else {
        rw / tokens.len() as f32
    };
    let mut words = Vec::with_capacity(tokens.len());
    for (i, token) in tokens.iter().enumerate() {
        let wx = rx + advance * i as f32;
        let mut chars = Vec::with_capacity(token.chars().count());
        let char_width = if token.is_empty() {
            advance
        } else {
            advance / token.chars().count() as f32
        };
        for (c, ch) in token.chars().enumerate() {
            chars.push(PdfChar {
                ch,
                x: wx + char_width * c as f32,
                y: ry,
                width: char_width,
                height,
                font_name: String::new(),
                font_size: 0.0,
            });
        }
        words.push(PdfWord {
            chars,
            x: wx,
            y: ry,
            width: advance * 0.95,
            height,
        });
    }
    let source = PdfLine {
        words,
        x: rx,
        y: ry,
        width: rw,
        height,
    };
    let mut out = Line::with_text(source, value.to_string());
    out.synthetic = true;
    Rc::new(out)
}

/// Java-style facade over this module's free functions; the ported pipeline
/// calls the Java `FormValues` helper as a static class.
#[allow(dead_code)]
pub struct FormValues;

#[allow(dead_code)]
impl FormValues {
    pub fn lines(doc: &Document, page_index: usize, existing: &[SharedLine]) -> Vec<SharedLine> {
        lines(doc, page_index, existing)
    }
}
