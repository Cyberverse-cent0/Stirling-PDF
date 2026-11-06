//! Renders a resolved cell grid as a GitHub-Flavored Markdown table.
//! Port of `GfmTable.java`.

use super::markdown_text::MarkdownText;

pub fn render(rows: &[Vec<String>], cols: usize) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let mut widths = vec![3usize; cols];
    for row in rows {
        for c in 0..cols {
            if c < row.len() {
                widths[c] = widths[c].max(utf16_len(&escape_cell(&row[c])));
            }
        }
    }
    let mut sb = String::new();
    sb.push_str(&build_gfm_row(&rows[0], &widths, cols));
    sb.push('\n');
    sb.push('|');
    for width in widths.iter().take(cols) {
        sb.push('-');
        sb.push_str(&"-".repeat(*width));
        sb.push('-');
        sb.push('|');
    }
    for row in rows.iter().skip(1) {
        sb.push('\n');
        sb.push_str(&build_gfm_row(row, &widths, cols));
    }
    sb
}

fn build_gfm_row(row: &[String], widths: &[usize], cols: usize) -> String {
    let mut sb = String::from("|");
    for c in 0..cols {
        let cell = if c < row.len() {
            escape_cell(&row[c])
        } else {
            String::new()
        };
        sb.push(' ');
        sb.push_str(&pad_right(&cell, widths[c]));
        sb.push(' ');
        sb.push('|');
    }
    sb
}

fn escape_cell(cell: &str) -> String {
    // Cell content is inline context: escape inline markdown (including the column delimiter)
    // but not leading block markers, which have no meaning inside a table cell.
    MarkdownText::escape_markdown_inline(cell)
}

/// Java `String.length()` counts UTF-16 code units; padding must match it
/// or astral characters shift the column edges.
fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

fn pad_right(s: &str, width: usize) -> String {
    let len = utf16_len(s);
    if len >= width {
        s.to_string()
    } else {
        format!("{}{}", s, " ".repeat(width - len))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_rows_render_nothing() {
        assert_eq!(render(&[], 3), "");
    }

    #[test]
    fn single_row_gets_header_and_separator() {
        let out = render(&[vec!["a".into(), "b".into()]], 2);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "| a   | b   |");
        // Minimum column width is 3, so the rule is 3 + 2 border dashes.
        assert_eq!(lines[1], "|-----|-----|");
    }

    #[test]
    fn columns_pad_to_widest_cell() {
        let out = render(
            &[
                vec!["h".into(), "x".into()],
                vec!["longer".into(), "y".into()],
            ],
            2,
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "| h      | x   |");
        assert_eq!(lines[1], "|--------|-----|");
        assert_eq!(lines[2], "| longer | y   |");
    }

    #[test]
    fn pipes_and_markdown_chars_are_escaped() {
        let out = render(&[vec!["a|b".into(), "*em*".into()]], 2);
        assert!(out.contains("a\\|b"));
        assert!(out.contains("\\*em\\*"));
        // The escaped pipe must not split the column.
        assert_eq!(out.lines().count(), 2);
    }

    #[test]
    fn short_rows_pad_missing_cells() {
        let out = render(&[vec!["only".into()]], 2);
        let header = out.lines().next().unwrap();
        assert!(header.ends_with('|'));
        assert_eq!(header.matches('|').count(), 3);
    }

    #[test]
    fn astral_chars_pad_by_utf16_length() {
        // U+1F600 is one char but two UTF-16 units: Java's String.length()
        // counts 2, so the padding must too.
        let out = render(&[vec!["\u{1F600}".into()]], 1);
        let header = out.lines().next().unwrap();
        // width = max(3, 2) = 3; cell occupies 2 utf16 units + 1 pad space.
        assert!(header.contains("\u{1F600}  "));
    }
}
