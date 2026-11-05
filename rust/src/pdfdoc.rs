//! Pure-Rust PDF backend on `lopdf`'s object model: page-tree surgery for
//! reorder/remove, reachability copying for merge/split, dictionary reads
//! for the analysis endpoints.

use std::collections::{HashMap, HashSet};

use lopdf::{decode_text_string, Dictionary, Document, Object, ObjectId, SaveOptions};

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
    inherited_slot(doc, page_id, b"Resources")
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
    f: impl FnOnce(&mut Dictionary),
) -> AppResult<()> {
    let object = doc
        .objects
        .get_mut(&path.root)
        .ok_or_else(|| AppError::Pdf(format!("missing object {:?}", path.root)))?;
    let dict = dict_mut(object)
        .ok_or_else(|| AppError::Pdf(format!("object {:?} is not a dictionary", path.root)))?;
    update_nested(dict, &path.keys, &mut |d| f(d));
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

/// Applies `f` to the dictionary behind `slot` (following one reference
/// level), then writes the result back through `parent`/`key`. No-op when
/// `slot` doesn't resolve to a dictionary or stream.
pub fn edit_dict_at(
    doc: &mut Document,
    parent: &mut Dictionary,
    key: &[u8],
    slot: &Object,
    f: impl FnOnce(&mut Document, &mut Dictionary),
) {
    let Some(mut object) = resolve_owned(doc, slot) else {
        return;
    };
    let Some(dict) = dict_mut(&mut object) else {
        return;
    };
    f(doc, dict);
    store_object(doc, parent, key, slot, object);
}
