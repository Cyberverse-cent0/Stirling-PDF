//! Contract serde tests: camelCase field names, snake_case enum tags,
//! internally-tagged unions, and `deny_unknown_fields` — the wire contract
//! the Python engine and Java proxy both depend on.

use serde_json::json;
use stirling_engine::contracts::common::{
    format_conversation_history, format_file_names, AiFile, ConversationMessage,
    NeedContentFileRequest, NeedContentResponse, OrchestratorResponse, PdfContentType,
    SupportedCapability, WorkflowOutcome,
};

fn need_content() -> NeedContentResponse {
    NeedContentResponse {
        files: vec![NeedContentFileRequest {
            file: AiFile {
                id: "file-1".to_string(),
                name: "report.pdf".to_string(),
            },
            page_numbers: vec![1, 3],
            content_types: vec![PdfContentType::PageLayout, PdfContentType::PageText],
        }],
        max_pages: 10,
        max_characters: 50_000,
        resume_with: SupportedCapability::PdfQuestion,
        reason: "need page text".to_string(),
    }
}

#[test]
fn conversation_message_uses_camel_case_and_rejects_unknown_fields() {
    let msg = ConversationMessage {
        role: "user".to_string(),
        content: "hi".to_string(),
    };
    let value = serde_json::to_value(&msg).unwrap();
    assert_eq!(value, json!({"role": "user", "content": "hi"}));
    assert!(serde_json::from_value::<ConversationMessage>(
        json!({"role": "user", "content": "hi", "extra": 1})
    )
    .is_err());
}

#[test]
fn pdf_content_type_and_outcome_use_snake_case() {
    assert_eq!(
        serde_json::to_value(PdfContentType::PageLayout).unwrap(),
        json!("page_layout")
    );
    assert_eq!(
        serde_json::to_value(WorkflowOutcome::NeedContent).unwrap(),
        json!("need_content")
    );
    assert_eq!(
        serde_json::from_value::<PdfContentType>(json!("full_text")).unwrap(),
        PdfContentType::FullText
    );
    assert!(serde_json::from_value::<WorkflowOutcome>(json!("bogus")).is_err());
}

#[test]
fn need_content_response_round_trips_with_camel_case_fields() {
    let response = need_content();
    let value = serde_json::to_value(&response).unwrap();
    assert_eq!(value["maxPages"], json!(10));
    assert_eq!(value["maxCharacters"], json!(50_000));
    assert_eq!(value["files"][0]["pageNumbers"], json!([1, 3]));
    assert_eq!(value["files"][0]["contentTypes"][0], json!("page_layout"));
    assert_eq!(value["resumeWith"], json!("pdf_question"));

    let back: NeedContentResponse = serde_json::from_value(value).unwrap();
    assert_eq!(back.files.len(), 1);
    assert_eq!(back.files[0].file.name, "report.pdf");
}

#[test]
fn orchestrator_response_is_internally_tagged_on_outcome() {
    let response = OrchestratorResponse::NeedContent(need_content());
    let value = serde_json::to_value(&response).unwrap();
    assert_eq!(value["outcome"], json!("need_content"));
    assert_eq!(value["reason"], json!("need page text"));

    let back: OrchestratorResponse = serde_json::from_value(value).unwrap();
    assert!(matches!(back, OrchestratorResponse::NeedContent(_)));
}

#[test]
fn orchestrator_response_rejects_unknown_outcome() {
    assert!(
        serde_json::from_value::<OrchestratorResponse>(json!({"outcome": "teleport", "x": 1}))
            .is_err()
    );
}

#[test]
fn unsupported_capability_variant_shape() {
    let response = OrchestratorResponse::UnsupportedCapability {
        capability: "ocr".to_string(),
        message: "not supported".to_string(),
    };
    let value = serde_json::to_value(&response).unwrap();
    assert_eq!(
        value,
        json!({
            "outcome": "unsupported_capability",
            "capability": "ocr",
            "message": "not supported",
        })
    );
    let back: OrchestratorResponse = serde_json::from_value(value).unwrap();
    assert!(matches!(
        back,
        OrchestratorResponse::UnsupportedCapability { .. }
    ));
}

#[test]
fn conversation_and_file_formatters_match_python() {
    let history = vec![
        ConversationMessage {
            role: "user".to_string(),
            content: "hello".to_string(),
        },
        ConversationMessage {
            role: "assistant".to_string(),
            content: "hi".to_string(),
        },
    ];
    assert_eq!(
        format_conversation_history(&history),
        "- user: hello\n- assistant: hi"
    );
    assert_eq!(format_conversation_history(&[]), "None");

    let files = vec![AiFile {
        id: "f1".to_string(),
        name: "a.pdf".to_string(),
    }];
    assert_eq!(format_file_names(&files), "a.pdf");
    assert_eq!(format_file_names(&[]), "No file names were provided.");
}
