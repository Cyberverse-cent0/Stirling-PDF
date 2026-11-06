//! Contradiction-agent contracts. Python-only: no Java counterpart.
//! Port of `stirling.contracts.contradiction`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClaimPolarity {
    Assert,
    Deny,
    Recommend,
    Reject,
    Neutral,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContradictionSeverity {
    Error,
    Warning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum AnchorQuality {
    #[default]
    Verbatim,
    Paraphrased,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Claim {
    pub page: i64,
    pub subject: String,
    pub polarity: ClaimPolarity,
    pub text: String,
    pub quote: String,
    #[serde(default)]
    pub anchor_quality: AnchorQuality,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Contradiction {
    pub subject: String,
    pub claim1: Claim,
    pub claim2: Claim,
    pub explanation: String,
    pub severity: ContradictionSeverity,
}

impl Contradiction {
    pub fn page1(&self) -> i64 {
        self.claim1.page.min(self.claim2.page)
    }

    pub fn page2(&self) -> i64 {
        self.claim1.page.max(self.claim2.page)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContradictionReport {
    #[serde(default)]
    pub contradictions: Vec<Contradiction>,
    #[serde(default)]
    pub pages_examined: Vec<i64>,
    pub clean: bool,
    pub summary: String,
}

impl ContradictionReport {
    pub fn error_count(&self) -> usize {
        self.contradictions
            .iter()
            .filter(|c| c.severity == ContradictionSeverity::Error)
            .count()
    }

    pub fn warning_count(&self) -> usize {
        self.contradictions
            .iter()
            .filter(|c| c.severity == ContradictionSeverity::Warning)
            .count()
    }
}
