/// Prefixes spreadsheet formula-like values so they are read as literal text.
pub fn sanitize_cell(value: Option<&str>) -> Option<String> {
    let value = value?;
    if value.is_empty() {
        return Some(String::new());
    }

    let candidate = strip_leading_java_whitespace(value);
    if !matches!(candidate.chars().next(), Some('=' | '+' | '-' | '@'))
        || is_plain_number(candidate)
    {
        return Some(value.to_owned());
    }

    Some(format!("'{value}"))
}

/// Sanitizes every cell while preserving null cells and null rows.
pub fn sanitize_row(row: Option<&[Option<String>]>) -> Option<Vec<Option<String>>> {
    row.map(|cells| {
        cells
            .iter()
            .map(|cell| sanitize_cell(cell.as_deref()))
            .collect()
    })
}

fn strip_leading_java_whitespace(value: &str) -> &str {
    let Some((index, _)) = value
        .char_indices()
        .find(|(_, character)| !is_java_whitespace(*character))
    else {
        return "";
    };
    &value[index..]
}

fn is_java_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'..='\u{000D}'
            | '\u{001C}'..='\u{0020}'
            | '\u{1680}'
            | '\u{2000}'..='\u{2006}'
            | '\u{2008}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{205F}'
            | '\u{3000}'
    )
}

fn is_plain_number(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = usize::from(matches!(bytes.first(), Some(b'-' | b'+')));

    if consume_digits(bytes, &mut index) == 0 {
        return false;
    }

    while matches!(bytes.get(index), Some(b'.' | b',')) {
        index += 1;
        if consume_digits(bytes, &mut index) == 0 {
            return false;
        }
    }

    index == bytes.len()
}

fn consume_digits(bytes: &[u8], index: &mut usize) -> usize {
    let start = *index;
    while matches!(bytes.get(*index), Some(byte) if byte.is_ascii_digit()) {
        *index += 1;
    }
    *index - start
}

#[cfg(test)]
mod tests {
    use super::{sanitize_cell, sanitize_row};

    #[test]
    fn prefixes_values_a_spreadsheet_would_evaluate() {
        let values = [
            "=SUM(A1:A2)",
            "+HYPERLINK(\"http://example.com\")",
            "-WEBSERVICE(\"http://example.com\")",
            "@SUM(1+1)",
            " =SUM(A1:A2)",
            "\t=SUM(A1:A2)",
            "\r@SUM(1+1)",
        ];

        for value in values {
            assert_eq!(sanitize_cell(Some(value)), Some(format!("'{value}")));
        }
    }

    #[test]
    fn leaves_plain_text_and_numbers_unchanged() {
        let values = [
            "Alice",
            "1234",
            "-42",
            "-1,234.56",
            "+0.5",
            " -42",
            "total = 12",
            "a=b",
            "   ",
        ];

        for value in values {
            assert_eq!(sanitize_cell(Some(value)), Some(value.to_owned()));
        }
    }

    #[test]
    fn prefixes_non_numeric_formula_triggers() {
        assert_eq!(sanitize_cell(Some("-1,")), Some("'-1,".to_owned()));
        assert_eq!(sanitize_cell(Some("+1.")), Some("'+1.".to_owned()));
    }

    #[test]
    fn preserves_nonbreaking_space_before_formula_trigger() {
        assert_eq!(
            sanitize_cell(Some("\u{00A0}=SUM(A1:A2)")),
            Some("\u{00A0}=SUM(A1:A2)".to_owned())
        );
    }

    #[test]
    fn preserves_null_and_empty_values() {
        assert_eq!(sanitize_cell(None), None);
        assert_eq!(sanitize_cell(Some("")), Some(String::new()));
        assert_eq!(sanitize_row(None), None);
    }

    #[test]
    fn sanitizes_every_cell_in_a_record() {
        let row = vec![
            Some("=SUM(A1:A2)".to_owned()),
            Some("Alice".to_owned()),
            None,
            Some("-42".to_owned()),
        ];

        assert_eq!(
            sanitize_row(Some(&row)),
            Some(vec![
                Some("'=SUM(A1:A2)".to_owned()),
                Some("Alice".to_owned()),
                None,
                Some("-42".to_owned()),
            ])
        );
    }
}
