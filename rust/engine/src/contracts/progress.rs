//! Progress events streamed during orchestrator runs. Port of
//! `stirling.contracts.progress`; `phase` is the serde tag.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum ProgressEvent {
    #[serde(rename = "whole_doc_read_started")]
    WholeDocReadStarted {
        question: String,
        pages: i64,
        slices: i64,
    },
    #[serde(rename = "whole_doc_slice_done")]
    WholeDocSliceDone {
        completed: i64,
        total: i64,
        pages: String,
        duration_ms: i64,
        excerpts: i64,
        facts: i64,
    },
    #[serde(rename = "whole_doc_compression_round")]
    WholeDocCompressionRound {
        round_number: i64,
        notes_in: i64,
        groups: i64,
    },
    #[serde(rename = "whole_doc_read_done")]
    WholeDocReadDone {
        completed: i64,
        slices: i64,
        duration_seconds: f64,
    },
}
