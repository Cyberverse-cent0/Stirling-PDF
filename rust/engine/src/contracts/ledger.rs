//! Ledger auditor wire contracts (Java ⇄ engine negotiation protocol).
//! Port of `stirling.contracts.ledger`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FolioType {
    Text,
    Image,
    Mixed,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FolioManifest {
    #[serde(alias = "session_id")]
    pub session_id: String,
    #[serde(alias = "page_count")]
    pub page_count: i64,
    #[serde(alias = "folio_types")]
    pub folio_types: Vec<FolioType>,
    #[serde(default = "default_round")]
    pub round: i64,
}

fn default_round() -> i64 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Requisition {
    #[serde(rename = "type")]
    pub kind: RequisitionKind,
    #[serde(default)]
    #[serde(alias = "need_text")]
    pub need_text: Vec<i64>,
    #[serde(default)]
    #[serde(alias = "need_tables")]
    pub need_tables: Vec<i64>,
    #[serde(default)]
    #[serde(alias = "need_ocr")]
    pub need_ocr: Vec<i64>,
    pub rationale: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
pub enum RequisitionKind {
    #[serde(rename = "requisition")]
    #[default]
    Requisition,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Folio {
    pub page: i64,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub tables: Option<Vec<String>>,
    #[serde(default)]
    #[serde(alias = "ocr_text")]
    pub ocr_text: Option<String>,
    #[serde(default)]
    #[serde(alias = "ocr_confidence")]
    pub ocr_confidence: Option<f64>,
}

impl Folio {
    /// Best available text — OCR wins over digital when present.
    pub fn readable_text(&self) -> &str {
        self.ocr_text
            .as_deref()
            .or(self.text.as_deref())
            .unwrap_or("")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Evidence {
    #[serde(alias = "session_id")]
    pub session_id: String,
    pub folios: Vec<Folio>,
    pub round: i64,
    #[serde(default)]
    #[serde(alias = "final_round")]
    pub final_round: bool,
    #[serde(default)]
    #[serde(alias = "unauditable_pages")]
    pub unauditable_pages: Vec<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DiscrepancyKind {
    Tally,
    Arithmetic,
    Consistency,
    Statement,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Discrepancy {
    pub page: i64,
    pub kind: DiscrepancyKind,
    pub severity: Severity,
    pub description: String,
    pub stated: String,
    pub expected: String,
    #[serde(default)]
    pub context: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Verdict {
    #[serde(rename = "type")]
    pub kind: VerdictKind,
    #[serde(alias = "session_id")]
    pub session_id: String,
    #[serde(default)]
    pub discrepancies: Vec<Discrepancy>,
    #[serde(alias = "pages_examined")]
    pub pages_examined: Vec<i64>,
    #[serde(alias = "rounds_taken")]
    pub rounds_taken: i64,
    pub summary: String,
    pub clean: bool,
    #[serde(default)]
    #[serde(alias = "unauditable_pages")]
    pub unauditable_pages: Vec<i64>,
}

impl Verdict {
    pub fn error_count(&self) -> usize {
        self.discrepancies
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .count()
    }

    pub fn warning_count(&self) -> usize {
        self.discrepancies
            .iter()
            .filter(|d| d.severity == Severity::Warning)
            .count()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
pub enum VerdictKind {
    #[serde(rename = "verdict")]
    #[default]
    Verdict,
}
