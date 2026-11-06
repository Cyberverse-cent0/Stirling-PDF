//! ClaimLedger — accumulator for claims keyed by canonical subject.
//! Port of `stirling.agents.contradiction.validators.ledger`: groups
//! [`Claim`] records by a normalised subject string and emits buckets
//! (subjects with >= 2 claims) for the contradiction detector. The lexical
//! normalisation is a defensive default; [`ClaimLedger::rekey_with_canonical`]
//! replaces keys with LLM-derived canonical groupings.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

use crate::contracts::contradiction::Claim;

// Strip punctuation that varies between contexts ("deadline:" vs "deadline —").
static LABEL_NOISE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[:\-—_,.;!?\s]+").unwrap());
// Common English articles and demonstratives that often pad subjects.
static ARTICLES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:the|a|an|this|that|these|those)\b").unwrap()
});

/// Lexical key for grouping subjects with no LLM help: lowercase, strip
/// articles/demonstratives, collapse punctuation/whitespace to single spaces.
fn normalise_subject(subject: &str) -> String {
    let lowered = subject.to_lowercase();
    let no_articles = ARTICLES.replace_all(&lowered, " ");
    LABEL_NOISE.replace_all(&no_articles, " ").trim().to_string()
}

pub struct ClaimLedger {
    /// Insertion-ordered keys; `records` holds the buckets.
    key_order: Vec<String>,
    records: HashMap<String, Vec<Claim>>,
}

impl ClaimLedger {
    pub fn new() -> Self {
        Self {
            key_order: Vec::new(),
            records: HashMap::new(),
        }
    }

    /// Register a claim under its lexical-normalised subject key. Claims
    /// whose subject normalises to empty are dropped — the detector has no
    /// way to bucket them usefully.
    pub fn record(&mut self, claim: Claim) {
        let key = normalise_subject(&claim.subject);
        if key.is_empty() {
            tracing::debug!(subject = %claim.subject, "[contradiction] dropping claim with empty subject");
            return;
        }
        if !self.records.contains_key(&key) {
            self.key_order.push(key.clone());
        }
        self.records.entry(key).or_default().push(claim);
    }

    /// Re-group every claim under the canonical subject from `mapping`
    /// (raw subject phrase -> canonical phrase). Subjects missing from the
    /// mapping fall back to lexical normalisation so no claim is dropped.
    pub fn rekey_with_canonical(&mut self, mapping: &HashMap<String, String>) {
        let flattened: Vec<Claim> = self
            .records
            .values()
            .flat_map(|bucket| bucket.iter().cloned())
            .collect();
        let mut new_records: HashMap<String, Vec<Claim>> = HashMap::new();
        let mut new_order: Vec<String> = Vec::new();

        for claim in flattened {
            let canonical = mapping
                .get(&claim.subject)
                .or_else(|| mapping.get(&normalise_subject(&claim.subject)));
            let key = match canonical {
                Some(c) if !c.trim().is_empty() => normalise_subject(c),
                _ => normalise_subject(&claim.subject),
            };
            if key.is_empty() {
                continue;
            }
            if !new_records.contains_key(&key) {
                new_order.push(key.clone());
            }
            new_records.entry(key).or_default().push(claim);
        }

        self.records = new_records;
        self.key_order = new_order;
    }

    /// Buckets with at least two claims, in first-seen key order.
    pub fn buckets(&self) -> Vec<(String, &[Claim])> {
        self.key_order
            .iter()
            .filter_map(|key| {
                let claims = self.records.get(key)?;
                (claims.len() >= 2).then(|| (key.clone(), claims.as_slice()))
            })
            .collect()
    }

    /// The raw subject strings seen across all recorded claims, first-seen
    /// order — the input to subject canonicalisation.
    pub fn unique_subjects(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut unique = Vec::new();
        for key in &self.key_order {
            if let Some(bucket) = self.records.get(key) {
                for claim in bucket {
                    if seen.insert(claim.subject.clone()) {
                        unique.push(claim.subject.clone());
                    }
                }
            }
        }
        unique
    }
}

impl Default for ClaimLedger {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::contradiction::{AnchorQuality, ClaimPolarity};

    fn claim(subject: &str, page: i64) -> Claim {
        Claim {
            page,
            subject: subject.to_string(),
            polarity: ClaimPolarity::Assert,
            text: "t".to_string(),
            quote: "q".to_string(),
            anchor_quality: AnchorQuality::Verbatim,
            file_name: None,
        }
    }

    #[test]
    fn normalise_strips_articles_and_punctuation() {
        assert_eq!(normalise_subject("The Project Deadline:"), "project deadline");
        assert_eq!(normalise_subject("a deadline — maybe"), "deadline maybe");
        assert_eq!(normalise_subject("   "), "");
    }

    #[test]
    fn buckets_require_two_claims() {
        let mut ledger = ClaimLedger::new();
        ledger.record(claim("deadline", 1));
        assert!(ledger.buckets().is_empty());
        ledger.record(claim("The deadline", 2));
        assert_eq!(ledger.buckets().len(), 1);
        assert_eq!(ledger.buckets()[0].1.len(), 2);
    }

    #[test]
    fn rekey_uses_canonical_mapping() {
        let mut ledger = ClaimLedger::new();
        ledger.record(claim("project deadline", 1));
        ledger.record(claim("the deadline for the project", 2));
        ledger.record(claim("budget", 3));
        ledger.record(claim("budget", 4));
        let mut mapping = HashMap::new();
        mapping.insert("project deadline".to_string(), "deadline".to_string());
        mapping.insert("the deadline for the project".to_string(), "deadline".to_string());
        ledger.rekey_with_canonical(&mapping);
        let buckets = ledger.buckets();
        // deadline bucket now has 2 claims; budget bucket has 2 — both >= 2.
        assert_eq!(buckets.len(), 2);
        let deadline = buckets.iter().find(|(k, _)| k == "deadline").unwrap();
        assert_eq!(deadline.1.len(), 2);
    }

    #[test]
    fn empty_subject_claims_are_dropped() {
        let mut ledger = ClaimLedger::new();
        ledger.record(claim("the a an", 1));
        assert!(ledger.unique_subjects().is_empty());
    }
}
