//! Ledger auditor — deterministic validators. Port of
//! `stirling.agents.ledger.validators`: regex/Decimal pipelines that check
//! arithmetic, named-figure consistency, and LLM-inferred table formulas.
//! No LLM calls live here.

pub mod arithmetic;
pub mod figures;
pub mod formula;

pub use arithmetic::ArithmeticScanner;
pub use figures::FigureTracker;
pub use formula::FormulaEvaluator;
