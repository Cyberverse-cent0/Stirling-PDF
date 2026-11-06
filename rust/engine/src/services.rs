//! Shared non-AI services. Port of `stirling.services` pieces as they are
//! needed.

pub mod language;
pub mod operation_shortlist;
pub mod progress;
pub mod tool_io_compat;
pub mod tracking;

pub use language::language_directive;
