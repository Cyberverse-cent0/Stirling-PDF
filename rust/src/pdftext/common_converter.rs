//! Built-in PDF → Markdown conversion — port of `PdfMarkdownConverter.java`,
//! `BasicPdfMarkdownExtractor.java` and the `PdfMarkdownExtractor`
//! interface. Body text is rebuilt from structured page lines; column
//! layout and tables are derived from line/word geometry directly.

use std::collections::HashSet;
use std::rc::Rc;

use lopdf::Document;

use super::common_heading;
use super::markdown_blocks::{MarkdownBlock, MarkdownBlocks};
use super::markdown_text::{self, strip};
use super::model::{document_text, PdfLine};
use super::page_images;
use super::shared::Element;
use crate::pdfdoc;

/// Width below which a line is treated as a stray glyph fragment to be
/// stitched.
const GLYPH_WIDTH: f32 = 7.5;

/// Seam for PDF → Markdown conversion, like `PdfMarkdownExtractor`: the
/// richer proprietary pipeline takes precedence on the Java classpath.
pub trait PdfMarkdownExtractor {
    /// Page-attributed Markdown elements in reading order; the one source
    /// of truth.
    fn extract_blocks(&self, doc: &Document) -> Vec<MarkdownBlock>;

    fn convert(&self, doc: &Document) -> String {
        MarkdownBlocks::join(&self.extract_blocks(doc))
    }
}

/// `PdfMarkdownConverter`: the built-in converter.
pub struct PdfMarkdownConverter;

impl PdfMarkdownConverter {
    pub fn extract_blocks(&self, doc: &Document) -> Vec<MarkdownBlock> {
        extract_blocks(doc)
    }

    pub fn convert(&self, doc: &Document) -> String {
        MarkdownBlocks::join(&self.extract_blocks(doc))
    }
}

impl PdfMarkdownExtractor for PdfMarkdownConverter {
    fn extract_blocks(&self, doc: &Document) -> Vec<MarkdownBlock> {
        extract_blocks(doc)
    }
}

/// Built-in Markdown conversion, used whenever no richer extractor is
/// registered — `BasicPdfMarkdownExtractor`.
pub struct BasicPdfMarkdownExtractor;

impl PdfMarkdownExtractor for BasicPdfMarkdownExtractor {
    fn extract_blocks(&self, doc: &Document) -> Vec<MarkdownBlock> {
        PdfMarkdownConverter.extract_blocks(doc)
    }
}

/// Page-attributed Markdown elements in reading order.
fn extract_blocks(doc: &Document) -> Vec<MarkdownBlock> {
    let stitched = stitch_tables(build_elements(doc));
    let blocks: Vec<MarkdownBlock> = stitched
        .iter()
        .map(|e| {
            let markdown = match &e.value {
                ElementValue::Table(tb) => tb.render(),
                ElementValue::Text(s) => s.clone(),
            };
            MarkdownBlock::new(markdown, e.page_start, e.page_end)
        })
        .collect();
    MarkdownBlocks::with_heading_paths(MarkdownBlocks::normalise_heading_levels(blocks))
}

/// What a pipeline [`Element`] carries here: a rendered text block or a
/// detected table awaiting stitching and rendering.
#[derive(Debug, Clone)]
enum ElementValue {
    Text(String),
    Table(TableBlock),
}

/// A detected table. Each row is a list of source lines: usually one, but
/// more when a cell wraps onto extra lines.
#[derive(Debug, Clone)]
struct TableBlock {
    rows: Vec<Vec<Rc<Line>>>,
    top: f32,
    bottom: f32,
}

impl TableBlock {
    fn render(&self) -> String {
        build_table_from_rows(&self.rows)
    }
}

fn build_elements(doc: &Document) -> Vec<Element<ElementValue>> {
    let all_page_text = document_text(doc);
    let median_size = common_heading::median_font_size(&all_page_text);
    let median_height = common_heading::median_line_height(&all_page_text);

    let page_count = pdfdoc::page_ids(doc).len();
    // Elements are either rendered text (String) or a structured
    // TableBlock. Tables stay structured until after the page loop so a
    // table split across a page break can be stitched back together
    // before rendering.
    let mut output: Vec<Element<ElementValue>> = Vec::new();
    // Header text of a table that ended the previous page, used to spot a
    // continuation whose header repeats at the top of the current page.
    let mut prev_page_trailing_table_header: Option<String> = None;

    for page_index in 0..page_count {
        let raw_lines: &[PdfLine] = all_page_text
            .get(page_index)
            .map(|p| p.lines.as_slice())
            .unwrap_or(&[]);

        // Stitch stray glyph fragments (apostrophes, asterisks,
        // superscripts, bullets) into their host lines so paragraph
        // assembly sees faithful, complete lines.
        let mut lines = stitch_glyphs(raw_lines);
        if lines.is_empty() {
            page_images::emit(
                doc,
                page_index,
                page_index + 1,
                &mut output,
                ElementValue::Text,
            );
            prev_page_trailing_table_header = None;
            continue;
        }

        // Sort top-to-bottom (PDF y=0 is the bottom of the page).
        lines.sort_by(|a, b| b.y.total_cmp(&a.y));

        // Multi-column guard: only genuine two-column prose should be
        // split. A table's column gutters must NOT be mistaken for a
        // page-layout gutter. A table that ran to the bottom of the
        // previous page and repeats its header at the top of this page is
        // a continuation, not a new two-column layout.
        let table_continuation = prev_page_trailing_table_header
            .as_deref()
            .is_some_and(|header| {
                lines
                    .iter()
                    .any(|l| markdown_text::normalise_space(&l.text) == header)
            });

        let two_column = !table_continuation && detects_two_columns(&lines);

        // Tables are detected from text/word geometry (the word-grid
        // detector), which handles both ruled and borderless tables.
        let mut table_row_texts: HashSet<String> = HashSet::new();
        let blocks = if two_column {
            Vec::new()
        } else {
            find_table_blocks(&lines)
        };
        let mut table_lines: HashSet<usize> = HashSet::new();
        for b in &blocks {
            for row in &b.rows {
                for l in row {
                    table_lines.insert(Rc::as_ptr(l) as usize);
                    table_row_texts.insert(strip(&repair_hyphens(&l.text)).to_string());
                }
            }
        }

        let mut page_items: Vec<Element<ElementValue>> = Vec::new();
        if two_column {
            for col in split_into_columns(&lines) {
                let mut paras = Vec::new();
                assemble_paragraphs(
                    &col,
                    median_size,
                    median_height,
                    &mut paras,
                    &table_row_texts,
                );
                for p in paras {
                    page_items.push(Element::new(ElementValue::Text(p), page_index + 1));
                }
            }
        } else {
            // Interleave tables with surrounding text by vertical position.
            // Each block sits in its own slot; non-table lines fall into
            // the slot for their y (text above a block, between blocks, or
            // below the last).
            let mut segments: Vec<Vec<Rc<Line>>> = (0..=blocks.len()).map(|_| Vec::new()).collect();
            for l in &lines {
                if table_lines.contains(&(Rc::as_ptr(l) as usize)) {
                    continue;
                }
                let mut slot = 0;
                for b in &blocks {
                    if b.bottom > l.y {
                        slot += 1;
                    }
                }
                segments[slot].push(l.clone());
            }
            for (s, segment) in segments.iter().enumerate() {
                let mut paras = Vec::new();
                assemble_paragraphs(
                    segment,
                    median_size,
                    median_height,
                    &mut paras,
                    &table_row_texts,
                );
                for p in paras {
                    page_items.push(Element::new(ElementValue::Text(p), page_index + 1));
                }
                if s < blocks.len() {
                    page_items.push(Element::new(
                        ElementValue::Table(blocks[s].clone()),
                        page_index + 1,
                    ));
                }
            }
        }

        page_images::emit(
            doc,
            page_index,
            page_index + 1,
            &mut page_items,
            ElementValue::Text,
        );

        if page_items.is_empty() {
            continue;
        }

        merge_across_page_boundary(&mut output, &mut page_items);
        let trailing = trailing_table_header(&page_items);
        output.extend(page_items);
        prev_page_trailing_table_header = trailing;
    }

    output
}

// --- Glyph stitching ---------------------------------------------------

/// A mutable assembled line: text plus geometry used for ordering and
/// heading detection.
#[derive(Debug)]
struct Line {
    text: String,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    source: PdfLine,
}

impl Line {
    fn new(source: PdfLine) -> Self {
        Self {
            text: source.text(),
            x: source.x,
            y: source.y,
            width: source.width,
            height: source.height,
            source,
        }
    }
}

/// Merges narrow glyph fragments (width < [`GLYPH_WIDTH`]) into the line
/// they belong to: inline between two same-baseline fragments, appended
/// to the line that ends at it, or prepended to the line that starts at
/// it.
fn stitch_glyphs(raw: &[PdfLine]) -> Vec<Rc<Line>> {
    let mut hosts: Vec<PdfLine> = Vec::new();
    let mut glyphs: Vec<PdfLine> = Vec::new();
    for l in raw {
        let line_text = l.text();
        let t = strip(&line_text);
        if t.is_empty() {
            continue;
        }
        if l.width < GLYPH_WIDTH && t.chars().count() <= 2 {
            glyphs.push(l.clone());
        } else {
            hosts.push(l.clone());
        }
    }

    let mut lines: Vec<Line> = hosts.into_iter().map(Line::new).collect();

    for g in &glyphs {
        let gt = strip(&g.text()).to_string();
        if is_bullet_glyph(&gt) {
            attach_bullet(g, &mut lines);
        } else {
            attach_inline_glyph(g, &gt, &mut lines);
        }
    }
    lines.into_iter().map(Rc::new).collect()
}

fn is_bullet_glyph(gt: &str) -> bool {
    gt == "•" || gt == "▪" || gt == "◦"
}

/// Attaches a bullet glyph to the body line it introduces: the closest
/// line that begins to the right of the bullet at roughly the same height
/// or just below it.
fn attach_bullet(g: &PdfLine, lines: &mut Vec<Line>) {
    let mut best: Option<usize> = None;
    let mut best_score = f32::MAX;
    for (idx, h) in lines.iter().enumerate() {
        if h.x < g.x - 2.0 {
            continue;
        }
        let dy = g.y - h.y;
        if dy < -4.0 || dy > 28.0 {
            continue;
        }
        let score = dy.abs() + (h.x - g.x) * 0.2;
        if score < best_score {
            best_score = score;
            best = Some(idx);
        }
    }
    match best {
        Some(b) if !lines[b].text.starts_with('•') => {
            lines[b].text = format!("• {}", lines[b].text);
            lines[b].x = g.x;
        }
        _ => lines.push(Line::new(g.clone())),
    }
}

/// Stitches a narrow inline glyph (apostrophe, quote, asterisk, superscript
/// marker) into the line it belongs to.
fn attach_inline_glyph(g: &PdfLine, gt: &str, lines: &mut Vec<Line>) {
    let mut left: Option<usize> = None;
    let mut right: Option<usize> = None;
    let mut lb = 7.0f32;
    let mut rb = 7.0f32;
    for (idx, h) in lines.iter().enumerate() {
        let same_baseline = g.y >= h.y - 4.0 && g.y <= h.y + h.height + 5.0;
        if !same_baseline {
            continue;
        }
        let dx_left = ((h.x + h.width) - g.x).abs();
        if dx_left < lb {
            lb = dx_left;
            left = Some(idx);
        }
        let dx_right = (h.x - g.x).abs();
        if dx_right < rb {
            rb = dx_right;
            right = Some(idx);
        }
    }

    if let (Some(l), Some(r)) = (left, right) {
        if l != r && (lines[l].y - lines[r].y).abs() < 6.0 {
            let right_text = std::mem::take(&mut lines[r].text);
            let right_end = lines[r].x + lines[r].width;
            let lx = lines[l].x;
            lines[l].text.push_str(gt);
            lines[l].text.push_str(&right_text);
            lines[l].width = right_end - lx;
            lines.remove(r);
            return;
        }
    }
    if let Some(l) = left {
        lines[l].text.push_str(gt);
        lines[l].width = lines[l].width.max(g.x + g.width - lines[l].x);
    } else if let Some(r) = right {
        lines[r].text = format!("{gt}{}", lines[r].text);
        lines[r].x = g.x;
    } else {
        lines.push(Line::new(g.clone()));
    }
}

// --- Column detection (guard only) -------------------------------------

/// True when the page is a genuine two-column layout: a wide horizontal
/// gutter populated on both sides, so single apostrophe glyphs cannot
/// create a false second column.
fn detects_two_columns(lines: &[Rc<Line>]) -> bool {
    if lines.len() < 8 {
        return false;
    }
    let mut min_x = f32::MAX;
    let mut max_x = f32::MIN;
    for l in lines {
        min_x = min_x.min(l.x);
        max_x = max_x.max(l.x + l.width);
    }
    if max_x - min_x < 200.0 {
        return false;
    }

    // Scan candidate gutter positions across the central band (35%-65% of
    // width) and pick the one crossed by the fewest lines.
    let centre_lo = min_x + (max_x - min_x) * 0.35;
    let centre_hi = min_x + (max_x - min_x) * 0.65;
    let mut best_crossing = i64::MAX;
    let mut best_left = 0i64;
    let mut best_right = 0i64;
    let mut gutter = centre_lo;
    while gutter <= centre_hi {
        let mut crossing = 0i64;
        let mut left_only = 0i64;
        let mut right_only = 0i64;
        for l in lines {
            let lx = l.x;
            let rx = l.x + l.width;
            if lx < gutter - 5.0 && rx > gutter + 5.0 {
                crossing += 1;
            } else if rx <= gutter {
                left_only += 1;
            } else {
                right_only += 1;
            }
        }
        if crossing < best_crossing {
            best_crossing = crossing;
            best_left = left_only;
            best_right = right_only;
        }
        gutter += 2.0;
    }

    best_left >= 4 && best_right >= 4 && best_crossing <= (lines.len() as f32 * 0.25) as i64
}

fn split_into_columns(lines: &[Rc<Line>]) -> Vec<Vec<Rc<Line>>> {
    let mut xs: Vec<f32> = lines
        .iter()
        .filter(|l| l.width >= 40.0)
        .map(|l| l.x)
        .collect();
    xs.sort_by(|a, b| a.total_cmp(b));
    if xs.is_empty() {
        return vec![lines.to_vec()];
    }
    let min_x = xs[0];
    let max_x = xs[xs.len() - 1];
    let mut split_at = (min_x + max_x) / 2.0;
    let mut biggest_gap = 0.0f32;
    for i in 1..xs.len() {
        let gap = xs[i] - xs[i - 1];
        if gap > biggest_gap {
            biggest_gap = gap;
            split_at = (xs[i - 1] + xs[i]) / 2.0;
        }
    }
    let mut left = Vec::new();
    let mut right = Vec::new();
    for l in lines {
        if l.x < split_at {
            left.push(l.clone());
        } else {
            right.push(l.clone());
        }
    }
    if left.is_empty() {
        return vec![right];
    }
    if right.is_empty() {
        return vec![left];
    }
    vec![left, right]
}

// --- Paragraph assembly ------------------------------------------------

fn assemble_paragraphs(
    lines: &[Rc<Line>],
    median_size: f32,
    median_height: f32,
    out: &mut Vec<String>,
    table_row_texts: &HashSet<String>,
) {
    let mut para = String::new();
    let mut prev_bottom_y = f32::MAX;
    let mut prev_height = 0.0f32;

    for line in lines {
        let text = strip(&repair_hyphens(&line.text)).to_string();
        if text.is_empty() {
            continue;
        }
        if table_row_texts.contains(&text) {
            continue;
        }

        let block_top = line.y + line.height;
        let gap = prev_bottom_y - block_top;
        let paragraph_break = prev_height > 0.0 && gap > prev_height * 0.8;

        let prefix = common_heading::heading_prefix(&line.source, median_size, median_height);
        let is_heading = !prefix.is_empty();
        let is_bullet = starts_with_bullet(&text);

        if is_heading {
            markdown_text::flush_paragraph(&mut para, out);
            out.push(format!("{prefix}{}", markdown_text::escape_markdown(&text)));
        } else if is_bullet {
            markdown_text::flush_paragraph(&mut para, out);
            out.push(markdown_text::escape_markdown(&text));
        } else if common_heading::is_bold_label(&line.source) {
            // Bold but not large enough to be a heading → emphasise as
            // bold, don't promote.
            markdown_text::flush_paragraph(&mut para, out);
            out.push(format!("**{}**", markdown_text::escape_markdown(&text)));
        } else if paragraph_break {
            markdown_text::flush_paragraph(&mut para, out);
            para.push_str(&text);
        } else {
            if !para.is_empty() {
                let fc = text.chars().next().unwrap();
                let no_space = matches!(fc, '\'' | '’' | '‘' | '"');
                if !no_space {
                    para.push(' ');
                }
            }
            para.push_str(&text);
        }

        prev_bottom_y = line.y;
        prev_height = line.height;
    }
    markdown_text::flush_paragraph(&mut para, out);
}

fn starts_with_bullet(text: &str) -> bool {
    text.starts_with('•') || text.starts_with('▪') || text.starts_with('◦')
}

// --- Word-grid table detection -----------------------------------------

/// Detects table blocks on a page. Anchor rows (lines with table-like
/// column gaps) are grouped into vertically-contiguous runs separated by
/// large vertical gaps; non-anchor lines within a run's vertical span are
/// treated as wrapped-cell continuations.
fn find_table_blocks(lines: &[Rc<Line>]) -> Vec<TableBlock> {
    let mut cands: Vec<Rc<Line>> = lines
        .iter()
        .filter(|l| is_table_candidate(&l.source))
        .cloned()
        .collect();
    cands.sort_by(|a, b| b.y.total_cmp(&a.y));
    if cands.len() < 2 {
        return Vec::new();
    }

    let mut gaps: Vec<f32> = (1..cands.len())
        .map(|i| cands[i - 1].y - cands[i].y)
        .collect();
    gaps.sort_by(|a, b| a.total_cmp(b));
    let median_gap = gaps[gaps.len() / 2];
    let split_threshold = (median_gap * 2.5).max(median_gap + 6.0);

    let mut anchor_groups: Vec<Vec<Rc<Line>>> = Vec::new();
    let mut current = vec![cands[0].clone()];
    for i in 1..cands.len() {
        let gap = cands[i - 1].y - cands[i].y;
        if gap > split_threshold {
            anchor_groups.push(std::mem::take(&mut current));
        }
        current.push(cands[i].clone());
    }
    anchor_groups.push(current);

    let non_candidates: Vec<Rc<Line>> = lines
        .iter()
        .filter(|l| !is_table_candidate(&l.source))
        .cloned()
        .collect();

    let mut blocks = Vec::new();
    for anchors in &anchor_groups {
        if anchors.len() < 2 {
            continue;
        }
        let top = anchors[0].y;
        let bottom = anchors[anchors.len() - 1].y;

        // Each anchor seeds a row; absorb wrapped continuation lines
        // (non-anchors within the run's vertical span, with a little slack
        // below the last row) into the anchor above.
        let mut rows: Vec<Vec<Rc<Line>>> = anchors.iter().map(|a| vec![a.clone()]).collect();
        for nc in &non_candidates {
            if nc.y > top || nc.y < bottom - median_gap {
                continue;
            }
            let mut owner = 0usize;
            let mut best_delta = f32::MAX;
            for (i, a) in anchors.iter().enumerate() {
                let delta = a.y - nc.y; // positive when anchor is above nc
                if delta >= -1.0 && delta < best_delta {
                    best_delta = delta;
                    owner = i;
                }
            }
            rows[owner].push(nc.clone());
        }

        if markdown_text::is_blank(&build_table_from_rows(&rows)) {
            continue;
        }
        blocks.push(TableBlock { rows, top, bottom });
    }
    blocks
}

fn build_table_from_rows(row_groups: &[Vec<Rc<Line>>]) -> String {
    // Detect columns by vertical-whitespace projection across all lines,
    // rather than a 1-D gap threshold on pooled word x's.
    let flat: Vec<Rc<Line>> = row_groups.iter().flat_map(|r| r.iter().cloned()).collect();
    let columns = find_column_ranges(&flat);
    if columns.len() < 2 || columns.len() > 15 {
        return String::new();
    }

    let centers: Vec<f32> = columns.iter().map(|c| (c[0] + c[1]) / 2.0).collect();
    let cols = centers.len();

    let mut rows: Vec<Vec<String>> = Vec::new();
    for row_lines in row_groups {
        let mut row = vec![String::new(); cols];
        let mut sorted = row_lines.clone();
        // Top line first so a wrapped cell's words stay in reading order
        // within the cell.
        sorted.sort_by(|a, b| b.y.total_cmp(&a.y));
        for line in &sorted {
            for word in &line.source.words {
                let word_text = word.text();
                let wt = strip(&word_text);
                if wt.is_empty() {
                    continue;
                }
                let col = nearest_column(word.x + word.width / 2.0, &centers);
                if row[col].is_empty() {
                    row[col] = wt.to_string();
                } else {
                    row[col].push(' ');
                    row[col].push_str(wt);
                }
            }
        }
        rows.push(row);
    }

    // Guard against false positives while tolerating uneven rows.
    let anchor_width = ((cols as f32 * 0.6 + 0.5).floor() as i64).max(2) as usize;
    let anchor_rows = rows
        .iter()
        .filter(|r| filled_cells(r) >= anchor_width)
        .count();
    let multi_column_rows = rows.iter().filter(|r| filled_cells(r) >= 2).count();
    if anchor_rows < 1
        || multi_column_rows < 2
        || (multi_column_rows as f64) < rows.len() as f64 * 0.5
    {
        return String::new();
    }
    render_gfm(&rows, cols)
}

/// Finds column x-ranges by vertical-whitespace projection: a column is a
/// contiguous band covered by a sufficient fraction of rows, and the gaps
/// between such bands are the gutters.
fn find_column_ranges(rows: &[Rc<Line>]) -> Vec<[f32; 2]> {
    let mut min_x = f32::MAX;
    let mut max_x = f32::MIN;
    for l in rows {
        for w in &l.source.words {
            min_x = min_x.min(w.x);
            max_x = max_x.max(w.x + w.width);
        }
    }
    // Real pages are under ~2000pt wide; anything larger is a
    // malformed/crafted coordinate that would allocate a multi-GB array or
    // produce a negative span on overflow.
    if max_x <= min_x || (max_x - min_x) > 2000.0 {
        return Vec::new();
    }

    let lo = min_x.floor() as i32;
    let span = ((max_x.ceil() as i32) - lo + 1).min(2001).max(0) as usize;
    let mut coverage = vec![0i32; span];
    for l in rows {
        let mut covered = vec![false; span];
        for w in &l.source.words {
            let a = ((w.x.floor() as i32) - lo).max(0);
            let b = ((w.x + w.width).ceil() as i32 - lo).min(span as i32);
            for x in a..b {
                if x >= 0 && (x as usize) < span {
                    covered[x as usize] = true;
                }
            }
        }
        for (x, cov) in coverage.iter_mut().enumerate() {
            if covered[x] {
                *cov += 1;
            }
        }
    }

    // A column band must be occupied by at least this many rows; below it
    // is gutter.
    let support = (((rows.len() as f64) * 0.35 + 0.5).floor() as i64).max(2) as usize;
    let mut columns: Vec<[f32; 2]> = Vec::new();
    let mut start: i64 = -1;
    for x in 0..span {
        let is_column = coverage[x] as usize >= support;
        if is_column && start < 0 {
            start = x as i64;
        } else if !is_column && start >= 0 {
            columns.push([(lo + start as i32) as f32, (lo + x as i32) as f32]);
            start = -1;
        }
    }
    if start >= 0 {
        columns.push([(lo + start as i32) as f32, (lo + span as i32) as f32]);
    }

    // Merge bands separated by only a narrow gutter: a real column
    // separator is several characters wide; the gaps *inside* a multi-word
    // cell are about one character.
    let char_width = average_char_width(rows);
    let min_gutter = (char_width * 2.5).max(10.0);
    let mut merged: Vec<[f32; 2]> = Vec::new();
    for band in columns {
        if let Some(last) = merged.last_mut() {
            if band[0] - last[1] < min_gutter {
                last[1] = band[1];
                continue;
            }
        }
        merged.push(band);
    }
    merged
}

fn average_char_width(rows: &[Rc<Line>]) -> f32 {
    let mut total_width = 0.0f64;
    let mut total_chars = 0usize;
    for l in rows {
        for w in &l.source.words {
            total_width += w.width as f64;
            total_chars += strip(&w.text()).chars().count().max(1);
        }
    }
    if total_chars == 0 {
        6.0
    } else {
        (total_width / total_chars as f64) as f32
    }
}

fn nearest_column(x: f32, centers: &[f32]) -> usize {
    let mut best = 0usize;
    let mut best_dist = f32::MAX;
    for (i, c) in centers.iter().enumerate() {
        let d = (x - c).abs();
        if d < best_dist {
            best_dist = d;
            best = i;
        }
    }
    best
}

fn filled_cells(row: &[String]) -> usize {
    row.iter().filter(|cell| !cell.is_empty()).count()
}

fn render_gfm(rows: &[Vec<String>], cols: usize) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let mut widths = vec![3usize; cols];
    for row in rows {
        for c in 0..cols {
            if c < row.len() {
                widths[c] = widths[c].max(
                    markdown_text::escape_markdown_inline(&row[c])
                        .chars()
                        .count(),
                );
            }
        }
    }
    let mut sb = String::new();
    sb.push_str(&build_gfm_row(&rows[0], &widths, cols));
    sb.push('\n');
    sb.push('|');
    for c in 0..cols {
        sb.push('-');
        sb.push_str(&"-".repeat(widths[c]));
        sb.push('-');
        sb.push('|');
    }
    for row in &rows[1..] {
        sb.push('\n');
        sb.push_str(&build_gfm_row(row, &widths, cols));
    }
    sb
}

/// A line looks like a table row if it has at least two words separated by
/// a gap far wider than normal inter-word spacing.
fn is_table_candidate(line: &PdfLine) -> bool {
    let words = &line.words;
    if words.len() < 2 {
        return false;
    }
    let mut total_width = 0.0f64;
    let mut total_chars = 0usize;
    for w in words {
        total_width += w.width as f64;
        total_chars += strip(&w.text()).chars().count().max(1);
    }
    let char_width = (total_width / total_chars.max(1) as f64) as f32;
    // A deliberate cell gap is several blank characters wide; ordinary
    // word spaces are ~a third of a character. Floor at 8pt so tiny fonts
    // still need a real gap.
    let cell_gap = (char_width * 3.0).max(8.0);
    for i in 1..words.len() {
        let prev = &words[i - 1];
        let gap = words[i].x - (prev.x + prev.width);
        if gap >= cell_gap {
            return true;
        }
    }
    false
}

fn build_gfm_row(row: &[String], widths: &[usize], cols: usize) -> String {
    let mut sb = String::from("|");
    for c in 0..cols {
        let cell = if c < row.len() {
            markdown_text::escape_markdown_inline(&row[c])
        } else {
            String::new()
        };
        sb.push(' ');
        sb.push_str(&pad_right(&cell, widths[c]));
        sb.push(' ');
        sb.push('|');
    }
    sb
}

fn pad_right(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - n))
    }
}

// --- Page-level emission helpers ---------------------------------------

fn merge_across_page_boundary(
    output: &mut Vec<Element<ElementValue>>,
    page_items: &mut Vec<Element<ElementValue>>,
) {
    if output.is_empty() || page_items.is_empty() {
        return;
    }
    // Only merge a sentence continuation between two text paragraphs,
    // never into/out of a table.
    let (ElementValue::Text(last), ElementValue::Text(first)) =
        (&output.last().unwrap().value, &page_items[0].value)
    else {
        return;
    };
    if !first.is_empty()
        && first.chars().next().is_some_and(|c| c.is_lowercase())
        && !markdown_text::ends_with_sentence_punctuation(last)
    {
        let merged = format!("{last} {first}");
        let head_start = output.last().unwrap().page_start;
        let tail_end = page_items[0].page_end;
        let last_el = output.last_mut().unwrap();
        last_el.value = ElementValue::Text(merged);
        last_el.page_start = head_start;
        last_el.page_end = tail_end;
        page_items.remove(0);
    }
}

/// Joins tables split across a page break: two consecutive TableBlocks
/// (no text between them) merge when their column layouts match; a
/// repeated header row on the continuation is dropped.
fn stitch_tables(elements: Vec<Element<ElementValue>>) -> Vec<Element<ElementValue>> {
    let mut out: Vec<Element<ElementValue>> = Vec::new();
    for e in elements {
        let replacement = match (&e.value, out.last()) {
            (
                ElementValue::Table(tb),
                Some(Element {
                    value: ElementValue::Table(prev),
                    ..
                }),
            ) => {
                if columns_match(&flatten(&prev.rows), &flatten(&tb.rows)) {
                    let mut merged = prev.rows.clone();
                    let mut tail = tb.rows.clone();
                    if !tail.is_empty()
                        && !prev.rows.is_empty()
                        && row_text(&tail[0]) == row_text(&prev.rows[0])
                    {
                        tail.remove(0);
                    }
                    merged.extend(tail);
                    let page_end = out.last().unwrap().page_end.max(e.page_end);
                    Some((
                        TableBlock {
                            rows: merged,
                            top: prev.top,
                            bottom: tb.bottom,
                        },
                        page_end,
                    ))
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some((tb, page_end)) = replacement {
            let last = out.last_mut().unwrap();
            last.value = ElementValue::Table(tb);
            last.page_end = page_end;
        } else {
            out.push(e);
        }
    }
    out
}

fn flatten(rows: &[Vec<Rc<Line>>]) -> Vec<Rc<Line>> {
    rows.iter().flat_map(|r| r.iter().cloned()).collect()
}

/// Header text of a table at the very bottom of a page, or `None` if the
/// page does not end in one. Trailing image placeholders are skipped; any
/// other text after a table means it did not run to the page bottom.
fn trailing_table_header(page_items: &[Element<ElementValue>]) -> Option<String> {
    for e in page_items.iter().rev() {
        match &e.value {
            ElementValue::Text(s) if MarkdownBlocks::is_image_placeholder(s) => continue,
            ElementValue::Table(tb) if !tb.rows.is_empty() => return Some(row_text(&tb.rows[0])),
            _ => return None,
        }
    }
    None
}

/// Whitespace-normalised text of a row's lines (top to bottom), for
/// header de-duplication.
fn row_text(row: &[Rc<Line>]) -> String {
    let mut ordered = row.to_vec();
    ordered.sort_by(|a, b| b.y.total_cmp(&a.y));
    let mut sb = String::new();
    for l in &ordered {
        if !sb.is_empty() {
            sb.push(' ');
        }
        sb.push_str(&l.text);
    }
    markdown_text::normalise_space(&sb)
}

/// True when two table blocks have the same number of columns at
/// near-identical x-centres.
fn columns_match(a: &[Rc<Line>], b: &[Rc<Line>]) -> bool {
    let ca = find_column_ranges(a);
    let cb = find_column_ranges(b);
    if ca.len() < 2 || ca.len() != cb.len() {
        return false;
    }
    for i in 0..ca.len() {
        let centre_a = (ca[i][0] + ca[i][1]) / 2.0;
        let centre_b = (cb[i][0] + cb[i][1]) / 2.0;
        if (centre_a - centre_b).abs() > 15.0 {
            return false;
        }
    }
    true
}

fn repair_hyphens(text: &str) -> String {
    // `(\w+)-\n([a-z])` → `$1$2`: a word char run, a hyphen, a newline and
    // a lowercase letter join without the break.
    let bytes = text.as_bytes();
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut out = String::with_capacity(text.len());
    let mut last = 0usize;
    let mut i = 0usize;
    while i + 2 < bytes.len() {
        if bytes[i] == b'-'
            && bytes[i + 1] == b'\n'
            && bytes[i + 2].is_ascii_lowercase()
            && i > last
            && is_word(bytes[i - 1])
        {
            out.push_str(&text[last..i]);
            out.push(bytes[i + 2] as char);
            i += 3;
            last = i;
            continue;
        }
        i += 1;
    }
    out.push_str(&text[last..]);
    out
}
