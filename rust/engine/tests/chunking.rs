//! Regression tests ported from `engine/tests/docparse/test_chunking.py`.
//! Golden expectations were produced by the Python implementation.

use stirling_engine::contracts::docparse::{DocChunk, MarkdownBlock};
use stirling_engine::docparse::chunking::{pack_blocks, page_texts};
use stirling_engine::documents::chunker::{chunk_text, clen, tail_overlap};

fn block(markdown: &str, page: i64, page_end: Option<i64>, headings: &[&str]) -> MarkdownBlock {
    MarkdownBlock {
        markdown: markdown.to_string(),
        page_start: page,
        page_end: page_end.unwrap_or(page),
        heading_path: headings.iter().map(|s| s.to_string()).collect(),
    }
}

fn texts(chunks: &[DocChunk]) -> Vec<String> {
    chunks.iter().map(|c| c.text.clone()).collect()
}

#[test]
fn packs_whole_blocks_up_to_chunk_size() {
    let chunks = pack_blocks(
        &[
            block("aaa", 1, None, &[]),
            block("bbb", 1, None, &[]),
            block("ccc", 1, None, &[]),
        ],
        10,
        0,
    );
    assert_eq!(texts(&chunks), ["aaa\n\nbbb\n\nccc"]);
    assert_eq!(chunks[0].index, 0);
}

#[test]
fn starts_a_new_chunk_when_the_next_block_would_not_fit() {
    let chunks = pack_blocks(
        &[
            block("aaaa", 1, None, &[]),
            block("bbbb", 1, None, &[]),
            block("cccc", 1, None, &[]),
        ],
        8,
        0,
    );
    assert_eq!(texts(&chunks), ["aaaa\n\nbbbb", "cccc"]);
    assert_eq!(chunks.iter().map(|c| c.index).collect::<Vec<_>>(), [0, 1]);
}

#[test]
fn takes_pages_from_the_min_and_max_of_what_it_packed() {
    let chunks = pack_blocks(
        &[
            block("alpha", 2, None, &[]),
            block("beta", 3, Some(4), &[]),
            block("gamma", 4, None, &[]),
        ],
        64,
        0,
    );
    assert_eq!(chunks.len(), 1);
    assert_eq!(
        (chunks[0].page_start, chunks[0].page_end),
        (Some(2), Some(4))
    );
}

#[test]
fn takes_the_heading_path_of_the_heading_it_holds() {
    let chunks = pack_blocks(
        &[
            block("## Risks", 1, None, &["Report", "Risks"]),
            block("body", 1, None, &["Report", "Risks"]),
        ],
        64,
        0,
    );
    assert_eq!(chunks[0].heading_path, ["Report", "Risks"]);
}

#[test]
fn labels_a_chunk_whose_heading_is_not_its_first_block() {
    let chunks = pack_blocks(
        &[
            block("preamble", 1, None, &[]),
            block("# Intro", 1, None, &["Intro"]),
            block("body", 1, None, &["Intro"]),
        ],
        512,
        0,
    );
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].heading_path, ["Intro"]);
}

#[test]
fn keeps_a_migrated_heading_run_inside_chunk_size() {
    let chunks = pack_blocks(
        &[
            block(&"x".repeat(10), 1, None, &[]),
            block("# Section One", 1, None, &["Section One"]),
            block("## Section Two", 1, None, &["Section One", "Section Two"]),
            block(&"y".repeat(20), 1, None, &["Section One", "Section Two"]),
        ],
        40,
        0,
    );
    assert_eq!(
        texts(&chunks),
        [
            "x".repeat(10),
            "# Section One\n\n## Section Two".to_string(),
            "y".repeat(20)
        ]
    );
    assert!(chunks.iter().all(|c| clen(&c.text) <= 40));
}

#[test]
fn keeps_a_trailing_heading_with_the_text_it_introduces() {
    let chunks = pack_blocks(
        &[
            block("para a", 1, None, &[]),
            block("## Section", 1, None, &["Section"]),
            block("para b", 1, None, &["Section"]),
        ],
        20,
        0,
    );
    assert_eq!(texts(&chunks), ["para a", "## Section\n\npara b"]);
    assert_eq!(chunks[0].heading_path, Vec::<String>::new());
    assert_eq!(chunks[1].heading_path, ["Section"]);
}

#[test]
fn emits_a_heading_only_buffer_rather_than_looping() {
    let chunks = pack_blocks(
        &[
            block("# A", 1, None, &[]),
            block("## B", 1, None, &[]),
            block("body xyz", 1, None, &[]),
        ],
        8,
        0,
    );
    assert_eq!(texts(&chunks), ["# A\n\n## B", "body xyz"]);
}

#[test]
fn carries_overlap_across_a_chunk_boundary() {
    let chunks = pack_blocks(
        &[
            block("alpha beta gamma", 1, None, &[]),
            block("delta epsilon zeta", 2, None, &[]),
        ],
        20,
        8,
    );
    assert_eq!(
        texts(&chunks),
        ["alpha beta gamma", "gamma\n\ndelta epsilon zeta"]
    );
    // The carry duplicates page 1 text, so it must not drag page 1 into chunk 2.
    assert_eq!(
        (chunks[1].page_start, chunks[1].page_end),
        (Some(2), Some(2))
    );
}

#[test]
fn falls_back_to_the_character_chunker_for_an_over_long_block() {
    let long_block = "Alpha beta gamma delta. ".repeat(10);
    let chunks = pack_blocks(&[block(long_block.trim(), 4, Some(5), &["Intro"])], 64, 0);
    let expected = chunk_text(long_block.trim(), 64, 0);
    assert_eq!(texts(&chunks), expected);
    assert!(chunks.len() > 1);
    assert!(chunks
        .iter()
        .all(|c| (c.page_start, c.page_end) == (Some(4), Some(5))));
    assert!(chunks.iter().all(|c| c.heading_path == ["Intro"]));
    assert_eq!(
        chunks.iter().map(|c| c.index).collect::<Vec<_>>(),
        (0..chunks.len() as i64).collect::<Vec<_>>()
    );
}

#[test]
fn flushes_the_buffer_before_an_over_long_block() {
    let long_block = "Alpha beta gamma delta. ".repeat(10);
    let chunks = pack_blocks(
        &[
            block("short intro", 1, None, &[]),
            block(long_block.trim(), 1, None, &[]),
        ],
        64,
        0,
    );
    assert_eq!(chunks[0].text, "short intro");
    assert_eq!(chunks.len(), 1 + chunk_text(long_block.trim(), 64, 0).len());
}

#[test]
fn skips_blank_blocks() {
    let chunks = pack_blocks(
        &[
            block("   ", 1, None, &[]),
            block("real", 1, None, &[]),
            block("\n\n", 1, None, &[]),
        ],
        64,
        0,
    );
    assert_eq!(texts(&chunks), ["real"]);
}

#[test]
fn returns_nothing_for_nothing() {
    assert!(pack_blocks(&[], 64, 0).is_empty());
}

#[test]
fn page_texts_groups_by_page_and_sorts() {
    let pages = page_texts(&[
        block("second", 2, None, &[]),
        block("first", 1, None, &[]),
        block("also first", 1, None, &[]),
    ]);
    let got: Vec<(i64, &str)> = pages
        .iter()
        .map(|p| (p.page_number, p.text.as_str()))
        .collect();
    assert_eq!(got, [(1, "first\n\nalso first"), (2, "second")]);
}

#[test]
fn page_texts_attributes_a_stitched_block_to_where_it_started() {
    let pages = page_texts(&[
        block("table halves", 1, Some(2), &[]),
        block("page two body", 2, None, &[]),
    ]);
    let got: Vec<(i64, &str)> = pages
        .iter()
        .map(|p| (p.page_number, p.text.as_str()))
        .collect();
    assert_eq!(got, [(1, "table halves"), (2, "page two body")]);
}

#[test]
fn page_texts_skips_blank_blocks() {
    let a = page_texts(&[block("  ", 1, None, &[]), block("kept", 2, None, &[])]);
    let b = page_texts(&[block("kept", 2, None, &[])]);
    assert_eq!(a.len(), b.len());
    assert_eq!(a[0].text, b[0].text);
}

// ── chunk_text coverage (documents/chunker) ──────────────────────────────

#[test]
fn chunk_text_empty_and_whitespace() {
    assert!(chunk_text("", 512, 64).is_empty());
    assert!(chunk_text("   \n\n  ", 512, 64).is_empty());
}

#[test]
fn chunk_text_respects_chunk_size_on_paragraph_boundaries() {
    let text = "aaaa\n\nbbbb\n\ncccc";
    let chunks = chunk_text(text, 10, 0);
    assert_eq!(chunks, ["aaaa\n\nbbbb", "cccc"]);
}

#[test]
fn chunk_text_counts_characters_not_bytes() {
    // 'é' is 2 UTF-8 bytes but 1 Python len() char; multi-byte must not split mid-char.
    let text = "é".repeat(20);
    let chunks = chunk_text(&text, 8, 2);
    assert!(chunks.iter().all(|c| clen(c) <= 8));
    assert!(chunks.iter().all(|c| c.chars().all(|ch| ch == 'é')));
}

#[test]
fn tail_overlap_snaps_to_word_boundary() {
    assert_eq!(tail_overlap("alpha beta gamma", 8), "gamma");
    assert_eq!(tail_overlap("", 8), "");
    assert_eq!(tail_overlap("short", 0), "");
}

#[test]
fn chunk_text_long_paragraph_splits_on_sentences() {
    let para = "One two three. Four five six. Seven eight nine.";
    let chunks = chunk_text(para, 20, 4);
    assert!(chunks.len() > 1);
    assert!(chunks.iter().all(|c| clen(c) <= 20 + 4 + 8)); // chunk_size + overlap + join slack
}
