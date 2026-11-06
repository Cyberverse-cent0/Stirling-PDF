//! Multi-column page layout: finding the gutters between columns of prose,
//! splitting a page's lines at them, and emitting the columns in reading
//! order. Port of `ColumnLayout.java`.

use std::rc::Rc;

use super::line::{Line, SharedLine};
use super::markdown_text::MarkdownText;
use super::model::PdfLine;

/// Narrowest run of near-empty x that can separate two columns of prose.
const MIN_GUTTER: f32 = 10.0;

/// Narrowest column worth splitting out; below this a "gutter" is just a ragged margin.
const MIN_COLUMN: f32 = 70.0;

/// Fraction of a page's lines that may cross a gutter and still leave it a gutter.
const MAX_CROSSING: f32 = 0.15;

/// Most columns recognised on one page. Beyond this the geometry is a table, not a layout.
const MAX_COLUMNS: usize = 4;

/// Lines of at least this fraction of a column's width count as that column's body text.
const BODY_LINE_WIDTH: f32 = 0.5;

/// Body lines a column must hold before it is accepted as a column.
const BODY_LINES: usize = 4;

/// Fraction of the finished lines that may straddle a gutter and still allow band ordering.
const BAND_CROSSING: f32 = 0.35;

/// Longest a line may be and still be a line of a heading rather than of a paragraph.
const HEADING_LENGTH_WORDS: usize = 12;

/// Finds the page's column gutters, or empty for a single column. Scans the
/// 5th to 95th percentile of line edges so one degenerate box cannot drag it
/// off the page.
pub fn detect_gutters(lines: &[SharedLine]) -> Vec<f32> {
    if lines.len() < 8 {
        return Vec::new();
    }
    let n = lines.len();
    let los: Vec<f32> = lines.iter().map(|l| l.left()).collect();
    let his: Vec<f32> = lines.iter().map(|l| l.right()).collect();
    let mut sorted_lo = los.clone();
    let mut sorted_hi = his.clone();
    sorted_lo.sort_by(f32::total_cmp);
    sorted_hi.sort_by(f32::total_cmp);
    let lo = sorted_lo[(n as f32 * 0.05) as usize];
    let hi = sorted_hi[((n as f32 * 0.95) as usize).min(n - 1)];
    if hi - lo < 2.0 * MIN_COLUMN + MIN_GUTTER || !plausible_span(lo, hi) {
        return Vec::new();
    }

    let max_crossing = (n as f32 * MAX_CROSSING) as usize;
    let mut start = -1i32;
    let mut bands: Vec<[f32; 2]> = Vec::new();
    // Stepped as an int: past 2^24 a float can no longer represent x + 1, so a float counter
    // over a crafted coordinate stops advancing and spins forever.
    let scan_from = (lo + MIN_COLUMN).floor() as i32;
    let scan_to = (hi - MIN_COLUMN).ceil() as i32;
    for xi in scan_from..=scan_to {
        let x = xi as f32;
        let mut crossing = 0usize;
        for i in 0..n {
            if los[i] < x - 2.0 && his[i] > x + 2.0 {
                crossing += 1;
            }
        }
        if crossing <= max_crossing {
            if start < 0 {
                start = xi;
            }
        } else if start >= 0 {
            bands.push([start as f32, x]);
            start = -1;
        }
    }
    if start >= 0 {
        bands.push([start as f32, hi - MIN_COLUMN]);
    }

    // Widest first, so the strongest separation wins; then keep only bands MIN_COLUMN apart.
    bands.sort_by(|a, b| (b[1] - b[0]).total_cmp(&(a[1] - a[0])));
    let mut gutters: Vec<f32> = Vec::new();
    for b in &bands {
        if b[1] - b[0] < MIN_GUTTER || gutters.len() >= MAX_COLUMNS - 1 {
            continue;
        }
        let mid = (b[0] + b[1]) / 2.0;
        let mut too_close = mid - lo < MIN_COLUMN || hi - mid < MIN_COLUMN;
        for g in &gutters {
            too_close |= (g - mid).abs() < MIN_COLUMN;
        }
        if !too_close {
            gutters.push(mid);
        }
    }
    gutters.sort_by(f32::total_cmp);

    if !gutters.is_empty() && columns_look_like_text(lines, &gutters) {
        return gutters;
    }
    central_gutter(lines, &los, &his, lo, hi)
}

/// Rejects geometry too wide to be real: past 2^24 a float cannot
/// represent x + 1, so a constant-step scan stops advancing.
fn plausible_span(lo: f32, hi: f32) -> bool {
    lo.is_finite() && hi.is_finite() && (hi - lo) <= 2000.0
}

/// Fallback: accepts halves of scattered labels, which read as columns but
/// not as prose.
fn central_gutter(lines: &[SharedLine], los: &[f32], his: &[f32], lo: f32, hi: f32) -> Vec<f32> {
    let n = lines.len();
    let centre_lo = lo + (hi - lo) * 0.35;
    let centre_hi = lo + (hi - lo) * 0.65;
    let mut best_crossing = i32::MAX;
    let mut best_at = 0.0f32;
    let mut best_left = 0i32;
    let mut best_right = 0i32;
    let mut gi = centre_lo.floor() as i32;
    let end = centre_hi.ceil() as i32;
    while gi <= end {
        let gutter = gi as f32;
        let mut crossing = 0i32;
        let mut left = 0i32;
        let mut right = 0i32;
        for i in 0..n {
            if los[i] < gutter - 5.0 && his[i] > gutter + 5.0 {
                crossing += 1;
            } else if his[i] <= gutter {
                left += 1;
            } else {
                right += 1;
            }
        }
        if crossing < best_crossing {
            best_crossing = crossing;
            best_at = gutter;
            best_left = left;
            best_right = right;
        }
        gi += 2;
    }
    let ok = best_left >= 4 && best_right >= 4 && best_crossing <= (n as f32 * 0.25) as i32;
    if ok {
        vec![best_at]
    } else {
        Vec::new()
    }
}

/// True when every carved-out column reads as running text; projection alone
/// cannot tell prose from any other empty lane, such as a bar chart's label
/// gaps.
fn columns_look_like_text(lines: &[SharedLine], gutters: &[f32]) -> bool {
    // Judge only lines inside a column: a spanning line is assigned to one by its centre, and
    // its width would set a measure no real body line could reach.
    let inside: Vec<SharedLine> = lines
        .iter()
        .filter(|l| !spans_gutter(l, gutters))
        .cloned()
        .collect();
    let columns = split_into_columns(&inside, gutters);
    if columns.len() < 2 {
        return false;
    }
    for column in &columns {
        let mut lo = f32::MAX;
        let mut hi = f32::MIN;
        for l in column {
            lo = lo.min(l.left());
            hi = hi.max(l.right());
        }
        let measure = hi - lo;
        let mut body = 0usize;
        for l in column {
            if l.right() - l.left() >= measure * BODY_LINE_WIDTH {
                body += 1;
            }
        }
        if body < BODY_LINES || measure < MIN_COLUMN {
            return false;
        }
    }
    true
}

/// Splits lines into columns at the given gutters. A line crossing one goes
/// to the column its centre falls in; band ordering then places it correctly.
pub fn split_into_columns(lines: &[SharedLine], gutters: &[f32]) -> Vec<Vec<SharedLine>> {
    if gutters.is_empty() {
        return vec![lines.to_vec()];
    }
    let mut columns: Vec<Vec<SharedLine>> = (0..=gutters.len()).map(|_| Vec::new()).collect();
    for l in lines {
        columns[column_of(l, gutters)].push(l.clone());
    }
    columns.retain(|c| !c.is_empty());
    columns
}

fn column_of(l: &Line, gutters: &[f32]) -> usize {
    let centre = (l.left() + l.right()) / 2.0;
    let mut col = 0usize;
    while col < gutters.len() && centre > gutters[col] {
        col += 1;
    }
    col
}

/// True when the finished lines still respect the gutters the unmerged lines
/// showed; merging widens lines, and band-ordering a straddled gutter
/// interleaves the columns.
pub fn gutter_respected(lines: &[SharedLine], gutters: &[f32]) -> bool {
    let real: Vec<&SharedLine> = lines.iter().filter(|l| !l.synthetic).collect();
    if real.is_empty() {
        return false;
    }
    let spanning = real.iter().filter(|l| spans_gutter(l, gutters)).count();
    (spanning as f32) <= real.len() as f32 * BAND_CROSSING
}

/// Fallback column split: cut at the widest gap between the lines' left edges.
pub fn legacy_split(lines: &[SharedLine]) -> Vec<Vec<SharedLine>> {
    let mut xs: Vec<f32> = lines
        .iter()
        .filter(|l| l.width >= 40.0)
        .map(|l| l.x)
        .collect();
    xs.sort_by(f32::total_cmp);
    if xs.is_empty() {
        return vec![lines.to_vec()];
    }
    let mut split_at = (xs[0] + xs[xs.len() - 1]) / 2.0;
    let mut biggest_gap = 0.0f32;
    for i in 1..xs.len() {
        let gap = xs[i] - xs[i - 1];
        if gap > biggest_gap {
            biggest_gap = gap;
            split_at = (xs[i - 1] + xs[i]) / 2.0;
        }
    }
    let mut left: Vec<SharedLine> = Vec::new();
    let mut right: Vec<SharedLine> = Vec::new();
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

/// True when a spanning line is short enough to be one line of a
/// full-width banner heading, which [`order_by_band`] keeps in a single group.
fn heading_length(l: &Line) -> bool {
    MarkdownText::word_count(&l.text) <= HEADING_LENGTH_WORDS
}

/// True when a line straddles a gutter, i.e. it belongs to no single column.
pub fn spans_gutter(l: &SharedLine, gutters: &[f32]) -> bool {
    let left = l.left();
    let right = l.right();
    for g in gutters {
        if left < g - 2.0 && right > g + 2.0 {
            return true;
        }
    }
    false
}

/// Orders a multi-column region as a one-level XY cut: spanning lines cut it
/// into bands, and each band's columns are emitted in turn.
pub fn order_by_band(lines: &[SharedLine], gutters: &[f32]) -> Vec<Vec<SharedLine>> {
    let mut ordered: Vec<SharedLine> = lines.to_vec();
    ordered.sort_by(|a, b| b.y.total_cmp(&a.y));
    let mut out: Vec<Vec<SharedLine>> = Vec::new();
    let mut band: Vec<SharedLine> = Vec::new();
    let mut spanning: Vec<SharedLine> = Vec::new();
    for l in ordered {
        if spans_gutter(&l, gutters) {
            if spanning.is_empty() {
                out.extend(split_into_columns(&band, gutters));
                band = Vec::new();
            } else if !heading_length(&l) || !heading_length(spanning.last().expect("non-empty")) {
                // Only heading-length lines are kept together: a full-width paragraph or list
                // is also a run of spanning lines, and merging those runs its items together.
                out.push(std::mem::take(&mut spanning));
            }
            spanning.push(l);
        } else {
            if !spanning.is_empty() {
                out.push(std::mem::take(&mut spanning));
            }
            band.push(l);
        }
    }
    if !spanning.is_empty() {
        out.push(spanning);
    }
    out.extend(split_into_columns(&band, gutters));
    out.retain(|c| !c.is_empty());
    out
}

/// Visible for testing: as `column_ranges::from_text_lines`, for gutter detection.
pub fn gutters_from_text_lines(rows: Vec<PdfLine>) -> Vec<f32> {
    let lines: Vec<SharedLine> = rows.into_iter().map(|l| Rc::new(Line::new(l))).collect();
    detect_gutters(&lines)
}

/// Java-style facade over this module's free functions; the ported pipeline
/// calls the Java `ColumnLayout` helper as a static class.
#[allow(dead_code)]
pub struct ColumnLayout;

#[allow(dead_code)]
impl ColumnLayout {
    pub fn detect_gutters(lines: &[SharedLine]) -> Vec<f32> {
        detect_gutters(lines)
    }
    pub fn split_into_columns(lines: &[SharedLine], gutters: &[f32]) -> Vec<Vec<SharedLine>> {
        split_into_columns(lines, gutters)
    }
    pub fn gutter_respected(lines: &[SharedLine], gutters: &[f32]) -> bool {
        gutter_respected(lines, gutters)
    }
    pub fn legacy_split(lines: &[SharedLine]) -> Vec<Vec<SharedLine>> {
        legacy_split(lines)
    }
    pub fn spans_gutter(l: &SharedLine, gutters: &[f32]) -> bool {
        spans_gutter(l, gutters)
    }
    pub fn order_by_band(lines: &[SharedLine], gutters: &[f32]) -> Vec<Vec<SharedLine>> {
        order_by_band(lines, gutters)
    }
    pub fn gutters_from_text_lines(rows: Vec<PdfLine>) -> Vec<f32> {
        gutters_from_text_lines(rows)
    }
}
