//! Converts a PDF to Markdown from structured page text — port of
//! `AdvancedPdfMarkdownConverter.java`. Orchestration only: each stage of
//! the pipeline lives in its own module under `pdftext`.

use std::collections::HashSet;
use std::rc::Rc;

use lopdf::Document;

use super::column_layout::ColumnLayout;
use super::form_values::FormValues;
use super::glyph_stitcher::GlyphStitcher;
use super::heading;
use super::line::SharedLine;
use super::line_merger::LineMerger;
use super::markdown_blocks::{MarkdownBlock, MarkdownBlocks};
use super::markdown_text;
use super::model::{document_text, PageText};
use super::page_images;
use super::page_rules::PageRules;
use super::page_stitcher::{PageElement, PageStitcher};
use super::paragraph;
use super::shared::Element;
use super::table_block::TableBlock;
use super::table_finder::TableFinder;
use super::table_shape::TableShape;
use crate::pdfdoc;

/// Page-attributed Markdown elements in reading order; `convert` is these
/// joined. The `PdfMarkdownExtractor` seam's richer implementation.
pub fn extract_blocks(doc: &Document) -> Vec<MarkdownBlock> {
    let elements = build_elements(doc);
    let blocks: Vec<MarkdownBlock> = elements
        .iter()
        .map(|e| {
            let markdown = match &e.value {
                PageElement::Table(tb) => tb.render(),
                PageElement::Text(s) => s.clone(),
            };
            MarkdownBlock::new(markdown, e.page_start, e.page_end)
        })
        .collect();
    MarkdownBlocks::with_heading_paths(MarkdownBlocks::normalise_heading_levels(blocks))
}

/// `PdfMarkdownExtractor.convert`: the blocks joined by `SEPARATOR`.
pub fn convert(doc: &Document) -> String {
    MarkdownBlocks::join(&extract_blocks(doc))
}

fn build_elements(doc: &Document) -> Vec<Element<PageElement>> {
    let all_page_text = document_text(doc);
    let median_size = heading::median_font_size(&all_page_text);
    let median_height = heading::median_line_height(&all_page_text);
    let body_font = heading::body_font(&all_page_text);

    let page_count = pdfdoc::page_ids(doc).len();
    // Tables stay structured until after the page loop so one split
    // across a page break can be stitched back together before rendering.
    let mut output: Vec<Element<PageElement>> = Vec::new();
    // Header of a table that ended the previous page, for spotting a
    // continuation; `None` if none.
    let mut prev_page_trailing_table_header: Option<String> = None;

    for page_index in 0..page_count {
        let page = page_lines(doc, &all_page_text, page_index);
        if page.lines.is_empty() {
            page_images::emit(
                doc,
                page_index,
                page_index + 1,
                &mut output,
                PageElement::Text,
            );
            prev_page_trailing_table_header = None;
            continue;
        }

        let mut page_items = build_page_items(
            doc,
            &page,
            page_index,
            median_size,
            median_height,
            &body_font,
            prev_page_trailing_table_header.as_deref(),
        );
        if page_items.is_empty() {
            continue;
        }

        PageStitcher::merge_across_page_boundary(&mut output, &mut page_items);
        let trailing = PageStitcher::trailing_table_header(&page_items);
        output.extend(page_items);
        prev_page_trailing_table_header = trailing;
    }

    // Stitch tables split across page breaks; callers decide how to
    // realise the elements.
    PageStitcher::stitch_tables(output)
}

/// One page's lines plus its layout verdict, which must be taken before
/// text repair: merging reduces the line count the two-column guard scales
/// with.
struct PageLines {
    lines: Vec<SharedLine>,
    gutters: Vec<f32>,
}

impl PageLines {
    #[allow(dead_code)]
    fn two_column_layout(&self) -> bool {
        !self.gutters.is_empty()
    }
}

/// Assembled lines for one page, sorted top-to-bottom (PDF y=0 is the page
/// bottom).
fn page_lines(doc: &Document, all_page_text: &[PageText], page_index: usize) -> PageLines {
    let raw_lines = all_page_text
        .get(page_index)
        .map(|p| p.lines.as_slice())
        .unwrap_or(&[]);
    let stitched = GlyphStitcher::stitch_glyphs(raw_lines);
    let gutters = ColumnLayout::detect_gutters(&stitched);
    let mut lines = LineMerger::merge_line_fragments(stitched, &gutters);
    lines.extend(FormValues::lines(doc, page_index, &lines));
    lines.sort_by(|a, b| b.y.total_cmp(&a.y));
    PageLines { lines, gutters }
}

/// One page's elements: paragraph strings interleaved with [`TableBlock`]s
/// in reading order. `continuation_header` is the previous page's trailing
/// table header, or `None`.
fn build_page_items(
    doc: &Document,
    page: &PageLines,
    page_index: usize,
    median_size: f32,
    median_height: f32,
    body_font: &str,
    continuation_header: Option<&str>,
) -> Vec<Element<PageElement>> {
    let lines = &page.lines;
    // Only genuine two-column prose is split: a table's column gutters
    // must not read as a page gutter, and a table continuing from the
    // previous page is not a new two-column layout.
    let table_continuation = continuation_header.is_some_and(|header| {
        lines
            .iter()
            .any(|l| markdown_text::normalise_space(&l.text) == header)
    });

    // Merging widens lines, so re-check the pre-repair verdict here:
    // ordering by a gutter the finished lines no longer respect is worse
    // than not splitting at all.
    let gutters = if table_continuation {
        Vec::new()
    } else {
        page.gutters.clone()
    };
    let two_column = !gutters.is_empty();
    let respected = two_column && ColumnLayout::gutter_respected(lines, &gutters);

    // Two detectors: ruling lines give exact boundaries and see
    // single-word cells; the word grid covers what the rules do not, i.e.
    // borderless and whitespace-aligned tables.
    let mut table_row_texts: HashSet<String> = HashSet::new();
    let rules = read_rules(doc, page_index);
    let mut blocks = TableFinder::find(lines, &rules, page_index + 1);
    tracing::debug!(
        "p{} lines={} hRules={} vRules={} twoColumn={} blocks={}",
        page_index,
        lines.len(),
        rules.horizontal.len(),
        rules.vertical.len(),
        !gutters.is_empty(),
        blocks.len()
    );
    for b in &blocks {
        let cells = b.cells();
        tracing::debug!(
            "block top={} bot={} ruled={} src={:?} rows={} cells={}x{} grid={} spans={} owns={}",
            b.top,
            b.bottom,
            b.ruled,
            b.row_source,
            b.rows.len(),
            cells.len(),
            cells.first().map(|r| r.len()).unwrap_or(0),
            TableShape::looks_like_grid(b),
            TableShape::spans_page(b, lines),
            TableShape::owns_its_band(b, lines)
        );
        for row in &b.rows {
            tracing::debug!("  row: {}", PageStitcher::row_text(row));
        }
    }
    if two_column {
        // On a multi-column page only a full-width block is a table;
        // anything narrower sits inside a column. A ruled block owning its
        // own band has no column layout to sit in.
        blocks.retain(|b| {
            (b.ruled || TableShape::looks_like_grid(b))
                && (TableShape::spans_page(b, lines)
                    || (b.ruled && TableShape::owns_its_band(b, lines)))
        });
    }
    let mut table_lines: HashSet<usize> = HashSet::new();
    for b in &blocks {
        for row in &b.rows {
            for l in row {
                table_lines.insert(Rc::as_ptr(l) as usize);
                table_row_texts.insert(markdown_text::strip(&l.text).to_string());
            }
        }
    }

    let mut page_items: Vec<Element<PageElement>> = Vec::new();
    let segments = segments_around(lines, &blocks, &table_lines);
    if two_column {
        // A full-width table interrupts both columns, so splitting at its
        // own vertical band keeps the prose above and below it in column
        // order.
        for (s, segment) in segments.iter().enumerate() {
            let groups = if respected {
                ColumnLayout::order_by_band(segment, &gutters)
            } else {
                ColumnLayout::legacy_split(segment)
            };
            for col in groups {
                let mut paras = Vec::new();
                paragraph::assemble_paragraphs(
                    &col,
                    median_size,
                    median_height,
                    body_font,
                    &mut paras,
                    &table_row_texts,
                );
                for p in paras {
                    page_items.push(Element::new(PageElement::Text(p), page_index + 1));
                }
            }
            if s < blocks.len() {
                page_items.push(Element::new(
                    PageElement::Table(blocks[s].clone()),
                    page_index + 1,
                ));
            }
        }
    } else {
        // Interleave tables with text by vertical position: each block
        // gets a slot, and non-table lines fall into the slot for their y,
        // keeping tables separate.
        for (s, segment) in segments.iter().enumerate() {
            let mut paras = Vec::new();
            paragraph::assemble_paragraphs(
                segment,
                median_size,
                median_height,
                body_font,
                &mut paras,
                &table_row_texts,
            );
            for p in paras {
                page_items.push(Element::new(PageElement::Text(p), page_index + 1));
            }
            if s < blocks.len() {
                page_items.push(Element::new(
                    PageElement::Table(blocks[s].clone()),
                    page_index + 1,
                ));
            }
        }
    }

    page_images::emit(
        doc,
        page_index,
        page_index + 1,
        &mut page_items,
        PageElement::Text,
    );
    page_items
}

/// Splits a page's non-table lines into the bands between its table
/// blocks, band `s` holding the lines above block `s`. Blocks must be in
/// top-to-bottom order.
fn segments_around(
    lines: &[SharedLine],
    blocks: &[TableBlock],
    table_lines: &HashSet<usize>,
) -> Vec<Vec<SharedLine>> {
    let mut segments: Vec<Vec<SharedLine>> = (0..=blocks.len()).map(|_| Vec::new()).collect();
    for l in lines {
        if table_lines.contains(&(Rc::as_ptr(l) as usize)) {
            continue;
        }
        let mut slot = 0;
        for b in blocks {
            if b.bottom > l.y {
                slot += 1;
            }
        }
        segments[slot].push(l.clone());
    }
    segments
}

/// Ruling lines of one page, or the empty ruleset if the page cannot be
/// opened.
fn read_rules(doc: &Document, page_index: usize) -> PageRules {
    pdfdoc::page_ids(doc)
        .get(page_index)
        .map(|&id| PageRules::of_page(doc, id))
        .unwrap_or_default()
}
