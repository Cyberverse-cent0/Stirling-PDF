//! Builds table blocks from a page's ruling lines, port of
//! `RuledTables.java`: whitespace projection cannot see single-word or
//! wrapped cells, as they leave no wide gap.

use super::line::SharedLine;
use super::page_rules::PageRules;
use super::rule_grid::{Level, RuleGrid};
use super::ruled_rows::RuledRows;
use super::shared::RowSource;
use super::table_block::TableBlock;
use super::table_finder::TableFinder;
use super::table_grid::TableGrid;

/// Largest vertical gap between two rules of one rows-only table.
const ROWS_ONLY_GAP: f32 = 150.0;

/// How far two rules of one rows-only table may differ at either end.
const EXTENT_TOLERANCE: f32 = 8.0;

/// Lines a rows-only group needs before two rules alone are enough to call it a table.
const ROWS_ONLY_LINES: usize = 4;

/// Fraction of a lattice's row bands that must contain text for it to be a real table.
const FILLED_BANDS: f32 = 0.6;

/// Fraction of the table's width an interior rule must run to be a row boundary.
const ROW_RULE_SPAN: f32 = 0.8;

/// Interior row rules needed before drawn bands beat text baselines; bands
/// keep a multi-line cell whole where baselines split it.
const MIN_INTERIOR_RULES: usize = 1;

/// Fraction of a region's width every rule must run for its rows to be a drawn lattice.
const FULL_WIDTH_RULE: f32 = 0.8;

/// How near a rule end must be to a vertical rule to count as landing on it.
const COLUMN_SNAP: f32 = 2.5;

/// Fraction of the table's height a vertical must run to be a column boundary.
const COLUMN_RUN: f32 = 0.5;

pub struct RuledTables;

impl RuledTables {
    pub fn find(lines: &[SharedLine], rules: &PageRules, page: usize) -> Vec<TableBlock> {
        if rules.is_empty() || lines.is_empty() {
            return Vec::new();
        }
        // Synthetic AcroForm values carry no glyphs of their own, so a
        // ruled grid must not claim them: they would seed rows and columns
        // the content stream never drew.
        let lines: Vec<SharedLine> = lines.iter().filter(|l| !l.synthetic).cloned().collect();
        if lines.is_empty() {
            return Vec::new();
        }
        let h_levels = RuleGrid::cluster(&rules.horizontal);
        let v_levels = RuleGrid::cluster(&rules.vertical);
        if h_levels.len() < 2 {
            return Vec::new();
        }
        let mut blocks = Vec::new();
        for part in RuleGrid::partition(&h_levels, &v_levels) {
            if let Some(b) = build(&part.h, &part.v, &lines, page) {
                blocks.push(b);
            }
        }

        // Horizontal rules no grid block claimed can still be a booktabs
        // table: rows ruled, columns not drawn at all. Whatever the grid
        // did not take is offered to that reading.
        let mut unclaimed = Vec::new();
        for h in &h_levels {
            let claimed = blocks.iter().any(|b| {
                h.pos >= b.bottom - RuleGrid::LEVEL_TOLERANCE
                    && h.pos <= b.top + RuleGrid::LEVEL_TOLERANCE
            });
            if !claimed {
                unclaimed.push(*h);
            }
        }
        if unclaimed.len() >= 2 {
            for b in rows_only(&unclaimed, &lines, page) {
                if !blocks
                    .iter()
                    .any(|existing| TableFinder::covers(existing, &b))
                {
                    blocks.push(b);
                }
            }
        }
        blocks.sort_by(|a, b| b.top.total_cmp(&a.top));
        blocks
    }
}

/// Blocks for a page ruled only across its rows (booktabs): no column
/// geometry to recover, so these only find a table the word grid missed.
fn rows_only(levels: &[Level], lines: &[SharedLine], page: usize) -> Vec<TableBlock> {
    let mut h_levels = levels.to_vec();
    h_levels.sort_by(|a, b| b.pos.total_cmp(&a.pos));
    let Some(&first) = h_levels.first() else {
        return Vec::new();
    };
    let mut groups: Vec<Vec<Level>> = Vec::new();
    let mut current = vec![first];
    for &l in h_levels.iter().skip(1) {
        let prev = *current.last().expect("seeded");
        // One booktabs table rules to a single extent; two stacked tables
        // differ in width, and grouping them would project their columns
        // into a single band.
        if prev.pos - l.pos > ROWS_ONLY_GAP
            || (prev.lo - l.lo).abs() > EXTENT_TOLERANCE
            || (prev.hi - l.hi).abs() > EXTENT_TOLERANCE
        {
            groups.push(std::mem::take(&mut current));
        }
        current.push(l);
    }
    groups.push(current);

    let mut blocks = Vec::new();
    for g in &groups {
        if g.len() < 2 {
            continue;
        }
        let top = g[0].pos;
        let bottom = g[g.len() - 1].pos;
        let mut left = f32::MAX;
        let mut right = -f32::MAX;
        for l in g {
            left = left.min(l.lo);
            right = right.max(l.hi);
        }
        let inside: Vec<SharedLine> = lines
            .iter()
            .filter(|l| {
                let cy = l.centre_y();
                let cx = l.centre_x();
                cy > bottom && cy < top && cx > left - 5.0 && cx < right + 5.0
            })
            .cloned()
            .collect();
        // Enough text to be a table: several rows, or for a two-row table
        // a third rule, the header separator a lone pair of decorative
        // rules does not draw.
        if inside.len() < 2 || (inside.len() < ROWS_ONLY_LINES && g.len() < 3) {
            continue;
        }
        let rows = RuledRows::baseline_rows(&inside);
        if rows.len() < 2
            || TableGrid::render(&rows, None, RowSource::RuleBounded)
                .trim()
                .is_empty()
        {
            continue;
        }
        blocks.push(TableBlock {
            rows,
            top,
            bottom,
            cols: None,
            ruled: true,
            row_source: RowSource::RuleBounded,
            page,
        });
    }
    blocks.sort_by(|a, b| b.top.total_cmp(&a.top));
    blocks
}

fn build(h_l: &[Level], v_l: &[Level], lines: &[SharedLine], page: usize) -> Option<TableBlock> {
    tracing::debug!(h = h_l.len(), v = v_l.len(), "ruled-table build");
    if h_l.len() < 2 || v_l.len() < 2 {
        return dbg_null("hL/vL < 2");
    }
    let mut h_l = h_l.to_vec();
    let mut v_l = v_l.to_vec();
    h_l.sort_by(|a, b| b.pos.total_cmp(&a.pos));
    v_l.sort_by(|a, b| a.pos.total_cmp(&b.pos));

    // The extent is the union of both families: a table ruled only
    // between its columns takes its top and bottom from the verticals,
    // and vice versa.
    let mut top = h_l[0].pos;
    let mut bottom = h_l[h_l.len() - 1].pos;
    let mut left = v_l[0].pos;
    let mut right = v_l[v_l.len() - 1].pos;
    for v in &v_l {
        top = top.max(v.hi);
        bottom = bottom.min(v.lo);
    }
    for h in &h_l {
        left = left.min(h.lo);
        right = right.max(h.hi);
    }
    if top - bottom < 6.0 || right - left < 20.0 {
        return dbg_null("too small");
    }

    let inside: Vec<SharedLine> = lines
        .iter()
        .filter(|l| {
            let cy = l.centre_y();
            let cx = l.centre_x();
            cy > bottom && cy < top && cx > left - 5.0 && cx < right + 5.0
        })
        .cloned()
        .collect();
    if inside.len() < 2 {
        return dbg_null("inside<2");
    }

    // `None` columns mean the grid is ruled between its rows only; the
    // block is still worth building, but its columns then come from
    // whitespace projection.
    let cols = RuledRows::columns(&v_l, left, right, top, bottom);

    // A row boundary is a y position, not a segment, and runs the table's
    // width: per-cell rectangles report it once per cell and also box
    // each wrapped line inside a cell.
    let row_rule_width = (right - left) * ROW_RULE_SPAN;
    let mut interior_h = Vec::new();
    let mut band_rules = Vec::new();
    let mut prev_wide = top;
    let mut i = 0;
    while i < h_l.len() {
        let pos = h_l[i].pos;
        let mut j = i;
        let mut widest = h_l[i];
        while j < h_l.len() && (h_l[j].pos - pos).abs() <= RuleGrid::LEVEL_TOLERANCE {
            if h_l[j].hi - h_l[j].lo > widest.hi - widest.lo {
                widest = h_l[j];
            }
            j += 1;
        }
        i = j;
        if pos <= bottom + RuleGrid::LEVEL_TOLERANCE || pos >= top - RuleGrid::LEVEL_TOLERANCE {
            band_rules.push(widest);
            continue;
        }
        let wide = widest.hi - widest.lo >= row_rule_width;
        let keep = wide || spanning_neighbour(&widest, &v_l, &inside, pos, prev_wide, top - bottom);
        if !keep {
            continue;
        }
        interior_h.push(pos);
        band_rules.push(widest);
        if wide {
            prev_wide = pos;
        }
    }

    let mut rows;
    let mut source = RowSource::RuleBounded;
    if interior_h.len() >= MIN_INTERIOR_RULES {
        let mut bands = vec![top];
        bands.extend(interior_h.iter().copied());
        bands.push(bottom);
        let filled = RuledRows::lattice_rows(&bands, &inside);
        // Most bands must carry text: a chart's axis ticks or a zebra
        // table's stripes rule many empty bands, and reading those as a
        // table steals lines from the prose.
        if (filled.len() as f32) < (bands.len() - 1) as f32 * FILLED_BANDS {
            tracing::debug!(
                filled = filled.len(),
                bands = bands.len() - 1,
                "ruled-table build rejected: too few filled bands"
            );
            return None;
        }
        rows = RuledRows::split_complete_bands(filled, cols.as_deref());
        if full_width_rules(&band_rules, left, right) {
            source = RowSource::Lattice;
        }
    } else {
        rows = RuledRows::baseline_rows(&inside);
    }
    if rows.len() < 2 {
        return dbg_null("rows<2");
    }

    // A grid is often ruled around its body only, leaving the header just
    // above the top rule; take it when it fits the grid's width and
    // resolves into its columns.
    if cols.is_some() {
        // The header's cells are separate lines when they sit far apart,
        // so the whole band above the grid is taken, not the nearest line.
        let mut hdr = Vec::new();
        let mut band = f32::MAX;
        for l in lines {
            if l.y <= top
                || l.y - top > TableFinder::HEADER_RULE_GAP * l.height.max(1.0)
                || l.x < left - 5.0
                || l.x + l.width > right + 5.0
            {
                continue;
            }
            band = band.min(l.y);
        }
        for l in lines {
            if band < f32::MAX
                && l.y >= band
                && l.y <= band + 2.0
                && l.x >= left - 5.0
                && l.x + l.width <= right + 5.0
            {
                hdr.push(l.clone());
            }
        }
        if !hdr.is_empty() {
            let mut with_header = Vec::with_capacity(rows.len() + 1);
            with_header.push(hdr.clone());
            with_header.extend(rows.iter().cloned());
            let grown = TableGrid::cells(&with_header, cols.as_deref(), source);
            if !grown.is_empty() {
                let filled_cells = TableGrid::filled_cells(&grown[0]);
                if filled_cells >= grown[0].len().saturating_sub(1)
                    && filled_cells >= 2
                    && TableFinder::word_groups(&hdr) == filled_cells
                {
                    rows = with_header;
                    top = band + hdr[0].height;
                }
            }
        }
    }

    // A block that fails the shared false-positive guards is not a table;
    // leaving its lines unclaimed lets the word-grid detector or ordinary
    // paragraph assembly handle them.
    if TableGrid::render(&rows, cols.as_deref(), source)
        .trim()
        .is_empty()
    {
        return dbg_null("guards rejected");
    }
    Some(TableBlock {
        rows,
        top,
        bottom,
        cols,
        ruled: true,
        row_source: source,
        page,
    })
}

/// True when a rule narrower than the table is still a row boundary: it
/// ends on the grid's verticals and a spanning cell's text sits beside it.
fn spanning_neighbour(
    rule: &Level,
    v_l: &[Level],
    inside: &[SharedLine],
    pos: f32,
    above: f32,
    height: f32,
) -> bool {
    // The vertical must run the table, not merely be there: a line box
    // inside a wrapped cell draws its own short verticals at its inset
    // edges.
    let column_run = height * COLUMN_RUN;
    let mut lo_on_rule = false;
    let mut hi_on_rule = false;
    for v in v_l {
        if v.hi - v.lo < column_run {
            continue;
        }
        if (v.pos - rule.lo).abs() <= COLUMN_SNAP {
            lo_on_rule = true;
        }
        if (v.pos - rule.hi).abs() <= COLUMN_SNAP {
            hi_on_rule = true;
        }
    }
    if !lo_on_rule || !hi_on_rule {
        return false;
    }
    // The spanning cell's text must sit beside the rule anywhere in the
    // row the last full-width boundary opened: it is written once, at the
    // top of the span.
    inside.iter().any(|l| {
        let cy = l.centre_y();
        let cx = l.centre_x();
        cy > pos && cy < above && (cx < rule.lo || cx > rule.hi)
    })
}

/// True when every horizontal rule runs nearly the region's full width;
/// legend swatches and per-cell outlines do not.
fn full_width_rules(h_l: &[Level], left: f32, right: f32) -> bool {
    let width = right - left;
    if width <= 0.0 {
        return false;
    }
    h_l.iter().all(|h| h.hi - h.lo >= width * FULL_WIDTH_RULE)
}

fn dbg_null(why: &str) -> Option<TableBlock> {
    tracing::debug!("ruled-table build rejected: {why}");
    None
}
