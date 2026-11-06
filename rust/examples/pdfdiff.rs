//! Structural PDF comparator for Java↔Rust parity checks.
//!
//! `cargo run --example pdfdiff -- a.pdf b.pdf` compares the object-level
//! structure lopdf exposes: page count, per-page boxes/rotation, document
//! metadata, outline and annotation counts, encryption flag. Byte-level
//! equality is not expected (different writers); semantic drift is.

use std::process::ExitCode;

use lopdf::{Document, Object, ObjectId};

fn num(v: &Object) -> String {
    match v {
        Object::Integer(i) => i.to_string(),
        Object::Real(f) => format!("{f:.4}"),
        _ => format!("{v:?}"),
    }
}

fn box_str(doc: &Document, page: ObjectId, key: &[u8]) -> Option<String> {
    let dict = doc.get_object(page).ok()?.as_dict().ok()?;
    let (_, resolved) = doc.dereference(dict.get(key).ok()?).ok()?;
    let arr = resolved.as_array().ok()?;
    Some(arr.iter().map(num).collect::<Vec<_>>().join(","))
}

fn rotate(doc: &Document, page: ObjectId) -> String {
    doc.get_object(page)
        .ok()
        .and_then(|o| o.as_dict().ok())
        .and_then(|d| d.get(b"Rotate").ok())
        .and_then(|v| doc.dereference(v).ok())
        .map(|(_, o)| num(o))
        .unwrap_or_else(|| "0".into())
}

fn annots(doc: &Document, page: ObjectId) -> usize {
    doc.get_object(page)
        .ok()
        .and_then(|o| o.as_dict().ok())
        .and_then(|d| d.get(b"Annots").ok())
        .and_then(|v| doc.dereference(v).ok())
        .and_then(|(_, o)| o.as_array().ok())
        .map_or(0, Vec::len)
}

fn ref_id(v: &Object) -> Option<ObjectId> {
    match v {
        Object::Reference(id) => Some(*id),
        _ => None,
    }
}

fn outline_count(doc: &Document) -> usize {
    let Ok(cat) = doc.catalog() else {
        return 0;
    };
    let Ok((_, outlines)) = cat.get(b"Outlines").and_then(|v| doc.dereference(v)) else {
        return 0;
    };
    let Ok(out_dict) = outlines.as_dict() else {
        return 0;
    };
    let Some(first) = out_dict.get(b"First").ok().and_then(ref_id) else {
        return 0;
    };

    let mut count = 0;
    let mut stack = vec![first];
    while let Some(id) = stack.pop() {
        let Ok(dict) = doc.get_object(id).and_then(|o| o.as_dict().cloned()) else {
            continue;
        };
        count += 1;
        for key in [&b"First"[..], &b"Next"[..]] {
            if let Some(cid) = dict.get(key).ok().and_then(ref_id) {
                stack.push(cid);
            }
        }
    }
    count
}

fn info_keys(doc: &Document) -> Vec<String> {
    let mut keys: Vec<String> = doc
        .trailer
        .get(b"Info")
        .ok()
        .and_then(|v| doc.dereference(v).ok())
        .and_then(|(_, o)| o.as_dict().ok())
        .map(|d| {
            d.iter()
                .map(|(k, _)| String::from_utf8_lossy(k).into_owned())
                .collect()
        })
        .unwrap_or_default();
    keys.sort();
    keys
}

struct Summary {
    pages: usize,
    boxes: Vec<String>,
    info_keys: Vec<String>,
    outlines: usize,
    annots: usize,
    encrypted: bool,
}

fn summarize(path: &str) -> Result<Summary, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let doc = Document::load_mem(&bytes).map_err(|e| format!("{path}: {e}"))?;
    let pages = doc.get_pages();
    let mut boxes = Vec::new();
    let mut total_annots = 0;
    for id in pages.values() {
        boxes.push(format!(
            "media={:?} crop={:?} rot={}",
            box_str(&doc, *id, b"MediaBox"),
            box_str(&doc, *id, b"CropBox"),
            rotate(&doc, *id),
        ));
        total_annots += annots(&doc, *id);
    }
    Ok(Summary {
        pages: pages.len(),
        boxes,
        info_keys: info_keys(&doc),
        outlines: outline_count(&doc),
        annots: total_annots,
        encrypted: doc.trailer.get(b"Encrypt").is_ok(),
    })
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: pdfdiff <a.pdf> <b.pdf>");
        return ExitCode::from(2);
    }
    let (a, b) = match (summarize(&args[1]), summarize(&args[2])) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let mut diffs = 0;
    let mut check = |label: &str, x: &str, y: &str| {
        if x != y {
            diffs += 1;
            println!("DIFF {label}:\n  A: {x}\n  B: {y}");
        }
    };
    check("pages", &a.pages.to_string(), &b.pages.to_string());
    check("outlines", &a.outlines.to_string(), &b.outlines.to_string());
    check("annots", &a.annots.to_string(), &b.annots.to_string());
    check(
        "encrypted",
        &a.encrypted.to_string(),
        &b.encrypted.to_string(),
    );
    check("info-keys", &a.info_keys.join(","), &b.info_keys.join(","));
    for (i, (ba, bb)) in a.boxes.iter().zip(b.boxes.iter()).enumerate() {
        check(&format!("page{}", i + 1), ba, bb);
    }
    if diffs == 0 {
        println!("OK: {} ≡ {}", args[1], args[2]);
        ExitCode::SUCCESS
    } else {
        println!("{diffs} difference(s)");
        ExitCode::FAILURE
    }
}
