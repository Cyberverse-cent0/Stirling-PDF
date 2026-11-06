//! Embedded-file (attachment) name-tree access for the `misc/*attachment*`
//! endpoints, porting `AttachmentService`: recursive `/Names` + `/Kids`
//! collection, `/UF`→`/F`→key filename fallback, embedded-file stream
//! resolution (`/EF` `/UF` `/F` `/DOS` `/Mac` `/Unix`), and the rename/delete
//! flatten-and-rewrite of the tree.

use std::collections::HashSet;

use chrono::{Datelike, Timelike};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};

use crate::error::{AppError, AppResult};
use crate::pdfdoc;
use crate::upload::simple_name;

/// `DEFAULT_MAX_ATTACHMENT_SIZE_BYTES` in AttachmentService.
const MAX_ATTACHMENT_SIZE: usize = 50 * 1024 * 1024;

/// One `/Names` leaf pair: tree key plus the file-specification object.
#[derive(Clone)]
pub struct AttachmentEntry {
    /// Decoded name-tree key.
    pub key: String,
    /// The `/Filespec` dictionary (resolved, owned).
    pub spec: Dictionary,
    /// Indirect id of the `/Filespec` when it has one, so a rewritten name
    /// tree can keep referencing it instead of inlining the dictionary.
    pub spec_id: Option<ObjectId>,
}

fn pdf_err(e: lopdf::Error) -> AppError {
    AppError::Pdf(e.to_string())
}

fn text_of(object: &Object) -> Option<String> {
    lopdf::decode_text_string(object).ok()
}

fn dict_value<'a>(dict: &'a Dictionary, key: &[u8]) -> Option<&'a Object> {
    dict.get(key).ok()
}

/// `/Names` → `/EmbeddedFiles` name-tree root, following one reference.
fn embedded_files_root(doc: &Document) -> Option<Object> {
    let catalog = doc.catalog().ok()?;
    let names = dict_value(catalog, b"Names")?;
    let names_dict = pdfdoc::peek_dict(doc, names)?;
    dict_value(names_dict, b"EmbeddedFiles").cloned()
}

/// Collects every `(key, filespec)` pair in the embedded-files name tree,
/// depth-first like PDFBox's `PDEmbeddedFilesNameTreeNode`. Nodes are visited
/// once by object id so cyclic `/Kids` graphs terminate.
pub fn collect_attachments(doc: &Document) -> AppResult<Vec<AttachmentEntry>> {
    let Some(root) = embedded_files_root(doc) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    let mut visited = HashSet::new();
    collect_node(doc, &root, &mut out, &mut visited, 0)?;
    // PDFBox collects into a LinkedHashMap: a repeated key keeps its first
    // position but takes the later value.
    let mut deduped: Vec<AttachmentEntry> = Vec::new();
    for entry in out {
        match deduped.iter_mut().find(|e| e.key == entry.key) {
            Some(existing) => *existing = entry,
            None => deduped.push(entry),
        }
    }
    Ok(deduped)
}

fn collect_node(
    doc: &Document,
    node: &Object,
    out: &mut Vec<AttachmentEntry>,
    visited: &mut HashSet<String>,
    depth: usize,
) -> AppResult<()> {
    if depth > 100 {
        return Ok(());
    }
    let Some(dict) = pdfdoc::peek_dict(doc, node) else {
        return Ok(());
    };
    // Identity for cycle detection: indirect id when present, else the
    // dictionary's own pointer is approximated by its debug form.
    let marker = match node {
        Object::Reference(id) => format!("ref:{id:?}"),
        _ => format!("dir:{:?}", dict),
    };
    if !visited.insert(marker) {
        return Ok(());
    }

    if let Some(names) = dict_value(dict, b"Names") {
        let (_, resolved) = doc.dereference(names).map_err(pdf_err)?;
        if let Object::Array(items) = resolved {
            for pair in items.as_chunks::<2>().0 {
                let key = match &pair[0] {
                    Object::String(_, _) => text_of(&pair[0]).unwrap_or_default(),
                    Object::Name(n) => String::from_utf8_lossy(n).into_owned(),
                    _ => continue,
                };
                let (spec_id, spec_obj) = doc.dereference(&pair[1]).map_err(pdf_err)?;
                if let Some(spec) = pdfdoc::dict_of(spec_obj) {
                    out.push(AttachmentEntry {
                        key,
                        spec: spec.clone(),
                        spec_id,
                    });
                }
            }
        }
    }

    if let Some(kids) = dict_value(dict, b"Kids") {
        let (_, resolved) = doc.dereference(kids).map_err(pdf_err)?;
        if let Object::Array(items) = resolved {
            for kid in items {
                collect_node(doc, kid, out, visited, depth + 1)?;
            }
        }
    }
    Ok(())
}

/// The embedded-file stream a file specification points at, in PDFBox's
/// priority order: `/UF`, `/F`, `/DOS`, `/Mac`, `/Unix`.
fn embedded_file_stream<'a>(doc: &'a Document, spec: &Dictionary) -> Option<&'a Stream> {
    let ef = pdfdoc::peek_dict(doc, spec.get(b"EF").ok()?)?;
    for key in [b"UF".as_slice(), b"F", b"DOS", b"Mac", b"Unix"] {
        if let Ok(entry) = ef.get(key) {
            if let Ok(Object::Stream(stream)) = doc.get_object(entry.as_reference().ok()?) {
                return Some(stream);
            }
        }
    }
    None
}

/// Display filename: `/UF`, then `/F`, then the tree key — first non-blank.
pub fn determine_filename(entry: &AttachmentEntry) -> String {
    for key in [b"UF".as_slice(), b"F"] {
        if let Some(name) = entry.spec.get(key).ok().and_then(text_of) {
            if !name.trim().is_empty() {
                return name;
            }
        }
    }
    if entry.key.trim().is_empty() {
        format!("unknown_attachment_{}", unix_millis())
    } else {
        entry.key.clone()
    }
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Decompressed embedded-file bytes, capped at 50 MB like Java's
/// `readAttachmentData`.
pub fn attachment_bytes(doc: &Document, entry: &AttachmentEntry) -> Option<Vec<u8>> {
    let stream = embedded_file_stream(doc, &entry.spec)?;
    let bytes = stream.decompressed_content().ok()?;
    (bytes.len() <= MAX_ATTACHMENT_SIZE).then_some(bytes)
}

fn pdf_date(object: Option<&Object>) -> Option<String> {
    let raw = text_of(object?)?;
    let digits = |range: std::ops::Range<usize>| raw.get(range)?.parse::<i64>().ok();
    // "D:YYYYMMDDHHmmSS…" — trailing timezone is ignored like PDFBox's
    // calendar keeps wall-clock fields in the default zone.
    let year = digits(2..6)?;
    let month = digits(6..8)?;
    let day = digits(8..10)?;
    let hour = digits(10..12).unwrap_or(0);
    let minute = digits(12..14).unwrap_or(0);
    let second = digits(14..16).unwrap_or(0);
    let naive = chrono::NaiveDate::from_ymd_opt(year as i32, month as u32, day as u32)?
        .and_hms_opt(hour as u32, minute as u32, second as u32)?;
    // Java's Date.toString() prints the zone abbreviation ("EAT"), which only
    // a named IANA zone provides — a fixed offset would print "+03:00".
    let tz: chrono_tz::Tz = iana_time_zone::get_timezone().ok()?.parse().ok()?;
    let local = naive.and_local_timezone(tz).single()?;
    Some(local.format("%a %b %d %H:%M:%S %Z %Y").to_string())
}

/// `AttachmentInfo` JSON shape from `listAttachments` — Jackson emits
/// camelCase keys and explicit nulls.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentInfo {
    pub filename: String,
    pub size: i64,
    pub content_type: Option<String>,
    pub description: Option<String>,
    pub creation_date: Option<String>,
    pub modification_date: Option<String>,
}

pub fn list_attachments(doc: &Document) -> AppResult<Vec<AttachmentInfo>> {
    let mut out = Vec::new();
    for entry in collect_attachments(doc)? {
        let Some(stream) = embedded_file_stream(doc, &entry.spec) else {
            continue;
        };
        let Some(params_obj) = stream.dict.get(b"Params").ok() else {
            continue;
        };
        let params = pdfdoc::peek_dict(doc, params_obj)
            .cloned()
            .unwrap_or_default();
        let subtype = stream
            .dict
            .get(b"Subtype")
            .ok()
            .and_then(|s| s.as_name().ok())
            .map(|n| String::from_utf8_lossy(n).into_owned());
        out.push(AttachmentInfo {
            filename: determine_filename(&entry),
            size: params
                .get(b"Size")
                .ok()
                .and_then(|s| s.as_i64().ok())
                .unwrap_or(-1),
            content_type: subtype,
            description: entry
                .spec
                .get(b"Desc")
                .ok()
                .and_then(text_of)
                .filter(|d| !d.is_empty()),
            creation_date: pdf_date(params.get(b"CreationDate").ok()),
            modification_date: pdf_date(params.get(b"ModDate").ok()),
        });
    }
    Ok(out)
}

/// Percent-decodes like Java's `decodeAttachmentName`: `%XX` sequences are
/// decoded, literal `+` is preserved.
fn decode_attachment_name(name: &str) -> Option<String> {
    let bytes = name.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            let s = std::str::from_utf8(hex).ok()?;
            out.push(u8::from_str_radix(s, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `findAttachmentEntry`: exact match on display name or key wins; a unique
/// case-insensitive match is next; simplified/decoded matching only when
/// unambiguous.
fn find_entry<'a>(entries: &'a [AttachmentEntry], target: &str) -> Option<&'a AttachmentEntry> {
    let norm = target.trim();
    if norm.is_empty() {
        return None;
    }

    let mut case_insensitive = None;
    for entry in entries {
        let candidate = determine_filename(entry);
        if norm == candidate || norm == entry.key {
            return Some(entry);
        }
        if norm.eq_ignore_ascii_case(&candidate) || norm.eq_ignore_ascii_case(&entry.key) {
            if case_insensitive.is_some() {
                return None;
            }
            case_insensitive = Some(entry);
        }
    }
    if case_insensitive.is_some() {
        return case_insensitive;
    }

    let mut fallback = None;
    for entry in entries {
        let candidate = determine_filename(entry);
        if matches_attachment_name(&candidate, &entry.key, target) {
            if fallback.is_some() {
                return None;
            }
            fallback = Some(entry);
        }
    }
    fallback
}

fn matches_attachment_name(candidate: &str, key: &str, target: &str) -> bool {
    let norm = target.trim();
    if norm.is_empty() {
        return false;
    }
    if norm.eq_ignore_ascii_case(candidate) || norm.eq_ignore_ascii_case(key) {
        return true;
    }
    let (st, sc, sk) = (simple_name(norm), simple_name(candidate), simple_name(key));
    if st.eq_ignore_ascii_case(sc) || st.eq_ignore_ascii_case(sk) {
        return true;
    }
    match (
        decode_attachment_name(norm),
        decode_attachment_name(candidate),
        decode_attachment_name(key),
    ) {
        (Some(dt), Some(dc), Some(dk)) => {
            dt.eq_ignore_ascii_case(&dc) || dt.eq_ignore_ascii_case(&dk)
        }
        _ => false,
    }
}

/// Extracts one attachment's bytes by name; `None` when absent, ambiguous,
/// or over the size cap (Java maps all of those to "not found").
pub fn extract_single(doc: &Document, name: &str) -> AppResult<Option<Vec<u8>>> {
    let entries = collect_attachments(doc)?;
    let Some(entry) = find_entry(&entries, name) else {
        return Ok(None);
    };
    Ok(attachment_bytes(doc, entry))
}

/// All attachments as `(unique filename, bytes)` pairs in a ZIP, like
/// `extractAttachments`: `/UF`→`/F`→key naming, simple-name + dedup suffixes.
pub fn extract_all(doc: &Document) -> AppResult<Option<Vec<u8>>> {
    let entries = collect_attachments(doc)?;
    if entries.is_empty() {
        return Ok(None);
    }
    let mut used = HashSet::new();
    let mut files = Vec::new();
    for entry in &entries {
        let Some(bytes) = attachment_bytes(doc, entry) else {
            continue;
        };
        let filename = simple_name(&determine_filename(entry)).to_string();
        let filename = if filename.trim().is_empty() {
            format!("unknown_attachment_{}", unix_millis())
        } else {
            filename
        };
        files.push((
            unique_filename(filename, &mut used),
            bytes,
            mod_time(doc, entry),
        ));
    }
    if files.is_empty() {
        return Ok(None);
    }

    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes, modified) in files {
        zip.start_file(name, options.last_modified_time(modified))
            .map_err(|e| AppError::Internal(e.into()))?;
        use std::io::Write;
        zip.write_all(&bytes)
            .map_err(|e| AppError::Internal(e.into()))?;
    }
    let cursor = zip.finish().map_err(|e| AppError::Internal(e.into()))?;
    Ok(Some(cursor.into_inner()))
}

/// `/Params /ModDate` of an entry's embedded file as a zip timestamp —
/// Java stamps each ZIP entry with the attachment's modification date.
fn mod_time(doc: &Document, entry: &AttachmentEntry) -> zip::DateTime {
    let Some(stream) = embedded_file_stream(doc, &entry.spec) else {
        return zip::DateTime::default();
    };
    let Some(params_obj) = stream.dict.get(b"Params").ok() else {
        return zip::DateTime::default();
    };
    let Some(params) = pdfdoc::peek_dict(doc, params_obj) else {
        return zip::DateTime::default();
    };
    let Some(raw) = params.get(b"ModDate").ok().and_then(text_of) else {
        return zip::DateTime::default();
    };
    let digits = |range: std::ops::Range<usize>| raw.get(range)?.parse::<i64>().ok();
    let (Some(y), Some(m), Some(d)) = (digits(2..6), digits(6..8), digits(8..10)) else {
        return zip::DateTime::default();
    };
    let (h, min, s) = (
        digits(10..12).unwrap_or(0),
        digits(12..14).unwrap_or(0),
        digits(14..16).unwrap_or(0),
    );
    let Some(naive) = chrono::NaiveDate::from_ymd_opt(y as i32, m as u32, d as u32)
        .and_then(|d| d.and_hms_opt(h as u32, min as u32, s as u32))
    else {
        return zip::DateTime::default();
    };
    // ZIP stores DOS datetimes as local wall-clock fields — no zone math.
    zip::DateTime::from_date_and_time(
        naive.year() as u16,
        naive.month() as u8,
        naive.day() as u8,
        naive.hour() as u8,
        naive.minute() as u8,
        naive.second() as u8,
    )
    .unwrap_or_default()
}

fn unique_filename(filename: String, used: &mut HashSet<String>) -> String {
    let (base, ext) = match filename.rfind('.') {
        Some(dot) if dot > 0 && dot < filename.len() - 1 => {
            (filename[..dot].to_string(), filename[dot..].to_string())
        }
        _ => (filename.clone(), String::new()),
    };
    let mut candidate = filename;
    let mut counter = 1;
    while used.contains(&candidate) {
        candidate = format!("{base}_{counter}{ext}");
        counter += 1;
    }
    used.insert(candidate.clone());
    candidate
}

/// Writes a JavaScript-compatible PDF text string: literal for Latin-1-safe
/// text, UTF-16BE-with-BOM hex otherwise.
pub(crate) fn text_string_object(text: &str) -> Object {
    if text.is_ascii() && !text.contains(['(', ')', '\\']) {
        return Object::string_literal(text.as_bytes().to_vec());
    }
    if text.chars().all(|c| (c as u32) <= 0xFF) {
        let mut bytes = Vec::with_capacity(text.len() + 2);
        for c in text.chars() {
            let b = c as u8;
            match b {
                b'(' | b')' | b'\\' => {
                    bytes.push(b'\\');
                    bytes.push(b);
                }
                _ => bytes.push(b),
            }
        }
        return Object::string_literal(bytes);
    }
    let mut bytes = vec![0xFE, 0xFF];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_be_bytes());
    }
    Object::String(bytes, lopdf::StringFormat::Hexadecimal)
}

/// Replaces the embedded-files name tree with a flat `/Names` array —
/// `setKids(null)` + `setNames(map)` in Java — after applying `f` to the
/// collected entries.
fn rewrite_name_tree(
    doc: &mut Document,
    f: impl FnOnce(&mut Vec<AttachmentEntry>),
) -> AppResult<bool> {
    if embedded_files_root(doc).is_none() {
        return Ok(false);
    }
    let mut entries = collect_attachments(doc)?;
    f(&mut entries);

    // Locate the dictionary holding /EmbeddedFiles: catalog → /Names →
    // /EmbeddedFiles, each possibly a reference or a direct dictionary.
    let catalog = doc.catalog().map_err(pdf_err)?;
    let Some(names_obj) = catalog.get(b"Names").ok().cloned() else {
        return Ok(false);
    };
    let names_path = match names_obj {
        Object::Reference(id) => pdfdoc::DictPath {
            root: id,
            keys: Vec::new(),
        },
        Object::Dictionary(_) => {
            let catalog_id = doc
                .trailer
                .get(b"Root")
                .ok()
                .and_then(|r| r.as_reference().ok())
                .ok_or_else(|| AppError::Pdf("missing catalog".into()))?;
            pdfdoc::DictPath {
                root: catalog_id,
                keys: vec![b"Names".to_vec()],
            }
        }
        _ => return Ok(false),
    };
    let Some(embedded_obj) =
        pdfdoc::dict_at(doc, &names_path).and_then(|d| d.get(b"EmbeddedFiles").ok().cloned())
    else {
        return Ok(false);
    };
    let embedded_path = match embedded_obj {
        Object::Reference(id) => pdfdoc::DictPath {
            root: id,
            keys: Vec::new(),
        },
        Object::Dictionary(_) => pdfdoc::DictPath {
            root: names_path.root,
            keys: {
                let mut k = names_path.keys.clone();
                k.push(b"EmbeddedFiles".to_vec());
                k
            },
        },
        _ => return Ok(false),
    };

    let mut flat = Vec::with_capacity(entries.len() * 2);
    for entry in entries {
        flat.push(text_string_object(&entry.key));
        match entry.spec_id {
            // Mutated spec dictionaries must be written back or the
            // reference would keep pointing at the unmodified object.
            Some(id) => {
                doc.objects.insert(id, Object::Dictionary(entry.spec));
                flat.push(Object::Reference(id));
            }
            None => flat.push(Object::Dictionary(entry.spec)),
        }
    }
    pdfdoc::update_at(doc, &embedded_path, move |dict| {
        dict.remove(b"Kids");
        dict.set("Names", Object::Array(std::mem::take(&mut flat)));
    })?;
    Ok(true)
}

/// Renames the first unambiguous match; errors like Java when absent.
pub fn rename_attachment(doc: &mut Document, name: &str, new_name: &str) -> AppResult<()> {
    let entries = collect_attachments(doc)?;
    let Some(target) = find_entry(&entries, name) else {
        return Err(AppError::BadRequest(format!(
            "attachment '{name}' not found for renaming"
        )));
    };
    let old_key = target.key.clone();
    rewrite_name_tree(doc, |entries| {
        for entry in entries.iter_mut() {
            if entry.key == old_key {
                entry.spec.set("F", text_string_object(new_name));
                entry.spec.set("UF", text_string_object(new_name));
                entry.key = new_name.to_string();
            }
        }
    })?;
    Ok(())
}

/// Deletes the first unambiguous match; errors like Java when absent.
pub fn delete_attachment(doc: &mut Document, name: &str) -> AppResult<()> {
    let entries = collect_attachments(doc)?;
    let Some(target) = find_entry(&entries, name) else {
        return Err(AppError::BadRequest(format!(
            "attachment '{name}' not found for deletion"
        )));
    };
    let old_key = target.key.clone();
    rewrite_name_tree(doc, |entries| {
        // Java's LinkedHashMap holds one entry per key; remove the first.
        if let Some(index) = entries.iter().position(|e| e.key == old_key) {
            entries.remove(index);
        }
    })?;
    Ok(())
}

/// PDF date string like PDFBox's `setDate`: `D:YYYYMMDDHHmmSS+HH'mm'`.
pub(crate) fn pdf_date_now() -> String {
    let now = chrono::Local::now();
    let offset = now.offset().local_minus_utc();
    let sign = if offset < 0 { '-' } else { '+' };
    let abs = offset.abs();
    format!(
        "D:{}{}{:02}'{:02}'",
        now.format("%Y%m%d%H%M%S"),
        sign,
        abs / 3600,
        (abs % 3600) / 60
    )
}

/// Creates catalog `/Names` → `/EmbeddedFiles` when absent so the name tree
/// has somewhere to live.
fn ensure_name_tree(doc: &mut Document) -> AppResult<()> {
    let catalog_id = doc
        .trailer
        .get(b"Root")
        .ok()
        .and_then(|r| r.as_reference().ok())
        .ok_or_else(|| AppError::Pdf("missing catalog".into()))?;
    let catalog = doc.get_dictionary(catalog_id).map_err(pdf_err)?.clone();

    let names_id = match catalog.get(b"Names") {
        Ok(Object::Reference(id)) => Some(*id),
        Ok(Object::Dictionary(d)) => {
            // Promote a direct /Names dictionary to an indirect object so the
            // EmbeddedFiles member can be added uniformly.
            let id = doc.new_object_id();
            doc.objects.insert(id, Object::Dictionary(d.clone()));
            let cat = doc.get_dictionary_mut(catalog_id).map_err(pdf_err)?;
            cat.set("Names", Object::Reference(id));
            Some(id)
        }
        _ => None,
    };
    let names_id = match names_id {
        Some(id) => id,
        None => {
            let id = doc.new_object_id();
            doc.objects
                .insert(id, Object::Dictionary(Dictionary::new()));
            let cat = doc.get_dictionary_mut(catalog_id).map_err(pdf_err)?;
            cat.set("Names", Object::Reference(id));
            id
        }
    };

    let has_ef = doc
        .get_dictionary(names_id)
        .map_err(pdf_err)?
        .get(b"EmbeddedFiles")
        .is_ok();
    if !has_ef {
        let mut ef = Dictionary::new();
        ef.set("Names", Object::Array(Vec::new()));
        let ef_id = doc.new_object_id();
        doc.objects.insert(ef_id, Object::Dictionary(ef));
        let names = doc.get_dictionary_mut(names_id).map_err(pdf_err)?;
        names.set("EmbeddedFiles", Object::Reference(ef_id));
    }
    Ok(())
}

/// `AttachmentService.addAttachment`: appends `(filename, bytes)` pairs to
/// the embedded-files name tree — creating it when absent — and switches the
/// catalog to `/PageMode /UseAttachments`.
pub fn add_attachments(doc: &mut Document, files: &[(String, Vec<u8>)]) -> AppResult<()> {
    let date = pdf_date_now();
    let mut new_entries = Vec::with_capacity(files.len());
    for (filename, bytes) in files {
        let mut ef = Dictionary::new();
        let mut stream_dict = Dictionary::new();
        stream_dict.set("Type", "EmbeddedFile");
        {
            let mut params = Dictionary::new();
            params.set("Size", bytes.len() as i64);
            params.set("CreationDate", text_string_object(&date));
            params.set("ModDate", text_string_object(&date));
            stream_dict.set("Params", Object::Dictionary(params));
        }
        let stream_id = doc.new_object_id();
        doc.objects.insert(
            stream_id,
            Object::Stream(Stream::new(stream_dict, bytes.clone())),
        );
        ef.set("F", Object::Reference(stream_id));
        ef.set("UF", Object::Reference(stream_id));

        let mut spec = Dictionary::new();
        spec.set("Type", "Filespec");
        spec.set("F", text_string_object(filename));
        spec.set("UF", text_string_object(filename));
        spec.set(
            "Desc",
            text_string_object(&format!("Embedded attachment: {filename}")),
        );
        spec.set("EF", Object::Dictionary(ef));
        let spec_id = doc.new_object_id();
        new_entries.push(AttachmentEntry {
            key: filename.clone(),
            spec,
            spec_id: Some(spec_id),
        });
    }

    ensure_name_tree(doc)?;
    rewrite_name_tree(doc, |entries| {
        for entry in new_entries {
            match entries.iter_mut().find(|e| e.key == entry.key) {
                Some(existing) => *existing = entry,
                None => entries.push(entry),
            }
        }
    })?;

    if let Ok(catalog) = doc.catalog_mut() {
        catalog.set("PageMode", Object::Name(b"UseAttachments".to_vec()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit;
    use lopdf::dictionary;

    /// Catalog → /Names → /EmbeddedFiles with one filespec + embedded stream.
    pub(super) fn attachment_pdf() -> testkit::Pdf {
        let mut pdf = testkit::Pdf::new();
        let embedded = pdf.add_stream(
            dictionary! {
                "Type" => "EmbeddedFile",
                "Subtype" => "text/plain",
                "Params" => dictionary! {
                    "Size" => 11,
                    "CreationDate" => Object::string_literal(b"D:20240101120000Z".to_vec()),
                    "ModDate" => Object::string_literal(b"D:20240202080000Z".to_vec()),
                },
            },
            b"hello world",
        );
        let spec = pdf.add(dictionary! {
            "Type" => "Filespec",
            "F" => Object::string_literal(b"report.txt".to_vec()),
            "UF" => Object::string_literal(b"report.txt".to_vec()),
            "Desc" => Object::string_literal(b"quarterly report".to_vec()),
            "EF" => dictionary! {
                "F" => Object::Reference(embedded),
                "UF" => Object::Reference(embedded),
            },
        });
        let embedded_files = pdf.add(dictionary! {
            "Names" => vec![
                text_string_object("report.txt"),
                Object::Reference(spec),
            ],
        });
        let names = pdf.add(dictionary! {
            "EmbeddedFiles" => Object::Reference(embedded_files),
        });
        pdf.edit_catalog(|catalog| {
            catalog.set("Names", Object::Reference(names));
        });
        pdf
    }

    #[test]
    fn lists_attachment_metadata() {
        let mut pdf = attachment_pdf();
        let bytes = pdf.bytes();
        let doc = Document::load_mem(&bytes).unwrap();
        let infos = list_attachments(&doc).unwrap();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].filename, "report.txt");
        assert_eq!(infos[0].size, 11);
        assert_eq!(infos[0].content_type.as_deref(), Some("text/plain"));
        assert_eq!(infos[0].description.as_deref(), Some("quarterly report"));
        assert!(infos[0].creation_date.is_some());
    }

    #[test]
    fn extracts_single_attachment_bytes() {
        let mut pdf = attachment_pdf();
        let bytes = pdf.bytes();
        let doc = Document::load_mem(&bytes).unwrap();
        let data = extract_single(&doc, "report.txt").unwrap().unwrap();
        assert_eq!(data, b"hello world");
        assert!(extract_single(&doc, "missing.bin").unwrap().is_none());
    }

    #[test]
    fn zips_all_attachments() {
        let mut pdf = attachment_pdf();
        let bytes = pdf.bytes();
        let doc = Document::load_mem(&bytes).unwrap();
        let zip = extract_all(&doc).unwrap().unwrap();
        assert_eq!(&zip[..2], b"PK");
        let mut reader = zip::ZipArchive::new(std::io::Cursor::new(zip)).unwrap();
        assert_eq!(reader.len(), 1);
        let mut file = reader.by_index(0).unwrap();
        assert_eq!(file.name(), "report.txt");
        let mut contents = String::new();
        use std::io::Read;
        file.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "hello world");
    }

    #[test]
    fn renames_and_deletes_attachment() {
        let mut pdf = attachment_pdf();
        let bytes = pdf.bytes();
        let mut doc = Document::load_mem(&bytes).unwrap();

        rename_attachment(&mut doc, "report.txt", "renamed.txt").unwrap();
        let entries = collect_attachments(&doc).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "renamed.txt");
        assert_eq!(determine_filename(&entries[0]), "renamed.txt");

        delete_attachment(&mut doc, "renamed.txt").unwrap();
        assert!(collect_attachments(&doc).unwrap().is_empty());
    }
}

#[cfg(test)]
mod add_tests {
    use super::*;
    use crate::testkit;

    #[test]
    fn adds_attachment_to_plain_pdf_and_lists_it() {
        let mut pdf = testkit::Pdf::new();
        let bytes = pdf.bytes();
        let mut doc = Document::load_mem(&bytes).unwrap();
        add_attachments(
            &mut doc,
            &[("note.txt".into(), b"attachment data".to_vec())],
        )
        .unwrap();
        let out = pdfdoc::save(&mut doc).unwrap();
        let doc = Document::load_mem(&out).unwrap();

        let infos = list_attachments(&doc).unwrap();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].filename, "note.txt");
        let entries = collect_attachments(&doc).unwrap();
        let data = attachment_bytes(&doc, &entries[0]).unwrap();
        assert_eq!(data, b"attachment data");
        // PageMode switched to UseAttachments.
        let catalog = doc.catalog().unwrap();
        assert_eq!(
            catalog.get(b"PageMode").unwrap().as_name().unwrap(),
            b"UseAttachments"
        );
    }

    #[test]
    fn add_preserves_existing_attachment() {
        let mut pdf = tests::attachment_pdf();
        let bytes = pdf.bytes();
        let mut doc = Document::load_mem(&bytes).unwrap();
        add_attachments(&mut doc, &[("extra.bin".into(), vec![1, 2, 3])]).unwrap();
        let out = pdfdoc::save(&mut doc).unwrap();
        let doc = Document::load_mem(&out).unwrap();
        let entries = collect_attachments(&doc).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(extract_single(&doc, "report.txt").unwrap().is_some());
        assert_eq!(
            extract_single(&doc, "extra.bin").unwrap().unwrap(),
            vec![1, 2, 3]
        );
    }
}
