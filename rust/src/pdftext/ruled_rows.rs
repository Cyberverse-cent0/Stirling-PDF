//! Groups the lines inside a ruled region into rows, and reads its column
//! bands off the vertical rules — port of `RuledRows.java`.

use super::line::SharedLine;
use super::rule_grid::{Level, RuleGrid};
use super::table_grid::TableGrid;

/// A vertical rule must cover this fraction of a region's height to be a column boundary.
const COLUMN_COVERAGE: f32 = 0.5;

pub struct RuledRows;

impl RuledRows {
    /// Splits a band whose every baseline is a complete row back into those
    /// rows; a wrapped cell leaves the other columns empty, a run of rows
    /// does not.
    pub fn split_complete_bands(
        bands: Vec<Vec<SharedLine>>,
        cols: Option<&[[f32; 2]]>,
    ) -> Vec<Vec<SharedLine>> {
        let Some(cols) = cols else {
            return bands;
        };
        if cols.len() < 2 {
            return bands;
        }
        let mut out = Vec::new();
        for band in bands {
            let baselines = Self::baseline_rows(&band);
            if baselines.len() < 2 || !all_rows_complete(&baselines, cols) {
                out.push(band);
                continue;
            }
            out.extend(baselines);
        }
        out
    }

    /// Column bands from the vertical rules spanning the region; `None`
    /// when no interior rule survives, as whitespace projection guesses
    /// better.
    pub fn columns(
        v_levels: &[Level],
        left: f32,
        right: f32,
        top: f32,
        bottom: f32,
    ) -> Option<Vec<[f32; 2]>> {
        let height = top - bottom;
        // Per-cell strokes give one rule per row, and a row that draws no
        // boxes breaks the run in two, so strokes at one x are measured
        // together rather than as separate runs.
        let mut sorted = v_levels.to_vec();
        sorted.sort_by(|a, b| a.pos.total_cmp(&b.pos));
        let mut xs = Vec::new();
        let mut at = 0;
        while at < sorted.len() {
            let pos = sorted[at].pos;
            let mut covered = 0.0f32;
            let mut end = at;
            while end < sorted.len() && sorted[end].pos - pos <= RuleGrid::LEVEL_TOLERANCE {
                let v = &sorted[end];
                covered += (top.min(v.hi) - bottom.max(v.lo)).max(0.0);
                end += 1;
            }
            if covered >= height * COLUMN_COVERAGE {
                xs.push(pos);
            }
            at = end;
        }
        let mut bounds = vec![left];
        for x in xs {
            if x > *bounds.last().expect("seeded") + RuleGrid::LEVEL_TOLERANCE
                && x < right - RuleGrid::LEVEL_TOLERANCE
            {
                bounds.push(x);
            }
        }
        if bounds.len() < 2 {
            return None;
        }
        bounds.push(right);
        Some(bounds.windows(2).map(|w| [w[0], w[1]]).collect())
    }

    /// Rows delimited by horizontal rules; this is what keeps a wrapped
    /// cell as one row. `bands` runs top to bottom.
    pub fn lattice_rows(bands: &[f32], inside: &[SharedLine]) -> Vec<Vec<SharedLine>> {
        let mut rows = Vec::new();
        for i in 1..bands.len() {
            let hi = bands[i - 1];
            let lo = bands[i];
            let band: Vec<SharedLine> = inside
                .iter()
                .filter(|l| {
                    let cy = l.centre_y();
                    cy > lo && cy <= hi
                })
                .cloned()
                .collect();
            if !band.is_empty() {
                rows.push(band);
            }
        }
        rows
    }

    /// Rows by baseline proximity, for a table ruled between its columns
    /// but not its rows.
    pub fn baseline_rows(inside: &[SharedLine]) -> Vec<Vec<SharedLine>> {
        if inside.is_empty() {
            return Vec::new();
        }
        let mut sorted = inside.to_vec();
        sorted.sort_by(|a, b| b.y.total_cmp(&a.y));
        let mut heights: Vec<f32> = sorted.iter().map(|l| l.height).collect();
        heights.sort_by(|a, b| a.total_cmp(b));
        let same_row = (2.0f32).max(heights[heights.len() / 2] * 0.6);
        let mut rows = Vec::new();
        let mut current: Vec<SharedLine> = Vec::new();
        let mut anchor = 0.0f32;
        for l in &sorted {
            if current.is_empty() {
                anchor = l.y;
            } else if anchor - l.y > same_row {
                rows.push(std::mem::take(&mut current));
                anchor = l.y;
            }
            current.push(l.clone());
        }
        if !current.is_empty() {
            rows.push(current);
        }
        rows
    }
}

/// True when every baseline group puts a word in every column band.
fn all_rows_complete(baselines: &[Vec<SharedLine>], cols: &[[f32; 2]]) -> bool {
    for row in baselines {
        let mut hit = vec![false; cols.len()];
        for l in row {
            for w in l.words() {
                if w.stripped().is_empty() {
                    continue;
                }
                let c = TableGrid::containing_column(w.x + w.width / 2.0, cols);
                if c < hit.len() {
                    hit[c] = true;
                }
            }
        }
        if hit.iter().any(|h| !*h) {
            return false;
        }
    }
    true
}
