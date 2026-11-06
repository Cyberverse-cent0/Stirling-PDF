//! A mutable assembled line: text plus geometry — port of `Line.java`.
//! `source` is the extractor's line; `merged` holds fragments folded in.

use std::rc::Rc;

use super::model::{PdfLine, PdfWord};

#[derive(Debug)]
pub struct Line {
    pub source: PdfLine,
    pub text: String,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// Extra extractor fragments merged into this line.
    pub merged: Vec<PdfLine>,
    /// True for a line synthesised from an AcroForm value.
    pub synthetic: bool,
}

impl Line {
    pub fn new(source: PdfLine) -> Self {
        Self::with_text(source, String::new())
    }

    /// `text` empty means "use the source line's own text".
    pub fn with_text(source: PdfLine, text: String) -> Self {
        let text = if text.is_empty() { source.text() } else { text };
        Self {
            x: source.x,
            y: source.y,
            width: source.width,
            height: source.height,
            source,
            text,
            merged: Vec::new(),
            synthetic: false,
        }
    }

    /// Every word on the line, in x order, across all merged fragments.
    pub fn words(&self) -> Vec<&PdfWord> {
        if self.merged.is_empty() {
            return self.source.words.iter().collect();
        }
        let mut all: Vec<&PdfWord> = self.source.words.iter().collect();
        for extra in &self.merged {
            all.extend(extra.words.iter());
        }
        all.sort_by(|a, b| a.x.total_cmp(&b.x));
        all
    }

    /// Text for the heading/bold classifiers; an unmerged line keeps the
    /// extractor's own string (which may differ from `text` when soft
    /// hyphens were stripped at construction).
    pub fn detect_text(&self) -> String {
        if self.merged.is_empty() {
            self.source.text()
        } else {
            self.text.clone()
        }
    }

    pub fn detect_height(&self) -> f32 {
        if self.merged.is_empty() {
            self.source.height
        } else {
            self.height
        }
    }

    /// Top edge; PDF y grows upwards.
    pub fn top(&self) -> f32 {
        self.y + self.height
    }

    pub fn centre_y(&self) -> f32 {
        self.y + self.height / 2.0
    }

    pub fn centre_x(&self) -> f32 {
        self.x + self.width / 2.0
    }

    /// Left edge of the line's words, falling back to its bounding box.
    pub fn left(&self) -> f32 {
        let mut edge = f32::INFINITY;
        for w in self.words() {
            edge = edge.min(w.x);
        }
        if edge == f32::INFINITY {
            self.x
        } else {
            edge
        }
    }

    pub fn right(&self) -> f32 {
        let mut edge = f32::NEG_INFINITY;
        for w in self.words() {
            edge = edge.max(w.x + w.width);
        }
        if edge == f32::NEG_INFINITY {
            self.x + self.width
        } else {
            edge
        }
    }

    /// Left edge of the line's glyphs, ignoring space a word box carries.
    pub fn glyph_left(&self) -> f32 {
        let mut edge = f32::INFINITY;
        for w in self.words() {
            for c in &w.chars {
                if !c.is_whitespace() && !c.is_newline() {
                    edge = edge.min(c.x);
                }
            }
        }
        if edge == f32::INFINITY {
            self.x
        } else {
            edge
        }
    }

    pub fn glyph_right(&self) -> f32 {
        let mut edge = f32::NEG_INFINITY;
        for w in self.words() {
            for c in &w.chars {
                if !c.is_whitespace() && !c.is_newline() {
                    edge = edge.max(c.x + c.width);
                }
            }
        }
        if edge == f32::NEG_INFINITY {
            self.x + self.width
        } else {
            edge
        }
    }
}

/// Shared handle used across the pipeline so table rows and prose segments
/// refer to the same line (Java relied on object identity).
pub type SharedLine = Rc<Line>;

#[cfg(test)]
mod tests {
    use super::super::model::{PdfChar, PdfLine, PdfWord};
    use super::*;

    fn ch(c: char, x: f32, w: f32) -> PdfChar {
        PdfChar {
            ch: c,
            x,
            y: 0.0,
            width: w,
            height: 10.0,
            font_name: String::new(),
            font_size: 10.0,
        }
    }

    fn word(text: &str, x: f32, width: f32) -> PdfWord {
        let count = text.chars().count().max(1) as f32;
        let chars: Vec<PdfChar> = text
            .chars()
            .enumerate()
            .map(|(i, c)| ch(c, x + i as f32 * width / count, width / count))
            .collect();
        PdfWord {
            x,
            y: 0.0,
            width,
            height: 10.0,
            chars,
        }
    }

    fn line(words: Vec<PdfWord>, x: f32, y: f32, width: f32, height: f32) -> PdfLine {
        PdfLine {
            words,
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn with_empty_text_uses_source_text() {
        let source = line(vec![word("hi", 0.0, 10.0)], 0.0, 0.0, 10.0, 10.0);
        let l = Line::with_text(source, String::new());
        assert_eq!(l.text, "hi");
    }

    #[test]
    fn explicit_text_wins_over_source() {
        let source = line(vec![word("hi", 0.0, 10.0)], 0.0, 0.0, 10.0, 10.0);
        let l = Line::with_text(source, "override".to_string());
        assert_eq!(l.text, "override");
        assert_eq!(l.detect_text(), "hi");
    }

    #[test]
    fn geometry_edges() {
        // y is the bottom edge; top = y + height (PDF y grows up).
        let l = Line::new(line(vec![word("ab", 10.0, 20.0)], 10.0, 5.0, 20.0, 8.0));
        assert_eq!(l.top(), 13.0);
        assert_eq!(l.centre_y(), 9.0);
        assert_eq!(l.centre_x(), 20.0);
        assert_eq!(l.left(), 10.0);
        assert_eq!(l.right(), 30.0);
    }

    #[test]
    fn empty_words_fall_back_to_bbox() {
        let l = Line::new(line(vec![], 3.0, 4.0, 5.0, 6.0));
        assert_eq!(l.left(), 3.0);
        assert_eq!(l.right(), 8.0);
    }

    #[test]
    fn merged_words_sort_by_x() {
        let mut source = line(vec![word("b", 20.0, 5.0)], 20.0, 0.0, 5.0, 10.0);
        source.words.push(word("a", 0.0, 5.0));
        let mut l = Line::new(source);
        l.merged
            .push(line(vec![word("c", 40.0, 5.0)], 40.0, 0.0, 5.0, 10.0));
        let texts: Vec<String> = l.words().iter().map(|w| w.text()).collect();
        assert_eq!(texts, ["a", "b", "c"]);
        // Merged lines report the assembled text, not the source's.
        assert_eq!(l.detect_text(), l.text.clone());
    }

    #[test]
    fn glyph_edges_ignore_whitespace_chars() {
        let mut w = word("a b", 0.0, 30.0);
        // Push the space char's box far right; glyph edges must skip it.
        w.chars[1].x = 100.0;
        w.chars[1].width = 50.0;
        let l = Line::new(line(vec![w], 0.0, 0.0, 30.0, 10.0));
        assert!(l.glyph_right() < 60.0);
        assert!(l.right() >= 30.0);
    }
}
