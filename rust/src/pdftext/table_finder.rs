//! Finds one page's table blocks and reconciles the two detectors: where
//! both see a table, rows come from the text and columns from the rules.
//! Port of `TableFinder.java`.

use std::collections::HashSet;

use super::column_ranges;
use super::line::SharedLine;
use super::model::PdfWord;
use super::page_rules::PageRules;
use super::ruled_tables::RuledTables;
use super::shared::RowSource;
use super::table_block::TableBlock;
use super::table_grid;

/// Fraction of the word grid's rows a ruled grid must also find before its
/// rows are trusted; below it the rules would merge several rows into one band.
const COMPLETE_LATTICE: f32 = 0.5;

/// Points a wrapped cell line may sit outside the anchor rows' own
/// horizontal extent. A continuation is written inside a cell, so anything
/// reaching past this is another region of the page that merely shares the
/// block's vertical band.
const WRAP_SLACK: f32 = 12.0;

/// Vertical gaps, in median row gaps, within which a line above a block can
/// be its header.
const HEADER_GAP: f32 = 1.6;

/// Line heights within which a line above a ruled grid can be its header row.
pub const HEADER_RULE_GAP: f32 = 2.5;

/// Detects a page's table blocks: ruled blocks first, then word-grid blocks
/// over whatever lines the rules did not claim.
pub fn find(lines: &[SharedLine], rules: &PageRules, page: usize) -> Vec<TableBlock> {
    let ruled = RuledTables::find(lines, rules, page);
    let word = from_word_grid(lines, page);
    if ruled.is_empty() {
        return renderable(word);
    }

    // Where both detectors see the same table, keep the word-grid's rows (read from the text)
    // but take the columns from the rules, which are exact where projection only guesses.
    let mut all: Vec<TableBlock> = Vec::new();
    // Indices into `ruled`; Java deduplicated the record by value, but only
    // objects from `ruled` ever enter the set, so identity is equivalent.
    let mut used_rules: HashSet<usize> = HashSet::new();
    for w in &word {
        // Only a grid with real column rules can improve on the word-grid; one ruled
        // across its rows alone contributes detection, never geometry.
        let m = ruled.iter().position(|r| r.cols.is_some() && covers(w, r));
        if let Some(mi) = m {
            let match_block = &ruled[mi];
            if (match_block.rows.len() as f32) >= w.rows.len() as f32 * COMPLETE_LATTICE {
                // The rules cover nearly every row, so take the whole grid from them; one ruled
                // grid can span several word-grid blocks, so emit it only once.
                if used_rules.insert(mi) {
                    all.push(match_block.clone());
                }
            } else {
                // Only some row boundaries are drawn: rows from the text, columns from the rules.
                used_rules.insert(mi);
                all.push(TableBlock::full(
                    w.rows.clone(),
                    w.top,
                    w.bottom,
                    match_block.cols.clone(),
                    true,
                    RowSource::Words,
                    w.page,
                ));
            }
            continue;
        }
        // No column rules, but a rules-only grid over the same lines still confirms that a
        // table is here. The word-grid's own reading of it stands, now rule-backed.
        let evidence = ruled.iter().position(|r| r.cols.is_none() && covers(w, r));
        match evidence {
            None => all.push(w.clone()),
            Some(ei) => {
                used_rules.insert(ei);
                all.push(TableBlock::full(
                    w.rows.clone(),
                    w.top,
                    w.bottom,
                    None,
                    true,
                    RowSource::Words,
                    w.page,
                ));
            }
        }
    }
    // A ruled table the word-grid never saw (single-word or wrapped cells leave it no wide gap
    // to anchor on) is emitted from its rules alone.
    for (i, r) in ruled.iter().enumerate() {
        if used_rules.contains(&i) {
            continue;
        }
        let covered = all.iter().any(|b| covers(b, r));
        if !covered {
            all.push(r.clone());
        }
    }
    all.sort_by(|a, b| b.top.total_cmp(&a.top));
    renderable(all)
}

/// Drops blocks the grid guards reject. A block claims its lines out of the
/// prose stream, so one that renders to nothing would delete them from the
/// document rather than mis-format them.
pub fn renderable(blocks: Vec<TableBlock>) -> Vec<TableBlock> {
    blocks
        .into_iter()
        .filter(|b| !b.cells().is_empty())
        .collect()
}

/// Detects table blocks from word geometry: anchor rows grouped into
/// contiguous runs, with non-anchor lines inside a run absorbed as wrapped
/// cells.
fn from_word_grid(lines: &[SharedLine], page: usize) -> Vec<TableBlock> {
    let mut cands: Vec<SharedLine> = lines
        .iter()
        .filter(|l| !l.synthetic && is_table_candidate(&l.words()))
        .cloned()
        .collect();
    cands.sort_by(|a, b| b.y.total_cmp(&a.y));
    if cands.len() < 2 {
        return Vec::new();
    }

    let mut gaps: Vec<f32> = Vec::with_capacity(cands.len() - 1);
    for i in 1..cands.len() {
        gaps.push(cands[i - 1].y - cands[i].y);
    }
    let mut sorted = gaps.clone();
    sorted.sort_by(f32::total_cmp);
    let median_gap = sorted[sorted.len() / 2];
    let split_threshold = (median_gap * 2.5).max(median_gap + 6.0);

    let mut anchor_groups: Vec<Vec<SharedLine>> = Vec::new();
    let mut current: Vec<SharedLine> = vec![cands[0].clone()];
    for i in 1..cands.len() {
        let gap = cands[i - 1].y - cands[i].y;
        if gap > split_threshold {
            anchor_groups.push(std::mem::take(&mut current));
        }
        current.push(cands[i].clone());
    }
    anchor_groups.push(current);

    // Synthetic form values are kept out of the table path: they must not seed a column layout
    // or be absorbed as wrapped cells, as they were never in the content stream.
    let non_candidates: Vec<SharedLine> = lines
        .iter()
        .filter(|l| !l.synthetic && !is_table_candidate(&l.words()))
        .cloned()
        .collect();

    let mut blocks = Vec::new();
    for anchors in anchor_groups {
        if anchors.len() < 2 {
            continue;
        }
        let mut top = anchors[0].y;
        let bottom = anchors[anchors.len() - 1].y;
        let mut left = f32::MAX;
        let mut right = f32::MIN;
        for a in &anchors {
            left = left.min(a.left());
            right = right.max(a.right());
        }

        // Each anchor seeds a row; absorb wrapped continuation lines (non-anchors within the
        // run's vertical span, with a little slack below the last row, and inside its
        // horizontal extent) into the anchor above.
        let mut rows: Vec<Vec<SharedLine>> = anchors.iter().map(|a| vec![a.clone()]).collect();
        for nc in &non_candidates {
            if nc.y > top || nc.y < bottom - median_gap {
                continue;
            }
            if nc.left() < left - WRAP_SLACK || nc.right() > right + WRAP_SLACK {
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

        let base = table_grid::cells(&rows, None, RowSource::Words);
        if base.is_empty() {
            continue;
        }
        // A header row often has no wide gap between its cells, so the anchor test misses it.
        // The line above is kept only if its grid has the same shape, excluding captions.
        if let Some(header) = header_above(&non_candidates, top, median_gap) {
            let mut with_header: Vec<Vec<SharedLine>> = vec![vec![header.clone()]];
            with_header.extend(rows.iter().cloned());
            let grown = table_grid::cells(&with_header, None, RowSource::Words);
            if !grown.is_empty()
                && grown[0].len() == base[0].len()
                && table_grid::filled_cells(&grown[0]) >= base[0].len()
            {
                rows = with_header;
                top = header.y;
            }
        }
        blocks.push(TableBlock::new(rows, top, bottom, page));
    }
    blocks
}

/// Runs of words separated by more than a cell gutter: a header row has one
/// per cell, a caption written across the table is a single run.
pub fn word_groups(row: &[SharedLine]) -> usize {
    let mut words: Vec<&PdfWord> = Vec::new();
    for line in row {
        for w in line.words() {
            if !w.stripped().is_empty() {
                words.push(w);
            }
        }
    }
    if words.is_empty() {
        return 0;
    }
    words.sort_by(|a, b| a.x.total_cmp(&b.x));
    let mut chars = 0.0f32;
    let mut width = 0.0f32;
    for w in &words {
        width += w.width;
        chars += w.stripped().chars().count().max(1) as f32;
    }
    let gutter =
        column_ranges::RULED_GUTTER_FLOOR.max((width / chars) * column_ranges::RULED_GUTTER_CHARS);
    let mut groups = 1usize;
    for i in 1..words.len() {
        let gap = words[i].x - (words[i - 1].x + words[i - 1].width);
        if gap >= gutter {
            groups += 1;
        }
    }
    groups
}

/// The nearest line above `top` close enough to be the block's header row.
fn header_above(lines: &[SharedLine], top: f32, median_gap: f32) -> Option<SharedLine> {
    let mut best: Option<SharedLine> = None;
    for l in lines {
        if l.y <= top || l.y - top > median_gap * HEADER_GAP || l.words().len() < 2 {
            continue;
        }
        let is_best = match &best {
            None => true,
            Some(b) => l.y < b.y,
        };
        if is_best {
            best = Some(l.clone());
        }
    }
    best
}

/// True when a line has two words separated by a gap far wider than word
/// spacing. The threshold comes from the line's own character width, not a
/// document font size.
fn is_table_candidate(words: &[&PdfWord]) -> bool {
    if words.len() < 2 {
        return false;
    }
    let mut total_width = 0.0f64;
    let mut total_chars = 0usize;
    for w in words {
        total_width += w.width as f64;
        total_chars += w.stripped().chars().count().max(1);
    }
    let char_width = (total_width / total_chars.max(1) as f64) as f32;
    // A deliberate cell gap is several blank characters wide; ordinary word spaces are ~a third
    // of a character. Floor at 8pt so tiny fonts still need a real gap.
    let cell_gap = 8.0f32.max(char_width * 3.0);
    for i in 1..words.len() {
        let prev = words[i - 1];
        let gap = words[i].x - (prev.x + prev.width);
        if gap >= cell_gap {
            return true;
        }
    }
    false
}

/// True when two blocks overlap in both axes, i.e. they describe the same
/// table. Vertical overlap alone would pair a table with one beside it,
/// whose column geometry is foreign to it.
pub fn covers(a: &TableBlock, b: &TableBlock) -> bool {
    if a.top.min(b.top) <= a.bottom.max(b.bottom) {
        return false;
    }
    let ax = x_extent(a);
    let bx = x_extent(b);
    ax[1].min(bx[1]) > ax[0].max(bx[0])
}

/// A block's left and right edges: its drawn column bands where it has them,
/// widened to the words it holds. A block with neither yields the whole
/// line, so it never vetoes a pairing.
fn x_extent(b: &TableBlock) -> [f32; 2] {
    let mut left = f32::MAX;
    let mut right = f32::MIN;
    if let Some(cols) = &b.cols {
        if !cols.is_empty() {
            left = cols[0][0];
            right = cols[cols.len() - 1][1];
        }
    }
    for row in &b.rows {
        for l in row {
            left = left.min(l.left());
            right = right.max(l.right());
        }
    }
    if left <= right {
        [left, right]
    } else {
        [f32::MIN, f32::MAX]
    }
}

/// Java-style facade over this module's free functions; the ported pipeline
/// calls the Java `TableFinder` helper as a static class.
#[allow(dead_code)]
pub struct TableFinder;

#[allow(dead_code)]
impl TableFinder {
    pub const HEADER_RULE_GAP: f32 = crate::pdftext::table_finder::HEADER_RULE_GAP;

    pub fn find(lines: &[SharedLine], rules: &PageRules, page: usize) -> Vec<TableBlock> {
        find(lines, rules, page)
    }
    pub fn renderable(blocks: Vec<TableBlock>) -> Vec<TableBlock> {
        renderable(blocks)
    }
    pub fn word_groups(row: &[SharedLine]) -> usize {
        word_groups(row)
    }
    pub fn covers(a: &TableBlock, b: &TableBlock) -> bool {
        covers(a, b)
    }
}
