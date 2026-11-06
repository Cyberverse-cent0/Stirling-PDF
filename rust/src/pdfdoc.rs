//! Pure-Rust PDF backend on `lopdf`'s object model: page-tree surgery for
//! reorder/remove, reachability copying for merge/split, dictionary reads
//! for the analysis endpoints.

// Shared helper surface for endpoints still being ported; unused helpers are
// expected while the strangler migration is incomplete.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};

use lopdf::{
    decode_text_string, Dictionary, Document, LoadOptions, Object, ObjectId, ObjectStream,
    SaveOptions, Stream, StringFormat,
};

use crate::error::{AppError, AppResult};

fn pdf_err(e: lopdf::Error) -> AppError {
    AppError::Pdf(e.to_string())
}

/// Loads a PDF from uploaded bytes. lopdf opens empty-password encrypted
/// files transparently; a file still carrying `/Encrypt` afterwards needs a
/// password these endpoints don't accept, matching the old loaders.
pub fn load(name: &str, bytes: &[u8]) -> AppResult<Document> {
    let doc =
        Document::load_mem(bytes).map_err(|e| AppError::BadRequest(format!("{name}: {e}")))?;
    finish_load(name, doc)
}

/// Password-protected variant for the security endpoints: the caller
/// supplies the opening credential. A wrong password fails inside
/// `load_mem_with_password`; the still-encrypted check below catches a
/// document that loaded without decrypting.
pub fn load_with_password(name: &str, bytes: &[u8], password: &str) -> AppResult<Document> {
    let doc = Document::load_mem_with_options(bytes, LoadOptions::with_password(password))
        .map_err(|e| AppError::BadRequest(format!("{name}: {e}")))?;
    finish_load(name, doc)
}

fn finish_load(name: &str, mut doc: Document) -> AppResult<Document> {
    if doc.trailer.has(b"Encrypt") && !doc.was_encrypted() {
        return Err(AppError::BadRequest(format!(
            "{name}: password required or incorrect password"
        )));
    }
    // lopdf expands /ObjStm contents only on the decrypt path, so an
    // unencrypted file keeps its page tree inside object streams and
    // looks page-less. Expand them the same way lopdf does after
    // decryption; `or_insert` never clobbers a real object.
    let mut pending = Vec::new();
    for object in doc.objects.values() {
        let Ok(stream) = object.as_stream() else {
            continue;
        };
        // Real-world files carry `/Type` misspelled (`/Txpe`); `/N` +
        // `/First` are the structural markers PDFBox keys object streams on.
        let is_objstm = stream.dict.has_type(b"ObjStm")
            || (stream.dict.get(b"N").is_ok() && stream.dict.get(b"First").is_ok());
        if !is_objstm {
            continue;
        }
        if let Ok(obj_stream) = ObjectStream::new_with_limit(stream, Some(64 * 1024 * 1024)) {
            pending.extend(obj_stream.objects);
        }
    }
    for (id, entry) in pending {
        doc.objects.entry(id).or_insert(entry);
    }
    Ok(doc)
}

/// Serialises with object streams and a cross-reference stream: smaller
/// output than the classic table for the object-heavy files these endpoints
/// produce.
pub fn save(doc: &mut Document) -> AppResult<Vec<u8>> {
    let mut out = Vec::new();
    doc.save_with_options(
        &mut out,
        SaveOptions::builder()
            .use_object_streams(true)
            .use_xref_streams(true)
            .build(),
    )
    .map_err(|e| AppError::Pdf(e.to_string()))?;
    Ok(out)
}

/// Page object ids in page order.
pub fn page_ids(doc: &Document) -> Vec<ObjectId> {
    doc.get_pages().into_values().collect()
}

fn pages_root(doc: &Document) -> AppResult<ObjectId> {
    let catalog = doc.catalog().map_err(pdf_err)?;
    match catalog.get(b"Pages").map_err(pdf_err)? {
        Object::Reference(id) => Ok(*id),
        _ => Err(AppError::Pdf(
            "catalog /Pages is not an indirect reference".into(),
        )),
    }
}

/// An inheritable page attribute (`MediaBox`, `CropBox`, `Resources`,
/// `Rotate`, `UserUnit`) as `(holder, value)`: the object id of the
/// page-tree node carrying it and the stored value, so callers can write
/// mutations back where they live — including on an ancestor when the
/// attribute is inherited.
fn inherited_slot(
    doc: &Document,
    page_id: ObjectId,
    key: &[u8],
) -> AppResult<Option<(ObjectId, Object)>> {
    let mut current = page_id;
    let mut visited = HashSet::new();
    loop {
        if !visited.insert(current) {
            return Err(AppError::Pdf(format!("reference cycle at {current:?}")));
        }
        let node = doc.get_dictionary(current).map_err(pdf_err)?;
        if let Ok(value) = node.get(key) {
            match doc.dereference(value).map_err(pdf_err)? {
                (_, Object::Null) => {}
                _ => return Ok(Some((current, value.clone()))),
            }
        }
        match node.get(b"Parent") {
            Err(_) | Ok(Object::Null) => return Ok(None),
            Ok(parent) => {
                current = parent
                    .as_reference()
                    .map_err(|_| AppError::Pdf("page /Parent is not a reference".into()))?
            }
        }
    }
}

/// Value of an inheritable page attribute: the nearest one walking up
/// `/Parent`, per the page-tree inheritance rules.
fn inherited(doc: &Document, page_id: ObjectId, key: &[u8]) -> AppResult<Option<Object>> {
    Ok(inherited_slot(doc, page_id, key)?.map(|(_, value)| value))
}

/// The `/Resources` value a page resolves to through page-tree inheritance,
/// paired with the id of the node holding it.
pub fn page_resources(doc: &Document, page_id: ObjectId) -> AppResult<Option<(ObjectId, Object)>> {
    Ok(page_resources_holder(doc, page_id))
}

/// Page-tree attributes a page inherits from its ancestors when it does not
/// carry them itself.
const INHERITABLE_PAGE_KEYS: [&[u8]; 5] = [
    b"MediaBox",
    b"CropBox",
    b"Resources",
    b"Rotate",
    b"UserUnit",
];

/// Copies inheritable attributes onto the page dictionary so it survives
/// being detached from its original (possibly nested) page-tree position.
/// One walk up `/Parent` collects every missing key at once.
fn materialize(doc: &mut Document, page_id: ObjectId) -> AppResult<()> {
    let mut page = doc.get_dictionary(page_id).map_err(pdf_err)?.clone();
    let mut missing: Vec<&[u8]> = INHERITABLE_PAGE_KEYS
        .iter()
        .copied()
        .filter(|key| !page.has(key))
        .collect();
    let mut found: Vec<(&[u8], Object)> = Vec::new();

    // One walk up /Parent collects every missing key, nearest ancestor wins.
    let mut current = page_id;
    let mut visited = HashSet::new();
    while !missing.is_empty() {
        if !visited.insert(current) {
            return Err(AppError::Pdf(format!("reference cycle at {current:?}")));
        }
        let node = doc.get_dictionary(current).map_err(pdf_err)?;
        missing.retain(|key| match node.get(key) {
            Ok(value) => match doc.dereference(value) {
                Ok((_, Object::Null)) | Err(_) => true,
                _ => {
                    found.push((*key, value.clone()));
                    false
                }
            },
            Err(_) => true,
        });
        match node.get(b"Parent") {
            Err(_) | Ok(Object::Null) => break,
            Ok(parent) => {
                current = parent
                    .as_reference()
                    .map_err(|_| AppError::Pdf("page /Parent is not a reference".into()))?
            }
        }
    }

    for (key, value) in found {
        page.set(key, value);
    }
    doc.objects.insert(page_id, Object::Dictionary(page));
    Ok(())
}

/// Rebuilds the page tree in place so it lists exactly `order` — 0-based
/// page indices, repeats allowed. Everything outside the page tree (catalog
/// extras, `/Info`, AcroForm, outlines) is preserved; detached page-tree
/// nodes and removed pages stay in the file as unreachable objects, the
/// same trade-off PDFBox makes. Returns an error on an out-of-range index.
pub fn subset(doc: &mut Document, order: &[u32]) -> AppResult<()> {
    let pages = page_ids(doc);
    let root = pages_root(doc)?;

    let mut kids = Vec::with_capacity(order.len());
    let mut used: HashMap<ObjectId, u32> = HashMap::new();
    for &index in order {
        let Some(&src) = pages.get(index as usize) else {
            return Err(AppError::BadRequest(format!(
                "page index {} out of range ({} pages)",
                index + 1,
                pages.len()
            )));
        };
        materialize(doc, src)?;
        let occurrence = used.entry(src).or_insert(0);
        *occurrence += 1;
        let id = if *occurrence == 1 {
            src
        } else {
            // A page object may appear in the tree only once, so repeats get
            // a shallow copy sharing Contents/Resources/Annots.
            let page = doc.get_dictionary(src).map_err(pdf_err)?.clone();
            let id = doc.new_object_id();
            doc.objects.insert(id, Object::Dictionary(page));
            id
        };
        kids.push(id);
    }

    for &id in &kids {
        doc.get_dictionary_mut(id)
            .map_err(pdf_err)?
            .set("Parent", root);
    }
    let root_dict = doc.get_dictionary_mut(root).map_err(pdf_err)?;
    root_dict.set(
        "Kids",
        Object::Array(kids.iter().map(|&id| Object::Reference(id)).collect()),
    );
    root_dict.set("Count", kids.len() as i64);
    Ok(())
}

/// A fresh document with an empty page tree, ready for [`import_pages`].
pub fn new_document() -> Document {
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let mut pages = Dictionary::new();
    pages.set("Type", "Pages");
    pages.set("Kids", Object::Array(Vec::new()));
    pages.set("Count", 0i64);
    doc.objects.insert(pages_id, Object::Dictionary(pages));
    let catalog_id = doc.new_object_id();
    let mut catalog = Dictionary::new();
    catalog.set("Type", "Catalog");
    catalog.set("Pages", pages_id);
    doc.objects.insert(catalog_id, Object::Dictionary(catalog));
    doc.trailer.set("Root", catalog_id);
    doc
}

/// Appends `page_numbers` (0-based) of `src` to `dst`'s page tree. Each
/// page and every object it reaches — contents, resources, fonts,
/// annotations — is copied with remapped object ids. Document-level
/// structures (outlines, AcroForm, names, `/Info`) are not carried over,
/// matching what a page import produces.
pub fn import_pages(
    dst: &mut Document,
    src: &mut Document,
    page_numbers: &[usize],
) -> AppResult<()> {
    let src_pages = page_ids(src);
    let root = pages_root(dst)?;
    let mut map = HashMap::new();
    let mut new_ids = Vec::with_capacity(page_numbers.len());
    for &n in page_numbers {
        let Some(&src_id) = src_pages.get(n) else {
            return Err(AppError::Pdf(format!(
                "page index {} out of range ({} pages)",
                n + 1,
                src_pages.len()
            )));
        };
        materialize(src, src_id)?;
        new_ids.push(import_page(dst, src, src_id, &mut map)?);
    }
    if new_ids.is_empty() {
        return Ok(());
    }
    let added = new_ids.len() as i64;
    {
        let root_dict = dst.get_dictionary_mut(root).map_err(pdf_err)?;
        let mut kids = root_dict
            .get(b"Kids")
            .and_then(|k| k.as_array())
            .map_err(pdf_err)?
            .clone();
        for id in &new_ids {
            kids.push(Object::Reference(*id));
        }
        root_dict.set("Kids", Object::Array(kids));
        let count = root_dict
            .get(b"Count")
            .and_then(|c| c.as_i64())
            .unwrap_or(0);
        root_dict.set("Count", count + added);
    }
    for id in new_ids {
        dst.get_dictionary_mut(id)
            .map_err(pdf_err)?
            .set("Parent", root);
    }
    Ok(())
}

/// Copies the source trailer's `/Info` dictionary — and anything it
/// references — into `dst`. Merge uses it so document metadata survives
/// the page import, which deliberately skips document-level structures.
pub fn import_info(dst: &mut Document, src: &Document) -> AppResult<()> {
    let Some(info) = src.trailer.get(b"Info").ok().cloned() else {
        return Ok(());
    };
    let mut map = HashMap::new();
    let copied = remap(dst, src, &info, &mut map)?;
    match copied {
        Object::Reference(id) => dst.trailer.set("Info", id),
        other => {
            let id = dst.new_object_id();
            dst.objects.insert(id, other);
            dst.trailer.set("Info", id);
        }
    }
    Ok(())
}

/// Copies one page dictionary plus its transitive object closure into
/// `dst`. `/Parent` is dropped before remapping so the source page tree is
/// never pulled in; it is re-pointed at the destination root by the caller.
fn import_page(
    dst: &mut Document,
    src: &Document,
    src_id: ObjectId,
    map: &mut HashMap<ObjectId, ObjectId>,
) -> AppResult<ObjectId> {
    if let Some(&id) = map.get(&src_id) {
        return Ok(id);
    }
    let new_id = dst.new_object_id();
    map.insert(src_id, new_id);
    let mut page = src.get_dictionary(src_id).map_err(pdf_err)?.clone();
    page.remove(b"Parent");
    let mut copied = Dictionary::new();
    for (key, value) in page.iter() {
        copied.set(key.clone(), remap(dst, src, value, map)?);
    }
    dst.objects.insert(new_id, Object::Dictionary(copied));
    Ok(new_id)
}

/// Copies any indirect object and its closure into `dst`, remapping
/// references. The id mapping is recorded before recursion so reference
/// cycles terminate.
fn import_object(
    dst: &mut Document,
    src: &Document,
    src_id: ObjectId,
    map: &mut HashMap<ObjectId, ObjectId>,
) -> AppResult<ObjectId> {
    if let Some(&id) = map.get(&src_id) {
        return Ok(id);
    }
    let new_id = dst.new_object_id();
    map.insert(src_id, new_id);
    let object = src.get_object(src_id).map_err(pdf_err)?.clone();
    let copied = remap(dst, src, &object, map)?;
    dst.objects.insert(new_id, copied);
    Ok(new_id)
}

fn remap(
    dst: &mut Document,
    src: &Document,
    object: &Object,
    map: &mut HashMap<ObjectId, ObjectId>,
) -> AppResult<Object> {
    Ok(match object {
        Object::Reference(id) => Object::Reference(import_object(dst, src, *id, map)?),
        Object::Dictionary(dict) => {
            let mut copied = Dictionary::new();
            for (key, value) in dict.iter() {
                // A copied object must never adopt a foreign page-tree parent;
                // pulling one in would drag the whole source tree across.
                if key == b"Parent" {
                    continue;
                }
                copied.set(key.clone(), remap(dst, src, value, map)?);
            }
            Object::Dictionary(copied)
        }
        Object::Array(items) => Object::Array(
            items
                .iter()
                .map(|item| remap(dst, src, item, map))
                .collect::<AppResult<Vec<_>>>()?,
        ),
        Object::Stream(stream) => {
            let mut copied = stream.clone();
            let mut dict = Dictionary::new();
            for (key, value) in stream.dict.iter() {
                if key == b"Parent" {
                    continue;
                }
                dict.set(key.clone(), remap(dst, src, value, map)?);
            }
            copied.dict = dict;
            Object::Stream(copied)
        }
        _ => object.clone(),
    })
}

/// Effective `/Rotate` of a page in degrees, honouring page-tree
/// inheritance.
pub fn rotation(doc: &Document, page_id: ObjectId) -> AppResult<i64> {
    Ok(match inherited(doc, page_id, b"Rotate")? {
        Some(Object::Integer(deg)) => deg.rem_euclid(360),
        _ => 0,
    })
}

/// Sets a page's own `/Rotate`, like `PDPage.setRotation`.
pub fn set_rotation(doc: &mut Document, page_id: ObjectId, degrees: i64) -> AppResult<()> {
    doc.get_dictionary_mut(page_id)
        .map_err(pdf_err)?
        .set("Rotate", degrees.rem_euclid(360));
    Ok(())
}

/// Post-rotation page size in points, matching what a renderer reports:
/// the crop box (or media box) with width/height swapped on 90°/270°.
pub fn page_dimensions(doc: &Document, page_id: ObjectId) -> AppResult<(f64, f64)> {
    let bbox = match inherited(doc, page_id, b"CropBox")? {
        Some(b) => b,
        None => inherited(doc, page_id, b"MediaBox")?
            .ok_or_else(|| AppError::Pdf("page has no MediaBox or CropBox".into()))?,
    };
    let Object::Array(box_) = bbox else {
        return Err(AppError::Pdf("page box is not an array".into()));
    };
    if box_.len() < 4 {
        return Err(AppError::Pdf("page box has fewer than 4 elements".into()));
    }
    let num = |o: &Object| -> AppResult<f64> {
        match o {
            Object::Integer(i) => Ok(*i as f64),
            Object::Real(f) => Ok(*f as f64),
            _ => Err(AppError::Pdf("page box element is not a number".into())),
        }
    };
    let (x0, y0, x1, y1) = (
        num(&box_[0])?,
        num(&box_[1])?,
        num(&box_[2])?,
        num(&box_[3])?,
    );
    let (mut w, mut h) = ((x1 - x0).abs(), (y1 - y0).abs());
    if matches!(rotation(doc, page_id)?, 90 | 270) {
        std::mem::swap(&mut w, &mut h);
    }
    Ok((w, h))
}

/// Trailer `/Info` string field, PDFDoc/UTF-16 decoded. Returns the raw PDF
/// date string for date fields, like the previous backend.
pub fn info_value(doc: &Document, key: &str) -> Option<String> {
    let info = doc.trailer.get(b"Info").ok()?;
    let (_, object) = doc.dereference(info).ok()?;
    let Object::Dictionary(dict) = object else {
        return None;
    };
    let value = dict.get(key.as_bytes()).ok()?;
    decode_text_string(value).ok().or_else(|| {
        value
            .as_str()
            .ok()
            .map(|b| String::from_utf8_lossy(b).into_owned())
    })
}

/// Resolved `/Annots` dictionaries of a page (references followed).
pub fn annotations(doc: &Document, page_id: ObjectId) -> AppResult<Vec<Dictionary>> {
    let page = doc.get_dictionary(page_id).map_err(pdf_err)?;
    let Some(annots) = page.get(b"Annots").ok() else {
        return Ok(Vec::new());
    };
    let (_, resolved) = doc.dereference(annots).map_err(pdf_err)?;
    let Object::Array(items) = resolved else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let (_, object) = doc.dereference(item).map_err(pdf_err)?;
        if let Object::Dictionary(dict) = object {
            out.push(dict.clone());
        }
    }
    Ok(out)
}

/// `/Subtype` name of an annotation dictionary, or `"Unknown"`.
pub fn annotation_subtype(annot: &Dictionary) -> String {
    annot
        .get(b"Subtype")
        .ok()
        .and_then(|o| o.as_name().ok())
        .map(|n| String::from_utf8_lossy(n).into_owned())
        .unwrap_or_else(|| "Unknown".into())
}

/// Base font names declared in the page's resource dictionaries.
///
/// Divergence: unlike the renderer-backed version this lists fonts the page
/// declares, including ones no text object uses, and does not descend into
/// form XObjects.
/// Font resource names (`F1`, `F40`, …) — the keys of `/Resources/Font`,
/// matching Java's `PDResources.getFontNames()`.
///
/// Divergence: unlike the renderer-backed version this lists fonts the page
/// declares, including ones no text object uses, and does not descend into
/// form XObjects.
pub fn page_fonts(doc: &Document, page_id: ObjectId) -> AppResult<Vec<String>> {
    let fonts = doc.get_page_fonts(page_id).map_err(pdf_err)?;
    Ok(fonts
        .keys()
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .collect())
}

/// The catalog's AcroForm dictionary, if any.
pub(crate) fn acroform(doc: &Document) -> Option<Dictionary> {
    let catalog = doc.catalog().ok()?;
    let (_, object) = doc.dereference(catalog.get(b"AcroForm").ok()?).ok()?;
    match object {
        Object::Dictionary(d) => Some(d.clone()),
        _ => None,
    }
}

pub fn has_xfa(doc: &Document) -> bool {
    #[allow(dead_code)]
    acroform(doc).is_some_and(|form| form.has(b"XFA"))
}

/// Whether any signature field exists, checking the AcroForm field tree
/// (`/FT /Sig`, recursing through `/Kids`) and merged widget/field
/// annotations on every page.
pub fn signature_field_exists(doc: &Document) -> AppResult<bool> {
    let mut visited = HashSet::new();
    if let Some(form) = acroform(doc) {
        if let Ok(fields) = form.get(b"Fields") {
            let (_, resolved) = doc.dereference(fields).map_err(pdf_err)?;
            if let Object::Array(items) = resolved {
                for item in items {
                    if signature_in_field(doc, item, &mut visited)? {
                        return Ok(true);
                    }
                }
            }
        }
    }
    for page_id in page_ids(doc) {
        for annot in annotations(doc, page_id)? {
            if annot.get(b"FT").ok().and_then(|o| o.as_name().ok()) == Some(b"Sig") {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn signature_in_field(
    doc: &Document,
    object: &Object,
    visited: &mut HashSet<ObjectId>,
) -> AppResult<bool> {
    let (_, resolved) = doc.dereference(object).map_err(pdf_err)?;
    let Object::Dictionary(dict) = resolved else {
        return Ok(false);
    };
    if dict.get(b"FT").ok().and_then(|o| o.as_name().ok()) == Some(b"Sig") {
        return Ok(true);
    }
    let Some(id) = object.as_reference().ok() else {
        return Ok(false);
    };
    if !visited.insert(id) {
        return Ok(false);
    }
    if let Ok(kids) = dict.get(b"Kids") {
        let (_, kids) = doc.dereference(kids).map_err(pdf_err)?;
        if let Object::Array(items) = kids {
            for kid in items {
                if signature_in_field(doc, kid, visited)? {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

/// Whether any AcroForm signature field carries a `/V` signature
/// dictionary — the `PDDocument.getSignatureDictionaries()` check the
/// password endpoints use to refuse protection changes on signed files.
pub fn signature_dict_exists(doc: &Document) -> AppResult<bool> {
    let mut visited = HashSet::new();
    let Some(form) = acroform(doc) else {
        return Ok(false);
    };
    let Ok(fields) = form.get(b"Fields") else {
        return Ok(false);
    };
    let (_, resolved) = doc.dereference(fields).map_err(pdf_err)?;
    let Object::Array(items) = resolved else {
        return Ok(false);
    };
    for item in items {
        if signature_value_in_field(doc, item, &mut visited, 0)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn signature_value_in_field(
    doc: &Document,
    object: &Object,
    visited: &mut HashSet<ObjectId>,
    depth: usize,
) -> AppResult<bool> {
    if depth > 100 {
        return Ok(false);
    }
    let Some(dict) = dict_owned(doc, object)? else {
        return Ok(false);
    };
    let is_sig = dict
        .get(b"FT")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| o.as_name().ok())
        .is_some_and(|n| n == b"Sig");
    if is_sig
        && dict
            .get(b"V")
            .ok()
            .and_then(|v| doc.dereference(v).ok())
            .is_some_and(|(_, o)| matches!(o, Object::Dictionary(_)))
    {
        return Ok(true);
    }
    let Some(id) = object.as_reference().ok() else {
        return Ok(false);
    };
    if !visited.insert(id) {
        return Ok(false);
    }
    let Ok(kids) = dict.get(b"Kids") else {
        return Ok(false);
    };
    let Ok((_, resolved)) = doc.dereference(kids) else {
        return Ok(false);
    };
    let Object::Array(items) = resolved else {
        return Ok(false);
    };
    for kid in items {
        if signature_value_in_field(doc, kid, visited, depth + 1)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Raw `/P` permission bits of the trailer `/Encrypt` dictionary, or `None`
/// for unencrypted files. Works whether `/Encrypt` is a reference or a
/// direct dictionary.
pub fn encrypt_permissions(doc: &Document) -> Option<u32> {
    let encrypt = doc.trailer.get(b"Encrypt").ok()?;
    let (_, object) = doc.dereference(encrypt).ok()?;
    let dict = match object {
        Object::Dictionary(d) => d,
        _ => return None,
    };
    dict.get(b"P").ok()?.as_i64().ok().map(|p| p as u32)
}

pub fn is_encrypted(doc: &Document) -> bool {
    doc.trailer.has(b"Encrypt")
}

/// A node of the document outline, mirroring Java's recursive bookmark
/// collection: `page` is the 0-based index of the destination page, `None`
/// for non-internal destinations.
#[derive(Debug)]
pub struct OutlineNode {
    pub title: String,
    pub page: Option<usize>,
    pub children: Vec<OutlineNode>,
}

/// Walks the catalog `/Outlines` tree depth-first.
pub fn outline(doc: &Document) -> AppResult<Vec<OutlineNode>> {
    let catalog = doc.catalog().map_err(pdf_err)?;
    let Some(outlines) = catalog.get(b"Outlines").ok() else {
        return Ok(Vec::new());
    };
    let (_, resolved) = doc.dereference(outlines).map_err(pdf_err)?;
    let Object::Dictionary(root) = resolved else {
        return Ok(Vec::new());
    };
    let Some(first) = root.get(b"First").ok() else {
        return Ok(Vec::new());
    };
    let page_lookup = page_lookup(doc)?;
    let mut nodes = Vec::new();
    outline_items(doc, first, &page_lookup, &mut nodes)?;
    Ok(nodes)
}

fn page_lookup(doc: &Document) -> AppResult<HashMap<ObjectId, usize>> {
    Ok(doc
        .get_pages()
        .into_iter()
        .map(|(num, id)| (id, (num - 1) as usize))
        .collect())
}

/// Iterates an outline item and its `/Next` siblings, recursing into
/// `/FirstChild` subtrees.
fn outline_items(
    doc: &Document,
    first: &Object,
    pages: &HashMap<ObjectId, usize>,
    out: &mut Vec<OutlineNode>,
) -> AppResult<()> {
    let mut current = Some(first.clone());
    while let Some(item) = current {
        let (_, resolved) = doc.dereference(&item).map_err(pdf_err)?;
        let Object::Dictionary(dict) = resolved else {
            break;
        };
        let title = dict
            .get(b"Title")
            .ok()
            .and_then(|t| {
                let (_, t) = doc.dereference(t).ok()?;
                decode_text_string(t).ok()
            })
            .unwrap_or_default()
            .replace('/', "");
        let page = outline_dest_page(doc, dict, pages)?;
        let mut children = Vec::new();
        if let Ok(first_child) = dict.get(b"FirstChild") {
            outline_items(doc, first_child, pages, &mut children)?;
        }
        out.push(OutlineNode {
            title,
            page,
            children,
        });
        current = dict.get(b"Next").ok().cloned();
    }
    Ok(())
}

/// The 0-based page index an outline item points at, or `None` when the
/// destination is not a local page (URI, remote file, named destination
/// that doesn't resolve).
fn outline_dest_page(
    doc: &Document,
    item: &Dictionary,
    pages: &HashMap<ObjectId, usize>,
) -> AppResult<Option<usize>> {
    if let Ok(dest) = item.get(b"Dest") {
        return dest_page_index(doc, dest, pages);
    }
    if let Ok(action) = item.get(b"A") {
        let (_, resolved) = doc.dereference(action).map_err(pdf_err)?;
        if let Object::Dictionary(dict) = resolved {
            if dict.get(b"S").ok().and_then(|s| s.as_name().ok()) == Some(b"GoTo") {
                if let Ok(d) = dict.get(b"D") {
                    return dest_page_index(doc, d, pages);
                }
            }
        }
    }
    Ok(None)
}

fn dest_page_index(
    doc: &Document,
    dest: &Object,
    pages: &HashMap<ObjectId, usize>,
) -> AppResult<Option<usize>> {
    let (_, resolved) = doc.dereference(dest).map_err(pdf_err)?;
    match resolved {
        Object::Array(items) => {
            // [page /Fit …] — the first element is the page reference.
            match items.first() {
                Some(Object::Reference(id)) => Ok(pages.get(id).copied()),
                _ => Ok(None),
            }
        }
        Object::Name(name) => named_destination(doc, name, pages),
        Object::String(bytes, _) => named_destination(doc, bytes, pages),
        Object::Dictionary(dict) => {
            // A destination dictionary wraps the real destination in /D.
            match dict.get(b"D") {
                Ok(d) => dest_page_index(doc, d, pages),
                Err(_) => Ok(None),
            }
        }
        _ => Ok(None),
    }
}

fn box_numbers(object: &Object) -> AppResult<[f64; 4]> {
    let Object::Array(items) = object else {
        return Err(AppError::Pdf("page box is not an array".into()));
    };
    if items.len() < 4 {
        return Err(AppError::Pdf("page box has fewer than 4 elements".into()));
    }
    let mut out = [0.0; 4];
    for (index, item) in items.iter().take(4).enumerate() {
        out[index] = match item {
            Object::Integer(i) => *i as f64,
            Object::Real(f) => *f as f64,
            _ => return Err(AppError::Pdf("page box element is not a number".into())),
        };
    }
    Ok(out)
}

/// Effective `/MediaBox`, honouring page-tree inheritance.
pub fn media_box(doc: &Document, page_id: ObjectId) -> AppResult<[f64; 4]> {
    let Some(object) = inherited(doc, page_id, b"MediaBox")? else {
        return Err(AppError::Pdf("page has no MediaBox".into()));
    };
    let (_, resolved) = doc.dereference(&object).map_err(pdf_err)?;
    box_numbers(resolved)
}

/// Sets `/MediaBox` and drops `/CropBox`, matching a fresh PDFBox page whose
/// crop box defaults to the media box.
pub fn set_page_box(doc: &mut Document, page_id: ObjectId, bbox: [f64; 4]) -> AppResult<()> {
    let page = doc.get_dictionary_mut(page_id).map_err(pdf_err)?;
    page.set(
        "MediaBox",
        Object::Array(bbox.iter().map(|v| Object::Real(*v as f32)).collect()),
    );
    page.remove(b"CropBox");
    Ok(())
}

pub fn clear_annots(doc: &mut Document, page_id: ObjectId) -> AppResult<()> {
    doc.get_dictionary_mut(page_id)
        .map_err(pdf_err)?
        .remove(b"Annots");
    Ok(())
}

fn set_page_content(doc: &mut Document, page_id: ObjectId, content: Vec<u8>) -> AppResult<()> {
    let id = doc.new_object_id();
    doc.objects
        .insert(id, Object::Stream(Stream::new(Dictionary::new(), content)));
    doc.get_dictionary_mut(page_id)
        .map_err(pdf_err)?
        .set("Contents", Object::Reference(id));
    Ok(())
}

pub(crate) fn pdf_num(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// Replaces a page's content with `prefix` + decoded existing content + `Q`,
/// isolating the transform/clip in a graphics-state pair.
pub fn wrap_page_content(doc: &mut Document, page_id: ObjectId, prefix: &str) -> AppResult<()> {
    let existing = doc
        .get_page_content_with_limit(page_id, 64 * 1024 * 1024)
        .map_err(pdf_err)?;
    let mut content = Vec::with_capacity(prefix.len() + existing.len() + 8);
    content.extend_from_slice(prefix.as_bytes());
    content.extend_from_slice(&existing);
    content.extend_from_slice(b"\nQ\n");
    set_page_content(doc, page_id, content)
}

pub fn scale_prefix(x: f64, y: f64, scale: f64) -> String {
    format!(
        "q\n1 0 0 1 {} {} cm\n{} 0 0 {} 0 0 cm\n",
        pdf_num(x),
        pdf_num(y),
        pdf_num(scale),
        pdf_num(scale)
    )
}

pub fn crop_prefix(x: f64, y: f64, width: f64, height: f64) -> String {
    format!(
        "q\n{} {} {} {} re\nW\nn\n",
        pdf_num(x),
        pdf_num(y),
        pdf_num(width),
        pdf_num(height)
    )
}

/// Clip to the new page box, then translate so the source section lands at
/// the origin — the same order `SplitPdfBySectionsController` uses.
pub fn section_prefix(width: f64, height: f64, tx: f64, ty: f64) -> String {
    format!(
        "q\n0 0 {} {} re\nW\nn\n1 0 0 1 {} {} cm\n",
        pdf_num(width),
        pdf_num(height),
        pdf_num(tx),
        pdf_num(ty)
    )
}

fn annot_rect(doc: &Document, object: &Object) -> Option<[f64; 4]> {
    let dict = peek_dict(doc, object)?;
    let rect = dict.get(b"Rect").ok()?;
    let (_, resolved) = doc.dereference(rect).ok()?;
    box_numbers(resolved).ok()
}

fn rect_object(rect: [f64; 4]) -> Object {
    Object::Array(rect.iter().map(|v| Object::Real(*v as f32)).collect())
}

fn transform_rect(rect: [f64; 4], matrix: [f64; 6]) -> [f64; 4] {
    let [a, b, c, d, e, f] = matrix;
    let mut xs = [f64::INFINITY; 4];
    let mut ys = [f64::INFINITY; 4];
    for (index, (x, y)) in [
        (rect[0], rect[1]),
        (rect[2], rect[1]),
        (rect[0], rect[3]),
        (rect[2], rect[3]),
    ]
    .into_iter()
    .enumerate()
    {
        xs[index] = a * x + c * y + e;
        ys[index] = b * x + d * y + f;
    }
    [
        xs.iter().copied().fold(f64::INFINITY, f64::min),
        ys.iter().copied().fold(f64::INFINITY, f64::min),
        xs.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        ys.iter().copied().fold(f64::NEG_INFINITY, f64::max),
    ]
}

fn rects_intersect(a: [f64; 4], b: [f64; 4]) -> bool {
    let (ax0, ax1) = if a[0] <= a[2] {
        (a[0], a[2])
    } else {
        (a[2], a[0])
    };
    let (ay0, ay1) = if a[1] <= a[3] {
        (a[1], a[3])
    } else {
        (a[3], a[1])
    };
    let (bx0, bx1) = if b[0] <= b[2] {
        (b[0], b[2])
    } else {
        (b[2], b[0])
    };
    let (by0, by1) = if b[1] <= b[3] {
        (b[1], b[3])
    } else {
        (b[3], b[1])
    };
    ax1 >= bx0 && ax0 <= bx1 && ay1 >= by0 && ay0 <= by1
}

fn kept_annotation(object: &Object, dict: &Dictionary, keep_stamp: bool) -> bool {
    let subtype = dict
        .get(b"Subtype")
        .ok()
        .and_then(|o| o.as_name().ok())
        .map(<[u8]>::to_vec);
    let kept = matches!(subtype.as_deref(), Some(b"Link") | Some(b"Widget"))
        || (keep_stamp && matches!(subtype.as_deref(), Some(b"Stamp")));
    kept && !matches!(object, Object::Null)
}

fn rebuild_annots(
    doc: &mut Document,
    page_id: ObjectId,
    matrix: Option<[f64; 6]>,
    clip: Option<[f64; 4]>,
    keep_stamp: bool,
) -> AppResult<()> {
    let annots = {
        let page = doc.get_dictionary(page_id).map_err(pdf_err)?;
        page.get(b"Annots").ok().cloned()
    };
    let Some(annots) = annots else {
        return Ok(());
    };
    let (_, resolved) = doc.dereference(&annots).map_err(pdf_err)?;
    let Object::Array(items) = resolved else {
        return Ok(());
    };
    let items = items.clone();

    let mut kept = Vec::new();
    let mut updates = Vec::new();
    for item in items {
        let Some(dict) = peek_dict(doc, &item) else {
            continue;
        };
        if !kept_annotation(&item, dict, keep_stamp) {
            continue;
        }
        let rect = annot_rect(doc, &item);
        if let (Some(clip), Some(rect)) = (clip, rect) {
            if !rects_intersect(rect, clip) {
                continue;
            }
        }
        let new_rect = match (matrix, rect) {
            (Some(matrix), Some(rect)) => Some(transform_rect(rect, matrix)),
            _ => rect,
        };
        match &item {
            Object::Reference(id) => {
                if matrix.is_some() {
                    if let Some(rect) = new_rect {
                        updates.push((*id, rect));
                    }
                }
                kept.push(item);
            }
            Object::Dictionary(_) => {
                let mut dict = dict.clone();
                if let Some(rect) = new_rect {
                    dict.set("Rect", rect_object(rect));
                }
                kept.push(Object::Dictionary(dict));
            }
            _ => {}
        }
    }

    for (id, rect) in updates {
        if let Some(object) = doc.objects.get_mut(&id) {
            if let Some(dict) = dict_mut(object) {
                dict.set("Rect", rect_object(rect));
            }
        }
    }

    let page = doc.get_dictionary_mut(page_id).map_err(pdf_err)?;
    if kept.is_empty() {
        page.remove(b"Annots");
    } else {
        page.set("Annots", Object::Array(kept));
    }
    Ok(())
}

/// Keeps link/widget annotations on a scaled page and moves their `/Rect`
/// by the same matrix applied to the page content. `Stamp` annotations
/// are kept too: `ScalePagesController` flattens their appearance streams
/// into the content for the same reason — the scaled `/Rect` makes the
/// viewer scale the appearance the same way.
pub fn scale_annots(doc: &mut Document, page_id: ObjectId, matrix: [f64; 6]) -> AppResult<()> {
    rebuild_annots(doc, page_id, Some(matrix), None, true)
}

/// Keeps link/widget annotations that intersect `crop` on a cropped page.
/// Annotation rectangles stay in page user space, matching the unchanged
/// content coordinates.
pub fn clip_annots(doc: &mut Document, page_id: ObjectId, crop: [f64; 4]) -> AppResult<()> {
    rebuild_annots(doc, page_id, None, Some(crop), false)
}

/// Resolves a named destination through the catalog's `/Dests` dictionary
/// or the `/Names` → `/Dests` name tree.
fn named_destination(
    doc: &Document,
    name: &[u8],
    pages: &HashMap<ObjectId, usize>,
) -> AppResult<Option<usize>> {
    let catalog = doc.catalog().map_err(pdf_err)?;

    // Old-style /Dests: a dictionary mapping names to destinations.
    if let Ok(dests) = catalog.get(b"Dests") {
        let (_, resolved) = doc.dereference(dests).map_err(pdf_err)?;
        if let Object::Dictionary(dict) = resolved {
            if let Ok(dest) = dict.get(name) {
                return dest_page_index(doc, dest, pages);
            }
        }
    }

    // Name tree under /Names → /Dests.
    if let Ok(names) = catalog.get(b"Names") {
        let (_, resolved) = doc.dereference(names).map_err(pdf_err)?;
        if let Object::Dictionary(dict) = resolved {
            if let Ok(dests) = dict.get(b"Dests") {
                return name_tree_lookup(doc, dests, name, pages, 0);
            }
        }
    }
    Ok(None)
}

fn name_tree_lookup(
    doc: &Document,
    node: &Object,
    name: &[u8],
    pages: &HashMap<ObjectId, usize>,
    depth: usize,
) -> AppResult<Option<usize>> {
    if depth > 100 {
        return Ok(None);
    }
    let (_, resolved) = doc.dereference(node).map_err(pdf_err)?;
    let Object::Dictionary(dict) = resolved else {
        return Ok(None);
    };
    if let Ok(pairs) = dict.get(b"Names") {
        let (_, pairs) = doc.dereference(pairs).map_err(pdf_err)?;
        if let Object::Array(items) = pairs {
            for pair in items.as_chunks::<2>().0 {
                if let Object::String(key, _) | Object::Name(key) = &pair[0] {
                    if key.as_slice() == name {
                        return dest_page_index(doc, &pair[1], pages);
                    }
                }
            }
        }
    }
    if let Ok(kids) = dict.get(b"Kids") {
        let (_, kids) = doc.dereference(kids).map_err(pdf_err)?;
        if let Object::Array(items) = kids {
            for kid in items {
                if let Some(found) = name_tree_lookup(doc, kid, name, pages, depth + 1)? {
                    return Ok(Some(found));
                }
            }
        }
    }
    Ok(None)
}

/// Resolved trailer `/Info` dictionary, when present.
pub fn info_dict(doc: &Document) -> Option<Dictionary> {
    let info = doc.trailer.get(b"Info").ok()?;
    let (_, object) = doc.dereference(info).ok()?;
    match object {
        Object::Dictionary(dict) => Some(dict.clone()),
        _ => None,
    }
}

/// Resolved trailer `/Encrypt` dictionary, when the file is encrypted.
pub fn encrypt_dict(doc: &Document) -> Option<Dictionary> {
    let encrypt = doc.trailer.get(b"Encrypt").ok()?;
    let (_, object) = doc.dereference(encrypt).ok()?;
    match object {
        Object::Dictionary(dict) => Some(dict.clone()),
        _ => None,
    }
}

/// Follows references and returns the dictionary behind an object — a
/// plain dictionary or a stream's dictionary. `None` for anything else.
pub fn dict_owned(doc: &Document, object: &Object) -> AppResult<Option<Dictionary>> {
    let (_, resolved) = doc.dereference(object).map_err(pdf_err)?;
    Ok(match resolved {
        Object::Dictionary(dict) => Some(dict.clone()),
        Object::Stream(stream) => Some(stream.dict.clone()),
        _ => None,
    })
}

/// The catalog's `/Names` dictionary (the root of the document's name
/// trees), resolved.
pub fn names_dict(doc: &Document) -> Option<Dictionary> {
    let catalog = doc.catalog().ok()?;
    let names = catalog.get(b"Names").ok()?;
    dict_owned(doc, names).ok().flatten()
}

/// Every entry of a PDF name tree (`/Names` key/value pairs plus `/Kids`
/// recursion), as `(key, resolved value)` pairs in tree order. Keys are
/// decoded text strings; values are dereferenced one level.
pub fn name_tree_entries(doc: &Document, node: &Object) -> AppResult<Vec<(String, Object)>> {
    let mut entries = Vec::new();
    name_tree_walk(doc, node, &mut entries, 0)?;
    Ok(entries)
}

fn name_tree_walk(
    doc: &Document,
    node: &Object,
    entries: &mut Vec<(String, Object)>,
    depth: usize,
) -> AppResult<()> {
    if depth > 100 {
        return Ok(());
    }
    let Some(dict) = dict_owned(doc, node)? else {
        return Ok(());
    };
    if let Ok(names) = dict.get(b"Names") {
        let (_, resolved) = doc.dereference(names).map_err(pdf_err)?;
        if let Object::Array(items) = resolved {
            for pair in items.as_chunks::<2>().0 {
                let key = match decode_text_string(&pair[0]) {
                    Ok(key) => key,
                    Err(_) => continue,
                };
                let (_, value) = doc.dereference(&pair[1]).map_err(pdf_err)?;
                entries.push((key, value.clone()));
            }
        }
    }
    if let Ok(kids) = dict.get(b"Kids") {
        let (_, resolved) = doc.dereference(kids).map_err(pdf_err)?;
        if let Object::Array(items) = resolved {
            for kid in items {
                name_tree_walk(doc, kid, entries, depth + 1)?;
            }
        }
    }
    Ok(())
}

/// The page's effective `/Resources` dictionary: the nearest one up the
/// page tree, matching PDFBox's inheritable-attribute lookup (no merging
/// of ancestor dictionaries).
pub fn page_resources_dict(doc: &Document, page_id: ObjectId) -> AppResult<Option<Dictionary>> {
    let (inline, ids) = doc.get_page_resources(page_id).map_err(pdf_err)?;
    if let Some(dict) = inline {
        return Ok(Some(dict.clone()));
    }
    for id in ids {
        if let Ok(dict) = doc.get_dictionary(id) {
            return Ok(Some(dict.clone()));
        }
    }
    Ok(None)
}

/// Entries of a `/Resources` subsection (`Font`, `XObject`, …): resource
/// name → resolved dictionary (stream dictionaries included).
pub fn resource_entries(
    doc: &Document,
    page_id: ObjectId,
    section: &[u8],
) -> AppResult<Vec<(String, Dictionary)>> {
    let mut entries = Vec::new();
    let Some(resources) = page_resources_dict(doc, page_id)? else {
        return Ok(entries);
    };
    let Some(section_dict) = resources
        .get(section)
        .ok()
        .and_then(|value| dict_owned(doc, value).ok().flatten())
    else {
        return Ok(entries);
    };
    for (name, value) in section_dict.iter() {
        if let Some(dict) = dict_owned(doc, value)? {
            entries.push((String::from_utf8_lossy(name).into_owned(), dict));
        }
    }
    Ok(entries)
}

/// The five page boxes with PDFBox's fallback rules: MediaBox defaults to
/// US Letter, CropBox falls back to MediaBox, Bleed/Trim/Art fall back to
/// CropBox, and every box except MediaBox is clipped to MediaBox. Each is
/// `[llx, lly, urx, ury]` in default user space units.
pub struct PageBoxes {
    pub media: [f32; 4],
    pub crop: [f32; 4],
    pub bleed: [f32; 4],
    pub trim: [f32; 4],
    pub art: [f32; 4],
}

pub fn page_boxes(doc: &Document, page_id: ObjectId) -> AppResult<PageBoxes> {
    let media = match inherited(doc, page_id, b"MediaBox")? {
        Some(object) => box_array(doc, &object)?.unwrap_or([0.0, 0.0, 612.0, 792.0]),
        None => [0.0, 0.0, 612.0, 792.0],
    };
    let crop = match inherited(doc, page_id, b"CropBox")? {
        Some(object) => match box_array(doc, &object)? {
            Some(b) => clip_to_media(media, b),
            None => media,
        },
        None => media,
    };
    let fallback_box = |key: &[u8]| -> AppResult<[f32; 4]> {
        Ok(match inherited(doc, page_id, key)? {
            Some(object) => match box_array(doc, &object)? {
                Some(b) => clip_to_media(media, b),
                None => crop,
            },
            None => crop,
        })
    };
    Ok(PageBoxes {
        media,
        crop,
        bleed: fallback_box(b"BleedBox")?,
        trim: fallback_box(b"TrimBox")?,
        art: fallback_box(b"ArtBox")?,
    })
}

/// Reads a `[x0 y0 x1 y1]` box array into f32s, like `PDRectangle`'s
/// float fields. `None` when the object isn't a 4+-element number array.
fn box_array(doc: &Document, object: &Object) -> AppResult<Option<[f32; 4]>> {
    let (_, resolved) = doc.dereference(object).map_err(pdf_err)?;
    let Object::Array(items) = resolved else {
        return Ok(None);
    };
    if items.len() < 4 {
        return Ok(None);
    }
    let mut out = [0.0f32; 4];
    for (i, item) in items.iter().take(4).enumerate() {
        out[i] = match item {
            Object::Integer(n) => *n as f32,
            Object::Real(f) => *f,
            _ => return Ok(None),
        };
    }
    Ok(Some(out))
}

/// `PDPage.clipToMediaBox`: intersection with the media box, no guarding
/// against inverted results.
fn clip_to_media(media: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    [
        media[0].max(b[0]),
        media[1].max(b[1]),
        media[2].min(b[2]),
        media[3].min(b[3]),
    ]
}

/// `/Rotate` normalised like `PDPage.getRotation`: only multiples of 90
/// survive; anything else reports 0.
pub fn page_rotation(doc: &Document, page_id: ObjectId) -> AppResult<i64> {
    let degrees = match inherited(doc, page_id, b"Rotate")? {
        Some(Object::Integer(i)) => i,
        Some(Object::Real(f)) => f as i64,
        _ => return Ok(0),
    };
    Ok(if degrees % 90 == 0 {
        degrees.rem_euclid(360)
    } else {
        0
    })
}

/// Flattened outline (`/Outlines`) titles in depth-first order — the shape
/// `GetInfoOnPDF` emits under `Other.Bookmarks/Outline/TOC`. Deduped by
/// object id and depth-capped at 64 like the Java walker; titles are raw
/// (`/` preserved, unlike [`outline`]). `None` entries mark items with no
/// title.
pub fn outline_titles(doc: &Document) -> AppResult<Vec<Option<String>>> {
    let catalog = doc.catalog().map_err(pdf_err)?;
    let Some(outlines) = catalog.get(b"Outlines").ok() else {
        return Ok(Vec::new());
    };
    let Some(root) = dict_owned(doc, outlines)? else {
        return Ok(Vec::new());
    };
    let mut titles = Vec::new();
    let mut visited = HashSet::new();
    if let Ok(first) = root.get(b"First") {
        outline_titles_walk(doc, first, &mut visited, 0, &mut titles)?;
    }
    Ok(titles)
}

fn outline_titles_walk(
    doc: &Document,
    item: &Object,
    visited: &mut HashSet<ObjectId>,
    depth: usize,
    out: &mut Vec<Option<String>>,
) -> AppResult<()> {
    if depth > 64 {
        return Ok(());
    }
    // Follow /Next siblings at this level, recursing into /FirstChild.
    // A revisited item is skipped like Java's `visited.add` check, but
    // sibling iteration continues.
    let mut current = Some(item.clone());
    while let Some(object) = current {
        let Some(dict) = dict_owned(doc, &object)? else {
            break;
        };
        let visited_now = object
            .as_reference()
            .ok()
            .is_some_and(|id| !visited.insert(id));
        if !visited_now {
            let title = dict
                .get(b"Title")
                .ok()
                .and_then(|t| decode_text_string(t).ok());
            out.push(title);
            if let Ok(first_child) = dict.get(b"FirstChild") {
                outline_titles_walk(doc, first_child, visited, depth + 1, out)?;
            }
        }
        current = dict.get(b"Next").ok().cloned();
    }
    Ok(())
}

/// `(fully-qualified name, value)` pairs from the AcroForm field tree,
/// mirroring `PDAcroForm.getFieldTree()` with `getFullyQualifiedName()` and
/// `getValueAsString()`. Non-terminal fields appear with a `None` value.
/// Kids that are pure widget annotations (no `/T`, `/FT`, or `/Kids`) are
/// not fields and are skipped.
pub fn form_field_entries(doc: &Document) -> AppResult<Vec<(String, Option<String>)>> {
    let mut entries = Vec::new();
    let Some(form) = acroform(doc) else {
        return Ok(entries);
    };
    let Ok(fields) = form.get(b"Fields") else {
        return Ok(entries);
    };
    let mut visited = HashSet::new();
    form_field_walk(doc, fields, "", &mut visited, &mut entries)?;
    Ok(entries)
}

fn form_field_walk(
    doc: &Document,
    object: &Object,
    prefix: &str,
    visited: &mut HashSet<ObjectId>,
    out: &mut Vec<(String, Option<String>)>,
) -> AppResult<()> {
    let Some(dict) = dict_owned(doc, object)? else {
        return Ok(());
    };
    let is_widget_only = dict.get(b"Subtype").ok().and_then(|s| s.as_name().ok())
        == Some(b"Widget")
        && !dict.has(b"T")
        && !dict.has(b"FT")
        && !dict.has(b"Kids");
    if is_widget_only {
        return Ok(());
    }
    if let Ok(id) = object.as_reference() {
        if !visited.insert(id) {
            return Ok(());
        }
    }

    let partial = dict
        .get(b"T")
        .ok()
        .and_then(|t| decode_text_string(t).ok())
        .unwrap_or_default();
    let name = if partial.is_empty() {
        prefix.to_string()
    } else if prefix.is_empty() {
        partial
    } else {
        format!("{prefix}.{partial}")
    };

    let value = dict.get(b"V").ok().and_then(|v| field_value(doc, v));
    out.push((name.clone(), value));

    if let Ok(kids) = dict.get(b"Kids") {
        if let Some(Object::Array(items)) = doc.dereference(kids).ok().map(|(_, o)| o.clone()) {
            for kid in &items {
                form_field_walk(doc, kid, &name, visited, out)?;
            }
        }
    }
    Ok(())
}

/// `/V` of a form field as a display string: text strings decode, name
/// values (e.g. `/Yes` for checkboxes) become their name, and arrays
/// (multi-select choices) join their decoded items.
fn field_value(doc: &Document, object: &Object) -> Option<String> {
    let (_, resolved) = doc.dereference(object).ok()?;
    match resolved {
        Object::String(..) => decode_text_string(resolved).ok(),
        Object::Name(name) => Some(String::from_utf8_lossy(name).into_owned()),
        Object::Array(items) => {
            let mut parts = Vec::new();
            for item in items {
                let (_, item) = doc.dereference(item).ok()?;
                match item {
                    Object::String(..) => parts.push(decode_text_string(item).ok()?),
                    Object::Name(name) => parts.push(String::from_utf8_lossy(name).into_owned()),
                    _ => return None,
                }
            }
            Some(parts.join(", "))
        }
        _ => None,
    }
}

/// Port of `FormUtils.pruneOrphanedFormFields`: drops AcroForm fields whose
/// widgets no longer appear on any page (e.g. after pages were removed or
/// split away), pruning emptied non-terminal subtrees, and removes
/// `/AcroForm` entirely when no fields remain. Called by the Java
/// remove-pages, split-pages, split-by-size-or-count and
/// split-pdf-by-chapters endpoints after their page selection.
pub fn prune_orphaned_form_fields(doc: &mut Document) -> AppResult<()> {
    let catalog = doc.catalog().map_err(pdf_err)?;
    let Some(acro_slot) = catalog.get(b"AcroForm").ok().cloned() else {
        return Ok(());
    };
    let Some(acro_dict) = dict_owned(doc, &acro_slot)? else {
        return Ok(());
    };
    let Ok(fields_obj) = acro_dict.get(b"Fields") else {
        return Ok(());
    };
    let fields = match doc.dereference(fields_obj).map_err(pdf_err)? {
        (_, Object::Array(items)) => items.clone(),
        _ => return Ok(()),
    };
    if fields.is_empty() {
        return Ok(());
    }

    let live = live_widget_ids(doc);
    let mut visited = HashSet::new();
    let mut kept = Vec::with_capacity(fields.len());
    for field in fields {
        if let Some(node) = prune_field_node(doc, field, &live, &mut visited)? {
            kept.push(node);
        }
    }

    if kept.is_empty() {
        if let Ok(catalog) = doc.catalog_mut() {
            catalog.remove(b"AcroForm");
        }
        return Ok(());
    }

    let mut acro_dict = acro_dict;
    acro_dict.set("Fields", Object::Array(kept));
    match acro_slot {
        Object::Reference(id) => {
            doc.objects.insert(id, Object::Dictionary(acro_dict));
        }
        _ => {
            if let Ok(catalog) = doc.catalog_mut() {
                catalog.set("AcroForm", Object::Dictionary(acro_dict));
            }
        }
    }
    Ok(())
}

/// Object ids of widget annotations still reachable from a page's
/// `/Annots` — the live set field widgets are checked against. Direct
/// (non-indirect) annots have no stable identity and are skipped; a field
/// can only reference its widgets indirectly anyway.
fn live_widget_ids(doc: &Document) -> HashSet<ObjectId> {
    let mut live = HashSet::new();
    for page_id in page_ids(doc) {
        let Ok(page) = doc.get_dictionary(page_id) else {
            continue;
        };
        let Ok(annots) = page.get(b"Annots") else {
            continue;
        };
        let Ok((_, resolved)) = doc.dereference(annots) else {
            continue;
        };
        let Object::Array(items) = resolved else {
            continue;
        };
        for item in items {
            let Ok(id) = item.as_reference() else {
                continue;
            };
            let is_widget = doc
                .get_object(id)
                .ok()
                .and_then(dict_of)
                .and_then(|d| d.get(b"Subtype").ok())
                .and_then(|s| s.as_name().ok())
                == Some(b"Widget");
            if is_widget {
                live.insert(id);
            }
        }
    }
    live
}

/// Whether a `/Kids` entry is a bare widget annotation rather than a field
/// node: `/Subtype /Widget` with no `/FT` or `/Kids` of its own.
fn is_pure_widget(doc: &Document, kid: &Object) -> bool {
    let resolved = match kid {
        Object::Reference(id) => doc.get_object(*id).ok(),
        other => Some(other),
    };
    resolved.and_then(dict_of).is_some_and(|d| {
        d.get(b"Subtype").ok().and_then(|s| s.as_name().ok()) == Some(b"Widget")
            && !d.has(b"FT")
            && !d.has(b"Kids")
    })
}

/// Recursive worker for [`prune_orphaned_form_fields`]. Returns the object
/// to keep in the parent's `/Kids` or `/Fields` — the original reference
/// for indirect fields (their dictionary was updated in place), a revised
/// dictionary for direct ones — or `None` to drop the node.
fn prune_field_node(
    doc: &mut Document,
    object: Object,
    live: &HashSet<ObjectId>,
    visited: &mut HashSet<ObjectId>,
) -> AppResult<Option<Object>> {
    let (id, dict) = match &object {
        Object::Reference(id) => {
            // A repeated or cyclic field reference: Java drops the copy.
            if !visited.insert(*id) {
                return Ok(None);
            }
            match doc.get_object(*id).map_err(pdf_err)?.clone() {
                Object::Dictionary(d) => (Some(*id), d),
                _ => return Ok(Some(object)),
            }
        }
        Object::Dictionary(d) => (None, d.clone()),
        _ => return Ok(Some(object)),
    };

    let kids: Vec<Object> = match dict.get(b"Kids") {
        Ok(kids_obj) => match doc.dereference(kids_obj).map_err(pdf_err)? {
            (_, Object::Array(items)) => items.clone(),
            _ => Vec::new(),
        },
        Err(_) => Vec::new(),
    };

    let had_kids = dict.has(b"Kids");

    // A node is non-terminal when it has field children; a `/Kids` array of
    // only widget annotations makes it a terminal field instead.
    let non_terminal = kids.iter().any(|kid| !is_pure_widget(doc, kid));

    // `None` leaves the node's `/Kids` untouched (merged field/widget with
    // no kids of its own).
    let new_kids = if non_terminal {
        let mut kept_kids = Vec::with_capacity(kids.len());
        for kid in kids {
            if let Some(node) = prune_field_node(doc, kid, live, visited)? {
                kept_kids.push(node);
            }
        }
        if kept_kids.is_empty() {
            return Ok(None);
        }
        Some(kept_kids)
    } else {
        // Terminal field: keep it only while at least one of its widgets is
        // still on a page — itself when it is a merged field/widget, plus
        // the `/Kids` widget annotations.
        let self_is_live_widget = id.is_some_and(|id| {
            live.contains(&id)
                && dict.get(b"Subtype").ok().and_then(|s| s.as_name().ok()) == Some(b"Widget")
        });
        let mut live_kids = Vec::new();
        for kid in &kids {
            let kid_live = match kid {
                Object::Reference(kid_id) => live.contains(kid_id),
                _ => false,
            };
            if kid_live {
                live_kids.push(kid.clone());
            }
        }
        if !self_is_live_widget && live_kids.is_empty() {
            return Ok(None);
        }
        had_kids.then_some(live_kids)
    };

    Ok(Some(write_back_field(doc, id, dict, new_kids)))
}

/// Writes the pruned `/Kids` list back onto a field node: indirect fields
/// are updated in the object table, direct ones return a revised
/// dictionary for the parent to embed. `None` leaves `/Kids` alone.
fn write_back_field(
    doc: &mut Document,
    id: Option<ObjectId>,
    mut dict: Dictionary,
    new_kids: Option<Vec<Object>>,
) -> Object {
    if let Some(kids) = new_kids {
        dict.set("Kids", Object::Array(kids));
    }
    match id {
        Some(id) => {
            if let Some(object) = doc.objects.get_mut(&id) {
                if let Some(slot) = dict_mut(object) {
                    *slot = dict;
                }
            }
            Object::Reference(id)
        }
        None => Object::Dictionary(dict),
    }
}

/// PDF date string (`D:YYYYMMDDHHmmSSOHH'mm'`) → `yyyy-MM-dd HH:mm:ss`
/// in the system timezone, like `GetInfoOnPDF.formatDate`. A missing
/// timezone is read as UTC, matching PDFBox's `DateConverter`. Returns
/// `None` for absent or unparseable values.
/// `(naive date-time, tz offset minutes)` of a PDF `D:` date string.
fn parse_pdf_date(text: &str) -> Option<(chrono::NaiveDateTime, i64)> {
    let text = text.trim();
    let bytes = text.as_bytes();
    let start = bytes.iter().position(|b| b.is_ascii_digit())?;
    let mut end = start;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    let digits = &text[start..end];
    if digits.len() < 4 {
        return None;
    }
    let num = |range: std::ops::Range<usize>, default: u32| -> u32 {
        digits
            .get(range)
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(default)
    };
    let (year, month, day) = (num(0..4, 0), num(4..6, 1), num(6..8, 1));
    let (hour, minute, second) = (num(8..10, 0), num(10..12, 0), num(12..14, 0));
    let naive = chrono::NaiveDate::from_ymd_opt(year as i32, month, day)?
        .and_hms_opt(hour, minute, second)?;

    // Timezone: the text after the digit run. 'Z'/'GMT'/'UTC' or a signed
    // HH['mm'] offset; anything else means no zone → UTC like PDFBox.
    Some((naive, parse_tz_offset(text[end..].trim_start())))
}

pub fn pdf_date_local(text: &str) -> Option<String> {
    use chrono::{Local, TimeZone, Utc};

    let (naive, offset_minutes) = parse_pdf_date(text)?;
    let utc = Utc.from_utc_datetime(&naive) - chrono::Duration::minutes(offset_minutes);
    Some(
        utc.with_timezone(&Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string(),
    )
}

/// Rewrites a PDF `D:` date in the local zone, like PDFBox's
/// `Calendar` round-trip in `PdfMetadataService`. `None` for absent or
/// unparseable values.
fn pdf_date_rezone(text: &str) -> Option<String> {
    use chrono::{Datelike, Local, TimeZone, Timelike, Utc};

    let (naive, offset_minutes) = parse_pdf_date(text)?;
    let utc = Utc.from_utc_datetime(&naive) - chrono::Duration::minutes(offset_minutes);
    let local = utc.with_timezone(&Local);
    // local_minus_utc is seconds; PDF dates want HH'mm'.
    let offset = local.offset().local_minus_utc() / 60;
    let (sign, abs) = if offset < 0 {
        ('-', -offset)
    } else {
        ('+', offset)
    };
    Some(format!(
        "D:{:04}{:02}{:02}{:02}{:02}{:02}{}{:02}'{:02}'",
        local.year(),
        local.month(),
        local.day(),
        local.hour(),
        local.minute(),
        local.second(),
        sign,
        abs / 60,
        abs % 60
    ))
}

/// Signed `+HH'mm'`/`-HH'mm'` offset in minutes; 0 for 'Z', named zones,
/// or absent zone info (PDFBox treats zoneless dates as UTC).
fn parse_tz_offset(rest: &str) -> i64 {
    let rest = rest.trim_start();
    let bytes = rest.as_bytes();
    if bytes.first() == Some(&b'Z') || bytes.first() == Some(&b'z') {
        return 0;
    }
    if rest.len() >= 3
        && (rest[..3].eq_ignore_ascii_case("gmt") || rest[..3].eq_ignore_ascii_case("utc"))
    {
        return 0;
    }
    let (sign, rest) = match bytes.first() {
        Some(b'+') => (1i64, &rest[1..]),
        Some(b'-') => (-1i64, &rest[1..]),
        _ => return 0,
    };
    let digits: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '\'')
        .filter(|c| c.is_ascii_digit())
        .collect();
    let (hours, minutes) = match digits.len() {
        0 => return 0,
        1 => (digits.parse::<i64>().unwrap_or(0), 0),
        n => (
            digits[..2].parse::<i64>().unwrap_or(0),
            if n >= 4 {
                digits[2..4].parse::<i64>().unwrap_or(0)
            } else {
                0
            },
        ),
    };
    sign * (hours * 60 + minutes)
}

/// `(name, code)` pairs from the catalog `/Names` → `/JavaScript` name
/// tree, like PDFBox's `PDDocumentNameDictionary.getJavaScript()`.
pub fn javascript_entries(doc: &Document) -> AppResult<Vec<(String, String)>> {
    let catalog = doc.catalog().map_err(pdf_err)?;
    let Some(names) = catalog.get(b"Names").ok() else {
        return Ok(Vec::new());
    };
    let (_, names) = doc.dereference(names).map_err(pdf_err)?;
    let Object::Dictionary(names_dict) = names else {
        return Ok(Vec::new());
    };
    let Some(js_tree) = names_dict.get(b"JavaScript").ok() else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    js_tree_walk(doc, js_tree, &mut out)?;
    Ok(out)
}

fn js_tree_walk(doc: &Document, node: &Object, out: &mut Vec<(String, String)>) -> AppResult<()> {
    let (_, resolved) = doc.dereference(node).map_err(pdf_err)?;
    let Object::Dictionary(dict) = resolved else {
        return Ok(());
    };
    if let Ok(items) = dict.get(b"Names") {
        let (_, items) = doc.dereference(items).map_err(pdf_err)?;
        if let Object::Array(kv) = items {
            for pair in kv.as_chunks::<2>().0 {
                let name = match &pair[0] {
                    Object::String(s, _) => decode_text_string(&pair[0])
                        .unwrap_or_else(|_| String::from_utf8_lossy(s).into_owned()),
                    Object::Name(n) => String::from_utf8_lossy(n).into_owned(),
                    _ => continue,
                };
                let (_, action) = doc.dereference(&pair[1]).map_err(pdf_err)?;
                if let Object::Dictionary(action_dict) = action {
                    if let Ok(js) = action_dict.get(b"JS") {
                        let (_, js_obj) = doc.dereference(js).map_err(pdf_err)?;
                        let code = decode_text_string(js_obj).unwrap_or_else(|_| {
                            String::from_utf8_lossy(js_obj.as_str().unwrap_or_default())
                                .into_owned()
                        });
                        out.push((name, code));
                    }
                }
            }
        }
    }
    if let Ok(kids) = dict.get(b"Kids") {
        let (_, kids) = doc.dereference(kids).map_err(pdf_err)?;
        if let Object::Array(items) = kids {
            for kid in items {
                js_tree_walk(doc, kid, out)?;
            }
        }
    }
    Ok(())
}

/// Dictionary view of an object that may be a plain dictionary or a stream
/// (form XObjects are streams); `None` for anything else.
pub fn dict_of(object: &Object) -> Option<&Dictionary> {
    match object {
        Object::Dictionary(dict) => Some(dict),
        Object::Stream(stream) => Some(&stream.dict),
        _ => None,
    }
}

pub fn dict_mut(object: &mut Object) -> Option<&mut Dictionary> {
    match object {
        Object::Dictionary(dict) => Some(dict),
        Object::Stream(stream) => Some(&mut stream.dict),
        _ => None,
    }
}

/// Borrows the dictionary behind `object`, following one reference level.
/// Used for reads so stream payloads are never cloned just to inspect a
/// dictionary entry.
pub fn peek_dict<'a>(doc: &'a Document, object: &'a Object) -> Option<&'a Dictionary> {
    let resolved = match object {
        Object::Reference(id) => doc.get_object(*id).ok()?,
        other => other,
    };
    dict_of(resolved)
}

/// Owned copy of `object` with one reference level resolved, for mutation.
pub fn resolve_owned(doc: &Document, object: &Object) -> Option<Object> {
    match object {
        Object::Reference(id) => doc.get_object(*id).ok().cloned(),
        other => Some(other.clone()),
    }
}

/// Writes `new_object` where `slot` pointed: indirect objects are replaced
/// in the object table (preserving sharing), direct ones inline in `parent`.
pub fn store_object(
    doc: &mut Document,
    parent: &mut Dictionary,
    key: &[u8],
    slot: &Object,
    new_object: Object,
) {
    match slot {
        Object::Reference(id) => {
            doc.objects.insert(*id, new_object);
        }
        _ => parent.set(key, new_object),
    }
}

/// Location of a dictionary inside the object graph: the dictionary of
/// object `root` (plain or stream), then direct-`Dictionary` members named
/// by `keys`. References are never path members — a referenced dictionary
/// is itself a root.
#[derive(Clone)]
pub struct DictPath {
    pub root: ObjectId,
    pub keys: Vec<Vec<u8>>,
}

/// Reads the dictionary at `path` as an owned clone.
pub fn dict_at(doc: &Document, path: &DictPath) -> Option<Dictionary> {
    let mut dict = dict_of(doc.get_object(path.root).ok()?)?.clone();
    for key in &path.keys {
        dict = match dict.get(key).ok()? {
            Object::Dictionary(inner) => inner.clone(),
            _ => return None,
        };
    }
    Some(dict)
}

/// Applies `f` to the dictionary at `path`, cloning each direct-dictionary
/// level on the way down. Missing levels are a no-op.
pub fn update_at(
    doc: &mut Document,
    path: &DictPath,
    mut f: impl FnMut(&mut Dictionary),
) -> AppResult<()> {
    let object = doc
        .objects
        .get_mut(&path.root)
        .ok_or_else(|| AppError::Pdf(format!("missing object {:?}", path.root)))?;
    let dict = dict_mut(object)
        .ok_or_else(|| AppError::Pdf(format!("object {:?} is not a dictionary", path.root)))?;
    update_nested(dict, &path.keys, &mut f);
    Ok(())
}

fn update_nested(dict: &mut Dictionary, keys: &[Vec<u8>], f: &mut dyn FnMut(&mut Dictionary)) {
    match keys.split_first() {
        None => f(dict),
        Some((key, rest)) => {
            let mut child = match dict.get(key) {
                Ok(Object::Dictionary(inner)) => inner.clone(),
                _ => return,
            };
            update_nested(&mut child, rest, f);
            dict.set(key.clone(), Object::Dictionary(child));
        }
    }
}

/// The page's effective `/Resources` value plus the object id of the
/// page-tree node that defines it. Resources inherit down the page tree,
/// so the holder may be an ancestor `/Pages` node rather than the page.
pub fn page_resources_holder(doc: &Document, page_id: ObjectId) -> Option<(ObjectId, Object)> {
    let mut current = page_id;
    let mut visited = HashSet::new();
    loop {
        if !visited.insert(current) {
            return None;
        }
        let dict = dict_of(doc.get_object(current).ok()?)?;
        if let Ok(resources) = dict.get(b"Resources") {
            if !matches!(resources, Object::Null) {
                return Some((current, resources.clone()));
            }
        }
        match dict.get(b"Parent").ok()? {
            Object::Reference(id) => current = *id,
            _ => return None,
        }
    }
}

// ---------------------------------------------------------------------------
// pdf-to-single-page
// ---------------------------------------------------------------------------

/// `java.awt.geom.AffineTransform` with its concatenate semantics: every
/// method right-multiplies, so the transform concatenated last applies to
/// points first — the ordering `LayerUtility.importPageAsForm` relies on.
#[derive(Clone, Copy)]
pub(crate) struct Affine {
    m00: f64,
    m01: f64,
    m02: f64,
    m10: f64,
    m11: f64,
    m12: f64,
}

impl Affine {
    pub(crate) fn identity() -> Self {
        Self {
            m00: 1.0,
            m01: 0.0,
            m02: 0.0,
            m10: 0.0,
            m11: 1.0,
            m12: 0.0,
        }
    }

    pub(crate) fn translate(&mut self, tx: f64, ty: f64) {
        self.m02 += self.m00 * tx + self.m01 * ty;
        self.m12 += self.m10 * tx + self.m11 * ty;
    }

    pub(crate) fn scale(&mut self, sx: f64, sy: f64) {
        self.m00 *= sx;
        self.m01 *= sy;
        self.m10 *= sx;
        self.m11 *= sy;
    }

    /// `quadrantRotate`: steps of 90°, matching OpenJDK's state matrices.
    fn quadrant_rotate(&mut self, quadrants: i64) {
        let (r00, r01, r10, r11) = match quadrants.rem_euclid(4) {
            1 => (0.0, -1.0, 1.0, 0.0),
            2 => (-1.0, 0.0, 0.0, -1.0),
            3 => (0.0, 1.0, -1.0, 0.0),
            _ => return,
        };
        let (m00, m01, m10, m11) = (self.m00, self.m01, self.m10, self.m11);
        self.m00 = m00 * r00 + m01 * r10;
        self.m01 = m00 * r01 + m01 * r11;
        self.m10 = m10 * r00 + m11 * r10;
        self.m11 = m10 * r01 + m11 * r11;
    }

    fn is_identity(&self) -> bool {
        self.m00 == 1.0
            && self.m01 == 0.0
            && self.m02 == 0.0
            && self.m10 == 0.0
            && self.m11 == 1.0
            && self.m12 == 0.0
    }

    /// Right-multiplies by `rhs`: points see `rhs` first, then `self`.
    pub(crate) fn concat(&mut self, rhs: &Affine) {
        let (m00, m01, m10, m11) = (self.m00, self.m01, self.m10, self.m11);
        self.m00 = m00 * rhs.m00 + m01 * rhs.m10;
        self.m01 = m00 * rhs.m01 + m01 * rhs.m11;
        self.m10 = m10 * rhs.m00 + m11 * rhs.m10;
        self.m11 = m10 * rhs.m01 + m11 * rhs.m11;
        self.m02 += m00 * rhs.m02 + m01 * rhs.m12;
        self.m12 += m10 * rhs.m02 + m11 * rhs.m12;
    }

    /// PDF `cm` operands `[a b c d e f]` for the same point mapping.
    pub(crate) fn pdf_cm(&self) -> [f64; 6] {
        [self.m00, self.m10, self.m01, self.m11, self.m02, self.m12]
    }

    /// PDF `/Matrix` `[a b c d e f]` for the same point mapping.
    fn pdf_array(&self) -> Object {
        Object::Array(
            [self.m00, self.m10, self.m01, self.m11, self.m02, self.m12]
                .iter()
                .map(|v| Object::Real(*v as f32))
                .collect(),
        )
    }
}

/// The `/Matrix` `LayerUtility.importPageAsForm` computes: maps source user
/// space into the form's displayed space — origin at the view box's
/// lower-left, y up, page rotation applied.
pub(crate) fn form_matrix(media: [f64; 4], view: [f64; 4], rotation: i64) -> Affine {
    let mut at = Affine::identity();
    at.translate(media[0] - view[0], media[1] - view[1]);
    let vw = view[2] - view[0];
    let vh = view[3] - view[1];
    match rotation {
        90 => {
            at.scale(vw / vh, vh / vw);
            at.translate(0.0, vw);
            at.quadrant_rotate(3);
        }
        180 => {
            at.translate(vw, vh);
            at.quadrant_rotate(2);
        }
        270 => {
            at.scale(vw / vh, vh / vw);
            at.translate(vh, 0.0);
            at.quadrant_rotate(1);
        }
        _ => {}
    }
    at.translate(-view[0], -view[1]);
    at
}

/// Copies `object` into `dst`, remapping references — `import_object` for
/// indirect objects, `remap` for direct dictionaries, arrays and streams.
fn copy_value(
    dst: &mut Document,
    src: &Document,
    object: &Object,
    map: &mut HashMap<ObjectId, ObjectId>,
) -> AppResult<Object> {
    match object {
        Object::Reference(id) => Ok(Object::Reference(import_object(dst, src, *id, map)?)),
        other => remap(dst, src, other, map),
    }
}

/// The label `PdfMetadataService` stamps as Producer/Creator:
/// `Stirling-PDF v<app version>`. `STIRLING_VERSION` overrides the crate
/// version, which is not the application version.
fn app_version_label() -> String {
    let version =
        std::env::var("STIRLING_VERSION").unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string());
    format!("Stirling-PDF v{version}")
}

/// Current time as a PDF `D:` date string in the local zone, like
/// `Calendar.getInstance()` written by PDFBox.
fn pdf_date_now() -> String {
    use chrono::{Datelike, Local, Timelike};

    let now = Local::now();
    // local_minus_utc is seconds; PDF dates want HH'mm'.
    let offset = now.offset().local_minus_utc() / 60;
    let (sign, abs) = if offset < 0 {
        ('-', -offset)
    } else {
        ('+', offset)
    };
    format!(
        "D:{:04}{:02}{:02}{:02}{:02}{:02}{}{:02}'{:02}'",
        now.year(),
        now.month(),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        sign,
        abs / 60,
        abs % 60
    )
}

fn stamp_producer_creator(info: &mut Dictionary) {
    let label = app_version_label();
    info.set("Producer", Object::string_literal(label.as_str()));
    info.set("Creator", Object::string_literal(label));
}

/// The `/Info` dictionary `createNewDocumentBasedOnOldDocument` produces:
/// only the standard text fields survive (custom keys are dropped with the
/// old `PDDocumentInformation`), dates re-zone to the system zone like
/// Java's `Calendar` round-trip, missing dates become now, and
/// Producer/Creator get the app label.
fn info_based_on(src_info: Option<Dictionary>) -> Dictionary {
    let mut info = Dictionary::new();
    for key in [
        &b"Title"[..],
        &b"Author"[..],
        &b"Subject"[..],
        &b"Keywords"[..],
    ] {
        if let Some(value) = src_info.as_ref().and_then(|d| d.get(key).ok()) {
            info.set(key, value.clone());
        }
    }
    for key in [&b"CreationDate"[..], &b"ModDate"[..]] {
        if let Some(text) = src_info
            .as_ref()
            .and_then(|d| d.get(key).ok())
            .and_then(|v| decode_text_string(v).ok())
            .and_then(|t| pdf_date_rezone(&t))
        {
            info.set(key, Object::string_literal(text));
        }
    }
    stamp_producer_creator(&mut info);
    let now = pdf_date_now();
    if !info.has(b"CreationDate") {
        info.set("CreationDate", Object::string_literal(now.as_str()));
    }
    if !info.has(b"ModDate") {
        info.set("ModDate", Object::string_literal(now.as_str()));
    }
    info
}

/// `createNewDocumentBasedOnOldDocument`'s `/Info` carry-over onto a fresh
/// document.
pub fn carry_info(src: &Document, dst: &mut Document) {
    dst.trailer
        .set("Info", Object::Dictionary(info_based_on(info_dict(src))));
}

/// `/Info` for a document built from scratch: app label plus both dates
/// set to now.
pub fn default_info(doc: &mut Document) {
    let mut info = Dictionary::new();
    stamp_producer_creator(&mut info);
    let now = pdf_date_now();
    info.set("CreationDate", Object::string_literal(now.as_str()));
    info.set("ModDate", Object::string_literal(now.as_str()));
    doc.trailer.set("Info", Object::Dictionary(info));
}

/// Applies [`carry_info`]'s carry-over to a document mutated in place:
/// only the standard `/Info` fields survive, dates re-zone, Producer and
/// Creator get the app label.
pub fn restamp_info(doc: &mut Document) {
    doc.trailer
        .set("Info", Object::Dictionary(info_based_on(info_dict(doc))));
}

/// `PdfMetadataService.setDefaultMetadata` as `CustomPDFDocumentFactory`
/// applies it to every non-read-only `load`: Producer is always the app
/// label and ModDate re-zones to the system zone (or becomes now), while
/// CreationDate/Creator are stamped only when the source has no parseable
/// CreationDate — a present CreationDate and Creator survive untouched, as
/// do all other `/Info` keys including custom ones.
pub fn touch_loaded_info(doc: &mut Document) {
    let mut info = info_dict(doc).unwrap_or_default();
    let label = app_version_label();
    info.set("Producer", Object::string_literal(label.as_str()));
    let mod_date = info
        .get(b"ModDate")
        .ok()
        .and_then(|v| decode_text_string(v).ok())
        .and_then(|t| pdf_date_rezone(&t));
    info.set(
        "ModDate",
        Object::string_literal(mod_date.unwrap_or_else(pdf_date_now).as_str()),
    );
    let creation = info
        .get(b"CreationDate")
        .ok()
        .and_then(|v| decode_text_string(v).ok())
        .and_then(|t| pdf_date_rezone(&t));
    if creation.is_none() {
        let now = pdf_date_now();
        info.set("CreationDate", Object::string_literal(now.as_str()));
        info.set("Creator", Object::string_literal(label.as_str()));
    }
    doc.trailer.set("Info", Object::Dictionary(info));
}

/// Port of `ToSinglePageController.pdfToSinglePage`: stacks every page's
/// appearance top-to-bottom on a single page sized `max(media widths) ×
/// sum(media heights)`. Each source page becomes a Form XObject placed by
/// `LayerUtility`-compatible marked content, including the `LayerN`
/// optional-content groups and the `/Info` metadata carry-over of
/// `createNewDocumentBasedOnOldDocument`.
pub fn to_single_page(src: &Document) -> AppResult<Document> {
    let page_ids = page_ids(src);

    // Java's sizing pass reads only the media boxes; rotation is ignored
    // for both the page size and the stacking offsets.
    let mut total_height = 0.0f64;
    let mut max_width = 0.0f64;
    let mut boxes = Vec::with_capacity(page_ids.len());
    for &page_id in &page_ids {
        // `importPageAsForm` sees `getMediaBox()`/`getCropBox()`: Letter
        // fallback for media, crop clipped to media — `page_boxes` parity.
        let pb = page_boxes(src, page_id)?;
        let media = f64_box(pb.media);
        total_height += media[3] - media[1];
        max_width = max_width.max(media[2] - media[0]);
        boxes.push((media, f64_box(pb.crop)));
    }

    let mut dst = new_document();
    let root = pages_root(&dst)?;
    let mut map = HashMap::new();
    let mut content = Vec::new();
    let mut xobjects = Dictionary::new();
    let mut ocgs = Vec::new();
    let mut y_offset = total_height;

    for (index, &page_id) in page_ids.iter().enumerate() {
        let (media, view) = boxes[index];
        let media_height = media[3] - media[1];
        let rotation = page_rotation(src, page_id)?;

        src.get_page_content_with_limit(page_id, 64 * 1024 * 1024)
            .map_err(pdf_err)?;

        let form_id = page_form_stream(&mut dst, src, page_id, media, view, rotation, &mut map)?;
        xobjects.set(format!("Fm{index}"), form_id);

        // appendFormAsLayer: OCG marked content around q/cm/Do/Q. The
        // translate uses the unrotated media height, like Java.
        content.extend_from_slice(
            format!(
                "/OC << /Type /OCG /Name (Layer{index}) >> BDC\nq\n1 0 0 1 0 {} cm\n/Fm{index} Do\nQ\nEMC\n",
                pdf_num(y_offset - media_height)
            )
            .as_bytes(),
        );
        y_offset -= media_height;

        let mut ocg = Dictionary::new();
        ocg.set("Type", "OCG");
        ocg.set("Name", Object::string_literal(format!("Layer{index}")));
        ocgs.push(Object::Dictionary(ocg));
    }

    let mut page = Dictionary::new();
    page.set("Type", "Page");
    page.set("Parent", root);
    page.set(
        "MediaBox",
        Object::Array(
            [0.0, 0.0, max_width, total_height]
                .iter()
                .map(|v| Object::Real(*v as f32))
                .collect(),
        ),
    );
    let mut resources = Dictionary::new();
    resources.set("XObject", Object::Dictionary(xobjects));
    page.set("Resources", Object::Dictionary(resources));
    let content_id = dst.new_object_id();
    dst.objects.insert(
        content_id,
        Object::Stream(Stream::new(Dictionary::new(), content)),
    );
    page.set("Contents", content_id);

    let new_page_id = dst.new_object_id();
    dst.objects.insert(new_page_id, Object::Dictionary(page));
    {
        let root_dict = dst.get_dictionary_mut(root).map_err(pdf_err)?;
        root_dict.set("Kids", Object::Array(vec![Object::Reference(new_page_id)]));
        root_dict.set("Count", 1i64);
    }

    // Catalog /OCProperties, created lazily by appendFormAsLayer: OCGs and
    // the /D Order list hold the same inline dictionaries.
    if !ocgs.is_empty() {
        let mut d = Dictionary::new();
        d.set("Name", Object::string_literal("Top"));
        d.set("Order", Object::Array(ocgs.clone()));
        let mut props = Dictionary::new();
        props.set("OCGs", Object::Array(ocgs));
        props.set("D", Object::Dictionary(d));
        let catalog_id = match dst.trailer.get(b"Root") {
            Ok(Object::Reference(id)) => Some(*id),
            _ => None,
        };
        if let Some(id) = catalog_id {
            if let Ok(catalog) = dst.get_dictionary_mut(id) {
                catalog.set("OCProperties", Object::Dictionary(props));
            }
        }
    }

    carry_info(src, &mut dst);

    Ok(dst)
}

/// `LayerUtility.importPageAsForm`: the page's content stream wrapped in a
/// Form XObject — `BBox` is the `getCropBox()` view box (crop clipped to
/// media, media fallback), `/Resources` and the `Group`/`LastModified`/
/// `Metadata` entries are copied into `dst`, and `/Matrix` maps source user
/// space into the form's displayed space. Returns the new form object's id.
pub fn page_as_form(dst: &mut Document, src: &Document, page_id: ObjectId) -> AppResult<ObjectId> {
    let boxes = page_boxes(src, page_id)?;
    let rotation = page_rotation(src, page_id)?;
    let mut map = HashMap::new();
    page_form_stream(
        dst,
        src,
        page_id,
        f64_box(boxes.media),
        f64_box(boxes.crop),
        rotation,
        &mut map,
    )
}

/// The box `LayerUtility.importPageAsForm` uses as the form's view box:
/// `PDPage.getCropBox()` — the crop box clipped to the media box, falling
/// back to the media box. Identical to [`page_boxes`]'s `crop`.
pub(crate) fn form_view_box(doc: &Document, page_id: ObjectId) -> AppResult<[f64; 4]> {
    Ok(f64_box(page_boxes(doc, page_id)?.crop))
}

fn page_form_stream(
    dst: &mut Document,
    src: &Document,
    page_id: ObjectId,
    media: [f64; 4],
    view: [f64; 4],
    rotation: i64,
    map: &mut HashMap<ObjectId, ObjectId>,
) -> AppResult<ObjectId> {
    let page_content = src
        .get_page_content_with_limit(page_id, 64 * 1024 * 1024)
        .map_err(pdf_err)?;

    let mut form_dict = Dictionary::new();
    form_dict.set("Type", "XObject");
    form_dict.set("Subtype", "Form");
    form_dict.set(
        "BBox",
        Object::Array(view.iter().map(|v| Object::Real(*v as f32)).collect()),
    );
    let resources = match page_resources(src, page_id)? {
        Some((_, resources)) => match copy_value(dst, src, &resources, map)? {
            Object::Null => Object::Dictionary(Dictionary::new()),
            copied => copied,
        },
        None => Object::Dictionary(Dictionary::new()),
    };
    form_dict.set("Resources", resources);
    // importPageAsForm carries only Group/LastModified/Metadata over
    // from the page dictionary.
    if let Ok(page_dict) = src.get_dictionary(page_id) {
        for key in [&b"Group"[..], &b"LastModified"[..], &b"Metadata"[..]] {
            if let Ok(value) = page_dict.get(key) {
                form_dict.set(key, copy_value(dst, src, value, map)?);
            }
        }
    }
    let at = form_matrix(media, view, rotation);
    if !at.is_identity() {
        form_dict.set("Matrix", at.pdf_array());
    }
    let form_id = dst.new_object_id();
    dst.objects.insert(
        form_id,
        Object::Stream(Stream::new(form_dict, page_content)),
    );
    Ok(form_id)
}

fn f64_box(b: [f32; 4]) -> [f64; 4] {
    [b[0] as f64, b[1] as f64, b[2] as f64, b[3] as f64]
}

/// `LayerUtility.importPageAsForm` with `PDPage` box semantics: the media
/// box falls back to US Letter and the crop box is clipped to it.
/// `map` is the caller's object-id
/// remap, shared across forms so resources clone once per destination
/// document like PDFBox's `PDFCloneUtility`. Returns the form's object id
/// in `dst`.
pub fn import_page_as_form(
    dst: &mut Document,
    src: &Document,
    page_id: ObjectId,
    map: &mut HashMap<ObjectId, ObjectId>,
) -> AppResult<ObjectId> {
    let boxes = page_boxes(src, page_id)?;
    let media = f64_box(boxes.media);
    let view = f64_box(boxes.crop);
    let rotation = page_rotation(src, page_id)?;
    page_form_stream(dst, src, page_id, media, view, rotation, map)
}

// ---------------------------------------------------------------------------
// overlay-pdfs / split-for-poster-print / outline editing
// ---------------------------------------------------------------------------

/// Adds `form_id` to the `/XObject` dictionary of the resources the page
/// resolves to — possibly an ancestor `/Pages` node's, matching PDFBox's
/// inheritable-attribute lookup — under the next free `OL<n>` name, like
/// `PDResources.add(xobject, "OL")`. Returns the resource name.
fn register_xobject(doc: &mut Document, page_id: ObjectId, form_id: ObjectId) -> AppResult<String> {
    let (holder, slot) = match page_resources_holder(doc, page_id) {
        Some(pair) => pair,
        None => {
            // No resources anywhere up the tree: Java installs a fresh
            // /Resources dictionary on the page itself.
            doc.get_dictionary_mut(page_id)
                .map_err(pdf_err)?
                .set("Resources", Object::Dictionary(Dictionary::new()));
            (page_id, Object::Dictionary(Dictionary::new()))
        }
    };
    let mut resources_obj = resolve_owned(doc, &slot)
        .filter(|object| dict_of(object).is_some())
        .ok_or_else(|| AppError::Pdf("page /Resources is not a dictionary".into()))?;
    let resources_dict = dict_mut(&mut resources_obj)
        .ok_or_else(|| AppError::Pdf("page /Resources is not a dictionary".into()))?;

    let xo_slot = resources_dict.get(b"XObject").ok().cloned();
    let xo_resolved = xo_slot.as_ref().and_then(|o| resolve_owned(doc, o));
    let xo_had_dict = xo_resolved
        .as_ref()
        .is_some_and(|object| dict_of(object).is_some());
    let mut xo_obj = match xo_resolved {
        Some(object) if dict_of(&object).is_some() => object,
        _ => Object::Dictionary(Dictionary::new()),
    };
    let xo_dict = dict_mut(&mut xo_obj)
        .ok_or_else(|| AppError::Pdf("page /Resources/XObject is not a dictionary".into()))?;

    // PDResources.createKey: no dict → "OL1"; otherwise the first free
    // "OL<n>" counting up from the dict's size.
    let name = if !xo_had_dict {
        "OL1".to_string()
    } else {
        let mut n = xo_dict.len();
        loop {
            n += 1;
            let key = format!("OL{n}");
            if !xo_dict.has(key.as_bytes()) {
                break key;
            }
        }
    };
    xo_dict.set(name.clone(), Object::Reference(form_id));
    store_object(
        doc,
        resources_dict,
        b"XObject",
        xo_slot.as_ref().unwrap_or(&Object::Null),
        xo_obj,
    );

    match &slot {
        Object::Reference(id) => {
            doc.objects.insert(*id, resources_obj);
        }
        _ => {
            let holder_object = doc
                .objects
                .get_mut(&holder)
                .ok_or_else(|| AppError::Pdf(format!("missing object {holder:?}")))?;
            let holder_dict = dict_mut(holder_object)
                .ok_or_else(|| AppError::Pdf("resources holder is not a dictionary".into()))?;
            holder_dict.set("Resources", resources_obj);
        }
    }
    Ok(name)
}

/// Stamps one overlay page onto a base page, mirroring PDFBox `Overlay`:
/// the overlay page's content becomes a Form XObject registered in the
/// base page's resources, and `/Contents` is rewritten so the form paints
/// in front of (`foreground`) or behind the existing content, centred on
/// the base page's media box.
pub fn overlay_page(
    base: &mut Document,
    src: &Document,
    src_page: ObjectId,
    base_page: ObjectId,
    foreground: bool,
) -> AppResult<()> {
    let media = media_box(src, src_page)?;
    let rotation = page_rotation(src, src_page)?;
    let content = src
        .get_page_content_with_limit(src_page, 64 * 1024 * 1024)
        .map_err(pdf_err)?;

    let (w, h) = (media[2] - media[0], media[3] - media[1]);
    let mut form_dict = Dictionary::new();
    form_dict.set("Type", "XObject");
    form_dict.set("Subtype", "Form");
    form_dict.set("FormType", 1i64);
    // createOverlayFormXObject retranslates the box to the origin.
    form_dict.set(
        "BBox",
        Object::Array(
            [0.0, 0.0, w, h]
                .iter()
                .map(|v| Object::Real(*v as f32))
                .collect(),
        ),
    );
    let resources = match page_resources(src, src_page)? {
        Some((_, resources)) => match copy_value(base, src, &resources, &mut HashMap::new())? {
            Object::Null => Object::Dictionary(Dictionary::new()),
            copied => copied,
        },
        None => Object::Dictionary(Dictionary::new()),
    };
    form_dict.set("Resources", resources);
    // The rotation compensation of createOverlayFormXObject: the same
    // translate/quadrantRotate pairs Java applies to the form matrix.
    let mut at = Affine::identity();
    match rotation {
        90 => {
            at.translate(0.0, w);
            at.quadrant_rotate(3);
        }
        180 => {
            at.translate(w, h);
            at.quadrant_rotate(2);
        }
        270 => {
            at.translate(h, 0.0);
            at.quadrant_rotate(1);
        }
        _ => {}
    }
    form_dict.set("Matrix", at.pdf_array());
    let form_id = base.new_object_id();
    base.objects
        .insert(form_id, Object::Stream(Stream::new(form_dict, content)));

    let name = register_xobject(base, base_page, form_id)?;

    // calculateAffineTransform centres the rotation-transposed overlay box
    // on the base page's media box.
    let (overlay_w, overlay_h) = if matches!(rotation, 90 | 270) {
        (h, w)
    } else {
        (w, h)
    };
    let page_media = media_box(base, base_page)?;
    let h_shift = (page_media[2] - page_media[0] - overlay_w) / 2.0;
    let v_shift = (page_media[3] - page_media[1] - overlay_h) / 2.0;
    let overlay_content = format!(
        "q\nq\n1 0 0 1 {} {} cm\n/{} Do Q\nQ\n",
        pdf_num(h_shift),
        pdf_num(v_shift),
        name
    );

    // The original /Contents entries are kept as objects — Java's
    // addOriginalContent splices the existing streams into a new array.
    let mut original: Vec<Object> = Vec::new();
    {
        let page = base.get_dictionary(base_page).map_err(pdf_err)?;
        if let Ok(contents) = page.get(b"Contents") {
            let contents = contents.clone();
            let (_, resolved) = base.dereference(&contents).map_err(pdf_err)?;
            match resolved {
                Object::Array(items) => original = items.clone(),
                Object::Stream(_) => original.push(contents),
                _ => {}
            }
        }
    }

    let mut add_stream = |bytes: &[u8]| {
        let id = base.new_object_id();
        base.objects.insert(
            id,
            Object::Stream(Stream::new(Dictionary::new(), bytes.to_vec())),
        );
        id
    };
    let overlay_stream = add_stream(overlay_content.as_bytes());
    let (q_stream, cap_stream) = if foreground {
        (Some(add_stream(b"q\n")), Some(add_stream(b"Q\n")))
    } else {
        (None, None)
    };

    let mut contents = Vec::with_capacity(original.len() + 3);
    if let Some(q) = q_stream {
        contents.push(Object::Reference(q));
    }
    if !foreground {
        contents.push(Object::Reference(overlay_stream));
    }
    contents.extend(original);
    if let Some(cap) = cap_stream {
        contents.push(Object::Reference(cap));
    }
    if foreground {
        contents.push(Object::Reference(overlay_stream));
    }
    base.get_dictionary_mut(base_page)
        .map_err(pdf_err)?
        .set("Contents", Object::Array(contents));
    Ok(())
}

/// `LayerUtility.importPageAsForm` as `PosterPdfController` uses it: the
/// page's MediaBox and CropBox are both forced to the crop box first, so
/// the form's user space starts at the crop box origin. Returns the form's
/// object id and the view box it was built from — the coordinate space the
/// poster grid's crop rectangles live in.
pub fn poster_page_form(
    dst: &mut Document,
    src: &Document,
    page_id: ObjectId,
    map: &mut HashMap<ObjectId, ObjectId>,
) -> AppResult<(ObjectId, [f64; 4])> {
    let view = form_view_box(src, page_id)?;
    let rotation = page_rotation(src, page_id)?;
    let id = page_form_stream(dst, src, page_id, view, view, rotation, map)?;
    Ok((id, view))
}

/// Appends a fully-built page dictionary to the document's page tree, like
/// `PDDocument.addPage`: `Parent` is set to the tree root and `/Kids`,
/// `/Count` are updated. Returns the new page's object id.
pub fn append_page(doc: &mut Document, mut page: Dictionary) -> AppResult<ObjectId> {
    let root = pages_root(doc)?;
    let id = doc.new_object_id();
    page.set("Parent", root);
    doc.objects.insert(id, Object::Dictionary(page));
    let root_dict = doc.get_dictionary_mut(root).map_err(pdf_err)?;
    let mut kids = root_dict
        .get(b"Kids")
        .and_then(|k| k.as_array())
        .map_err(pdf_err)?
        .clone();
    kids.push(Object::Reference(id));
    root_dict.set("Kids", Object::Array(kids));
    let count = root_dict
        .get(b"Count")
        .and_then(|c| c.as_i64())
        .unwrap_or(0);
    root_dict.set("Count", count + 1);
    Ok(id)
}

/// One node of the extracted outline tree for `extract-bookmarks`: the raw
/// title (`None` when absent), the 0-based destination page index (`None`
/// for non-local destinations), and children.
#[derive(Debug)]
pub struct TocNode {
    pub title: Option<String>,
    pub page: Option<usize>,
    pub children: Vec<TocNode>,
}

/// Walks the catalog `/Outlines` tree for `extract-bookmarks`, mirroring
/// `EditTableOfContentsController.processChildren`: each item is emitted
/// once across the whole walk and nesting is capped at depth 64.
pub fn outline_bookmarks(doc: &Document) -> AppResult<Vec<TocNode>> {
    let catalog = doc.catalog().map_err(pdf_err)?;
    let Some(outlines) = catalog.get(b"Outlines").ok() else {
        return Ok(Vec::new());
    };
    let Some(root) = dict_owned(doc, outlines)? else {
        return Ok(Vec::new());
    };
    let Some(first) = root.get(b"First").ok() else {
        return Ok(Vec::new());
    };
    let pages = page_lookup(doc)?;
    let mut visited = HashSet::new();
    let mut nodes = Vec::new();
    outline_bookmarks_level(doc, first, &pages, &mut visited, 0, &mut nodes)?;
    Ok(nodes)
}

fn outline_bookmarks_level(
    doc: &Document,
    first: &Object,
    pages: &HashMap<ObjectId, usize>,
    visited: &mut HashSet<ObjectId>,
    depth: usize,
    out: &mut Vec<TocNode>,
) -> AppResult<()> {
    const MAX_OUTLINE_DEPTH: usize = 64;
    if depth > MAX_OUTLINE_DEPTH {
        return Ok(());
    }
    let mut current = Some(first.clone());
    while let Some(object) = current {
        let Some(dict) = dict_owned(doc, &object)? else {
            break;
        };
        // Java dedupes on the item's COSDictionary identity; only indirect
        // objects can be shared, so direct dictionaries need no check.
        let seen = object
            .as_reference()
            .ok()
            .is_some_and(|id| !visited.insert(id));
        if !seen {
            let title = dict.get(b"Title").ok().and_then(|t| {
                let (_, resolved) = doc.dereference(t).ok()?;
                decode_text_string(resolved).ok()
            });
            let page = outline_dest_page(doc, &dict, pages)?;
            let mut children = Vec::new();
            if let Ok(first_child) = dict.get(b"FirstChild") {
                outline_bookmarks_level(
                    doc,
                    first_child,
                    pages,
                    visited,
                    depth + 1,
                    &mut children,
                )?;
            }
            out.push(TocNode {
                title,
                page,
                children,
            });
        }
        current = dict.get(b"Next").ok().cloned();
    }
    Ok(())
}

/// One node of the replacement outline for `edit-table-of-contents`:
/// `page` is a validated 0-based page index.
#[derive(Debug)]
pub struct OutlineSpec {
    pub title: String,
    pub page: usize,
    pub children: Vec<OutlineSpec>,
}

/// Replaces the catalog `/Outlines` tree with `items`, like
/// `EditTableOfContentsController`: a fresh outline root, `/XYZ`
/// destinations, and PDFBox's count bookkeeping — every node is created
/// closed, so each parent's `/Count` is minus its direct-child count and
/// the root's is minus the top-level count.
pub fn write_outline(doc: &mut Document, items: &[OutlineSpec]) -> AppResult<()> {
    let pages = page_ids(doc);
    let root_id = doc.new_object_id();
    let mut root = Dictionary::new();
    root.set("Type", "Outlines");
    doc.objects.insert(root_id, Object::Dictionary(root));

    let top = outline_level(doc, items, root_id, &pages)?;
    if !top.is_empty() {
        let root_dict = doc.get_dictionary_mut(root_id).map_err(pdf_err)?;
        root_dict.set("First", top[0]);
        root_dict.set("Last", *top.last().unwrap());
        root_dict.set("Count", -(top.len() as i64));
    }
    let catalog_id = match doc.trailer.get(b"Root") {
        Ok(Object::Reference(id)) => *id,
        _ => return Err(AppError::Pdf("document has no catalog".into())),
    };
    doc.get_dictionary_mut(catalog_id)
        .map_err(pdf_err)?
        .set("Outlines", root_id);
    Ok(())
}

fn outline_level(
    doc: &mut Document,
    items: &[OutlineSpec],
    parent: ObjectId,
    pages: &[ObjectId],
) -> AppResult<Vec<ObjectId>> {
    let mut ids = Vec::with_capacity(items.len());
    for item in items {
        let Some(&page_id) = pages.get(item.page) else {
            return Err(AppError::BadRequest(format!(
                "outline page index {} out of range ({} pages)",
                item.page + 1,
                pages.len()
            )));
        };
        let id = doc.new_object_id();
        let mut dict = Dictionary::new();
        dict.set("Title", outline_text(&item.title));
        dict.set("Parent", parent);
        // PDPageXYZDestination with only the page set: [page /XYZ null…].
        dict.set(
            "Dest",
            Object::Array(vec![
                Object::Reference(page_id),
                Object::Name(b"XYZ".to_vec()),
                Object::Null,
                Object::Null,
                Object::Null,
            ]),
        );
        doc.objects.insert(id, Object::Dictionary(dict));
        ids.push(id);
    }
    for pair in ids.windows(2) {
        doc.get_dictionary_mut(pair[0])
            .map_err(pdf_err)?
            .set("Next", pair[1]);
        doc.get_dictionary_mut(pair[1])
            .map_err(pdf_err)?
            .set("Prev", pair[0]);
    }
    for (item, &id) in items.iter().zip(&ids) {
        if item.children.is_empty() {
            continue;
        }
        let children = outline_level(doc, &item.children, id, pages)?;
        let dict = doc.get_dictionary_mut(id).map_err(pdf_err)?;
        dict.set("FirstChild", children[0]);
        dict.set("LastChild", *children.last().unwrap());
        dict.set("Count", -(children.len() as i64));
    }
    Ok(ids)
}

/// PDF text string for outline titles: literal for ASCII (PDFDocEncoding
/// territory), UTF-16BE hex otherwise — the encodings `COSString` picks.
fn outline_text(value: &str) -> Object {
    if value.is_ascii() {
        Object::string_literal(value.as_bytes().to_vec())
    } else {
        let mut bytes = vec![0xFE, 0xFF];
        for unit in value.encode_utf16() {
            bytes.extend_from_slice(&unit.to_be_bytes());
        }
        Object::String(bytes, StringFormat::Hexadecimal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit;
    use lopdf::{dictionary, Document, Object};

    /// Runs `to_single_page` on a testkit document and returns the result.
    fn run(pdf: &mut testkit::Pdf) -> Document {
        let bytes = pdf.bytes();
        let src = Document::load_mem(&bytes).unwrap();
        to_single_page(&src).unwrap()
    }

    fn give_contents(pdf: &mut testkit::Pdf) {
        let content = pdf.add_stream(Dictionary::new(), b"0 0 m\n");
        pdf.edit_page(|page| {
            page.set("Contents", Object::Reference(content));
        });
    }

    /// The `(BBox, Matrix, Resources)` dictionary of the `name` Form
    /// XObject on the single output page.
    fn form_dict(doc: &Document, name: &str) -> Dictionary {
        let page_id = doc.get_pages().into_values().next().unwrap();
        let page = doc.get_dictionary(page_id).unwrap();
        let resources = page.get(b"Resources").unwrap();
        let (_, resources) = doc.dereference(resources).unwrap();
        let Object::Dictionary(resources) = resources else {
            panic!("page resources are not a dictionary");
        };
        let xobjects = resources.get(b"XObject").unwrap();
        let (_, xobjects) = doc.dereference(xobjects).unwrap();
        let Object::Dictionary(xobjects) = xobjects else {
            panic!("XObject resources are not a dictionary");
        };
        let form = xobjects.get(name.as_bytes()).unwrap();
        let (_, resolved) = doc.dereference(form).unwrap();
        let Object::Stream(stream) = resolved else {
            panic!("{name} is not a stream");
        };
        stream.dict.clone()
    }

    fn page_content(doc: &Document) -> String {
        let page_id = doc.get_pages().into_values().next().unwrap();
        let bytes = doc.get_page_content(page_id);
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[test]
    fn to_single_page_stacks_pages_and_carries_rotation_matrix() {
        let mut pdf = testkit::Pdf::new();
        give_contents(&mut pdf);
        pdf.edit_page(|page| {
            page.set("Rotate", Object::Integer(90));
        });
        pdf.add_page([0, 0, 300, 400], Dictionary::new());

        let single = run(&mut pdf);
        let page_id = single.get_pages().into_values().next().unwrap();
        assert_eq!(
            media_box(&single, page_id).unwrap(),
            [0.0, 0.0, 612.0, 1192.0]
        );

        let content = page_content(&single);
        assert!(content.contains("/Fm0 Do"));
        assert!(content.contains("/Fm1 Do"));
        assert!(content.contains("(Layer0)"));
        assert!(content.contains("(Layer1)"));

        let rotated = form_dict(&single, "Fm0");
        assert!(rotated.has(b"Matrix"));
        let plain = form_dict(&single, "Fm1");
        assert!(!plain.has(b"Matrix"));

        let catalog = single.catalog().unwrap();
        assert!(catalog.has(b"OCProperties"));
    }

    #[test]
    fn to_single_page_handles_empty_document() {
        // Java still emits one page (0x0 MediaBox) for a page-less source.
        let src = new_document();
        let dst = to_single_page(&src).unwrap();
        assert_eq!(dst.get_pages().len(), 1);
        let page_id = dst.get_pages().into_values().next().unwrap();
        assert_eq!(media_box(&dst, page_id).unwrap(), [0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn to_single_page_copies_inherited_page_resources() {
        let mut pdf = testkit::Pdf::new();
        give_contents(&mut pdf);
        let font = pdf.add(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let mut pages = pdf.dict(pdf.pages);
        pages.set(
            "Resources",
            Object::Dictionary(dictionary! {
                "Font" => Object::Dictionary(dictionary! {
                    "F1" => Object::Reference(font),
                }),
            }),
        );
        pdf.put_dict(pdf.pages, pages);

        let single = run(&mut pdf);
        let form = form_dict(&single, "Fm0");
        let resources = form.get(b"Resources").unwrap();
        let (_, resolved) = single.dereference(resources).unwrap();
        let Object::Dictionary(resources) = resolved else {
            panic!("form resources are not a dictionary");
        };
        let fonts = resources.get(b"Font").unwrap();
        let (_, resolved) = single.dereference(fonts).unwrap();
        let Object::Dictionary(fonts) = resolved else {
            panic!("form fonts are not a dictionary");
        };
        assert!(fonts.has(b"F1"));
    }

    #[test]
    fn to_single_page_handles_nonzero_mediabox_origin() {
        let mut pdf = testkit::Pdf::new();
        give_contents(&mut pdf);
        pdf.edit_page(|page| {
            page.set(
                "MediaBox",
                Object::Array(vec![100.into(), 200.into(), 712.into(), 992.into()]),
            );
        });

        let single = run(&mut pdf);
        let page_id = single.get_pages().into_values().next().unwrap();
        assert_eq!(
            media_box(&single, page_id).unwrap(),
            [0.0, 0.0, 612.0, 792.0]
        );

        let form = form_dict(&single, "Fm0");
        let bbox = form.get(b"BBox").unwrap();
        let (_, resolved) = single.dereference(bbox).unwrap();
        let Object::Array(nums) = resolved else {
            panic!("form BBox is not an array");
        };
        let values: Vec<f64> = nums
            .iter()
            .map(|n| match n {
                Object::Integer(v) => *v as f64,
                Object::Real(v) => *v as f64,
                _ => panic!("non-numeric BBox entry"),
            })
            .collect();
        assert_eq!(values, vec![100.0, 200.0, 712.0, 992.0]);

        let matrix = form.get(b"Matrix").unwrap();
        let (_, resolved) = single.dereference(matrix).unwrap();
        let Object::Array(nums) = resolved else {
            panic!("form Matrix is not an array");
        };
        let values: Vec<f64> = nums
            .iter()
            .map(|n| match n {
                Object::Integer(v) => *v as f64,
                Object::Real(v) => *v as f64,
                _ => panic!("non-numeric Matrix entry"),
            })
            .collect();
        assert!((values[4] - -100.0).abs() < 0.01);
        assert!((values[5] - -200.0).abs() < 0.01);
    }

    /// Two pages; a text field's widget sits on page 2 only. Subsetting to
    /// page 1 then pruning must drop the field and its `/AcroForm`.
    #[test]
    fn prune_orphaned_form_fields_drops_fields_on_removed_pages() {
        let mut pdf = testkit::Pdf::new();
        let page2 = pdf.add_page([0, 0, 612, 792].map(|v| v as i64), Dictionary::new());
        let widget = pdf.add(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Widget",
            "FT" => "Tx",
            "T" => "gone",
            "Rect" => Object::Array(vec![0.into(), 0.into(), 10.into(), 10.into()]),
            "P" => Object::Reference(page2),
        });
        let field = pdf.add(dictionary! {
            "FT" => "Tx",
            "T" => "gone",
            "Kids" => vec![Object::Reference(widget)],
        });
        let acroform = pdf.add(dictionary! {
            "Fields" => vec![Object::Reference(field)],
        });
        pdf.edit_catalog(|catalog| {
            catalog.set("AcroForm", Object::Reference(acroform));
        });
        {
            let mut p2 = pdf.dict(page2);
            p2.set("Annots", vec![Object::Reference(widget)]);
            pdf.put_dict(page2, p2);
        }

        let bytes = pdf.bytes();
        let mut doc = Document::load_mem(&bytes).unwrap();
        subset(&mut doc, &[0]).unwrap();
        prune_orphaned_form_fields(&mut doc).unwrap();

        let catalog = doc.catalog().unwrap();
        assert!(catalog.get(b"AcroForm").is_err());
    }

    /// Same fixture, but the widget survives on the kept page: the field
    /// and its `/Kids` must be preserved.
    #[test]
    fn prune_orphaned_form_fields_keeps_live_widgets() {
        let mut pdf = testkit::Pdf::new();
        let widget = pdf.add(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Widget",
            "FT" => "Tx",
            "T" => "kept",
            "Rect" => Object::Array(vec![0.into(), 0.into(), 10.into(), 10.into()]),
            "P" => Object::Reference(pdf.page),
        });
        let field = pdf.add(dictionary! {
            "FT" => "Tx",
            "T" => "kept",
            "Kids" => vec![Object::Reference(widget)],
        });
        let acroform = pdf.add(dictionary! {
            "Fields" => vec![Object::Reference(field)],
        });
        pdf.edit_catalog(|catalog| {
            catalog.set("AcroForm", Object::Reference(acroform));
        });
        pdf.edit_page(|page| {
            page.set("Annots", vec![Object::Reference(widget)]);
        });

        let bytes = pdf.bytes();
        let mut doc = Document::load_mem(&bytes).unwrap();
        subset(&mut doc, &[0]).unwrap();
        prune_orphaned_form_fields(&mut doc).unwrap();

        let catalog = doc.catalog().unwrap();
        let acro = catalog.get(b"AcroForm").unwrap();
        let (_, acro_obj) = doc.dereference(acro).unwrap();
        let Object::Dictionary(acro_dict) = acro_obj else {
            panic!("AcroForm is not a dictionary");
        };
        let Ok(Object::Array(fields)) = acro_dict.get(b"Fields") else {
            panic!("/Fields is not an array");
        };
        assert_eq!(fields.len(), 1);
    }

    /// `PDPage.getCropBox()` clips to the media box, so a CropBox reaching
    /// past it must not leak into the Form XObject's `BBox`.
    #[test]
    fn to_single_page_clips_crop_box_to_media_box() {
        let mut pdf = testkit::Pdf::new();
        give_contents(&mut pdf);
        pdf.edit_page(|page| {
            page.set(
                "CropBox",
                Object::Array(vec![
                    Object::Integer(-50),
                    Object::Integer(-50),
                    Object::Integer(700),
                    Object::Integer(900),
                ]),
            );
        });

        let single = run(&mut pdf);
        let form = form_dict(&single, "Fm0");
        let bbox = form.get(b"BBox").unwrap();
        let Object::Array(bbox) = bbox else {
            panic!("BBox is not an array");
        };
        let nums: Vec<f64> = bbox
            .iter()
            .map(|o| match o {
                Object::Real(f) => *f as f64,
                Object::Integer(i) => *i as f64,
                _ => panic!("BBox element is not a number"),
            })
            .collect();
        assert_eq!(nums, vec![0.0, 0.0, 612.0, 792.0]);
    }
}
