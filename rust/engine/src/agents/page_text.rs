//! Extracted page-text helpers shared by the pdf-edit and pdf-question
//! agents. Port of `stirling.agents._page_text` (the
//! `get_extracted_text_artifact` half belongs to the orchestrator port).

use crate::contracts::common::ExtractedFileText;

pub fn has_page_text(page_text: &[ExtractedFileText]) -> bool {
    page_text
        .iter()
        .flat_map(|f| f.pages.iter())
        .any(|p| !p.text.trim().is_empty())
}

pub fn format_page_text(page_text: &[ExtractedFileText], empty: &str) -> String {
    if !has_page_text(page_text) {
        return empty.to_string();
    }
    page_text
        .iter()
        .flat_map(|f| {
            f.pages.iter().map(move |p| {
                format!(
                    "[File: {}, Page {}]\n{}",
                    f.file_name,
                    // Python `page_number or '?'`: 0 is falsy there too.
                    p.page_number
                        .filter(|n| *n != 0)
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "?".to_string()),
                    p.text
                )
            })
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}
