//! Port of `engine/tests/test_tool_model_descriptions.py`: every
//! AI-selectable operation must describe itself in the planner prompt.
//! Descriptions come from the generated `tool_schemas.json` (pydantic
//! `model_json_schema()` output), so this validates the same source the
//! planner prompt renders.

use std::collections::HashMap;

use stirling_engine::models::tool_models::ToolEndpoint;
use stirling_engine::models::tool_schemas::operation_description;

fn normalised_name(op: ToolEndpoint) -> String {
    // Python: endpoint.name.lower().replace("_", " ")
    op.member_name().to_lowercase().replace('_', " ")
}

fn normalised_description(description: &str) -> String {
    description
        .to_lowercase()
        .replace(['-', '_'], " ")
        .trim_matches(|c| c == ' ' || c == '.')
        .to_string()
}

#[test]
fn every_operation_has_a_description() {
    for op in ToolEndpoint::ALL {
        assert!(
            !operation_description(op).is_empty(),
            "{} ({}) has no description",
            op.member_name(),
            op.as_str()
        );
    }
}

#[test]
fn description_is_not_just_the_endpoint_name() {
    for op in ToolEndpoint::ALL {
        let description = operation_description(op);
        if description.is_empty() {
            continue;
        }
        assert_ne!(
            normalised_description(&description),
            normalised_name(op),
            "{} description restates its own name: {description:?}",
            op.member_name()
        );
    }
}

#[test]
fn description_is_a_single_line() {
    for op in ToolEndpoint::ALL {
        let description = operation_description(op);
        assert_eq!(
            description.split_whitespace().collect::<Vec<_>>().join(" "),
            description,
            "{} description carries raw line breaks: {description:?}",
            op.member_name()
        );
    }
}

#[test]
fn descriptions_are_mutually_distinct() {
    let mut by_description: HashMap<String, Vec<String>> = HashMap::new();
    for op in ToolEndpoint::ALL {
        by_description
            .entry(operation_description(op))
            .or_default()
            .push(op.member_name());
    }
    let shared: Vec<String> = by_description
        .iter()
        .filter(|(_, names)| names.len() > 1)
        .map(|(text, names)| format!("{} all say {text:?}", names.join(", ")))
        .collect();
    assert!(shared.is_empty(), "{}", shared.join("; "));
}
