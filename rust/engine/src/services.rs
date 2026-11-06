//! Shared non-AI services. Port of `stirling.services` pieces as they are
//! needed; currently reply-language directives.

/// Per-request reply language, bound by the orchestrator, read by prompt
/// builders. Port of `stirling.services.language`.
///
/// The Python version uses a contextvar set on the orchestrator path; until
/// the Rust orchestrator lands, the locale is carried in axum request
/// extensions by [`crate::middleware::user_id`]-style middleware. For now
/// only the default directive is reachable.
pub fn language_directive() -> &'static str {
    "Write anything the user will read in the same language as their message."
}
