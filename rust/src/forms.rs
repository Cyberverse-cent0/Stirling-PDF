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
    let catalog = doc
        .catalog()
        .map_err(|e| crate::error::AppError::Pdf(e.to_string()))?;
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
    if dict.get(b"Subtype").ok().and_then(|o| o.as_name().ok()) == Some(b"Widget") {
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

/// Port of `RemoveCertSignController` (`/api/v1/security/remove-cert-sign`):
/// only root-level signature fields — `/Fields` entries whose dictionary
/// declares `/FT /Sig` — go through the same widget-appearance flatten as
/// `flatten()`, then they are removed from `/Fields`. `/XFA` is dropped
/// unconditionally (as `PDAcroForm.flatten` does) and `/SigFlags` only once
/// no signature field with a `/V` dictionary remains in the field tree,
/// matching `PDDocument.getSignatureDictionaries()`.
pub fn remove_signature_fields(doc: &mut Document) -> AppResult<()> {
    let catalog = doc
        .catalog()
        .map_err(|e| crate::error::AppError::Pdf(e.to_string()))?;
    let Some(acro_slot) = catalog.get(b"AcroForm").ok().cloned() else {
        return Ok(());
    };
    let Some(mut acro) = pdfdoc::peek_dict(doc, &acro_slot).cloned() else {
        return Ok(());
    };
    let Some(fields_obj) = acro.get(b"Fields").ok().cloned() else {
        return Ok(());
    };
    let (_, resolved) = doc.dereference(&fields_obj).map_err(pdf_err)?;
    let Object::Array(items) = resolved else {
        return Ok(());
    };
    let items = items.clone();

    let mut widgets = HashSet::new();
    let mut kept = Vec::with_capacity(items.len());
    let mut found = false;
    for item in items {
        let is_sig = pdfdoc::dict_owned(doc, &item)?.is_some_and(|d| is_sig_field(doc, &d));
        if !is_sig {
            kept.push(item);
            continue;
        }
        found = true;
        collect_sig_widgets(doc, &item, &mut widgets)?;
    }
    if !found {
        return Ok(());
    }

    for page_id in pdfdoc::page_ids(doc) {
        flatten_page(doc, page_id, &widgets)?;
    }

    // PDFBox removes each field object from the array in place; keep the
    // array indirect when it already is one.
    match &fields_obj {
        Object::Reference(id) => {
            if let Some(Object::Array(arr)) = doc.objects.get_mut(id) {
                *arr = kept;
            }
        }
        _ => {
            acro.set("Fields", Object::Array(kept));
        }
    }
    acro.remove(b"XFA");
    if !signature_dict_remains(doc, &acro)? {
        acro.remove(b"SigFlags");
    }
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

/// `/FT` resolving to the `Sig` name, like `PDField`'s signature check.
fn is_sig_field(doc: &Document, dict: &Dictionary) -> bool {
    dict.get(b"FT")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| o.as_name().ok())
        .is_some_and(|n| n == b"Sig")
}

/// Widget object ids of one signature field, per
/// `PDTerminalField.getWidgets()`: every `/Kids` dictionary when `/Kids` is
/// a non-empty array, the field itself otherwise (including when `/Kids`
/// is absent or not an array). Only indirect objects can match page
/// `/Annots` entries, so direct dictionaries contribute nothing — the same
/// identity limitation `flatten()` already documents.
fn collect_sig_widgets(
    doc: &Document,
    field: &Object,
    widgets: &mut HashSet<ObjectId>,
) -> AppResult<()> {
    let Some(dict) = pdfdoc::dict_owned(doc, field)? else {
        return Ok(());
    };
    let kids = dict
        .get(b"Kids")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| o.as_array().ok().cloned());
    match kids {
        Some(kids) if !kids.is_empty() => {
            for kid in kids {
                if let Ok(id) = kid.as_reference() {
                    widgets.insert(id);
                }
            }
        }
        Some(_) => {}
        None => {
            if let Ok(id) = field.as_reference() {
                widgets.insert(id);
            }
        }
    }
    Ok(())
}

/// Whether any signature field left in the tree still carries a `/V`
/// signature dictionary — the `getSignatureDictionaries()` check that
/// decides `/SigFlags` removal.
fn signature_dict_remains(doc: &Document, acro: &Dictionary) -> AppResult<bool> {
    let mut visited = HashSet::new();
    let Ok(fields) = acro.get(b"Fields") else {
        return Ok(false);
    };
    let (_, resolved) = doc.dereference(fields).map_err(pdf_err)?;
    let Object::Array(items) = resolved else {
        return Ok(false);
    };
    for item in items {
        let Some(dict) = pdfdoc::dict_owned(doc, item)? else {
            continue;
        };
        if let Ok(id) = item.as_reference() {
            if !visited.insert(id) {
                continue;
            }
        }
        if signature_dict_in_field(doc, &dict, &mut visited, 0)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn signature_dict_in_field(
    doc: &Document,
    dict: &Dictionary,
    visited: &mut HashSet<ObjectId>,
    depth: usize,
) -> AppResult<bool> {
    if depth > 100 {
        return Ok(false);
    }
    if is_sig_field(doc, dict)
        && dict
            .get(b"V")
            .ok()
            .and_then(|v| doc.dereference(v).ok())
            .is_some_and(|(_, o)| matches!(o, Object::Dictionary(_)))
    {
        return Ok(true);
    }
    let Ok(kids) = dict.get(b"Kids") else {
        return Ok(false);
    };
    let (_, resolved) = doc.dereference(kids).map_err(pdf_err)?;
    let Object::Array(items) = resolved else {
        return Ok(false);
    };
    for kid in items {
        let Some(kid_dict) = pdfdoc::dict_owned(doc, kid)? else {
            continue;
        };
        if let Ok(id) = kid.as_reference() {
            if !visited.insert(id) {
                continue;
            }
        }
        if signature_dict_in_field(doc, &kid_dict, visited, depth + 1)? {
            return Ok(true);
        }
    }
    Ok(false)
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
    let page = doc.get_dictionary(page_id).map_err(pdf_err)?.clone();
    let Some(annots_obj) = page.get(b"Annots").ok().cloned() else {
        return Ok(());
    };
    let (_, resolved) = doc.dereference(&annots_obj).map_err(pdf_err)?;
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
    let flags = dict
        .get(b"F")
        .ok()
        .and_then(|o| o.as_i64().ok())
        .unwrap_or(0);
    if flags & 0b11 != 0 {
        return Ok(None);
    }

    let Some((x0, y0, x1, y1)) = dict.get(b"Rect").ok().and_then(|r| rect_array(doc, r)) else {
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

    let (mut min_x, mut min_y, mut max_x, mut max_y) = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
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
            if let Some(stream) = doc.get_object(*id).ok().and_then(|o| match o {
                Object::Stream(s) => Some(s.dict.clone()),
                _ => None,
            }) {
                return Some((*id, stream));
            }
            None
        }
        Object::Dictionary(dict) => {
            let state = widget.get(b"AS").ok()?.as_name().ok()?;
            let entry = dict.get(state).ok()?;
            let id = entry.as_reference().ok()?;
            let stream = doc.get_object(id).ok().and_then(|o| match o {
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
    // Pages without /Resources get one created, like PDFBox's content
    // stream does when drawing into a resource-less page.
    let Some((holder, resources)) = pdfdoc::page_resources_holder(doc, page_id) else {
        let name = format!("XObjectFlatten{}_{}", ap_id.0, ap_id.1);
        let page_dict = doc.get_dictionary_mut(page_id).map_err(pdf_err)?;
        let mut xobject = Dictionary::new();
        xobject.set(name.clone(), Object::Reference(ap_id));
        let mut resources_dict = Dictionary::new();
        resources_dict.set("XObject", Object::Dictionary(xobject));
        page_dict.set("Resources", Object::Dictionary(resources_dict));
        return Ok(name);
    };

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
            pdfdoc::dict_at(
                doc,
                &DictPath {
                    root: xid,
                    keys: Vec::new(),
                },
            ),
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
            let entry_name = name.clone();
            pdfdoc::update_at(doc, &resources_path, move |dict| {
                let mut xobject = Dictionary::new();
                xobject.set(entry_name.clone(), Object::Reference(ap_id));
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
    let name_for_closure = name.clone();
    pdfdoc::update_at(doc, &xpath, move |dict| {
        dict.set(name_for_closure.clone(), Object::Reference(ap_id));
    })?;
    Ok(name)
}

/// Resolves one reference level, then reads a 4-number rectangle array.
fn rect_array(doc: &Document, object: &Object) -> Option<(f64, f64, f64, f64)> {
    let resolved = match object {
        Object::Reference(_id) => doc.dereference(object).ok()?.1,
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
    Some((
        num(&items[0])?,
        num(&items[1])?,
        num(&items[2])?,
        num(&items[3])?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit;
    use lopdf::dictionary;

    /// Page + AcroForm with one text field whose widget has a normal
    /// appearance stream.
    pub(super) fn form_pdf() -> testkit::Pdf {
        let mut pdf = testkit::Pdf::new();
        let appearance = pdf.add_stream(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Form",
                "BBox" => Object::Array(vec![0.into(), 0.into(), 50.into(), 10.into()]),
                "Resources" => dictionary! {},
            },
            b"0 0 1 rg 0 0 50 10 re f",
        );
        let widget = pdf.add(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Widget",
            "FT" => "Tx",
            "T" => "field1",
            "Rect" => Object::Array(vec![10.into(), 20.into(), 110.into(), 40.into()]),
            "F" => 0,
            "P" => Object::Reference(pdf.page),
            "AP" => dictionary! {
                "N" => Object::Reference(appearance),
            },
        });
        let field = pdf.add(dictionary! {
            "FT" => "Tx",
            "T" => "field1",
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
        pdf
    }

    #[test]
    fn draws_appearance_and_removes_widget_and_fields() {
        let mut pdf = form_pdf();
        let bytes = pdf.bytes();
        let mut doc = Document::load_mem(&bytes).unwrap();
        let acro_id = {
            let catalog = doc.catalog().unwrap();
            catalog.get(b"AcroForm").unwrap().as_reference().unwrap()
        };

        flatten(&mut doc).unwrap();

        // Object ids can shift across save/load, so re-resolve the page.
        let page_id = *doc.get_pages().values().next().unwrap();
        let page = doc.get_dictionary(page_id).unwrap();
        let annots = page.get(b"Annots").unwrap().as_array().unwrap();
        assert!(annots.is_empty());

        // A new content stream carries the Do operator; /Contents may be a
        // bare stream reference or an array of streams.
        let (_, contents) = doc.dereference(page.get(b"Contents").unwrap()).unwrap();
        let stream_objs: Vec<&Object> = match contents {
            Object::Array(arr) => arr.iter().collect(),
            other => vec![other],
        };
        let mut drew = false;
        for stream in stream_objs {
            if let Ok((_, Object::Stream(s))) = doc.dereference(stream) {
                let text = String::from_utf8_lossy(&s.content);
                if text.contains(" Do") {
                    drew = true;
                    assert!(text.contains(" cm"));
                    assert!(text.contains('q'));
                }
            }
        }
        assert!(drew, "no appearance stream was drawn");

        // Field tree emptied, /XFA and /SigFlags gone.
        let acro = doc.get_dictionary(acro_id).unwrap();
        let fields = acro.get(b"Fields").unwrap().as_array().unwrap();
        assert!(fields.is_empty());
    }

    #[test]
    fn leaves_documents_without_acroform_untouched() {
        let mut pdf = testkit::Pdf::new();
        let bytes = pdf.bytes();
        let mut doc = Document::load_mem(&bytes).unwrap();
        flatten(&mut doc).unwrap();
        assert_eq!(doc.get_pages().len(), 1);
        assert!(doc.catalog().unwrap().get(b"AcroForm").is_err());
    }
}

#[cfg(test)]
mod more_tests {
    use super::tests::*;
    use super::*;
    use crate::testkit;
    use lopdf::dictionary;

    #[test]
    fn widget_without_appearance_is_removed_not_drawn() {
        let mut pdf = testkit::Pdf::new();
        let widget = pdf.add(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Widget",
            "FT" => "Btn",
            "T" => "check1",
            "Rect" => Object::Array(vec![0.into(), 0.into(), 10.into(), 10.into()]),
            "F" => 0,
            "P" => Object::Reference(pdf.page),
        });
        let field = pdf.add(dictionary! {
            "FT" => "Btn",
            "T" => "check1",
            "Kids" => vec![Object::Reference(widget)],
        });
        let acro = pdf.add(dictionary! {
            "Fields" => vec![Object::Reference(field)],
        });
        pdf.edit_catalog(|c| c.set("AcroForm", Object::Reference(acro)));
        pdf.edit_page(|p| p.set("Annots", vec![Object::Reference(widget)]));
        let bytes = pdf.bytes();
        let mut doc = Document::load_mem(&bytes).unwrap();
        flatten(&mut doc).unwrap();
        let page_id = *doc.get_pages().values().next().unwrap();
        let page = doc.get_dictionary(page_id).unwrap();
        assert!(page.get(b"Annots").unwrap().as_array().unwrap().is_empty());
        // No appearance → no new content stream was appended.
        assert!(page.get(b"Contents").is_err());
    }

    #[test]
    fn unrelated_annotations_are_preserved() {
        let mut pdf = form_pdf();
        // A free-text annotation that is not a widget.
        let note = pdf.add(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Text",
            "Rect" => Object::Array(vec![0.into(), 0.into(), 5.into(), 5.into()]),
            "Contents" => Object::string_literal(b"note".to_vec()),
        });
        pdf.edit_page(|p| {
            let mut annots = p.get(b"Annots").unwrap().as_array().unwrap().clone();
            annots.push(Object::Reference(note));
            p.set("Annots", Object::Array(annots));
        });
        let bytes = pdf.bytes();
        let mut doc = Document::load_mem(&bytes).unwrap();
        flatten(&mut doc).unwrap();
        let page_id = *doc.get_pages().values().next().unwrap();
        let page = doc.get_dictionary(page_id).unwrap();
        let annots = page.get(b"Annots").unwrap().as_array().unwrap();
        assert_eq!(annots.len(), 1);
        assert_eq!(annots[0].as_reference().unwrap(), note);
    }

    /// PDF with a merged signature field-widget on the page plus a regular
    /// text field, an `/XFA` entry and `/SigFlags` on the AcroForm.
    fn signed_pdf() -> (testkit::Pdf, ObjectId, ObjectId, ObjectId) {
        let mut pdf = testkit::Pdf::new();
        let sig_dict = pdf.add(dictionary! {
            "Type" => "Sig",
            "Filter" => "Adobe.PPKLite",
            "SubFilter" => "adbe.pkcs7.detached",
            "Contents" => Object::string_literal("c2ln"),
            "ByteRange" => Object::Array(vec![0.into(), 100.into(), 200.into(), 100.into()]),
        });
        let sig_field = pdf.add(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Widget",
            "FT" => "Sig",
            "T" => "Signature1",
            "Rect" => Object::Array(vec![0.into(), 0.into(), 100.into(), 20.into()]),
            "P" => Object::Reference(pdf.page),
            "V" => Object::Reference(sig_dict),
        });
        let text_widget = pdf.add(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Widget",
            "Rect" => Object::Array(vec![0.into(), 30.into(), 100.into(), 50.into()]),
            "P" => Object::Reference(pdf.page),
        });
        let text_field = pdf.add(dictionary! {
            "FT" => "Tx",
            "T" => "name",
            "Kids" => vec![Object::Reference(text_widget)],
        });
        let acro = pdf.add(dictionary! {
            "Fields" => vec![Object::Reference(sig_field), Object::Reference(text_field)],
            "XFA" => Object::Array(vec![Object::string_literal("preamble")]),
            "SigFlags" => 1,
        });
        pdf.edit_catalog(|c| c.set("AcroForm", Object::Reference(acro)));
        pdf.edit_page(|p| {
            p.set(
                "Annots",
                vec![Object::Reference(sig_field), Object::Reference(text_widget)],
            );
        });
        (pdf, acro, sig_field, text_field)
    }

    #[test]
    fn remove_cert_sign_drops_root_signature_field() {
        let (mut pdf, acro_id, sig_field, text_field) = signed_pdf();
        let bytes = pdf.bytes();
        let mut doc = Document::load_mem(&bytes).unwrap();

        remove_signature_fields(&mut doc).unwrap();

        let acro = doc.get_dictionary(acro_id).unwrap();
        let fields = acro.get(b"Fields").unwrap().as_array().unwrap();
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].as_reference().unwrap(), text_field);
        assert!(acro.get(b"XFA").is_err());
        // The only signature dictionary went away with its field.
        assert!(acro.get(b"SigFlags").is_err());

        let page_id = *doc.get_pages().values().next().unwrap();
        let annots = doc
            .get_dictionary(page_id)
            .unwrap()
            .get(b"Annots")
            .unwrap()
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(annots.len(), 1);
        assert_ne!(annots[0].as_reference().unwrap(), sig_field);
        // The signature field object itself is orphaned but still present,
        // like PDFBox leaving the dereferenced COS objects in memory only —
        // on save it simply becomes unreachable.
    }

    #[test]
    fn remove_cert_sign_keeps_sigflags_when_nested_sig_remains() {
        let mut pdf = testkit::Pdf::new();
        // Nested signature field under a non-terminal parent: not a root
        // field, so Java's getFields() filter never selects it.
        let nested_sig = pdf.add(dictionary! {
            "FT" => "Sig",
            "T" => "nested",
            "V" => Object::Dictionary(dictionary! { "Type" => "Sig", "Contents" => Object::string_literal("eA==") }),
        });
        let parent = pdf.add(dictionary! {
            "T" => "parent",
            "Kids" => vec![Object::Reference(nested_sig)],
        });
        let root_sig = pdf.add(dictionary! {
            "FT" => "Sig",
            "T" => "root",
            "V" => Object::Dictionary(dictionary! { "Type" => "Sig" }),
        });
        let acro = pdf.add(dictionary! {
            "Fields" => vec![Object::Reference(parent), Object::Reference(root_sig)],
            "SigFlags" => 1,
        });
        pdf.edit_catalog(|c| c.set("AcroForm", Object::Reference(acro)));
        let bytes = pdf.bytes();
        let mut doc = Document::load_mem(&bytes).unwrap();

        remove_signature_fields(&mut doc).unwrap();

        let acro_dict = doc.get_dictionary(acro).unwrap();
        let fields = acro_dict.get(b"Fields").unwrap().as_array().unwrap();
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].as_reference().unwrap(), parent);
        // The nested /V signature dictionary keeps /SigFlags alive.
        assert_eq!(acro_dict.get(b"SigFlags").unwrap().as_i64().unwrap(), 1);
    }

    #[test]
    fn remove_cert_sign_without_signatures_is_a_no_op() {
        let mut pdf = form_pdf();
        let bytes = pdf.bytes();
        let mut doc = Document::load_mem(&bytes).unwrap();
        remove_signature_fields(&mut doc).unwrap();
        // The text field survives in /Fields and its widget stays on the page.
        let acro_id = {
            let catalog = doc.catalog().unwrap();
            catalog.get(b"AcroForm").unwrap().as_reference().unwrap()
        };
        let acro = doc.get_dictionary(acro_id).unwrap();
        assert_eq!(acro.get(b"Fields").unwrap().as_array().unwrap().len(), 1);
        let page_id = *doc.get_pages().values().next().unwrap();
        assert_eq!(
            doc.get_dictionary(page_id)
                .unwrap()
                .get(b"Annots")
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
}
