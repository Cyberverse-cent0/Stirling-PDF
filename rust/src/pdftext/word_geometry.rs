//! Word-level geometry and spacing. PDFium splits words on its own bounding
//! boxes, so both real spaces and edges are re-derived from the glyphs —
//! port of `WordGeometry.java`.

use super::line::Line;
use super::model::PdfWord;

/// Gap below this many average character widths reads as no space at all
/// (mid-word split).
pub const NO_SPACE_GAP: f32 = 0.30;

/// Punctuation that binds to the words on both sides. Closing words up is
/// only considered around one of these, because dropping a real space
/// corrupts the text.
const BINDING_MARKS: &str = "'’ʼ´`-‐‑";

/// True for a lone apostrophe or hyphen, as in `firm`, `'`, `s`.
pub fn is_binding_mark(word: &str) -> bool {
    let mut chars = word.chars();
    matches!((chars.next(), chars.next()), (Some(c), None) if BINDING_MARKS.contains(c))
}

const APOSTROPHES: &[char] = &['’', '\'', 'ʼ', '´', '`'];
/// Longest-first so `re`/`ve`/`ll` win over `s`/`t`/`d`/`m` at the same spot.
const CONTRACTION_SUFFIXES: &[&str] = &["re", "ve", "ll", "s", "t", "d", "m"];

/// Closes up an apostrophe the extractor left standing alone inside a cell,
/// e.g. `don ' t` → `don't`. English suffixes only: a spaced lone apostrophe
/// is an opening quote. Port of the `SPLIT_CONTRACTION` pattern
/// `(\p{L})\s*([’'ʼ´`])\s*(s|t|d|m|re|ve|ll)\b`.
pub fn rejoin_contractions(cell: &str) -> String {
    if !cell.contains(' ') {
        return cell.to_string();
    }
    let chars: Vec<char> = cell.chars().collect();
    let mut out = String::with_capacity(cell.len());
    let mut i = 0;
    while i < chars.len() {
        if let Some((apos, range, next)) = contraction_at(&chars, i) {
            out.push(chars[i]);
            out.push(chars[apos]);
            out.extend(chars[range].iter().copied());
            i = next;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// If a contraction starts at `i`, returns (apostrophe index, suffix char
/// range, next index); the caller emits letter + apostrophe + suffix.
fn contraction_at(chars: &[char], i: usize) -> Option<(usize, std::ops::Range<usize>, usize)> {
    if !chars[i].is_alphabetic() {
        return None;
    }
    let mut j = i + 1;
    while j < chars.len() && chars[j] == ' ' {
        j += 1;
    }
    if j >= chars.len() || !APOSTROPHES.contains(&chars[j]) {
        return None;
    }
    let apos = j;
    let mut k = apos + 1;
    while k < chars.len() && chars[k] == ' ' {
        k += 1;
    }
    for suffix in CONTRACTION_SUFFIXES {
        let suff: Vec<char> = suffix.chars().collect();
        if k + suff.len() <= chars.len()
            && chars[k..k + suff.len()] == suff[..]
            && (k + suff.len() == chars.len() || !chars[k + suff.len()].is_alphanumeric())
        {
            // Suffix must start on a letter boundary for `re`/`ve`/`ll`;
            // single letters always qualify.
            return Some((apos, k..k + suff.len(), k + suff.len()));
        }
    }
    None
}

/// Mean glyph width across two words, used to size the space test.
fn word_char_width(a: &PdfWord, b: &PdfWord) -> f32 {
    let mut width = 0.0f32;
    let mut chars = 0usize;
    for w in [a, b] {
        for c in &w.chars {
            if !c.is_whitespace() && !c.is_newline() {
                width += c.width;
                chars += 1;
            }
        }
    }
    if chars == 0 {
        0.0
    } else {
        width / chars as f32
    }
}

/// Right edge of a word's non-whitespace glyphs, falling back to its box.
pub fn right_edge(w: &PdfWord) -> f32 {
    let mut edge = f32::NEG_INFINITY;
    for c in &w.chars {
        if !c.is_whitespace() && !c.is_newline() {
            edge = edge.max(c.x + c.width);
        }
    }
    if edge == f32::NEG_INFINITY {
        w.x + w.width
    } else {
        edge
    }
}

/// Left edge of a word's non-whitespace glyphs, falling back to its box.
pub fn left_edge(w: &PdfWord) -> f32 {
    let mut edge = f32::INFINITY;
    for c in &w.chars {
        if !c.is_whitespace() && !c.is_newline() {
            edge = edge.min(c.x);
        }
    }
    if edge == f32::INFINITY {
        w.x
    } else {
        edge
    }
}

/// True when two words of a cell are far enough apart to be separated by
/// a space; punctuation set tight against its neighbour arrives as its own
/// word.
pub fn separated(previous: Option<&PdfWord>, current: &PdfWord) -> bool {
    let Some(prev) = previous else {
        return true;
    };
    let gap = left_edge(current) - right_edge(prev);
    if gap < 0.0 {
        // Overlapping or out of order: keep the space.
        return true;
    }
    let char_width = word_char_width(prev, current);
    char_width <= 0.0 || gap >= char_width * NO_SPACE_GAP
}

/// Mean advance per stripped character across every word of `rows`, like
/// `WordGeometry.averageCharWidth`.
pub fn average_char_width<L: std::borrow::Borrow<Line>>(rows: &[L]) -> f32 {
    let mut total_width = 0.0f64;
    let mut total_chars = 0usize;
    for l in rows {
        let line: &Line = l.borrow();
        for w in line.words() {
            total_width += w.width as f64;
            total_chars += w.stripped().chars().count().max(1);
        }
    }
    if total_chars == 0 {
        6.0
    } else {
        (total_width / total_chars as f64) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::PdfChar;
    use super::*;

    fn word(text: &str, x: f32, w: f32) -> PdfWord {
        let chars: Vec<PdfChar> = text
            .chars()
            .enumerate()
            .map(|(i, c)| PdfChar {
                ch: c,
                x: x + i as f32 * (w / text.chars().count() as f32),
                y: 0.0,
                width: w / text.chars().count() as f32,
                height: 10.0,
                font_name: String::new(),
                font_size: 10.0,
            })
            .collect();
        PdfWord {
            chars,
            x,
            y: 0.0,
            width: w,
            height: 10.0,
        }
    }

    #[test]
    fn binding_mark_is_single_punctuation() {
        assert!(is_binding_mark("'"));
        assert!(is_binding_mark("-"));
        assert!(!is_binding_mark("''"));
        assert!(!is_binding_mark("s'"));
        assert!(!is_binding_mark("word"));
    }

    #[test]
    fn contractions_rejoin() {
        assert_eq!(rejoin_contractions("don ' t"), "don't");
        assert_eq!(rejoin_contractions("it ' s"), "it's");
        assert_eq!(rejoin_contractions("we ' re"), "we're");
        assert_eq!(rejoin_contractions("I ' ll"), "I'll");
        // No spaces: untouched.
        assert_eq!(rejoin_contractions("don't"), "don't");
        // Spaced apostrophe not followed by a contraction suffix: untouched.
        assert_eq!(rejoin_contractions("rock ' n' roll"), "rock ' n' roll");
    }

    #[test]
    fn separation_uses_glyph_gap() {
        // "ab" then "cd" 1pt apart: gap < 0.3 * avg char width → joined.
        let a = word("ab", 0.0, 10.0);
        let b = word("cd", 11.0, 10.0);
        assert!(!separated(Some(&a), &b));
        // 8pt gap with ~5pt chars → separated.
        let c = word("cd", 18.0, 10.0);
        assert!(separated(Some(&a), &c));
        // First word always "separated" (no predecessor).
        assert!(separated(None, &a));
    }

    #[test]
    fn edges_ignore_whitespace_chars() {
        let mut w = word("ab", 0.0, 10.0);
        w.chars.push(PdfChar {
            ch: ' ',
            x: 50.0,
            y: 0.0,
            width: 5.0,
            height: 10.0,
            font_name: String::new(),
            font_size: 10.0,
        });
        assert_eq!(right_edge(&w), 10.0);
        assert_eq!(left_edge(&w), 0.0);
    }
}
