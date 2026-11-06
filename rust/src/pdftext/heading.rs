//! Heading promotion heuristics — port of the proprietary
//! `HeadingDetector.java`: size ratio, brevity, isolation, weight and
//! clause shape, never text matching.

use std::collections::HashMap;

use super::markdown_text::{is_blank, is_java_whitespace, is_regex_space, strip};
use super::model::{PageText, PdfLine, PdfWord};

/// A heading is at most this many words; longer lines are body text.
const MAX_HEADING_WORDS: usize = 12;

/// A heading in a script written without word spaces is at most this many
/// characters.
const MAX_HEADING_UNSPACED_CHARS: usize = 30;

/// Size ratio at which a line is a level-1 heading on size alone.
const H1_RATIO: f32 = 1.4;

/// Size ratio at which a line is a level-2 heading on size alone.
const H2_RATIO: f32 = 1.3;

/// PostScript name fragments denoting a weight heavier than regular.
const BOLD_TOKENS: [&str; 7] = [
    "bold",
    "black",
    "heavy",
    "semibold",
    "demi",
    "ultra",
    "extrabold",
];

/// A float label followed by its number: set like a heading but naming an
/// illustration, not a section.
const CAPTION_KEYS: [&str; 10] = [
    "table",
    "figure",
    "fig",
    "chart",
    "exhibit",
    "plate",
    "scheme",
    "graph",
    "diagram",
    "illustration",
];

/// Markdown heading prefix from size, brevity, isolation and weight.
/// Bold is vetoed on the body face itself.
pub fn heading_prefix(
    line: &PdfLine,
    median_body_size: f32,
    median_body_height: f32,
    body_font: &str,
    isolated: bool,
) -> &'static str {
    let words: Vec<&PdfWord> = line.words.iter().collect();
    heading_prefix_parts(
        &line.text(),
        line.height,
        &words,
        median_body_size,
        median_body_height,
        body_font,
        isolated,
    )
}

/// Geometry-only form, judging a merged line on its merged text, height
/// and words. Boldness counts only when the face differs from `body_font`.
pub fn heading_prefix_parts(
    line_text: &str,
    line_height: f32,
    words: &[&PdfWord],
    median_body_size: f32,
    median_body_height: f32,
    body_font: &str,
    isolated: bool,
) -> &'static str {
    let text = strip(line_text);
    if text.is_empty() || too_long_for_heading(text) {
        return "";
    }
    let ratio = size_ratio(line_height, words, median_body_size, median_body_height);
    if ratio < 0.0 {
        return "";
    }
    // A heading names something. A line with no word in it is a value, an
    // equation fragment or a chart label, however large it is set; a
    // caption names a float, not a section.
    if caption_match(text) || !has_word(text) {
        return "";
    }
    if runs_on_find(text) {
        return "";
    }

    // Bold only marks a heading when it stands out from the body face.
    // Some documents set the whole body in a bold-named font, where
    // boldness carries no structural meaning.
    let bold = is_bold(words) && normalised_font(words) != normalise(body_font);
    // A line that reads as a sentence is prose, unless it carries heading
    // typography.
    if ends_like_sentence(text) && !bold && ratio <= H2_RATIO {
        return "";
    }

    if ratio > H1_RATIO {
        return "# ";
    }
    if ratio > H2_RATIO {
        return "## ";
    }
    // A numbered clause: the section number is the structure, so it needs
    // no blank line above it to be one.
    if bold && clause_match(text) {
        return "### ";
    }
    // Same size as the body but bold and starting its own block: a run-in
    // section heading.
    if bold && isolated && has_word(text) {
        return "### ";
    }
    // Some documents give a clause heading no size and no weight, only
    // capitals; the section number carries the evidence typography does not.
    if isolated
        && !ends_like_sentence(text)
        && word_count(text) >= 3
        && is_all_caps(text)
        && numbered_clause_match(text)
    {
        return "### ";
    }
    ""
}

/// True when a line should be emphasised as bold rather than promoted:
/// bold, short, and not a full sentence.
pub fn is_bold_label(line_text: &str, words: &[&PdfWord]) -> bool {
    let text = strip(line_text);
    if text.is_empty() || too_long_for_heading(text) || ends_like_sentence(text) {
        return false;
    }
    if runs_on_find(text) {
        return false;
    }
    has_word(text) && is_bold(words)
}

/// Most-used font across the document, weighted by glyph count: the body
/// face.
pub fn body_font(all_pages: &[PageText]) -> String {
    let mut counts: HashMap<String, u32> = HashMap::new();
    for page in all_pages {
        for ch in &page.chars {
            if ch.is_whitespace() || ch.is_newline() {
                continue;
            }
            if !ch.font_name.trim().is_empty() {
                *counts.entry(normalise(&ch.font_name)).or_insert(0) += 1;
            }
        }
    }
    let mut dominant = String::new();
    let mut max: i64 = -1;
    for (name, count) in &counts {
        if (*count as i64) > max {
            max = *count as i64;
            dominant = name.clone();
        }
    }
    dominant
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

/// Body baseline for the height path: median per-line `glyph_height`
/// weighted by glyph count, so tiny axis labels cannot drag it down.
pub fn median_line_height(all_pages: &[PageText]) -> f32 {
    let mut weighted: Vec<(f32, f32)> = Vec::new();
    let mut total = 0.0f64;
    for page in all_pages {
        for line in &page.lines {
            if is_blank(&line.text()) {
                continue;
            }
            let words: Vec<&PdfWord> = line.words.iter().collect();
            let mut h = glyph_height(&words);
            if h <= 0.0 {
                h = line.height;
            }
            if h <= 0.0 {
                continue;
            }
            let w = glyph_count(line);
            weighted.push((h, w));
            total += w as f64;
        }
    }
    if weighted.is_empty() {
        return 12.0;
    }
    weighted.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let half = total / 2.0;
    let mut seen = 0.0f64;
    for p in &weighted {
        seen += p.1 as f64;
        if seen >= half {
            return p.0;
        }
    }
    weighted[weighted.len() - 1].0
}

fn size_ratio(
    line_height: f32,
    words: &[&PdfWord],
    median_body_size: f32,
    median_body_height: f32,
) -> f32 {
    let dominant = dominant_font_size(words);
    let (value, baseline) = if dominant > 2.0 && median_body_size > 2.0 {
        (dominant, median_body_size)
    } else {
        let glyph = glyph_height(words);
        (
            if glyph > 0.0 { glyph } else { line_height },
            median_body_height,
        )
    };
    if baseline <= 0.0 {
        -1.0
    } else {
        value / baseline
    }
}

/// Quantile of a line's glyph heights taken as its size: high enough for
/// the cap band, low enough that one rogue glyph box cannot set it.
const GLYPH_HEIGHT_QUANTILE: f32 = 0.8;

/// A line's type size from its glyphs, for PDFs encoding visual size in
/// the text matrix; the line box runs ascender to descender.
fn glyph_height(words: &[&PdfWord]) -> f32 {
    let capacity: usize = words.iter().map(|w| w.chars.len()).sum();
    if capacity == 0 {
        return 0.0;
    }
    let mut heights = Vec::with_capacity(capacity);
    for word in words {
        for ch in &word.chars {
            if ch.is_whitespace() || ch.is_newline() {
                continue;
            }
            // Letters and digits only: brackets and maths operators are
            // drawn taller than the cap height, so an equation would
            // measure as display type.
            if !ch.ch.is_alphanumeric() {
                continue;
            }
            let h = ch.height;
            if h.is_finite() && h > 0.0 {
                heights.push(h);
            }
        }
    }
    if heights.is_empty() {
        return 0.0;
    }
    heights.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = heights.len();
    heights[(GLYPH_HEIGHT_QUANTILE * (n - 1) as f32) as usize]
}

/// Strips a six-letter subset tag PDF writers prepend to embedded font
/// names (`^[A-Z]{6}\+`).
fn normalise(font_name: &str) -> String {
    let b = font_name.as_bytes();
    if b.len() >= 7 && b[..6].iter().all(|c| c.is_ascii_uppercase()) && b[6] == b'+' {
        font_name[7..].to_string()
    } else {
        font_name.to_string()
    }
}

fn normalised_font(words: &[&PdfWord]) -> String {
    normalise(&dominant_font_name(words))
}

/// A heading is made of words: at least two letters, and letters at least
/// half the glyphs.
fn has_word(text: &str) -> bool {
    let mut letters = 0usize;
    let mut glyphs = 0usize;
    for c in text.chars() {
        if is_java_whitespace(c) {
            continue;
        }
        glyphs += 1;
        if c.is_alphabetic() {
            letters += 1;
        }
    }
    letters >= 2 && letters * 2 >= glyphs
}

fn word_count(text: &str) -> usize {
    text.split(|c: char| is_regex_space(c))
        .filter(|s| !s.is_empty())
        .count()
}

/// Whitespace tokens do not measure a line written in a script with no
/// word spaces.
fn too_long_for_heading(text: &str) -> bool {
    word_count(text) > MAX_HEADING_WORDS || unspaced_script_chars(text) > MAX_HEADING_UNSPACED_CHARS
}

fn unspaced_script_chars(text: &str) -> usize {
    text.chars().filter(|&c| is_unspaced_script(c)).count()
}

/// Scripts written without word spaces: the CJK and Southeast-Asian
/// ranges `Character.UnicodeScript` reports as HAN through MYANMAR.
fn is_unspaced_script(c: char) -> bool {
    matches!(
        c as u32,
        0x3040..=0x309F // Hiragana
        | 0x30A0..=0x30FF // Katakana
        | 0x31F0..=0x31FF // Katakana phonetic extensions
        | 0xFF66..=0xFF9D // halfwidth katakana
        | 0x0E00..=0x0E7F // Thai
        | 0x0E80..=0x0EFF // Lao
        | 0x1780..=0x17FF // Khmer
        | 0x19E0..=0x19FF // Khmer symbols
        | 0x1000..=0x109F // Myanmar
        | 0xAA60..=0xAA7F // Myanmar Extended-A
        | 0xA9E0..=0xA9FF // Myanmar Extended-B
        | 0x4E00..=0x9FFF // CJK unified ideographs
        | 0x3400..=0x4DBF // CJK extension A
        | 0xF900..=0xFAFF // CJK compatibility ideographs
        | 0x20000..=0x2A6DF // CJK extension B
        | 0x2A700..=0x2B73F // CJK extension C
        | 0x2B740..=0x2B81F // CJK extension D
        | 0x2B820..=0x2EBEF // CJK extensions E-I
        | 0x30000..=0x3134F // CJK extensions G-H
        | 0x2F800..=0x2FA1F // CJK compatibility supplement
    )
}

fn ends_like_sentence(text: &str) -> bool {
    // Ideographic and full-width stops end a sentence exactly as the ASCII
    // ones do.
    matches!(
        text.chars().last(),
        Some('.')
            | Some('!')
            | Some('?')
            | Some('\u{3002}')
            | Some('\u{FF01}')
            | Some('\u{FF1F}')
            | Some('\u{FF61}')
    )
}

/// True when the line's dominant font is bold, inferred from PostScript
/// font names.
fn is_bold(words: &[&PdfWord]) -> bool {
    let lower = normalised_font(words).to_lowercase();
    if BOLD_TOKENS.iter().any(|t| lower.contains(t)) {
        return true;
    }
    // URW/Nimbus bold is "-Medi", TeX bold extended is CMBX/CMSSBX.
    // "Medium" is excluded: matching it makes whole CJK paragraphs bold.
    other_bold(&lower)
}

/// `medi(?!um)|cm(ss)?bx` on the lowercased font name.
fn other_bold(lower: &str) -> bool {
    for (i, _) in lower.match_indices("medi") {
        if !lower[i + 4..].starts_with("um") {
            return true;
        }
    }
    lower.contains("cmbx") || lower.contains("cmssbx")
}

fn dominant_font_name(words: &[&PdfWord]) -> String {
    let mut counts: HashMap<String, u32> = HashMap::new();
    for word in words {
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
    dominant_font
}

/// The font size appearing most often by character count in the line;
/// ties go to the larger size.
fn dominant_font_size(words: &[&PdfWord]) -> f32 {
    let mut counts: HashMap<u32, u32> = HashMap::new();
    for word in words {
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

/// How much text a line carries, in glyphs; at least one so an empty line
/// still counts.
fn glyph_count(line: &PdfLine) -> f32 {
    let mut glyphs = 0usize;
    for word in &line.words {
        for ch in &word.chars {
            if !ch.is_whitespace() && !ch.is_newline() {
                glyphs += 1;
            }
        }
    }
    glyphs.max(1) as f32
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

fn caption_match(text: &str) -> bool {
    CAPTION_KEYS.iter().any(|key| {
        let Some(rest) = strip_prefix_ignore_ascii_case(text, key) else {
            return false;
        };
        caption_tail(rest)
    })
}

/// CASE_INSENSITIVE in Java without UNICODE_CASE folds ASCII only.
fn strip_prefix_ignore_ascii_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let pb = prefix.as_bytes();
    let tb = text.as_bytes();
    if tb.len() < pb.len() || !tb[..pb.len()].eq_ignore_ascii_case(pb) {
        return None;
    }
    text.get(pb.len()..)
}

/// `\s*\.?\s*\d` after the caption keyword.
fn caption_tail(rest: &str) -> bool {
    let chars: Vec<char> = rest.chars().collect();
    let mut i = 0;
    while i < chars.len() && is_regex_space(chars[i]) {
        i += 1;
    }
    if i < chars.len() && chars[i] == '.' {
        i += 1;
    }
    while i < chars.len() && is_regex_space(chars[i]) {
        i += 1;
    }
    i < chars.len() && chars[i].is_ascii_digit()
}

/// `\p{Ll}{3}[.!?][\s\u00a0]+\p{Lu}`: two sentences in a row — a bold
/// run-in lead-in, not a heading.
fn runs_on_find(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    for i in 0..chars.len() {
        if i + 3 >= chars.len() {
            break;
        }
        if !(0..3).all(|k| chars[i + k].is_lowercase()) {
            continue;
        }
        let mut j = i + 3;
        if !matches!(chars[j], '.' | '!' | '?') {
            continue;
        }
        j += 1;
        let space_start = j;
        while j < chars.len() && (is_regex_space(chars[j]) || chars[j] == '\u{A0}') {
            j += 1;
        }
        if j > space_start && j < chars.len() && chars[j].is_uppercase() {
            return true;
        }
    }
    false
}

/// `[.!?]\s+\p{Lu}`; also the wrapped-heading guard in `paragraph.rs`.
pub(crate) fn sentence_break_find(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    for i in 0..chars.len() {
        if !matches!(chars[i], '.' | '!' | '?') {
            continue;
        }
        let mut j = i + 1;
        let start = j;
        while j < chars.len() && is_regex_space(chars[j]) {
            j += 1;
        }
        if j > start && j < chars.len() && chars[j].is_uppercase() {
            return true;
        }
    }
    false
}

/// `^\d{1,2}(\.\d{1,2})*\.?\s+\p{Lu}`: a numbered section clause.
fn clause_match(text: &str) -> bool {
    clause_tail_check(text, false)
}

/// `^\d{1,2}(\.\d{1,2})*\.\s+\p{Lu}`: stricter — only the period tells a
/// clause from a header's page number.
fn numbered_clause_match(text: &str) -> bool {
    clause_tail_check(text, true)
}

fn clause_tail_check(text: &str, numbered: bool) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut positions: Vec<usize> = Vec::new();
    for dlen in [2usize, 1] {
        if dlen <= n && chars[..dlen].iter().all(|c| c.is_ascii_digit()) {
            positions.push(dlen);
        }
    }
    let mut seen = vec![false; n + 1];
    let mut work: Vec<usize> = Vec::new();
    for &p in &positions {
        if p <= n && !seen[p] {
            seen[p] = true;
            work.push(p);
        }
    }
    while let Some(p) = work.pop() {
        if clause_end(&chars, p, numbered) {
            return true;
        }
        // One more `(\.\d{1,2})` group: `\d{1,2}` is greedy but backtracks,
        // so both the one- and two-digit ends are reachable.
        if p < n && chars[p] == '.' && p + 1 < n && chars[p + 1].is_ascii_digit() {
            let mut ends = vec![p + 2];
            if p + 2 < n && chars[p + 2].is_ascii_digit() {
                ends.push(p + 3);
            }
            for end in ends {
                if end <= n && !seen[end] {
                    seen[end] = true;
                    work.push(end);
                }
            }
        }
    }
    false
}

/// `\.?\s+\p{Lu}` (or `\.\s+\p{Lu}` when `numbered`) at char position `p`.
fn clause_end(chars: &[char], p: usize, numbered: bool) -> bool {
    let n = chars.len();
    if numbered {
        return p < n && chars[p] == '.' && space_upper_tail(chars, p + 1);
    }
    if p < n && chars[p] == '.' && space_upper_tail(chars, p + 1) {
        return true;
    }
    space_upper_tail(chars, p)
}

/// `\s+\p{Lu}` at char position `i`.
fn space_upper_tail(chars: &[char], mut i: usize) -> bool {
    let n = chars.len();
    let start = i;
    while i < n && is_regex_space(chars[i]) {
        i += 1;
    }
    i > start && i < n && chars[i].is_uppercase()
}

/// True when every cased letter is a capital; digits and uncased scripts do
/// not count, so `BIO 181` qualifies.
fn is_all_caps(text: &str) -> bool {
    let mut upper = 0usize;
    for c in text.chars() {
        if c.is_lowercase() {
            return false;
        }
        if c.is_uppercase() {
            upper += 1;
        }
    }
    upper >= 2
}
