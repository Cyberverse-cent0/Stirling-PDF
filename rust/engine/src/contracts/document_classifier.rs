//! Document classifier contracts. Port of `stirling.contracts.document_classifier`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::contracts::documents::PageText;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LabelOption {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClassifyDocumentRequest {
    pub file_name: String,
    #[serde(default)]
    pub pages: Vec<PageText>,
    pub labels: Vec<LabelOption>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DocumentClassificationResponse {
    #[serde(default)]
    pub labels: Vec<String>,
}
