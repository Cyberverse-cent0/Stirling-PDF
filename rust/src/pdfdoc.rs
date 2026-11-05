//! Pure-Rust PDF backend on `lopdf`'s object model: page-tree surgery for
//! reorder/remove, reachability copying for merge/split, dictionary reads
//! for the analysis endpoints.

// Shared helper surface for endpoints still being ported; unused helpers are
// expected while the strangler migration is incomplete.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};

use lopdf::{decode_text_string, Dictionary, Document, Object, ObjectId, SaveOptions, Stream};

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
    if doc.trailer.has(b"Encrypt") && !doc.was_encrypted() {
        return Err(AppError::BadRequest(format!(
            "{name}: password required or incorrect password"
        )));
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
pub fn page_fonts(doc: &Document, page_id: ObjectId) -> AppResult<Vec<String>> {
    let fonts = doc.get_page_fonts(page_id).map_err(pdf_err)?;
    let mut names = Vec::with_capacity(fonts.len());
    for (_, font) in fonts {
        if let Ok(name) = font.get(b"BaseFont").and_then(|o| o.as_name()) {
            names.push(String::from_utf8_lossy(name).into_owned());
        }
    }
    Ok(names)
}

/// The catalog's AcroForm dictionary, if any.
fn acroform(doc: &Document) -> Option<Dictionary> {
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

fn pdf_num(value: f64) -> String {
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

fn kept_annotation(object: &Object, dict: &Dictionary) -> bool {
    matches!(
        dict.get(b"Subtype")
            .ok()
            .and_then(|o| o.as_name().ok())
            .map(<[u8]>::to_vec)
            .as_deref(),
        Some(b"Link") | Some(b"Widget")
    ) && !matches!(object, Object::Null)
}

fn rebuild_annots(
    doc: &mut Document,
    page_id: ObjectId,
    matrix: Option<[f64; 6]>,
    clip: Option<[f64; 4]>,
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
        if !kept_annotation(&item, dict) {
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
/// by the same matrix applied to the page content.
pub fn scale_annots(doc: &mut Document, page_id: ObjectId, matrix: [f64; 6]) -> AppResult<()> {
    rebuild_annots(doc, page_id, Some(matrix), None)
}

/// Keeps link/widget annotations that intersect `crop` on a cropped page.
/// Annotation rectangles stay in page user space, matching the unchanged
/// content coordinates.
pub fn clip_annots(doc: &mut Document, page_id: ObjectId, crop: [f64; 4]) -> AppResult<()> {
    rebuild_annots(doc, page_id, None, Some(crop))
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
struct Affine {
    m00: f64,
    m01: f64,
    m02: f64,
    m10: f64,
    m11: f64,
    m12: f64,
}

impl Affine {
    fn identity() -> Self {
        Self {
            m00: 1.0,
            m01: 0.0,
            m02: 0.0,
            m10: 0.0,
            m11: 1.0,
            m12: 0.0,
        }
    }

    fn translate(&mut self, tx: f64, ty: f64) {
        self.m02 += self.m00 * tx + self.m01 * ty;
        self.m12 += self.m10 * tx + self.m11 * ty;
    }

    fn scale(&mut self, sx: f64, sy: f64) {
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
fn form_matrix(media: [f64; 4], view: [f64; 4], rotation: i64) -> Affine {
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
        let media = media_box(src, page_id)?;
        total_height += media[3] - media[1];
        max_width = max_width.max(media[2] - media[0]);
        // PDFBox's viewBox is the raw crop box when present — no clipping
        // to the media box, unlike `page_boxes`.
        let view = match inherited(src, page_id, b"CropBox")? {
            Some(object) => {
                let (_, resolved) = src.dereference(&object).map_err(pdf_err)?;
                box_numbers(resolved)?
            }
            None => media,
        };
        boxes.push((media, view));
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
            Some((_, resources)) => match copy_value(&mut dst, src, &resources, &mut map)? {
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
                    form_dict.set(key, copy_value(&mut dst, src, value, &mut map)?);
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

    // createNewDocumentBasedOnOldDocument: the standard /Info fields carry
    // over; Producer/Creator get the app label; dates fall back to now.
    let mut info = Dictionary::new();
    let src_info = info_dict(src);
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
    // PdfMetadataService round-trips dates through Calendar, rewriting
    // them in the system zone; unparseable values become null → now.
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
    let label = app_version_label();
    info.set("Producer", Object::string_literal(label.as_str()));
    info.set("Creator", Object::string_literal(label));
    let now = pdf_date_now();
    if !info.has(b"CreationDate") {
        info.set("CreationDate", Object::string_literal(now.as_str()));
    }
    if !info.has(b"ModDate") {
        info.set("ModDate", Object::string_literal(now.as_str()));
    }
    dst.trailer.set("Info", Object::Dictionary(info));

    Ok(dst)
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
}
