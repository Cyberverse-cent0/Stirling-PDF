//! Provider-aware output mode. Port of `stirling.agents.output_mode`:
//! Ollama/custom OpenAI-compatible endpoints need tool-delivered structured
//! output, which also earns a larger output-validation retry budget.

/// Providers whose OpenAI-compatible endpoint needs tool-delivered structured output.
pub fn uses_tool_output(chat_provider: &str) -> bool {
    matches!(chat_provider, "ollama" | "custom")
}

/// Local models delivering via ToolOutput need more output-validation
/// retries than native structured output.
pub fn output_retries(chat_provider: &str, native: usize, tool: usize) -> usize {
    if uses_tool_output(chat_provider) {
        tool
    } else {
        native
    }
}

/// The pydantic-ai defaults used across agents: 1 retry for native output,
/// 6 for tool-delivered output.
pub fn structured_output_retries(chat_provider: &str) -> usize {
    output_retries(chat_provider, 1, 6)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_output_only_for_local_providers() {
        assert!(uses_tool_output("ollama"));
        assert!(uses_tool_output("custom"));
        assert!(!uses_tool_output("openai"));
        assert!(!uses_tool_output("anthropic"));
        assert!(!uses_tool_output(""));
    }

    #[test]
    fn retries_follow_output_mode() {
        assert_eq!(output_retries("ollama", 1, 6), 6);
        assert_eq!(output_retries("openai", 1, 6), 1);
        assert_eq!(structured_output_retries("custom"), 6);
    }
}
