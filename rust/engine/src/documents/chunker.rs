//! Text chunking for embeddings. Port of `stirling.documents.chunker`.
//!
//! All lengths are character counts (Python `len()` semantics = Unicode
//! scalar values), matching the rest of the engine's character budgets.

use std::sync::LazyLock;

use regex::Regex;

static PARAGRAPH_SPLIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n\s*\n").unwrap());

/// Character count matching Python `len(str)`.
pub fn clen(s: &str) -> usize {
    s.chars().count()
}

/// Split text into chunks of approximately `chunk_size` characters with
/// overlap, preferring paragraph then sentence boundaries.
pub fn chunk_text(text: &str, chunk_size: usize, overlap: usize) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }

    let paragraphs = split_paragraphs(text);
    let mut chunks: Vec<String> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut current_len = 0usize;

    for para in paragraphs {
        let para_len = clen(&para);

        if current_len + para_len <= chunk_size {
            current.push(para);
            current_len += para_len;
            continue;
        }

        if !current.is_empty() {
            chunks.push(current.join("\n\n"));
        }

        if para_len > chunk_size {
            let sentence_chunks = split_long_paragraph(&para, chunk_size, overlap);
            chunks.extend(sentence_chunks);
            current.clear();
            current_len = 0;
        } else {
            let overlap_text = get_overlap(&chunks, overlap);
            if !overlap_text.is_empty() {
                current_len = clen(&overlap_text) + para_len;
                current = vec![overlap_text, para];
            } else {
                current = vec![para];
                current_len = para_len;
            }
        }
    }

    if !current.is_empty() {
        chunks.push(current.join("\n\n"));
    }

    chunks
        .into_iter()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .collect()
}

fn split_paragraphs(text: &str) -> Vec<String> {
    PARAGRAPH_SPLIT
        .split(text)
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// Split on sentence ends (`.!?` followed by whitespace), keeping the
/// delimiter attached. Rust `regex` has no lookbehind, so this scans chars.
fn split_sentences(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < chars.len() {
        if matches!(chars[i], '.' | '!' | '?') {
            // Find the end of the whitespace run after the punctuation.
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j > i + 1 && j < chars.len() {
                let sentence: String = chars[start..=i].iter().collect();
                let trimmed = sentence.trim().to_string();
                if !trimmed.is_empty() {
                    out.push(trimmed);
                }
                start = j;
                i = j;
                continue;
            }
        }
        i += 1;
    }
    let tail: String = chars[start..].iter().collect();
    let trimmed = tail.trim().to_string();
    if !trimmed.is_empty() {
        out.push(trimmed);
    }
    out
}

fn split_long_paragraph(paragraph: &str, chunk_size: usize, overlap: usize) -> Vec<String> {
    let sentences = split_sentences(paragraph);
    let mut chunks: Vec<String> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut current_len = 0usize;

    for sentence in sentences {
        let sent_len = clen(&sentence);

        if current_len + sent_len <= chunk_size {
            current.push(sentence);
            current_len += sent_len + 1; // +1 for the joining space
            continue;
        }

        if !current.is_empty() {
            chunks.push(current.join(" "));
        }

        if sent_len > chunk_size {
            // Force-split the over-long sentence on character boundaries.
            let step = chunk_size.saturating_sub(overlap).max(1);
            let chars: Vec<char> = sentence.chars().collect();
            let mut i = 0usize;
            while i < chars.len() {
                let end = (i + chunk_size).min(chars.len());
                chunks.push(chars[i..end].iter().collect());
                i += step;
            }
            current.clear();
            current_len = 0;
        } else {
            let overlap_text = get_overlap(&chunks, overlap);
            if !overlap_text.is_empty() {
                current_len = clen(&overlap_text) + sent_len + 1;
                current = vec![overlap_text, sentence];
            } else {
                current = vec![sentence];
                current_len = sent_len;
            }
        }
    }

    if !current.is_empty() {
        chunks.push(current.join(" "));
    }

    chunks
}

/// Last ~`overlap` characters of `text`, snapped forward past the first space.
pub fn tail_overlap(text: &str, overlap: usize) -> String {
    if text.is_empty() || overlap == 0 {
        return String::new();
    }
    let chars: Vec<char> = text.chars().collect();
    let total = chars.len();
    let start = total.saturating_sub(overlap);
    let mut tail: String = chars[start..].iter().collect();
    if let Some(idx) = tail.find(' ') {
        if idx > 0 {
            tail = tail[idx + 1..].to_string();
        }
    }
    tail
}

fn get_overlap(chunks: &[String], overlap: usize) -> String {
    chunks
        .last()
        .map_or_else(String::new, |c| tail_overlap(c, overlap))
}
