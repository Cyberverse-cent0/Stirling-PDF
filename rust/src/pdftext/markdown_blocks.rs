//! One rendered Markdown element with the pages it came from — port of
//! `MarkdownBlock.java` / `MarkdownBlocks.java`: the block record plus the
//! document-wide passes (joining, heading-level rebasing, heading paths).

use super::markdown_text::is_regex_space;

/// Page numbers are 1-based and differ only for content stitched across a
/// page break.
#[derive(Debug, Clone)]
pub struct MarkdownBlock {
    pub markdown: String,
    pub page_start: usize,
    pub page_end: usize,
    pub heading_path: Vec<String>,
}

impl MarkdownBlock {
    pub fn new(markdown: impl Into<String>, page_start: usize, page_end: usize) -> Self {
        Self {
            markdown: markdown.into(),
            page_start,
            page_end,
            heading_path: Vec::new(),
        }
    }

    pub fn with_markdown(&self, replacement: impl Into<String>) -> Self {
        Self {
            markdown: replacement.into(),
            page_start: self.page_start,
            page_end: self.page_end,
            heading_path: self.heading_path.clone(),
        }
    }

    pub fn with_heading_path(&self, path: Vec<String>) -> Self {
        Self {
            markdown: self.markdown.clone(),
            page_start: self.page_start,
            page_end: self.page_end,
            heading_path: path,
        }
    }
}

/// Separator between blocks; it ends in a newline, so every block starts
/// at a line start.
pub const SEPARATOR: &str = "\n\n";

/// Opening marker of the placeholder both converters emit in place of an
/// image's bytes.
pub const IMAGE_PLACEHOLDER: &str = "<image redacted";

/// Every character `escape_markdown` may have backslash-escaped, so
/// unescaping is exact.
const ESCAPABLE: &str = "\\`*_[]<>|~#-+.)";

/// Document-wide passes over a block list: joining, heading-level
/// rebasing, heading paths.
pub fn join(blocks: &[MarkdownBlock]) -> String {
    blocks
        .iter()
        .map(|b| b.markdown.as_str())
        .collect::<Vec<_>>()
        .join(SEPARATOR)
}

/// Rebases headings so the strongest is level 1 and no level is skipped:
/// levels only mean anything against the other headings in the same
/// document. The set is collected over the whole list, so the decision is
/// made once and then applied block by block.
pub fn normalise_heading_levels(blocks: Vec<MarkdownBlock>) -> Vec<MarkdownBlock> {
    let mut levels = std::collections::BTreeSet::new();
    for block in &blocks {
        for (_, _, level) in atx_matches(&block.markdown) {
            levels.insert(level);
        }
    }
    let max_level = levels.iter().copied().max().unwrap_or(0);
    if levels.is_empty() || (levels.contains(&1) && levels.len() == max_level) {
        return blocks;
    }
    let rebased: std::collections::HashMap<usize, String> = levels
        .iter()
        .enumerate()
        .map(|(rank, level)| (*level, "#".repeat(rank + 1)))
        .collect();
    blocks
        .iter()
        .map(|block| {
            let markdown = rebase_headings(&block.markdown, &rebased);
            block.with_markdown(markdown)
        })
        .collect()
}

fn rebase_headings(markdown: &str, rebased: &std::collections::HashMap<usize, String>) -> String {
    let matches = atx_matches(markdown);
    if matches.is_empty() {
        return markdown.to_string();
    }
    let mut out = String::with_capacity(markdown.len());
    let mut cursor = 0usize;
    for (start, end, level) in matches {
        out.push_str(&markdown[cursor..start]);
        out.push_str(rebased.get(&level).map(String::as_str).unwrap_or(""));
        out.push(' ');
        cursor = end;
    }
    out.push_str(&markdown[cursor..]);
    out
}

/// Each block's enclosing heading chain; a heading block includes itself.
pub fn with_heading_paths(blocks: Vec<MarkdownBlock>) -> Vec<MarkdownBlock> {
    let mut open: Vec<Heading> = Vec::new();
    let mut out = Vec::with_capacity(blocks.len());
    for block in blocks {
        let level = heading_level(&block.markdown);
        if level > 0 {
            while open.last().is_some_and(|h| h.level >= level) {
                open.pop();
            }
            open.push(Heading {
                level,
                text: heading_text(&block.markdown, level),
            });
        }
        let path: Vec<String> = open.iter().map(|h| h.text.clone()).collect();
        out.push(block.with_heading_path(path));
    }
    out
}

/// ATX level of a block's own first line, or 0 when the block is not a
/// heading.
pub fn heading_level(markdown: &str) -> usize {
    let chars: Vec<(usize, char)> = markdown.char_indices().collect();
    atx_at(&chars, 0).map(|m| m.2).unwrap_or(0)
}

/// True when a block is nothing but an image placeholder, so it carries
/// no readable text.
pub fn is_image_placeholder(markdown: &str) -> bool {
    super::markdown_text::strip(markdown).starts_with(IMAGE_PLACEHOLDER)
}

/// Reverses `escape_markdown` so a heading path reads as the page does.
pub fn unescape(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() && ESCAPABLE.contains(chars[i + 1]) {
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

fn heading_text(markdown: &str, level: usize) -> String {
    let rest: String = markdown.chars().skip(level + 1).collect();
    super::markdown_text::strip(&unescape(&rest)).to_string()
}

/// A Markdown ATX heading at a line start: `^(#{1,6}) (?=\S)` with `(?m)`.
/// Returns (byte start, byte end, level) for each match; the end is just
/// past the single separating space.
fn atx_matches(markdown: &str) -> Vec<(usize, usize, usize)> {
    let chars: Vec<(usize, char)> = markdown.char_indices().collect();
    let mut out = Vec::new();
    for (idx, &(byte_pos, _)) in chars.iter().enumerate() {
        if !is_line_start(&chars, idx) {
            continue;
        }
        if let Some(m) = atx_at(&chars, idx) {
            out.push((byte_pos, chars[m.1].0, m.2));
        }
    }
    out
}

/// `(?m)^` line-start positions, treating `\r\n` as one terminator like
/// Java does.
fn is_line_start(chars: &[(usize, char)], idx: usize) -> bool {
    if idx == 0 {
        return true;
    }
    match chars[idx - 1].1 {
        '\n' | '\u{85}' | '\u{2028}' | '\u{2029}' => true,
        '\r' => chars[idx].1 != '\n',
        _ => false,
    }
}

/// The `#{1,6} ` match at char index `start`, returning (hashes end char
/// index, lookahead char index, level).
fn atx_at(chars: &[(usize, char)], start: usize) -> Option<(usize, usize, usize)> {
    let mut level = 0usize;
    let mut i = start;
    while i < chars.len() && chars[i].1 == '#' {
        level += 1;
        i += 1;
    }
    if level == 0 || level > 6 {
        return None;
    }
    if i >= chars.len() || chars[i].1 != ' ' {
        return None;
    }
    let lookahead = i + 1;
    if lookahead >= chars.len() || is_regex_space(chars[lookahead].1) {
        return None;
    }
    Some((i, lookahead, level))
}

struct Heading {
    level: usize,
    text: String,
}

/// Java-style facade over this module's free functions; the ported pipeline
/// calls the Java `MarkdownBlocks` helper as a static class.
#[allow(dead_code)]
pub struct MarkdownBlocks;

#[allow(dead_code)]
impl MarkdownBlocks {
    pub fn join(blocks: &[MarkdownBlock]) -> String {
        join(blocks)
    }
    pub fn normalise_heading_levels(blocks: Vec<MarkdownBlock>) -> Vec<MarkdownBlock> {
        normalise_heading_levels(blocks)
    }
    pub fn with_heading_paths(blocks: Vec<MarkdownBlock>) -> Vec<MarkdownBlock> {
        with_heading_paths(blocks)
    }
    pub fn heading_level(markdown: &str) -> usize {
        heading_level(markdown)
    }
    pub fn is_image_placeholder(markdown: &str) -> bool {
        is_image_placeholder(markdown)
    }
    pub fn unescape(text: &str) -> String {
        unescape(text)
    }
}
