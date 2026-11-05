//! Port of PDFBox `PDAcroForm.flatten()` for `misc/flatten` with
//! `flattenOnlyForms=true`: each widget annotation's normal appearance stream
//! is drawn into the page content stream, widget annotations are removed from
//! `/Annots`, and the field tree is emptied.
//!
//! Divergence: widgets whose `/AP` `/N` is missing, invisible/hidden, or has
//! an empty `/BBox` are removed without being drawn — same as Java. Widgets
//! that are direct (non-indirect) dictionaries can't be matched to page
//! annotations and are ignored, again matching PDFBox's object-identity
//! comparison.

use std::collections::{HashMap, HashSet};

use lopdf::{Dictionary, Document, Object, ObjectId, Stream};

use crate::error::AppResult;
use crate::pdfdoc::{self, DictPath};

/// Flattens every AcroForm field in `doc`, like `PDAcroForm.flatten()`.
/// Documents without an AcroForm or with dynamic XFA (XFA present and no
/// fields) are returned unchanged.
pub fn flatten(doc: &mut Document) -> AppResult<()> {
    let catalog = doc.catalog().map_err(|e| crate::error::AppError::Pdf(e.to_string()))?;
    let Some(acro_slot) = catalog.get(b"AcroForm").ok().cloned() else {
        return Ok(());
    };
    let Some(mut acro) = pdfdoc::peek_dict(doc, &acro_slot).cloned() else {
        return Ok(());
    };

    // Dynamic XFA forms (XFA present, no fields) can't be flattened without
    // rendering the XFA; hybrid forms with fields do flatten and lose /XFA.
    let nodes = field_nodes(doc, &acro)?;
    if nodes.is_empty() {
        return Ok(());
    }

    let mut widgets = HashSet::new();
    let mut visited = HashSet::new();
    collect_widgets(doc, &acro, &mut widgets, &mut visited, 0)?;

    for page_id in pdfdoc::page_ids(doc) {
        flatten_page(doc, page_id, &widgets)?;
    }

    // Every field node is detached: clearing /Fields orphans the whole tree,
    // the same end state as PDFBox removing each field from its parent.
    acro.set("Fields", Object::Array(Vec::new()));
    acro.remove(b"XFA");
    // All signature fields were just removed, so SigFlags is meaningless.
    acro.remove(b"SigFlags");
    match &acro_slot {
        Object::Reference(id) => {
            doc.objects.insert(*id, Object::Dictionary(acro));
        }
        _ => {
            if let Ok(catalog) = doc.catalog_mut() {
                catalog.set("AcroForm", Object::Dictionary(acro));
            }
        }
    }
    Ok(())
}

/// Object ids of all dictionaries reachable from `/Fields` — the field tree
/// nodes PDFBox would hand to `flatten()`.
fn field_nodes(doc: &Document, acro: &Dictionary) -> AppResult<Vec<ObjectId>> {
    let mut nodes = Vec::new();
    let mut visited = HashSet::new();
    if let Ok(fields) = acro.get(b"Fields") {
        let (_, resolved) = doc
            .dereference(fields)
            .map_err(|e| crate::error::AppError::Pdf(e.to_string()))?;
        if let Object::Array(items) = resolved {
            for item in items {
                collect_field_nodes(doc, item, &mut nodes, &mut visited, 0)?;
            }
        }
    }
    Ok(nodes)
}

fn collect_field_nodes(
    doc: &Document,
    object: &Object,
    nodes: &mut Vec<ObjectId>,
    visited: &mut HashSet<ObjectId>,
    depth: usize,
) -> AppResult<()> {
    if depth > 100 {
        return Ok(());
    }
    let Ok(id) = object.as_reference() else {
        return Ok(());
    };
    if !visited.insert(id) {
        return Ok(());
    }
    let Some(dict) = pdfdoc::dict_of(doc.get_object(id).map_err(pdf_err)?) else {
        return Ok(());
    };
    let dict = dict.clone();
    nodes.push(id);
    if let Ok(kids) = dict.get(b"Kids") {
        let (_, resolved) = doc
            .dereference(kids)
            .map_err(|e| crate::error::AppError::Pdf(e.to_string()))?;
        if let Object::Array(items) = resolved {
            for kid in items {
                collect_field_nodes(doc, kid, nodes, visited, depth + 1)?;
            }
        }
    }
    Ok(())
}

fn collect_widgets(
    doc: &Document,
    acro: &Dictionary,
    widgets: &mut HashSet<ObjectId>,
    visited: &mut HashSet<ObjectId>,
    depth: usize,
) -> AppResult<()> {
    if depth > 100 {
        return Ok(());
    }
    let Ok(fields) = acro.get(b"Fields") else {
        return Ok(());
    };
    let (_, resolved) = doc
        .dereference(fields)
        .map_err(|e| crate::error::AppError::Pdf(e.to_string()))?;
    let Object::Array(items) = resolved else {
        return Ok(());
    };
    for item in items {
        walk_field_node(doc, item, widgets, visited, depth)?;
    }
    Ok(())
}

fn walk_field_node(
    doc: &Document,
    object: &Object,
    widgets: &mut HashSet<ObjectId>,
    visited: &mut HashSet<ObjectId>,
    depth: usize,
) -> AppResult<()> {
    if depth > 100 {
        return Ok(());
    }
    let Ok(id) = object.as_reference() else {
        return Ok(());
    };
    if !visited.insert(id) {
        return Ok(());
    }
    let Some(dict) = pdfdoc::dict_of(doc.get_object(id).map_err(pdf_err)?) else {
        return Ok(());
    };
    let dict = dict.clone();
    if dict
        .get(b"Subtype")
        .ok()
        .and_then(|o| o.as_name().ok())
        == Some(b"Widget")
    {
        widgets.insert(id);
    }
    if let Ok(kids) = dict.get(b"Kids") {
        let (_, resolved) = doc
            .dereference(kids)
            .map_err(|e| crate::error::AppError::Pdf(e.to_string()))?;
        if let Object::Array(items) = resolved {
            for kid in items {
                walk_field_node(doc, kid, widgets, visited, depth + 1)?;
            }
        }
    }
    Ok(())
}

fn pdf_err(e: lopdf::Error) -> crate::error::AppError {
    crate::error::AppError::Pdf(e.to_string())
}

/// Removes this page's widget annotations, drawing each visible one's
/// appearance stream into the content, then appends the collected ops as a
/// new content stream.
fn flatten_page(
    doc: &mut Document,
    page_id: ObjectId,
    widgets: &HashSet<ObjectId>,
) -> AppResult<()> {
    if widgets.is_empty() {
        return Ok(());
    }
    let page = doc
        .get_dictionary(page_id)
        .map_err(pdf_err)?
        .clone();
    let Some(annots_obj) = page.get(b"Annots").ok().cloned() else {
        return Ok(());
    };
    let (_, resolved) = doc
        .dereference(&annots_obj)
        .map_err(pdf_err)?;
    let Object::Array(items) = resolved else {
        return Ok(());
    };
    let items = items.clone();

    let mut keep = Vec::with_capacity(items.len());
    let mut ops = Vec::new();
    let mut names: HashMap<(ObjectId, ObjectId), String> = HashMap::new();

    for item in &items {
        let is_widget = item
            .as_reference()
            .ok()
            .is_some_and(|id| widgets.contains(&id));
        if !is_widget {
            keep.push(item.clone());
            continue;
        }
        if let Some(bytes) = widget_appearance_ops(doc, page_id, item, &mut names)? {
            ops.extend_from_slice(&bytes);
        }
    }

    {
        let page_dict = doc.get_dictionary_mut(page_id).map_err(pdf_err)?;
        page_dict.set("Annots", Object::Array(keep));
    }

    if !ops.is_empty() {
        let mut stream = Stream::new(Dictionary::new(), ops);
        let _ = stream.compress();
        let stream_id = doc.new_object_id();
        doc.objects.insert(stream_id, Object::Stream(stream));

        let page_dict = doc.get_dictionary_mut(page_id).map_err(pdf_err)?;
        let contents = page_dict.get(b"Contents").ok().cloned();
        match contents {
            Some(Object::Array(mut arr)) => {
                arr.push(Object::Reference(stream_id));
                page_dict.set("Contents", Object::Array(arr));
            }
            Some(existing) if !matches!(existing, Object::Null) => {
                page_dict.set(
                    "Contents",
                    Object::Array(vec![existing, Object::Reference(stream_id)]),
                );
            }
            _ => {
                page_dict.set("Contents", Object::Reference(stream_id));
            }
        }
    }
    Ok(())
}

/// Builds the `q … cm /Fx Do Q` snippet placing the widget's normal
/// appearance at its `/Rect`, registering the form XObject in the page's
/// resources. `None` when the widget shouldn't be drawn (invisible, hidden,
/// no usable appearance) — the annotation is still removed by the caller.
fn widget_appearance_ops(
    doc: &mut Document,
    page_id: ObjectId,
    annot: &Object,
    names: &mut HashMap<(ObjectId, ObjectId), String>,
) -> AppResult<Option<Vec<u8>>> {
    let Ok(id) = annot.as_reference() else {
        return Ok(None);
    };
    let Some(dict) = pdfdoc::dict_of(doc.get_object(id).map_err(pdf_err)?) else {
        return Ok(None);
    };
    let dict = dict.clone();

    // Annotation flags: bit 0 invisible, bit 1 hidden.
    let flags = dict.get(b"F").ok().and_then(|o| o.as_i64().ok()).unwrap_or(0);
    if flags & 0b11 != 0 {
        return Ok(None);
    }

    let Some((x0, y0, x1, y1)) = dict.get(b"Rect").ok().and_then(|r| rect_array(doc, r))
    else {
        return Ok(None);
    };
    let (rect_w, rect_h) = (x1 - x0, y1 - y0);
    if rect_w == 0.0 || rect_h == 0.0 {
        return Ok(None);
    }

    let Some((ap_id, ap_dict)) = normal_appearance(doc, &dict) else {
        return Ok(None);
    };

    let Some(bbox) = ap_dict.get(b"BBox").ok().and_then(|b| rect_array(doc, b)) else {
        return Ok(None);
    };
    let box_w = bbox.2 - bbox.0;
    let box_h = bbox.3 - bbox.1;
    if box_w <= 0.0 || box_h <= 0.0 {
        return Ok(None);
    }

    // Appearance /Matrix maps form space into the appearance's own space;
    // PDFBox measures the BBox *after* that transform, then maps the result
    // onto the annotation rect.
    let matrix = ap_dict
        .get(b"Matrix")
        .ok()
        .and_then(|o| o.as_array().ok())
        .map(|a| {
            let num = |i: usize| match a.get(i) {
                Some(Object::Integer(v)) => *v as f64,
                Some(Object::Real(v)) => *v as f64,
                _ => f64::NAN,
            };
            [num(0), num(1), num(2), num(3), num(4), num(5)]
        })
        .filter(|m| m.iter().all(|v| v.is_finite()))
        .unwrap_or([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);

    let (mut min_x, mut min_y, mut max_x, mut max_y) =
        (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
    for (px, py) in [
        (bbox.0, bbox.1),
        (bbox.2, bbox.1),
        (bbox.0, bbox.3),
        (bbox.2, bbox.3),
    ] {
        let tx = matrix[0] * px + matrix[2] * py + matrix[4];
        let ty = matrix[1] * px + matrix[3] * py + matrix[5];
        min_x = min_x.min(tx);
        min_y = min_y.min(ty);
        max_x = max_x.max(tx);
        max_y = max_y.max(ty);
    }
    let (box_w, box_h) = (max_x - min_x, max_y - min_y);
    if box_w <= 0.0 || box_h <= 0.0 {
        return Ok(None);
    }

    let sx = rect_w / box_w;
    let sy = rect_h / box_h;
    let dx = x0 - min_x;
    let dy = y0 - min_y;

    let name = match names.get(&(page_id, ap_id)) {
        Some(name) => name.clone(),
        None => {
            let name = register_xobject(doc, page_id, ap_id)?;
            names.insert((page_id, ap_id), name.clone());
            name
        }
    };

    Ok(Some(
        format!("q\n{sx} 0 0 {sy} {dx} {dy} cm\n/{name} Do\nQ\n").into_bytes(),
    ))
}

/// The widget's normal appearance: `/AP` `/N` is either a stream or a
/// sub-dictionary keyed by appearance state (`/AS` on the widget).
fn normal_appearance(doc: &Document, widget: &Dictionary) -> Option<(ObjectId, Dictionary)> {
    let ap = pdfdoc::peek_dict(doc, widget.get(b"AP").ok()?)?;
    let normal = ap.get(b"N").ok()?;
    match normal {
        Object::Reference(id) => {
            if let Some(stream) = doc
                .get_object(*id)
                .ok()
                .and_then(|o| match o {
                    Object::Stream(s) => Some(s.dict.clone()),
                    _ => None,
                })
            {
                return Some((*id, stream));
            }
            None
        }
        Object::Dictionary(dict) => {
            let state = widget.get(b"AS").ok()?.as_name().ok()?;
            let entry = dict.get(state).ok()?;
            let id = entry.as_reference().ok()?;
            let stream = doc
                .get_object(id)
                .ok()
                .and_then(|o| match o {
                    Object::Stream(s) => Some(s.dict.clone()),
                    _ => None,
                })?;
            Some((id, stream))
        }
        _ => None,
    }
}

/// Adds the appearance stream to the page's effective `/Resources`
/// `/XObject` under a fresh name, reusing an existing entry when the same
/// stream is already registered.
fn register_xobject(doc: &mut Document, page_id: ObjectId, ap_id: ObjectId) -> AppResult<String> {
    let (holder, resources) = pdfdoc::page_resources_holder(doc, page_id)
        .ok_or_else(|| pdf_err(lopdf::Error::ObjectNotFound(page_id)))?;

    let (root, mut keys) = match &resources {
        Object::Reference(id) => (*id, Vec::new()),
        Object::Dictionary(_) => (holder, vec![b"Resources".to_vec()]),
        _ => return Ok(String::new()),
    };

    // Locate or create the /XObject dictionary inside the resources dict.
    let resources_path = DictPath {
        root,
        keys: keys.clone(),
    };
    let resources_dict = pdfdoc::dict_at(doc, &resources_path)
        .ok_or_else(|| pdf_err(lopdf::Error::ObjectNotFound(root)))?;
    let xobjects = resources_dict.get(b"XObject").ok().cloned();

    let (xpath, existing) = match xobjects {
        Some(Object::Reference(xid)) => (
            DictPath {
                root: xid,
                keys: Vec::new(),
            },
            pdfdoc::dict_at(doc, &DictPath {
                root: xid,
                keys: Vec::new(),
            }),
        ),
        Some(Object::Dictionary(_)) => {
            keys.push(b"XObject".to_vec());
            let path = DictPath { root, keys };
            (path.clone(), pdfdoc::dict_at(doc, &path))
        }
        _ => {
            // No /XObject yet: create one holding just this entry. The name
            // only needs to be legal PDF name syntax and unique within the
            // fresh dictionary.
            let name = format!("XObjectFlatten{}_{}", ap_id.0, ap_id.1);
            pdfdoc::update_at(doc, &resources_path, move |dict| {
                let mut xobject = Dictionary::new();
                xobject.set(name.clone(), Object::Reference(ap_id));
                dict.set("XObject", Object::Dictionary(xobject));
            })?;
            return Ok(name);
        }
    };

    let Some(existing) = existing else {
        return Ok(String::new());
    };
    // Reuse the name if this stream is already registered.
    for (key, value) in existing.iter() {
        if value.as_reference().ok() == Some(ap_id) {
            return Ok(String::from_utf8_lossy(key).into_owned());
        }
    }
    let mut counter = 0u32;
    let name = loop {
        let candidate = format!("XObjectFlatten{counter}");
        counter += 1;
        if !existing.has(candidate.as_bytes()) {
            break candidate;
        }
    };
    pdfdoc::update_at(doc, &xpath, move |dict| {
        dict.set(name.clone(), Object::Reference(ap_id));
    })?;
    Ok(name)
}

/// Resolves one reference level, then reads a 4-number rectangle array.
fn rect_array(doc: &Document, object: &Object) -> Option<(f64, f64, f64, f64)> {
    let resolved = match object {
        Object::Reference(id) => doc.dereference(object).ok()?.1,
        other => other,
    };
    rect_array_from(resolved)
}

fn rect_array_from(object: &Object) -> Option<(f64, f64, f64, f64)> {
    let Object::Array(items) = object else {
        return None;
    };
    if items.len() < 4 {
        return None;
    }
    let num = |o: &Object| match o {
        Object::Integer(v) => Some(*v as f64),
        Object::Real(v) => Some(*v as f64),
        _ => None,
    };
    Some((num(&items[0])?, num(&items[1])?, num(&items[2])?, num(&items[3])?))
}
