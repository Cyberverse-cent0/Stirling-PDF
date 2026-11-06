//! FigureTracker — cross-page consistency checker for named figures.
//! Port of `validators/figures.py`: labels are normalised, values compared
//! within tolerance, one Discrepancy per conflict against the first-seen
//! (canonical) value.

use std::sync::LazyLock;

use regex::Regex;
use rust_decimal::Decimal;

use crate::contracts::ledger::{Discrepancy, DiscrepancyKind, Severity};

struct FigureRecord {
    /// Normalised label (the ledger key).
    label: String,
    value: Decimal,
    page: i64,
    raw: String,
}

// Strip punctuation that varies between contexts ("revenue:" vs "revenue —")
static LABEL_NOISE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[:\-—\s]+").unwrap());

fn normalise_label(label: &str) -> String {
    LABEL_NOISE
        .replace_all(&label.to_lowercase(), " ")
        .trim()
        .to_string()
}

pub struct FigureTracker {
    tolerance: Decimal,
    /// Insertion-ordered like Python's dict — `conflicts()` emits
    /// discrepancies in first-seen label order.
    ledger: Vec<(String, Vec<FigureRecord>)>,
}

impl FigureTracker {
    pub fn new(tolerance: Decimal) -> Self {
        Self {
            tolerance,
            ledger: Vec::new(),
        }
    }

    pub fn record(&mut self, label: &str, value: Decimal, page: i64, raw: &str) {
        let key = normalise_label(label);
        let record = FigureRecord {
            label: key.clone(),
            value,
            page,
            raw: raw.to_string(),
        };
        match self.ledger.iter_mut().find(|(k, _)| *k == key) {
            Some((_, records)) => records.push(record),
            None => self.ledger.push((key, vec![record])),
        }
    }

    /// One Discrepancy per label whose later sightings differ from the
    /// canonical (first-seen) value by more than tolerance. O(n) per label.
    pub fn conflicts(&self) -> Vec<Discrepancy> {
        let mut discrepancies = Vec::new();
        for (label, records) in &self.ledger {
            if records.len() < 2 {
                continue;
            }
            let canonical = &records[0];
            for other in &records[1..] {
                if (canonical.value - other.value).abs() > self.tolerance {
                    discrepancies.push(Discrepancy {
                        page: other.page,
                        kind: DiscrepancyKind::Consistency,
                        severity: Severity::Warning,
                        description: format!(
                            "\"{label}\" stated as {} on page {} but {} on page {}",
                            canonical.raw,
                            canonical.page + 1,
                            other.raw,
                            other.page + 1
                        ),
                        stated: other.raw.clone(),
                        expected: canonical.raw.clone(),
                        context: format!(
                            "First seen: page {} | Later: page {}",
                            canonical.page + 1,
                            other.page + 1
                        ),
                    });
                }
            }
        }
        discrepancies
    }

    pub fn entry_count(&self) -> usize {
        self.ledger.iter().map(|(_, v)| v.len()).sum()
    }
}
