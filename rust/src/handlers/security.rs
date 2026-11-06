//! Port of `GetInfoOnPDF` (`/api/v1/security/get-info-on-pdf`): the
//! comprehensive PDF inspection report — metadata, basic stats, document
//! info, compliance, encryption, permissions, form fields, document
//! structures, and per-page detail, assembled into one JSON object.
//!
//! Divergences from the Java/PDFBox version:
//! - `Compliancy`/`SummaryData.Compliance`: no veraPDF equivalent exists in
//!   Rust, so every standard reports non-compliant and no per-standard
//!   keys or `Compliance` array are emitted. `IsPDF/SECCompliant` is
//!   computed locally like Java.
//! - Text-derived counts (`WordCount`, `ParagraphCount`,
//!   `CharacterCount`, `Text Characters Count`) come from lopdf's content
//!   stream reader, not PDFBox's `PDFTextStripper`, so the numbers differ
//!   on real documents.
//! - `Other.XMPMetadata` is the raw `/Metadata` stream decoded as UTF-8;
//!   Java reserialises it through an XMP parser when that succeeds.
//! - `Other.StructureTree` is rebuilt from raw structure-element
//!   dictionaries; marked-content kids that are not structure elements
//!   are skipped the way PDFBox's `PDStructureNode` filter does.

use std::collections::{HashMap, HashSet};

use axum::extract::{Multipart, Query, State};
use axum::response::Response;
use axum::Json;
use lopdf::{decode_text_string, Dictionary, Document, Object, ObjectId, Stream};
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::error::{AppError, AppResult};
use crate::handlers::jobs::AppState;
use crate::handlers::merge::{parse_bool, pdf_response};
use crate::handlers::misc;
use crate::page_select;
use crate::pdfdoc;
use crate::pdftext;
use crate::upload::{
    output_name, param, param_bool, param_f64, receive_multipart, Upload, FILE_FIELD,
};

const MAX_STRUCT_DEPTH: usize = 64;

fn load_pdf(uploads: Vec<Upload>) -> AppResult<(String, usize, Document)> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(crate::error::AppError::BadRequest(format!(
            "{FILE_FIELD} is required"
        )));
    };
    let name = upload.name.clone();
    let size = upload.bytes.len();
    let document = pdfdoc::load(&name, &upload.bytes)?;
    Ok((name, size, document))
}

/// Follows references and decodes a text string, like PDFBox's COSString
/// getters. `None` for non-string objects.
fn text(doc: &Document, object: &Object) -> Option<String> {
    let (_, resolved) = doc.dereference(object).ok()?;
    decode_text_string(resolved).ok()
}

/// `/Key` of a dictionary as decoded text.
fn entry_text(doc: &Document, dict: &Dictionary, key: &[u8]) -> Option<String> {
    dict.get(key).ok().and_then(|o| text(doc, o))
}

/// `/Key` of a dictionary as a name string.
fn entry_name(doc: &Document, dict: &Dictionary, key: &[u8]) -> Option<String> {
    let (_, resolved) = doc.dereference(dict.get(key).ok()?).ok()?;
    resolved
        .as_name()
        .ok()
        .map(|n| String::from_utf8_lossy(n).into_owned())
}

/// `/Key` of a dictionary as an integer with a default, like COSDictionary
/// `getInt(key, default)`.
fn entry_int(doc: &Document, dict: &Dictionary, key: &[u8], default: i64) -> i64 {
    dict.get(key)
        .ok()
        .and_then(|o| {
            let (_, resolved) = doc.dereference(o).ok()?;
            resolved.as_i64().ok()
        })
        .unwrap_or(default)
}

/// `/Key` of a dictionary as a dictionary (plain or stream), resolved.
fn entry_dict(doc: &Document, dict: &Dictionary, key: &[u8]) -> Option<Dictionary> {
    dict.get(key)
        .ok()
        .and_then(|o| pdfdoc::dict_owned(doc, o).ok().flatten())
}

fn null_if_none(value: Option<String>) -> Value {
    value.map(Value::String).unwrap_or(Value::Null)
}

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

const STANDARD_INFO_KEYS: &[&str] = &[
    "Title",
    "Author",
    "Subject",
    "Keywords",
    "Producer",
    "Creator",
    "CreationDate",
    "ModDate",
    "Trapped",
];

fn metadata(doc: &Document) -> Value {
    let mut out = Map::new();
    let Some(info) = pdfdoc::info_dict(doc) else {
        return Value::Object(out);
    };
    for key in [
        "Title", "Author", "Subject", "Keywords", "Producer", "Creator",
    ] {
        if let Some(value) = entry_text(doc, &info, key.as_bytes()) {
            out.insert(key.to_string(), Value::String(value));
        }
    }
    for (pdf_key, out_key) in [
        ("CreationDate", "CreationDate"),
        ("ModDate", "ModificationDate"),
    ] {
        if let Some(date) = info
            .get(pdf_key.as_bytes())
            .ok()
            .and_then(|o| text(doc, o))
            .and_then(|s| pdfdoc::pdf_date_local(&s))
        {
            out.insert(out_key.to_string(), Value::String(date));
        }
    }
    // Custom Info entries beyond the standard fields (e.g. a classification
    // policy marker), non-blank strings only.
    for (key, object) in info.iter() {
        let key = String::from_utf8_lossy(key).into_owned();
        if STANDARD_INFO_KEYS.contains(&key.as_str()) {
            continue;
        }
        if let Some(value) = text(doc, object) {
            if !value.trim().is_empty() {
                out.insert(key, Value::String(value));
            }
        }
    }
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// BasicInfo
// ---------------------------------------------------------------------------

/// Java `String.split` length semantics: trailing empty fields are
/// discarded, a leading separator still yields an empty first field, and
/// splitting the empty string yields one field.
fn java_split_len(text: &str, mut sep: impl FnMut(&[char], usize) -> Option<usize>) -> usize {
    if text.is_empty() {
        return 1;
    }
    let chars: Vec<char> = text.chars().collect();
    let mut fields: Vec<bool> = vec![false];
    let mut i = 0;
    while i < chars.len() {
        if let Some(len) = sep(&chars, i) {
            fields.push(false);
            i += len;
        } else {
            *fields.last_mut().unwrap() = true;
            i += 1;
        }
    }
    fields.iter().rposition(|b| *b).map_or(0, |p| p + 1)
}

/// Java `\s+` — ASCII whitespace only (no Unicode spaces, unlike Rust's
/// `char::is_whitespace`).
fn is_java_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0B' | '\x0C' | '\r')
}

fn whitespace_sep(chars: &[char], i: usize) -> Option<usize> {
    if !is_java_space(chars[i]) {
        return None;
    }
    let mut len = 1;
    while i + len < chars.len() && is_java_space(chars[i + len]) {
        len += 1;
    }
    Some(len)
}

/// Java `\r\n|\r|\n` — `\r\n` is a single separator.
fn newline_sep(chars: &[char], i: usize) -> Option<usize> {
    match chars[i] {
        '\r' if chars.get(i + 1) == Some(&'\n') => Some(2),
        '\r' | '\n' => Some(1),
        _ => None,
    }
}

/// `PDImageXObject.getSuffix()`: the image kind implied by the stream's
/// `/Filter` list.
fn image_suffix(dict: &Dictionary) -> Option<String> {
    let mut filters = Vec::new();
    if let Ok(filter) = dict.get(b"Filter") {
        match filter {
            Object::Name(name) => filters.push(name.clone()),
            Object::Array(items) => {
                for item in items {
                    if let Object::Name(name) = item {
                        filters.push(name.clone());
                    }
                }
            }
            _ => {}
        }
    }
    let has = |name: &[u8]| filters.iter().any(|f| f == name);
    if filters.is_empty() || has(b"FlateDecode") || has(b"LZWDecode") || has(b"RunLengthDecode") {
        Some("png".into())
    } else if has(b"DCTDecode") {
        Some("jpg".into())
    } else if has(b"JPXDecode") {
        Some("jpx".into())
    } else if has(b"CCITTFaxDecode") {
        Some("tiff".into())
    } else if has(b"JBIG2Decode") {
        Some("jb2".into())
    } else {
        None
    }
}

fn image_stats(doc: &Document) -> (u64, u64) {
    let mut total = 0u64;
    let mut unique = HashSet::new();
    for page_id in pdfdoc::page_ids(doc) {
        let Ok(xobjects) = pdfdoc::resource_entries(doc, page_id, b"XObject") else {
            continue;
        };
        for (_, dict) in xobjects {
            if entry_name(doc, &dict, b"Subtype").as_deref() != Some("Image") {
                continue;
            }
            total += 1;
            let width = entry_int(doc, &dict, b"Width", 0);
            let height = entry_int(doc, &dict, b"Height", 0);
            let bpc = entry_int(doc, &dict, b"BitsPerComponent", 1);
            let suffix = image_suffix(&dict).unwrap_or_default();
            unique.insert(format!("{width}_{height}_{bpc}_{suffix}"));
        }
    }
    (total, unique.len() as u64)
}

fn basic_info(doc: &Document, file_size: usize, page_numbers: &[u32]) -> Value {
    let mut out = Map::new();
    out.insert("FileSizeInBytes".to_string(), Value::from(file_size as u64));

    // PDFTextStripper equivalent; on failure Java aborts the rest of the
    // section, so only FileSizeInBytes survives.
    let Ok(full_text) = doc.extract_text(page_numbers) else {
        tracing::warn!("text extraction failed; reporting file size only");
        return Value::Object(out);
    };
    out.insert(
        "WordCount".to_string(),
        Value::from(java_split_len(&full_text, whitespace_sep) as u64),
    );
    out.insert(
        "ParagraphCount".to_string(),
        Value::from(java_split_len(&full_text, newline_sep) as u64),
    );
    out.insert(
        "CharacterCount".to_string(),
        Value::from(full_text.encode_utf16().count() as u64),
    );

    if let Ok(catalog) = doc.catalog() {
        if let Some(language) = catalog.get(b"Lang").ok().and_then(|o| text(doc, o)) {
            out.insert("Language".to_string(), Value::String(language));
        }
    }
    out.insert(
        "Number of pages".to_string(),
        Value::from(page_numbers.len() as u64),
    );

    let (total, unique) = image_stats(doc);
    out.insert("TotalImages".to_string(), Value::from(total));
    out.insert("UniqueImages".to_string(), Value::from(unique));
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// DocumentInfo
// ---------------------------------------------------------------------------

/// `PDDocumentCatalog.getPageMode()` + `PageMode.name()`: the PDF name maps
/// to an enum constant whose Java name is reported; absent or unknown
/// names give `USE_NONE`.
fn page_mode(doc: &Document) -> &'static str {
    let mode = doc
        .catalog()
        .ok()
        .and_then(|c| c.get(b"PageMode").ok())
        .and_then(|o| {
            let (_, resolved) = doc.dereference(o).ok()?;
            resolved.as_name().ok().map(|n| n.to_vec())
        });
    match mode.as_deref() {
        Some(b"UseOutlines") => "USE_OUTLINES",
        Some(b"UseThumbs") => "USE_THUMBS",
        Some(b"FullScreen") => "FULL_SCREEN",
        Some(b"UseOC") => "USE_OPTIONAL_CONTENT",
        Some(b"UseAttachments") => "USE_ATTACHMENTS",
        _ => "USE_NONE",
    }
}

fn document_info(doc: &Document) -> Value {
    let mut out = Map::new();
    let version = if doc.version.is_empty() {
        "1.7"
    } else {
        doc.version.as_str()
    };
    out.insert("PDF version".to_string(), Value::String(version.into()));
    if let Some(info) = pdfdoc::info_dict(doc) {
        out.insert(
            "Trapped".to_string(),
            null_if_none(entry_name(doc, &info, b"Trapped")),
        );
    }
    out.insert(
        "Page Mode".to_string(),
        Value::String(page_mode(doc).into()),
    );
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// Compliancy + permissions
// ---------------------------------------------------------------------------

/// PDF permission bits as PDFBox `AccessPermission` sees them: bit numbers
/// are 1-based in `isPermissionBitOn`.
struct PermBits(u32);

impl PermBits {
    fn can_print(&self) -> bool {
        self.0 & 0x4 != 0
    }
    fn can_modify(&self) -> bool {
        self.0 & 0x8 != 0
    }
    fn can_extract(&self) -> bool {
        self.0 & 0x10 != 0
    }
    fn can_modify_annots(&self) -> bool {
        self.0 & 0x20 != 0
    }
    fn can_fill_form(&self) -> bool {
        self.0 & 0x100 != 0
    }
    fn can_extract_accessibility(&self) -> bool {
        self.0 & 0x200 != 0
    }
    fn can_assemble(&self) -> bool {
        self.0 & 0x400 != 0
    }
}

/// The user's permission bits: `/P` of the `/Encrypt` dictionary for
/// encrypted files (absent `/P` reads as 0, like `PDEncryption.getInt`),
/// or all-allowed for unencrypted ones (owner access permission).
fn permission_bits(doc: &Document) -> PermBits {
    if !pdfdoc::is_encrypted(doc) {
        return PermBits(u32::MAX);
    }
    let bits = pdfdoc::encrypt_dict(doc)
        .and_then(|enc| enc.get(b"P").ok()?.as_i64().ok())
        .unwrap_or(0);
    PermBits(bits as u32)
}

fn permissions(doc: &Document) -> Value {
    let p = permission_bits(doc);
    let state =
        |allowed: bool| Value::String(if allowed { "Allowed" } else { "Not Allowed" }.into());
    json!({
        "Document Assembly": state(p.can_assemble()),
        "Extracting Content": state(p.can_extract()),
        "Extracting for accessibility": state(p.can_extract_accessibility()),
        "Form Filling": state(p.can_fill_form()),
        "Modifying": state(p.can_modify()),
        "Modifying annotations": state(p.can_modify_annots()),
        "Printing": state(p.can_print()),
    })
}

/// `isSECCompliant`: not encrypted, no JavaScript actions or name-tree
/// entries, no AcroForm, no embedded files, and no link annotations with
/// external actions.
fn sec_compliant(doc: &Document) -> bool {
    if pdfdoc::is_encrypted(doc) {
        return false;
    }
    let Ok(catalog) = doc.catalog() else {
        return false;
    };
    if let Ok(open_action) = catalog.get(b"OpenAction") {
        if let Some(action) = pdfdoc::dict_owned(doc, open_action).ok().flatten() {
            if entry_name(doc, &action, b"S").as_deref() == Some("JavaScript") {
                return false;
            }
        }
    }
    if let Some(names) = pdfdoc::names_dict(doc) {
        for key in [b"JavaScript".as_slice(), b"EmbeddedFiles".as_slice()] {
            if let Ok(value) = names.get(key) {
                let is_null = doc
                    .dereference(value)
                    .map(|(_, o)| matches!(o, Object::Null))
                    .unwrap_or(true);
                if !is_null {
                    return false;
                }
            }
        }
    }
    if catalog.get(b"AcroForm").is_ok() {
        let has_form = catalog
            .get(b"AcroForm")
            .ok()
            .and_then(|o| doc.dereference(o).ok())
            .is_some_and(|(_, o)| !matches!(o, Object::Null));
        if has_form {
            return false;
        }
    }
    for page_id in pdfdoc::page_ids(doc) {
        let Ok(annots) = pdfdoc::annotations(doc, page_id) else {
            continue;
        };
        for annot in annots {
            if pdfdoc::annotation_subtype(&annot) != "Link" {
                continue;
            }
            let Some(action) = annot
                .get(b"A")
                .ok()
                .and_then(|a| pdfdoc::dict_owned(doc, a).ok().flatten())
            else {
                continue;
            };
            if matches!(
                entry_name(doc, &action, b"S").as_deref(),
                Some("URI" | "Launch" | "RemoteGoTo" | "SubmitForm")
            ) {
                return false;
            }
        }
    }
    true
}

fn compliancy(doc: &Document) -> Value {
    json!({
        "IsPDF/ACompliant": false,
        "IsPDF/UACompliant": false,
        "IsPDF/ECompliant": false,
        "IsPDF/VTCompliant": false,
        "IsPDF/BCompliant": false,
        "IsPDF/SECCompliant": sec_compliant(doc),
    })
}

/// `generatePDFSummaryData` minus the veraPDF `Compliance` array: `encrypted`
/// when encrypted, plus `restrictedPermissions`/`restrictedPermissionsCount`
/// when any permission bit is off.
fn summary_data(doc: &Document) -> Value {
    let mut out = Map::new();
    let encrypted = pdfdoc::is_encrypted(doc);
    if encrypted {
        out.insert("encrypted".into(), Value::Bool(true));
    }
    let p = permission_bits(doc);
    let mut restricted = Vec::new();
    if !p.can_assemble() {
        restricted.push("document assembly");
    }
    if !p.can_extract() {
        restricted.push("content extraction");
    }
    if !p.can_extract_accessibility() {
        restricted.push("accessibility extraction");
    }
    if !p.can_fill_form() {
        restricted.push("form filling");
    }
    if !p.can_modify() {
        restricted.push("modification");
    }
    if !p.can_modify_annots() {
        restricted.push("annotation modification");
    }
    if !p.can_print() {
        restricted.push("printing");
    }
    if !restricted.is_empty() {
        let count = restricted.len() as u64;
        out.insert(
            "restrictedPermissions".into(),
            Value::Array(
                restricted
                    .into_iter()
                    .map(|s| Value::String(s.into()))
                    .collect(),
            ),
        );
        out.insert("restrictedPermissionsCount".into(), Value::from(count));
    }
    Value::Object(out)
}

fn encryption(doc: &Document) -> Value {
    let mut out = Map::new();
    if !pdfdoc::is_encrypted(doc) {
        out.insert("IsEncrypted".into(), Value::Bool(false));
        return Value::Object(out);
    }
    out.insert("IsEncrypted".into(), Value::Bool(true));
    if let Some(enc) = pdfdoc::encrypt_dict(doc) {
        // /Filter is a name in practice; an array reads as null like
        // COSDictionary.getNameAsString.
        let algorithm = enc
            .get(b"Filter")
            .ok()
            .and_then(|o| {
                let (_, resolved) = doc.dereference(o).ok()?;
                resolved.as_name().ok().map(|n| n.to_vec())
            })
            .map(|n| String::from_utf8_lossy(&n).into_owned());
        out.insert("EncryptionAlgorithm".into(), null_if_none(algorithm));
        out.insert(
            "KeyLength".into(),
            Value::from(entry_int(doc, &enc, b"Length", 40)),
        );
        out.insert("Version".into(), Value::from(entry_int(doc, &enc, b"V", 0)));
        out.insert(
            "Revision".into(),
            Value::from(entry_int(doc, &enc, b"R", 0)),
        );
    }
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// Other
// ---------------------------------------------------------------------------

/// `/EF` → `/F` embedded-file stream dictionary of a file specification.
fn embedded_file_dict(doc: &Document, filespec: &Dictionary) -> Option<Dictionary> {
    let ef = entry_dict(doc, filespec, b"EF")?;
    entry_dict(doc, &ef, b"F")
}

fn embedded_files(doc: &Document) -> Value {
    let mut out = Vec::new();
    let Some(names) = pdfdoc::names_dict(doc) else {
        return Value::Array(out);
    };
    let Some(tree) = names.get(b"EmbeddedFiles").ok() else {
        return Value::Array(out);
    };
    let Ok(entries) = pdfdoc::name_tree_entries(doc, tree) else {
        return Value::Array(out);
    };
    for (name, spec_object) in entries {
        let mut node = Map::new();
        node.insert("Name".into(), Value::String(name));
        if let Some(spec) = pdfdoc::dict_owned(doc, &spec_object).ok().flatten() {
            if let Some(ef) = embedded_file_dict(doc, &spec) {
                node.insert(
                    "FileSize".into(),
                    Value::from(entry_int(doc, &ef, b"Length", 0)),
                );
                node.insert(
                    "MimeType".into(),
                    null_if_none(entry_name(doc, &ef, b"Subtype")),
                );
                if let Some(params) = entry_dict(doc, &ef, b"Params") {
                    for (pdf_key, out_key) in [
                        ("CreationDate", "CreationDate"),
                        ("ModDate", "ModificationDate"),
                    ] {
                        let date = params
                            .get(pdf_key.as_bytes())
                            .ok()
                            .and_then(|o| text(doc, o))
                            .and_then(|s| pdfdoc::pdf_date_local(&s));
                        node.insert(out_key.into(), null_if_none(date));
                    }
                }
            }
        }
        out.push(Value::Object(node));
    }
    Value::Array(out)
}

fn attachments(doc: &Document) -> Value {
    let mut out = Vec::new();
    for page_id in pdfdoc::page_ids(doc) {
        let Ok(annots) = pdfdoc::annotations(doc, page_id) else {
            continue;
        };
        for annot in annots {
            if pdfdoc::annotation_subtype(&annot) != "FileAttachment" {
                continue;
            }
            let mut node = Map::new();
            node.insert(
                "Name".into(),
                Value::String(entry_name(doc, &annot, b"Name").unwrap_or_else(|| "PushPin".into())),
            );
            node.insert(
                "Description".into(),
                null_if_none(entry_text(doc, &annot, b"Contents")),
            );
            // FileSize only for complex file specifications (a dictionary
            // /FS); simple string specs can't be cast in Java either.
            if let Some(spec) = annot
                .get(b"FS")
                .ok()
                .and_then(|fs| pdfdoc::dict_owned(doc, fs).ok().flatten())
            {
                if let Some(ef) = embedded_file_dict(doc, &spec) {
                    node.insert(
                        "FileSize".into(),
                        Value::from(entry_int(doc, &ef, b"Length", 0)),
                    );
                }
            }
            out.push(Value::Object(node));
        }
    }
    Value::Array(out)
}

fn javascript(doc: &Document) -> Value {
    let mut out = Vec::new();
    let Some(names) = pdfdoc::names_dict(doc) else {
        return Value::Array(out);
    };
    let Some(tree) = names.get(b"JavaScript").ok() else {
        return Value::Array(out);
    };
    let Ok(entries) = pdfdoc::name_tree_entries(doc, tree) else {
        return Value::Array(out);
    };
    for (name, action_object) in entries {
        let mut node = Map::new();
        node.insert("JS Name".into(), Value::String(name));
        if let Some(action) = pdfdoc::dict_owned(doc, &action_object).ok().flatten() {
            if let Some(code) = action.get(b"JS").ok().and_then(|js| text(doc, js)) {
                node.insert(
                    "JS Script Length".into(),
                    Value::from(code.encode_utf16().count() as u64),
                );
            }
        }
        out.push(Value::Object(node));
    }
    Value::Array(out)
}

fn layers(doc: &Document) -> Value {
    let mut out = Vec::new();
    if let Ok(catalog) = doc.catalog() {
        if let Some(oc) = catalog
            .get(b"OCProperties")
            .ok()
            .and_then(|o| pdfdoc::dict_owned(doc, o).ok().flatten())
        {
            if let Ok(ocgs) = oc.get(b"OCGs") {
                if let Ok((_, Object::Array(items))) = doc.dereference(ocgs) {
                    for item in items {
                        if let Some(group) = pdfdoc::dict_owned(doc, item).ok().flatten() {
                            out.push(
                                json!({ "Name": null_if_none(entry_text(doc, &group, b"Name")) }),
                            );
                        }
                    }
                }
            }
        }
    }
    Value::Array(out)
}

/// One node of the logical structure tree, mirroring the Java output:
/// structure elements carry `Type`/`Content`/`Children`, non-element
/// structure nodes emit an empty object.
enum StructNode {
    Element {
        kind: String,
        content: String,
        children: Vec<StructNode>,
    },
    Empty,
}

/// What a structure-tree kid dictionary is: a `PDStructureElement`
/// (`/S` or `/Type /StructElem`), a non-element `PDStructureNode`
/// (`/Type /StructTreeRoot` — Java emits an empty object for it), or
/// neither (marked-content references etc., which Java skips).
enum StructKind {
    Element,
    Node,
    Skip,
}

fn struct_kind(dict: &Dictionary) -> StructKind {
    if dict.has(b"S") || entry_name_from(dict, b"Type").as_deref() == Some("StructElem") {
        StructKind::Element
    } else if entry_name_from(dict, b"Type").as_deref() == Some("StructTreeRoot") {
        StructKind::Node
    } else {
        StructKind::Skip
    }
}

fn entry_name_from(dict: &Dictionary, key: &[u8]) -> Option<String> {
    dict.get(key)
        .ok()
        .and_then(|o| o.as_name().ok())
        .map(|n| String::from_utf8_lossy(n).into_owned())
}

/// `PDStructureElement.getKids()` items that are `COSString`s contribute
/// their text; nested structure elements recurse. Anything else is ignored.
fn struct_content(
    doc: &Document,
    kids: &Object,
    visited: &mut HashSet<lopdf::ObjectId>,
    depth: usize,
) -> String {
    if depth > MAX_STRUCT_DEPTH {
        return String::new();
    }
    let mut content = String::new();
    let items: Vec<Object> = match doc.dereference(kids) {
        Ok((_, Object::Array(items))) => items.to_vec(),
        Ok((_, other)) => vec![other.clone()],
        Err(_) => Vec::new(),
    };
    for item in &items {
        match item {
            Object::String(..) => {
                if let Ok(s) = decode_text_string(item) {
                    content.push_str(&s);
                }
            }
            Object::Reference(id) => {
                if !visited.insert(*id) {
                    continue;
                }
                if let Ok(dict) = doc.get_dictionary(*id) {
                    if let Ok(k) = dict.get(b"K") {
                        content.push_str(&struct_content(doc, k, visited, depth + 1));
                    }
                }
            }
            Object::Dictionary(dict) => {
                if let Ok(k) = dict.get(b"K") {
                    content.push_str(&struct_content(doc, k, visited, depth + 1));
                }
            }
            _ => {}
        }
    }
    content
}

fn struct_children(
    doc: &Document,
    kids: &Object,
    expanded: &mut HashSet<lopdf::ObjectId>,
    depth: usize,
    out: &mut Vec<StructNode>,
) {
    if depth > MAX_STRUCT_DEPTH {
        return;
    }
    let items: Vec<Object> = match doc.dereference(kids) {
        Ok((_, Object::Array(items))) => items.to_vec(),
        Ok((_, other)) => vec![other.clone()],
        Err(_) => Vec::new(),
    };
    for item in &items {
        let Some(dict) = pdfdoc::dict_owned(doc, item).ok().flatten() else {
            continue;
        };
        if let Ok(id) = item.as_reference() {
            if !expanded.insert(id) {
                continue;
            }
        }
        match struct_kind(&dict) {
            StructKind::Skip => continue,
            StructKind::Node => {
                out.push(StructNode::Empty);
                continue;
            }
            StructKind::Element => {}
        }
        let kind = entry_name_from(&dict, b"S").unwrap_or_default();
        let mut visited = HashSet::new();
        let content = dict
            .get(b"K")
            .map(|k| struct_content(doc, k, &mut visited, 0))
            .unwrap_or_default();
        let mut children = Vec::new();
        if let Ok(k) = dict.get(b"K") {
            struct_children(doc, k, expanded, depth + 1, &mut children);
        }
        out.push(StructNode::Element {
            kind,
            content,
            children,
        });
    }
}

fn struct_node_json(node: &StructNode) -> Value {
    match node {
        StructNode::Empty => json!({}),
        StructNode::Element {
            kind,
            content,
            children,
        } => {
            let mut obj = Map::new();
            obj.insert("Type".into(), Value::String(kind.clone()));
            obj.insert("Content".into(), Value::String(content.clone()));
            if !children.is_empty() {
                obj.insert(
                    "Children".into(),
                    Value::Array(children.iter().map(struct_node_json).collect()),
                );
            }
            Value::Object(obj)
        }
    }
}

fn structure_tree(doc: &Document) -> Option<Value> {
    let catalog = doc.catalog().ok()?;
    let root = catalog
        .get(b"StructTreeRoot")
        .ok()
        .and_then(|o| pdfdoc::dict_owned(doc, o).ok().flatten())?;
    let kids = root.get(b"K").ok()?.clone();
    let mut expanded = HashSet::new();
    let mut nodes = Vec::new();
    struct_children(doc, &kids, &mut expanded, 0, &mut nodes);
    Some(Value::Array(nodes.iter().map(struct_node_json).collect()))
}

fn xmp_metadata(doc: &Document) -> Value {
    let Some(catalog) = doc.catalog().ok() else {
        return Value::Null;
    };
    let Some(metadata) = catalog.get(b"Metadata").ok() else {
        return Value::Null;
    };
    let Ok((_, resolved)) = doc.dereference(metadata) else {
        return Value::Null;
    };
    let Object::Stream(stream) = resolved else {
        return Value::Null;
    };
    let bytes = stream
        .decompressed_content()
        .unwrap_or_else(|_| stream.content.clone());
    Value::String(String::from_utf8_lossy(&bytes).into_owned())
}

fn other(doc: &Document) -> Value {
    let mut out = Map::new();
    out.insert("EmbeddedFiles".into(), embedded_files(doc));
    out.insert("Attachments".into(), attachments(doc));

    // Portfolio: /Collection in the catalog; /View only when it is a dict.
    let mut is_portfolio = false;
    let mut portfolio_view = Value::Null;
    if let Ok(catalog) = doc.catalog() {
        if let Ok(collection) = catalog.get(b"Collection") {
            if let Ok((_, resolved)) = doc.dereference(collection) {
                if !matches!(resolved, Object::Null) {
                    is_portfolio = true;
                    if let Object::Dictionary(dict) = resolved {
                        portfolio_view = dict
                            .get(b"View")
                            .ok()
                            .and_then(|v| {
                                let (_, v) = doc.dereference(v).ok()?;
                                v.as_name().ok().map(|n| n.to_vec())
                            })
                            .map(|n| Value::String(String::from_utf8_lossy(&n).into_owned()))
                            .unwrap_or(Value::Null);
                    }
                }
            }
        }
    }
    out.insert("IsPortfolio".into(), Value::Bool(is_portfolio));
    if is_portfolio {
        // Java only writes PortfolioView when /Collection is a dictionary;
        // a non-dict collection leaves the key absent entirely.
        if let Ok(catalog) = doc.catalog() {
            let is_dict = catalog
                .get(b"Collection")
                .ok()
                .and_then(|o| doc.dereference(o).ok())
                .is_some_and(|(_, o)| matches!(o, Object::Dictionary(_)));
            if is_dict {
                out.insert("PortfolioView".into(), portfolio_view);
            }
        }
    }

    out.insert("JavaScript".into(), javascript(doc));
    out.insert("Layers".into(), layers(doc));

    let bookmarks = pdfdoc::outline_titles(doc)
        .unwrap_or_default()
        .into_iter()
        .map(|title| json!({ "Title": null_if_none(title) }))
        .collect();
    out.insert("Bookmarks/Outline/TOC".into(), Value::Array(bookmarks));

    out.insert("XMPMetadata".into(), xmp_metadata(doc));
    if let Some(tree) = structure_tree(doc) {
        out.insert("StructureTree".into(), tree);
    }
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// PerPageInfo
// ---------------------------------------------------------------------------

/// `String.format("%.2f")` on the float dimensions.
fn fmt2(value: f64) -> String {
    format!("{value:.2}")
}

/// Java `Float.toString` shape for box coordinates: integral values keep a
/// `.0`, everything else uses the shortest round-trip form.
fn java_float(value: f32) -> String {
    if value.fract() == 0.0 && value.is_finite() {
        format!("{value:.1}")
    } else {
        format!("{value}")
    }
}

fn rect_string(b: [f32; 4]) -> String {
    format!(
        "[{},{},{},{}]",
        java_float(b[0]),
        java_float(b[1]),
        java_float(b[2]),
        java_float(b[3])
    )
}

fn page_orientation(width: f32, height: f32) -> &'static str {
    if width > height {
        "Landscape"
    } else if height > width {
        "Portrait"
    } else {
        "Square"
    }
}

/// `getPageSize`: first standard size within `SIZE_TOLERANCE` (1 pt), in
/// PDFBox's `PDRectangle` float values.
fn standard_page_size(width: f32, height: f32) -> &'static str {
    const POINTS_PER_MM: f32 = 1.0 / (10.0 * 2.54) * 72.0;
    const SIZES: &[(&str, f32, f32)] = &[
        ("Letter", 612.0, 792.0),
        ("LEGAL", 612.0, 1008.0),
        ("A0", 841.0 * POINTS_PER_MM, 1189.0 * POINTS_PER_MM),
        ("A1", 594.0 * POINTS_PER_MM, 841.0 * POINTS_PER_MM),
        ("A2", 420.0 * POINTS_PER_MM, 594.0 * POINTS_PER_MM),
        ("A3", 297.0 * POINTS_PER_MM, 420.0 * POINTS_PER_MM),
        ("A4", 210.0 * POINTS_PER_MM, 297.0 * POINTS_PER_MM),
        ("A5", 148.0 * POINTS_PER_MM, 210.0 * POINTS_PER_MM),
        ("A6", 105.0 * POINTS_PER_MM, 148.0 * POINTS_PER_MM),
    ];
    for (name, w, h) in SIZES {
        if (width - w).abs() <= 1.0 && (height - h).abs() <= 1.0 {
            return name;
        }
    }
    "Custom"
}

fn annotation_counts(annots: &[Dictionary]) -> (usize, usize, usize) {
    let mut subtype = 0;
    let mut contents = 0;
    for annot in annots {
        // /Subtype counts when it resolves to a name; /Contents when it
        // decodes to a string (PDFBox getters return null otherwise).
        if annot
            .get(b"Subtype")
            .ok()
            .and_then(|o| o.as_name().ok())
            .is_some()
        {
            subtype += 1;
        }
        if annot
            .get(b"Contents")
            .ok()
            .and_then(|o| decode_text_string(o).ok())
            .is_some()
        {
            contents += 1;
        }
    }
    (annots.len(), subtype, contents)
}

fn page_images(doc: &Document, page_id: lopdf::ObjectId) -> AppResult<Vec<Value>> {
    let mut out = Vec::new();
    for (_, dict) in pdfdoc::resource_entries(doc, page_id, b"XObject")? {
        if entry_name(doc, &dict, b"Subtype").as_deref() != Some("Image") {
            continue;
        }
        let mut node = Map::new();
        node.insert(
            "Width".into(),
            Value::from(entry_int(doc, &dict, b"Width", 0)),
        );
        node.insert(
            "Height".into(),
            Value::from(entry_int(doc, &dict, b"Height", 0)),
        );
        // /ColorSpace: a name, or the first name of an array form.
        if let Ok(cs) = dict.get(b"ColorSpace") {
            let (_, resolved) = doc
                .dereference(cs)
                .map_err(|e| crate::error::AppError::Pdf(e.to_string()))?;
            let cs_name = match resolved {
                Object::Name(name) => Some(String::from_utf8_lossy(name).into_owned()),
                Object::Array(items) => items.first().and_then(|f| {
                    let (_, f) = doc.dereference(f).ok()?;
                    f.as_name()
                        .ok()
                        .map(|n| String::from_utf8_lossy(n).into_owned())
                }),
                _ => None,
            };
            if let Some(cs_name) = cs_name {
                node.insert("ColorSpace".into(), Value::String(cs_name));
            }
        }
        node.insert(
            "BitsPerComponent".into(),
            Value::from(entry_int(doc, &dict, b"BitsPerComponent", 1)),
        );
        out.push(Value::Object(node));
    }
    Ok(out)
}

fn page_links(doc: &Document, page_id: lopdf::ObjectId) -> AppResult<Vec<Value>> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for annot in pdfdoc::annotations(doc, page_id)? {
        if pdfdoc::annotation_subtype(&annot) != "Link" {
            continue;
        }
        let Some(action) = annot
            .get(b"A")
            .ok()
            .and_then(|a| pdfdoc::dict_owned(doc, a).ok().flatten())
        else {
            continue;
        };
        if entry_name(doc, &action, b"S").as_deref() != Some("URI") {
            continue;
        }
        if let Some(uri) = action.get(b"URI").ok().and_then(|u| text(doc, u)) {
            if seen.insert(uri.clone()) {
                out.push(json!({ "URI": uri }));
            }
        }
    }
    Ok(out)
}

/// Per-font node like Java's: descriptor flags become booleans, identical
/// nodes merge into one entry with an incremented `Count`.
fn page_fonts(doc: &Document, page_id: lopdf::ObjectId) -> AppResult<Vec<Value>> {
    let mut unique: HashMap<String, (Map<String, Value>, u64)> = HashMap::new();
    for (_, font) in pdfdoc::resource_entries(doc, page_id, b"Font")? {
        let mut node = Map::new();
        node.insert(
            "IsEmbedded".into(),
            Value::Bool(font_is_embedded(doc, &font)),
        );
        node.insert(
            "Name".into(),
            null_if_none(entry_name(doc, &font, b"BaseFont")),
        );
        node.insert(
            "Subtype".into(),
            null_if_none(entry_name(doc, &font, b"Subtype")),
        );
        if let Some(desc) = font_descriptor(doc, &font) {
            node.insert(
                "ItalicAngle".into(),
                Value::from(desc_real(doc, &desc, b"ItalicAngle")),
            );
            let flags = entry_int(doc, &desc, b"Flags", 0) as u32;
            node.insert("IsItalic".into(), Value::Bool(flags & 1 != 0));
            node.insert("IsBold".into(), Value::Bool(flags & 64 != 0));
            node.insert("IsFixedPitch".into(), Value::Bool(flags & 2 != 0));
            node.insert("IsSerif".into(), Value::Bool(flags & 4 != 0));
            node.insert("IsSymbolic".into(), Value::Bool(flags & 8 != 0));
            node.insert("IsScript".into(), Value::Bool(flags & 16 != 0));
            node.insert("IsNonsymbolic".into(), Value::Bool(flags & 32 != 0));
            node.insert(
                "FontFamily".into(),
                null_if_none(entry_text(doc, &desc, b"FontFamily")),
            );
            node.insert(
                "FontWeight".into(),
                Value::from(entry_int(doc, &desc, b"FontWeight", 0)),
            );
        }
        let key = serde_json::to_string(&Value::Object(node.clone())).unwrap_or_default();
        match unique.get_mut(&key) {
            Some((_, count)) => *count += 1,
            None => {
                unique.insert(key, (node, 1));
            }
        }
    }
    let mut out: Vec<Value> = unique
        .into_values()
        .map(|(mut node, count)| {
            node.insert("Count".into(), Value::from(count));
            Value::Object(node)
        })
        .collect();
    out.sort_by_key(|v| serde_json::to_string(v).unwrap_or_default());
    Ok(out)
}

/// `/FontDescriptor` of a font dict, descending into `/DescendantFonts`
/// for Type0 fonts like `PDFont.getFontDescriptor()`.
fn font_descriptor(doc: &Document, font: &Dictionary) -> Option<Dictionary> {
    if let Some(desc) = entry_dict(doc, font, b"FontDescriptor") {
        return Some(desc);
    }
    let descendants = font.get(b"DescendantFonts").ok()?;
    let (_, resolved) = doc.dereference(descendants).ok()?;
    let Object::Array(items) = resolved else {
        return None;
    };
    let descendant = pdfdoc::dict_owned(doc, items.first()?).ok().flatten()?;
    entry_dict(doc, &descendant, b"FontDescriptor")
}

fn font_is_embedded(doc: &Document, font: &Dictionary) -> bool {
    let Some(desc) = font_descriptor(doc, font) else {
        return false;
    };
    [
        b"FontFile".as_slice(),
        b"FontFile2".as_slice(),
        b"FontFile3".as_slice(),
    ]
    .iter()
    .any(|key| desc.get(key).is_ok())
}

fn desc_real(doc: &Document, dict: &Dictionary, key: &[u8]) -> f64 {
    dict.get(key)
        .ok()
        .and_then(|o| {
            let (_, resolved) = doc.dereference(o).ok()?;
            match resolved {
                Object::Real(f) => Some(*f as f64),
                Object::Integer(i) => Some(*i as f64),
                _ => None,
            }
        })
        .unwrap_or(0.0)
}

fn xobject_counts(doc: &Document, page_id: lopdf::ObjectId) -> AppResult<Value> {
    let mut counts: HashMap<&'static str, u64> = HashMap::new();
    for (_, dict) in pdfdoc::resource_entries(doc, page_id, b"XObject")? {
        let kind = match entry_name(doc, &dict, b"Subtype").as_deref() {
            Some("Image") => "Image",
            Some("Form") => "Form",
            _ => "Other",
        };
        *counts.entry(kind).or_insert(0) += 1;
    }
    Ok(Value::Object(
        counts
            .into_iter()
            .map(|(k, v)| (k.to_string(), Value::from(v)))
            .collect(),
    ))
}

fn multimedia(doc: &Document, page_id: lopdf::ObjectId) -> AppResult<Vec<Value>> {
    let mut out = Vec::new();
    for annot in pdfdoc::annotations(doc, page_id)? {
        if pdfdoc::annotation_subtype(&annot) == "RichMedia" {
            out.push(json!({
                "Subtype": "RichMedia",
                "Contents": null_if_none(entry_text(doc, &annot, b"Contents")),
            }));
        }
    }
    Ok(out)
}

fn single_page_info(
    doc: &Document,
    page_id: lopdf::ObjectId,
    page_number: u32,
) -> AppResult<Value> {
    let mut page = Map::new();

    let boxes = pdfdoc::page_boxes(doc, page_id)?;
    let (width, height) = (
        (boxes.media[2] - boxes.media[0]) as f64,
        (boxes.media[3] - boxes.media[1]) as f64,
    );

    let mut size = Map::new();
    let width_in = width / 72.0;
    let height_in = height / 72.0;
    size.insert("Width (px)".into(), Value::String(fmt2(width)));
    size.insert("Height (px)".into(), Value::String(fmt2(height)));
    size.insert("Width (in)".into(), Value::String(fmt2(width_in)));
    size.insert("Height (in)".into(), Value::String(fmt2(height_in)));
    size.insert("Width (cm)".into(), Value::String(fmt2(width_in * 2.54)));
    size.insert("Height (cm)".into(), Value::String(fmt2(height_in * 2.54)));
    size.insert(
        "Standard Page".into(),
        Value::String(
            standard_page_size(
                boxes.media[2] - boxes.media[0],
                boxes.media[3] - boxes.media[1],
            )
            .into(),
        ),
    );
    page.insert("Size".into(), Value::Object(size));

    page.insert(
        "Rotation".into(),
        Value::from(pdfdoc::page_rotation(doc, page_id)?),
    );
    page.insert(
        "Page Orientation".into(),
        Value::String(
            page_orientation(
                boxes.media[2] - boxes.media[0],
                boxes.media[3] - boxes.media[1],
            )
            .into(),
        ),
    );

    page.insert("MediaBox".into(), Value::String(rect_string(boxes.media)));
    page.insert("CropBox".into(), Value::String(rect_string(boxes.crop)));
    page.insert("BleedBox".into(), Value::String(rect_string(boxes.bleed)));
    page.insert("TrimBox".into(), Value::String(rect_string(boxes.trim)));
    page.insert("ArtBox".into(), Value::String(rect_string(boxes.art)));

    // PDFTextStripper for one page. Pages without a content stream (or
    // with unreadable one) yield no text rather than dropping the entry —
    // PDFBox returns "" where lopdf errors.
    let page_text = doc
        .extract_text_chunks(&[page_number])
        .into_iter()
        .next()
        .and_then(|r| r.ok())
        .unwrap_or_default();
    page.insert(
        "Text Characters Count".into(),
        Value::from(page_text.encode_utf16().count() as u64),
    );

    let annots = pdfdoc::annotations(doc, page_id)?;
    let (total, subtype, contents) = annotation_counts(&annots);
    page.insert(
        "Annotations".into(),
        json!({
            "AnnotationsCount": total,
            "SubtypeCount": subtype,
            "ContentsCount": contents,
        }),
    );

    if pdfdoc::page_resources_dict(doc, page_id)?.is_some() {
        page.insert("Images".into(), Value::Array(page_images(doc, page_id)?));
        page.insert("Links".into(), Value::Array(page_links(doc, page_id)?));
        page.insert("Fonts".into(), Value::Array(page_fonts(doc, page_id)?));
        page.insert("XObjectCounts".into(), xobject_counts(doc, page_id)?);
    }

    page.insert("Multimedia".into(), Value::Array(multimedia(doc, page_id)?));

    Ok(Value::Object(page))
}

fn per_page_info(doc: &Document) -> Value {
    let mut out = Map::new();
    for (number, page_id) in doc.get_pages() {
        match single_page_info(doc, page_id, number) {
            Ok(info) => {
                out.insert(format!("Page {number}"), info);
            }
            Err(e) => {
                tracing::warn!(page = number, error = %e, "skipping page info");
            }
        }
    }
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// Endpoint
// ---------------------------------------------------------------------------

/// `POST /api/v1/security/get-info-on-pdf` → the full inspection report as
/// JSON (`response.json` in Java; the frontend reads the parsed body).
#[stirling_macros::endpoint]
fn get_info_on_pdf(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    _params: Vec<(String, String)>,
) -> AppResult<Json<Value>> {
    let (_, file_size, document) = load_pdf(uploads)?;
    let page_numbers: Vec<u32> = document.get_pages().keys().copied().collect();

    let mut out = Map::new();
    out.insert("Metadata".into(), metadata(&document));
    out.insert(
        "BasicInfo".into(),
        basic_info(&document, file_size, &page_numbers),
    );
    out.insert("DocumentInfo".into(), document_info(&document));
    out.insert("Compliancy".into(), compliancy(&document));
    out.insert("Encryption".into(), encryption(&document));
    out.insert("Permissions".into(), permissions(&document));
    out.insert("FormFields".into(), form_fields(&document));
    out.insert("Other".into(), other(&document));
    out.insert("PerPageInfo".into(), per_page_info(&document));

    let summary = summary_data(&document);
    if summary.as_object().is_some_and(|o| !o.is_empty()) {
        out.insert("SummaryData".into(), summary);
    }

    Ok(Json(Value::Object(out)))
}

fn form_fields(doc: &Document) -> Value {
    let mut out = Map::new();
    if let Ok(entries) = pdfdoc::form_field_entries(doc) {
        for (name, value) in entries {
            out.insert(name, null_if_none(value));
        }
    }
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// sanitize-pdf
// ---------------------------------------------------------------------------

/// `POST /api/v1/security/sanitize-pdf` — strips JavaScript, embedded
/// files, XMP/Info metadata, launch/URI link actions and page fonts,
/// each behind its own flag. Java defaults: JavaScript and embedded files
/// on, the rest off (`SanitizePdfRequest` field initializers).
#[stirling_macros::endpoint]
fn sanitize_pdf(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let (name, _size, mut document) = load_pdf(uploads)?;
    let flag = |key: &str, default: &str| parse_bool(param(&params, key, default), key);

    if flag("removeJavaScript", "true")? {
        remove_javascript(&mut document)?;
    }
    if flag("removeEmbeddedFiles", "true")? {
        remove_embedded_files(&mut document)?;
    }
    if flag("removeXMPMetadata", "false")? {
        remove_catalog_key(&mut document, b"Metadata");
    }
    if flag("removeMetadata", "false")? {
        // Java swaps in an empty PDDocumentInformation.
        let info_id = document.new_object_id();
        document
            .objects
            .insert(info_id, Object::Dictionary(Dictionary::new()));
        document.trailer.set("Info", info_id);
    }
    if flag("removeLinks", "false")? {
        remove_link_actions(&mut document)?;
    }
    if flag("removeFonts", "false")? {
        remove_page_fonts(&mut document);
    }

    let bytes = pdfdoc::save(&mut document)?;
    Ok(pdf_response(
        bytes,
        &output_name(Some(&name), "_sanitized.pdf"),
    ))
}

/// `POST /api/v1/security/remove-cert-sign` — removes root-level signature
/// fields (`/FT /Sig` entries of the AcroForm `/Fields` array) like
/// `PDAcroForm.flatten(fields, false)`: widget appearances are drawn into
/// the page, widget annotations and the field entries are dropped, `/XFA`
/// is removed, and `/SigFlags` goes once no `/V` signature dictionary
/// remains. Documents without signature fields are re-saved unchanged.
#[stirling_macros::endpoint]
fn remove_cert_sign(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    _params: Vec<(String, String)>,
) -> AppResult<Response> {
    let (name, _size, mut document) = load_pdf(uploads)?;
    crate::forms::remove_signature_fields(&mut document)?;
    pdfdoc::touch_loaded_info(&mut document);
    let bytes = pdfdoc::save(&mut document)?;
    Ok(pdf_response(
        bytes,
        &output_name(Some(&name), "_unsigned.pdf"),
    ))
}

fn catalog_id(doc: &Document) -> AppResult<ObjectId> {
    match doc.trailer.get(b"Root").ok() {
        Some(Object::Reference(id)) => Ok(*id),
        _ => Err(AppError::Pdf("document has no catalog".into())),
    }
}

fn remove_catalog_key(doc: &mut Document, key: &[u8]) {
    if let Ok(id) = catalog_id(doc) {
        if let Ok(dict) = doc.get_dictionary_mut(id) {
            dict.remove(key);
        }
    }
}

/// True when `object` resolves to an action dictionary with `/S /JavaScript`.
fn is_js_action(doc: &Document, object: &Object) -> bool {
    let Ok((_, resolved)) = doc.dereference(object) else {
        return false;
    };
    let Some(dict) = pdfdoc::dict_of(resolved) else {
        return false;
    };
    dict.get(b"S")
        .ok()
        .and_then(|s| {
            let (_, s) = doc.dereference(s).ok()?;
            s.as_name().ok().map(|n| n == b"JavaScript")
        })
        .unwrap_or(false)
}

/// Removes `key` from `dict` when its value is a JavaScript action.
fn strip_js_key(doc: &Document, dict: &mut Dictionary, key: &[u8]) {
    let is_js = dict.get(key).ok().is_some_and(|v| is_js_action(doc, v));
    if is_js {
        dict.remove(key);
    }
}

fn remove_javascript(doc: &mut Document) -> AppResult<()> {
    let cat_id = catalog_id(doc)?;

    // Resolve everything with immutable borrows first; mutation happens in
    // one pass afterwards so `is_js_action` can keep reading the document.
    let catalog = doc
        .get_dictionary(cat_id)
        .map_err(|e| AppError::Pdf(e.to_string()))?
        .clone();
    let open_is_js = catalog
        .get(b"OpenAction")
        .ok()
        .is_some_and(|o| is_js_action(doc, o));
    let aa_stripped = catalog.get(b"AA").ok().and_then(|aa| {
        let (_, resolved) = doc.dereference(aa).ok()?;
        let mut d = pdfdoc::dict_of(resolved)?.clone();
        for key in [&b"WC"[..], &b"WS"[..], &b"DS"[..], &b"WP"[..], &b"DP"[..]] {
            strip_js_key(doc, &mut d, key);
        }
        Some(d)
    });
    let names_ref = catalog
        .get(b"Names")
        .ok()
        .and_then(|o| o.as_reference().ok());
    let names_direct = catalog.get(b"Names").ok().cloned();

    if let Some(id) = names_ref {
        if let Some(Object::Dictionary(dict)) = doc.objects.get_mut(&id) {
            dict.remove(b"JavaScript");
        }
    } else if let Some(Object::Dictionary(mut dict)) = names_direct {
        dict.remove(b"JavaScript");
        if let Ok(catalog) = doc.get_dictionary_mut(cat_id) {
            catalog.set("Names", Object::Dictionary(dict));
        }
    }

    {
        let catalog = doc
            .get_dictionary_mut(cat_id)
            .map_err(|e| AppError::Pdf(e.to_string()))?;
        if open_is_js {
            catalog.remove(b"OpenAction");
        }
        if let Some(d) = aa_stripped {
            catalog.set("AA", Object::Dictionary(d));
        }
    }

    // AcroForm field actions: /A plus /AA C,F,K,V on every field and kid.
    let acro_id = doc
        .get_dictionary(cat_id)
        .map_err(|e| AppError::Pdf(e.to_string()))?
        .get(b"AcroForm")
        .ok()
        .and_then(|o| o.as_reference().ok());
    if let Some(id) = acro_id {
        let field_ids: Vec<ObjectId> = doc
            .get_object(id)
            .ok()
            .and_then(|o| pdfdoc::dict_of(o).cloned())
            .and_then(|d| d.get(b"Fields").ok().cloned())
            .and_then(|f| doc.dereference(&f).ok().map(|(_, o)| o.clone()))
            .and_then(|o| o.as_array().ok().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|f| f.as_reference().ok())
            .collect();
        let mut stack = field_ids;
        let mut visited = HashSet::new();
        while let Some(fid) = stack.pop() {
            if !visited.insert(fid) {
                continue;
            }
            let Some(mut field) = doc
                .get_object(fid)
                .ok()
                .and_then(|o| pdfdoc::dict_of(o).cloned())
            else {
                continue;
            };
            if let Ok(aa) = field.get(b"AA") {
                let mut aa_dict = match doc.dereference(aa) {
                    Ok((_, Object::Dictionary(d))) => d.clone(),
                    _ => Dictionary::new(),
                };
                let mut changed = false;
                for key in [&b"C"[..], &b"F"[..], &b"K"[..], &b"V"[..]] {
                    if aa_dict.get(key).ok().is_some_and(|v| is_js_action(doc, v)) {
                        aa_dict.remove(key);
                        changed = true;
                    }
                }
                if changed {
                    field.set("AA", Object::Dictionary(aa_dict));
                }
            }
            // Descend into /Kids (non-terminal fields).
            if let Ok(kids) = field.get(b"Kids") {
                if let Ok((_, Object::Array(items))) = doc.dereference(kids) {
                    for kid in items {
                        if let Ok(kid_id) = kid.as_reference() {
                            stack.push(kid_id);
                        }
                    }
                }
            }
            if let Some(Object::Dictionary(slot)) = doc.objects.get_mut(&fid) {
                *slot = field;
            }
        }
    }

    // Page /AA (O=open, C=close) and widget /A.
    for (_, page_id) in doc.get_pages() {
        let page = doc
            .get_dictionary(page_id)
            .map_err(|e| AppError::Pdf(e.to_string()))?
            .clone();
        if let Ok(aa) = page.get(b"AA") {
            let mut aa_dict = match doc.dereference(aa) {
                Ok((_, Object::Dictionary(d))) => d.clone(),
                _ => Dictionary::new(),
            };
            let mut changed = false;
            for key in [&b"O"[..], &b"C"[..]] {
                if aa_dict.get(key).ok().is_some_and(|v| is_js_action(doc, v)) {
                    aa_dict.remove(key);
                    changed = true;
                }
            }
            if changed {
                doc.get_dictionary_mut(page_id)
                    .map_err(|e| AppError::Pdf(e.to_string()))?
                    .set("AA", Object::Dictionary(aa_dict));
            }
        }
        let annots = page
            .get(b"Annots")
            .ok()
            .and_then(|a| doc.dereference(a).ok())
            .and_then(|(_, o)| o.as_array().ok().cloned())
            .unwrap_or_default();
        for annot in annots {
            let Some(aid) = annot.as_reference().ok() else {
                continue;
            };
            let mut changed = false;
            if let Some(mut adict) = doc
                .get_object(aid)
                .ok()
                .and_then(|o| pdfdoc::dict_of(o).cloned())
            {
                if adict.get(b"Subtype").ok().and_then(|s| s.as_name().ok()) == Some(b"Widget")
                    && adict.get(b"A").ok().is_some_and(|v| is_js_action(doc, v))
                {
                    adict.remove(b"A");
                    changed = true;
                }
                if changed {
                    if let Some(Object::Dictionary(slot)) = doc.objects.get_mut(&aid) {
                        *slot = adict;
                    }
                }
            }
        }
    }
    Ok(())
}

fn remove_embedded_files(doc: &mut Document) -> AppResult<()> {
    // /Names → /EmbeddedFiles.
    if let Ok(cat_id) = catalog_id(doc) {
        if let Some(names_id) = doc
            .get_dictionary(cat_id)
            .map_err(|e| AppError::Pdf(e.to_string()))?
            .get(b"Names")
            .ok()
            .and_then(|o| o.as_reference().ok())
        {
            if let Some(Object::Dictionary(dict)) = doc.objects.get_mut(&names_id) {
                dict.remove(b"EmbeddedFiles");
            }
        }
    }
    // /FileAttachment annotations on every page.
    for (_, page_id) in doc.get_pages() {
        let Ok(page) = doc.get_dictionary(page_id).cloned() else {
            continue;
        };
        let Some(annots_obj) = page.get(b"Annots").ok().cloned() else {
            continue;
        };
        let Ok((_, Object::Array(items))) = doc.dereference(&annots_obj) else {
            continue;
        };
        let items = items.clone();
        let mut keep = Vec::with_capacity(items.len());
        for item in items {
            let is_attachment = item
                .as_reference()
                .ok()
                .and_then(|id| doc.get_object(id).ok())
                .and_then(|o| pdfdoc::dict_of(o))
                .and_then(|d| d.get(b"Subtype").ok())
                .and_then(|s| s.as_name().ok())
                .is_some_and(|n| n == b"FileAttachment");
            if !is_attachment {
                keep.push(item);
            }
        }
        if keep.len()
            != page
                .get(b"Annots")
                .ok()
                .and_then(|a| doc.dereference(a).ok())
                .and_then(|(_, o)| o.as_array().ok())
                .map_or(0, Vec::len)
        {
            doc.get_dictionary_mut(page_id)
                .map_err(|e| AppError::Pdf(e.to_string()))?
                .set("Annots", Object::Array(keep));
        }
    }
    Ok(())
}

fn remove_link_actions(doc: &mut Document) -> AppResult<()> {
    for (_, page_id) in doc.get_pages() {
        let Ok(page) = doc.get_dictionary(page_id).cloned() else {
            continue;
        };
        let Some(annots_obj) = page.get(b"Annots").ok().cloned() else {
            continue;
        };
        let Ok((_, Object::Array(items))) = doc.dereference(&annots_obj) else {
            continue;
        };
        let items = items.clone();
        let mut keep = Vec::with_capacity(items.len());
        for item in items {
            let Some(aid) = item.as_reference().ok() else {
                keep.push(item);
                continue;
            };
            let Some(mut adict) = doc
                .get_object(aid)
                .ok()
                .and_then(|o| pdfdoc::dict_of(o).cloned())
            else {
                keep.push(item);
                continue;
            };
            let is_link =
                adict.get(b"Subtype").ok().and_then(|s| s.as_name().ok()) == Some(b"Link");
            let launch_or_uri = adict
                .get(b"A")
                .ok()
                .and_then(|a| {
                    let (_, resolved) = doc.dereference(a).ok()?;
                    pdfdoc::dict_of(resolved)?.get(b"S").ok().and_then(|s| {
                        let (_, s) = doc.dereference(s).ok()?;
                        s.as_name().ok().map(|n| n == b"Launch" || n == b"URI")
                    })
                })
                .unwrap_or(false);
            if is_link && launch_or_uri {
                adict.remove(b"A");
                if let Some(Object::Dictionary(slot)) = doc.objects.get_mut(&aid) {
                    *slot = adict;
                }
            }
            keep.push(item);
        }
        doc.get_dictionary_mut(page_id)
            .map_err(|e| AppError::Pdf(e.to_string()))?
            .set("Annots", Object::Array(keep));
    }
    Ok(())
}

fn remove_page_fonts(doc: &mut Document) {
    for (_, page_id) in doc.get_pages() {
        let Some((holder, resources)) = pdfdoc::page_resources_holder(doc, page_id) else {
            continue;
        };
        let mut object = match doc.dereference(&resources) {
            Ok((_, o)) => o.clone(),
            Err(_) => continue,
        };
        let Some(dict) = pdfdoc::dict_mut(&mut object) else {
            continue;
        };
        dict.remove(b"Font");
        // Write the edited resources dict back through its holder slot.
        let holder_dict = match doc.get_dictionary_mut(holder) {
            Ok(d) => d,
            Err(_) => continue,
        };
        match resources {
            Object::Reference(id) => {
                if let Some(slot) = doc.objects.get_mut(&id) {
                    *slot = object;
                }
            }
            _ => {
                holder_dict.set("Resources", object);
            }
        }
    }
}

/// `POST /api/v1/security/redact` — `ManualRedactPdfRequest`: black out
/// user-marked `redactions` areas and/or wipe whole `pageNumbers` pages.
/// Java's PDFBox path is itself overlay-only (filled rects appended to
/// the content stream); this port reproduces that exactly, including the
/// top-left-origin → PDF-bottom-left coordinate flip against the crop
/// box height. `convertPDFToImage` rasterises every page through
/// Ghostscript at 300 DPI — Java's `PDF_TO_IMAGE_DPI` — and rebuilds the
/// document image-only.
///
/// `/auto-redact` and `/redact-execute` are handled below: they share the
/// same overlay approach since jpdfium's verified content removal has no
/// lopdf equivalent.
pub async fn redact(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, params) = receive_multipart(multipart).await?;
    misc::dispatch(state, query, uploads, params, redact_work).await
}

#[derive(Deserialize)]
struct RedactionArea {
    x: Option<f64>,
    y: Option<f64>,
    width: Option<f64>,
    height: Option<f64>,
    page: Option<i64>,
    color: Option<String>,
}

fn redact_work(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    let mut document = pdfdoc::load(&upload.name, &upload.bytes)?;
    let page_ids = pdfdoc::page_ids(&document);
    let total = page_ids.len();

    // `pageNumbers` shadows the shared default with no fallback: absent
    // or blank wipes nothing, matching `getPageNumbers`' null branch.
    if let Some(selection) = last_param(&params, "pageNumbers").filter(|s| !s.trim().is_empty()) {
        page_select::require_valid_page_selection(selection, total)
            .map_err(AppError::BadRequest)?;
        let mut wipes =
            page_select::parse_page_list(selection, total).map_err(AppError::BadRequest)?;
        wipes.sort_unstable();
        let (r, g, b) = redact_color(last_param(&params, "pageRedactionColor"));
        for index in wipes {
            let page_id = page_ids[index];
            let crop = pdfdoc::page_boxes(&document, page_id)?.crop;
            let (w, h) = (f64::from(crop[2] - crop[0]), f64::from(crop[3] - crop[1]));
            misc::append_page_content(
                &mut document,
                page_id,
                &format!("q {r} {g} {b} rg 0 0 {w} {h} re f Q\n"),
            )?;
        }
    }

    if let Some(raw) = last_param(&params, "redactions") {
        let areas: Vec<RedactionArea> = serde_json::from_str(raw)
            .map_err(|e| AppError::BadRequest(format!("invalid redactions: {e}")))?;
        for area in areas {
            // Java skips entries with no page or non-positive size;
            // a missing x/y crashes its floatValue() call, so refuse
            // rather than silently dropping the mark.
            let (Some(page), Some(width), Some(height)) = (area.page, area.width, area.height)
            else {
                continue;
            };
            if page <= 0 || width <= 0.0 || height <= 0.0 || page as usize > total {
                continue;
            }
            let (Some(x), Some(y)) = (area.x, area.y) else {
                return Err(AppError::BadRequest(
                    "redactions entries require numeric x and y".into(),
                ));
            };
            let page_id = page_ids[page as usize - 1];
            let crop = pdfdoc::page_boxes(&document, page_id)?.crop;
            let bbox_h = f64::from(crop[3] - crop[1]);
            let pdf_y = bbox_h - y - height;
            let (r, g, b) = redact_color(area.color.as_deref());
            misc::append_page_content(
                &mut document,
                page_id,
                &format!("q {r} {g} {b} rg {x} {pdf_y} {width} {height} re f Q\n"),
            )?;
        }
    }

    pdfdoc::touch_loaded_info(&mut document);
    let bytes = pdfdoc::save(&mut document)?;
    let bytes = if param_bool(&params, "convertPDFToImage") {
        rasterize_pdf(&upload.name, &bytes)?
    } else {
        bytes
    };
    let simple = upload
        .name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&upload.name);
    let stem = crate::upload::remove_extension(Some(simple));
    Ok(pdf_response(bytes, &format!("{stem}_redacted.pdf")))
}

/// `ManualRedactionService.decodeOrDefault`: `Color.decode` on the hex
/// string (alpha ignored by `setNonStrokingColor`), BLACK on null or any
/// parse failure.
fn redact_color(value: Option<&str>) -> (f64, f64, f64) {
    let Some(value) = value else {
        return (0.0, 0.0, 0.0);
    };
    let hex = value.strip_prefix('#').unwrap_or(value);
    if !hex.is_ascii() {
        return (0.0, 0.0, 0.0);
    }
    let rgb = if hex.len() == 8 {
        &hex[2..]
    } else if hex.len() == 6 {
        hex
    } else {
        return (0.0, 0.0, 0.0);
    };
    let channel = |i: usize| u8::from_str_radix(&rgb[i..i + 2], 16).ok();
    match (channel(0), channel(2), channel(4)) {
        (Some(r), Some(g), Some(b)) => (
            f64::from(r) / 255.0,
            f64::from(g) / 255.0,
            f64::from(b) / 255.0,
        ),
        _ => (0.0, 0.0, 0.0),
    }
}

fn last_param<'a>(params: &'a [(String, String)], name: &str) -> Option<&'a str> {
    params
        .iter()
        .rev()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

/// Every value of a repeated multipart field, in order — Spring binds
/// `List` fields from repeated params the same way.
fn all_params<'a>(params: &'a [(String, String)], name: &str) -> Vec<&'a str> {
    params
        .iter()
        .filter(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
        .collect()
}

/// `X-Stirling-Redaction-Matches` — `RedactController.MATCHES_HEADER`.
const MATCHES_HEADER: &str = "x-stirling-redaction-matches";

/// `POST /api/v1/security/auto-redact` — `RedactPdfRequest`: overlay boxes
/// over every text match of the newline-separated `listOfText` terms.
///
/// jpdfium's `PdfRedactor` does verified content removal and has no lopdf
/// equivalent, so this always runs Java's documented fallback: locate matches
/// with [`pdftext::redact`] and paint cover rectangles, exactly what
/// `RedactController` does when native redaction throws. `convertPDFToImage`
/// rasterises pages through Ghostscript like `/redact`; regexes the
/// `regex` crate cannot compile (Java lookaround/backreferences/possessive
/// quantifiers) are rejected.
pub async fn auto_redact(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, params) = receive_multipart(multipart).await?;
    if auto_redact_regex_unsupported(&params) {
        return Err(AppError::BadRequest(
            "useRegex terms use syntax the native matcher cannot compile".into(),
        ));
    }
    misc::dispatch(state, query, uploads, params, auto_redact_work).await
}

/// True when `useRegex` terms fail the `regex` crate — valid Java syntax the
/// native matcher cannot honour, so the request is rejected.
fn auto_redact_regex_unsupported(params: &[(String, String)]) -> bool {
    if !param_bool(params, "useRegex") {
        return false;
    }
    let terms = pdftext::redact::parse_terms(last_param(params, "listOfText").unwrap_or(""));
    pdftext::redact::compile_terms(
        &terms,
        true,
        param_bool(params, "wholeWordSearch"),
        param_bool(params, "caseSensitive"),
    )
    .is_err()
}

fn auto_redact_work(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    if upload.bytes.is_empty() {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is empty")));
    }
    let raw = last_param(&params, "listOfText").unwrap_or("");
    if raw.trim().is_empty() {
        return Err(AppError::BadRequest(
            "No text patterns provided for redaction".into(),
        ));
    }
    let terms = pdftext::redact::parse_terms(raw);
    if terms.is_empty() {
        return Err(AppError::BadRequest(
            "No text patterns provided for redaction".into(),
        ));
    }
    let use_regex = param_bool(&params, "useRegex");
    let patterns = pdftext::redact::compile_terms(
        &terms,
        use_regex,
        param_bool(&params, "wholeWordSearch"),
        param_bool(&params, "caseSensitive"),
    )
    .map_err(|e| AppError::BadRequest(format!("Invalid regex pattern: {e}")))?;

    let mut document = pdfdoc::load(&upload.name, &upload.bytes)?;
    let page_ids = pdfdoc::page_ids(&document);
    if page_ids.is_empty() {
        return Err(AppError::Pdf("Invalid or empty PDF document".into()));
    }
    let page_texts = pdftext::model::document_text(&document);
    let heights = crop_heights(&document, &page_ids)?;
    let hits = pdftext::redact::find_matches(&page_texts, &heights, &patterns);

    let color = redact_color(last_param(&params, "redactColor"));
    let padding = param_f64(&params, "customPadding", 0.0) as f32;
    overlay_found_text(&mut document, &page_ids, &hits, padding, color)?;
    if !hits.is_empty() {
        clean_redaction_metadata(&mut document)?;
    }
    pdfdoc::touch_loaded_info(&mut document);
    let bytes = pdfdoc::save(&mut document)?;
    let bytes = if param_bool(&params, "convertPDFToImage") {
        rasterize_pdf(&upload.name, &bytes)?
    } else {
        bytes
    };
    let mut response = pdf_response(bytes, &redacted_name(&upload.name));
    if let Ok(value) = axum::http::HeaderValue::from_str(&hits.len().to_string()) {
        response.headers_mut().insert(MATCHES_HEADER, value);
    }
    Ok(response)
}

/// Crop-box heights per page — the flip value PDFBox's stripper uses.
fn crop_heights(document: &Document, page_ids: &[ObjectId]) -> AppResult<Vec<f32>> {
    page_ids
        .iter()
        .map(|id| {
            let crop = pdfdoc::page_boxes(document, *id)?.crop;
            Ok(crop[3] - crop[1])
        })
        .collect()
}

fn crop_dims(document: &Document, page_id: ObjectId) -> AppResult<(f32, f32)> {
    let crop = pdfdoc::page_boxes(document, page_id)?.crop;
    Ok((crop[3] - crop[1], crop[2] - crop[0]))
}

fn redacted_name(original: &str) -> String {
    let simple = original.rsplit(['/', '\\']).next().unwrap_or(original);
    format!(
        "{}_redacted.pdf",
        crate::upload::remove_extension(Some(simple))
    )
}

/// `PdfUtils.convertPdfToPdfImage`: Ghostscript `png16m` at 300 DPI —
/// Java's `PDF_TO_IMAGE_DPI`; `system.maxDPI` has no Rust counterpart —
/// then one full-page lossless image per page on a fresh document,
/// matching `LosslessFactory.createFromImage` + `drawImage(0, 0, w, h)`.
/// Java's renderer is PDFBox; Ghostscript is the Rust-side rasteriser
/// the other endpoints already use.
fn rasterize_pdf(name: &str, bytes: &[u8]) -> AppResult<Vec<u8>> {
    let src = pdfdoc::load(name, bytes)?;
    let src_pages = pdfdoc::page_ids(&src);
    if src_pages.is_empty() {
        return Err(AppError::Pdf("document has no pages".into()));
    }
    let dir = tempfile::tempdir()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("rasterize tempdir: {e}")))?;
    let input = dir.path().join("input.pdf");
    std::fs::write(&input, bytes)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("write rasterize input: {e}")))?;
    let spec = crate::convert::EngineSpec::new(&crate::settings::engine_program("gs"))
        .args([
            "-dNOPAUSE".to_string(),
            "-dBATCH".to_string(),
            "-dQUIET".to_string(),
            "-dSAFER".to_string(),
            "-sDEVICE=png16m".to_string(),
            "-r300".to_string(),
            format!("-sOutputFile={}", dir.path().join("page-%d.png").display()),
        ])
        .arg(input.to_string_lossy().to_string())
        .timeout(std::time::Duration::from_secs(600));
    let out = crate::convert::run(&spec)?;
    crate::convert::require_success("gs", &out)?;

    let mut pages: Vec<std::path::PathBuf> = std::fs::read_dir(dir.path())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("read rasterize output: {e}")))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("page-"))
        })
        .collect();
    pages.sort_by_key(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n[5..].split('.').next()?.parse::<u32>().ok())
            .unwrap_or(0)
    });
    if pages.len() < src_pages.len() {
        return Err(AppError::Pdf(
            "ghostscript produced fewer page images than pages".into(),
        ));
    }

    let mut doc = pdfdoc::new_document();
    for (i, src_page) in src_pages.iter().enumerate() {
        let png = std::fs::read(&pages[i]).map_err(|e| {
            AppError::Pdf(format!("ghostscript produced no page {} image: {e}", i + 1))
        })?;
        let rgb = image::load_from_memory(&png)
            .map_err(|e| AppError::Pdf(format!("cannot decode rendered page {}: {e}", i + 1)))?
            .to_rgb8();
        let (width, height) = rgb.dimensions();

        let mut img_dict = Dictionary::new();
        img_dict.set("Type", Object::Name(b"XObject".to_vec()));
        img_dict.set("Subtype", Object::Name(b"Image".to_vec()));
        img_dict.set("Width", Object::Integer(width as i64));
        img_dict.set("Height", Object::Integer(height as i64));
        img_dict.set("ColorSpace", Object::Name(b"DeviceRGB".to_vec()));
        img_dict.set("BitsPerComponent", Object::Integer(8));
        let mut img_stream = Stream::new(img_dict, rgb.into_raw());
        let _ = img_stream.compress();
        let img_id = doc.new_object_id();
        doc.objects.insert(img_id, Object::Stream(img_stream));

        // Java sizes the new page to the source MediaBox width/height at
        // the origin — `new PDRectangle(width, height)`.
        let mb = pdfdoc::media_box(&src, *src_page)?;
        let (w, h) = (mb[2] - mb[0], mb[3] - mb[1]);
        let mut xobjects = Dictionary::new();
        xobjects.set("Im0", Object::Reference(img_id));
        let mut resources = Dictionary::new();
        resources.set("XObject", Object::Dictionary(xobjects));

        let mut page = Dictionary::new();
        page.set("Type", Object::Name(b"Page".to_vec()));
        page.set(
            "MediaBox",
            Object::Array(vec![
                Object::Real(0.0),
                Object::Real(0.0),
                Object::Real(w as f32),
                Object::Real(h as f32),
            ]),
        );
        page.set("Resources", Object::Dictionary(resources));
        let page_id = pdfdoc::append_page(&mut doc, page)?;

        let content = format!(
            "q {} 0 0 {} 0 0 cm /Im0 Do Q\n",
            pdfdoc::pdf_num(w),
            pdfdoc::pdf_num(h)
        );
        let content_id = doc.new_object_id();
        doc.objects.insert(
            content_id,
            Object::Stream(Stream::new(Dictionary::new(), content.into_bytes())),
        );
        doc.get_dictionary_mut(page_id)
            .map_err(|e| AppError::Pdf(format!("page dict: {e}")))?
            .set("Contents", Object::Reference(content_id));
    }
    pdfdoc::save(&mut doc)
}

/// `ManualRedactionService.redactFoundText` + the annotation sweep: padded
/// cover rectangles per page, then drop annotations whose rect overlaps a
/// block (so redacted link targets stay hidden).
fn overlay_found_text(
    document: &mut Document,
    page_ids: &[ObjectId],
    hits: &[pdftext::redact::FoundText],
    custom_padding: f32,
    color: (f64, f64, f64),
) -> AppResult<()> {
    let mut by_page: HashMap<usize, Vec<&pdftext::redact::FoundText>> = HashMap::new();
    for hit in hits {
        by_page.entry(hit.page_index).or_default().push(hit);
    }
    let (r, g, b) = color;
    for (index, blocks) in by_page {
        let Some(&page_id) = page_ids.get(index) else {
            continue;
        };
        let (page_h, _) = crop_dims(document, page_id)?;
        let mut ops = format!("q {r} {g} {b} rg\n");
        for block in &blocks {
            let (x, y, w, h) = pdftext::redact::cover_rect(block, page_h, custom_padding, false);
            ops.push_str(&format!("{x} {y} {w} {h} re\n"));
        }
        ops.push_str("f Q\n");
        misc::append_page_content(document, page_id, &ops)?;
        prune_overlapping_annots(document, page_id, &blocks, page_h, custom_padding)?;
    }
    Ok(())
}

/// Removes `/Annots` entries whose `/Rect` intersects any padded block,
/// converted to PDF user space the way `redactFoundText` tests them.
fn prune_overlapping_annots(
    document: &mut Document,
    page_id: ObjectId,
    blocks: &[&pdftext::redact::FoundText],
    page_height: f32,
    custom_padding: f32,
) -> AppResult<()> {
    let annots_object = document
        .get_dictionary(page_id)
        .map_err(|e| AppError::Pdf(e.to_string()))?
        .get(b"Annots")
        .ok()
        .cloned();
    let Some(annots_object) = annots_object else {
        return Ok(());
    };
    let Ok((_, resolved)) = document.dereference(&annots_object) else {
        return Ok(());
    };
    let Object::Array(items) = resolved else {
        return Ok(());
    };
    let items = items.clone();

    let mut keep = Vec::with_capacity(items.len());
    for item in &items {
        let mut drop_annot = false;
        if let Ok((_, Object::Dictionary(dict))) = document.dereference(item) {
            if let Some(rect) = dict.get(b"Rect").ok().and_then(|r| rect_array(document, r)) {
                for block in blocks {
                    let padding = (block.y2 - block.y1) * 0.6 + f64::from(custom_padding);
                    let bx1 = block.x1;
                    let bx2 = block.x2;
                    let by1 = f64::from(page_height) - block.y2 - padding;
                    let by2 = f64::from(page_height) - block.y1 + padding;
                    if rect[0] < bx2 && rect[2] > bx1 && rect[1] < by2 && rect[3] > by1 {
                        drop_annot = true;
                        break;
                    }
                }
            }
        }
        if !drop_annot {
            keep.push(item.clone());
        }
    }
    if keep.len() != items.len() {
        document
            .get_dictionary_mut(page_id)
            .map_err(|e| AppError::Pdf(e.to_string()))?
            .set("Annots", Object::Array(keep));
    }
    Ok(())
}

/// `/Rect` as four PDF-space numbers; `None` when absent or malformed —
/// Java keeps annotations it cannot measure.
fn rect_array(document: &Document, object: &Object) -> Option<[f64; 4]> {
    let (_, resolved) = document.dereference(object).ok()?;
    let Object::Array(items) = resolved else {
        return None;
    };
    if items.len() != 4 {
        return None;
    }
    let mut rect = [0.0; 4];
    for (i, item) in items.iter().enumerate() {
        rect[i] = crate::text::num(item)?;
    }
    Some(rect)
}

/// `ManualRedactionService.cleanDocumentMetadata`: strip author/subject/
/// keywords, stamp ModDate, drop XMP metadata — only when blocks were drawn.
fn clean_redaction_metadata(document: &mut Document) -> AppResult<()> {
    if let Some(mut info) = pdfdoc::info_dict(document) {
        for key in [
            b"Author".as_slice(),
            b"Subject".as_slice(),
            b"Keywords".as_slice(),
        ] {
            info.remove(key);
        }
        info.set(
            "ModDate",
            Object::string_literal(crate::attachments::pdf_date_now().as_str()),
        );
        document.trailer.set("Info", Object::Dictionary(info));
    }
    if let Ok(root) = document.trailer.get(b"Root").and_then(|o| o.as_reference()) {
        if let Ok(catalog) = document.get_dictionary_mut(root) {
            catalog.remove(b"Metadata");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// POST /api/v1/security/redact-execute — `RedactExecuteRequest`
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
struct ExecuteStyle {
    color: String,
    padding: f32,
    convert_to_image: bool,
    image_finalize: bool,
}

impl ExecuteStyle {
    /// `style.convert_to_image || strategy == IMAGE_FINALIZE`.
    fn rasterizes(&self) -> bool {
        self.convert_to_image || self.image_finalize
    }
}

#[derive(Clone, Debug)]
struct TextRangeReq {
    start: String,
    end: String,
}

#[derive(Clone, Debug)]
struct ImageBoxReq {
    page_index: i64,
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
}

#[derive(Clone, Debug, Default)]
struct ExecuteRequest {
    text_values: Vec<String>,
    regex_patterns: Vec<String>,
    /// `None` entries survive like Java's nullable `List<Integer>` so
    /// `requirePagesExist` can reject them.
    wipe_pages: Vec<Option<i64>>,
    ranges: Vec<TextRangeReq>,
    image_boxes: Vec<ImageBoxReq>,
    /// `None` = field absent (skipped); `Some(vec![])` = every page.
    redact_image_pages: Option<Vec<Option<i64>>>,
    style: ExecuteStyle,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StyleJson {
    color: Option<String>,
    padding: Option<f32>,
    convert_to_image: Option<bool>,
    strategy: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RangeJson {
    start_string: Option<String>,
    end_string: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImageBoxJson {
    page_index: Option<i64>,
    x1: Option<f64>,
    y1: Option<f64>,
    x2: Option<f64>,
    y2: Option<f64>,
}

/// `RedactExecuteRequest` bound from multipart fields. `ranges`, `imageBoxes`
/// and `style` arrive as JSON (the `JsonList/JsonObjectPropertyEditor`
/// fields); the plain list fields follow Spring's repeated-param/comma
/// binding, with a JSON array accepted as a lenient extension.
fn parse_execute_request(params: &[(String, String)]) -> AppResult<ExecuteRequest> {
    let text_values = string_list_param(params, "textValues")?;
    let regex_patterns = string_list_param(params, "regexPatterns")?;
    let wipe_pages = int_list_param(params, "wipePages")?;
    let redact_image_pages = match last_param(params, "redactImagePages") {
        None => None,
        Some(raw) if raw.trim().eq_ignore_ascii_case("null") => None,
        Some(_) => Some(int_list_param(params, "redactImagePages")?),
    };

    let mut ranges = Vec::new();
    if let Some(raw) = last_param(params, "ranges") {
        let parsed: Vec<RangeJson> = parse_json_field(raw, "ranges")?;
        ranges = parsed
            .into_iter()
            .map(|r| TextRangeReq {
                // `trimOrEmpty`: null → "".
                start: r.start_string.unwrap_or_default().trim().to_string(),
                end: r.end_string.unwrap_or_default().trim().to_string(),
            })
            .collect();
    }
    let mut image_boxes = Vec::new();
    if let Some(raw) = last_param(params, "imageBoxes") {
        let parsed: Vec<ImageBoxJson> = parse_json_field(raw, "imageBoxes")?;
        image_boxes = parsed
            .into_iter()
            .map(|b| ImageBoxReq {
                page_index: b.page_index.unwrap_or(0),
                x1: b.x1.unwrap_or(0.0),
                y1: b.y1.unwrap_or(0.0),
                x2: b.x2.unwrap_or(0.0),
                y2: b.y2.unwrap_or(0.0),
            })
            .collect();
    }
    let mut style = ExecuteStyle::default();
    if let Some(raw) = last_param(params, "style") {
        if !raw.trim().is_empty() {
            let parsed: StyleJson = serde_json::from_str(raw)
                .map_err(|e| AppError::BadRequest(format!("invalid style: {e}")))?;
            let strategy = parsed.strategy.unwrap_or_else(|| "AUTO".into());
            // Jackson binds the enum case-sensitively; unknown values are
            // a binding error there, so refuse them here too.
            if !matches!(
                strategy.as_str(),
                "AUTO" | "OVERLAY_ONLY" | "IMAGE_FINALIZE"
            ) {
                return Err(AppError::BadRequest(format!(
                    "invalid style.strategy: {strategy}"
                )));
            }
            style = ExecuteStyle {
                color: parsed.color.unwrap_or_else(|| "#000000".into()),
                padding: parsed.padding.unwrap_or(0.0),
                convert_to_image: parsed.convert_to_image.unwrap_or(false),
                image_finalize: strategy == "IMAGE_FINALIZE",
            };
        }
    }
    Ok(ExecuteRequest {
        text_values,
        regex_patterns,
        wipe_pages,
        ranges,
        image_boxes,
        redact_image_pages,
        style,
    })
}

fn parse_json_field<T: for<'de> Deserialize<'de>>(raw: &str, field: &str) -> AppResult<T> {
    serde_json::from_str(raw).map_err(|e| AppError::BadRequest(format!("invalid {field}: {e}")))
}

/// Spring's `String`→`List` binding: repeated fields each contribute, a
/// single comma-joined value splits on commas; a JSON array is accepted as a
/// lenient extension for JSON-speaking clients.
fn string_list_param(params: &[(String, String)], name: &str) -> AppResult<Vec<String>> {
    let values = all_params(params, name);
    if values.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('[') {
            if let Ok(items) = serde_json::from_str::<Vec<serde_json::Value>>(trimmed) {
                for item in items {
                    match item {
                        serde_json::Value::String(s) => out.push(s),
                        serde_json::Value::Null => {}
                        other => out.push(other.to_string()),
                    }
                }
                continue;
            }
        }
        out.extend(trimmed.split(',').map(str::to_string));
    }
    Ok(out)
}

/// `List<Integer>` binding: same shapes as [`string_list_param`], parsed to
/// integers; JSON `null` entries are preserved.
fn int_list_param(params: &[(String, String)], name: &str) -> AppResult<Vec<Option<i64>>> {
    let values = all_params(params, name);
    if values.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('[') {
            if let Ok(items) = serde_json::from_str::<Vec<Option<i64>>>(trimmed) {
                out.extend(items);
                continue;
            }
        }
        for token in trimmed.split(',') {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            let parsed = token
                .parse::<i64>()
                .map_err(|_| AppError::BadRequest(format!("invalid integer in {name}: {token}")))?;
            out.push(Some(parsed));
        }
    }
    Ok(out)
}

/// `POST /api/v1/security/redact-execute` — `RedactExecuteRequest`: the
/// unified redaction plan (text values, regex, anchored ranges, image boxes,
/// page wipes, detected-image pages).
///
/// jpdfium's verified content removal has no lopdf equivalent, so AUTO's
/// text-removal step always degrades to Java's overlay fallback — cover
/// rectangles in every strategy, narrower only when native removal
/// succeeded, which never happens here. Whole-page wipes still remove page
/// content natively (contents stream dropped). `style.convert_to_image` and
/// `IMAGE_FINALIZE` rasterise every output page through Ghostscript.
pub async fn redact_execute(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, params) = receive_multipart(multipart).await?;
    let request = parse_execute_request(&params)?;
    if regex_patterns_unsupported(&request.regex_patterns) {
        return Err(AppError::BadRequest(
            "regexPatterns use syntax the native matcher cannot compile".into(),
        ));
    }
    misc::dispatch(
        state,
        query,
        uploads,
        params,
        move |state, query, uploads, params| {
            redact_execute_work(state, query, uploads, params, request)
        },
    )
    .await
}

/// True when any `regexPatterns` entry fails the `regex` crate — valid Java
/// syntax (lookaround, backreferences, possessive quantifiers) the native
/// matcher cannot honour. Execute uses the patterns unwrapped, so a plain
/// compile is the exact check `find_text` will run.
fn regex_patterns_unsupported(patterns: &[String]) -> bool {
    patterns.iter().any(|p| Regex::new(p).is_err())
}

fn redact_execute_work(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    _params: Vec<(String, String)>,
    request: ExecuteRequest,
) -> AppResult<Response> {
    // `cleanStrings`: trim, drop empties.
    let text_values: Vec<String> = request
        .text_values
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let regex_patterns: Vec<String> = request
        .regex_patterns
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let wipe_pages: Vec<Option<i64>> = request.wipe_pages.clone();
    let ranges = request.ranges.clone();
    let image_boxes = request.image_boxes.clone();
    let redact_image_pages = request.redact_image_pages.clone();
    let style = request.style.clone();

    let has_text_ops = !text_values.is_empty() || !regex_patterns.is_empty();
    let text_only = has_text_ops
        && wipe_pages.is_empty()
        && ranges.is_empty()
        && image_boxes.is_empty()
        && redact_image_pages.is_none();

    let has_targets = has_text_ops
        || !wipe_pages.is_empty()
        || !ranges.is_empty()
        || !image_boxes.is_empty()
        || redact_image_pages.is_some();
    if !has_targets {
        return Err(AppError::BadRequest("No redaction targets provided".into()));
    }

    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    let mut document = pdfdoc::load(&upload.name, &upload.bytes)?;
    let page_ids = pdfdoc::page_ids(&document);
    let page_count = page_ids.len() as i64;
    require_pages_exist("wipePages", &wipe_pages, page_count)?;
    if let Some(pages) = &redact_image_pages {
        require_pages_exist("redactImagePages", pages, page_count)?;
    }

    // Lazily computed per need: text extraction feeds text matches, range
    // anchors and wipe element boxes; image detection feeds ranges,
    // redactImagePages and wipes.
    let needs_text = has_text_ops || !ranges.is_empty() || !wipe_pages.is_empty();
    let page_texts = if needs_text {
        pdftext::model::document_text(&document)
    } else {
        Vec::new()
    };
    let heights = crop_heights(&document, &page_ids)?;
    let widths: Vec<f32> = page_ids
        .iter()
        .map(|id| crop_dims(&document, *id).map(|(_, w)| w))
        .collect::<AppResult<Vec<_>>>()?;

    let mut image_pages: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    if !ranges.is_empty() || redact_image_pages.as_ref().is_some_and(|p| p.is_empty()) {
        image_pages.extend(0..page_ids.len());
    }
    image_pages.extend(wipe_pages.iter().flatten().map(|p| (*p - 1) as usize));
    if let Some(pages) = &redact_image_pages {
        image_pages.extend(pages.iter().flatten().map(|p| (*p - 1) as usize));
    }
    let mut images: Vec<Vec<[f64; 4]>> = vec![Vec::new(); page_ids.len()];
    for &page in &image_pages {
        if page < page_ids.len() {
            images[page] = pdftext::redact::image_boxes(&document, page_ids[page]);
        }
    }

    // Single text scan, terms then regex — `collectTextMatches`.
    let mut found: Vec<pdftext::redact::FoundText> = Vec::new();
    if !text_values.is_empty() {
        found.extend(
            pdftext::redact::find_text(&page_texts, &heights, &text_values, false, false, false)
                .map_err(|e| AppError::BadRequest(format!("invalid textValues pattern: {e}")))?,
        );
    }
    if !regex_patterns.is_empty() {
        found.extend(
            pdftext::redact::find_text(&page_texts, &heights, &regex_patterns, true, false, false)
                .map_err(|e| AppError::BadRequest(format!("invalid regexPatterns[{e}]")))?,
        );
    }
    let total_matches = found.len();

    let color = redact_color(Some(&style.color));
    let padding = style.padding;

    // Non-text operations, in Java's order: wipes, ranges, image boxes,
    // detected images; the text-match overlay is painted last by
    // `finalizeRedaction`.
    let mut wipe_indices: Vec<usize> = wipe_pages
        .iter()
        .filter_map(|p| p.map(|v| (v - 1) as usize))
        .collect();
    wipe_indices.sort_unstable();
    for index in wipe_indices {
        apply_page_wipe(
            &mut document,
            &page_ids,
            index,
            &page_texts,
            &heights,
            &images,
            color,
        )?;
    }

    for range in &ranges {
        let blocks = pdftext::redact::range_blocks(
            &page_texts,
            &heights,
            &widths,
            &images,
            &range.start,
            &range.end,
        );
        overlay_found_text(&mut document, &page_ids, &blocks, padding, color)?;
    }

    for bx in &image_boxes {
        apply_image_box(&mut document, &page_ids, bx, color)?;
    }

    if let Some(pages) = &redact_image_pages {
        let mut indices: Vec<usize> = pages
            .iter()
            .filter_map(|p| p.map(|v| (v - 1) as usize))
            .collect();
        if indices.is_empty() {
            indices = (0..page_ids.len()).collect();
        }
        indices.sort_unstable();
        indices.dedup();
        for index in indices {
            if index >= page_ids.len() {
                continue;
            }
            let boxes: Vec<[f64; 4]> = images[index].clone();
            apply_detected_images(&mut document, &page_ids, index, &boxes, color)?;
        }
    }

    overlay_found_text(&mut document, &page_ids, &found, padding, color)?;
    if !found.is_empty() {
        clean_redaction_metadata(&mut document)?;
    }
    pdfdoc::touch_loaded_info(&mut document);
    let bytes = pdfdoc::save(&mut document)?;
    let bytes = if style.rasterizes() {
        rasterize_pdf(&upload.name, &bytes)?
    } else {
        bytes
    };
    let mut response = pdf_response(bytes, &redacted_name(&upload.name));
    // Only a text-only plan can be judged by its match count.
    if text_only {
        if let Ok(value) = axum::http::HeaderValue::from_str(&total_matches.to_string()) {
            response.headers_mut().insert(MATCHES_HEADER, value);
        }
    }
    Ok(response)
}

/// `RedactExecuteService.requirePagesExist`: every entry must be a page
/// number in `1..=page_count`; nulls count as missing.
fn require_pages_exist(field: &str, pages: &[Option<i64>], page_count: i64) -> AppResult<()> {
    let missing: Vec<String> = pages
        .iter()
        .filter(|p| p.is_none_or(|v| v < 1 || v > page_count))
        .map(|p| p.map(|v| v.to_string()).unwrap_or_else(|| "null".into()))
        .collect();
    if !missing.is_empty() {
        return Err(AppError::BadRequest(format!(
            "{field} {missing:?}: pages are numbered 1 to {page_count}"
        )));
    }
    Ok(())
}

/// `applyPageWipe`: drop the page's content stream and resources, then draw
/// the detected element boxes (or the whole page when there are none) in the
/// style color. This is the one redaction step lopdf can do natively.
fn apply_page_wipe(
    document: &mut Document,
    page_ids: &[ObjectId],
    index: usize,
    page_texts: &[pdftext::model::PageText],
    heights: &[f32],
    images: &[Vec<[f64; 4]>],
    color: (f64, f64, f64),
) -> AppResult<()> {
    let page_id = page_ids[index];
    let (page_h, page_w) = crop_dims(document, page_id)?;
    let mut rects: Vec<(f64, f64, f64, f64)> = Vec::new();
    if let Some(text) = page_texts.get(index) {
        for b in pdftext::redact::line_pdf_boxes(text, heights[index]) {
            rects.push((
                f64::from(b[0]),
                f64::from(b[1]),
                f64::from(b[2] - b[0]),
                f64::from(b[3] - b[1]),
            ));
        }
    }
    if let Some(page_images) = images.get(index) {
        for ib in page_images {
            rects.push((ib[0], ib[1], ib[2] - ib[0], ib[3] - ib[1]));
        }
    }

    {
        let page = document
            .get_dictionary_mut(page_id)
            .map_err(|e| AppError::Pdf(e.to_string()))?;
        page.remove(b"Contents");
        page.set("Resources", Object::Dictionary(lopdf::Dictionary::new()));
    }

    let (r, g, b) = color;
    let mut ops = format!("q {r} {g} {b} rg\n");
    if rects.is_empty() {
        ops.push_str(&format!("0 0 {page_w} {page_h} re\n"));
    } else {
        for (x, y, w, h) in rects {
            ops.push_str(&format!("{x} {y} {w} {h} re\n"));
        }
    }
    ops.push_str("f Q\n");
    misc::append_page_content(document, page_id, &ops)
}

/// `applyImageBoxRedaction`: user-supplied boxes are drawn raw, no
/// coordinate flip — Java passes them straight to `addRect`.
fn apply_image_box(
    document: &mut Document,
    page_ids: &[ObjectId],
    bx: &ImageBoxReq,
    color: (f64, f64, f64),
) -> AppResult<()> {
    let index = bx.page_index;
    if index < 0 || index as usize >= page_ids.len() {
        return Ok(());
    }
    let (r, g, b) = color;
    misc::append_page_content(
        document,
        page_ids[index as usize],
        &format!(
            "q {r} {g} {b} rg {} {} {} {} re f Q\n",
            bx.x1,
            bx.y1,
            bx.x2 - bx.x1,
            bx.y2 - bx.y1
        ),
    )
}

/// `applyAllImagesRedaction` for one page: detected image boxes drawn raw.
fn apply_detected_images(
    document: &mut Document,
    page_ids: &[ObjectId],
    index: usize,
    boxes: &[[f64; 4]],
    color: (f64, f64, f64),
) -> AppResult<()> {
    if boxes.is_empty() {
        return Ok(());
    }
    let (r, g, b) = color;
    let mut ops = format!("q {r} {g} {b} rg\n");
    for b in boxes {
        ops.push_str(&format!(
            "{} {} {} {} re\n",
            b[0],
            b[1],
            b[2] - b[0],
            b[3] - b[1]
        ));
    }
    ops.push_str("f Q\n");
    misc::append_page_content(document, page_ids[index], &ops)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit;
    use lopdf::dictionary;

    fn js_action() -> Dictionary {
        dictionary! {
            "S" => "JavaScript",
            "JS" => Object::string_literal("app.alert(1)"),
        }
    }

    fn goto_action() -> Dictionary {
        dictionary! { "S" => "GoTo" }
    }

    /// Every sanitizable feature, with `Names`/`AcroForm`/annotations as
    /// indirect objects like PDFBox writes them (the reference-only paths
    /// in `remove_embedded_files`/`remove_javascript` depend on it).
    fn fixture() -> testkit::Pdf {
        let mut pdf = testkit::Pdf::new();

        let names = pdf.add(dictionary! {
            "JavaScript" => dictionary! {
                "Names" => Object::Array(vec![
                    Object::string_literal("js1"),
                    Object::Dictionary(js_action()),
                ]),
            },
            "EmbeddedFiles" => dictionary! {
                "Names" => Object::Array(vec![
                    Object::string_literal("f1"),
                    Object::Dictionary(dictionary! {
                        "Type" => "Filespec",
                        "F" => Object::string_literal("a.txt"),
                    }),
                ]),
            },
        });
        let acro = pdf.add(dictionary! {
            "Fields" => Vec::<Object>::new(),
        });
        pdf.edit_catalog(|catalog| {
            catalog.set("Names", Object::Reference(names));
            catalog.set("OpenAction", Object::Dictionary(js_action()));
            catalog.set(
                "AA",
                dictionary! {
                    "WC" => Object::Dictionary(js_action()),
                    "DP" => Object::Dictionary(goto_action()),
                },
            );
            catalog.set("AcroForm", Object::Reference(acro));
        });

        let kid = pdf.add(dictionary! {
            "T" => Object::string_literal("sub"),
            "AA" => dictionary! {
                "F" => Object::Dictionary(js_action()),
                "K" => Object::Dictionary(goto_action()),
            },
        });
        let field = pdf.add(dictionary! {
            "FT" => "Tx",
            "T" => Object::string_literal("f1"),
            "AA" => dictionary! {
                "C" => Object::Dictionary(js_action()),
                "V" => Object::Dictionary(js_action()),
                "K" => Object::Dictionary(goto_action()),
            },
            "Kids" => vec![Object::Reference(kid)],
        });
        // Point the AcroForm's Fields array at the field.
        let mut acro_dict = pdf.dict(acro);
        acro_dict.set("Fields", vec![Object::Reference(field)]);
        pdf.put_dict(acro, acro_dict);

        let xmp = pdf.add_stream(
            dictionary! { "Type" => "Metadata", "Subtype" => "XML" },
            b"<x:xmpmeta/>",
        );
        pdf.edit_catalog(|catalog| {
            catalog.set("Metadata", Object::Reference(xmp));
        });
        pdf.set_info(dictionary! {
            "Title" => Object::string_literal("doc"),
            "Producer" => Object::string_literal("test"),
        });

        let link = pdf.add(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Link",
            "Rect" => Object::Array(vec![0.into(), 0.into(), 10.into(), 10.into()]),
            "A" => dictionary! {
                "S" => "URI",
                "URI" => Object::string_literal("https://example.com"),
            },
        });
        let attach = pdf.add(dictionary! {
            "Type" => "Annot",
            "Subtype" => "FileAttachment",
            "Rect" => Object::Array(vec![0.into(), 0.into(), 10.into(), 10.into()]),
            "FS" => dictionary! {
                "Type" => "Filespec",
                "F" => Object::string_literal("a.txt"),
            },
        });
        let widget = pdf.add(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Widget",
            "Rect" => Object::Array(vec![0.into(), 0.into(), 10.into(), 10.into()]),
            "A" => Object::Dictionary(js_action()),
        });
        let text = pdf.add(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Text",
            "Rect" => Object::Array(vec![0.into(), 0.into(), 10.into(), 10.into()]),
            "Contents" => Object::string_literal("note"),
        });
        pdf.edit_page(|page| {
            page.set(
                "Resources",
                dictionary! {
                    "Font" => dictionary! {
                        "F1" => dictionary! {
                            "Type" => "Font",
                            "Subtype" => "Type1",
                            "BaseFont" => "Helvetica",
                        },
                    },
                },
            );
            page.set(
                "AA",
                dictionary! {
                    "O" => Object::Dictionary(js_action()),
                    "C" => Object::Dictionary(goto_action()),
                },
            );
            page.set(
                "Annots",
                vec![
                    Object::Reference(link),
                    Object::Reference(attach),
                    Object::Reference(widget),
                    Object::Reference(text),
                ],
            );
        });
        pdf
    }

    fn params(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn all_off() -> Vec<(String, String)> {
        params(&[
            ("removeJavaScript", "false"),
            ("removeEmbeddedFiles", "false"),
            ("removeXMPMetadata", "false"),
            ("removeMetadata", "false"),
            ("removeLinks", "false"),
            ("removeFonts", "false"),
        ])
    }

    fn upload(bytes: Vec<u8>) -> Vec<Upload> {
        vec![Upload {
            name: "in.pdf".into(),
            bytes: crate::upload::UploadBytes::from_vec(bytes),
            content_type: None,
        }]
    }

    async fn run(bytes: Vec<u8>, params: Vec<(String, String)>) -> Document {
        let response =
            sanitize_pdf_work(AppState::new(), HashMap::new(), upload(bytes), params).unwrap();
        testkit::response_document(response).await
    }

    fn annot_subtypes(doc: &Document, page: &Dictionary) -> Vec<Vec<u8>> {
        let Some(annots) = page.get(b"Annots").ok() else {
            return Vec::new();
        };
        let (_, resolved) = doc.dereference(annots).unwrap();
        let Object::Array(items) = resolved else {
            return Vec::new();
        };
        items
            .iter()
            .filter_map(|a| {
                pdfdoc::peek_dict(doc, a)
                    .and_then(|d| d.get(b"Subtype").ok())
                    .and_then(|s| s.as_name().ok())
                    .map(<[u8]>::to_vec)
            })
            .collect()
    }

    /// Annotation order is fixture order minus removals: link, attach,
    /// widget, text → link, widget, text once the attachment is dropped.
    #[tokio::test]
    async fn defaults_strip_javascript_and_embedded_files_only() {
        let mut pdf = fixture();
        let doc = run(pdf.bytes(), vec![]).await;

        let catalog = doc.catalog().unwrap();
        let names = pdfdoc::peek_dict(&doc, catalog.get(b"Names").unwrap()).unwrap();
        assert!(names.get(b"JavaScript").is_err());
        assert!(names.get(b"EmbeddedFiles").is_err());
        assert!(catalog.get(b"OpenAction").is_err());
        let aa = pdfdoc::peek_dict(&doc, catalog.get(b"AA").unwrap()).unwrap();
        assert!(aa.get(b"WC").is_err());
        assert!(aa.get(b"DP").is_ok());

        let acro = pdfdoc::peek_dict(&doc, catalog.get(b"AcroForm").unwrap()).unwrap();
        let fields = acro.get(b"Fields").unwrap();
        let (_, fields) = doc.dereference(fields).unwrap();
        let Object::Array(fields) = fields else {
            panic!("fields")
        };
        let field = pdfdoc::peek_dict(&doc, &fields[0]).unwrap();
        let faa = pdfdoc::peek_dict(&doc, field.get(b"AA").unwrap()).unwrap();
        assert!(faa.get(b"C").is_err());
        assert!(faa.get(b"V").is_err());
        assert!(faa.get(b"K").is_ok());
        let kids = field.get(b"Kids").unwrap();
        let (_, kids) = doc.dereference(kids).unwrap();
        let Object::Array(kids) = kids else {
            panic!("kids")
        };
        let kid = pdfdoc::peek_dict(&doc, &kids[0]).unwrap();
        let kaa = pdfdoc::peek_dict(&doc, kid.get(b"AA").unwrap()).unwrap();
        assert!(kaa.get(b"F").is_err());
        assert!(kaa.get(b"K").is_ok());

        let pages = doc.get_pages();
        let page = doc.get_dictionary(*pages.values().next().unwrap()).unwrap();
        let paa = pdfdoc::peek_dict(&doc, page.get(b"AA").unwrap()).unwrap();
        assert!(paa.get(b"O").is_err());
        assert!(paa.get(b"C").is_ok());
        assert_eq!(
            annot_subtypes(&doc, page),
            vec![b"Link".to_vec(), b"Widget".to_vec(), b"Text".to_vec()]
        );
        let annots = page.get(b"Annots").unwrap();
        let (_, annots) = doc.dereference(annots).unwrap();
        let Object::Array(annots) = annots else {
            panic!("annots")
        };
        let link = pdfdoc::peek_dict(&doc, &annots[0]).unwrap();
        assert!(link.get(b"A").is_ok()); // removeLinks off by default
        let widget = pdfdoc::peek_dict(&doc, &annots[1]).unwrap();
        assert!(widget.get(b"A").is_err());

        // Off-by-default targets survive.
        assert!(catalog.get(b"Metadata").is_ok());
        let info = doc.trailer.get(b"Info").unwrap();
        assert!(pdfdoc::peek_dict(&doc, info).is_some());
        let resources = pdfdoc::peek_dict(&doc, page.get(b"Resources").unwrap()).unwrap();
        assert!(resources.get(b"Font").is_ok());
    }

    #[tokio::test]
    async fn all_flags_strip_everything() {
        let mut pdf = fixture();
        let mut p = all_off();
        for pair in p.iter_mut() {
            pair.1 = "true".into();
        }
        let doc = run(pdf.bytes(), p).await;

        let catalog = doc.catalog().unwrap();
        assert!(catalog.get(b"Metadata").is_err());
        assert!(catalog.get(b"OpenAction").is_err());
        let names = pdfdoc::peek_dict(&doc, catalog.get(b"Names").unwrap()).unwrap();
        assert!(names.is_empty());
        let info = doc.trailer.get(b"Info").unwrap();
        let (_, info) = doc.dereference(info).unwrap();
        assert!(pdfdoc::dict_of(info).unwrap().is_empty());

        let pages = doc.get_pages();
        let page = doc.get_dictionary(*pages.values().next().unwrap()).unwrap();
        let resources = pdfdoc::peek_dict(&doc, page.get(b"Resources").unwrap()).unwrap();
        assert!(resources.get(b"Font").is_err());
        assert_eq!(
            annot_subtypes(&doc, page),
            vec![b"Link".to_vec(), b"Widget".to_vec(), b"Text".to_vec()]
        );
        let annots = page.get(b"Annots").unwrap();
        let (_, annots) = doc.dereference(annots).unwrap();
        let Object::Array(annots) = annots else {
            panic!("annots")
        };
        let link = pdfdoc::peek_dict(&doc, &annots[0]).unwrap();
        assert!(link.get(b"A").is_err());
        let widget = pdfdoc::peek_dict(&doc, &annots[1]).unwrap();
        assert!(widget.get(b"A").is_err());
    }

    /// Each opt-in flag alone strips only its own target.
    #[tokio::test]
    async fn individual_flags_strip_only_their_target() {
        type Check = fn(&Document);
        let cases: &[(&str, Check)] = &[
            ("removeXMPMetadata", |doc| {
                assert!(doc.catalog().unwrap().get(b"Metadata").is_err());
                assert!(doc.catalog().unwrap().get(b"OpenAction").is_ok());
            }),
            ("removeMetadata", |doc| {
                let info = doc.trailer.get(b"Info").unwrap();
                let (_, info) = doc.dereference(info).unwrap();
                assert!(pdfdoc::dict_of(info).unwrap().is_empty());
            }),
            ("removeFonts", |doc| {
                let pages = doc.get_pages();
                let page = doc.get_dictionary(*pages.values().next().unwrap()).unwrap();
                let resources = pdfdoc::peek_dict(doc, page.get(b"Resources").unwrap()).unwrap();
                assert!(resources.get(b"Font").is_err());
            }),
            ("removeLinks", |doc| {
                let pages = doc.get_pages();
                let page = doc.get_dictionary(*pages.values().next().unwrap()).unwrap();
                let annots = page.get(b"Annots").unwrap();
                let (_, annots) = doc.dereference(annots).unwrap();
                let Object::Array(annots) = annots else {
                    panic!("annots")
                };
                // Fixture order is link, attach, widget, text — nothing is
                // dropped with only removeLinks on.
                let link = pdfdoc::peek_dict(doc, &annots[0]).unwrap();
                assert!(link.get(b"A").is_err());
                // The widget's JS action survives: removeJavaScript is off.
                let widget = pdfdoc::peek_dict(doc, &annots[2]).unwrap();
                assert!(widget.get(b"A").is_ok());
            }),
        ];
        for (flag, check) in cases {
            let mut pdf = fixture();
            let mut p = all_off();
            for pair in p.iter_mut() {
                if pair.0 == *flag {
                    pair.1 = "true".into();
                }
            }
            let doc = run(pdf.bytes(), p).await;
            check(&doc);
        }
    }

    #[tokio::test]
    async fn document_without_annotations_or_forms_survives() {
        let mut pdf = testkit::Pdf::new();
        let doc = run(pdf.bytes(), vec![]).await;
        assert_eq!(doc.get_pages().len(), 1);
        // Output re-opens cleanly — pdfdoc::save/load round-trip.
        let _ = pdfdoc::save(&mut doc.clone()).unwrap();
    }

    #[tokio::test]
    async fn malformed_pdf_is_rejected() {
        let result = sanitize_pdf_work(
            AppState::new(),
            HashMap::new(),
            upload(b"not a pdf".to_vec()),
            vec![],
        );
        assert!(result.is_err());
    }

    // ------------------------------------------------------------------
    // get-info-on-pdf
    // ------------------------------------------------------------------

    async fn info(bytes: Vec<u8>) -> Value {
        get_info_on_pdf_work(AppState::new(), HashMap::new(), upload(bytes), vec![])
            .unwrap()
            .0
    }

    /// Every top-level section is present on a minimal document, with the
    /// unencrypted/permission-free defaults Java reports.
    #[tokio::test]
    async fn info_sections_present_on_minimal_pdf() {
        let mut pdf = testkit::Pdf::new();
        let report = info(pdf.bytes()).await;
        for key in [
            "Metadata",
            "BasicInfo",
            "DocumentInfo",
            "Compliancy",
            "Encryption",
            "Permissions",
            "FormFields",
            "Other",
            "PerPageInfo",
        ] {
            assert!(report.get(key).is_some(), "missing section {key}");
        }
        assert_eq!(report["Encryption"]["IsEncrypted"], false);
        assert_eq!(report["Permissions"]["Printing"], "Allowed");
        assert_eq!(report["Compliancy"]["IsPDF/SECCompliant"], true);
        assert_eq!(report["BasicInfo"]["Number of pages"], 1);
        assert!(report.get("SummaryData").is_none());

        let page = &report["PerPageInfo"]["Page 1"];
        assert_eq!(page["Size"]["Standard Page"], "Letter");
        assert_eq!(page["Page Orientation"], "Portrait");
        assert_eq!(page["Rotation"], 0);
        assert_eq!(page["MediaBox"], "[0.0,0.0,612.0,792.0]");
    }

    /// Info-dictionary fields, PDF dates, custom keys, Trapped and PageMode.
    #[tokio::test]
    async fn info_metadata_documentinfo_and_dates() {
        let mut pdf = testkit::Pdf::new();
        pdf.set_info(dictionary! {
            "Title" => Object::string_literal("My Doc"),
            "Author" => Object::string_literal("Ann"),
            "CreationDate" => Object::string_literal("D:20200101120000Z"),
            "Trapped" => "True",
            "CustomKey" => Object::string_literal("custom-value"),
        });
        pdf.edit_catalog(|catalog| {
            catalog.set("PageMode", "UseOutlines");
        });
        let report = info(pdf.bytes()).await;

        let metadata = &report["Metadata"];
        assert_eq!(metadata["Title"], "My Doc");
        assert_eq!(metadata["Author"], "Ann");
        assert_eq!(metadata["CustomKey"], "custom-value");
        // Standard keys never leak into the custom section.
        assert!(metadata.get("Trapped").is_none());
        assert!(metadata.get("CreationDate").is_some());
        let creation = metadata["CreationDate"].as_str().unwrap();
        assert_eq!(creation.len(), 19);
        assert_eq!(&creation[4..5], "-");

        let doc_info = &report["DocumentInfo"];
        assert_eq!(doc_info["Trapped"], "True");
        assert_eq!(doc_info["Page Mode"], "USE_OUTLINES");
        assert_eq!(doc_info["PDF version"], "1.7");
    }

    /// Page resources surface images, fonts, links, XObject counts and
    /// document-level attachments/bookmarks in the report.
    #[tokio::test]
    async fn info_page_resources_and_other_sections() {
        let mut pdf = testkit::Pdf::new();
        let image = pdf.add_stream(testkit::image_xobject(), &[255, 0, 0]);
        let font = pdf.add(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let link = pdf.add(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Link",
            "Rect" => Object::Array(vec![0.into(), 0.into(), 10.into(), 10.into()]),
            "A" => dictionary! {
                "S" => "URI",
                "URI" => Object::string_literal("https://example.com"),
            },
        });
        let attach = pdf.add(dictionary! {
            "Type" => "Annot",
            "Subtype" => "FileAttachment",
            "Rect" => Object::Array(vec![0.into(), 0.into(), 10.into(), 10.into()]),
            "Name" => "PushPin",
            "Contents" => Object::string_literal("note"),
        });
        pdf.edit_page(|page| {
            page.set(
                "Resources",
                dictionary! {
                    "XObject" => dictionary! { "Im1" => Object::Reference(image) },
                    "Font" => dictionary! { "F1" => Object::Reference(font) },
                },
            );
            page.set(
                "Annots",
                vec![Object::Reference(link), Object::Reference(attach)],
            );
        });
        let report = info(pdf.bytes()).await;

        assert_eq!(report["BasicInfo"]["TotalImages"], 1);
        assert_eq!(report["BasicInfo"]["UniqueImages"], 1);

        let page = &report["PerPageInfo"]["Page 1"];
        assert_eq!(page["Images"].as_array().unwrap().len(), 1);
        assert_eq!(page["Images"][0]["Width"], 1);
        assert_eq!(page["Images"][0]["ColorSpace"], "DeviceRGB");
        assert_eq!(page["XObjectCounts"]["Image"], 1);
        assert_eq!(page["Links"].as_array().unwrap().len(), 1);
        assert_eq!(page["Links"][0]["URI"], "https://example.com");
        assert_eq!(page["Fonts"].as_array().unwrap().len(), 1);
        assert_eq!(page["Fonts"][0]["Name"], "Helvetica");
        assert_eq!(page["Fonts"][0]["Count"], 1);
        assert_eq!(page["Annotations"]["AnnotationsCount"], 2);

        assert_eq!(report["Other"]["Attachments"].as_array().unwrap().len(), 1);
        assert_eq!(report["Other"]["Attachments"][0]["Description"], "note");
        assert_eq!(report["Other"]["IsPortfolio"], false);
    }

    /// JavaScript/AcroForm/link actions flip SEC compliance off, matching
    /// `isSECCompliant`.
    #[tokio::test]
    async fn info_sec_compliance_detects_active_content() {
        let mut pdf = testkit::Pdf::new();
        pdf.edit_catalog(|catalog| {
            catalog.set(
                "OpenAction",
                Object::Dictionary(dictionary! {
                    "S" => "JavaScript",
                    "JS" => Object::string_literal("app.alert(1)"),
                }),
            );
        });
        let report = info(pdf.bytes()).await;
        assert_eq!(report["Compliancy"]["IsPDF/SECCompliant"], false);
        // Still unencrypted: permissions stay fully allowed.
        assert_eq!(report["Permissions"]["Printing"], "Allowed");
    }

    /// JavaScript in the name tree is reported under Other.JavaScript with
    /// the script length in UTF-16 units.
    #[tokio::test]
    async fn info_javascript_name_tree() {
        let mut pdf = testkit::Pdf::new();
        let names = pdf.add(dictionary! {
            "JavaScript" => dictionary! {
                "Names" => Object::Array(vec![
                    Object::string_literal("js1"),
                    Object::Dictionary(js_action()),
                ]),
            },
        });
        pdf.edit_catalog(|catalog| {
            catalog.set("Names", Object::Reference(names));
        });
        let report = info(pdf.bytes()).await;
        let js = report["Other"]["JavaScript"].as_array().unwrap();
        assert_eq!(js.len(), 1);
        assert_eq!(js[0]["JS Name"], "js1");
        assert_eq!(
            js[0]["JS Script Length"],
            "app.alert(1)".encode_utf16().count() as u64
        );
        // A document JavaScript name tree is active content → not SEC clean.
        assert_eq!(report["Compliancy"]["IsPDF/SECCompliant"], false);
    }

    /// Unit coverage for the formatting/parsing helpers.
    #[test]
    fn helper_formats_match_java() {
        assert_eq!(java_split_len("", whitespace_sep), 1);
        assert_eq!(java_split_len("   ", whitespace_sep), 0);
        assert_eq!(java_split_len(" a b ", whitespace_sep), 3);
        assert_eq!(java_split_len("a\nb", newline_sep), 2);
        assert_eq!(java_split_len("a\r\nb", newline_sep), 2);
        assert_eq!(java_split_len("a\n\rb", newline_sep), 3);

        assert_eq!(page_orientation(100.0, 200.0), "Portrait");
        assert_eq!(page_orientation(200.0, 100.0), "Landscape");
        assert_eq!(page_orientation(100.0, 100.0), "Square");
        assert_eq!(standard_page_size(612.0, 792.0), "Letter");
        assert_eq!(standard_page_size(100.0, 100.0), "Custom");
        assert_eq!(java_float(612.0), "612.0");
        assert_eq!(
            rect_string([0.0, 0.0, 612.0, 792.0]),
            "[0.0,0.0,612.0,792.0]"
        );

        assert_eq!(pdfdoc::pdf_date_local("not a date"), None);
        assert!(pdfdoc::pdf_date_local("D:20200101120000Z").is_some());
        assert!(pdfdoc::pdf_date_local("D:20201301120000Z").is_none());
    }

    /// One-page PDF with a Helvetica text run, like `text.rs` tests.
    fn pdf_with_text(ops: &str) -> Vec<u8> {
        let mut pdf = testkit::Pdf::new();
        let font = pdf.add(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let content = pdf.add_stream(dictionary! {}, ops.as_bytes());
        pdf.edit_page(|page| {
            page.set("Contents", Object::Reference(content));
            page.set(
                "Resources",
                dictionary! {
                    "Font" => dictionary! {
                        "F1" => Object::Reference(font),
                    },
                },
            );
        });
        pdf.bytes()
    }

    fn matches_header(response: &Response) -> Option<String> {
        response
            .headers()
            .get(MATCHES_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    }

    #[tokio::test]
    async fn auto_redact_overlays_matches_and_reports_count() {
        let bytes = pdf_with_text("BT /F1 12 Tf 72 720 Td (hello world) Tj ET");
        let response = auto_redact_work(
            AppState::new(),
            HashMap::new(),
            upload(bytes),
            params(&[
                ("listOfText", "hello"),
                ("useRegex", "false"),
                ("wholeWordSearch", "false"),
                ("caseSensitive", "false"),
            ]),
        )
        .unwrap();
        assert_eq!(matches_header(&response).as_deref(), Some("1"));
        let doc = testkit::response_document(response).await;
        // The appended content stream carries the cover rectangle.
        let page = *doc.get_pages().values().next().unwrap();
        let content = doc.get_and_decode_page_content(page).unwrap();
        let ops: String = content
            .operations
            .iter()
            .map(|o| o.operator.clone())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(ops.contains(" re "), "expected a rect op, got {ops}");
    }

    #[tokio::test]
    async fn auto_redact_no_match_still_returns_pdf() {
        let bytes = pdf_with_text("BT /F1 12 Tf 72 720 Td (hello world) Tj ET");
        let response = auto_redact_work(
            AppState::new(),
            HashMap::new(),
            upload(bytes),
            params(&[("listOfText", "absent")]),
        )
        .unwrap();
        assert_eq!(matches_header(&response).as_deref(), Some("0"));
    }

    #[tokio::test]
    async fn auto_redact_rejects_missing_patterns() {
        let err = auto_redact_work(
            AppState::new(),
            HashMap::new(),
            upload(pdf_with_text("BT /F1 12 Tf 72 720 Td (hi) Tj ET")),
            params(&[("listOfText", "  \n ")]),
        )
        .unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[tokio::test]
    async fn redact_execute_text_values_sets_header() {
        let bytes = pdf_with_text("BT /F1 12 Tf 72 720 Td (hello world) Tj ET");
        let response = redact_execute_work(
            AppState::new(),
            HashMap::new(),
            upload(bytes),
            params(&[]),
            ExecuteRequest {
                text_values: vec!["hello".into()],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(matches_header(&response).as_deref(), Some("1"));
    }

    #[tokio::test]
    async fn redact_execute_wipe_removes_page_content() {
        let bytes = pdf_with_text("BT /F1 12 Tf 72 720 Td (hello world) Tj ET");
        let response = redact_execute_work(
            AppState::new(),
            HashMap::new(),
            upload(bytes),
            params(&[]),
            ExecuteRequest {
                wipe_pages: vec![Some(1)],
                ..Default::default()
            },
        )
        .unwrap();
        // Wipes are not text-only, so no match-count header.
        assert_eq!(matches_header(&response), None);
        let doc = testkit::response_document(response).await;
        let page_id = *doc.get_pages().values().next().unwrap();
        // The wipe stream replaces the original content: no text ops remain.
        let content = doc.get_and_decode_page_content(page_id).unwrap();
        let ops: String = content
            .operations
            .iter()
            .map(|o| o.operator.clone())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(!ops.contains("Tj"), "wipe left text ops: {ops}");
    }

    #[tokio::test]
    async fn redact_execute_rejects_empty_plan() {
        let err = redact_execute_work(
            AppState::new(),
            HashMap::new(),
            upload(pdf_with_text("BT /F1 12 Tf 72 720 Td (hi) Tj ET")),
            params(&[]),
            ExecuteRequest::default(),
        )
        .unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
    }
}
