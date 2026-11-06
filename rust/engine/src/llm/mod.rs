//! LLM provider access: Anthropic Messages and OpenAI-compatible chat
//! completions over reqwest, with a process-wide concurrency semaphore.
//! Port of the model/provider half of `stirling.services.runtime`.

pub mod client;
pub mod model;
