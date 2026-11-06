//! Agent spec contracts. Port of `stirling.contracts.agent_specs`.
//! `parameters` stays `serde_json::Value` until the typify tool-params module lands.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::contracts::common::{StepKind, ToolOperationStep};
use crate::models::ToolEndpoint;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AiToolAgentStepPayload {
    pub title: String,
    pub description: String,
    pub tool: ToolEndpoint,
    pub instruction: String,
}

/// `kind`-discriminated spec step (`tool` | `ai_tool`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentSpecStep {
    #[serde(rename = "tool")]
    Tool(ToolOperationStep),
    #[serde(rename = "ai_tool")]
    AiTool(AiToolAgentStepPayload),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentSpec {
    pub name: String,
    pub description: String,
    pub objective: String,
    #[serde(default)]
    pub steps: Vec<AgentSpecStep>,
}

// StepKind is re-exported for parity with Python's contracts.common surface.
#[allow(dead_code)]
type _StepKindAlias = StepKind;
