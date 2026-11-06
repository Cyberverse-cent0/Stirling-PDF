//! PDF create contracts + planner models. Port of `stirling.contracts.pdf_create`.

use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::contracts::common::ConversationMessage;
use crate::contracts::pdf_edit::{EditCannotDoPayload, EditPlanPayload};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SectionType {
    Text,
    KeyValue,
    LineItems,
    BulletList,
    Signature,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DocumentSection {
    #[serde(rename = "text")]
    Text {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        heading: Option<String>,
        body: String,
    },
    #[serde(rename = "key_value")]
    KeyValue {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        heading: Option<String>,
        pairs: Vec<(String, String)>,
    },
    #[serde(rename = "line_items")]
    LineItems {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        heading: Option<String>,
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total_row: Option<Vec<String>>,
    },
    #[serde(rename = "bullet_list")]
    BulletList {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        heading: Option<String>,
        items: Vec<String>,
    },
    #[serde(rename = "signature")]
    Signature {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        heading: Option<String>,
        signatories: Vec<String>,
    },
}

/// `#RRGGBB` only; anything else deserializes to `None` (Python field_validator).
fn safe_color<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    Ok(value.filter(|v| {
        v.len() == 7 && v.starts_with('#') && v[1..].chars().all(|c| c.is_ascii_hexdigit())
    }))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DocumentStyle {
    #[serde(default, deserialize_with = "safe_color")]
    pub primary_color: Option<String>,
    #[serde(default, deserialize_with = "safe_color")]
    pub background_color: Option<String>,
    #[serde(default, deserialize_with = "safe_color")]
    pub body_text_color: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GeneratedDocument {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_number: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<DocumentStyle>,
    pub sections: Vec<DocumentSection>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SectionDepth {
    Brief,
    Standard,
    Detailed,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlannedSection {
    pub heading: String,
    #[serde(rename = "type")]
    pub section_type: SectionType,
    pub depth: SectionDepth,
    pub key_points: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DocumentMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cannot_do_reason: Option<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_number: Option<String>,
    #[serde(default)]
    pub tone_brief: String,
    #[serde(default)]
    pub shared_terms: HashMap<String, String>,
    #[serde(default)]
    pub document_context: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style_primary_color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style_background_color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style_body_text_color: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DocumentSections {
    #[serde(default)]
    pub sections: Vec<PlannedSection>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DocumentPlan {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cannot_do_reason: Option<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_number: Option<String>,
    #[serde(default)]
    pub tone_brief: String,
    #[serde(default)]
    pub shared_terms: HashMap<String, String>,
    #[serde(default)]
    pub document_context: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<DocumentStyle>,
    #[serde(default)]
    pub sections: Vec<PlannedSection>,
}

impl DocumentPlan {
    /// `DocumentPlan.assemble(meta, sections)` — rebuilds `DocumentStyle` from
    /// the flat meta colour strings, dropping unsafe values.
    pub fn assemble(meta: DocumentMeta, sections: DocumentSections) -> Self {
        let style = {
            let primary = meta.style_primary_color.as_deref().and_then(safe_hex);
            let background = meta.style_background_color.as_deref().and_then(safe_hex);
            let body = meta.style_body_text_color.as_deref().and_then(safe_hex);
            if primary.is_some() || background.is_some() || body.is_some() {
                Some(DocumentStyle {
                    primary_color: primary,
                    background_color: background,
                    body_text_color: body,
                })
            } else {
                None
            }
        };
        Self {
            cannot_do_reason: meta.cannot_do_reason,
            title: meta.title,
            subtitle: meta.subtitle,
            reference_number: meta.reference_number,
            tone_brief: meta.tone_brief,
            shared_terms: meta.shared_terms,
            document_context: meta.document_context,
            style,
            sections: sections.sections,
        }
    }
}

fn safe_hex(v: &str) -> Option<String> {
    (v.len() == 7 && v.starts_with('#') && v[1..].chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| v.to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WrittenSections {
    pub sections: Vec<DocumentSection>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PdfCreateRequest {
    pub user_message: String,
    #[serde(default)]
    pub conversation_history: Vec<ConversationMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PdfCreateSuccessPayload {
    pub document: GeneratedDocument,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PdfCreateResponse {
    #[serde(rename = "document_created")]
    DocumentCreated(PdfCreateSuccessPayload),
    #[serde(rename = "cannot_do")]
    CannotDo(EditCannotDoPayload),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PdfCreateOrchestrateResponse {
    #[serde(rename = "plan")]
    Plan(EditPlanPayload),
    #[serde(rename = "cannot_do")]
    CannotDo(EditCannotDoPayload),
}
