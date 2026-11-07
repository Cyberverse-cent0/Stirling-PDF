//! Agent-execution contracts. Port of `stirling.contracts.execution`.
//! `parameters` stays `serde_json::Value` until the typify tool-params module lands.

use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::contracts::agent_specs::AgentSpec;
use crate::models::{ToolEndpoint, ToolId};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionStepResult {
    #[serde(alias = "step_index")]
    pub step_index: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<ToolEndpoint>,
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(alias = "output_summary")]
    pub output_summary: Option<String>,
    #[serde(default)]
    #[serde(alias = "output_data")]
    pub output_data: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(alias = "trigger_type")]
    pub trigger_type: Option<String>,
    #[serde(default)]
    #[serde(alias = "input_files")]
    pub input_files: Vec<String>,
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentExecutionRequest {
    #[serde(alias = "agent_spec")]
    pub agent_spec: AgentSpec,
    #[serde(alias = "current_step_index")]
    pub current_step_index: i64,
    #[serde(alias = "execution_context")]
    pub execution_context: ExecutionContext,
    #[serde(default)]
    #[serde(alias = "previous_step_results")]
    pub previous_step_results: Vec<ExecutionStepResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolCallExecutionPayload {
    pub tool: ToolId,
    pub parameters: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompletedExecutionPayload {
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CannotContinueExecutionPayload {
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum NextExecutionAction {
    #[serde(rename = "tool_call")]
    ToolCall(ToolCallExecutionPayload),
    #[serde(rename = "completed")]
    Completed(CompletedExecutionPayload),
    #[serde(rename = "cannot_continue")]
    CannotContinue(CannotContinueExecutionPayload),
}
