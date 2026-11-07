//! ArithmeticScanner — finds and verifies inline arithmetic expressions in
//! text. Port of `validators/arithmetic.py`: deterministic regex + Decimal
//! pipeline, no LLM.

use std::sync::LazyLock;

use regex::Regex;
use rust_decimal::Decimal;

use crate::contracts::ledger::{Discrepancy, DiscrepancyKind, Severity};

use super::parsing::{strip_chars, to_decimal};

// Currency / number token: optional sign, optional currency symbol,
// digits with optional thousands separator and decimal point.
const NUM: &str = r"[£$€¥]?-?[\d,]+(?:\.\d+)?";

static EQUALS_EXPR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"({NUM}(?:\s*[+\-]\s*{NUM})+)\s*=\s*({NUM})")).unwrap());

static TOTAL_THEN_ADDENDS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?:total|sum|grand total|subtotal)\s*[:\-]?\s*({NUM})\s*\(({NUM}(?:\s*[+\-]\s*{NUM})+)\)",
    ))
    .unwrap()
});

/// Evaluate a simple additive expression `A +/- B +/- C ...`; `None` when
/// unparseable. Mirrors Python's `re.split(r"([+\-])", ...)`: every `+`/`-`
/// is an operator token, so `-` before a number reads as subtraction from
/// the running total (same result as a unary minus on the first operand).
fn eval_expression(expr: &str) -> Option<Decimal> {
    let cleaned = strip_chars(expr.trim());
    let mut result = Decimal::ZERO;
    let mut operator = '+';
    let mut number = String::new();
    let flush = |number: &mut String, operator: char, result: &mut Decimal| -> Option<()> {
        if number.is_empty() {
            return Some(());
        }
        // Trailing operator chars can never appear inside `number`.
        let val = to_decimal(number)?;
        number.clear();
        *result = if operator == '+' {
            *result + val
        } else {
            *result - val
        };
        Some(())
    };
    for c in cleaned.chars() {
        if c == '+' || c == '-' {
            flush(&mut number, operator, &mut result)?;
            operator = c;
        } else {
            number.push(c);
        }
    }
    flush(&mut number, operator, &mut result)?;
    Some(result)
}

pub struct ArithmeticScanner {
    tolerance: Decimal,
}

impl ArithmeticScanner {
    pub fn new(tolerance: Decimal) -> Self {
        Self { tolerance }
    }

    /// Find all verifiable arithmetic expressions in `text` and return a
    /// Discrepancy for each one that does not balance within tolerance.
    pub fn scan(&self, page: i64, text: &str) -> Vec<Discrepancy> {
        let mut discrepancies = self.check_equals_expressions(page, text);
        discrepancies.extend(self.check_total_then_addends(page, text));
        discrepancies
    }

    /// Patterns like `100 + 200 = 300`.
    fn check_equals_expressions(&self, page: i64, text: &str) -> Vec<Discrepancy> {
        let mut found = Vec::new();
        for m in EQUALS_EXPR.captures_iter(text) {
            let expr_str = m.get(1).map(|g| g.as_str()).unwrap_or("");
            let stated_str = m.get(2).map(|g| g.as_str()).unwrap_or("");
            let (Some(computed), Some(stated)) =
                (eval_expression(expr_str), to_decimal(stated_str))
            else {
                continue;
            };
            if (computed - stated).abs() > self.tolerance {
                found.push(Discrepancy {
                    page,
                    kind: DiscrepancyKind::Arithmetic,
                    severity: Severity::Error,
                    description: format!(
                        "Arithmetic error: {} should equal {}, not {}",
                        expr_str.trim(),
                        computed,
                        stated
                    ),
                    stated: stated.to_string(),
                    expected: computed.to_string(),
                    context: m.get(0).map(|g| g.as_str()).unwrap_or("").to_string(),
                });
            }
        }
        found
    }

    /// Patterns like `Total: 450 (100 + 200 + 150)` — total precedes addends.
    fn check_total_then_addends(&self, page: i64, text: &str) -> Vec<Discrepancy> {
        let mut found = Vec::new();
        for m in TOTAL_THEN_ADDENDS.captures_iter(text) {
            let stated_str = m.get(1).map(|g| g.as_str()).unwrap_or("");
            let expr_str = m.get(2).map(|g| g.as_str()).unwrap_or("");
            let (Some(stated), Some(computed)) =
                (to_decimal(stated_str), eval_expression(expr_str))
            else {
                continue;
            };
            if (computed - stated).abs() > self.tolerance {
                found.push(Discrepancy {
                    page,
                    kind: DiscrepancyKind::Arithmetic,
                    severity: Severity::Error,
                    description: format!(
                        "Stated total {} does not match addends ({} = {})",
                        stated,
                        expr_str.trim(),
                        computed
                    ),
                    stated: stated.to_string(),
                    expected: computed.to_string(),
                    context: m.get(0).map(|g| g.as_str()).unwrap_or("").to_string(),
                });
            }
        }
        found
    }
}
