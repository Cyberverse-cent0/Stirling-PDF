//! Document classifier contracts. Port of `stirling.contracts.document_classifier`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::contracts::documents::PageText;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LabelOption {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ClassifyDocumentRequest {
    #[serde(alias = "file_name")]
    pub file_name: String,
    #[serde(default)]
    pub pages: Vec<PageText>,
    pub labels: Vec<LabelOption>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DocumentClassificationResponse {
    #[serde(default)]
    pub labels: Vec<String>,
}
