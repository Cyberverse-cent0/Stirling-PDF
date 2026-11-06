//! Resolves a detected table block into a cell grid and renders it. The
//! false-positive guards live here, so every consumer sees the same cells
//! and the same verdict. Port of `TableGrid.java`.

use super::column_ranges;
use super::line::SharedLine;
use super::model::PdfWord;
use super::shared::RowSource;
use super::table_shape::TableShape;
use super::word_geometry::{is_binding_mark, rejoin_contractions, separated};

/// Renders a table block; `ruled_columns` are exact bands from vertical
/// rules, or `None` to project the columns from whitespace instead.
pub fn render(
    row_groups: &[Vec<SharedLine>],
    ruled_columns: Option<&[[f32; 2]]>,
    row_source: RowSource,
) -> String {
    let rows = cells(row_groups, ruled_columns, row_source);
    if rows.is_empty() {
        String::new()
    } else {
        super::gfm_table::render(&rows, rows[0].len())
    }
}

/// Resolves a table block into a cell grid, or empty when it fails the
/// false-positive guards.
pub fn cells(
    row_groups: &[Vec<SharedLine>],
    ruled_columns: Option<&[[f32; 2]]>,
    row_source: RowSource,
) -> Vec<Vec<String>> {
    // Columns come from cross-row whitespace projection, not a 1-D gap threshold on pooled word
    // x's, which is fragile with right-aligned numbers or sparse cells in their own band.
    let flat: Vec<SharedLine> = row_groups.iter().flatten().cloned().collect();
    // Inside a region the rules already declare a table, a narrower gutter still separates
    // columns: the wide floor only exists to stop word spacing splitting an unruled block.
    let rule_confirmed = row_source.rule_confirmed();
    let mut columns: Vec<[f32; 2]> = match ruled_columns {
        Some(rc) => rc.to_vec(),
        None => column_ranges::find_with(
            &flat,
            if rule_confirmed {
                column_ranges::RULED_GUTTER_CHARS
            } else {
                column_ranges::GUTTER_CHARS
            },
            if rule_confirmed {
                column_ranges::RULED_GUTTER_FLOOR
            } else {
                column_ranges::GUTTER_FLOOR
            },
        ),
    };
    // A column only the header occupies is invisible to the projection, which needs a band
    // shared by several rows; but inside a ruled region a blank answer column is still one.
    let mut header_only_column = false;
    if columns.len() < 2 && ruled_columns.is_none() && rule_confirmed {
        let retry = column_ranges::find_with_support(
            &flat,
            column_ranges::RULED_GUTTER_CHARS,
            column_ranges::RULED_GUTTER_FLOOR,
            1,
        );
        // Only the worksheet shape: exactly one row, the first, reaches past the supported
        // column. Anything else would invent a column and swallow the headings around it.
        if (2..=15).contains(&retry.len()) {
            let edge = retry[0][1];
            let mut beyond = 0usize;
            let mut first_beyond = -1i64;
            for (r, row_lines) in row_groups.iter().enumerate() {
                let mut out = false;
                for l in row_lines {
                    for w in l.words() {
                        if !w.stripped().is_empty() && w.x + w.width / 2.0 > edge {
                            out = true;
                        }
                    }
                }
                if out {
                    beyond += 1;
                    if first_beyond < 0 {
                        first_beyond = r as i64;
                    }
                }
            }
            if beyond == 1 && first_beyond == 0 && row_groups.len() >= 3 {
                columns = retry;
                header_only_column = true;
            }
        }
    }
    // Only a drawn lattice can be a one-column table; inferred from whitespace it is just a
    // run of centred lines.
    let min_columns = if row_source == RowSource::Lattice {
        1
    } else {
        2
    };
    if columns.len() < min_columns || columns.len() > 15 {
        return Vec::new();
    }

    let centers: Vec<f32> = columns.iter().map(|c| (c[0] + c[1]) / 2.0).collect();

    let mut cols = centers.len();
    let mut rows: Vec<Vec<String>> = Vec::new();
    for row_lines in row_groups {
        let mut row = vec![String::new(); cols];
        let mut last_word: Vec<Option<PdfWord>> = vec![None; cols];
        let mut last_text = vec![String::new(); cols];
        let mut bound_mark = vec![false; cols];
        // Top line first so a wrapped cell's words stay in reading order within the cell.
        let mut sorted: Vec<SharedLine> = row_lines.clone();
        sorted.sort_by(|a, b| b.y.total_cmp(&a.y));
        for line in &sorted {
            for word in line.words() {
                let wt = word.stripped();
                if wt.is_empty() {
                    continue;
                }
                let mid = word.x + word.width / 2.0;
                // Ruled columns are real boundaries, so a word belongs to the band that
                // contains it; projected columns are only approximate centres, so nearest wins.
                let col = if ruled_columns.is_some() {
                    containing_column(mid, &columns)
                } else {
                    nearest_column(mid, &centers)
                };
                // A mark that closed up against the word on its left closes up against the
                // word on its right too, so Party - List does not settle at "Party- List".
                let bind = !row[col].is_empty()
                    && (bound_mark[col]
                        || ((is_binding_mark(&wt) || is_binding_mark(&last_text[col]))
                            && !separated(last_word[col].as_ref(), word)));
                if row[col].is_empty() {
                    row[col] = wt.clone();
                } else {
                    row[col].push_str(if bind { "" } else { " " });
                    row[col].push_str(&wt);
                }
                bound_mark[col] = bind && is_binding_mark(&wt);
                last_word[col] = Some(word.clone());
                last_text[col] = wt;
            }
        }
        for cell in row.iter_mut().take(cols) {
            *cell = rejoin_contractions(cell);
        }
        rows.push(row);
    }

    // Guard against false positives while tolerating uneven rows: require an anchor row that
    // nearly fills the grid, and that most rows are genuinely multi-column.
    if ruled_columns.is_some() {
        // A rule that is not a column separator (a cell outline, a shading edge) leaves an
        // empty column; drop those rather than emitting them across every row.
        let mut keep: Vec<usize> = Vec::new();
        for c in 0..cols {
            if rows.iter().any(|r| !r[c].is_empty()) {
                keep.push(c);
            }
        }
        // Two is the floor whatever the rows say: one filled column means the rules drew a box
        // round a single block of text, not a table.
        if keep.len() < 2 {
            return Vec::new();
        }
        if keep.len() < cols {
            let mut trimmed: Vec<Vec<String>> = Vec::with_capacity(rows.len());
            for r in &rows {
                trimmed.push(keep.iter().map(|&i| r[i].clone()).collect());
            }
            rows = trimmed;
            cols = keep.len();
        }
    }

    if cols == 1 {
        // A one-column table has no cross-row alignment to check, so the evidence is the rules
        // plus the shape of the run: enough rows, nearly all carrying text.
        let filled = rows.iter().filter(|r| !r[0].is_empty()).count();
        return if rows.len() >= SINGLE_COLUMN_ROWS
            && (filled as f32) >= rows.len() as f32 * SINGLE_COLUMN_FILLED
        {
            rows
        } else {
            Vec::new()
        };
    }

    let anchor_width = ((cols as f32 * 0.6).round() as usize).max(2);
    let anchor_rows = rows
        .iter()
        .filter(|r| filled_cells(r) >= anchor_width)
        .count();
    let multi_column_rows = rows.iter().filter(|r| filled_cells(r) >= 2).count();
    // The multi-column tests ask whether a grid inferred from whitespace is real; when rows
    // and columns are both drawn there is nothing to infer, and a blank worksheet would fail.
    let drawn_grid =
        header_only_column || (ruled_columns.is_some() && row_source == RowSource::Lattice);
    if if drawn_grid {
        anchor_rows < 1
    } else {
        anchor_rows < 1
            || multi_column_rows < 2
            || (multi_column_rows as f64) < rows.len() as f64 * 0.5
    } {
        return Vec::new();
    }
    if ruled_columns.is_none() && TableShape::is_prose_not_table(&rows, cols) {
        return Vec::new();
    }
    rows
}

/// Rows a single-column ruled table needs before it is a table rather than a run of lines.
const SINGLE_COLUMN_ROWS: usize = 3;

/// Fraction of a single-column table's rows that must carry text.
const SINGLE_COLUMN_FILLED: f32 = 0.8;

/// Index of the column band containing x, clamped to the first/last band outside the grid.
pub fn containing_column(x: f32, columns: &[[f32; 2]]) -> usize {
    for (i, c) in columns.iter().enumerate() {
        if x < c[1] {
            return i;
        }
    }
    columns.len() - 1
}

fn nearest_column(x: f32, centers: &[f32]) -> usize {
    let mut best = 0usize;
    let mut best_dist = f32::MAX;
    for (i, &c) in centers.iter().enumerate() {
        let d = (x - c).abs();
        if d < best_dist {
            best_dist = d;
            best = i;
        }
    }
    best
}

pub fn filled_cells(row: &[String]) -> usize {
    row.iter().filter(|c| !c.is_empty()).count()
}

/// Java-style facade over this module's free functions; the ported pipeline
/// calls the Java `TableGrid` helper as a static class.
#[allow(dead_code)]
pub struct TableGrid;

#[allow(dead_code)]
impl TableGrid {
    pub fn render(
        row_groups: &[Vec<SharedLine>],
        ruled_columns: Option<&[[f32; 2]]>,
        row_source: RowSource,
    ) -> String {
        render(row_groups, ruled_columns, row_source)
    }
    pub fn cells(
        row_groups: &[Vec<SharedLine>],
        ruled_columns: Option<&[[f32; 2]]>,
        row_source: RowSource,
    ) -> Vec<Vec<String>> {
        cells(row_groups, ruled_columns, row_source)
    }
    pub fn containing_column(x: f32, columns: &[[f32; 2]]) -> usize {
        containing_column(x, columns)
    }
    pub fn filled_cells(row: &[String]) -> usize {
        filled_cells(row)
    }
}
