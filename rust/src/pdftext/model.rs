//! Structured page text, the Rust counterpart of jpdfium's
//! `stirling.software.jpdfium.text` package: PDFium emits raw characters
//! with boxes; `build_lines`/`build_words` group them into the
//! line → word → char hierarchy every downstream stage consumes.
//!
//! Boxes are approximations: `CharItem` records the baseline origin and
//! advance, so the glyph box is rebuilt from the standard-14 ascent /
//! descent metrics in [`crate::fontmetrics`] and the snapped direction.
//! PDFium reports tight glyph bounds; these are advance-height bounds,
//! which the pipeline's relative thresholds tolerate.

use crate::text::CharItem;

/// One glyph with an axis-aligned bounding box, like jpdfium's `TextChar`.
/// `y` is the bottom edge; the page's y axis points up.
#[derive(Debug, Clone)]
pub struct PdfChar {
    pub ch: char,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub font_name: String,
    pub font_size: f32,
}

impl PdfChar {
    pub fn is_whitespace(&self) -> bool {
        self.ch.is_whitespace()
    }

    /// LF, CR, or the Unicode paragraph/line separators jpdfium treats as
    /// hard breaks.
    pub fn is_newline(&self) -> bool {
        matches!(self.ch, '\n' | '\r' | '\u{2028}' | '\u{2029}')
    }

    pub fn is_cased_letter_or_digit(&self) -> bool {
        self.ch.is_alphanumeric()
    }
}

/// A contiguous run of non-whitespace chars, like jpdfium's `TextWord`.
#[derive(Debug, Clone)]
pub struct PdfWord {
    pub chars: Vec<PdfChar>,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl PdfWord {
    pub fn text(&self) -> String {
        self.chars.iter().map(|c| c.ch).collect()
    }

    /// Word text with surrounding whitespace removed, the `strip()` the
    /// Java pipeline applies everywhere.
    pub fn stripped(&self) -> String {
        self.text().trim().to_string()
    }
}

/// A horizontal run of words sharing a baseline band, like jpdfium's
/// `TextLine`.
#[derive(Debug, Clone)]
pub struct PdfLine {
    pub words: Vec<PdfWord>,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl PdfLine {
    /// Words joined by single spaces, like jpdfium's `TextLine.text()`.
    pub fn text(&self) -> String {
        self.words
            .iter()
            .map(|w| w.text())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// One page's structured text: lines in extraction order plus the flat
/// char list, matching jpdfium's `PageText`.
#[derive(Debug, Clone, Default)]
pub struct PageText {
    pub page_index: usize,
    pub lines: Vec<PdfLine>,
    pub chars: Vec<PdfChar>,
}

impl PageText {
    /// Lines joined by newlines, like `PageText.plainText()`.
    pub fn plain_text(&self) -> String {
        self.lines
            .iter()
            .map(|l| l.text())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Converts one page's [`CharItem`]s into PDFium-style chars with boxes,
/// then groups them into lines and words exactly like
/// `PdfTextExtractor.buildLines`/`buildWords`.
pub fn page_text(page_index: usize, items: &[CharItem]) -> PageText {
    let chars: Vec<PdfChar> = items.iter().map(pdf_char).collect();
    let lines = build_lines(&chars);
    PageText {
        page_index,
        lines,
        chars,
    }
}

/// Structured text for every page, in page order — `PdfTextExtractor.extractAll`.
pub fn document_text(doc: &lopdf::Document) -> Vec<PageText> {
    crate::pdfdoc::page_ids(doc)
        .iter()
        .enumerate()
        .map(|(i, id)| page_text(i, &crate::text::page_chars(doc, *id)))
        .collect()
}

/// Rebuilds the axis-aligned glyph box around a [`CharItem`]'s baseline
/// origin: the advance runs along the snapped direction and the font's
/// ascent/descent run perpendicular to it.
fn pdf_char(item: &CharItem) -> PdfChar {
    let (ascent, descent) = crate::fontmetrics::ascent_descent(&item.font);
    let size = item.font_size as f32;
    let asc = ascent as f32 / 1000.0 * size;
    let desc = descent as f32 / 1000.0 * size;
    let w = item.w as f32;

    let rad = (item.direction as f32).to_radians();
    let (sin, cos) = rad.sin_cos();
    // Baseline direction and the ascender direction (baseline rotated +90°).
    let (ax, ay) = (cos, sin);
    let (ux, uy) = (-sin, cos);

    let (x, y) = (item.x as f32, item.y as f32);
    let mut min_x = x;
    let mut max_x = x;
    let mut min_y = y;
    let mut max_y = y;
    for &(along, up) in &[
        (0.0f32, 0.0f32),
        (w, 0.0),
        (0.0, desc),
        (0.0, asc),
        (w, desc),
        (w, asc),
    ] {
        let px = x + ax * along + ux * up;
        let py = y + ay * along + uy * up;
        min_x = min_x.min(px);
        max_x = max_x.max(px);
        min_y = min_y.min(py);
        max_y = max_y.max(py);
    }
    PdfChar {
        ch: item.text.chars().next().unwrap_or(' '),
        x: min_x,
        y: min_y,
        width: (max_x - min_x).max(0.0),
        height: (max_y - min_y).max(0.0),
        font_name: item.font.clone(),
        font_size: size,
    }
}

/// jpdfium `buildLines`: consecutive chars join a line while their bottom
/// edge stays within half the running char's height of the line's anchor.
fn build_lines(chars: &[PdfChar]) -> Vec<PdfLine> {
    if chars.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut current: Vec<PdfChar> = Vec::new();
    let mut current_y = chars[0].y;
    let mut threshold = chars[0].height * 0.5;
    for ch in chars {
        if !current.is_empty() && (ch.y - current_y).abs() > threshold {
            lines.push(build_line(std::mem::take(&mut current)));
            threshold = ch.height * 0.5;
        }
        current.push(ch.clone());
        current_y = ch.y;
    }
    lines.push(build_line(current));
    lines
}

fn build_line(chars: Vec<PdfChar>) -> PdfLine {
    let words = build_words(&chars);
    if chars.is_empty() {
        return PdfLine {
            words,
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 0.0,
        };
    }
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for c in &chars {
        min_x = min_x.min(c.x);
        min_y = min_y.min(c.y);
        max_x = max_x.max(c.x + c.width);
        max_y = max_y.max(c.y + c.height);
    }
    PdfLine {
        words,
        x: min_x,
        y: min_y,
        width: max_x - min_x,
        height: max_y - min_y,
    }
}

/// jpdfium `buildWords`: whitespace and newline chars split words; each
/// word's box is the union of its chars' boxes.
fn build_words(chars: &[PdfChar]) -> Vec<PdfWord> {
    let mut words = Vec::new();
    let mut current: Vec<PdfChar> = Vec::new();
    for ch in chars {
        if ch.is_whitespace() || ch.is_newline() {
            if !current.is_empty() {
                words.push(build_word(std::mem::take(&mut current)));
            }
        } else {
            current.push(ch.clone());
        }
    }
    if !current.is_empty() {
        words.push(build_word(current));
    }
    words
}

fn build_word(chars: Vec<PdfChar>) -> PdfWord {
    if chars.is_empty() {
        return PdfWord {
            chars,
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 0.0,
        };
    }
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for c in &chars {
        min_x = min_x.min(c.x);
        min_y = min_y.min(c.y);
        max_x = max_x.max(c.x + c.width);
        max_y = max_y.max(c.y + c.height);
    }
    PdfWord {
        chars,
        x: min_x,
        y: min_y,
        width: max_x - min_x,
        height: max_y - min_y,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(c: char, x: f32, y: f32, w: f32, h: f32) -> PdfChar {
        PdfChar {
            ch: c,
            x,
            y,
            width: w,
            height: h,
            font_name: String::new(),
            font_size: 10.0,
        }
    }

    #[test]
    fn words_split_on_whitespace() {
        let chars = vec![
            ch('h', 0.0, 0.0, 5.0, 10.0),
            ch('i', 5.0, 0.0, 5.0, 10.0),
            ch(' ', 10.0, 0.0, 3.0, 10.0),
            ch('y', 13.0, 0.0, 5.0, 10.0),
            ch('o', 18.0, 0.0, 5.0, 10.0),
        ];
        let line = build_line(chars);
        assert_eq!(line.words.len(), 2);
        assert_eq!(line.words[0].text(), "hi");
        assert_eq!(line.words[1].text(), "yo");
        assert_eq!(line.words[0].x, 0.0);
        assert_eq!(line.words[0].width, 10.0);
        assert_eq!(line.text(), "hi yo");
    }

    #[test]
    fn lines_split_on_baseline_jump() {
        // Second row's bottom edge is 20pt below the first — beyond half
        // the char height (5pt), so a new line starts.
        let chars = vec![
            ch('a', 0.0, 0.0, 5.0, 10.0),
            ch('b', 20.0, -20.0, 5.0, 10.0),
        ];
        let lines = build_lines(&chars);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].words[0].text(), "a");
        assert_eq!(lines[1].words[0].text(), "b");
    }

    #[test]
    fn lines_keep_nearby_baselines_together() {
        let chars = vec![
            ch('a', 0.0, 0.0, 5.0, 10.0),
            // 2pt below: inside the 5pt threshold.
            ch('b', 10.0, -2.0, 5.0, 10.0),
        ];
        let lines = build_lines(&chars);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn newline_chars_split_words() {
        let chars = vec![
            ch('a', 0.0, 0.0, 5.0, 10.0),
            ch('\n', 5.0, 0.0, 2.0, 10.0),
            ch('b', 8.0, 0.0, 5.0, 10.0),
        ];
        let line = build_line(chars);
        assert_eq!(line.words.len(), 2);
    }

    #[test]
    fn plain_text_joins_lines() {
        let page = PageText {
            page_index: 0,
            lines: vec![
                build_line(vec![ch('h', 0.0, 0.0, 5.0, 10.0)]),
                build_line(vec![ch('i', 0.0, -12.0, 5.0, 10.0)]),
            ],
            chars: Vec::new(),
        };
        assert_eq!(page.plain_text(), "h\ni");
    }
}
