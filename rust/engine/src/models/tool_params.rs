//! Generated tool-parameter types land here via
//! `scripts/generate_tool_params.sh` (typify over the Java OpenAPI spec).
//!
//! Until the first generation run, `ToolOperationStep.parameters` and
//! `ToolCallExecutionPayload.parameters` stay `serde_json::Value` — the wire
//! shape round-trips either way; the typed models exist for schema-driven
//! prompt construction in the agent layer.
