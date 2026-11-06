//! Markdown escaping and paragraph helpers — port of `MarkdownText.java`.
//! Escaping is defence-in-depth so literal PDF text reaches the Markdown
//! verbatim; it is not a substitute for safe rendering downstream.

/// `Character.isWhitespace`: the Unicode separators except the no-break
/// spaces, plus the C0 controls Java counts. `str::trim` would also strip
/// U+00A0, which Java keeps as text.
pub(crate) fn is_java_whitespace(c: char) -> bool {
    matches!(c, '\u{9}'..='\u{D}' | '\u{1C}'..='\u{1F}' | ' ')
        || (is_unicode_separator(c) && !matches!(c, '\u{A0}' | '\u{2007}' | '\u{202F}'))
}

fn is_unicode_separator(c: char) -> bool {
    matches!(
        c as u32,
        0x20 | 0xA0 | 0x1680 | 0x2000..=0x200A | 0x2028 | 0x2029 | 0x202F | 0x205F | 0x3000
    )
}

/// Java `String.strip()`.
pub(crate) fn strip(s: &str) -> &str {
    s.trim_matches(is_java_whitespace)
}

/// Java `String.isBlank()`.
pub(crate) fn is_blank(s: &str) -> bool {
    strip(s).is_empty()
}

/// `\s` in the Java regexes ported here: ASCII whitespace only — notably
/// not U+00A0, which Java `\s` excludes and the pipeline treats as a glyph.
pub(crate) fn is_regex_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0B' | '\x0C' | '\r')
}

/// `Character.isDigit`: the closest std check is `char::is_numeric`, which
/// also admits the letter-like and symbol numerics Java rejects.
pub(crate) fn is_digit(c: char) -> bool {
    c.is_numeric()
}

/// Whitespace-separated token count, like `text.strip().split("\\s+")`.
pub fn word_count(text: &str) -> usize {
    let t = strip(text);
    if t.is_empty() {
        return 0;
    }
    t.split(|c: char| is_regex_space(c))
        .filter(|s| !s.is_empty())
        .count()
}

/// Escapes Markdown control characters so extracted text is emitted
/// literally.
pub fn escape_markdown(text: &str) -> String {
    if text.is_empty() {
        return text.to_string();
    }
    let inline = escape_markdown_inline(text);
    escape_leading_block_marker(&inline, text)
}

/// Escapes inline-significant Markdown characters anywhere in the string.
pub fn escape_markdown_inline(text: &str) -> String {
    let mut sb = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        match c {
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '|' | '~' => {
                sb.push('\\');
                sb.push(c);
            }
            _ => sb.push(c),
        }
    }
    sb
}

/// Escapes markers significant only at line start: `#`, `-`, `+` and
/// ordered-list numbers. `original` is unescaped, so positions line up.
fn escape_leading_block_marker(escaped: &str, original: &str) -> String {
    let orig: Vec<char> = original.chars().collect();
    let c0 = orig[0];
    if c0 == '#' || c0 == '-' || c0 == '+' {
        return format!("\\{escaped}");
    }
    let mut i = 0;
    while i < orig.len() && is_digit(orig[i]) {
        i += 1;
    }
    if i > 0 && i < orig.len() {
        let delim = orig[i];
        if delim == '.' || delim == ')' {
            // The leading digits are never inline-escaped, so the first `i`
            // chars of `escaped` are identical to `original`'s.
            let esc: Vec<char> = escaped.chars().collect();
            let mut out = String::with_capacity(escaped.len() + 1);
            for (idx, c) in esc.iter().enumerate() {
                if idx == i {
                    out.push('\\');
                }
                out.push(*c);
            }
            return out;
        }
    }
    escaped.to_string()
}

/// `s.strip().replaceAll("\\s+", " ")`.
pub fn normalise_space(s: &str) -> String {
    let t = strip(s);
    let mut out = String::with_capacity(t.len());
    let mut in_space = false;
    for c in t.chars() {
        if is_regex_space(c) {
            if !in_space {
                out.push(' ');
                in_space = true;
            }
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out
}

/// Appends the escaped paragraph and clears the buffer, like
/// `MarkdownText.flushParagraph`.
pub fn flush_paragraph(para: &mut String, out: &mut Vec<String>) {
    if !para.is_empty() {
        out.push(escape_markdown(para));
        para.clear();
    }
}

pub fn ends_with_sentence_punctuation(s: &str) -> bool {
    matches!(
        s.chars().last(),
        Some('.') | Some('?') | Some('!') | Some(':')
    )
}

/// Java-style facade over this module's free functions; the ported pipeline
/// calls the Java `MarkdownText` helper as a static class.
#[allow(dead_code)]
pub struct MarkdownText;

#[allow(dead_code)]
impl MarkdownText {
    pub fn is_java_whitespace(c: char) -> bool {
        is_java_whitespace(c)
    }
    pub fn strip(s: &str) -> &str {
        strip(s)
    }
    pub fn is_blank(s: &str) -> bool {
        is_blank(s)
    }
    pub fn is_regex_space(c: char) -> bool {
        is_regex_space(c)
    }
    pub fn is_digit(c: char) -> bool {
        is_digit(c)
    }
    pub fn word_count(text: &str) -> usize {
        word_count(text)
    }
    pub fn escape_markdown(text: &str) -> String {
        escape_markdown(text)
    }
    pub fn escape_markdown_inline(text: &str) -> String {
        escape_markdown_inline(text)
    }
    pub fn normalise_space(s: &str) -> String {
        normalise_space(s)
    }
    pub fn flush_paragraph(para: &mut String, out: &mut Vec<String>) {
        flush_paragraph(para, out)
    }
    pub fn ends_with_sentence_punctuation(s: &str) -> bool {
        ends_with_sentence_punctuation(s)
    }
}
