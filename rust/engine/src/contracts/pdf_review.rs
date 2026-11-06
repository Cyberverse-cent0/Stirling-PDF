//! PDF review contracts. Port of `stirling.contracts.pdf_review`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::contracts::common::NeedIngestResponse;
use crate::contracts::pdf_edit::EditPlanPayload;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PdfReviewOrchestrateResponse {
    #[serde(rename = "plan")]
    Plan(EditPlanPayload),
    #[serde(rename = "need_ingest")]
    NeedIngest(NeedIngestResponse),
}
