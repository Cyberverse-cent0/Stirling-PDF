//! Document classifier agent. Port of `stirling.agents.document_classifier`:
//! one fast-model structured pass over a bounded page window, then the answer
//! is coerced onto the caller-supplied label vocabulary.

use std::collections::HashMap;

use serde_json::Value;

use crate::contracts::document_classifier::{
    ClassifyDocumentRequest, DocumentClassificationResponse, LabelOption,
};
use crate::contracts::documents::PageText;
use crate::error::{ApiError, ApiResult};
use crate::llm::client::LlmClient;

/// A document carries at most this many labels; anything past the cap is
/// dropped in the model's order.
const MAX_ASSIGNED_LABELS: usize = 5;
/// Pages read from each end of the document — enough to identify the type at
/// flat cost regardless of length.
const WINDOW_PAGES: usize = 2;
/// pydantic-ai `output_retries`: native structured output gets 1 retry,
/// tool-delivered output (ollama/custom providers) gets 6.
const NATIVE_RETRIES: usize = 1;
const TOOL_RETRIES: usize = 6;

const SYSTEM_PROMPT: &str =
    "You identify what a document is by assigning labels, choosing only from a \
fixed list of allowed labels you are given.\n\n\
Rules:\n\
- Pick up to 5 labels that describe this document's type.\n\
- Only use labels from the allowed list, spelled exactly as listed.\n\
- Return an empty list if none fit.\n\
- Judge from the document's content and structure, not from keywords alone. \
The document may be in any language.\n\
- You are shown only the first and last pages; that is enough to identify the type.";

const OUTPUT_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "labels": {
      "type": "array",
      "items": {"type": "string"},
      "description": "Labels from the allowed list that describe this document's type."
    }
  },
  "required": ["labels"],
  "additionalProperties": false
}"#;

fn uses_tool_output(chat_provider: &str) -> bool {
    matches!(chat_provider, "ollama" | "custom")
}

fn render_labels(labels: &[LabelOption]) -> String {
    let names: Vec<&str> = labels.iter().map(|l| l.name.as_str()).collect();
    format!(
        "Allowed labels: {}",
        if names.is_empty() {
            "(none)".to_string()
        } else {
            names.join(", ")
        }
    )
}

/// First and last `window` pages, never overlapping; short documents are
/// returned whole.
pub fn select_window(pages: &[PageText], window: usize) -> Vec<PageText> {
    if window == 0 || pages.len() <= window * 2 {
        return pages.to_vec();
    }
    let mut out = pages[..window].to_vec();
    out.extend_from_slice(&pages[pages.len() - window..]);
    out
}

fn format_window(pages: &[PageText]) -> String {
    if pages.is_empty() {
        return "(no extractable text)".to_string();
    }
    pages
        .iter()
        .map(|p| format!("[Page {}]\n{}", p.page_number, p.text))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Match model-returned names case-insensitively onto the allowed vocabulary,
/// returning label ids: off-list names dropped, duplicates collapse to the
/// first occurrence, capped at MAX_ASSIGNED_LABELS in model order.
fn validate_labels(raw: &[String], allowed: &[LabelOption]) -> DocumentClassificationResponse {
    let id_by_lower_name: HashMap<String, &str> = allowed
        .iter()
        .map(|l| (l.name.to_lowercase(), l.id.as_str()))
        .collect();

    let mut kept: Vec<String> = Vec::new();
    for name in raw {
        if let Some(id) = id_by_lower_name.get(name.trim().to_lowercase().as_str()) {
            if !kept.iter().any(|k| k == id) {
                kept.push((*id).to_string());
            }
            if kept.len() == MAX_ASSIGNED_LABELS {
                break;
            }
        }
    }
    DocumentClassificationResponse { labels: kept }
}

/// Tolerant parse of the model's `labels` field: missing → empty list
/// (pydantic `default_factory`), wrong types → retryable error.
fn parse_labels(output: &Value) -> Result<Vec<String>, String> {
    match output.get("labels") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| "labels contains a non-string entry".to_string())
            })
            .collect(),
        Some(_) => Err("labels is not an array".to_string()),
    }
}

fn build_prompt(request: &ClassifyDocumentRequest, window: &[PageText]) -> String {
    format!(
        "{}\n\nDocument file name: {}\nDocument content (first and last pages):\n{}",
        render_labels(&request.labels),
        request.file_name,
        format_window(window)
    )
}

/// Classify `request` with the fast model. The request is self-contained —
/// no document storage is touched.
pub async fn classify(
    client: &LlmClient,
    chat_provider: &str,
    request: &ClassifyDocumentRequest,
) -> ApiResult<DocumentClassificationResponse> {
    let window = select_window(&request.pages, WINDOW_PAGES);
    let prompt = build_prompt(request, &window);
    tracing::debug!(prompt = %prompt, "[classify] prompt");

    let schema: Value = serde_json::from_str(OUTPUT_SCHEMA)
        .map_err(|e| ApiError::internal(format!("classifier schema: {e}")))?;
    let retries = if uses_tool_output(chat_provider) {
        TOOL_RETRIES
    } else {
        NATIVE_RETRIES
    };
    let output = client
        .complete_json(SYSTEM_PROMPT, &prompt, &schema, retries)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "[classify] model call failed");
            ApiError::internal(format!("classification failed: {e}"))
        })?;

    // Schema-shape validation the JSON Schema cannot express is retried by
    // pydantic-ai in Python; here a malformed `labels` field is a hard error.
    let labels = parse_labels(&output)
        .map_err(|e| ApiError::internal(format!("classification output invalid: {e}")))?;
    Ok(validate_labels(&labels, &request.labels))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn page(n: i64, text: &str) -> PageText {
        PageText {
            page_number: n,
            text: text.to_string(),
        }
    }

    fn label(id: &str, name: &str) -> LabelOption {
        LabelOption {
            id: id.to_string(),
            name: name.to_string(),
        }
    }

    #[test]
    fn select_window_returns_short_docs_whole() {
        let pages = vec![page(1, "a"), page(2, "b"), page(3, "c")];
        assert_eq!(select_window(&pages, WINDOW_PAGES).len(), 3);
    }

    #[test]
    fn select_window_takes_first_and_last() {
        let pages: Vec<PageText> = (1..=10).map(|n| page(n, "x")).collect();
        let window = select_window(&pages, WINDOW_PAGES);
        let nums: Vec<i64> = window.iter().map(|p| p.page_number).collect();
        assert_eq!(nums, vec![1, 2, 9, 10]);
    }

    #[test]
    fn select_window_zero_returns_all() {
        let pages = vec![page(1, "a"), page(2, "b")];
        assert_eq!(select_window(&pages, 0).len(), 2);
    }

    #[test]
    fn render_labels_lists_names() {
        let labels = vec![label("l1", "Invoice"), label("l2", "Report")];
        assert_eq!(render_labels(&labels), "Allowed labels: Invoice, Report");
        assert_eq!(render_labels(&[]), "Allowed labels: (none)");
    }

    #[test]
    fn format_window_marks_pages() {
        assert_eq!(format_window(&[]), "(no extractable text)");
        let out = format_window(&[page(3, "hello")]);
        assert_eq!(out, "[Page 3]\nhello");
    }

    #[test]
    fn validate_labels_matches_case_insensitive_and_dedupes() {
        let allowed = vec![label("inv", "Invoice"), label("rep", "Report")];
        let raw = vec![
            "  invoice ".to_string(),
            "INVOICE".to_string(),
            "Report".to_string(),
            "Unknown".to_string(),
        ];
        let out = validate_labels(&raw, &allowed);
        assert_eq!(out.labels, vec!["inv", "rep"]);
    }

    #[test]
    fn validate_labels_caps_at_max() {
        let allowed: Vec<LabelOption> = (0..8)
            .map(|i| label(&format!("l{i}"), &format!("Label {i}")))
            .collect();
        let raw: Vec<String> = (0..8).map(|i| format!("label {i}")).collect();
        assert_eq!(
            validate_labels(&raw, &allowed).labels.len(),
            MAX_ASSIGNED_LABELS
        );
    }

    #[test]
    fn parse_labels_tolerates_missing() {
        assert_eq!(parse_labels(&json!({})).unwrap(), Vec::<String>::new());
        assert_eq!(
            parse_labels(&json!({"labels": ["a", "b"]})).unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
        assert!(parse_labels(&json!({"labels": "nope"})).is_err());
        assert!(parse_labels(&json!({"labels": [1]})).is_err());
    }

    #[test]
    fn build_prompt_contains_vocabulary_filename_and_window() {
        let request = ClassifyDocumentRequest {
            file_name: "contract.pdf".to_string(),
            pages: vec![page(1, "body")],
            labels: vec![label("inv", "Invoice")],
        };
        let prompt = build_prompt(&request, &select_window(&request.pages, WINDOW_PAGES));
        assert!(prompt.contains("Allowed labels: Invoice"));
        assert!(prompt.contains("Document file name: contract.pdf"));
        assert!(prompt.contains("[Page 1]\nbody"));
    }
}
