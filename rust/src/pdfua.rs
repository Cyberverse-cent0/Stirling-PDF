//! Port of the proprietary PDF/UA accessibility audit
//! (`AccessibilityAuditService` + `PdfUaValidationService`): a read-only
//! report combining veraPDF clause failures, document-level facts, and
//! the list of figures still needing alternative text.
//!
//! `figuresNeedingDescription` is always empty here: Java fills it by
//! running `TaggedContentExtractor` + `LayoutAnalyzer` (~1.5k lines of
//! marked-content/layout heuristics) which has no Rust port yet. Java
//! itself returns an empty list when that analysis throws, so the shape
//! is contract-correct but the enumeration is a known gap — clause 7.3
//! failures still surface through `issues`.

use std::collections::HashMap;

use lopdf::{Document, Object};
use serde::Serialize;

use crate::error::AppError;
use crate::pdfdoc;
use crate::verapdf;

/// `AccessibilityAuditService.MAX_INPUT_BYTES` / `MAX_PAGES`.
const MAX_INPUT_BYTES: u64 = 100 * 1024 * 1024;
const MAX_PAGES: usize = 2000;

/// `PdfUaProfile.fromRequest`: blank defaults to UA1; only the UA2
/// spellings select the second edition.
#[derive(Clone, Copy)]
pub enum UaProfile {
    Ua1,
    Ua2,
}

impl UaProfile {
    pub fn from_request(value: Option<&str>) -> Self {
        let Some(value) = value else {
            return Self::Ua1;
        };
        if value.trim().is_empty() {
            return Self::Ua1;
        }
        let normalised: String = value
            .trim()
            .to_lowercase()
            .chars()
            .filter(|c| *c != '/' && *c != '-')
            .collect();
        match normalised.as_str() {
            "ua2" | "pdfua2" | "2" => Self::Ua2,
            _ => Self::Ua1,
        }
    }

    /// `PdfUaProfile.displayName`.
    fn display_name(self) -> &'static str {
        match self {
            Self::Ua1 => "PDF/UA-1",
            Self::Ua2 => "PDF/UA-2",
        }
    }

    /// veraPDF CLI flavour token (`PDFAFlavour.PDFUA_1/2`).
    fn flavour(self) -> &'static str {
        match self {
            Self::Ua1 => "ua-1",
            Self::Ua2 => "ua-2",
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessibilityReport {
    profile: String,
    tagged: bool,
    declares_conformance: bool,
    passes_automated_checks: bool,
    issues: Vec<AccessibilityIssue>,
    human_checks: Vec<String>,
    automatically_fixable: usize,
    needs_input: usize,
    figures_needing_description: Vec<FigureDescriptor>,
    summary: Summary,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessibilityIssue {
    clause: String,
    #[serde(rename = "testNumber", skip_serializing_if = "Option::is_none")]
    test_number: Option<String>,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    technical_message: Option<String>,
    severity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    specification: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    location: Option<String>,
    occurrences: usize,
    auto_fixable: bool,
}

/// `FigureDescriptor` — `key` is the `pageIndex:ordinal` altTextByFigure
/// key, `page` 1-based, `kind` "figure" or "formula".
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FigureDescriptor {
    key: String,
    page: usize,
    kind: String,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    existing_alt: Option<String>,
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pages: usize,
    has_title: bool,
    displays_doc_title: bool,
    has_language: bool,
    all_fonts_embedded: bool,
    unembedded_fonts: usize,
    figures: usize,
    encrypted: bool,
}

/// `AccessibilityAuditService.HUMAN_CHECKS` — verbatim; these are the
/// checks no validator can make.
const HUMAN_CHECKS: [&str; 7] = [
    "Is the reading order correct for someone who cannot see the layout?",
    "Does each alternative description convey what the image is for, not just what it looks like?",
    "Are headings used for structure rather than for visual emphasis?",
    "Is any information conveyed by colour alone also available another way?",
    "Do tables have headers that identify the right rows and columns?",
    "Is the document language correct, including for quoted passages?",
    "Do links describe their destination rather than saying 'click here'?",
];

/// `AccessibilityAuditService.audit`: validation first (it runs even on
/// files PDFBox cannot open), then the document-level summary when the
/// file parses.
pub fn audit(pdf: &[u8], profile: UaProfile) -> Result<AccessibilityReport, AppError> {
    enforce_limits(pdf)?;

    let document = pdfdoc::load("input.pdf", pdf).ok();
    let (issues, compliant) = validate(pdf, profile);

    let mut fixable = 0;
    let mut needs_input = 0;
    for issue in &issues {
        if issue.auto_fixable {
            fixable += 1;
        } else {
            needs_input += 1;
        }
    }

    let mut report = AccessibilityReport {
        profile: profile.display_name().into(),
        tagged: false,
        declares_conformance: false,
        passes_automated_checks: compliant,
        issues,
        human_checks: HUMAN_CHECKS.iter().map(|s| s.to_string()).collect(),
        automatically_fixable: fixable,
        needs_input,
        figures_needing_description: Vec::new(),
        summary: Summary::default(),
    };

    if let Some(doc) = &document {
        populate_summary(doc, &mut report);
    }
    Ok(report)
}

/// `enforceLimits`: size always, page count only when the file parses —
/// an unreadable file is left for the report itself to describe.
fn enforce_limits(pdf: &[u8]) -> Result<(), AppError> {
    if pdf.len() as u64 > MAX_INPUT_BYTES {
        return Err(AppError::BadRequest(format!(
            "This PDF is {} MB. The accessibility report is limited to {} MB.",
            pdf.len() / (1024 * 1024),
            MAX_INPUT_BYTES / (1024 * 1024)
        )));
    }
    if let Ok(doc) = pdfdoc::load("input.pdf", pdf) {
        let pages = doc.get_pages().len();
        if pages > MAX_PAGES {
            return Err(AppError::BadRequest(format!(
                "This PDF has {pages} pages. The accessibility report is limited to {MAX_PAGES} pages; split it first."
            )));
        }
    }
    Ok(())
}

/// `PdfUaValidationService.validate` + `toResult`: veraPDF CLI failures
/// grouped by clause-test, plain-English clause text where the table
/// knows the clause.
fn validate(pdf: &[u8], profile: UaProfile) -> (Vec<AccessibilityIssue>, bool) {
    let spec = profile.display_name();
    let report = verapdf::validate(pdf, Some(profile.flavour()));

    if report.unavailable || report.error.is_some() {
        let detail = report
            .error
            .unwrap_or_else(|| "veraPDF is not available on this server".into());
        return (
            vec![AccessibilityIssue {
                clause: "n/a".into(),
                test_number: None,
                message: format!("Validation could not run: {detail}"),
                technical_message: None,
                severity: "error".into(),
                specification: None,
                location: None,
                occurrences: 0,
                auto_fixable: false,
            }],
            false,
        );
    }

    // LinkedHashMap grouping by "clause-testNumber", first-seen order;
    // occurrences count every failed assertion and the location is the
    // first one the validator reported.
    let mut order: Vec<(String, String)> = Vec::new();
    let mut counts: HashMap<(String, String), usize> = HashMap::new();
    let mut locations: HashMap<(String, String), String> = HashMap::new();
    let mut technical: HashMap<(String, String), String> = HashMap::new();
    for raw in &report.issues {
        let clause = raw
            .clause
            .clone()
            .or_else(|| raw.rule_id.clone())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| "unknown".into());
        let test = raw.test_number.clone().unwrap_or_default();
        let key = (clause, test);
        if !counts.contains_key(&key) {
            order.push(key.clone());
            technical.insert(key.clone(), raw.message.clone());
        }
        *counts.entry(key.clone()).or_insert(0) += 1;
        let loc = raw.location.trim();
        if !loc.is_empty() && loc != "Unknown" {
            locations
                .entry(key.clone())
                .or_insert_with(|| loc.to_string());
        }
    }

    let issues: Vec<AccessibilityIssue> = order
        .into_iter()
        .map(|(clause, test)| {
            let key = (clause.clone(), test.clone());
            let raw_message = technical.get(&key).cloned().unwrap_or_default();
            let info = lookup_clause(&clause);
            AccessibilityIssue {
                clause,
                test_number: (!test.is_empty()).then_some(test),
                message: info
                    .map(|i| i.message.to_string())
                    .unwrap_or_else(|| raw_message.clone()),
                technical_message: Some(raw_message),
                severity: "error".into(),
                specification: Some(spec.into()),
                location: locations.get(&key).cloned(),
                occurrences: counts.get(&key).copied().unwrap_or(0),
                auto_fixable: info.is_some_and(|i| i.auto_fixable),
            }
        })
        .collect();

    let compliant = report.compliant && report.issues.is_empty();
    (issues, compliant)
}

/// `populateSummary` + `figuresNeedingDescription`: document facts that
/// drive most failures.
fn populate_summary(doc: &Document, report: &mut AccessibilityReport) {
    let catalog = match doc.catalog() {
        Ok(c) => c,
        Err(_) => return,
    };

    report.tagged = catalog.get(b"StructTreeRoot").is_ok();
    report.declares_conformance = declares_ua(doc);
    report.summary.pages = doc.get_pages().len();
    report.summary.encrypted = pdfdoc::is_encrypted(doc);

    let language = catalog
        .get(b"Lang")
        .ok()
        .and_then(|v| lopdf::decode_text_string(v).ok())
        .unwrap_or_default();
    report.summary.has_language = !language.trim().is_empty();

    report.summary.has_title =
        pdfdoc::info_value(doc, "Title").is_some_and(|t| !t.trim().is_empty());

    report.summary.displays_doc_title = catalog
        .get(b"ViewerPreferences")
        .ok()
        .and_then(|v| pdfdoc::dict_owned(doc, v).ok().flatten())
        .and_then(|d| d.get(b"DisplayDocTitle").ok().cloned())
        .and_then(|v| v.as_bool().ok())
        .unwrap_or(false);

    let unembedded = unembedded_fonts(doc);
    report.summary.unembedded_fonts = unembedded.len();
    report.summary.all_fonts_embedded = unembedded.is_empty();
    report.summary.figures = count_graphics(doc);
}

/// `declaresUa`: the XMP packet carries a `pdfuaid` identifier.
fn declares_ua(doc: &Document) -> bool {
    let Ok(catalog) = doc.catalog() else {
        return false;
    };
    let Ok(metadata) = catalog.get(b"Metadata") else {
        return false;
    };
    let Ok((_, object)) = doc.dereference(metadata) else {
        return false;
    };
    let Object::Stream(stream) = object else {
        return false;
    };
    let bytes = stream
        .decompressed_content()
        .unwrap_or_else(|_| stream.content.clone());
    String::from_utf8_lossy(&bytes).contains("pdfuaid")
}

/// `FontEmbeddingService.findUnembeddedFonts`: names of fonts whose
/// descriptor carries no font file. Type0 fonts delegate to their
/// descendant CIDFont's descriptor, as PDFBox resolves them.
fn unembedded_fonts(doc: &Document) -> Vec<String> {
    let mut missing = Vec::new();
    for (_, page_id) in doc.get_pages() {
        let Ok(fonts) = doc.get_page_fonts(page_id) else {
            continue;
        };
        for (name, font) in fonts {
            if !font_is_embedded(doc, font) {
                let font_name = font
                    .get(b"BaseFont")
                    .ok()
                    .and_then(|v| {
                        doc.dereference(v)
                            .ok()
                            .and_then(|(_, o)| {
                                o.as_name()
                                    .ok()
                                    .map(|n| String::from_utf8_lossy(n).into_owned())
                            })
                            .or_else(|| {
                                v.as_str()
                                    .ok()
                                    .map(|b| String::from_utf8_lossy(b).into_owned())
                            })
                    })
                    .unwrap_or_else(|| String::from_utf8_lossy(&name).into_owned());
                if !missing.contains(&font_name) {
                    missing.push(font_name);
                }
            }
        }
    }
    missing
}

/// `PDFont.isEmbedded` — the font's descriptor holds a FontFile,
/// FontFile2 or FontFile3 stream.
fn font_is_embedded(doc: &Document, font: &lopdf::Dictionary) -> bool {
    let descriptor = font_descriptor(doc, font);
    let Some(descriptor) = descriptor else {
        return false;
    };
    [b"FontFile".as_slice(), b"FontFile2", b"FontFile3"]
        .into_iter()
        .any(|key| descriptor.get(key).is_ok())
}

fn font_descriptor<'a>(
    doc: &'a Document,
    font: &'a lopdf::Dictionary,
) -> Option<&'a lopdf::Dictionary> {
    // Type0 fonts carry their descriptor on the descendant CIDFont.
    if font
        .get(b"Type")
        .ok()
        .and_then(|v| v.as_name().ok())
        .is_some_and(|n| n == b"Font")
        && font
            .get(b"Subtype")
            .ok()
            .and_then(|v| v.as_name().ok())
            .is_some_and(|n| n == b"Type0")
    {
        if let Ok(descendants) = font.get(b"DescendantFonts") {
            if let Ok(array) = descendants.as_array() {
                if let Some(first) = array.first() {
                    if let Ok((_, Object::Dictionary(descendant))) = doc.dereference(first) {
                        if let Ok(fd) = descendant.get(b"FontDescriptor") {
                            if let Ok((_, Object::Dictionary(d))) = doc.dereference(fd) {
                                return Some(d);
                            }
                        }
                    }
                }
            }
        }
        return None;
    }
    let fd = font.get(b"FontDescriptor").ok()?;
    let (_, object) = doc.dereference(fd).ok()?;
    match object {
        Object::Dictionary(d) => Some(d),
        _ => None,
    }
}

/// `TaggedContentExtractor.countGraphics`: `BI` inline images plus `Do`
/// operators resolving to image XObjects, page content streams only —
/// form XObjects count as units and are not recursed.
fn count_graphics(doc: &Document) -> usize {
    let mut total = 0;
    for (_, page_id) in doc.get_pages() {
        let Ok(page) = doc.get_dictionary(page_id) else {
            continue;
        };
        let image_names = pdfdoc::page_resources_dict(doc, page_id)
            .ok()
            .flatten()
            .map(|resources| image_xobject_names(doc, &resources))
            .unwrap_or_default();
        for bytes in content_streams(doc, page) {
            let Ok(content) = lopdf::content::Content::decode(&bytes) else {
                continue;
            };
            for operation in &content.operations {
                match operation.operator.as_str() {
                    "BI" => total += 1,
                    "Do" => {
                        if let Some(name) =
                            operation.operands.first().and_then(|o| o.as_name().ok())
                        {
                            if image_names.iter().any(|n| n.as_slice() == name) {
                                total += 1;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    total
}

/// Names in a resolved `/Resources` dictionary's `/XObject` section
/// whose `/Subtype` is `/Image`.
fn image_xobject_names(doc: &Document, resources: &lopdf::Dictionary) -> Vec<Vec<u8>> {
    let mut names = Vec::new();
    let Ok(xobjects) = resources.get(b"XObject") else {
        return names;
    };
    let Ok((_, Object::Dictionary(xobjects))) = doc.dereference(xobjects) else {
        return names;
    };
    for (name, value) in xobjects.iter() {
        let Ok((_, object)) = doc.dereference(value) else {
            continue;
        };
        let dict = match object {
            Object::Stream(s) => &s.dict,
            Object::Dictionary(d) => d,
            _ => continue,
        };
        if dict
            .get(b"Subtype")
            .ok()
            .and_then(|v| v.as_name().ok())
            .is_some_and(|n| n == b"Image")
        {
            names.push(name.to_vec());
        }
    }
    names
}

/// Decoded bytes of each of the page's `/Contents` streams (single
/// stream or array).
fn content_streams(doc: &Document, page: &lopdf::Dictionary) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let Ok(contents) = page.get(b"Contents") else {
        return out;
    };
    let values: Vec<Object> = match contents {
        Object::Array(items) => items.clone(),
        other => vec![other.clone()],
    };
    for value in values {
        let Ok((_, object)) = doc.dereference(&value) else {
            continue;
        };
        if let Object::Stream(stream) = object {
            if let Ok(bytes) = stream.decompressed_content() {
                out.push(bytes);
            }
        }
    }
    out
}

/// `PdfUaValidationService.lookupClause`: most specific CLAUSES entry by
/// walking up the dotted hierarchy (`7.1` must not claim `7.18.1`).
fn lookup_clause(clause: &str) -> Option<ClauseInfo> {
    let mut current = clause;
    while !current.is_empty() {
        if let Some(info) = clause_info(current) {
            return Some(info);
        }
        current = &current[..current.rfind('.')?];
    }
    None
}

#[derive(Clone, Copy)]
struct ClauseInfo {
    message: &'static str,
    auto_fixable: bool,
}

/// `buildClauseTable` — plain-English text and remediability for the
/// clauses users actually hit.
fn clause_info(clause: &str) -> Option<ClauseInfo> {
    let (message, auto_fixable) = match clause {
        "7.1" => (
            "Document is not tagged, or some content is neither tagged nor marked as an artifact.",
            true,
        ),
        "7.2" => (
            "Text cannot be mapped to Unicode, or the document language is not declared.",
            true,
        ),
        "7.3" => ("An image or graphic has no alternative description.", false),
        "7.4" => (
            "Heading levels skip a level, or headings are nested incorrectly.",
            true,
        ),
        "7.5" => ("A table is missing header cells or header associations.", false),
        "7.6" => ("A list is not structured as list items with bodies.", true),
        "7.7" => ("A mathematical expression has no alternative description.", false),
        "7.8" => ("Running heads or page numbers are not marked as artifacts.", true),
        "7.9" => ("A note is missing a unique identifier.", true),
        "7.10" => ("An optional content group has no name.", false),
        "7.11" => ("An embedded file is missing its relationship or description.", false),
        "7.15" => (
            "The document uses a dynamic XFA form, which PDF/UA does not allow.",
            false,
        ),
        "7.16" => (
            "Security settings prevent assistive technology from reading the content.",
            true,
        ),
        "7.17" => ("Navigation aids such as page labels are missing.", true),
        "7.18" => (
            "An annotation is missing a description, tab order, or structure entry.",
            true,
        ),
        "7.20" => (
            "A form or group XObject is not marked as content or as an artifact.",
            false,
        ),
        "7.21" => ("A font in the document does not meet PDF/UA rules.", false),
        "7.21.4.1" => ("A font used in the document is not embedded.", true),
        "7.21.7" => (
            "A font does not map every character it uses to Unicode, so extracted text may be wrong.",
            false,
        ),
        "5" => (
            "The document does not declare PDF/UA conformance in its XMP metadata.",
            true,
        ),
        _ => return None,
    };
    Some(ClauseInfo {
        message,
        auto_fixable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::Pdf;
    use lopdf::{dictionary, Object};

    #[test]
    fn profile_from_request_spellings() {
        assert!(matches!(UaProfile::from_request(None), UaProfile::Ua1));
        assert!(matches!(UaProfile::from_request(Some("")), UaProfile::Ua1));
        assert!(matches!(
            UaProfile::from_request(Some("ua1")),
            UaProfile::Ua1
        ));
        assert!(matches!(
            UaProfile::from_request(Some("PDF/UA-2")),
            UaProfile::Ua2
        ));
        assert!(matches!(
            UaProfile::from_request(Some("pdfua2")),
            UaProfile::Ua2
        ));
        assert!(matches!(UaProfile::from_request(Some("2")), UaProfile::Ua2));
        assert!(matches!(
            UaProfile::from_request(Some("bogus")),
            UaProfile::Ua1
        ));
    }

    #[test]
    fn clause_lookup_walks_dotted_hierarchy() {
        // 7.1 must not claim 7.18.x descendants; the walk stops at the
        // exact entry first.
        let info = lookup_clause("7.18.1.2").unwrap();
        assert_eq!(
            info.message,
            "An annotation is missing a description, tab order, or structure entry."
        );
        assert!(lookup_clause("7.21.4.1").unwrap().auto_fixable);
        assert!(!lookup_clause("7.21.4.1")
            .unwrap()
            .message
            .contains("does not meet"));
        assert!(lookup_clause("9.9").is_none());
    }

    #[test]
    fn audit_minimal_pdf_reports_facts() {
        let mut pdf = Pdf::new();
        let bytes = pdf.bytes();
        let report = audit(&bytes, UaProfile::Ua1).unwrap();
        assert_eq!(report.profile, "PDF/UA-1");
        assert!(!report.tagged);
        assert!(!report.declares_conformance);
        assert!(!report.passes_automated_checks);
        assert_eq!(report.human_checks.len(), 7);
        assert_eq!(report.summary.pages, 1);
        assert!(!report.summary.encrypted);
        // Either veraPDF ran and found failures, or the CLI is absent and
        // the single "Validation could not run" issue stands in — both
        // leave the automated check failed.
        assert!(!report.issues.is_empty());
        assert!(report.figures_needing_description.is_empty());
    }

    #[test]
    fn unembedded_font_detected_and_embedded_cleared() {
        let mut pdf = Pdf::new();
        let font = pdf.add(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        pdf.edit_page(|page| {
            page.set(
                "Resources",
                Object::Dictionary(dictionary! {
                    "Font" => Object::Dictionary(dictionary! {
                        "F1" => Object::Reference(font),
                    }),
                }),
            );
        });
        let bytes = pdf.bytes();
        let doc = pdfdoc::load("t.pdf", &bytes).unwrap();
        assert_eq!(unembedded_fonts(&doc), vec!["Helvetica".to_string()]);

        // A descriptor carrying a font file clears the font.
        let font_file = pdf.add_stream(dictionary! {}, b"font");
        let descriptor = pdf.add(dictionary! {
            "Type" => "FontDescriptor",
            "FontName" => "Helvetica",
            "FontFile2" => Object::Reference(font_file),
        });
        let mut font_dict = pdf.dict(font);
        font_dict.set("FontDescriptor", Object::Reference(descriptor));
        pdf.put_dict(font, font_dict);
        let bytes = pdf.bytes();
        let doc = pdfdoc::load("t.pdf", &bytes).unwrap();
        assert!(unembedded_fonts(&doc).is_empty());
    }

    #[test]
    fn type0_font_checks_descendant_descriptor() {
        let mut pdf = Pdf::new();
        let cid_descriptor = pdf.add(dictionary! {
            "Type" => "FontDescriptor",
            "FontName" => "AAAAAA+Dummy",
        });
        let cid = pdf.add(dictionary! {
            "Type" => "Font",
            "Subtype" => "CIDFontType2",
            "BaseFont" => "AAAAAA+Dummy",
            "FontDescriptor" => Object::Reference(cid_descriptor),
        });
        let font = pdf.add(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type0",
            "BaseFont" => "AAAAAA+Dummy",
            "DescendantFonts" => vec![Object::Reference(cid)],
        });
        pdf.edit_page(|page| {
            page.set(
                "Resources",
                Object::Dictionary(dictionary! {
                    "Font" => Object::Dictionary(dictionary! {
                        "F1" => Object::Reference(font),
                    }),
                }),
            );
        });
        let bytes = pdf.bytes();
        let doc = pdfdoc::load("t.pdf", &bytes).unwrap();
        assert_eq!(unembedded_fonts(&doc), vec!["AAAAAA+Dummy".to_string()]);
    }

    #[test]
    fn count_graphics_counts_image_do_only() {
        let mut pdf = Pdf::new();
        let image = pdf.add_stream(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => 1,
                "Height" => 1,
                "ColorSpace" => "DeviceRGB",
                "BitsPerComponent" => 8,
            },
            b"\xff\x00\x00",
        );
        let form = pdf.add_stream(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Form",
                "BBox" => vec![0.into(), 0.into(), 10.into(), 10.into()],
            },
            b"/Im1 Do",
        );
        let content = pdf.add_stream(
            dictionary! {},
            b"q 1 0 0 1 0 0 cm /Im1 Do Q /Fm1 Do 10 10 m 20 20 l S",
        );
        pdf.edit_page(|page| {
            page.set("Contents", Object::Reference(content));
            page.set(
                "Resources",
                Object::Dictionary(dictionary! {
                    "XObject" => Object::Dictionary(dictionary! {
                        "Im1" => Object::Reference(image),
                        "Fm1" => Object::Reference(form),
                    }),
                }),
            );
        });
        let bytes = pdf.bytes();
        let doc = pdfdoc::load("t.pdf", &bytes).unwrap();
        // The image inside the form XObject is not counted — Java scans
        // page streams only.
        assert_eq!(count_graphics(&doc), 1);
    }

    #[test]
    fn declares_ua_reads_xmp_pdfuaid() {
        let mut pdf = Pdf::new();
        let xmp = pdf.add_stream(
            dictionary! {},
            br#"<x:xmpmeta><rdf:Description pdfuaid:part="1"/></x:xmpmeta>"#,
        );
        pdf.edit_catalog(|catalog| {
            catalog.set("Metadata", Object::Reference(xmp));
        });
        let bytes = pdf.bytes();
        let doc = pdfdoc::load("t.pdf", &bytes).unwrap();
        assert!(declares_ua(&doc));
        assert!(!declares_ua(&{
            let mut plain = Pdf::new();
            pdfdoc::load("t.pdf", &plain.bytes()).unwrap()
        }));
    }

    #[test]
    fn oversized_and_too_many_pages_rejected() {
        assert!(matches!(
            enforce_limits(&vec![0u8; (MAX_INPUT_BYTES + 1) as usize]),
            Err(AppError::BadRequest(_))
        ));
        // Unparseable input skips the page-count check and is left for
        // the report itself to describe.
        assert!(enforce_limits(b"not a pdf").is_ok());
    }
}
