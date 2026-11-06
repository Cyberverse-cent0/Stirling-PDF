//! Types shared across the pipeline stages, mirroring the small Java
//! records/enums that several classes pass between each other.

/// An element plus the 1-based pages it came from; `page_end` widens when a
/// page break is joined. Java: `record Element(Object value, int pageStart,
/// int pageEnd)` — generic here so each stage keeps its own payload type.
#[derive(Debug, Clone)]
pub struct Element<T> {
    pub value: T,
    pub page_start: usize,
    pub page_end: usize,
}

impl<T> Element<T> {
    pub fn new(value: T, page: usize) -> Self {
        Self {
            value,
            page_start: page,
            page_end: page,
        }
    }
}

/// How much of a block's row structure the page itself drew; stronger
/// evidence means weaker false-positive guards.
/// Java: `enum RowSource`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowSource {
    /// Rows inferred from word geometry alone; nothing on the page confirms
    /// a table.
    Words,
    /// Rows sit inside a region fenced by drawn rules, but the rules do not
    /// delimit them.
    RuleBounded,
    /// Every row boundary is a drawn rule running the table's own width.
    Lattice,
}

impl RowSource {
    pub fn rule_confirmed(self) -> bool {
        self != RowSource::Words
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn element_starts_with_matching_page_range() {
        let e = Element::new("block".to_string(), 3);
        assert_eq!(e.page_start, 3);
        assert_eq!(e.page_end, 3);
        assert_eq!(e.value, "block");
    }

    #[test]
    fn only_words_source_is_unconfirmed() {
        assert!(!RowSource::Words.rule_confirmed());
        assert!(RowSource::RuleBounded.rule_confirmed());
        assert!(RowSource::Lattice.rule_confirmed());
    }
}
