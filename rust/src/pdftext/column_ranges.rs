//! Finds a table's column x-ranges by vertical-whitespace projection: a
//! column is a contiguous x-band occupied by enough rows, and the gaps
//! between bands are the gutters. Port of `ColumnRanges.java`.

use std::rc::Rc;

use super::line::{Line, SharedLine};
use super::model::PdfLine;
use super::word_geometry;

/// Character widths of clear space that separate two columns of an unruled block.
pub const GUTTER_CHARS: f32 = 2.5;

/// Absolute floor, in points, on an unruled block's column gutter.
pub const GUTTER_FLOOR: f32 = 10.0;

/// As [`GUTTER_CHARS`], for a block the page's rules already declare to be a table.
pub const RULED_GUTTER_CHARS: f32 = 1.2;

/// As [`GUTTER_FLOOR`], for a block the page's rules already declare to be a table.
pub const RULED_GUTTER_FLOOR: f32 = 4.0;

/// Column bands as `[left, right)` x pairs, empty when the rows carry no
/// table-shaped whitespace.
pub fn find(rows: &[SharedLine]) -> Vec<[f32; 2]> {
    find_with(rows, GUTTER_CHARS, GUTTER_FLOOR)
}

/// As [`find`], with the gutter thresholds given explicitly.
pub fn find_with(rows: &[SharedLine], gutter_chars: f32, gutter_floor: f32) -> Vec<[f32; 2]> {
    find_with_support(rows, gutter_chars, gutter_floor, 0)
}

/// As above, but `min_support` overrides how many rows must occupy an
/// x-band; zero keeps the row-count-scaled default.
pub fn find_with_support(
    rows: &[SharedLine],
    gutter_chars: f32,
    gutter_floor: f32,
    min_support: i32,
) -> Vec<[f32; 2]> {
    let mut min_x = f32::MAX;
    let mut max_x = f32::MIN;
    for l in rows {
        for w in l.words() {
            min_x = min_x.min(w.x);
            max_x = max_x.max(w.x + w.width);
        }
    }
    // Real pages are under ~2000pt wide; anything larger is a malformed/crafted coordinate
    // that would allocate a multi-GB array or produce a negative span on overflow.
    if max_x <= min_x || (max_x - min_x) > 2000.0 {
        return Vec::new();
    }

    let lo = min_x.floor() as i32;
    let span = (max_x.ceil() as i32)
        .wrapping_sub(lo)
        .wrapping_add(1)
        .clamp(0, 2001) as usize;
    let mut coverage = vec![0i32; span];
    for l in rows {
        let mut covered = vec![false; span];
        for w in l.words() {
            let a = (w.x.floor() as i32).wrapping_sub(lo).max(0);
            let b = ((w.x + w.width).ceil() as i32)
                .wrapping_sub(lo)
                .min(span as i32);
            for x in a..b {
                covered[x as usize] = true;
            }
        }
        for (x, c) in covered.iter().enumerate() {
            if *c {
                coverage[x] += 1;
            }
        }
    }

    // A column band must be occupied by at least this many rows; below it is gutter.
    let support = if min_support > 0 {
        min_support
    } else {
        (rows.len() as f32 * 0.35).round().max(2.0) as i32
    };
    let mut columns: Vec<[f32; 2]> = Vec::new();
    let mut start = -1i32;
    for x in 0..span as i32 {
        let is_column = coverage[x as usize] >= support;
        if is_column && start < 0 {
            start = x;
        } else if !is_column && start >= 0 {
            columns.push([lo.wrapping_add(start) as f32, lo.wrapping_add(x) as f32]);
            start = -1;
        }
    }
    if start >= 0 {
        columns.push([
            lo.wrapping_add(start) as f32,
            lo.wrapping_add(span as i32) as f32,
        ]);
    }

    // Merge bands closer than a real column separator: the gaps inside a multi-word cell are
    // about one character, and would otherwise split "January 20th, 2026" into three columns.
    let char_width = word_geometry::average_char_width(rows);
    let min_gutter = gutter_floor.max(char_width * gutter_chars);
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

/// Visible for testing: column detection depends only on word geometry, so
/// tests can exercise degenerate coordinates without a binary fixture.
pub fn from_text_lines(rows: Vec<PdfLine>) -> Vec<[f32; 2]> {
    let lines: Vec<SharedLine> = rows.into_iter().map(|l| Rc::new(Line::new(l))).collect();
    find(&lines)
}

#[cfg(test)]
mod tests {
    use super::super::model::{PdfChar, PdfLine, PdfWord};
    use super::*;

    fn word(x: f32, width: f32) -> PdfWord {
        PdfWord {
            chars: vec![PdfChar {
                ch: 'x',
                x,
                y: 0.0,
                width,
                height: 10.0,
                font_name: String::new(),
                font_size: 10.0,
            }],
            x,
            y: 0.0,
            width,
            height: 10.0,
        }
    }

    fn row(words: Vec<PdfWord>) -> SharedLine {
        Rc::new(Line::new(PdfLine {
            words,
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 10.0,
        }))
    }

    #[test]
    fn empty_rows_yield_no_columns() {
        assert!(find_with_support(&[], 2.5, 10.0, 1).is_empty());
    }

    #[test]
    fn distant_word_bands_become_columns() {
        // Three identical rows: a left word and a right word 90pt away.
        let rows: Vec<SharedLine> = (0..3)
            .map(|_| row(vec![word(0.0, 10.0), word(100.0, 10.0)]))
            .collect();
        let cols = find_with_support(&rows, 2.5, 10.0, 1);
        assert_eq!(cols, vec![[0.0, 10.0], [100.0, 110.0]]);
    }

    #[test]
    fn narrow_gaps_merge_into_one_column() {
        // Two words 2pt apart — inside a cell, not a column gutter.
        let rows: Vec<SharedLine> = (0..3)
            .map(|_| row(vec![word(0.0, 10.0), word(12.0, 10.0)]))
            .collect();
        let cols = find_with_support(&rows, 2.5, 10.0, 1);
        assert_eq!(cols.len(), 1);
        assert_eq!(cols[0], [0.0, 22.0]);
    }

    #[test]
    fn support_threshold_filters_sparse_bands() {
        // Band at x=100 is occupied by only 1 of 4 rows; with min_support=2
        // it must not become a column.
        let mut rows: Vec<SharedLine> = (0..3).map(|_| row(vec![word(0.0, 10.0)])).collect();
        rows.push(row(vec![word(0.0, 10.0), word(100.0, 10.0)]));
        let cols = find_with_support(&rows, 2.5, 10.0, 2);
        assert_eq!(cols, vec![[0.0, 10.0]]);
    }

    #[test]
    fn degenerate_coordinates_are_rejected() {
        // A word claiming a >2000pt span would allocate a huge coverage
        // array; the guard must reject before allocating.
        let rows = vec![row(vec![word(0.0, 5000.0)])];
        assert!(find_with_support(&rows, 2.5, 10.0, 1).is_empty());
    }
}
