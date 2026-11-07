//! Shared parsing helpers for ledger validators. Port of
//! `validators/_parsing.py`.

use rust_decimal::Decimal;

/// Strip common currency symbols and thousands separators before parsing —
/// the `STRIP_PATTERN` sub in Python.
pub(crate) fn strip_chars(raw: &str) -> String {
    raw.chars()
        .filter(|c| !matches!(c, '£' | '$' | '€' | '¥' | ',') && !c.is_whitespace())
        .collect()
}

/// Parse a cell value to Decimal, returning `None` for non-numeric cells.
pub fn to_decimal(raw: &str) -> Option<Decimal> {
    let mut cleaned = strip_chars(raw.trim());
    if cleaned.is_empty() || matches!(cleaned.as_str(), "-" | "—" | "n/a" | "N/A" | "na" | "NA") {
        return None;
    }
    // Parenthesised negatives: (123.45) → -123.45
    if cleaned.starts_with('(') && cleaned.ends_with(')') && cleaned.len() >= 2 {
        cleaned = format!("-{}", &cleaned[1..cleaned.len() - 1]);
    }
    cleaned.parse::<Decimal>().ok()
}

/// Parse a CSV string into rows, dropping completely empty rows. Python's
/// `csv.reader` handles quoted cells with embedded commas/newlines; this
/// minimal parser covers the tabula/CSV shapes the auditor sees.
pub fn parse_csv(table_csv: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = table_csv.trim().chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                if in_quotes && chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    in_quotes = !in_quotes;
                }
            }
            ',' if !in_quotes => {
                row.push(std::mem::take(&mut field));
            }
            '\n' | '\r' if !in_quotes => {
                if c == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                row.push(std::mem::take(&mut field));
                if row.iter().any(|cell| !cell.trim().is_empty()) {
                    rows.push(std::mem::take(&mut row));
                } else {
                    row.clear();
                }
            }
            _ => field.push(c),
        }
    }
    row.push(field);
    if row.iter().any(|cell| !cell.trim().is_empty()) {
        rows.push(row);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    #[test]
    fn decimal_parsing() {
        assert_eq!(to_decimal("£1,200.00"), Some(dec("1200.00")));
        assert_eq!(to_decimal("(123.45)"), Some(dec("-123.45")));
        assert_eq!(to_decimal("n/a"), None);
        assert_eq!(to_decimal(""), None);
        assert_eq!(to_decimal("abc"), None);
    }

    #[test]
    fn csv_parsing() {
        let rows = parse_csv("a,b\n1,2\n\n3,\"x, y\"\n");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2], vec!["3".to_string(), "x, y".to_string()]);
    }
}
