//! Rebuilds assembled lines from extractor lines, folding in the narrow
//! glyph fragments PDFium emits for apostrophes, markers and bullets —
//! port of `GlyphStitcher.java`.

use std::rc::Rc;

use super::line::{Line, SharedLine};
use super::markdown_text;
use super::model::PdfLine;

/// Width below which a line is treated as a stray glyph fragment to be stitched.
const GLYPH_WIDTH: f32 = 7.5;
const SOFT_HYPHEN: char = '\u{00AD}';

/// Merges narrow glyph fragments into the line they belong to: inline between
/// two same-baseline fragments, or appended/prepended at a line's edge.
pub fn stitch_glyphs(raw: &[PdfLine]) -> Vec<SharedLine> {
    let mut hosts = Vec::new();
    let mut glyphs = Vec::new();
    for line in raw {
        let stripped = strip_soft_hyphens(&line.text());
        let text = markdown_text::strip(&stripped);
        if text.is_empty() {
            continue;
        }
        if line.width < GLYPH_WIDTH && text.encode_utf16().count() <= 2 {
            glyphs.push(line);
        } else {
            hosts.push(line);
        }
    }

    let mut lines: Vec<Line> = hosts
        .iter()
        .map(|line| Line::with_text((*line).clone(), strip_soft_hyphens(&line.text())))
        .collect();

    for glyph in glyphs {
        let stripped = strip_soft_hyphens(&glyph.text());
        let text = markdown_text::strip(&stripped);
        if is_bullet_glyph(text) {
            attach_bullet(glyph, text, &mut lines);
        } else {
            attach_inline_glyph(glyph, text, &mut lines);
        }
    }
    lines.into_iter().map(Rc::new).collect()
}

/// Removes U+00AD SOFT HYPHEN, a break-opportunity marker PDFium hands back
/// verbatim as `ar<AD>e`.
fn strip_soft_hyphens(text: &str) -> String {
    if !text.contains(SOFT_HYPHEN) {
        return text.to_string();
    }
    text.replace(SOFT_HYPHEN, "")
}

fn is_bullet_glyph(text: &str) -> bool {
    matches!(text, "•" | "▪" | "◦")
}

/// Attaches a bullet glyph to the closest line beginning to its right, at
/// roughly the same height or just below.
fn attach_bullet(glyph: &PdfLine, text: &str, lines: &mut Vec<Line>) {
    let mut best = None;
    let mut best_score = f32::MAX;
    for (index, line) in lines.iter().enumerate() {
        if line.x < glyph.x - 2.0 {
            continue;
        }
        let dy = glyph.y - line.y;
        if !(-4.0..=28.0).contains(&dy) {
            continue;
        }
        let score = dy.abs() + (line.x - glyph.x) * 0.2;
        if score < best_score {
            best_score = score;
            best = Some(index);
        }
    }

    if let Some(index) = best {
        if !lines[index].text.starts_with("•") {
            lines[index].text.insert_str(0, "• ");
            lines[index].x = glyph.x;
            return;
        }
    }
    lines.push(Line::with_text(glyph.clone(), text.to_string()));
}

/// Stitches a narrow inline glyph into its line: between two same-baseline
/// fragments, appended to the line ending at it, or prepended to the one
/// starting at it.
fn attach_inline_glyph(glyph: &PdfLine, text: &str, lines: &mut Vec<Line>) {
    let mut left = None;
    let mut right = None;
    let mut left_best = 7.0;
    let mut right_best = 7.0;
    for (index, line) in lines.iter().enumerate() {
        let same_baseline = glyph.y >= line.y - 4.0 && glyph.y <= line.y + line.height + 5.0;
        if !same_baseline {
            continue;
        }
        let right_edge = line.x + line.width;
        let dx_left = (right_edge - glyph.x).abs();
        if dx_left < left_best {
            left_best = dx_left;
            left = Some(index);
        }
        let dx_right = (line.x - glyph.x).abs();
        if dx_right < right_best {
            right_best = dx_right;
            right = Some(index);
        }
    }

    let merge = match (left, right) {
        (Some(left_index), Some(right_index)) => {
            left_index != right_index && (lines[left_index].y - lines[right_index].y).abs() < 6.0
        }
        _ => false,
    };

    if merge {
        let (Some(left_index), Some(right_index)) = (left, right) else {
            unreachable!()
        };
        let right_line = lines.remove(right_index);
        let left_index = if left_index > right_index {
            left_index - 1
        } else {
            left_index
        };
        let host = &mut lines[left_index];
        host.text.push_str(text);
        host.text.push_str(&right_line.text);
        host.width = (right_line.x + right_line.width) - host.x;
        absorb_fragment(host, glyph);
        absorb_line(host, &right_line);
    } else if let Some(index) = left {
        let host = &mut lines[index];
        host.text.push_str(text);
        host.width = java_max(host.width, glyph.x + glyph.width - host.x);
        absorb_fragment(host, glyph);
    } else if let Some(index) = right {
        let host = &mut lines[index];
        host.text.insert_str(0, text);
        host.x = glyph.x;
        absorb_fragment(host, glyph);
    } else {
        lines.push(Line::with_text(glyph.clone(), text.to_string()));
    }
}

/// Java `Math.max` propagates NaN where `f32::max` returns the other operand.
fn java_max(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() {
        f32::NAN
    } else {
        a.max(b)
    }
}

fn absorb_fragment(line: &mut Line, fragment: &PdfLine) {
    line.merged.push(fragment.clone());
}

fn absorb_line(line: &mut Line, fragment: &Line) {
    line.merged.push(fragment.source.clone());
    line.merged.extend(fragment.merged.iter().cloned());
}

/// Java-style facade over this module's free functions; the ported pipeline
/// calls the Java `GlyphStitcher` helper as a static class.
#[allow(dead_code)]
pub struct GlyphStitcher;

#[allow(dead_code)]
impl GlyphStitcher {
    pub fn stitch_glyphs(raw: &[PdfLine]) -> Vec<SharedLine> {
        stitch_glyphs(raw)
    }
}
