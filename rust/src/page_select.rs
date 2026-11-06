//! Port of `GeneralUtils.parsePageList` / `requireValidPageSelection` — the
//! page-selection syntax shared by the Java controllers (`1,3,5-9`, `all`,
//! `an+b` n-functions). Kept behaviour-identical so migrated endpoints accept
//! the same `pageNumbers`/`pageOrder` strings the frontend already sends.

use std::collections::HashSet;

const MAX_N_FUNCTION_PAGES: usize = 10_000;

/// 0-based page indices for `selection`, mirroring
/// `GeneralUtils.parsePageList(selection, totalPages, /* oneBased */ false)`.
/// Out-of-range and malformed tokens are dropped, not errors — validation is
/// [`require_valid_page_selection`]'s job, run beforehand by the caller.
pub fn parse_page_list(selection: &str, total_pages: usize) -> Result<Vec<usize>, String> {
    let mut result: Vec<usize> = Vec::new();
    let mut seen = HashSet::new();
    let max_size = 1000usize.max(total_pages.saturating_mul(3));

    for raw in selection.split(',') {
        let token = raw.trim();
        if token.eq_ignore_ascii_case("all") {
            for i in 0..total_pages {
                if seen.insert(i) {
                    result.push(i);
                }
            }
        } else {
            for page in handle_part(token, total_pages)? {
                if seen.insert(page) {
                    result.push(page);
                }
            }
        }
        if result.len() > max_size {
            return Err(format!(
                "Page list exceeds maximum allowed size of {max_size}"
            ));
        }
    }
    Ok(result)
}

/// Mirrors `GeneralUtils.requireValidPageSelection`: rejects malformed
/// selections and ones matching no page; numbers outside the document are
/// fine while something else matches.
pub fn require_valid_page_selection(selection: &str, total_pages: usize) -> Result<(), String> {
    check_page_selection(selection, total_pages, true)
}

/// Mirrors `GeneralUtils.requireWellFormedPageSelection`: rejects malformed
/// selections but allows ones matching no page — filters treat "nothing
/// matched" as a filter result, not an error.
pub fn require_well_formed_page_selection(
    selection: &str,
    total_pages: usize,
) -> Result<(), String> {
    check_page_selection(selection, total_pages, false)
}

fn check_page_selection(
    selection: &str,
    total_pages: usize,
    require_match: bool,
) -> Result<(), String> {
    if selection.trim().is_empty() {
        return Ok(());
    }

    let mut malformed: Vec<&str> = Vec::new();
    let mut out_of_range: Vec<&str> = Vec::new();
    let mut matches_a_page = false;
    let mut has_token = false;

    for raw in selection.split(',') {
        let token = raw.trim();
        if token.is_empty() {
            continue;
        }
        has_token = true;
        if token.eq_ignore_ascii_case("all") {
            matches_a_page |= total_pages > 0;
            continue;
        }
        if token.contains('n') && is_n_function(token) {
            // Java short-circuits on totalPages > 0 before evaluating, so a
            // zero-page document reports the token out of range, not an error.
            if total_pages == 0 {
                out_of_range.push(token);
                continue;
            }
            match evaluate_n_function(token, total_pages) {
                Ok(pages) if !pages.is_empty() => matches_a_page = true,
                Ok(_) => out_of_range.push(token),
                Err(e) => return Err(e),
            }
            continue;
        }
        match classify_page_token(token, total_pages) {
            PageToken::InRange => matches_a_page = true,
            PageToken::OutOfRange => out_of_range.push(token),
            PageToken::Malformed => malformed.push(token),
        }
    }

    let mut invalid: Vec<&str> = if has_token {
        malformed
    } else {
        vec![selection]
    };
    if require_match && invalid.is_empty() && !matches_a_page {
        invalid = if out_of_range.is_empty() {
            vec![selection]
        } else {
            out_of_range
        };
    }
    if invalid.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Invalid page selection '{}': pages are numbered 1 to {total_pages}.",
            invalid.join(",")
        ))
    }
}

#[derive(PartialEq)]
enum PageToken {
    InRange,
    OutOfRange,
    Malformed,
}

fn classify_page_token(token: &str, total_pages: usize) -> PageToken {
    if !token.is_empty() && token.bytes().all(|b| b.is_ascii_digit()) {
        return match token.parse::<i32>() {
            Ok(page) if (1..=total_pages as i64).contains(&(page as i64)) => PageToken::InRange,
            Ok(_) => PageToken::OutOfRange,
            Err(_) => PageToken::Malformed,
        };
    }
    if let Some((start_raw, end_raw)) = split_range(token) {
        let Ok(start) = start_raw.trim().parse::<i32>() else {
            return PageToken::Malformed;
        };
        let start = start.max(1);
        let end = if end_raw.trim().is_empty() {
            start.max(total_pages as i32)
        } else {
            match end_raw.trim().parse::<i32>() {
                Ok(end) => end,
                Err(_) => return PageToken::Malformed,
            }
        };
        if end < start {
            return PageToken::Malformed;
        }
        return if start <= total_pages as i32 {
            PageToken::InRange
        } else {
            PageToken::OutOfRange
        };
    }
    PageToken::Malformed
}

/// `\d+\s*-\s*\d*` as a full match: leading digits, a dash, optional trailing
/// digits. Returns the raw halves so the caller can apply Java's blank-end
/// rule.
fn split_range(token: &str) -> Option<(&str, &str)> {
    let bytes = token.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 {
        return None;
    }
    let digits_end = i;
    while i < bytes.len() && is_java_space(bytes[i]) {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] != b'-' {
        return None;
    }
    i += 1;
    while i < bytes.len() && is_java_space(bytes[i]) {
        i += 1;
    }
    let end_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i != bytes.len() {
        return None;
    }
    Some((&token[..digits_end], &token[end_start..]))
}

/// Java regex `\s`: space, tab, LF, VT, FF, CR.
fn is_java_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r')
}

/// 0-based indices for one comma-separated token; mirrors
/// `GeneralUtils.handlePart` with `oneBased = false`.
fn handle_part(part: &str, total_pages: usize) -> Result<Vec<usize>, String> {
    let trimmed = part.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    if trimmed.contains('n') && is_n_function(trimmed) {
        return Ok(evaluate_n_function(trimmed, total_pages)?
            .into_iter()
            .filter_map(|page| page.checked_sub(1))
            .filter(|&page| page < total_pages)
            .collect());
    }

    if trimmed.contains('-') {
        let mut range_parts = trimmed.split('-');
        let start_raw = range_parts.next().unwrap_or("");
        let end_raw = range_parts.next().unwrap_or("");
        let Ok(start) = start_raw.trim().parse::<i64>() else {
            return Ok(Vec::new());
        };
        let end = if !end_raw.trim().is_empty() {
            match end_raw.trim().parse::<i64>() {
                Ok(end) => end,
                Err(_) => return Ok(Vec::new()),
            }
        } else {
            total_pages as i64
        };
        return Ok((start..=end)
            .filter(|&i| (1..=total_pages as i64).contains(&i))
            .map(|i| (i - 1) as usize)
            .collect());
    }

    Ok(match trimmed.parse::<i64>() {
        Ok(page) if (1..=total_pages as i64).contains(&page) => vec![(page - 1) as usize],
        _ => Vec::new(),
    })
}

/// Mirrors `GeneralUtils.isNFunction`: the token must consist solely of
/// `[0-9n+\-*/() ]` and parse once `n` is substituted.
fn is_n_function(expression: &str) -> bool {
    let trimmed = expression.trim();
    if trimmed.is_empty()
        || !trimmed
            .chars()
            .all(|c| matches!(c, '0'..='9' | 'n' | '+' | '-' | '*' | '/' | '(' | ')' | ' '))
    {
        return false;
    }
    evaluate_expression(&sanitize_n_function(trimmed, 1)).is_ok()
}

/// Mirrors `GeneralUtils.evaluateNFunc`: evaluates the expression for
/// `n = 1..=max_value`, keeping integral results in `1..=max_value`.
fn evaluate_n_function(expression: &str, max_value: usize) -> Result<Vec<usize>, String> {
    let trimmed = expression.trim();
    if trimmed.is_empty() {
        return Err("Expression cannot be null or empty".into());
    }
    if max_value == 0 || max_value > MAX_N_FUNCTION_PAGES {
        return Err(format!(
            "maxValue must be between 1 and {MAX_N_FUNCTION_PAGES} for safety"
        ));
    }
    if !is_math_expression(trimmed) {
        return Err(format!("Invalid expression format: {trimmed}"));
    }

    let mut results = Vec::new();
    for n in 1..=max_value {
        let sanitized = sanitize_n_function(trimmed, n);
        if let Ok(value) = evaluate_expression(&sanitized) {
            if value.is_finite() {
                let int_result = value as i64;
                if int_result > 0 && int_result <= max_value as i64 {
                    results.push(int_result as usize);
                }
            }
        }
    }
    Ok(results)
}

fn is_math_expression(expression: &str) -> bool {
    !expression.is_empty()
        && expression
            .chars()
            .all(|c| matches!(c, '0'..='9' | 'n' | '+' | '-' | '*' | '/' | '(' | ')' | ' '))
}

/// Port of `GeneralUtils.sanitizeNFunction`, `insertMultiplicationBeforeN`
/// and `formatConsecutiveNsForNFunction`: strips spaces, inserts the `*` the
/// Java side relies on for implicit multiplication, then substitutes `n`.
fn sanitize_n_function(expression: &str, n_value: usize) -> String {
    let mut text: String = expression.chars().filter(|&c| c != ' ').collect();

    // ")" or digit or "n" followed by "(" implies multiplication.
    let mut out = String::with_capacity(text.len() + 8);
    let chars: Vec<char> = text.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        out.push(c);
        if matches!(c, '0'..='9' | 'n' | ')') && chars.get(i + 1) == Some(&'(') {
            out.push('*');
        }
    }
    text = out;

    // ")" followed by ")", a digit or "n" implies multiplication.
    let mut out = String::with_capacity(text.len() + 8);
    let chars: Vec<char> = text.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        out.push(c);
        if c == ')'
            && chars
                .get(i + 1)
                .is_some_and(|n| matches!(n, ')' | '0'..='9' | 'n'))
        {
            out.push('*');
        }
    }
    text = out;

    // Digit immediately before 'n' implies multiplication.
    let mut out = String::with_capacity(text.len() + 8);
    let chars: Vec<char> = text.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        out.push(c);
        if c.is_ascii_digit() && chars.get(i + 1) == Some(&'n') {
            out.push('*');
        }
    }
    text = out;

    // Consecutive 'n's become "n*n" until none remain.
    while text.contains("nn") {
        let mut out = String::with_capacity(text.len() + 2);
        let chars: Vec<char> = text.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if i + 1 < chars.len() && chars[i] == 'n' && chars[i + 1] == 'n' {
                out.push_str("n*n");
                i += 2;
            } else {
                out.push(chars[i]);
                i += 1;
            }
        }
        text = out;
    }

    text.replace('n', &n_value.to_string())
}

/// Minimal `+ - * / ( )` expression evaluator covering the subset of
/// javaluator's `DoubleEvaluator` the n-function grammar can produce.
fn evaluate_expression(expression: &str) -> Result<f64, String> {
    let chars: Vec<char> = expression.chars().collect();
    let mut pos = 0;
    let value = parse_sum(&chars, &mut pos)?;
    if pos != chars.len() {
        return Err(format!("trailing characters in expression: {expression}"));
    }
    Ok(value)
}

fn parse_sum(chars: &[char], pos: &mut usize) -> Result<f64, String> {
    let mut value = parse_product(chars, pos)?;
    loop {
        match peek_op(chars, *pos) {
            Some('+') => {
                *pos += 1;
                value += parse_product(chars, pos)?;
            }
            Some('-') => {
                *pos += 1;
                value -= parse_product(chars, pos)?;
            }
            _ => return Ok(value),
        }
    }
}

fn parse_product(chars: &[char], pos: &mut usize) -> Result<f64, String> {
    let mut value = parse_factor(chars, pos)?;
    loop {
        match peek_op(chars, *pos) {
            Some('*') => {
                *pos += 1;
                value *= parse_factor(chars, pos)?;
            }
            Some('/') => {
                *pos += 1;
                value /= parse_factor(chars, pos)?;
            }
            _ => return Ok(value),
        }
    }
}

fn parse_factor(chars: &[char], pos: &mut usize) -> Result<f64, String> {
    match chars.get(*pos) {
        Some('+') => {
            *pos += 1;
            parse_factor(chars, pos)
        }
        Some('-') => {
            *pos += 1;
            Ok(-parse_factor(chars, pos)?)
        }
        Some('(') => {
            *pos += 1;
            let value = parse_sum(chars, pos)?;
            if chars.get(*pos) != Some(&')') {
                return Err("missing closing parenthesis".into());
            }
            *pos += 1;
            Ok(value)
        }
        Some(c) if c.is_ascii_digit() || *c == '.' => {
            let start = *pos;
            while chars
                .get(*pos)
                .is_some_and(|c| c.is_ascii_digit() || *c == '.')
            {
                *pos += 1;
            }
            chars[start..*pos]
                .iter()
                .collect::<String>()
                .parse::<f64>()
                .map_err(|_| "invalid number".to_string())
        }
        _ => Err("expected a number, sign or '('".into()),
    }
}

fn peek_op(chars: &[char], pos: usize) -> Option<char> {
    match chars.get(pos) {
        Some(&c @ ('+' | '-' | '*' | '/')) => Some(c),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ranges_and_lists() {
        assert_eq!(parse_page_list("1,3,5-7", 10).unwrap(), vec![0, 2, 4, 5, 6]);
        assert_eq!(parse_page_list("all", 3).unwrap(), vec![0, 1, 2]);
        assert_eq!(parse_page_list("2-", 4).unwrap(), vec![1, 2, 3]);
        assert_eq!(parse_page_list("5-3", 10).unwrap(), Vec::<usize>::new());
        assert_eq!(parse_page_list("99", 10).unwrap(), Vec::<usize>::new());
        assert_eq!(parse_page_list("", 10).unwrap(), Vec::<usize>::new());
    }

    #[test]
    fn parses_n_functions() {
        assert_eq!(parse_page_list("2n", 5).unwrap(), vec![1, 3]);
        assert_eq!(parse_page_list("2n+1", 5).unwrap(), vec![2, 4]);
        assert_eq!(parse_page_list("n", 3).unwrap(), vec![0, 1, 2]);
        assert_eq!(parse_page_list("no", 5).unwrap(), Vec::<usize>::new());
        assert_eq!(parse_page_list("n+", 5).unwrap(), Vec::<usize>::new());
    }

    #[test]
    fn validation_rejects_bad_selections() {
        assert!(require_valid_page_selection("1,3,5-9", 10).is_ok());
        assert!(require_valid_page_selection("all", 10).is_ok());
        assert!(require_valid_page_selection("", 10).is_ok());
        assert!(require_valid_page_selection("2n+1", 10).is_ok());
        assert!(require_valid_page_selection("no", 10).is_err());
        assert!(require_valid_page_selection("99", 10).is_err());
        assert!(require_valid_page_selection("5-3", 10).is_err());
        assert!(require_valid_page_selection("1,,", 10).is_ok());
        // Out-of-range tokens are tolerated while something else matches.
        assert!(require_valid_page_selection("1,99", 10).is_ok());
        assert!(require_valid_page_selection(",,", 10).is_err());
    }
}
