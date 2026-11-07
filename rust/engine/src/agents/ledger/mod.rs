//! Ledger auditor (math auditor) agent. Port of `stirling.agents.ledger`.

pub mod agent;
pub mod prompts;
pub mod validators;

pub use agent::{
    ExtractedFigure, FigureExtractionResult, FormulaCheck, MathAuditorAgent, StatementCheck,
    StatementsResult, TableFormulas,
};
