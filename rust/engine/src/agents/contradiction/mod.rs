//! Contradiction agent — Python-only textual contradiction detection.
//! Port of `stirling.agents.contradiction`: no Java counterpart, no HTTP
//! endpoint, no resume-turn artifact. Consumed by the review agent and by
//! the question agent via the `find_contradictions` tool capability.

pub mod capability;
pub mod detector;
pub mod intent;
pub mod ledger;
pub mod prompts;

pub use capability::{format_report, ContradictionCapability, DEFAULT_MAX_AUDITS};
pub use detector::{ContradictionDetector, DetectorError};
pub use intent::ContradictionIntentClassifier;
pub use ledger::ClaimLedger;
