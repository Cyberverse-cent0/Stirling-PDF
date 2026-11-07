//! FormulaEvaluator — verifies LLM-inferred formulas against CSV table
//! data. Port of `validators/formula.py`: safe expression syntax
//! (`colN`, `cell(r,c)`, `sum(colN, a-b)`, `+ - * /`), all Decimal, no eval.

use std::sync::LazyLock;

use regex::Regex;
use rust_decimal::Decimal;

use crate::contracts::ledger::{Discrepancy, DiscrepancyKind, Severity};

use super::parsing::{parse_csv, to_decimal};

static SUM_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"sum\(\s*col(\d+)\s*,\s*(\d+)\s*-\s*(\d+)\s*\)").unwrap());
static CELL_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"cell\(\s*(\d+)\s*,\s*(\d+)\s*\)").unwrap());
static COL_REF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bcol(\d+)\b").unwrap());
static LHS_CELL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"cell\(\s*(\d+)\s*,\s*(\d+)\s*\)").unwrap());
static SAFE_EVAL_TOKENS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d+(?:\.\d+)?|[+\-*/]").unwrap());
static COL_REF_PLAIN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"col(\d+)").unwrap());

pub struct FormulaEvaluator {
    tolerance: Decimal,
}

impl FormulaEvaluator {
    pub fn new(tolerance: Decimal) -> Self {
        Self { tolerance }
    }

    /// `scope`: `"each_row"` | `"column_total"` | `"single_cell"`.
    #[allow(clippy::too_many_arguments)]
    pub fn evaluate(
        &self,
        page: i64,
        table_csv: &str,
        formula: &str,
        scope: &str,
        description: &str,
        row_range: Option<&[i64]>,
        target_row: Option<i64>,
        target_col: Option<i64>,
    ) -> Vec<Discrepancy> {
        let rows = parse_csv(table_csv);
        if rows.len() < 2 {
            return Vec::new();
        }
        match scope {
            "each_row" => self.check_each_row(page, &rows, formula, description, row_range),
            "column_total" => {
                self.check_column_total(page, &rows, formula, description, target_row, target_col)
            }
            "single_cell" => {
                self.check_single_cell(page, &rows, formula, description, target_row, target_col)
            }
            _ => {
                tracing::warn!("[formula] unknown scope {scope:?}, skipping");
                Vec::new()
            }
        }
    }

    /// Verify formula holds for each data row — `"colX = expr"` format.
    fn check_each_row(
        &self,
        page: i64,
        rows: &[Vec<String>],
        formula: &str,
        description: &str,
        row_range: Option<&[i64]>,
    ) -> Vec<Discrepancy> {
        let mut discrepancies = Vec::new();
        let Some((lhs, rhs)) = formula.split_once('=') else {
            return discrepancies;
        };
        let lhs = lhs.trim();
        let rhs = rhs.trim();
        let Some(lhs_col) = Self::parse_col_ref(lhs) else {
            return discrepancies;
        };

        let check_rows: Vec<i64> = match row_range {
            Some(range) => range.to_vec(),
            None => (1..rows.len() as i64).collect(),
        };

        for row_idx in check_rows {
            // Python indexes rows[row_idx] directly — negatives wrap from
            // the end; out-of-range positives are skipped by the guard.
            let row_idx = if row_idx < 0 {
                rows.len() as i64 + row_idx
            } else {
                row_idx
            };
            if row_idx < 0 || row_idx as usize >= rows.len() {
                continue;
            }
            let row = &rows[row_idx as usize];
            let Some(stated) = Self::get_cell(row, lhs_col) else {
                continue;
            };
            let Some(computed) = self.eval_row_expr(rhs, row, rows) else {
                continue;
            };
            if (stated - computed).abs() > self.tolerance {
                discrepancies.push(Discrepancy {
                    page,
                    kind: DiscrepancyKind::Tally,
                    severity: Severity::Error,
                    description: format!(
                        "{description}: row {row_idx} — stated {stated}, expected {computed}"
                    ),
                    stated: stated.to_string(),
                    expected: computed.to_string(),
                    context: format!("row {row_idx}, {formula}"),
                });
            }
        }
        discrepancies
    }

    /// Verify a total row contains correct column sums.
    fn check_column_total(
        &self,
        page: i64,
        rows: &[Vec<String>],
        _formula: &str,
        description: &str,
        target_row: Option<i64>,
        target_col: Option<i64>,
    ) -> Vec<Discrepancy> {
        let mut discrepancies = Vec::new();
        let Some(target_row) = target_row else {
            return discrepancies;
        };
        // Python `rows[target_row]` — negatives index from the end.
        let total_idx = if target_row < 0 {
            rows.len() as i64 + target_row
        } else {
            target_row
        };
        if total_idx < 0 || total_idx as usize >= rows.len() {
            return discrepancies;
        }
        let total_row = &rows[total_idx as usize];

        // Which columns to check: explicit target_col, else every column of
        // the total row.
        let cols_to_check: Vec<i64> = match target_col {
            Some(c) => vec![c],
            None => (0..total_row.len() as i64).collect(),
        };
        // Addend rows: everything between header (row 0) and the total row.
        let addend_rows: Vec<i64> = (1..target_row).collect();

        for col in cols_to_check {
            let Some(stated) = Self::get_cell(total_row, col) else {
                continue;
            };
            let mut computed = Decimal::ZERO;
            let mut has_addends = false;
            for r_idx in &addend_rows {
                let Some(row) = rows.get(*r_idx as usize) else {
                    continue;
                };
                if let Some(val) = Self::get_cell(row, col) {
                    computed += val;
                    has_addends = true;
                }
            }
            if !has_addends {
                continue;
            }
            if (stated - computed).abs() > self.tolerance {
                discrepancies.push(Discrepancy {
                    page,
                    kind: DiscrepancyKind::Tally,
                    severity: Severity::Error,
                    description: format!(
                        "{description}: column {col} — stated {stated}, expected {computed}"
                    ),
                    stated: stated.to_string(),
                    expected: computed.to_string(),
                    context: format!("column {col}, total row {target_row}"),
                });
            }
        }
        discrepancies
    }

    /// Verify a single cell formula (e.g. Grand Total = Subtotal + Tax).
    fn check_single_cell(
        &self,
        page: i64,
        rows: &[Vec<String>],
        formula: &str,
        description: &str,
        target_row: Option<i64>,
        target_col: Option<i64>,
    ) -> Vec<Discrepancy> {
        let Some((lhs, rhs)) = formula.split_once('=') else {
            return Vec::new();
        };

        // Parse target from LHS cell(r,c) when not provided explicitly.
        let (target_row, target_col) = match (target_row, target_col) {
            (Some(r), Some(c)) => (r, c),
            _ => {
                let Some(m) = LHS_CELL.captures(lhs.trim()) else {
                    return Vec::new();
                };
                let r: i64 = m[1].parse().unwrap_or(-1);
                let c: i64 = m[2].parse().unwrap_or(-1);
                (r, c)
            }
        };
        // Python `rows[target_row]` — negatives index from the end.
        let row_idx = if target_row < 0 {
            rows.len() as i64 + target_row
        } else {
            target_row
        };
        if row_idx < 0 || row_idx as usize >= rows.len() {
            return Vec::new();
        }

        let row = &rows[row_idx as usize];
        let Some(stated) = Self::get_cell(row, target_col) else {
            return Vec::new();
        };
        let Some(computed) = self.eval_row_expr(rhs.trim(), row, rows) else {
            return Vec::new();
        };
        if (stated - computed).abs() > self.tolerance {
            return vec![Discrepancy {
                page,
                kind: DiscrepancyKind::Tally,
                severity: Severity::Error,
                description: format!("{description}: stated {stated}, expected {computed}"),
                stated: stated.to_string(),
                expected: computed.to_string(),
                context: format!("cell({target_row},{target_col}), {formula}"),
            }];
        }
        Vec::new()
    }

    /// Evaluate an expression in the context of a specific row: `colN` refs,
    /// `cell(r,c)` refs, `sum(colN, start-end)`, `+ - * /`.
    fn eval_row_expr(
        &self,
        expr: &str,
        row: &[String],
        all_rows: &[Vec<String>],
    ) -> Option<Decimal> {
        let mut resolved = expr.to_string();

        // sum(colN, start-end) — resolved against all_rows.
        for m in SUM_PATTERN.captures_iter(expr) {
            let col: usize = m[1].parse().ok()?;
            let start: i64 = m[2].parse().ok()?;
            let end: i64 = m[3].parse().ok()?;
            let mut total = Decimal::ZERO;
            for r_idx in start..=end {
                if r_idx >= 0 && (r_idx as usize) < all_rows.len() {
                    if let Some(val) = Self::get_cell(&all_rows[r_idx as usize], col as i64) {
                        total += val;
                    }
                }
            }
            resolved = resolved.replacen(m.get(0).unwrap().as_str(), &total.to_string(), 1);
        }

        // cell(r, c) references — a missing cell fails the whole expression.
        for m in CELL_PATTERN.captures_iter(&resolved.clone()) {
            let r: i64 = m[1].parse().ok()?;
            let c: i64 = m[2].parse().ok()?;
            if r < 0 || (r as usize) >= all_rows.len() {
                return None;
            }
            let val = Self::get_cell(&all_rows[r as usize], c)?;
            resolved = resolved.replacen(m.get(0).unwrap().as_str(), &val.to_string(), 1);
        }

        // colN references resolve against the current row; any failure
        // aborts (Python's `_failed` flag).
        let mut failed = false;
        let mut out = String::new();
        let mut last = 0usize;
        for m in COL_REF.find_iter(&resolved) {
            out.push_str(&resolved[last..m.start()]);
            last = m.end();
            let col_idx: i64 = m.as_str()[3..].parse().ok()?;
            match Self::get_cell(row, col_idx) {
                Some(val) => out.push_str(&val.to_string()),
                None => {
                    failed = true;
                    out.push_str(m.as_str());
                }
            }
        }
        out.push_str(&resolved[last..]);
        if failed {
            return None;
        }

        self.safe_eval(&out)
    }

    /// Evaluate a simple arithmetic expression of numbers and `+ - * /`
    /// with standard precedence. No eval.
    fn safe_eval(&self, expr: &str) -> Option<Decimal> {
        let tokens: Vec<&str> = SAFE_EVAL_TOKENS
            .find_iter(expr.trim())
            .map(|m| m.as_str())
            .collect();
        if tokens.is_empty() {
            return None;
        }

        // (values, ops) — merge a leading '-' or an operator-adjacent '-'
        // into the next number token, matching the Python pass.
        let mut values: Vec<Decimal> = Vec::new();
        let mut ops: Vec<char> = Vec::new();
        let mut i = 0usize;
        while i < tokens.len() {
            let tok = tokens[i];
            let is_op = matches!(tok, "+" | "-" | "*" | "/");
            if is_op && values.is_empty() && tok == "-" {
                // Leading negative: merge with next number.
                i += 1;
                if i >= tokens.len() {
                    return None;
                }
                values.push(format!("-{}", tokens[i]).parse().ok()?);
            } else if is_op {
                if i + 2 < tokens.len()
                    && tokens[i + 1] == "-"
                    && !matches!(tokens[i + 2], "+" | "-" | "*" | "/")
                {
                    ops.push(tok.chars().next().unwrap());
                    values.push(format!("-{}", tokens[i + 2]).parse().ok()?);
                    i += 2; // skip the '-' and the number
                } else {
                    ops.push(tok.chars().next().unwrap());
                }
            } else {
                values.push(tok.parse().ok()?);
            }
            i += 1;
        }
        if values.is_empty() {
            return None;
        }

        // Pass 1: * and / (left to right).
        let mut j = 0usize;
        while j < ops.len() {
            if ops[j] == '*' || ops[j] == '/' {
                let rhs = values[j + 1];
                values[j] = if ops[j] == '*' {
                    values[j] * rhs
                } else {
                    if rhs.is_zero() {
                        return None;
                    }
                    values[j].checked_div(rhs)?
                };
                values.remove(j + 1);
                ops.remove(j);
            } else {
                j += 1;
            }
        }

        // Pass 2: + and -.
        let mut result = values[0];
        for (j, op) in ops.iter().enumerate() {
            // Malformed token streams (e.g. a leading '+') leave ops longer
            // than values-1; Python surfaces this as IndexError → None.
            let &rhs = values.get(j + 1)?;
            match op {
                '+' => result += rhs,
                '-' => result -= rhs,
                _ => {}
            }
        }
        Some(result)
    }

    fn parse_col_ref(reference: &str) -> Option<i64> {
        let m = COL_REF_PLAIN.find(reference.trim())?;
        // Python uses re.match — must anchor at start.
        if m.start() != 0 {
            return None;
        }
        m.as_str()[3..].parse().ok()
    }

    /// Python `_get_cell`: `col >= len(row)` → None, otherwise `row[col]`
    /// (negative cols index from the end).
    fn get_cell(row: &[String], col: i64) -> Option<Decimal> {
        if col >= row.len() as i64 {
            return None;
        }
        let idx = if col < 0 { row.len() as i64 + col } else { col };
        if idx < 0 {
            return None;
        }
        to_decimal(&row[idx as usize])
    }
}
