//! Rejoins extractor fragments that are really one visual line. Fragments
//! are grouped into rows first, then joined left to right — port of
//! `LineMerger.java`.

use std::rc::Rc;

use super::line::{Line, SharedLine};
use super::markdown_text;
use super::word_geometry::NO_SPACE_GAP;

/// Gap above this many average character widths is a real layout gap, so
/// never merged.
const MAX_MERGE_GAP: f32 = 1.60;

/// Rejoins fragments of one visual line: PDFium splits on bounding box, so a
/// run with no ascender (`rou`) lands apart from the rest. Each line must be
/// uniquely owned; merged fragments are re-wrapped on return.
pub fn merge_line_fragments(lines: Vec<SharedLine>, gutters: &[f32]) -> Vec<SharedLine> {
    if lines.len() < 2 {
        return lines;
    }
    // Merge within each column, so a line ending at the gutter never joins the
    // next column's.
    if !gutters.is_empty() {
        let columns = crate::pdftext::column_layout::split_into_columns(&lines, gutters);
        drop(lines);
        if columns.len() > 1 {
            let merged: Vec<Line> = columns
                .into_iter()
                .flat_map(|column| merge_rows(into_lines(column)))
                .collect();
            return into_shared(merged);
        }
        let flat: Vec<SharedLine> = columns.into_iter().flatten().collect();
        return into_shared(merge_rows(into_lines(flat)));
    }
    into_shared(merge_rows(into_lines(lines)))
}

fn into_lines(lines: Vec<SharedLine>) -> Vec<Line> {
    lines
        .into_iter()
        .map(|line| {
            Rc::try_unwrap(line).expect("merge_line_fragments requires uniquely owned lines")
        })
        .collect()
}

fn into_shared(lines: Vec<Line>) -> Vec<SharedLine> {
    lines.into_iter().map(Rc::new).collect()
}

fn merge_rows(lines: Vec<Line>) -> Vec<Line> {
    if lines.len() < 2 {
        return lines;
    }
    let mut ordered = lines;
    // Top edge first, so fragments of one visual line arrive together whatever
    // their heights.
    ordered.sort_by(|a, b| (b.y + b.height).total_cmp(&(a.y + a.height)));

    // Group into rows first: a fragment's continuation is its right-hand
    // neighbour on the same row, not whichever line the extractor emitted next.
    let mut rows: Vec<Vec<Line>> = Vec::new();
    for line in ordered {
        let mut row_index = None;
        let first = rows.len().saturating_sub(3);
        for index in (first..rows.len()).rev() {
            if overlaps_row(&rows[index], &line) {
                row_index = Some(index);
                break;
            }
        }
        match row_index {
            Some(index) => rows[index].push(line),
            None => rows.push(vec![line]),
        }
    }

    let mut out = Vec::new();
    for mut row in rows {
        row.sort_by(|a, b| a.x.total_cmp(&b.x));
        let mut host = None;
        for line in row {
            if let Some(host_index) = host {
                if adjacent_on_row(&out[host_index], &line) {
                    append_fragment(&mut out[host_index], &line);
                    continue;
                }
            }
            out.push(line);
            host = Some(out.len() - 1);
        }
    }
    out
}

/// True when a line shares a row with the lines already in it (vertical overlap).
fn overlaps_row(row: &[Line], line: &Line) -> bool {
    row.iter().any(|member| {
        let overlap =
            java_min(member.y + member.height, line.y + line.height) - java_max(member.y, line.y);
        let min_height = java_min(member.height, line.height);
        min_height > 0.0 && overlap >= min_height * 0.5
    })
}

/// Java `Math.min`/`max` propagate NaN where `f32::min`/`max` return the
/// other operand.
fn java_min(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() {
        f32::NAN
    } else {
        a.min(b)
    }
}

fn java_max(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() {
        f32::NAN
    } else {
        a.max(b)
    }
}

/// True when `next` sits close enough to `host` to be the same visual line.
fn adjacent_on_row(host: &Line, next: &Line) -> bool {
    // Merging concatenates left to right, which is reading order for LTR only;
    // RTL fragments would be joined back to front.
    if has_strong_rtl(&host.text) || has_strong_rtl(&next.text) {
        return false;
    }
    let gap = next.glyph_left() - host.glyph_right();
    let char_width = fragment_char_width(host, next);
    gap > -char_width && gap < char_width * MAX_MERGE_GAP
}

/// True when the text contains a character whose Unicode Bidi_Class is R or
/// AL, matching the directionality classes Java's `Character` reports as
/// right-to-left.
fn has_strong_rtl(text: &str) -> bool {
    text.encode_utf16()
        .filter_map(|unit| char::from_u32(u32::from(unit)))
        .any(is_strong_rtl)
}

fn is_strong_rtl(c: char) -> bool {
    let code = c as u32;
    STRONG_RTL_RANGES
        .iter()
        .any(|&(lo, hi)| lo <= code && code <= hi)
}

fn append_fragment(host: &mut Line, next: &Line) {
    let gap = next.glyph_left() - host.glyph_right();
    let char_width = fragment_char_width(host, next);
    let left = host
        .text
        .trim_end_matches(markdown_text::is_java_whitespace)
        .to_string();
    let right = next
        .text
        .trim_start_matches(markdown_text::is_java_whitespace);
    let space = gap >= char_width * NO_SPACE_GAP;
    host.text = if space {
        format!("{} {}", left, right)
    } else {
        format!("{}{}", left, right)
    };
    host.merged.push(next.source.clone());
    host.merged.extend(next.merged.iter().cloned());

    let right0 = java_max(host.x + host.width, next.x + next.width);
    let top = java_max(host.y + host.height, next.y + next.height);
    host.x = java_min(host.x, next.x);
    host.y = java_min(host.y, next.y);
    host.width = right0 - host.x;
    host.height = top - host.y;
}

fn fragment_char_width(a: &Line, b: &Line) -> f32 {
    let mut total = 0.0f64;
    let mut chars = 0usize;
    for line in [a, b] {
        for word in line.words() {
            for c in &word.chars {
                if !c.is_whitespace() && !c.is_newline() && c.width > 0.0 {
                    total += c.width as f64;
                    chars += 1;
                }
            }
        }
    }
    if chars == 0 {
        6.0
    } else {
        (total / chars as f64) as f32
    }
}

/// Inclusive code-point ranges with Unicode Bidi_Class R or AL.
const STRONG_RTL_RANGES: &[(u32, u32)] = &[
    (0x0590, 0x0590),
    (0x05BE, 0x05BE),
    (0x05C0, 0x05C0),
    (0x05C6, 0x05C6),
    (0x05C8, 0x05FF),
    (0x060B, 0x060B),
    (0x060D, 0x060D),
    (0x061B, 0x064A),
    (0x066D, 0x066F),
    (0x0671, 0x06D5),
    (0x06E5, 0x06E6),
    (0x06FA, 0x0710),
    (0x0712, 0x072F),
    (0x074B, 0x07A5),
    (0x07B1, 0x07BF),
    (0x07C0, 0x07EA),
    (0x07F4, 0x07F5),
    (0x07FA, 0x07FC),
    (0x07FE, 0x0815),
    (0x0824, 0x0824),
    (0x0828, 0x0828),
    (0x082E, 0x0858),
    (0x085C, 0x085F),
    (0x0860, 0x086A),
    (0x0870, 0x088E),
    (0x088F, 0x088F),
    (0x0892, 0x0896),
    (0x08A0, 0x08C9),
    (0x200F, 0x200F),
    (0xFB1D, 0xFB1D),
    (0xFB1F, 0xFB28),
    (0xFB50, 0xFD3D),
    (0xFDF0, 0xFDFC),
    (0x10800, 0x1091E),
    (0x10A04, 0x10A04),
    (0x10A07, 0x10A0B),
    (0x10A3B, 0x10A3E),
    (0x10A40, 0x10AE4),
    (0x10B40, 0x10CFF),
    (0x10D28, 0x10D2F),
    (0x10D3A, 0x10D3F),
    (0x10E7F, 0x10EAA),
    (0x10EAD, 0x10EC1),
    (0x10EC2, 0x10EC4),
    (0x10F00, 0x10F2F),
    (0x10F51, 0x10F59),
    (0x10F86, 0x10FFF),
    (0x1E8D7, 0x1E943),
    (0x1E94B, 0x1EC70),
    (0x1EC71, 0x1ECB4),
    (0x1ED01, 0x1ED3D),
    (0x1ED3E, 0x1EDFF),
    (0x1EEF2, 0x1EEFF),
];

/// Java-style facade over this module's free functions; the ported pipeline
/// calls the Java `LineMerger` helper as a static class. Takes ownership
/// like the free function: merging `Rc::try_unwrap`s each line.
#[allow(dead_code)]
pub struct LineMerger;

#[allow(dead_code)]
impl LineMerger {
    pub fn merge_line_fragments(lines: Vec<SharedLine>, gutters: &[f32]) -> Vec<SharedLine> {
        merge_line_fragments(lines, gutters)
    }
}
