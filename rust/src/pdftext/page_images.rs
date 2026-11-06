//! Emits a placeholder per image on a page — port of `PageImages.java`
//! (and the identical private copy inside `PdfMarkdownConverter.java`).
//! Image bytes are deliberately not carried into the Markdown.
//!
//! TODO(jpdfium): `ExtractedImage.bounds()` placement comes from JPDFium's
//! content-stream walk (the `Do` operator's CTM); lopdf has no equivalent
//! here yet, so `bounds` is always `None` and the on-page size/DPI parts
//! of the placeholder are omitted.

use lopdf::{Document, Object, Stream};

use super::markdown_blocks::IMAGE_PLACEHOLDER;
use super::shared::Element;
use crate::pdfdoc;

/// Appends one [`Element`] per image XObject on the page, like
/// `PageImages.emit`. `wrap` turns the placeholder text into the caller's
/// element payload.
pub fn emit<T>(
    doc: &Document,
    page_index: usize,
    page_number: usize,
    sink: &mut Vec<Element<T>>,
    wrap: impl Fn(String) -> T,
) {
    for image in page_images(doc, page_index) {
        sink.push(Element {
            value: wrap(describe(&image)),
            page_start: page_number,
            page_end: page_number,
        });
    }
}

/// The stream behind an `/XObject` entry when it is an image XObject,
/// mirroring `imgextract.rs`'s walk of the page's own resource table.
fn image_stream<'a>(doc: &'a Document, value: &'a Object) -> Option<&'a Stream> {
    let object = match value {
        Object::Reference(id) => doc.get_object(*id).ok()?,
        other => other,
    };
    let Object::Stream(stream) = object else {
        return None;
    };
    let is_image = stream
        .dict
        .get(b"Subtype")
        .ok()
        .and_then(|o| o.as_name().ok())
        .is_some_and(|n| n == b"Image");
    is_image.then_some(stream)
}

fn dict_int(dict: &lopdf::Dictionary, key: &[u8]) -> Option<i64> {
    dict.get(key).ok().and_then(|o| o.as_i64().ok())
}

/// What JPDFium's `ExtractedImage` exposes, minus the content-stream
/// placement documented at the top of this file.
struct ExtractedImage {
    width: i64,
    height: i64,
    bounds: Option<(f32, f32, f32, f32)>,
    extension: Option<String>,
    color_space: Option<String>,
    bits_per_pixel: i64,
}

fn from_stream(stream: &Stream) -> ExtractedImage {
    let dict = &stream.dict;
    let width = dict_int(dict, b"Width").unwrap_or(0);
    let height = dict_int(dict, b"Height").unwrap_or(0);
    let color_space = color_space_name(dict);
    let bpc = dict_int(dict, b"BitsPerComponent").unwrap_or(0);
    ExtractedImage {
        width,
        height,
        bounds: None,
        extension: suggested_extension(stream),
        bits_per_pixel: bpc * color_components(color_space.as_deref()),
        color_space,
    }
}

/// First filter's file extension, like `ExtractedImage.suggestedExtension`.
fn suggested_extension(stream: &Stream) -> Option<String> {
    let filters = stream.filters().unwrap_or_default();
    let ext = match filters.first().copied() {
        Some(f) if f == b"DCTDecode" => "jpg",
        Some(f) if f == b"JPXDecode" => "jp2",
        Some(f) if f == b"CCITTFaxDecode" => "ccitt",
        Some(f) if f == b"JBIG2Decode" => "jb2",
        _ => "png",
    };
    Some(ext.to_string())
}

/// The colour space's leading name: a bare name, or the first element of
/// a colour-space array like `[/Indexed /DeviceRGB ...]`.
fn color_space_name(dict: &lopdf::Dictionary) -> Option<String> {
    let value = dict.get(b"ColorSpace").ok()?;
    color_space_of(value)
}

fn color_space_of(value: &Object) -> Option<String> {
    match value {
        Object::Name(name) => Some(String::from_utf8_lossy(name).into_owned()),
        Object::Array(items) => items.iter().find_map(|item| match item {
            Object::Name(name) => Some(String::from_utf8_lossy(name).into_owned()),
            _ => None,
        }),
        _ => None,
    }
}

fn color_components(color_space: Option<&str>) -> i64 {
    match color_space {
        Some("DeviceRGB") | Some("ICCBased") => 3,
        Some("DeviceCMYK") => 4,
        _ => 1,
    }
}

fn page_images(doc: &Document, page_index: usize) -> Vec<ExtractedImage> {
    let Some(page_id) = pdfdoc::page_ids(doc).get(page_index).copied() else {
        return Vec::new();
    };
    let Some((_holder, resources)) = pdfdoc::page_resources_holder(doc, page_id) else {
        return Vec::new();
    };
    let Some(resources_dict) = pdfdoc::peek_dict(doc, &resources) else {
        return Vec::new();
    };
    let Ok(xobjects) = resources_dict.get(b"XObject") else {
        return Vec::new();
    };
    let Some(xobject_dict) = pdfdoc::peek_dict(doc, xobjects) else {
        return Vec::new();
    };
    xobject_dict
        .iter()
        .filter_map(|(_name, value)| image_stream(doc, value))
        .map(from_stream)
        .collect()
}

/// Image placeholder annotated with whatever metadata is available:
/// pixels, placement, DPI, format, colour space, depth. Missing fields are
/// omitted.
fn describe(img: &ExtractedImage) -> String {
    let mut parts: Vec<String> = Vec::new();
    if img.width > 0 && img.height > 0 {
        parts.push(format!("{}x{}px", img.width, img.height));
    }
    if let Some((_x, _y, w, h)) = img.bounds {
        if w > 0.0 && h > 0.0 {
            parts.push(format!("{w:.0}x{h:.0}pt"));
            if img.width > 0 {
                let dpi_x = img.width as f32 / (w / 72.0);
                let dpi_y = img.height as f32 / (h / 72.0);
                if dpi_x.is_finite() && dpi_x > 0.0 {
                    parts.push(format!("~{:.0}dpi", (dpi_x + dpi_y) / 2.0));
                }
            }
        }
    }
    if let Some(ext) = &img.extension {
        let trimmed = ext.trim_start_matches('.');
        if !trimmed.is_empty() {
            parts.push(trimmed.to_uppercase());
        }
    }
    if let Some(color_space) = &img.color_space {
        parts.push(color_space.clone());
    }
    if img.bits_per_pixel > 0 {
        parts.push(format!("{}bpp", img.bits_per_pixel));
    }

    let mut sb = String::from(IMAGE_PLACEHOLDER);
    if !parts.is_empty() {
        sb.push_str(": ");
        sb.push_str(&parts.join(", "));
    }
    sb.push('>');
    sb
}
