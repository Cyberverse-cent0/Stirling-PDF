use std::collections::BTreeMap;
use std::process::ExitCode;

use lopdf::content::Content;
use lopdf::{decode_text_string, Document, Object, ObjectId};
use serde_json::{json, Map, Value};

fn name(object: &Object) -> Option<String> {
    object
        .as_name()
        .ok()
        .map(|n| String::from_utf8_lossy(n).into_owned())
}

fn scalar(doc: &Document, object: &Object) -> Value {
    let Ok((_, object)) = doc.dereference(object) else {
        return Value::String("<unresolved>".into());
    };
    match object {
        Object::Null => Value::Null,
        Object::Boolean(v) => json!(v),
        Object::Integer(v) => json!(v),
        Object::Real(v) => json!(v),
        Object::Name(_) => name(object).map(Value::String).unwrap_or(Value::Null),
        Object::String(_, _) => decode_text_string(object)
            .map(Value::String)
            .unwrap_or_else(|_| Value::String("<undecodable>".into())),
        Object::Reference(id) => json!(format!("{} {} R", id.0, id.1)),
        other => Value::String(format!("{other:?}")),
    }
}

fn box_value(doc: &Document, page_id: ObjectId, key: &[u8]) -> Value {
    let mut current = page_id;
    let mut visited = std::collections::HashSet::new();
    loop {
        if !visited.insert(current) {
            return Value::String("<cycle>".into());
        }
        let Ok(dict) = doc.get_dictionary(current) else {
            return Value::Null;
        };
        if let Ok(value) = dict.get(key) {
            if let Ok((_, resolved)) = doc.dereference(value) {
                if !matches!(resolved, Object::Null) {
                    return scalar(doc, value);
                }
            }
        }
        match dict.get(b"Parent") {
            Err(_) | Ok(Object::Null) => return Value::Null,
            Ok(parent) => match parent.as_reference() {
                Ok(id) => current = id,
                Err(_) => return Value::Null,
            },
        }
    }
}

fn rotate_value(doc: &Document, page_id: ObjectId) -> Value {
    let mut current = page_id;
    let mut visited = std::collections::HashSet::new();
    loop {
        if !visited.insert(current) {
            return Value::Null;
        }
        let Ok(dict) = doc.get_dictionary(current) else {
            return Value::Null;
        };
        if let Ok(value) = dict.get(b"Rotate") {
            if let Ok(deg) = value.as_i64() {
                return json!(deg.rem_euclid(360));
            }
        }
        match dict.get(b"Parent") {
            Err(_) | Ok(Object::Null) => return json!(0),
            Ok(parent) => match parent.as_reference() {
                Ok(id) => current = id,
                Err(_) => return json!(0),
            },
        }
    }
}

fn annotations(doc: &Document, page_id: ObjectId) -> Value {
    let Ok(page) = doc.get_dictionary(page_id) else {
        return json!({"count": 0, "subtypes": {}});
    };
    let Ok(annots) = page.get(b"Annots") else {
        return json!({"count": 0, "subtypes": {}});
    };
    let Ok((_, resolved)) = doc.dereference(annots) else {
        return json!({"count": 0, "subtypes": {}});
    };
    let Object::Array(items) = resolved else {
        return json!({"count": 0, "subtypes": {}});
    };
    let mut subtypes = BTreeMap::<String, u64>::new();
    let mut count = 0u64;
    for item in items {
        let Ok((_, object)) = doc.dereference(item) else {
            continue;
        };
        let Object::Dictionary(dict) = object else {
            continue;
        };
        count += 1;
        let subtype = dict
            .get(b"Subtype")
            .ok()
            .and_then(name)
            .unwrap_or_else(|| "Unknown".into());
        *subtypes.entry(subtype).or_insert(0) += 1;
    }
    json!({"count": count, "subtypes": subtypes})
}

fn fonts(doc: &Document, page_id: ObjectId) -> Vec<String> {
    let Ok(fonts) = doc.get_page_fonts(page_id) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for (_, font) in fonts {
        if let Ok(object) = font.get(b"BaseFont") {
            if let Some(name) = name(object) {
                names.push(name);
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

fn fnv1a(bytes: &[u8]) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn content(doc: &Document, page_id: ObjectId) -> Value {
    let streams = doc.get_page_contents(page_id).len();
    let bytes = doc
        .get_page_content_with_limit(page_id, 64 * 1024 * 1024)
        .unwrap_or_default();
    match Content::decode(&bytes) {
        Ok(content) => {
            let mut operators = BTreeMap::<String, u64>::new();
            let mut operands = 0u64;
            for operation in &content.operations {
                *operators.entry(operation.operator.clone()).or_insert(0) += 1;
                operands += operation.operands.len() as u64;
            }
            json!({
                "streams": streams,
                "decodedBytes": bytes.len(),
                "operations": content.operations.len(),
                "operands": operands,
                "operators": operators,
                "fnv1a64": fnv1a(&bytes),
            })
        }
        Err(_) => json!({
            "streams": streams,
            "decodedBytes": bytes.len(),
            "decodeError": true,
            "fnv1a64": fnv1a(&bytes),
        }),
    }
}

fn page_fingerprint(doc: &Document, page_id: ObjectId, index: usize) -> Value {
    json!({
        "index": index,
        "id": [page_id.0, page_id.1],
        "mediaBox": box_value(doc, page_id, b"MediaBox"),
        "cropBox": box_value(doc, page_id, b"CropBox"),
        "rotate": rotate_value(doc, page_id),
        "annots": annotations(doc, page_id),
        "fonts": fonts(doc, page_id),
        "contents": content(doc, page_id),
    })
}

fn object_counts(doc: &Document) -> Value {
    let mut by_type = BTreeMap::<String, u64>::new();
    let mut stream_subtypes = BTreeMap::<String, u64>::new();
    let mut stream_filters = BTreeMap::<String, u64>::new();
    let mut streams = 0u64;

    for object in doc.objects.values() {
        let kind = match object {
            Object::Null => "null",
            Object::Boolean(_) => "boolean",
            Object::Integer(_) => "integer",
            Object::Real(_) => "real",
            Object::Name(_) => "name",
            Object::String(_, _) => "string",
            Object::Array(_) => "array",
            Object::Dictionary(_) => "dictionary",
            Object::Stream(_) => "stream",
            Object::Reference(_) => "reference",
        };
        *by_type.entry(kind.to_string()).or_insert(0) += 1;

        if let Object::Stream(stream) = object {
            streams += 1;
            let subtype = stream
                .dict
                .get(b"Subtype")
                .ok()
                .and_then(name)
                .unwrap_or_else(|| "Unknown".into());
            *stream_subtypes.entry(subtype).or_insert(0) += 1;
            if let Ok(filter) = stream.dict.get(b"Filter") {
                match filter {
                    Object::Name(_) => {
                        if let Some(filter) = name(filter) {
                            *stream_filters.entry(filter).or_insert(0) += 1;
                        }
                    }
                    Object::Array(filters) => {
                        for filter in filters {
                            if let Some(filter) = name(filter) {
                                *stream_filters.entry(filter).or_insert(0) += 1;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    json!({
        "total": doc.objects.len(),
        "byType": by_type,
        "streams": streams,
        "streamSubtypes": stream_subtypes,
        "streamFilters": stream_filters,
        "maxId": doc.max_id,
        "xrefRecovery": doc.xref_recovery.map(|r| format!("{r:?}")),
    })
}

fn info(doc: &Document) -> Value {
    let Some(info) = doc.trailer.get(b"Info").ok() else {
        return Value::Null;
    };
    let Ok((_, object)) = doc.dereference(info) else {
        return Value::Null;
    };
    let Object::Dictionary(dict) = object else {
        return Value::Null;
    };
    let mut out = Map::new();
    for (key, value) in dict.iter() {
        out.insert(
            String::from_utf8_lossy(key).into_owned(),
            scalar(doc, value),
        );
    }
    Value::Object(out)
}

fn catalog(doc: &Document) -> Value {
    let Ok(catalog) = doc.catalog() else {
        return Value::Null;
    };
    let keys: Vec<String> = catalog
        .iter()
        .map(|(key, _)| String::from_utf8_lossy(key).into_owned())
        .collect();
    json!({
        "keys": keys,
        "hasAcroForm": catalog.has(b"AcroForm"),
        "hasOutlines": catalog.has(b"Outlines"),
        "hasNames": catalog.has(b"Names"),
        "hasMetadata": catalog.has(b"Metadata"),
        "hasPieceInfo": catalog.has(b"PieceInfo"),
    })
}

fn fingerprint(path: &str) -> Value {
    let doc = match Document::load(path) {
        Ok(doc) => doc,
        Err(error) => return json!({"ok": false, "error": error.to_string()}),
    };
    let pages = doc.get_pages();
    let page_ids: Vec<ObjectId> = pages.into_values().collect();
    let pages_json: Vec<Value> = page_ids
        .iter()
        .enumerate()
        .map(|(index, id)| page_fingerprint(&doc, *id, index + 1))
        .collect();

    json!({
        "ok": true,
        "version": doc.version,
        "encrypted": doc.trailer.has(b"Encrypt"),
        "wasEncrypted": doc.was_encrypted(),
        "pageCount": page_ids.len(),
        "info": info(&doc),
        "catalog": catalog(&doc),
        "pages": pages_json,
        "objects": object_counts(&doc),
    })
}

fn main() -> ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: pdf-fingerprint <pdf>");
        return ExitCode::from(2);
    };
    let fingerprint = fingerprint(&path);
    println!(
        "{}",
        serde_json::to_string(&fingerprint).unwrap_or_else(|_| "{}".into())
    );
    if fingerprint.get("ok").and_then(Value::as_bool) == Some(true) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
