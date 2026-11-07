//! veraPDF-backed standard detection and validation for the security
//! endpoints (`verify-pdf`, `validate-compliance`).
//!
//! Java's `VeraPDFService` uses the veraPDF *library*; the Rust port
//! drives the same engine through its CLI (`verapdf --format text
//! [--flavour F]`), which reports per-flavour compliance. Declared
//! standards are read from the document's XMP metadata — the same
//! signal `PDFAParser.getFlavours()` consumes: `pdfaid:part` /
//! `pdfaid:conformance` for PDF/A, `pdfuaid:part` for PDF/UA, and the
//! WTPDF namespace for WTPDF.
//!
//! Approximations vs the Java library path, all documented at the call
//! sites: `standard`/`validationProfile` ids use the CLI flavour token
//! (`"2b"`, `"ua-1"`) rather than `PDFAFlavour.getId()`; `failures` are
//! parsed from the CLI's text report, so `ruleId`/`location` granularity
//! depends on the report layout; WTPDF has no CLI flavour and is
//! reported declared-but-unvalidated.

use lopdf::{Document, Object};

use crate::handlers::convert::{run_verapdf, VeraOutcome};

/// Which specification family a detected flavour belongs to — mirrors
/// `PDFAFlavour.SpecificationFamily`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    PdfA,
    PdfUa,
    Wtpdf,
}

/// One declared standard, equivalent to a `PDFAFlavour` entry.
#[derive(Clone, Debug)]
pub struct DetectedFlavour {
    pub family: Family,
    /// veraPDF CLI `--flavour` token (`"1b"`, `"2u"`, `"ua-1"`, …);
    /// `None` for WTPDF, which the CLI cannot validate.
    pub flavour: Option<String>,
    /// `PDFAFlavour.getId()` approximation — the CLI token.
    pub standard_id: String,
    /// `getStandardName` display form: `PDF/A-2b`, `PDF/UA-1`, `WTPDF 2`.
    pub standard_name: String,
    pub declared_pdfa: bool,
}

/// The document's declared standards plus whether a usable PDF/A
/// identification (`pdfaid:part` + `pdfaid:conformance`) is present —
/// Java's `hasValidPdfaMetadata` gate.
pub struct Detection {
    pub flavours: Vec<DetectedFlavour>,
    pub has_pdfa_metadata: bool,
}

fn xmp_metadata(doc: &Document) -> Option<String> {
    let catalog = doc.catalog().ok()?;
    let metadata = catalog.get(b"Metadata").ok()?;
    let (_, resolved) = doc.dereference(metadata).ok()?;
    let Object::Stream(stream) = resolved else {
        return None;
    };
    let bytes = stream
        .decompressed_content()
        .unwrap_or_else(|_| stream.content.clone());
    String::from_utf8(bytes).ok()
}

/// `part="N"` attribute value inside an XMP element, either quote style.
fn xmp_attr(xmp: &str, element: &str, attr: &str) -> Option<String> {
    let needle = format!("{element}:{attr}");
    // The same `prefix:name` text appears on elements (`<pdfaid:part>`)
    // before the attribute (`pdfaid:part="2"`); only `=` followers count.
    let mut offset = 0;
    while let Some(found) = xmp[offset..].find(&needle) {
        let pos = offset + found + needle.len();
        offset = pos;
        let Some(rest) = xmp[pos..].trim_start().strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start();
        let quote = rest.chars().next()?;
        if quote != '"' && quote != '\'' {
            return None;
        }
        let end = rest[1..].find(quote)? + 1;
        return Some(rest[1..end].to_string());
    }
    None
}

fn xmp_has(xmp: &str, needle: &str) -> bool {
    xmp.contains(needle)
}

fn pdfa_level(part: u32, conformance: &str) -> Option<String> {
    // PDF/A-1 has only a/b; 2 and 3 add u. Anything else is not a flavour.
    let level = conformance.to_ascii_lowercase();
    match (part, level.as_str()) {
        (1, "a") => Some("1a".into()),
        (1, "b") => Some("1b".into()),
        (2, "a") => Some("2a".into()),
        (2, "b") => Some("2b".into()),
        (2, "u") => Some("2u".into()),
        (3, "a") => Some("3a".into()),
        (3, "b") => Some("3b".into()),
        (3, "u") => Some("3u".into()),
        _ => None,
    }
}

fn pdfa_name(flavour: &str) -> String {
    let level = flavour.chars().last().unwrap_or('b').to_ascii_uppercase();
    format!("PDF/A-{}{}", &flavour[..flavour.len() - 1], level)
}

/// `VeraPDFService.validatePDF`'s detection half: the declared flavours
/// a document's XMP announces, in Java's check order.
pub fn detect(pdf: &[u8]) -> Detection {
    let mut flavours = Vec::new();
    let mut has_pdfa_metadata = false;
    let xmp = Document::load_mem(pdf)
        .ok()
        .and_then(|doc| xmp_metadata(&doc));
    if let Some(xmp) = xmp {
        if let (Some(part), Some(conf)) = (
            xmp_attr(&xmp, "pdfaid", "part"),
            xmp_attr(&xmp, "pdfaid", "conformance"),
        ) {
            has_pdfa_metadata = true;
            if let Ok(part) = part.parse::<u32>() {
                if let Some(flavour) = pdfa_level(part, &conf) {
                    flavours.push(DetectedFlavour {
                        family: Family::PdfA,
                        standard_id: flavour.clone(),
                        standard_name: pdfa_name(&flavour),
                        flavour: Some(flavour),
                        declared_pdfa: true,
                    });
                }
            }
        }
        if let Some(part) = xmp_attr(&xmp, "pdfuaid", "part") {
            let flavour = match part.as_str() {
                "1" => "ua-1",
                "2" => "ua-2",
                _ => "",
            };
            if !flavour.is_empty() {
                flavours.push(DetectedFlavour {
                    family: Family::PdfUa,
                    standard_id: flavour.into(),
                    standard_name: format!("PDF/UA-{part}"),
                    flavour: Some(flavour.into()),
                    declared_pdfa: false,
                });
            }
        }
        if xmp_has(&xmp, "wtpdf.org") {
            flavours.push(DetectedFlavour {
                family: Family::Wtpdf,
                standard_id: "wtpdf".into(),
                standard_name: "WTPDF".into(),
                flavour: None,
                declared_pdfa: false,
            });
        }
    }
    Detection {
        flavours,
        has_pdfa_metadata,
    }
}

/// One failed test clause parsed from the CLI's text report.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Issue {
    #[serde(rename = "ruleId", skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    pub message: String,
    pub location: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub specification: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clause: Option<String>,
    #[serde(rename = "testNumber", skip_serializing_if = "Option::is_none")]
    pub test_number: Option<String>,
}

/// Outcome of one `verapdf` CLI run.
pub struct Report {
    pub compliant: bool,
    pub issues: Vec<Issue>,
    /// `true` when the CLI is not installed — callers decide whether that
    /// is fatal (Java's library is always present, so this is a Rust-only
    /// deployment gap).
    pub unavailable: bool,
    pub error: Option<String>,
}

/// `veraPDF --format text` verdict for `pdf` under `flavour`
/// (`None` = no `--flavour`, structural validation only).
pub fn validate(pdf: &[u8], flavour: Option<&str>) -> Report {
    let dir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(e) => {
            return Report {
                compliant: false,
                issues: Vec::new(),
                unavailable: false,
                error: Some(format!("tempdir: {e}")),
            }
        }
    };
    match run_verapdf(pdf, flavour, dir.path()) {
        VeraOutcome::Compliant => Report {
            compliant: true,
            issues: Vec::new(),
            unavailable: false,
            error: None,
        },
        VeraOutcome::NonCompliant(text) => Report {
            compliant: false,
            issues: parse_failed_clauses(&text),
            unavailable: false,
            error: None,
        },
        VeraOutcome::Unavailable => Report {
            compliant: false,
            issues: Vec::new(),
            unavailable: true,
            error: None,
        },
        VeraOutcome::Error(detail) => Report {
            compliant: false,
            issues: Vec::new(),
            unavailable: false,
            error: Some(detail),
        },
    }
}

/// Extracts failed test clauses from veraPDF's text report. The CLI
/// prints blocks like `Clause: 6.2.10.2` / `Test number: 4` /
/// `Status: failed` / `Message: …`; the parser tolerates either the
/// `Clause:` label or a bare `6.2.10.2:` header and only emits blocks
/// whose status is failed.
fn parse_failed_clauses(report: &str) -> Vec<Issue> {
    let mut issues = Vec::new();
    let mut clause: Option<String> = None;
    let mut test_number: Option<String> = None;
    let mut message: Option<String> = None;
    let mut failed = false;

    let flush = |issues: &mut Vec<Issue>,
                 clause: &mut Option<String>,
                 test_number: &mut Option<String>,
                 message: &mut Option<String>,
                 failed: &mut bool| {
        if *failed {
            issues.push(Issue {
                rule_id: clause.clone(),
                message: message
                    .take()
                    .unwrap_or_else(|| "PDF/A validation failed".into()),
                location: "Unknown".into(),
                specification: clause.clone(),
                clause: clause.clone(),
                test_number: test_number.take(),
            });
        }
        *clause = None;
        *test_number = None;
        *message = None;
        *failed = false;
    };

    for line in report.lines() {
        let line = line.trim();
        let opens_clause = line
            .strip_prefix("Clause:")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                // Bare header form: "6.2.10.2:" alone on the line.
                let h = line.trim_end_matches(':');
                (h != line && h.chars().all(|c| c.is_ascii_digit() || c == '.'))
                    .then(|| h.to_string())
            });
        if let Some(c) = opens_clause {
            flush(
                &mut issues,
                &mut clause,
                &mut test_number,
                &mut message,
                &mut failed,
            );
            clause = Some(c);
            continue;
        }
        if let Some(v) = line.strip_prefix("Test number:") {
            test_number = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("Status:") {
            failed = v.trim().eq_ignore_ascii_case("failed");
        } else if let Some(v) = line
            .strip_prefix("Message:")
            .or_else(|| line.strip_prefix("Description:"))
        {
            message = Some(v.trim().to_string());
        }
    }
    flush(
        &mut issues,
        &mut clause,
        &mut test_number,
        &mut message,
        &mut failed,
    );
    issues
}

/// `formatStandardDisplay`: `"<name> compliant"`, `"<name> with errors"`,
/// or the not-PDF/A placeholder.
pub fn standard_display(base: &str, error_count: usize, declared_pdfa: bool) -> String {
    const NOT_PDFA: &str = "Not PDF/A (no PDF/A identification metadata)";
    if !declared_pdfa && base == NOT_PDFA {
        return NOT_PDFA.into();
    }
    if error_count > 0 {
        format!("{base} with errors")
    } else {
        format!("{base} compliant")
    }
}

pub const NOT_PDFA_ID: &str = "not-pdfa";
pub const NOT_PDFA_NAME: &str = "Not PDF/A (no PDF/A identification metadata)";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xmp_attr_reads_single_and_double_quotes() {
        let xmp = r#"<pdfaid:part>2</pdfaid:part><rdf:Description pdfaid:part="2" pdfaid:conformance='B'/>"#;
        assert_eq!(xmp_attr(xmp, "pdfaid", "part").as_deref(), Some("2"));
        assert_eq!(xmp_attr(xmp, "pdfaid", "conformance").as_deref(), Some("B"));
        assert_eq!(xmp_attr(xmp, "pdfaid", "missing"), None);
    }

    #[test]
    fn pdfa_level_maps_valid_combinations() {
        assert_eq!(pdfa_level(1, "B").as_deref(), Some("1b"));
        assert_eq!(pdfa_level(2, "A").as_deref(), Some("2a"));
        assert_eq!(pdfa_level(2, "U").as_deref(), Some("2u"));
        assert_eq!(pdfa_level(3, "B").as_deref(), Some("3b"));
        assert_eq!(pdfa_level(4, "B"), None);
        assert_eq!(pdfa_level(2, "X"), None);
    }

    #[test]
    fn parse_failed_clauses_keeps_only_failed_blocks() {
        let report = "Clause: 6.2.10.2\nTest number: 4\nStatus: failed\nMessage: bad output intent\nClause: 7.1\nStatus: passed\nCompliance: false";
        let issues = parse_failed_clauses(report);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].clause.as_deref(), Some("6.2.10.2"));
        assert_eq!(issues[0].test_number.as_deref(), Some("4"));
        assert_eq!(issues[0].message, "bad output intent");
    }

    #[test]
    fn standard_display_matches_java_forms() {
        assert_eq!(standard_display("PDF/A-1b", 0, true), "PDF/A-1b compliant");
        assert_eq!(
            standard_display("PDF/A-1b", 2, true),
            "PDF/A-1b with errors"
        );
        assert_eq!(standard_display(NOT_PDFA_NAME, 1, false), NOT_PDFA_NAME);
    }
}
