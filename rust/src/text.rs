//! Positioned text extraction for the auto-rename and auto-rotate endpoints.
//!
//! lopdf's `extract_text` yields flat strings; these endpoints need each run's
//! baseline position, effective point size and direction, so this walks the
//! content stream itself: text-state operators (`Tf`, `Td`, `Tm`, `T*`, …)
//! are tracked through the graphics state, strings are decoded through the
//! same per-font `Encoding` lopdf uses, and form XObjects are descended into
//! like PDFBox's stripper does.
//!
//! Fidelity notes vs PDFBox: glyph advances inside `TJ` arrays and Type3 /
//! CID font metrics are approximated — positions are per text-showing
//! operation, not per glyph, and only the baseline origin is recorded.

use lopdf::content::{Content, Operation};
use lopdf::{Dictionary, Document, Object, ObjectId};

/// One decoded text run: the string shown by a single `Tj`/`TJ`/`'`/`"`
/// operation plus where and how it was rendered.
#[derive(Debug, Clone)]
pub struct TextItem {
    pub text: String,
    /// Baseline origin in default user space, points.
    #[allow(dead_code)]
    pub x: f64,
    pub y: f64,
    /// Rendered size in points: the `Tf` size scaled by the text matrix.
    pub font_size: f64,
    /// Baseline direction in degrees CCW from +x after quadrant snapping,
    /// matching PDFBox's `TextPosition.getDir` (0/90/180/270 only).
    pub direction: f64,
}

/// One glyph for table extraction, the counterpart of Tabula's
/// `TextElement`: a single decoded character with its advance and the
/// font metrics `mergeWords` needs. Positions are baseline origins in
/// default user space (y up), like [`TextItem`].
#[derive(Debug, Clone)]
pub struct CharItem {
    pub text: String,
    pub x: f64,
    pub y: f64,
    /// Advance in user-space units along the baseline.
    pub w: f64,
    /// Rendered size in points (`Tf` size × text matrix scale).
    pub font_size: f64,
    /// `/BaseFont` name, subset prefix stripped — feeds the standard-14
    /// width tables in `fontmetrics`.
    pub font: String,
    /// Space-glyph advance in the same units as `w`.
    pub space_w: f64,
    pub direction: f64,
}

/// PDF transformation matrix `[a b c d e f]`; points map as
/// `x' = a*x + c*y + e`, `y' = b*x + d*y + f`.
pub(crate) type Matrix = [f64; 6];

pub(crate) const IDENTITY: Matrix = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

/// `m · n`: the matrix product, applying `n` first then `m`.
pub(crate) fn mat_mul(m: Matrix, n: Matrix) -> Matrix {
    [
        m[0] * n[0] + m[2] * n[1],
        m[1] * n[0] + m[3] * n[1],
        m[0] * n[2] + m[2] * n[3],
        m[1] * n[2] + m[3] * n[3],
        m[0] * n[4] + m[2] * n[5] + m[4],
        m[1] * n[4] + m[3] * n[5] + m[5],
    ]
}

pub(crate) fn apply(m: Matrix, x: f64, y: f64) -> (f64, f64) {
    (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5])
}

pub(crate) fn num(object: &Object) -> Option<f64> {
    match object {
        Object::Integer(i) => Some(*i as f64),
        Object::Real(r) => Some(*r as f64),
        _ => None,
    }
}

pub(crate) fn matrix_operand(operands: &[Object]) -> Option<Matrix> {
    if operands.len() < 6 {
        return None;
    }
    let mut m = [0.0; 6];
    for (i, slot) in m.iter_mut().enumerate() {
        *slot = num(&operands[i])?;
    }
    Some(m)
}

/// Text state carried by the graphics state (`q`/`Q` save and restore it).
#[derive(Clone)]
struct TextState {
    tm: Matrix,
    tlm: Matrix,
    font_size: f64,
    leading: f64,
    char_space: f64,
    word_space: f64,
    /// `Tz` horizontal scaling, percent; 100 is unscaled.
    horiz_scale: f64,
    /// `Tr` rendering mode; 3 is invisible text, which still advances
    /// the pen but emits no glyph.
    render_mode: i64,
}

impl Default for TextState {
    fn default() -> Self {
        Self {
            tm: IDENTITY,
            tlm: IDENTITY,
            font_size: 0.0,
            leading: 0.0,
            char_space: 0.0,
            word_space: 0.0,
            horiz_scale: 100.0,
            render_mode: 0,
        }
    }
}

/// The `/Resources` dictionaries in effect for one content stream: a page's
/// resolved resources, or a form XObject's own (falling back to the caller's
/// when the form omits the entry, which producers do).
pub(crate) struct Resources<'a> {
    doc: &'a Document,
    dicts: Vec<&'a Dictionary>,
}

impl<'a> Resources<'a> {
    pub(crate) fn new(doc: &'a Document, dicts: Vec<&'a Dictionary>) -> Self {
        Self { doc, dicts }
    }

    /// First `/Resources` subsection (`Font`, `XObject`, …) found.
    pub(crate) fn section(&self, name: &[u8]) -> Option<&'a Dictionary> {
        for dict in &self.dicts {
            if let Ok(value) = dict.get(name) {
                if let Ok((_, Object::Dictionary(section))) = self.doc.dereference(value) {
                    return Some(section);
                }
            }
        }
        None
    }

    /// Encoding of the font a `Tf` resource name resolves to, plus its
    /// `/BaseFont` for width lookups.
    fn font_info(&self, name: &[u8]) -> Option<(lopdf::Encoding<'a>, String)> {
        let value = self.section(b"Font")?.get(name).ok()?;
        let (_, object) = self.doc.dereference(value).ok()?;
        let dict = object.as_dict().ok()?;
        let encoding = dict.get_font_encoding(self.doc).ok()?;
        let base = dict
            .get(b"BaseFont")
            .ok()
            .and_then(|v| {
                self.doc
                    .dereference(v)
                    .ok()
                    .and_then(|(_, o)| {
                        o.as_name()
                            .ok()
                            .map(|n| String::from_utf8_lossy(n).into_owned())
                    })
                    .or_else(|| {
                        v.as_str()
                            .ok()
                            .map(|b| String::from_utf8_lossy(b).into_owned())
                    })
            })
            .unwrap_or_default();
        // "ABCDEF+Helvetica" subset prefixes carry no metrics meaning.
        let base = base.rsplit('+').next().unwrap_or(&base).to_string();
        Some((encoding, base))
    }

    /// The object id and stream dictionary a `Do` name resolves to. XObjects
    /// are streams, which are always indirect objects.
    pub(crate) fn xobject(&self, name: &[u8]) -> Option<(ObjectId, &'a Dictionary)> {
        let value = self.section(b"XObject")?.get(name).ok()?;
        let (id, object) = self.doc.dereference(value).ok()?;
        match object {
            Object::Stream(stream) => Some((id?, &stream.dict)),
            _ => None,
        }
    }

    /// Resources declared by a form XObject dictionary.
    pub(crate) fn of_form(&self, form: &'a Dictionary) -> Self {
        let mut dicts = Vec::new();
        if let Ok(value) = form.get(b"Resources") {
            if let Ok((_, Object::Dictionary(dict))) = self.doc.dereference(value) {
                dicts.push(dict);
            }
        }
        // A form without /Resources still resolves names against the
        // resources in effect at the Do site on many producers' files.
        dicts.extend(self.dicts.iter().copied());
        Self::new(self.doc, dicts)
    }
}

/// Depth guard for form XObject recursion; cyclic forms would otherwise
/// recurse forever.
pub(crate) const MAX_XOBJECT_DEPTH: usize = 16;

struct ExtractState<'a> {
    ctm: Matrix,
    ctm_stack: Vec<Matrix>,
    text: TextState,
    text_stack: Vec<TextState>,
    font_info: Option<(lopdf::Encoding<'a>, String)>,
    items: Vec<TextItem>,
    chars: Vec<CharItem>,
    emit_chars: bool,
}

/// Positioned text runs of one page, including text inside form XObjects.
pub fn page_items(doc: &Document, page_id: ObjectId) -> Vec<TextItem> {
    page_items_inner(doc, page_id, false).0
}

/// Per-glyph text of one page for table extraction, including form
/// XObjects. Unlike [`page_items`], this tracks the pen across show
/// operators and `TJ` adjustments, which cell text assignment needs.
pub fn page_chars(doc: &Document, page_id: ObjectId) -> Vec<CharItem> {
    page_items_inner(doc, page_id, true).1
}

fn page_items_inner(
    doc: &Document,
    page_id: ObjectId,
    emit_chars: bool,
) -> (Vec<TextItem>, Vec<CharItem>) {
    let mut dicts = Vec::new();
    if let Ok((inline, ids)) = doc.get_page_resources(page_id) {
        if let Some(dict) = inline {
            dicts.push(dict);
        }
        for id in ids {
            if let Ok(dict) = doc.get_dictionary(id) {
                dicts.push(dict);
            }
        }
    }
    let resources = Resources::new(doc, dicts);

    let mut state = ExtractState {
        ctm: IDENTITY,
        ctm_stack: Vec::new(),
        text: TextState::default(),
        text_stack: Vec::new(),
        font_info: None,
        items: Vec::new(),
        chars: Vec::new(),
        emit_chars,
    };
    if let Ok(content) = doc.get_and_decode_page_content(page_id) {
        state.walk(&resources, &content.operations, 0);
    }
    (state.items, state.chars)
}

/// Positioned text runs for every page, in page order.
pub fn document_items(doc: &Document) -> Vec<Vec<TextItem>> {
    crate::pdfdoc::page_ids(doc)
        .iter()
        .map(|id| page_items(doc, *id))
        .collect()
}

impl<'a> ExtractState<'a> {
    fn walk(&mut self, resources: &Resources<'a>, operations: &[Operation], depth: usize) {
        for operation in operations {
            let operands = &operation.operands;
            match operation.operator.as_str() {
                "q" => {
                    self.ctm_stack.push(self.ctm);
                    self.text_stack.push(self.text.clone());
                }
                "Q" => {
                    if let Some(ctm) = self.ctm_stack.pop() {
                        self.ctm = ctm;
                    }
                    if let Some(text) = self.text_stack.pop() {
                        self.text = text;
                    }
                }
                "cm" => {
                    if let Some(m) = matrix_operand(operands) {
                        // The operand maps the inner space outward, so it is
                        // applied before the existing CTM.
                        self.ctm = mat_mul(self.ctm, m);
                    }
                }
                "BT" => {
                    self.text.tm = IDENTITY;
                    self.text.tlm = IDENTITY;
                }
                "Tf" => {
                    if let Some(name) = operands.first().and_then(|o| o.as_name().ok()) {
                        self.font_info = resources.font_info(name);
                    }
                    if let Some(size) = operands.get(1).and_then(num) {
                        self.text.font_size = size;
                    }
                }
                "Td" | "TD" => {
                    let (Some(tx), Some(ty)) = (
                        operands.first().and_then(num),
                        operands.get(1).and_then(num),
                    ) else {
                        continue;
                    };
                    if operation.operator == "TD" {
                        self.text.leading = -ty;
                    }
                    // The offset is in unscaled text space, so the
                    // translation is the innermost matrix: Tlm × T.
                    let m = [1.0, 0.0, 0.0, 1.0, tx, ty];
                    self.text.tlm = mat_mul(self.text.tlm, m);
                    self.text.tm = self.text.tlm;
                }
                "Tm" => {
                    if let Some(m) = matrix_operand(operands) {
                        self.text.tm = m;
                        self.text.tlm = m;
                    }
                }
                "T*" => self.next_line(),
                "TL" => {
                    if let Some(leading) = operands.first().and_then(num) {
                        self.text.leading = leading;
                    }
                }
                "Tc" => {
                    if let Some(v) = operands.first().and_then(num) {
                        self.text.char_space = v;
                    }
                }
                "Tw" => {
                    if let Some(v) = operands.first().and_then(num) {
                        self.text.word_space = v;
                    }
                }
                "Tz" => {
                    if let Some(v) = operands.first().and_then(num) {
                        self.text.horiz_scale = v;
                    }
                }
                "Tr" => {
                    if let Some(v) = operands.first().and_then(num) {
                        self.text.render_mode = v as i64;
                    }
                }
                "Tj" | "TJ" => {
                    if self.emit_chars {
                        self.show_chars(operands);
                    } else {
                        self.show_text(operands);
                    }
                }
                // `'` is `T* Tj`; `"` is `aw Tw ac Tc T* Tj` — only the string
                // matters here, and it is shown after the line move.
                "'" => {
                    self.next_line();
                    if self.emit_chars {
                        self.show_chars(operands);
                    } else {
                        self.show_text(operands);
                    }
                }
                "\"" => {
                    self.next_line();
                    if let Some(string) = operands.get(2..) {
                        if self.emit_chars {
                            self.show_chars(string);
                        } else {
                            self.show_text(string);
                        }
                    }
                }
                "Do" => self.show_xobject(resources, operands, depth),
                _ => {}
            }
        }
    }

    fn next_line(&mut self) {
        let m = [1.0, 0.0, 0.0, 1.0, 0.0, -self.text.leading];
        self.text.tlm = mat_mul(self.text.tlm, m);
        self.text.tm = self.text.tlm;
    }

    fn show_xobject(&mut self, resources: &Resources<'a>, operands: &[Object], depth: usize) {
        if depth >= MAX_XOBJECT_DEPTH {
            return;
        }
        let Some(name) = operands.first().and_then(|o| o.as_name().ok()) else {
            return;
        };
        let Some((id, dict)) = resources.xobject(name) else {
            return;
        };
        let is_form = dict
            .get(b"Subtype")
            .ok()
            .and_then(|s| s.as_name().ok())
            .is_some_and(|s| s == &b"Form"[..]);
        if !is_form {
            return;
        }
        // Do executes the form under the concatenation of its /Matrix with
        // the CTM in effect, inside a saved graphics state.
        let saved_ctm = self.ctm;
        let saved_text = self.text.clone();
        let saved_font = self.font_info.take();
        if let Some(m) = dict
            .get(b"Matrix")
            .ok()
            .and_then(|v| v.as_array().ok())
            .and_then(|a| matrix_operand(a))
        {
            self.ctm = mat_mul(self.ctm, m);
        }
        let form_resources = resources.of_form(dict);
        let content = resources
            .doc
            .get_object(id)
            .ok()
            .and_then(|o| match o {
                Object::Stream(s) => s.decompressed_content().ok(),
                _ => None,
            })
            .and_then(|bytes| Content::decode(&bytes).ok());
        if let Some(content) = content {
            self.walk(&form_resources, &content.operations, depth + 1);
        }
        self.ctm = saved_ctm;
        self.text = saved_text;
        self.font_info = saved_font;
    }

    fn show_text(&mut self, operands: &[Object]) {
        let Some((encoding, _)) = &self.font_info else {
            return;
        };
        let mut text = String::new();
        collect_shown(&mut text, encoding, operands);
        if text.is_empty() {
            return;
        }
        // Text space → user space: the text matrix first, then the CTM.
        let combined = mat_mul(self.ctm, self.text.tm);
        let (x, y) = apply(combined, 0.0, 0.0);
        let scale = combined[0].hypot(combined[1]);
        let font_size = self.text.font_size * scale;
        let degrees = combined[1].atan2(combined[0]).to_degrees();
        let snapped = (degrees.rem_euclid(360.0) / 90.0).round() * 90.0;
        self.items.push(TextItem {
            text,
            x,
            y,
            font_size,
            direction: snapped.rem_euclid(360.0),
        });
    }

    /// Glyph-at-a-time variant of [`Self::show_text`] for table
    /// extraction. Tracks the pen in text space across operands so
    /// consecutive `Tj`/`TJ` ops and kerning adjustments position each
    /// character, then commits the advance into `Tm`/`Tlm` like a real
    /// text-showing operator.
    fn show_chars(&mut self, operands: &[Object]) {
        let Some((encoding, base_font)) = &self.font_info else {
            return;
        };
        // Text space → user space: the text matrix first, then the CTM.
        let combined = mat_mul(self.ctm, self.text.tm);
        let scale = combined[0].hypot(combined[1]);
        let font_size = self.text.font_size * scale;
        let degrees = combined[1].atan2(combined[0]).to_degrees();
        let direction = ((degrees.rem_euclid(360.0) / 90.0).round() * 90.0).rem_euclid(360.0);
        let params = CharEmit {
            encoding,
            base_font,
            combined,
            scale,
            font_size,
            direction,
            space_w: crate::fontmetrics::string_width(base_font, &[32]) / 1000.0
                * self.text.font_size
                * scale,
            tz: self.text.horiz_scale / 100.0,
            tf_size: self.text.font_size,
            char_space: self.text.char_space,
            word_space: self.text.word_space,
            invisible: self.text.render_mode == 3,
            tx: 0.0,
            out: Vec::new(),
        };
        let mut params = params;
        for operand in operands {
            emit_operand_chars(operand, &mut params);
        }
        let tx = params.tx;
        self.chars.extend(params.out);
        if tx != 0.0 {
            let advance = [1.0, 0.0, 0.0, 1.0, tx, 0.0];
            self.text.tm = mat_mul(self.text.tm, advance);
            self.text.tlm = self.text.tm;
        }
    }
}

/// Per-show-operation state for [`emit_operand_chars`]: the font
/// encoding and metrics resolved once, plus the running text-space pen.
struct CharEmit<'e, 'a> {
    encoding: &'e lopdf::Encoding<'a>,
    base_font: &'e str,
    combined: Matrix,
    scale: f64,
    font_size: f64,
    direction: f64,
    space_w: f64,
    tz: f64,
    tf_size: f64,
    char_space: f64,
    word_space: f64,
    invisible: bool,
    tx: f64,
    out: Vec<CharItem>,
}

fn emit_operand_chars(operand: &Object, p: &mut CharEmit) {
    match operand {
        Object::String(bytes, _) => {
            let Ok(decoded) = p.encoding.bytes_to_string(bytes) else {
                return;
            };
            for ch in decoded.chars() {
                let units = crate::fontmetrics::string_width(p.base_font, &[winansi_byte(ch)]);
                let mut adv = (units / 1000.0) * p.tf_size
                    + p.char_space
                    + if ch == ' ' { p.word_space } else { 0.0 };
                adv *= p.tz;
                if !p.invisible {
                    let (x, y) = apply(p.combined, p.tx, 0.0);
                    p.out.push(CharItem {
                        text: ch.to_string(),
                        x,
                        y,
                        w: adv * p.scale,
                        font_size: p.font_size,
                        font: p.base_font.to_string(),
                        space_w: p.space_w,
                        direction: p.direction,
                    });
                }
                p.tx += adv;
            }
        }
        Object::Array(arr) => {
            for element in arr {
                match element {
                    Object::String(..) => emit_operand_chars(element, p),
                    // TJ adjustments are in thousandths of an em and shift
                    // the pen permanently, like a negative Tc.
                    Object::Integer(n) => p.tx += -(*n as f64) / 1000.0 * p.tf_size,
                    Object::Real(r) => p.tx += -(*r as f64) / 1000.0 * p.tf_size,
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// WinAnsi byte used for the standard-14 width tables; characters the
/// encoding cannot represent carry the width of `?`, matching
/// `fontmetrics`' own fallback.
fn winansi_byte(ch: char) -> u8 {
    match ch {
        c if c.is_ascii() => c as u8,
        c if ('\u{a0}'..='\u{ff}').contains(&c) => c as u8,
        _ => b'?',
    }
}

/// Decodes every string operand (recursing into `TJ` arrays) through the
/// font encoding.
fn collect_shown(text: &mut String, encoding: &lopdf::Encoding, operands: &[Object]) {
    for operand in operands {
        match operand {
            Object::String(bytes, _) => {
                if let Ok(decoded) = encoding.bytes_to_string(bytes) {
                    text.push_str(&decoded);
                }
            }
            Object::Array(arr) => collect_shown(text, encoding, arr),
            _ => {}
        }
    }
}

/// Per-page glyph-direction histogram: non-whitespace character counts per
/// 90° quadrant, for `AutoRotateDetection`'s dominance test.
pub fn page_direction_counts(items: &[TextItem]) -> [usize; 4] {
    let mut counts = [0usize; 4];
    for item in items {
        let quadrant = ((item.direction as i64) / 90).rem_euclid(4) as usize;
        counts[quadrant] += item.text.chars().filter(|c| !c.is_whitespace()).count();
    }
    counts
}

/// Line grouping for auto-rename: consecutive items sharing a baseline `y`
/// merge into one line keeping the largest font size, like the custom
/// `PDFTextStripper` in `AutoRenameController`.
pub fn lines_of(items: &[TextItem]) -> Vec<(String, f64)> {
    let mut lines: Vec<(String, f64)> = Vec::new();
    let mut last_y: Option<f64> = None;
    for item in items {
        if last_y.is_none_or(|y| y != item.y) {
            lines.push((String::new(), 0.0));
            last_y = Some(item.y);
        }
        let line = lines.last_mut().expect("line just pushed");
        line.0.push_str(&item.text);
        if item.font_size > line.1 {
            line.1 = item.font_size;
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit;
    use lopdf::{dictionary, Document, Object};

    /// A one-page PDF whose content stream shows `ops` in Helvetica 12pt.
    fn pdf_with_text(ops: &str) -> Vec<u8> {
        let mut pdf = testkit::Pdf::new();
        let font = pdf.add(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let content = pdf.add_stream(dictionary! {}, ops.as_bytes());
        pdf.edit_page(|page| {
            page.set("Contents", Object::Reference(content));
            page.set(
                "Resources",
                dictionary! {
                    "Font" => dictionary! {
                        "F1" => Object::Reference(font),
                    },
                },
            );
        });
        pdf.bytes()
    }

    fn items_of(bytes: &[u8]) -> Vec<TextItem> {
        let doc = Document::load_mem(bytes).unwrap();
        let page_id = *doc.get_pages().values().next().unwrap();
        page_items(&doc, page_id)
    }

    #[test]
    fn extracts_positioned_text_runs() {
        let bytes = pdf_with_text("BT /F1 12 Tf 72 720 Td (Hello) Tj ET");
        let items = items_of(&bytes);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "Hello");
        assert!((items[0].x - 72.0).abs() < 0.01);
        assert!((items[0].y - 720.0).abs() < 0.01);
        assert!((items[0].font_size - 12.0).abs() < 0.01);
        assert_eq!(items[0].direction, 0.0);
    }

    #[test]
    fn tj_arrays_and_quotes_decode_strings() {
        let bytes = pdf_with_text("BT /F1 10 Tf 100 700 Td [(Hel) -120 (lo)] TJ (World) Tj ET");
        let items = items_of(&bytes);
        let texts: Vec<&str> = items.iter().map(|i| i.text.as_str()).collect();
        assert!(texts.contains(&"Hello"));
        assert!(texts.contains(&"World"));
    }

    #[test]
    fn rotated_text_reports_snapped_direction() {
        // Tm rotates the baseline 90° CCW.
        let bytes = pdf_with_text("BT /F1 12 Tf 0 1 -1 0 100 100 Tm (Up) Tj ET");
        let items = items_of(&bytes);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].direction, 90.0);
    }

    #[test]
    fn text_matrix_scales_font_size() {
        // Tm with scale 2 doubles the rendered size.
        let bytes = pdf_with_text("BT /F1 10 Tf 2 0 0 2 50 50 Tm (Big) Tj ET");
        let items = items_of(&bytes);
        assert!((items[0].font_size - 20.0).abs() < 0.01);
    }

    #[test]
    fn no_font_means_no_items() {
        // Tj without a preceding Tf has no encoding to decode through.
        let bytes = pdf_with_text("BT 72 720 Td (Hi) Tj ET");
        assert!(items_of(&bytes).is_empty());
    }

    #[test]
    fn lines_of_groups_by_baseline() {
        let item = |text: &str, y: f64, size: f64| TextItem {
            text: text.into(),
            x: 0.0,
            y,
            font_size: size,
            direction: 0.0,
        };
        let items = vec![
            item("Hello", 720.0, 10.0),
            item(" ", 720.0, 12.0),
            item("World", 720.0, 8.0),
            item("Next", 700.0, 10.0),
        ];
        let lines = lines_of(&items);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].0, "Hello World");
        assert_eq!(lines[0].1, 12.0); // largest font size on the line
        assert_eq!(lines[1].0, "Next");
    }

    #[test]
    fn direction_counts_bucket_by_quadrant() {
        let item = |text: &str, dir: f64| TextItem {
            text: text.into(),
            x: 0.0,
            y: 0.0,
            font_size: 10.0,
            direction: dir,
        };
        let items = vec![
            item("ab", 0.0),
            item("cd", 90.0),
            item("  ", 180.0), // whitespace does not count
            item("ef", 270.0),
        ];
        assert_eq!(page_direction_counts(&items), [2, 2, 0, 2]);
    }
}
