//! The shared cases in `testing/tool-io-cases.json`, which the Java
//! `ToolChainValidatorConformanceTest`, the frontend `toolIOCompat.test.ts`
//! and the Python `test_tool_io_compat.py` also run. Specs are synthetic so
//! retyping a real endpoint never invalidates a case; `expected` is matched
//! on stepIndex/severity/code only — messages are free text.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;
use stirling_engine::models::tool_io::ToolIOSpec;
use stirling_engine::models::tool_models::ToolEndpoint;
use stirling_engine::services::tool_io_compat::{
    validate_tool_chain, DiagnosticCode, DiagnosticSeverity, ToolChainStep,
};

#[derive(Deserialize)]
struct Fixture {
    specs: HashMap<String, ToolIOSpec>,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    #[serde(default)]
    steps: Vec<Step>,
    #[serde(rename = "sourceFormat")]
    source_format: Option<String>,
    #[serde(default)]
    expected: Vec<Expected>,
}

#[derive(Deserialize)]
struct Step {
    spec: Option<String>,
    #[serde(default)]
    parameters: HashMap<String, Value>,
}

#[derive(Deserialize)]
struct Expected {
    #[serde(rename = "stepIndex")]
    step_index: usize,
    severity: String,
    code: String,
}

fn fixture() -> Fixture {
    // The fixture lives at the repo root, shared with backend and frontend.
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        let candidate = dir.join("testing/tool-io-cases.json");
        if candidate.exists() {
            return serde_json::from_str(&std::fs::read_to_string(candidate).unwrap()).unwrap();
        }
        if !dir.pop() {
            panic!("testing/tool-io-cases.json not found above the crate");
        }
    }
}

fn severity_name(s: DiagnosticSeverity) -> &'static str {
    match s {
        DiagnosticSeverity::Error => "ERROR",
        DiagnosticSeverity::Warn => "WARN",
        DiagnosticSeverity::Info => "INFO",
    }
}

fn code_name(c: DiagnosticCode) -> &'static str {
    match c {
        DiagnosticCode::Undeclared => "undeclared-operation",
        DiagnosticCode::FormatMismatch => "format-mismatch",
        DiagnosticCode::OutputUncertain => "output-uncertain",
        DiagnosticCode::SourceMismatch => "source-mismatch",
        DiagnosticCode::FanOut => "fan-out",
        DiagnosticCode::FanIn => "fan-in",
    }
}

#[test]
fn shared_tool_io_cases() {
    let data = fixture();
    assert!(!data.cases.is_empty());

    for case in &data.cases {
        let mut table: HashMap<ToolEndpoint, ToolIOSpec> = HashMap::new();
        let mut steps = Vec::new();
        for (index, step) in case.steps.iter().enumerate() {
            // Distinct endpoints so the same spec can appear twice in a chain.
            // Which ones is irrelevant: the case supplies its own table.
            let operation = ToolEndpoint::ALL[index % ToolEndpoint::ALL.len()];
            if let Some(spec) = &step.spec {
                table.insert(
                    operation,
                    data.specs
                        .get(spec)
                        .unwrap_or_else(|| panic!("{}: unknown spec {spec}", case.name))
                        .clone(),
                );
            }
            steps.push(ToolChainStep {
                operation,
                parameters: step.parameters.clone(),
            });
        }

        let source = case.source_format.as_deref().map(|s| {
            serde_json::from_value::<stirling_engine::models::tool_io::ToolFormat>(Value::String(
                s.to_string(),
            ))
            .unwrap()
        });
        let diagnostics = validate_tool_chain(&steps, source, Some(&table));
        let actual: Vec<String> = diagnostics
            .iter()
            .map(|d| {
                format!(
                    "{}:{}:{}",
                    d.step_index,
                    severity_name(d.severity),
                    code_name(d.code)
                )
            })
            .collect();
        let expected: Vec<String> = case
            .expected
            .iter()
            .map(|e| format!("{}:{}:{}", e.step_index, e.severity, e.code))
            .collect();
        assert_eq!(
            actual,
            expected,
            "case {:?}: {:?}",
            case.name,
            diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }
}
