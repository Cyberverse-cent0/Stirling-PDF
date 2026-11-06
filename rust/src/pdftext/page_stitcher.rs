//! Joins what a page break split: a sentence running into the next page, and
//! a table whose rows continue on it — port of `PageStitcher.java`.

use crate::pdftext::column_accumulator::ColumnAccumulator;
use crate::pdftext::column_ranges;
use crate::pdftext::line::SharedLine;
use crate::pdftext::markdown_blocks;
use crate::pdftext::markdown_text;
use crate::pdftext::shared::Element;
use crate::pdftext::table_block::TableBlock;

/// Payload carried by a page element before rendering: markdown text or a
/// structured table.
#[derive(Debug, Clone)]
pub enum PageElement {
    Text(String),
    Table(TableBlock),
}

/// Merges a lowercase sentence continuation from the next page into the
/// trailing paragraph, but never into or out of a table.
pub fn merge_across_page_boundary(
    output: &mut Vec<Element<PageElement>>,
    page_items: &mut Vec<Element<PageElement>>,
) {
    if output.is_empty() || page_items.is_empty() {
        return;
    }
    let (last, first) = match (
        output.last().map(|element| &element.value),
        page_items.first().map(|element| &element.value),
    ) {
        (Some(PageElement::Text(last)), Some(PageElement::Text(first))) => (last, first),
        _ => return,
    };
    let first_is_lower = first
        .encode_utf16()
        .next()
        .and_then(|unit| char::from_u32(u32::from(unit)))
        .is_some_and(|c| c.is_lowercase());
    if first.is_empty() || !first_is_lower || markdown_text::ends_with_sentence_punctuation(last) {
        return;
    }

    let head_page_start = output.last().map(|element| element.page_start);
    let tail_page_end = page_items.first().map(|element| element.page_end);
    let (Some(head_page_start), Some(tail_page_end)) = (head_page_start, tail_page_end) else {
        return;
    };
    let merged = format!("{} {}", last, first);
    output.pop();
    output.push(Element {
        value: PageElement::Text(merged),
        page_start: head_page_start,
        page_end: tail_page_end,
    });
    page_items.remove(0);
}

/// Joins tables split across a page break when their column layouts match,
/// dropping a repeated header row.
pub fn stitch_tables(elements: Vec<Element<PageElement>>) -> Vec<Element<PageElement>> {
    let mut out: Vec<Element<PageElement>> = Vec::new();
    let mut acc = None;
    for element in elements {
        let table_pair = matches!(&element.value, PageElement::Table(_))
            && matches!(
                out.last().map(|last| &last.value),
                Some(PageElement::Table(_))
            );
        if !table_pair {
            out.push(element);
            acc = None;
            continue;
        }

        let prev = match out.last().map(|last| &last.value) {
            Some(PageElement::Table(table)) => table,
            _ => unreachable!(),
        };
        let next = match &element.value {
            PageElement::Table(table) => table,
            _ => unreachable!(),
        };
        if acc.is_none() {
            acc = Some(ColumnAccumulator::of(&prev.rows));
        }
        let accumulated = acc
            .as_ref()
            .map(ColumnAccumulator::columns)
            .unwrap_or_default();
        let next_columns = column_ranges::find(&flatten(&next.rows));
        if !columns_match(&accumulated, &next_columns) {
            out.push(element);
            acc = None;
            continue;
        }

        let prev_element = out.pop().expect("table pair has a previous element");
        let prev_page_start = prev_element.page_start;
        let prev_page_end = prev_element.page_end;
        let next_page_end = element.page_end;
        let PageElement::Table(prev) = prev_element.value else {
            unreachable!()
        };
        let PageElement::Table(next) = element.value else {
            unreachable!()
        };
        let TableBlock {
            rows: mut merged_rows,
            top,
            cols,
            ruled,
            row_source,
            page,
            ..
        } = prev;
        let TableBlock {
            rows: mut tail,
            bottom,
            ..
        } = next;

        if let (Some(first_tail), Some(first_prev)) = (tail.first(), merged_rows.first()) {
            if row_text(first_tail) == row_text(first_prev) {
                tail.remove(0);
            }
        }
        if let Some(accumulator) = acc.as_mut() {
            for row in &tail {
                for line in row {
                    accumulator.add_line(line);
                }
            }
        }
        merged_rows.extend(tail);

        out.push(Element {
            value: PageElement::Table(TableBlock {
                rows: merged_rows,
                top,
                bottom,
                cols,
                ruled,
                row_source,
                page,
            }),
            page_start: prev_page_start,
            page_end: prev_page_end.max(next_page_end),
        });
    }
    out
}

fn flatten(rows: &[Vec<SharedLine>]) -> Vec<SharedLine> {
    rows.iter().flatten().cloned().collect()
}

/// Header text of a table at the very bottom of a page, or `None`. Trailing
/// image placeholders are skipped; any other text means it is not a
/// continuation.
pub fn trailing_table_header(page_items: &[Element<PageElement>]) -> Option<String> {
    for element in page_items.iter().rev() {
        match &element.value {
            PageElement::Text(text) if markdown_blocks::is_image_placeholder(text) => continue,
            PageElement::Table(table) if !table.rows.is_empty() => {
                return Some(row_text(&table.rows[0]));
            }
            _ => return None,
        }
    }
    None
}

pub fn row_text(row: &[SharedLine]) -> String {
    let mut ordered = row.to_vec();
    ordered.sort_by(|a, b| f32::total_cmp(&b.y, &a.y));
    let mut text = String::new();
    for line in ordered {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&line.text);
    }
    markdown_text::normalise_space(&text)
}

/// True when two table blocks have the same number of columns at
/// near-identical x-centres.
fn columns_match(a: &[[f32; 2]], b: &[[f32; 2]]) -> bool {
    if a.len() < 2 || a.len() != b.len() {
        return false;
    }
    for (a, b) in a.iter().zip(b) {
        let centre_a = (a[0] + a[1]) / 2.0;
        let centre_b = (b[0] + b[1]) / 2.0;
        if (centre_a - centre_b).abs() > 15.0 {
            return false;
        }
    }
    true
}

/// Java-style facade over this module's free functions; the ported pipeline
/// calls the Java `PageStitcher` helper as a static class.
#[allow(dead_code)]
pub struct PageStitcher;

#[allow(dead_code)]
impl PageStitcher {
    pub fn merge_across_page_boundary(
        output: &mut Vec<Element<PageElement>>,
        page_items: &mut Vec<Element<PageElement>>,
    ) {
        merge_across_page_boundary(output, page_items)
    }
    pub fn stitch_tables(elements: Vec<Element<PageElement>>) -> Vec<Element<PageElement>> {
        stitch_tables(elements)
    }
    pub fn trailing_table_header(page_items: &[Element<PageElement>]) -> Option<String> {
        trailing_table_header(page_items)
    }
    pub fn row_text(row: &[SharedLine]) -> String {
        row_text(row)
    }
}
