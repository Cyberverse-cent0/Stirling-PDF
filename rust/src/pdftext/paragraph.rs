//! Turns a column's lines into Markdown blocks — port of
//! `ParagraphAssembler.java`: headings (including wrapped ones), bullets,
//! bold labels and paragraphs. Contents lists are recognised and left
//! alone.

use std::collections::HashSet;

use super::heading;
use super::line::{Line, SharedLine};
use super::markdown_text::{self, is_digit, strip};

/// Glyphs a document may set its list markers in beyond the three already
/// recognised.
const EXTRA_BULLETS: &str = "‣⁃▶●○■□◆⮚➢➣➤";

/// Longest a heading may grow to by absorbing its continuation lines, in
/// words.
const MAX_WRAPPED_HEADING_WORDS: usize = 24;

/// How far a continuation line's type size may differ from the line it
/// continues.
const WRAP_SIZE_TOLERANCE: f32 = 0.2;

/// Entries this many lines long make a contents list rather than a
/// coincidence.
const MIN_CONTENTS_RUN: usize = 3;

pub fn assemble_paragraphs(
    lines: &[SharedLine],
    median_size: f32,
    median_height: f32,
    body_font: &str,
    out: &mut Vec<String>,
    table_row_texts: &HashSet<String>,
) {
    let mut para = String::new();
    let mut prev_bottom_y = f32::MAX;
    let mut prev_height = 0.0f32;
    let in_contents = contents_run(lines);

    let mut i = 0usize;
    while i < lines.len() {
        // `li` tracks the line the gap bookkeeping belongs to: a wrapped
        // heading advances it to the last absorbed continuation.
        let mut li = i;
        let line = &lines[li];
        let text = strip(&line.text).to_string();
        if text.is_empty() {
            i += 1;
            continue;
        }
        if table_row_texts.contains(&text) {
            i += 1;
            continue;
        }

        let block_top = line.y + line.height;
        let gap = prev_bottom_y - block_top;
        let paragraph_break = prev_height > 0.0 && gap > prev_height * 0.8;
        // A contents entry carries the typography of the section it points
        // at without being that section, so nothing on one is promoted or
        // emphasised.
        let structural = in_contents[i];

        // A field value is data, never a heading: its widget box is taller
        // than a text line and would otherwise be promoted purely on
        // height.
        let mut prefix = if line.synthetic || structural {
            ""
        } else {
            heading::heading_prefix_parts(
                &line.detect_text(),
                line.detect_height(),
                &line.words(),
                median_size,
                median_height,
                body_font,
                prev_height <= 0.0 || paragraph_break,
            )
        };
        if prefix.is_empty() && !structural && contents_title(lines, &in_contents, i) {
            // The line a contents list runs on from is its heading: a
            // contents page is often set in one face, leaving no size or
            // weight to promote it on.
            prefix = "# ";
        }
        let is_bullet = starts_with_bullet(&text);
        // A line that opens with a list marker is an item of a list,
        // whatever it is set in.
        let is_heading = !prefix.is_empty() && !is_bullet;

        if is_heading {
            markdown_text::flush_paragraph(&mut para, out);
            let mut heading_text = markdown_text::escape_markdown(&text);
            let mut words = markdown_text::word_count(&text);
            let mut j = i;
            let mut k = i + 1;
            while k < lines.len() && words < MAX_WRAPPED_HEADING_WORDS {
                let next = &lines[k];
                let nt = strip(&next.text).to_string();
                if nt.is_empty() {
                    // An empty extractor record is not a break in the text;
                    // the vertical gap below decides whether the heading
                    // ended.
                    k += 1;
                    continue;
                }
                if in_contents[k] || table_row_texts.contains(&nt) {
                    break;
                }
                if !wraps_heading(
                    &lines[j],
                    next,
                    prefix,
                    median_size,
                    median_height,
                    body_font,
                ) {
                    break;
                }
                heading_text.push(' ');
                heading_text.push_str(&markdown_text::escape_markdown(&nt));
                words += markdown_text::word_count(&nt);
                j = k;
                k += 1;
            }
            out.push(format!("{prefix}{heading_text}"));
            if j > i {
                li = j;
                i = j;
            }
        } else if is_bullet {
            markdown_text::flush_paragraph(&mut para, out);
            out.push(markdown_text::escape_markdown(&text));
        } else if !line.synthetic
            && !structural
            && heading::is_bold_label(&line.detect_text(), &line.words())
        {
            // Bold but not large enough to be a heading → emphasise as
            // bold, don't promote.
            markdown_text::flush_paragraph(&mut para, out);
            out.push(format!("**{}**", markdown_text::escape_markdown(&text)));
        } else if paragraph_break {
            markdown_text::flush_paragraph(&mut para, out);
            para.push_str(&text);
        } else {
            if !para.is_empty() {
                let fc = text.chars().next().unwrap();
                let no_space = matches!(fc, '\'' | '’' | '‘' | '"');
                if !no_space {
                    para.push(' ');
                }
            }
            para.push_str(&text);
        }

        prev_bottom_y = lines[li].y;
        prev_height = lines[li].height;
        i += 1;
    }
    markdown_text::flush_paragraph(&mut para, out);
}

fn starts_with_bullet(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    if text.starts_with('•') || text.starts_with('▪') || text.starts_with('◦') {
        return true;
    }
    let c = text.chars().next().unwrap();
    EXTRA_BULLETS.contains(c)
}

/// True when `next` continues a wrapped heading: each visual line arrives
/// separately, so an unjoined heading emits as several spurious ones.
fn wraps_heading(
    head: &Line,
    next: &Line,
    prefix: &str,
    median_size: f32,
    median_height: f32,
    body_font: &str,
) -> bool {
    if next.synthetic {
        return false;
    }
    let height = head.detect_height();
    if height <= 0.0 {
        return false;
    }
    // The next baseline down, not the next block. The same 0.8 the
    // paragraph assembler uses, so a heading absorbs exactly what the
    // converter already calls one block.
    let gap = head.y - (next.y + next.height);
    if gap > height * 0.8 || gap < -height * 0.5 {
        return false;
    }
    let next_height = next.detect_height();
    if (next_height - height).abs() > WRAP_SIZE_TOLERANCE * next_height.max(height) {
        return false;
    }
    // Same column: an x-range that misses the heading's belongs to another
    // block entirely.
    if next.x >= head.x + head.width || head.x >= next.x + next.width {
        return false;
    }
    // A heading does not run to a full stop and then start another
    // sentence; the bold run-in lead-in below it does, and nothing else
    // tells the two apart.
    if heading::sentence_break_find(&next.text) {
        return false;
    }
    let words = next.words();
    let next_prefix = heading::heading_prefix_parts(
        &next.detect_text(),
        next.detect_height(),
        &words,
        median_size,
        median_height,
        body_font,
        false,
    );
    // Either the continuation is display type in its own right, or it is
    // the bold remainder of a run-in heading, which cannot be promoted on
    // its own because no gap precedes it.
    next_prefix == prefix
        || (next_prefix.is_empty() && heading::is_bold_label(&next.detect_text(), &words))
}

/// A leader run: the dots that carry the eye from a contents entry to its
/// page number — `([.][ ]?){4,}|[.·]{3,}|…{2,}`.
fn leader_find(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    for i in 0..n {
        if chars[i] == '\u{2026}' {
            let mut j = i + 1;
            while j < n && chars[j] == '\u{2026}' {
                j += 1;
            }
            if j - i >= 2 {
                return true;
            }
        }
        if chars[i] == '.' || chars[i] == '\u{00B7}' {
            let mut j = i + 1;
            while j < n && (chars[j] == '.' || chars[j] == '\u{00B7}') {
                j += 1;
            }
            if j - i >= 3 {
                return true;
            }
        }
        if chars[i] == '.' {
            // `([.][ ]?){4,}`: each unit is a dot plus an optional single
            // space; consuming the space greedily is always at least as
            // good, since only a dot can start the next unit.
            let mut count = 0usize;
            let mut j = i;
            while j < n && chars[j] == '.' {
                count += 1;
                j += 1;
                if j < n && chars[j] == ' ' {
                    j += 1;
                }
            }
            if count >= 4 {
                return true;
            }
        }
    }
    false
}

/// Marks the lines of a contents list: titles joined to page numbers by
/// leader dots, carrying the typography of the sections they point at.
fn contents_run(lines: &[SharedLine]) -> Vec<bool> {
    let mut entry = vec![false; lines.len()];
    let mut run = 0usize;
    for i in 0..lines.len() {
        let t = &lines[i].text;
        if leader_find(t) && ends_with_number(t) {
            entry[i] = true;
            run += 1;
        } else {
            if run < MIN_CONTENTS_RUN {
                clear(&mut entry, i.saturating_sub(run), i);
            }
            run = 0;
        }
    }
    if run < MIN_CONTENTS_RUN {
        clear(&mut entry, lines.len().saturating_sub(run), lines.len());
    }
    entry
}

fn clear(flags: &mut [bool], from: usize, to: usize) {
    for flag in flags.iter_mut().take(to).skip(from) {
        *flag = false;
    }
}

fn ends_with_number(text: &str) -> bool {
    let t = strip(text);
    !t.is_empty() && t.chars().last().is_some_and(is_digit)
}

/// True for the short line a contents list runs on from: the list's own
/// heading.
fn contents_title(lines: &[SharedLine], in_contents: &[bool], index: usize) -> bool {
    if index + 1 >= lines.len() || in_contents[index] || !in_contents[index + 1] {
        return false;
    }
    let t = strip(&lines[index].text);
    !t.is_empty() && t.split(' ').filter(|s| !s.is_empty()).count() <= 6 && !ends_with_number(t)
}
