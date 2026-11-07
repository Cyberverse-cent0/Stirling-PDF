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
    #[serde(alias = "file_name")]
    pub file_name: String,
    #[serde(default)]
    pub pages: Vec<PageText>,
    pub labels: Vec<LabelOption>,
}

impl LabelOption {
    /// `id`/`name` are `Field(min_length=1)` in the pydantic contract.
    pub fn validate(&self) -> Result<(), String> {
        if self.id.is_empty() {
            return Err("label id must not be empty".to_string());
        }
        if self.name.is_empty() {
            return Err("label name must not be empty".to_string());
        }
        Ok(())
    }
}

impl ClassifyDocumentRequest {
    /// `Field(min_length=1)` checks from the pydantic contract.
    pub fn validate(&self) -> Result<(), String> {
        if self.file_name.is_empty() {
            return Err("fileName must not be empty".to_string());
        }
        if self.labels.is_empty() {
            return Err("labels must not be empty".to_string());
        }
        for label in &self.labels {
            label.validate()?;
        }
        for page in &self.pages {
            page.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DocumentClassificationResponse {
    #[serde(default)]
    pub labels: Vec<String>,
}
