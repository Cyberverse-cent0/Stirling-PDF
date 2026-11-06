//! Structure-aware packing of Markdown blocks into chunks. Port of
//! `stirling.docparse.chunking`.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;

use crate::contracts::docparse::{DocChunk, MarkdownBlock};
use crate::contracts::documents::PageText;
use crate::documents::chunker::{chunk_text, clen, tail_overlap};

static ATX_HEADING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^#{1,6} \S").unwrap());

fn is_heading(markdown: &str) -> bool {
    ATX_HEADING.is_match(markdown)
}

fn chunk_headings(buffer: &[MarkdownBlock]) -> Vec<String> {
    for block in buffer {
        if is_heading(&block.markdown) {
            return block.heading_path.clone();
        }
    }
    buffer[0].heading_path.clone()
}

/// Flush `buffer` into `chunks` as one DocChunk, then reseed `carry` with the
/// overlap tail of that chunk. Returns the new `buffer_len` (carry length).
fn emit(
    chunks: &mut Vec<DocChunk>,
    buffer: &mut Vec<MarkdownBlock>,
    carry: &mut String,
    overlap: usize,
) -> usize {
    if buffer.is_empty() {
        return clen(carry);
    }
    let mut parts: Vec<String> = Vec::new();
    if !carry.is_empty() {
        parts.push(carry.clone());
    }
    parts.extend(buffer.iter().map(|b| b.markdown.clone()));
    let text = parts.join("\n\n");
    let text = text.trim().to_string();
    if !text.is_empty() {
        chunks.push(DocChunk {
            index: chunks.len() as i64,
            text,
            page_start: buffer.iter().map(|b| b.page_start).min(),
            page_end: buffer.iter().map(|b| b.page_end).max(),
            heading_path: chunk_headings(buffer),
        });
    }
    *carry = chunks
        .last()
        .map_or_else(String::new, |c| tail_overlap(&c.text, overlap));
    buffer.clear();
    clen(carry)
}

/// Pack whole blocks up to `chunk_size`, splitting only a block too big to fit
/// alone. `chunk_size` bounds the new text a chunk holds; on top of that a
/// chunk carries the overlap tail of the one before it.
pub fn pack_blocks(blocks: &[MarkdownBlock], chunk_size: usize, overlap: usize) -> Vec<DocChunk> {
    let mut chunks: Vec<DocChunk> = Vec::new();
    let mut buffer: Vec<MarkdownBlock> = Vec::new();
    let mut buffer_len = 0usize;
    let mut carry = String::new();

    for block in blocks {
        let markdown = block.markdown.trim().to_string();
        if markdown.is_empty() {
            continue;
        }
        let length = clen(&markdown);

        if length > chunk_size {
            // Too big to pack whole: the character chunker applies its own
            // overlap, so nothing carries into or across this branch.
            emit(&mut chunks, &mut buffer, &mut carry, overlap);
            carry.clear();
            for piece in chunk_text(&markdown, chunk_size, overlap) {
                chunks.push(DocChunk {
                    index: chunks.len() as i64,
                    page_start: Some(block.page_start),
                    page_end: Some(block.page_end),
                    heading_path: block.heading_path.clone(),
                    text: piece,
                });
            }
            carry = chunks
                .last()
                .map_or_else(String::new, |c| tail_overlap(&c.text, overlap));
            buffer_len = clen(&carry);
            continue;
        }

        if !buffer.is_empty() && buffer_len + length > chunk_size {
            // Keep a heading with the text it introduces: a trailing run of
            // heading-only blocks moves to the front of the next chunk.
            let mut held: Vec<MarkdownBlock> = Vec::new();
            while buffer.last().is_some_and(|b| is_heading(&b.markdown)) {
                held.insert(0, buffer.pop().unwrap());
            }
            if buffer.is_empty() {
                buffer = held;
                held = Vec::new();
                // buffer_len unchanged: the held headings were already counted.
            }
            buffer_len = emit(&mut chunks, &mut buffer, &mut carry, overlap);
            for heading in held {
                buffer_len += clen(&heading.markdown);
                buffer.push(heading);
            }
            if !buffer.is_empty() && buffer_len + length > chunk_size {
                buffer_len = emit(&mut chunks, &mut buffer, &mut carry, overlap);
            }
        }

        buffer_len += length;
        let mut updated = block.clone();
        updated.markdown = markdown;
        buffer.push(updated);
    }

    emit(&mut chunks, &mut buffer, &mut carry, overlap);
    chunks
}

/// The whole-document read representation: one entry per page that has
/// content. A block stitched across a page break is attributed to where it
/// started, so its text appears once.
pub fn page_texts(blocks: &[MarkdownBlock]) -> Vec<PageText> {
    let mut by_page: BTreeMap<i64, Vec<String>> = BTreeMap::new();
    for block in blocks {
        let markdown = block.markdown.trim();
        if !markdown.is_empty() {
            by_page
                .entry(block.page_start)
                .or_default()
                .push(markdown.to_string());
        }
    }
    by_page
        .into_iter()
        .map(|(number, texts)| PageText {
            page_number: number,
            text: texts.join("\n\n"),
        })
        .collect()
}
