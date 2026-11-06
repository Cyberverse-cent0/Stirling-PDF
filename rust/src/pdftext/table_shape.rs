//! False-positive guards, port of `TableShape.java`: whether a block is
//! really a table, and whether it is wide enough to outrank the page's
//! own column layout. The Java `Pattern`s are small enough to match
//! directly; there is no regex dependency in this crate.

use std::collections::HashSet;
use std::rc::Rc;

use super::line::SharedLine;
use super::table_block::TableBlock;

/// Fraction of the page's text width a table must span to override two-column layout.
const FULL_WIDTH: f32 = 0.6;

/// Rows of a two-column block that must end in a page number for it to be a contents list.
const TOC_ROWS: f32 = 0.65;

/// Mean filled-cell length above which a two-column block reads as prose, not cells.
const PROSE_CELL: f32 = 40.0;

/// Mean filled-cell length above which a column carries sentences rather than values.
const PROSE_COLUMN: f32 = 20.0;

/// Fraction of neighbouring cells that must continue each other's sentence to read as prose.
const PROSE_RUN_ON: f32 = 0.5;

/// Columns a full-width unruled block needs before it can outrank the page's column layout.
const GRID_COLUMNS: usize = 3;

/// Mean filled-cell length above which a full-width unruled block is prose read across.
const GRID_CELL: f32 = 25.0;

pub struct TableShape;

impl TableShape {
    /// True when a block is running text the word grid mistook for a
    /// table: a contents list, or two columns of prose whose cells are
    /// whole sentences.
    pub fn is_prose_not_table(rows: &[Vec<String>], cols: usize) -> bool {
        if rows.is_empty() {
            return false;
        }
        for row in rows {
            for cell in row {
                if has_dot_leader(cell) {
                    return true;
                }
            }
        }
        if cols != 2 {
            return Self::every_column_is_prose(rows, cols);
        }
        let mut folios = 0usize;
        let mut length = 0usize;
        let mut filled = 0usize;
        for row in rows {
            let mut last = "";
            for cell in row {
                if !cell.is_empty() {
                    length += utf16_len(cell);
                    filled += 1;
                    last = cell.as_str();
                }
            }
            if is_page_number(last) && !is_page_number(row.first().map_or("", String::as_str)) {
                folios += 1;
            }
        }
        if folios as f32 >= rows.len() as f32 * TOC_ROWS {
            return true;
        }
        filled > 0 && (length as f32 / filled as f32) >= PROSE_CELL
    }

    /// True when a wide block is multi-column prose read across, not a
    /// table: no column keys the rows, and the cells continue each
    /// other's sentences.
    pub fn every_column_is_prose(rows: &[Vec<String>], cols: usize) -> bool {
        if cols < 3 {
            return false;
        }
        for c in 0..cols {
            let mut length = 0usize;
            let mut filled = 0usize;
            for row in rows {
                if c < row.len() && !row[c].is_empty() {
                    length += utf16_len(&row[c]);
                    filled += 1;
                }
            }
            if filled == 0 || (length as f32 / filled as f32) < PROSE_COLUMN {
                return false;
            }
        }
        runs_on_across_cells(rows)
    }

    /// True when no text outside the block sits in its vertical band, so
    /// it cannot be one column of a two-column layout. Identity is by
    /// shared line handle, like the Java `Set<Line>`.
    pub fn owns_its_band(block: &TableBlock, lines: &[SharedLine]) -> bool {
        let mut own = HashSet::new();
        for row in &block.rows {
            for l in row {
                own.insert(Rc::as_ptr(l) as usize);
            }
        }
        for l in lines {
            if own.contains(&(Rc::as_ptr(l) as usize)) {
                continue;
            }
            let centre = l.centre_y();
            if centre > block.bottom && centre < block.top {
                return false;
            }
        }
        true
    }

    /// True when an unruled full-width block is really a table: a data
    /// table's cells are short values, a page gutter's are sentences.
    pub fn looks_like_grid(block: &TableBlock) -> bool {
        let cells = block.cells();
        if cells.is_empty() || cells[0].len() < GRID_COLUMNS {
            return false;
        }
        let mut length = 0usize;
        let mut filled = 0usize;
        for row in &cells {
            for cell in row {
                if !cell.is_empty() {
                    length += utf16_len(cell);
                    filled += 1;
                }
            }
        }
        filled > 0 && (length as f32 / filled as f32) <= GRID_CELL
    }

    /// True when a table block is wide enough to be a full-width table,
    /// not one inside a column.
    pub fn spans_page(block: &TableBlock, lines: &[SharedLine]) -> bool {
        let mut page_lo = f32::MAX;
        let mut page_hi = -f32::MAX;
        for l in lines {
            page_lo = page_lo.min(l.x);
            page_hi = page_hi.max(l.x + l.width);
        }
        let mut lo = f32::MAX;
        let mut hi = -f32::MAX;
        for row in &block.rows {
            for l in row {
                lo = lo.min(l.x);
                hi = hi.max(l.x + l.width);
            }
        }
        page_hi > page_lo && (hi - lo) >= (page_hi - page_lo) * FULL_WIDTH
    }
}

/// Java `String.length()`: UTF-16 code units, so the cell-length
/// thresholds compare the same counts the Java guards did.
fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// Java regex `[0-9]{1,4}|[ivxlcdmIVXLCDM]{1,7}` with `matches()`.
fn is_page_number(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    if (1..=4).contains(&n) && chars.iter().all(|c| c.is_ascii_digit()) {
        return true;
    }
    const ROMAN: &[char] = &[
        'i', 'v', 'x', 'l', 'c', 'd', 'm', 'I', 'V', 'X', 'L', 'C', 'D', 'M',
    ];
    (1..=7).contains(&n) && chars.iter().all(|c| ROMAN.contains(c))
}

/// Java regex `(\.\s*){4,}|…` with `find()`.
fn has_dot_leader(cell: &str) -> bool {
    if cell.contains('…') {
        return true;
    }
    let chars: Vec<char> = cell.chars().collect();
    for i in 0..chars.len() {
        if chars[i] != '.' {
            continue;
        }
        let mut dots = 0;
        let mut j = i;
        while j < chars.len() && chars[j] == '.' {
            dots += 1;
            j += 1;
            while j < chars.len() && java_space(chars[j]) {
                j += 1;
            }
            if dots >= 4 {
                return true;
            }
        }
    }
    false
}

/// Java `\s`: ASCII whitespace only.
fn java_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0B' | '\x0C' | '\r')
}

/// Java regex `[.!?:;,]$` with `find()`; `$` also matches before one
/// final line terminator.
fn ends_clause(cell: &str) -> bool {
    let mut chars: Vec<char> = cell.chars().collect();
    if let Some(&last) = chars.last() {
        if matches!(last, '\n' | '\r' | '\u{85}' | '\u{2028}' | '\u{2029}') {
            chars.pop();
            // \r\n is a single terminator.
            if last == '\n' && chars.last() == Some(&'\r') {
                chars.pop();
            }
        }
    }
    matches!(chars.last(), Some('.' | '!' | '?' | ':' | ';' | ','))
}

/// Fraction of side-by-side filled cells where the right one continues
/// the left one's clause.
fn runs_on_across_cells(rows: &[Vec<String>]) -> bool {
    let mut pairs = 0usize;
    let mut run_on = 0usize;
    for row in rows {
        let mut previous: Option<&str> = None;
        for cell in row {
            if cell.is_empty() {
                continue;
            }
            if let Some(prev) = previous {
                pairs += 1;
                if !ends_clause(prev) && cell.chars().next().is_some_and(char::is_lowercase) {
                    run_on += 1;
                }
            }
            previous = Some(cell);
        }
    }
    pairs > 0 && (run_on as f32 / pairs as f32) > PROSE_RUN_ON
}
