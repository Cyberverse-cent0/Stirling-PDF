//! Tool-chain compatibility validation — port of
//! `stirling.services.tool_io_compat`. The same rules exist in Java
//! (`ToolChainValidator`) and the frontend (`toolIoCompat.ts`);
//! `testing/tool-io-cases.json` runs against all three, so a rule that
//! changes in one place and not the others fails there.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::models::tool_io::{tool_io, ToolArity, ToolFormat, ToolIOSpec};
use crate::models::tool_models::ToolEndpoint;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiagnosticSeverity {
    /// Only ERROR means the chain cannot run as configured.
    #[serde(rename = "ERROR")]
    Error,
    #[serde(rename = "WARN")]
    Warn,
    #[serde(rename = "INFO")]
    Info,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiagnosticCode {
    #[serde(rename = "undeclared-operation")]
    Undeclared,
    #[serde(rename = "format-mismatch")]
    FormatMismatch,
    #[serde(rename = "output-uncertain")]
    OutputUncertain,
    #[serde(rename = "source-mismatch")]
    SourceMismatch,
    #[serde(rename = "fan-out")]
    FanOut,
    #[serde(rename = "fan-in")]
    FanIn,
}

/// One problem, against the step that cannot run.
#[derive(Debug, Clone)]
pub struct ToolDiagnostic {
    pub step_index: usize,
    pub severity: DiagnosticSeverity,
    pub code: DiagnosticCode,
    pub message: String,
}

/// `parameters` is only used to resolve an output that depends on one.
#[derive(Debug, Clone)]
pub struct ToolChainStep {
    pub operation: ToolEndpoint,
    pub parameters: HashMap<String, Value>,
}

/// `certain` is false when a case reads a parameter we cannot see.
#[derive(Debug, Clone)]
pub struct ResolvedOutput {
    pub format: ToolFormat,
    pub arity: ToolArity,
    pub certain: bool,
}

fn is_multi_input(arity: ToolArity) -> bool {
    matches!(arity, ToolArity::Miso | ToolArity::Mimo)
}

fn is_multi_output(arity: ToolArity) -> bool {
    matches!(arity, ToolArity::Simo | ToolArity::Mimo)
}

fn accepts_format(spec: &ToolIOSpec, candidate: ToolFormat) -> bool {
    candidate == ToolFormat::Any
        || spec.accepts.contains(&ToolFormat::Any)
        || spec.accepts.contains(&candidate)
}

/// Both sides of a condition go through this, so a declaration's casing
/// cannot matter.
fn normalise(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.trim().to_lowercase(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

fn normalise_str(value: &str) -> String {
    value.trim().to_lowercase()
}

/// First case whose conditions all hold wins. If none match but one reads a
/// parameter we cannot see, the declared output comes back uncertain: an
/// unseen value might have picked another branch.
pub fn resolve_output(
    spec: &ToolIOSpec,
    parameters: Option<&HashMap<String, Value>>,
) -> ResolvedOutput {
    let mut saw_unknown_param = false;
    for rule in &spec.cases {
        let mut all_hold = true;
        for condition in &rule.when {
            let raw = match parameters.and_then(|p| p.get(&condition.param)) {
                Some(v) => Some(v.clone()),
                None => condition.default.as_ref().map(|d| Value::String(d.clone())),
            };
            match raw {
                Some(v) => {
                    let n = normalise(&v);
                    all_hold = all_hold && condition.matches.iter().any(|m| normalise_str(m) == n);
                }
                None => {
                    saw_unknown_param = true;
                    all_hold = false;
                }
            }
        }
        if all_hold {
            return ResolvedOutput {
                format: rule.produces,
                arity: rule.arity,
                certain: true,
            };
        }
    }
    ResolvedOutput {
        format: spec.produces,
        arity: spec.arity,
        certain: !saw_unknown_param,
    }
}

/// Every problem with the chain, against the step that cannot run.
///
/// `table` lets tests (and the shared `tool-io-cases.json` fixture) inject
/// synthetic specs; production callers pass `None` for the generated `TOOL_IO`.
pub fn validate_tool_chain(
    steps: &[ToolChainStep],
    source_format: Option<ToolFormat>,
    table: Option<&HashMap<ToolEndpoint, ToolIOSpec>>,
) -> Vec<ToolDiagnostic> {
    let mut diagnostics = Vec::new();
    let mut carried: Option<ResolvedOutput> = None;

    for (index, step) in steps.iter().enumerate() {
        let spec = match table {
            Some(t) => t.get(&step.operation).cloned(),
            None => tool_io(step.operation),
        };
        let Some(spec) = spec else {
            diagnostics.push(ToolDiagnostic {
                step_index: index,
                severity: DiagnosticSeverity::Warn,
                code: DiagnosticCode::Undeclared,
                message: format!(
                    "{} does not declare what it accepts or produces, \
                     so the rest of the chain cannot be checked.",
                    step.operation.as_str()
                ),
            });
            // Nothing is known past an undeclared step.
            carried = None;
            continue;
        };

        // Only the first step is handed the pipeline's input. Every later
        // step receives the previous step's output, unknown once an
        // undeclared step intervened.
        if index == 0 {
            if let Some(source) = source_format {
                if !accepts_format(&spec, source) {
                    diagnostics.push(ToolDiagnostic {
                        step_index: index,
                        severity: DiagnosticSeverity::Error,
                        code: DiagnosticCode::SourceMismatch,
                        message: format!(
                            "{} accepts {} but the input is {:?}.",
                            step.operation.as_str(),
                            describe(&spec),
                            source
                        ),
                    });
                }
            }
        } else if let Some(prev) = &carried {
            diagnostics.extend(check_transition(index, step, &spec, prev));
        }
        carried = Some(resolve_output(&spec, Some(&step.parameters)));
    }

    diagnostics
}

fn check_transition(
    index: usize,
    step: &ToolChainStep,
    spec: &ToolIOSpec,
    previous: &ResolvedOutput,
) -> Vec<ToolDiagnostic> {
    if previous.format == ToolFormat::None {
        return vec![ToolDiagnostic {
            step_index: index,
            severity: DiagnosticSeverity::Error,
            code: DiagnosticCode::FormatMismatch,
            message: format!(
                "The previous step returns a report rather than a file, so {} has nothing to run on.",
                step.operation.as_str()
            ),
        }];
    }

    if !accepts_format(spec, previous.format) {
        let message = format!(
            "{} accepts {} but the previous step produces {:?}.",
            step.operation.as_str(),
            describe(spec),
            previous.format
        );
        // Unresolved output: may yet be fine once the step is configured.
        return vec![ToolDiagnostic {
            step_index: index,
            severity: if previous.certain {
                DiagnosticSeverity::Error
            } else {
                DiagnosticSeverity::Warn
            },
            code: if previous.certain {
                DiagnosticCode::FormatMismatch
            } else {
                DiagnosticCode::OutputUncertain
            },
            message,
        }];
    }

    if !previous.certain {
        return vec![ToolDiagnostic {
            step_index: index,
            severity: DiagnosticSeverity::Warn,
            code: DiagnosticCode::OutputUncertain,
            message: format!(
                "The previous step's output depends on how it is configured, \
                 so {} may not be able to run.",
                step.operation.as_str()
            ),
        }];
    }

    if is_multi_output(previous.arity) {
        let fan_in = is_multi_input(spec.arity);
        return vec![ToolDiagnostic {
            step_index: index,
            severity: DiagnosticSeverity::Info,
            code: if fan_in {
                DiagnosticCode::FanIn
            } else {
                DiagnosticCode::FanOut
            },
            message: if fan_in {
                format!(
                    "{} combines every file the previous step produced.",
                    step.operation.as_str()
                )
            } else {
                format!(
                    "{} runs once for each file the previous step produced.",
                    step.operation.as_str()
                )
            },
        }];
    }

    Vec::new()
}

fn describe(spec: &ToolIOSpec) -> String {
    let mut names: Vec<String> = spec.accepts.iter().map(|f| format!("{f:?}")).collect();
    names.sort();
    names.dedup();
    names.join(" or ")
}

/// The diagnostics that mean the chain cannot run.
pub fn blocking(diagnostics: &[ToolDiagnostic]) -> Vec<&ToolDiagnostic> {
    diagnostics
        .iter()
        .filter(|d| d.severity == DiagnosticSeverity::Error)
        .collect()
}
