//! Heading promotion heuristics — port of the common
//! `HeadingDetector.java`: the simpler size/brevity/sentence signals the
//! built-in extractor uses. Boldness is deliberately not a heading signal.

use std::collections::HashMap;

use super::markdown_text::{is_regex_space, strip};
use super::model::{PageText, PdfLine};

/// A heading is at most this many words; longer lines are treated as body
/// text.
const MAX_HEADING_WORDS: usize = 12;

/// Returns the Markdown heading prefix for a line. The decision combines
/// several signals, never text matching, so a plain line that merely shares
/// text with a heading is never promoted:
///
/// - **Size** — dominant glyph font size vs. the document body median
///   (primary signal). Some PDFs encode visual size in the text matrix, so
///   every glyph reports ~1.0; for those the line height is the proxy.
/// - **Brevity** — headings are short labels; a line over
///   [`MAX_HEADING_WORDS`] words is body text regardless of size.
/// - **Not a sentence** — a line ending in `. ! ?` reads as prose.
///
/// Boldness is not a heading signal — a bold-but-not-larger line is
/// emphasis, not a heading (see [`is_bold_label`]).
pub fn heading_prefix(
    line: &PdfLine,
    median_body_size: f32,
    median_body_height: f32,
) -> &'static str {
    let line_text = line.text();
    let text = strip(&line_text);
    if text.is_empty() || word_count(text) > MAX_HEADING_WORDS || ends_like_sentence(text) {
        return "";
    }

    let dominant = dominant_font_size(line);
    let (value, baseline) = if dominant > 2.0 && median_body_size > 2.0 {
        (dominant, median_body_size)
    } else {
        (line.height, median_body_height)
    };
    if baseline <= 0.0 {
        return "";
    }

    let ratio = value / baseline;
    if ratio > 1.4 {
        return "# ";
    }
    if ratio > 1.2 {
        return "## ";
    }
    ""
}

/// True when a line should be emphasised as bold (`**like this**`) rather
/// than promoted to a heading: bold, short, and not a full sentence.
pub fn is_bold_label(line: &PdfLine) -> bool {
    let line_text = line.text();
    let text = strip(&line_text);
    if text.is_empty() || word_count(text) > MAX_HEADING_WORDS || ends_like_sentence(text) {
        return false;
    }
    is_bold(line)
}

fn word_count(text: &str) -> usize {
    text.split(|c: char| is_regex_space(c))
        .filter(|s| !s.is_empty())
        .count()
}

fn ends_like_sentence(text: &str) -> bool {
    matches!(text.chars().last(), Some('.') | Some('!') | Some('?'))
}

/// True when the line's dominant font is bold, inferred from PostScript
/// font names.
fn is_bold(line: &PdfLine) -> bool {
    let mut counts: HashMap<String, u32> = HashMap::new();
    for word in &line.words {
        for ch in &word.chars {
            if ch.is_whitespace() || ch.is_newline() {
                continue;
            }
            if !ch.font_name.trim().is_empty() {
                *counts.entry(ch.font_name.clone()).or_insert(0) += 1;
            }
        }
    }
    let mut dominant_font = String::new();
    let mut max: i64 = -1;
    for (name, count) in &counts {
        if (*count as i64) > max {
            max = *count as i64;
            dominant_font = name.clone();
        }
    }
    let lower = dominant_font.to_lowercase();
    lower.contains("bold")
        || lower.contains("black")
        || lower.contains("heavy")
        || lower.contains("semibold")
}

/// Median glyph font size across all pages.
pub fn median_font_size(all_pages: &[PageText]) -> f32 {
    let mut sizes = Vec::new();
    for page in all_pages {
        for ch in &page.chars {
            if !ch.is_whitespace() && !ch.is_newline() && ch.font_size > 0.0 {
                sizes.push(ch.font_size);
            }
        }
    }
    median(&mut sizes, 12.0)
}

/// Median `PdfLine` height across all pages. Used when font size is
/// degenerate.
pub fn median_line_height(all_pages: &[PageText]) -> f32 {
    let mut heights = Vec::new();
    for page in all_pages {
        for line in &page.lines {
            if line.height > 0.0 && !super::markdown_text::is_blank(&line.text()) {
                heights.push(line.height);
            }
        }
    }
    median(&mut heights, 12.0)
}

fn median(values: &mut [f32], fallback: f32) -> f32 {
    if values.is_empty() {
        return fallback;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    }
}

/// The font size appearing most often (by character count) in the line;
/// ties are broken in favour of the larger size.
fn dominant_font_size(line: &PdfLine) -> f32 {
    let mut counts: HashMap<u32, u32> = HashMap::new();
    for word in &line.words {
        for ch in &word.chars {
            if !ch.is_whitespace() && !ch.is_newline() && ch.font_size > 0.0 {
                *counts.entry(ch.font_size.to_bits()).or_insert(0) += 1;
            }
        }
    }
    if counts.is_empty() {
        return 0.0;
    }
    let mut dominant = 0.0f32;
    let mut max_count: i64 = -1;
    for (bits, count) in &counts {
        let size = f32::from_bits(*bits);
        let count = *count as i64;
        if count > max_count || (count == max_count && size > dominant) {
            max_count = count;
            dominant = size;
        }
    }
    dominant
}
