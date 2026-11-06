//! Text-match redaction: the Rust counterpart of Java's
//! `TextRedactionService.findTextToRedact` (PDFBox `MultiPatternTextFinder`)
//! plus the geometry of `ManualRedactionService.redactFoundText`.
//!
//! jpdfium's `PdfRedactor` performs verified content removal; lopdf has no
//! equivalent, so this module only locates matches. Handlers draw cover
//! rectangles — the same overlay path Java itself falls back to when native
//! redaction fails (`RedactController`'s catch branch calling
//! `finalizeRedaction`, which passes `isTextRemovalMode=false` on that path).
//!
//! Matching model: `MultiPatternTextFinder` rebuilds each page's text with
//! PDFBox, a single space between words and `\n` between lines, keeping one
//! glyph position per character (null for separators). [`PageScan`] mirrors
//! that over [`PageText`]: words are rejoined with single spaces, so runs of
//! real space glyphs collapse — a match spanning irregular spacing that
//! PDFBox would find can be missed here. Regex syntax is the `regex` crate's,
//! not Java's: lookaround, backreferences and possessive quantifiers do not
//! compile and those requests are forwarded to Java instead.

use std::collections::HashMap;

use lopdf::content::{Content, Operation};
use lopdf::{Document, Object, ObjectId};
use regex::{Regex, RegexBuilder};

use super::model::PageText;
use crate::text::{self, Resources};

/// One located match — `PDFText` parity: 0-based page index and screen
/// coordinates (top-left origin, y grows downward, `y1` < `y2`).
#[derive(Debug, Clone)]
pub struct FoundText {
    pub page_index: usize,
    pub x1: f64,
    pub y1: f64,
    pub x2: f64,
    pub y2: f64,
    pub text: String,
}

/// `RedactPdfRequest.listOfText`: one term per line, trimmed, empties and
/// terms over 4096 UTF-16 units dropped (`String.length()` semantics).
pub fn parse_terms(raw: &str) -> Vec<String> {
    raw.split('\n')
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.encode_utf16().count() <= 4096)
        .map(str::to_string)
        .collect()
}

/// A compiled search term. `check_neighbors` marks the single-character
/// whole-word case: Java wraps it in `(?<![\w])…(?![\w])`, which Rust regex
/// cannot express, so the boundary test runs after each match instead.
pub struct TermPattern {
    regex: Regex,
    check_neighbors: bool,
}

/// `TextFinderUtils.createOptimizedSearchPatterns` + dedup (`Collectors.toSet`).
/// Multi-character whole-word terms get Java's `\b…\b` wrap; the ASCII-scoped
/// `(?-u:\b)` keeps `\b` ASCII like Java's default `\w`.
pub fn compile_terms(
    terms: &[String],
    use_regex: bool,
    whole_word: bool,
    case_sensitive: bool,
) -> Result<Vec<TermPattern>, regex::Error> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for term in terms {
        let term = term.trim();
        if term.is_empty() || term.encode_utf16().count() > 4096 || !seen.insert(term.to_string()) {
            continue;
        }
        let mut pattern = if use_regex {
            term.to_string()
        } else {
            regex::escape(term)
        };
        let mut check_neighbors = false;
        if whole_word {
            // Java `applyWordBoundaries`: `length()==1` is UTF-16 units.
            if term.encode_utf16().count() == 1 {
                check_neighbors = true;
            } else {
                pattern = format!("(?-u:\\b){pattern}(?-u:\\b)");
            }
        }
        let regex = RegexBuilder::new(&pattern)
            .case_insensitive(!case_sensitive)
            .build()?;
        out.push(TermPattern {
            regex,
            check_neighbors,
        });
    }
    Ok(out)
}

/// `TextRedactionService.findTextToRedact`: scan every page once per pattern
/// and return all hits. `page_heights[i]` is page i's crop-box height, the
/// value PDFBox's stripper uses for its screen↔PDF y flip.
pub fn find_text(
    pages: &[PageText],
    page_heights: &[f32],
    terms: &[String],
    use_regex: bool,
    whole_word: bool,
    case_sensitive: bool,
) -> Result<Vec<FoundText>, regex::Error> {
    let patterns = compile_terms(terms, use_regex, whole_word, case_sensitive)?;
    Ok(find_matches(pages, page_heights, &patterns))
}

/// Runs already-compiled patterns over the pages; shared by `find_text` and
/// the range-anchor fallback search, which tries several candidate patterns.
pub fn find_matches(
    pages: &[PageText],
    page_heights: &[f32],
    patterns: &[TermPattern],
) -> Vec<FoundText> {
    let mut out = Vec::new();
    for (index, page) in pages.iter().enumerate() {
        let height = page_heights.get(index).copied().unwrap_or(0.0);
        let scan = PageScan::build(page, height);
        if scan.text.is_empty() {
            continue;
        }
        for pattern in patterns {
            for found in pattern.regex.find_iter(&scan.text) {
                let (start, end) = scan.char_range(found.start(), found.end());
                if pattern.check_neighbors && !neighbors_clear(&scan.chars, start, end) {
                    continue;
                }
                if let Some(hit) =
                    scan.resolve(index, start, end, &scan.text[found.start()..found.end()])
                {
                    out.push(hit);
                }
            }
        }
    }
    out
}

/// Java `\w` is ASCII `[A-Za-z0-9_]` here (no UNICODE_CHARACTER_CLASS flag).
fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn neighbors_clear(chars: &[char], start: usize, end: usize) -> bool {
    let before = start.checked_sub(1).map(|i| chars[i]);
    let after = chars.get(end).copied();
    !before.is_some_and(is_word_char) && !after.is_some_and(is_word_char)
}

#[derive(Clone, Copy)]
struct GlyphBox {
    x: f32,
    /// Bottom edge, PDF user space (y up).
    y: f32,
    w: f32,
    h: f32,
}

/// One page rebuilt the way `MultiPatternTextFinder` sees it: the visible
/// text plus one glyph box (or separator `None`) per character.
struct PageScan {
    text: String,
    chars: Vec<char>,
    positions: Vec<Option<GlyphBox>>,
    /// Byte offset of each char in `text`, for regex-offset → char-index.
    char_starts: Vec<usize>,
    height: f32,
}

impl PageScan {
    fn build(page: &PageText, height: f32) -> Self {
        let mut text = String::new();
        let mut chars = Vec::new();
        let mut positions = Vec::new();
        let mut char_starts = Vec::new();
        for (line_index, line) in page.lines.iter().enumerate() {
            if line_index > 0 {
                Self::push(
                    &mut text,
                    &mut chars,
                    &mut positions,
                    &mut char_starts,
                    '\n',
                    None,
                );
            }
            for (word_index, word) in line.words.iter().enumerate() {
                if word_index > 0 {
                    Self::push(
                        &mut text,
                        &mut chars,
                        &mut positions,
                        &mut char_starts,
                        ' ',
                        None,
                    );
                }
                for c in &word.chars {
                    Self::push(
                        &mut text,
                        &mut chars,
                        &mut positions,
                        &mut char_starts,
                        c.ch,
                        Some(GlyphBox {
                            x: c.x,
                            y: c.y,
                            w: c.width,
                            h: c.height,
                        }),
                    );
                }
            }
        }
        Self {
            text,
            chars,
            positions,
            char_starts,
            height,
        }
    }

    fn push(
        text: &mut String,
        chars: &mut Vec<char>,
        positions: &mut Vec<Option<GlyphBox>>,
        char_starts: &mut Vec<usize>,
        ch: char,
        glyph: Option<GlyphBox>,
    ) {
        char_starts.push(text.len());
        text.push(ch);
        chars.push(ch);
        positions.push(glyph);
    }

    /// Char-index range `[start, end)` covering the byte range; regex match
    /// offsets always land on char boundaries.
    fn char_range(&self, byte_start: usize, byte_end: usize) -> (usize, usize) {
        (
            self.char_starts
                .partition_point(|&b| b < byte_start)
                .min(self.chars.len()),
            self.char_starts
                .partition_point(|&b| b < byte_end)
                .min(self.chars.len()),
        )
    }

    /// `MultiPatternTextFinder.resolveMatchPosition`: union the glyph boxes
    /// under the match; when every covered position is a separator, take the
    /// first real glyph within ±5 chars, like Java's fallback scan.
    fn resolve(
        &self,
        page_index: usize,
        start: usize,
        end: usize,
        matched: &str,
    ) -> Option<FoundText> {
        let mut min_x = f32::MAX;
        let mut min_y = f32::MAX;
        let mut max_x = f32::MIN;
        let mut max_y = f32::MIN;
        let mut union = |g: GlyphBox, found: &mut bool| {
            *found = true;
            min_x = min_x.min(g.x);
            max_x = max_x.max(g.x + g.w);
            min_y = min_y.min(g.y);
            max_y = max_y.max(g.y + g.h);
        };
        let mut found = false;
        for i in start..end.min(self.positions.len()) {
            if let Some(g) = self.positions[i] {
                union(g, &mut found);
            }
        }
        if !found && start < self.positions.len() {
            let from = start.saturating_sub(5);
            let to = (end + 5).min(self.positions.len());
            for i in from..to {
                if let Some(g) = self.positions[i] {
                    union(g, &mut found);
                    break;
                }
            }
        }
        if !found {
            return None;
        }
        // User space (y up) → screen space (top-left origin, y down), the
        // conversion PDFBox's stripper applies with the page height.
        let h = self.height as f64;
        Some(FoundText {
            page_index,
            x1: f64::from(min_x),
            y1: h - f64::from(max_y),
            x2: f64::from(max_x),
            y2: h - f64::from(min_y),
            text: matched.to_string(),
        })
    }
}

// ---------------------------------------------------------------------------
// Line boxes, column layout, image boxes — the `AllTextLineExtractor`,
// `PageColumnLayout` and `PageImageLocator` counterparts range redaction and
// page wipes need.
// ---------------------------------------------------------------------------

const LINE_Y_TOLERANCE: f32 = 3.0;
const COLUMN_GAP_X: f32 = 14.0;

struct ScreenLine {
    x1: f32,
    /// Top edge, screen coordinates (smaller y = higher on the page).
    y_top: f32,
    x2: f32,
    /// Bottom edge, screen coordinates.
    y_bottom: f32,
}

/// `AllTextLineExtractor`: glyphs sorted by position, then split into lines
/// on a >3pt baseline jump or a >14pt horizontal gap. Whitespace glyphs are
/// skipped like Java's `unicode.isBlank()` filter.
fn screen_lines(page: &PageText, height: f32) -> Vec<ScreenLine> {
    let mut glyphs: Vec<(f32, f32, f32, f32)> = page
        .chars
        .iter()
        .filter(|c| !c.is_whitespace())
        .map(|c| {
            // screen baseline ≈ bottom edge flipped; top = flipped top edge.
            (c.y, c.x, c.x + c.width, c.y + c.height)
        })
        .collect();
    // Reading order: highest on the page first (largest y-up), then left to
    // right — PDFBox's `setSortByPosition(true)`.
    glyphs.sort_by(|a, b| {
        b.3.partial_cmp(&a.3)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
    });

    let mut lines = Vec::new();
    let mut current: Vec<(f32, f32, f32, f32)> = Vec::new();
    let mut last_screen_y = f32::NAN;
    let mut last_right = f32::NAN;
    for g in glyphs {
        let screen_y = height - g.0;
        let y_jump = !last_screen_y.is_nan() && (screen_y - last_screen_y).abs() > LINE_Y_TOLERANCE;
        let x_jump = !last_right.is_nan() && (g.1 - last_right) > COLUMN_GAP_X;
        if y_jump || x_jump {
            if let Some(line) = flush_line(&mut current, height) {
                lines.push(line);
            }
        }
        last_screen_y = screen_y;
        last_right = g.2;
        current.push(g);
    }
    if let Some(line) = flush_line(&mut current, height) {
        lines.push(line);
    }
    lines
}

/// `(bottom_y_up, x, right, top_y_up)` glyphs → one screen-space line box.
fn flush_line(glyphs: &mut Vec<(f32, f32, f32, f32)>, height: f32) -> Option<ScreenLine> {
    if glyphs.is_empty() {
        return None;
    }
    let mut min_x = f32::MAX;
    let mut max_x = f32::MIN;
    let mut max_top = f32::MIN; // largest y-up top edge
    let mut min_bottom = f32::MAX; // smallest y-up bottom edge
    for &(bottom, x, right, top) in glyphs.iter() {
        min_x = min_x.min(x);
        max_x = max_x.max(right);
        max_top = max_top.max(top);
        min_bottom = min_bottom.min(bottom);
    }
    glyphs.clear();
    Some(ScreenLine {
        x1: min_x,
        y_top: height - max_top,
        x2: max_x,
        y_bottom: height - min_bottom,
    })
}

/// `AllTextLineExtractor.getScreenLineBoxes`: `[x1, yTop, x2, yBottom]`,
/// screen coordinates.
pub fn line_screen_boxes(page: &PageText, height: f32) -> Vec<[f32; 4]> {
    screen_lines(page, height)
        .into_iter()
        .map(|l| [l.x1, l.y_top, l.x2, l.y_bottom])
        .collect()
}

/// `AllTextLineExtractor.getLineBoxes`: `[x1, yBottom, x2, yTop]` in PDF
/// user space (y up) — what page wipes draw.
pub fn line_pdf_boxes(page: &PageText, height: f32) -> Vec<[f32; 4]> {
    screen_lines(page, height)
        .into_iter()
        .map(|l| [l.x1, height - l.y_bottom, l.x2, height - l.y_top])
        .collect()
}

const SPAN_SLACK_PT: f32 = 2.0;
const MIDPOINT_SLACK_PT: f32 = 30.0;
const MIN_COLUMN_LINE_WIDTH_PT: f32 = 100.0;
const MIN_SIDE_LINES: usize = 3;

/// `PageColumnLayout`: one or two columns detected by a midpoint vote.
#[derive(Debug, Clone)]
pub struct ColumnLayout {
    columns: Vec<(f32, f32)>,
    gutters: Vec<(f32, f32)>,
}

impl ColumnLayout {
    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    /// Column index containing the `[x1, x2]` midpoint, closest column on a
    /// gutter miss — `PageColumnLayout.columnOf`.
    pub fn column_of(&self, x1: f32, x2: f32) -> usize {
        let mid = (x1 + x2) * 0.5;
        let mut best = 0;
        let mut best_dist = f32::MAX;
        for (i, &(l, r)) in self.columns.iter().enumerate() {
            if mid >= l && mid <= r {
                return i;
            }
            let dist = if mid < l { l - mid } else { mid - r };
            if dist < best_dist {
                best_dist = dist;
                best = i;
            }
        }
        best
    }

    /// Columns overlapping `[x1, x2]` by more than [`SPAN_SLACK_PT`]; falls
    /// back to [`Self::column_of`] when nothing overlaps — `columnsCrossing`.
    pub fn columns_crossing(&self, x1: f32, x2: f32) -> Vec<usize> {
        let mut hits: Vec<usize> = self
            .columns
            .iter()
            .enumerate()
            .filter(|(_, &(l, r))| x2.min(r) - x1.max(l) > SPAN_SLACK_PT)
            .map(|(i, _)| i)
            .collect();
        if hits.is_empty() {
            hits.push(self.column_of(x1, x2));
        }
        hits
    }
}

/// `PageColumnLayout.fromLineBoxes` over `[x1, _, x2, _]` line boxes.
pub fn column_layout(line_boxes: &[[f32; 4]], page_width: f32) -> ColumnLayout {
    if line_boxes.is_empty() {
        return ColumnLayout {
            columns: vec![(0.0, page_width)],
            gutters: Vec::new(),
        };
    }
    let page_mid = page_width / 2.0;
    let mut left = 0usize;
    let mut right = 0usize;
    for lb in line_boxes {
        let width = lb[2] - lb[0];
        if width < MIN_COLUMN_LINE_WIDTH_PT {
            continue;
        }
        let mid = (lb[0] + lb[2]) * 0.5;
        if mid < page_mid - MIDPOINT_SLACK_PT {
            left += 1;
        } else if mid > page_mid + MIDPOINT_SLACK_PT {
            right += 1;
        }
    }
    if left < MIN_SIDE_LINES || right < MIN_SIDE_LINES {
        return ColumnLayout {
            columns: vec![(0.0, page_width)],
            gutters: Vec::new(),
        };
    }
    let gutter_l = page_mid - MIDPOINT_SLACK_PT;
    let gutter_r = page_mid + MIDPOINT_SLACK_PT;
    ColumnLayout {
        columns: vec![(0.0, gutter_l), (gutter_r, page_width)],
        gutters: vec![(gutter_l, gutter_r)],
    }
}

/// `PageImageLocator`: every image XObject's bounding box in PDF user space
/// (y up), found by walking `Do` operators under the CTM — including inside
/// form XObjects. Inline images (`BI`/`ID`/`EI`) are not covered.
pub fn image_boxes(doc: &Document, page_id: ObjectId) -> Vec<[f64; 4]> {
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
    let mut walker = ImageWalker {
        doc,
        ctm: text::IDENTITY,
        ctm_stack: Vec::new(),
        boxes: Vec::new(),
    };
    if let Ok(content) = doc.get_and_decode_page_content(page_id) {
        walker.walk(&resources, &content.operations, 0);
    }
    walker.boxes
}

struct ImageWalker<'a> {
    doc: &'a Document,
    ctm: text::Matrix,
    ctm_stack: Vec<text::Matrix>,
    boxes: Vec<[f64; 4]>,
}

impl<'a> ImageWalker<'a> {
    fn walk(&mut self, resources: &Resources<'a>, operations: &[Operation], depth: usize) {
        if depth >= text::MAX_XOBJECT_DEPTH {
            return;
        }
        for operation in operations {
            let operands = &operation.operands;
            match operation.operator.as_str() {
                "q" => self.ctm_stack.push(self.ctm),
                "Q" => {
                    if let Some(ctm) = self.ctm_stack.pop() {
                        self.ctm = ctm;
                    }
                }
                "cm" => {
                    if let Some(m) = text::matrix_operand(operands) {
                        self.ctm = text::mat_mul(self.ctm, m);
                    }
                }
                "Do" => self.do_xobject(resources, operands, depth),
                _ => {}
            }
        }
    }

    fn do_xobject(&mut self, resources: &Resources<'a>, operands: &[Object], depth: usize) {
        let Some(name) = operands.first().and_then(|o| o.as_name().ok()) else {
            return;
        };
        let Some((id, dict)) = resources.xobject(name) else {
            return;
        };
        let subtype = dict
            .get(b"Subtype")
            .ok()
            .and_then(|s| s.as_name().ok())
            .map(|s| s.to_vec());
        match subtype.as_deref() {
            Some(b"Image") => {
                // The image covers the unit square in image space.
                let mut min_x = f64::MAX;
                let mut min_y = f64::MAX;
                let mut max_x = f64::MIN;
                let mut max_y = f64::MIN;
                for (x, y) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)] {
                    let (px, py) = text::apply(self.ctm, x, y);
                    min_x = min_x.min(px);
                    max_x = max_x.max(px);
                    min_y = min_y.min(py);
                    max_y = max_y.max(py);
                }
                self.boxes.push([min_x, min_y, max_x, max_y]);
            }
            Some(b"Form") => {
                let saved = self.ctm;
                if let Some(m) = dict
                    .get(b"Matrix")
                    .ok()
                    .and_then(|v| v.as_array().ok())
                    .and_then(|a| text::matrix_operand(a))
                {
                    self.ctm = text::mat_mul(self.ctm, m);
                }
                let form_resources = resources.of_form(dict);
                let content = self
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
                self.ctm = saved;
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Anchored ranges — `RedactExecuteService.collectRangeBlocks` and helpers.
// ---------------------------------------------------------------------------

/// `RedactExecuteService.findWithFallbacks`: progressively more permissive
/// anchor candidates, first non-empty scan wins. A candidate whose pattern
/// does not compile simply yields no hits, like Java's per-term catch.
fn find_with_fallbacks(pages: &[PageText], page_heights: &[f32], raw: &str) -> Vec<FoundText> {
    let trimmed = raw.trim();
    let collapsed = collapse_letter_spacing(trimmed);
    let tolerant = punctuation_tolerant_regex(trimmed);

    let mut candidates: Vec<(String, bool)> =
        vec![(trimmed.to_string(), true), (trimmed.to_string(), false)];
    if collapsed != trimmed {
        candidates.push((collapsed.clone(), true));
        candidates.push((collapsed, false));
    }
    if let Some(tolerant) = tolerant {
        if tolerant != trimmed {
            candidates.push((tolerant, true));
        }
    }
    if trimmed.contains('\n') {
        let first_line = trimmed
            .split('\n')
            .map(str::trim)
            .find(|s| !s.is_empty())
            .filter(|s| s.encode_utf16().count() >= 4);
        if let Some(first) = first_line {
            let first_collapsed = collapse_letter_spacing(first);
            let first_tolerant = punctuation_tolerant_regex(first);
            candidates.push((first.to_string(), false));
            if first_collapsed != first {
                candidates.push((first_collapsed, false));
            }
            if let Some(t) = first_tolerant {
                if t != first {
                    candidates.push((t, true));
                }
            }
        }
    }

    for (pattern, use_regex) in candidates {
        if let Ok(hits) = find_text(pages, page_heights, &[pattern], use_regex, false, false) {
            if !hits.is_empty() {
                return hits;
            }
        }
    }
    Vec::new()
}

/// `collapseLetterSpacing`: rejoins letter-spaced extraction like
/// `"T a b l e"` back into words.
fn collapse_letter_spacing(text: &str) -> String {
    let tokens: Vec<&str> = text.split(' ').collect();
    let mut result = String::new();
    let mut current = String::new();
    for token in tokens {
        if token.is_empty() {
            if !current.is_empty() {
                if !result.is_empty() {
                    result.push(' ');
                }
                result.push_str(&current);
                current.clear();
            }
        } else if token.encode_utf16().count() == 1 {
            current.push_str(token);
        } else {
            if !current.is_empty() {
                if !result.is_empty() {
                    result.push(' ');
                }
                result.push_str(&current);
                current.clear();
            }
            if !result.is_empty() {
                result.push(' ');
            }
            result.push_str(token);
        }
    }
    if !current.is_empty() {
        if !result.is_empty() {
            result.push(' ');
        }
        result.push_str(&current);
    }
    result.trim().to_string()
}

/// `punctuationTolerantRegex`: alphanumeric runs joined by `[^\w]*`-style
/// gaps — written as `[^A-Za-z0-9_]*` to match Java's ASCII `\W` exactly.
fn punctuation_tolerant_regex(raw: &str) -> Option<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in raw.chars() {
        if ch.is_alphanumeric() {
            current.push(ch);
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    if tokens.len() < 2 {
        return None;
    }
    let mut out = String::new();
    for (i, token) in tokens.iter().enumerate() {
        if i > 0 {
            out.push_str("[^A-Za-z0-9_]*");
        }
        out.push_str(&regex::escape(token));
    }
    Some(out)
}

struct Anchor {
    page: usize,
    col: usize,
    /// Screen-space top of the anchor match (`PDFText.y1`).
    y: f64,
    hit: FoundText,
}

/// Reading order: page, then column, then screen y — `READING_ORDER`.
fn anchor_before(a: &Anchor, b: &Anchor) -> std::cmp::Ordering {
    a.page
        .cmp(&b.page)
        .then(a.col.cmp(&b.col))
        .then(a.y.partial_cmp(&b.y).unwrap_or(std::cmp::Ordering::Equal))
}

struct RangeContext<'a> {
    pages: &'a [PageText],
    page_heights: &'a [f32],
    page_widths: &'a [f32],
    images: &'a [Vec<[f64; 4]>],
    layouts: HashMap<usize, ColumnLayout>,
}

impl<'a> RangeContext<'a> {
    fn layout_for(&mut self, page: usize) -> ColumnLayout {
        if !self.layouts.contains_key(&page) {
            let boxes = self
                .page_heights
                .get(page)
                .map(|&h| line_screen_boxes(&self.pages[page], h))
                .unwrap_or_default();
            let width = self.page_widths.get(page).copied().unwrap_or(0.0);
            self.layouts.insert(page, column_layout(&boxes, width));
        }
        self.layouts.get(&page).cloned().unwrap_or(ColumnLayout {
            columns: vec![(0.0, self.page_widths.get(page).copied().unwrap_or(0.0))],
            gutters: Vec::new(),
        })
    }

    fn anchors_for(&mut self, hits: &[FoundText]) -> Vec<Anchor> {
        let mut anchors = Vec::new();
        for hit in hits {
            let layout = self.layout_for(hit.page_index);
            anchors.push(Anchor {
                page: hit.page_index,
                col: layout.column_of(hit.x1 as f32, hit.x2 as f32),
                y: hit.y1,
                hit: hit.clone(),
            });
        }
        anchors.sort_by(anchor_before);
        anchors
    }
}

/// `RedactExecuteService.collectRangeBlocks`: every line box and image box
/// between the start and end anchors in reading order (columns respected).
/// `images[p]` holds page p's [`image_boxes`] output (y up); `page_heights`
/// and `page_widths` are the crop-box dimensions per page.
pub fn range_blocks(
    pages: &[PageText],
    page_heights: &[f32],
    page_widths: &[f32],
    images: &[Vec<[f64; 4]>],
    start: &str,
    end: &str,
) -> Vec<FoundText> {
    let open_ended = end.trim().is_empty();
    let start_hits = find_with_fallbacks(pages, page_heights, start);
    if start_hits.is_empty() {
        return Vec::new();
    }
    let end_hits = if open_ended {
        Vec::new()
    } else {
        let hits = find_with_fallbacks(pages, page_heights, end);
        if hits.is_empty() {
            return Vec::new();
        }
        hits
    };

    let mut ctx = RangeContext {
        pages,
        page_heights,
        page_widths,
        images,
        layouts: HashMap::new(),
    };
    let starts = ctx.anchors_for(&start_hits);
    let ends = ctx.anchors_for(&end_hits);

    let total_pages = pages.len();
    let mut blocks = Vec::new();
    for start_anchor in &starts {
        let (end_page, end_col, end_y);
        if open_ended {
            end_page = total_pages.saturating_sub(1);
            end_col = ctx.layout_for(end_page).column_count().saturating_sub(1);
            end_y = f64::INFINITY;
        } else {
            let Some(end_anchor) = ends.iter().find(|e| anchor_before(e, start_anchor).is_gt())
            else {
                continue;
            };
            end_page = end_anchor.page;
            end_col = end_anchor.col;
            // Bottom of the end anchor so its own line is included.
            end_y = end_anchor.hit.y2;
        }
        if start_anchor.page > end_page {
            continue;
        }
        ctx.collect_blocks_for_range(start_anchor, end_page, end_col, end_y, &mut blocks);
    }
    blocks
}

impl<'a> RangeContext<'a> {
    /// `collectBlocksForRange` + `emitColumnSlices` + `inColumnZone`.
    fn collect_blocks_for_range(
        &mut self,
        start: &Anchor,
        end_page: usize,
        end_col: usize,
        end_y: f64,
        blocks: &mut Vec<FoundText>,
    ) {
        let last_page = self.pages.len().saturating_sub(1);
        for page in start.page..=end_page.min(last_page) {
            let Some(&height) = self.page_heights.get(page) else {
                continue;
            };
            let layout = self.layout_for(page);
            let emit =
                |x1: f32, x2: f32, y_top: f32, y_bottom: f32, blocks: &mut Vec<FoundText>| {
                    emit_column_slices(
                        page, &layout, x1, x2, y_top, y_bottom, start.page, start.col, start.y,
                        end_page, end_col, end_y, blocks,
                    );
                };
            for sb in line_screen_boxes(&self.pages[page], height) {
                emit(sb[0], sb[2], sb[1], sb[3], blocks);
            }
            if let Some(page_images) = self.images.get(page) {
                for ib in page_images {
                    // ImageBox coordinates are y up; the predicate works in
                    // screen space like the line boxes.
                    let screen_top = f64::from(height) - ib[3];
                    let screen_bottom = f64::from(height) - ib[1];
                    emit(
                        ib[0] as f32,
                        ib[2] as f32,
                        screen_top as f32,
                        screen_bottom as f32,
                        blocks,
                    );
                }
            }
        }
    }
}

/// `emitColumnSlices`: a box inside one column is kept when the reading-order
/// predicate accepts it; a box crossing columns is kept only when every
/// column it touches is inside the zone.
#[allow(clippy::too_many_arguments)]
fn emit_column_slices(
    page: usize,
    layout: &ColumnLayout,
    x1: f32,
    x2: f32,
    y_top: f32,
    y_bottom: f32,
    start_page: usize,
    start_col: usize,
    start_y: f64,
    end_page: usize,
    end_col: usize,
    end_y: f64,
    blocks: &mut Vec<FoundText>,
) {
    let cols = layout.columns_crossing(x1, x2);
    if cols.len() == 1 {
        if in_column_zone(
            page, cols[0], y_bottom, start_page, start_col, start_y, end_page, end_col, end_y,
        ) {
            blocks.push(FoundText {
                page_index: page,
                x1: f64::from(x1),
                y1: f64::from(y_top),
                x2: f64::from(x2),
                y2: f64::from(y_bottom),
                text: String::new(),
            });
        }
        return;
    }
    for &col in &cols {
        if !in_column_zone(
            page, col, y_bottom, start_page, start_col, start_y, end_page, end_col, end_y,
        ) {
            return;
        }
    }
    blocks.push(FoundText {
        page_index: page,
        x1: f64::from(x1),
        y1: f64::from(y_top),
        x2: f64::from(x2),
        y2: f64::from(y_bottom),
        text: String::new(),
    });
}

/// `inColumnZone`: is `(page, col, yBottom)` between the anchors, inclusive?
/// `yBottom` is the screen-space bottom of the candidate box.
#[allow(clippy::too_many_arguments)]
fn in_column_zone(
    page: usize,
    col: usize,
    y_bottom: f32,
    start_page: usize,
    start_col: usize,
    start_y: f64,
    end_page: usize,
    end_col: usize,
    end_y: f64,
) -> bool {
    let y = f64::from(y_bottom);
    if page > start_page && page < end_page {
        return true;
    }
    if page == start_page && page == end_page {
        if start_col == end_col {
            return col == start_col && y >= start_y && y <= end_y;
        }
        if start_col < end_col {
            if col < start_col || col > end_col {
                return false;
            }
            if col == start_col {
                return y >= start_y;
            }
            if col == end_col {
                return y <= end_y;
            }
            return true;
        }
        return col == start_col && y >= start_y;
    }
    if page == start_page {
        if col == start_col {
            return y >= start_y;
        }
        return col > start_col;
    }
    if page == end_page {
        if col == end_col {
            return y <= end_y;
        }
        return col < end_col;
    }
    false
}

/// `ManualRedactionService.redactFoundText` geometry for one block: the
/// padded cover rectangle as `(x, y_bottom, width, height)` in PDF user
/// space (y up). `text_removal_mode` shrinks the width to 90% centered —
/// only used when Java's native removal succeeded; this port always passes
/// `false`, matching the overlay fallback.
pub fn cover_rect(
    block: &FoundText,
    page_height: f32,
    custom_padding: f32,
    text_removal_mode: bool,
) -> (f64, f64, f64, f64) {
    const TEXT_PADDING_MULTIPLIER: f64 = 0.6;
    const WIDTH_REDUCTION: f64 = 0.9;
    let padding = (block.y2 - block.y1) * TEXT_PADDING_MULTIPLIER + f64::from(custom_padding);
    let original_width = block.x2 - block.x1;
    let (box_width, box_x) = if text_removal_mode {
        let w = original_width * WIDTH_REDUCTION;
        (w, block.x1 + (original_width - w) / 2.0)
    } else {
        (original_width, block.x1)
    };
    let pdf_y = f64::from(page_height) - block.y2 - padding;
    let height = (block.y2 - block.y1) + 2.0 * padding;
    (box_x, pdf_y, box_width, height)
}

#[cfg(test)]
mod tests {
    use super::super::model::{PdfChar, PdfLine, PdfWord};
    use super::*;

    fn ch(c: char, x: f32, y: f32) -> PdfChar {
        PdfChar {
            ch: c,
            x,
            y,
            width: 5.0,
            height: 10.0,
            font_name: String::new(),
            font_size: 10.0,
        }
    }

    /// One word of `text` starting at `x`, baseline bottom `y` (y up).
    fn word(text: &str, x: f32, y: f32) -> PdfWord {
        let chars: Vec<PdfChar> = text
            .chars()
            .enumerate()
            .map(|(i, c)| ch(c, x + i as f32 * 5.0, y))
            .collect();
        PdfWord {
            x,
            y,
            width: text.chars().count() as f32 * 5.0,
            height: 10.0,
            chars,
        }
    }

    fn line(words: Vec<PdfWord>) -> PdfLine {
        PdfLine {
            x: words.first().map_or(0.0, |w| w.x),
            y: words.first().map_or(0.0, |w| w.y),
            width: 0.0,
            height: 10.0,
            words,
        }
    }

    fn page(lines: Vec<PdfLine>) -> PageText {
        let chars = lines
            .iter()
            .flat_map(|l| l.words.iter())
            .flat_map(|w| w.chars.iter().cloned())
            .collect();
        PageText {
            page_index: 0,
            lines,
            chars,
        }
    }

    fn find(
        pages: &[PageText],
        heights: &[f32],
        terms: &[&str],
        use_regex: bool,
        whole: bool,
        case: bool,
    ) -> Vec<FoundText> {
        let terms: Vec<String> = terms.iter().map(|s| s.to_string()).collect();
        find_text(pages, heights, &terms, use_regex, whole, case).unwrap()
    }

    #[test]
    fn literal_match_gives_screen_rect() {
        // "hello" at x=10, bottom y=700, page height 792.
        let p = page(vec![line(vec![word("hello", 10.0, 700.0)])]);
        let hits = find(
            std::slice::from_ref(&p),
            &[792.0],
            &["hello"],
            false,
            false,
            true,
        );
        assert_eq!(hits.len(), 1);
        let h = &hits[0];
        assert_eq!(h.x1, 10.0);
        assert_eq!(h.x2, 35.0);
        assert_eq!(h.y1, 82.0); // 792 - (700 + 10)
        assert_eq!(h.y2, 92.0); // 792 - 700
        assert_eq!(h.text, "hello");
    }

    #[test]
    fn matching_is_case_insensitive_by_default() {
        let p = page(vec![line(vec![word("Hello", 10.0, 700.0)])]);
        assert_eq!(
            find(
                std::slice::from_ref(&p),
                &[792.0],
                &["HELLO"],
                false,
                false,
                false
            )
            .len(),
            1
        );
        assert_eq!(
            find(
                std::slice::from_ref(&p),
                &[792.0],
                &["HELLO"],
                false,
                false,
                true
            )
            .len(),
            0
        );
    }

    #[test]
    fn whole_word_blocks_substrings() {
        let p = page(vec![line(vec![word("hello", 10.0, 700.0)])]);
        assert_eq!(
            find(
                std::slice::from_ref(&p),
                &[792.0],
                &["hell"],
                false,
                true,
                true
            )
            .len(),
            0
        );
        assert_eq!(
            find(
                std::slice::from_ref(&p),
                &[792.0],
                &["hello"],
                false,
                true,
                true
            )
            .len(),
            1
        );
    }

    #[test]
    fn whole_word_single_char_uses_neighbour_check() {
        let p = page(vec![line(vec![word("hello", 10.0, 700.0)])]);
        // 'o' inside "hello" has word-char neighbours → rejected.
        assert_eq!(
            find(
                std::slice::from_ref(&p),
                &[792.0],
                &["o"],
                false,
                true,
                true
            )
            .len(),
            0
        );
        // Standalone 'o' matches.
        let p2 = page(vec![line(vec![word("o", 10.0, 700.0)])]);
        assert_eq!(
            find(
                std::slice::from_ref(&p2),
                &[792.0],
                &["o"],
                false,
                true,
                true
            )
            .len(),
            1
        );
    }

    #[test]
    fn regex_terms_match() {
        let p = page(vec![line(vec![word("id 12345 end", 10.0, 700.0)])]);
        let hits = find(
            std::slice::from_ref(&p),
            &[792.0],
            &[r"\d+"],
            true,
            false,
            true,
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text, "12345");
    }

    #[test]
    fn multi_word_match_spans_words() {
        let p = page(vec![line(vec![
            word("foo", 10.0, 700.0),
            word("bar", 40.0, 700.0),
        ])]);
        let hits = find(
            std::slice::from_ref(&p),
            &[792.0],
            &["foo bar"],
            false,
            false,
            true,
        );
        assert_eq!(hits.len(), 1);
        // Covers both words: x 10..55.
        assert_eq!(hits[0].x1, 10.0);
        assert_eq!(hits[0].x2, 55.0);
    }

    #[test]
    fn match_on_second_line() {
        let p = page(vec![
            line(vec![word("top", 10.0, 700.0)]),
            line(vec![word("secret", 10.0, 680.0)]),
        ]);
        let hits = find(
            std::slice::from_ref(&p),
            &[792.0],
            &["secret"],
            false,
            false,
            true,
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].y1, 792.0 - 690.0);
    }

    #[test]
    fn cover_rect_matches_java_math() {
        let block = FoundText {
            page_index: 0,
            x1: 10.0,
            y1: 80.0,
            x2: 50.0,
            y2: 90.0,
            text: String::new(),
        };
        let (x, y, w, h) = cover_rect(&block, 792.0, 0.0, false);
        // pad = 10 * 0.6 = 6; pdf_y = 792 - 90 - 6 = 696; h = 10 + 12 = 22.
        assert_eq!((x, y, w, h), (10.0, 696.0, 40.0, 22.0));
        let (x, y, w, h) = cover_rect(&block, 792.0, 0.0, true);
        // Removal mode: width * 0.9 centred.
        assert_eq!((x, y, w, h), (12.0, 696.0, 36.0, 22.0));
    }

    #[test]
    fn parse_terms_filters_and_trims() {
        assert_eq!(
            parse_terms(" a \n\nb\n"),
            vec!["a".to_string(), "b".to_string()]
        );
        assert!(parse_terms("  \n ").is_empty());
    }

    #[test]
    fn line_boxes_split_on_column_gap() {
        // Same baseline, 30pt apart horizontally → two line boxes.
        let p = page(vec![line(vec![
            word("left", 10.0, 700.0),
            word("right", 60.0, 700.0),
        ])]);
        let boxes = line_screen_boxes(&p, 792.0);
        assert_eq!(boxes.len(), 2);
        assert!(boxes[0][0] < boxes[1][0]);
    }

    #[test]
    fn column_layout_detects_two_columns() {
        // Each side gets six lines wider than MIN_COLUMN_LINE_WIDTH_PT,
        // left and right of the 612pt page midpoint.
        let left_text = "l".repeat(30); // 150pt at 5pt/char
        let right_text = "r".repeat(30);
        let mut lines = Vec::new();
        for i in 0..6 {
            let y = 700.0 - i as f32 * 20.0;
            lines.push(line(vec![
                word(&left_text, 40.0, y),
                word(&right_text, 330.0, y),
            ]));
        }
        let p = page(lines);
        let boxes = line_screen_boxes(&p, 792.0);
        // The 30pt+ x gap splits each row into a left and a right box.
        assert_eq!(boxes.len(), 12);
        let layout = column_layout(&boxes, 612.0);
        assert_eq!(layout.column_count(), 2);
        assert_eq!(layout.column_of(50.0, 180.0), 0);
        assert_eq!(layout.column_of(340.0, 470.0), 1);
    }
}
