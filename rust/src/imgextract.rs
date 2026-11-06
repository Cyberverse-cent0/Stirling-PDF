//! Port of `ExtractImagesController` (`POST /api/v1/misc/extract-images`):
//! walks every page's `/Resources` `/XObject` table, decodes each image
//! XObject once per unique content and writes the rasters to a ZIP named
//! `<base>_extracted-images.zip` with entries `<base>_page_<n>_<i>.<format>`.
//!
//! Like Java, only the page's own resource `/XObject` table is consulted —
//! images nested inside Form XObjects are not extracted by either
//! implementation (`PDResources.getXObjectNames` + `isImageXObject` sees
//! only direct entries).
//!
//! Decode coverage is `imgcomp::decode_image`'s subset — DCTDecode JPEGs
//! (passed through byte-identical for `format=jpeg`) and 8-bit
//! DeviceGray/DeviceRGB/DeviceCMYK raw-sample images, with `/SMask`
//! composited as alpha for PNG output. JPXDecode, CCITTFaxDecode,
//! JBIG2Decode, image masks, Indexed/ICCBased/Separation colour spaces,
//! `/Decode` remapping, colour-key `/Mask` and non-matching soft masks are
//! outside the subset: [`supported`] detects them up front and the handler
//! forwards the whole request to Java, which owns PDFBox/ImageIO's full
//! coverage, rather than emitting a ZIP silently missing entries.
//! `format` accepts the `image` crate's codec set (png, jpeg/jpg, tiff,
//! bmp, webp, gif); anything else is rejected by the handler. Non-PNG
//! formats drop `/SMask` alpha and emit RGB, matching Java's
//! `TYPE_INT_RGB` target for non-PNG writers.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::Hasher;
use std::io::Write;

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, GrayImage, ImageEncoder, RgbImage, RgbaImage};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::error::{AppError, AppResult};
use crate::imgcomp::{self, Pixels};
use crate::pdfdoc;
use crate::upload::remove_extension;

/// Bounds one fingerprint walk, matching Java's `MAX_KEY_DEPTH`/
/// `MAX_KEY_NODES`: an indirect object expands at most once per walk and the
/// whole walk is capped so a crafted object graph cannot fan out.
const MAX_KEY_DEPTH: usize = 32;
const MAX_KEY_NODES: usize = 4096;

/// Stream-dictionary keys that can change the decoded pixels; everything
/// else (metadata, optional content) is ignored so it cannot defeat dedup.
const DECODE_RELEVANT_IMAGE_KEYS: &[&[u8]] = &[
    b"BitsPerComponent",
    b"ColorSpace",
    b"Decode",
    b"DecodeParms",
    b"Filter",
    b"Height",
    b"ImageMask",
    b"Mask",
    b"Matte",
    b"SMask",
    b"SMaskInData",
    b"Width",
];

/// `true` when every image XObject in the document decodes inside
/// [`imgcomp::decode_image`]'s subset (plus the `/SMask` rules below), so
/// [`extract`] can produce the same ZIP entries Java would. `false` means
/// the request must go to Java — a partial ZIP would silently drop images
/// PDFBox extracts fine.
pub fn supported(pdf_name: &str, bytes: &[u8]) -> AppResult<bool> {
    let doc = pdfdoc::load(pdf_name, bytes)?;
    for page_id in pdfdoc::page_ids(&doc) {
        let Some((_holder, resources)) = pdfdoc::page_resources_holder(&doc, page_id) else {
            continue;
        };
        let Some(resources_dict) = pdfdoc::peek_dict(&doc, &resources) else {
            continue;
        };
        let Ok(xobjects) = resources_dict.get(b"XObject") else {
            continue;
        };
        let Some(xobject_dict) = pdfdoc::peek_dict(&doc, xobjects) else {
            continue;
        };
        for (_name, value) in xobject_dict.iter() {
            let Some(stream) = image_stream(&doc, value) else {
                continue;
            };
            if !image_supported(&doc, stream) {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Extracts every image XObject into a deflated ZIP, like
/// `ExtractImagesController.extractImages`. `format` is the raw request
/// value; it doubles as the entry extension exactly as Java uses it.
/// Callers must run [`supported`] first — an image that fails to decode
/// here is skipped with a warning, matching nothing Java would produce.
pub fn extract(pdf_name: &str, bytes: &[u8], format: &str) -> AppResult<Vec<u8>> {
    let doc = pdfdoc::load(pdf_name, bytes)?;
    let base = remove_extension(Some(pdf_name));

    let mut zip = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    // Java's ZipOutputStream runs at Deflater.BEST_COMPRESSION.
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .compression_level(Some(9));
    let mut fingerprints = Fingerprints::default();

    for (page_index, page_id) in pdfdoc::page_ids(&doc).iter().enumerate() {
        let page_number = page_index + 1;
        let Some((_holder, resources)) = pdfdoc::page_resources_holder(&doc, *page_id) else {
            continue;
        };
        let Some(resources_dict) = pdfdoc::peek_dict(&doc, &resources) else {
            continue;
        };
        let Ok(xobjects) = resources_dict.get(b"XObject") else {
            continue;
        };
        let Some(xobject_dict) = pdfdoc::peek_dict(&doc, xobjects) else {
            continue;
        };

        let mut image_count = 1usize;
        for (_name, value) in xobject_dict.iter() {
            let Some(stream) = image_stream(&doc, value) else {
                continue;
            };
            if !fingerprints.is_first_occurrence(&doc, value, stream) {
                continue;
            }
            let Some(encoded) = render_image(&doc, stream, format) else {
                tracing::warn!("skipping embedded image outside the supported decode subset");
                continue;
            };
            let entry = format!("{base}_page_{page_number}_{}.{format}", image_count);
            image_count += 1;
            zip.start_file(entry, options)
                .and_then(|()| zip.write_all(&encoded).map_err(Into::into))
                .map_err(|e| AppError::Internal(anyhow::anyhow!("zip: {e}")))?;
        }
    }

    let cursor = zip
        .finish()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("zip: {e}")))?;
    Ok(cursor.into_inner())
}

/// The stream behind an `/XObject` entry when it is an image XObject.
/// Streams are always indirect, but a direct dictionary is tolerated.
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

/// The stream behind an arbitrary indirect (or direct) object — used for
/// `/SMask` entries, which are image XObjects without a resources hop.
fn resolve_stream<'a>(doc: &'a Document, value: &'a Object) -> Option<&'a Stream> {
    let object = match value {
        Object::Reference(id) => doc.get_object(*id).ok()?,
        other => other,
    };
    match object {
        Object::Stream(stream) => Some(stream),
        _ => None,
    }
}

fn dict_int(dict: &Dictionary, key: &[u8]) -> Option<i64> {
    dict.get(key).ok().and_then(|o| o.as_i64().ok())
}

/// Whether one image (and its `/SMask` chain) stays inside the decode
/// subset. `/Decode` remapping and `/Mask` (colour-key or explicit) change
/// pixels `decode_image` does not model, `/Matte` alters preblended colour,
/// and a soft mask must itself decode at the base image's dimensions —
/// anything else sends the request to Java.
fn image_supported(doc: &Document, stream: &Stream) -> bool {
    let dict = &stream.dict;
    if dict.get(b"Decode").is_ok() || dict.get(b"Mask").is_ok() || dict.get(b"Matte").is_ok() {
        return false;
    }
    if !imgcomp::decodable(stream) {
        return false;
    }
    // `/DecodeParms` on a DCT stream can carry `/ColorTransform`, which
    // neither the JPEG decoder nor the passthrough models — Java owns it.
    let filters = stream.filters().unwrap_or_default();
    if filters.len() == 1 && filters[0] == b"DCTDecode" && dict.get(b"DecodeParms").is_ok() {
        return false;
    }
    let (Some(width), Some(height)) = (dict_int(dict, b"Width"), dict_int(dict, b"Height")) else {
        return false;
    };
    let mut smask = dict.get(b"SMask").ok().cloned();
    for _ in 0..4 {
        let Some(value) = smask else { return true };
        // `/SMask /None` explicitly disables masking.
        if value.as_name().ok().is_some_and(|n| n == b"None") {
            return true;
        }
        let Some(mask) = resolve_stream(doc, &value) else {
            return false;
        };
        let md = &mask.dict;
        if md.get(b"Decode").is_ok() || md.get(b"Mask").is_ok() || md.get(b"Matte").is_ok() {
            return false;
        }
        if !imgcomp::decodable(mask)
            || dict_int(md, b"Width") != Some(width)
            || dict_int(md, b"Height") != Some(height)
        {
            return false;
        }
        smask = md.get(b"SMask").ok().cloned();
    }
    false
}

/// `DCTDecode` payloads are already JPEG files; for `format=jpeg` with no
/// decode-relevant extras the original compressed bytes are the most
/// faithful extraction possible — no decode/re-encode generation loss.
/// PDFBox skips one leading line feed, so do the same.
fn passthrough_jpeg(stream: &Stream) -> Option<Vec<u8>> {
    let filters = stream.filters().unwrap_or_default();
    if !(filters.len() == 1 && filters[0] == b"DCTDecode") {
        return None;
    }
    let dict = &stream.dict;
    // `/DecodeParms` can carry `/ColorTransform`, which changes how the
    // JPEG's samples are interpreted; passthrough would bypass it.
    if dict.get(b"Decode").is_ok()
        || dict.get(b"DecodeParms").is_ok()
        || dict.get(b"SMask").is_ok()
        || dict.get(b"Mask").is_ok()
    {
        return None;
    }
    let mut data = stream.content.as_slice();
    if data.first() == Some(&b'\n') && data.get(1) == Some(&0xFF) {
        data = &data[1..];
    }
    Some(data.to_vec())
}

/// Decodes one image XObject and re-encodes it in `format`. PNG keeps
/// `/SMask` alpha (Java's `TYPE_INT_ARGB` target); every other format
/// emits RGB with soft masks dropped, like Java's `TYPE_INT_RGB` target.
/// JPEG inputs pass through byte-identical when nothing decode-relevant
/// rides along.
fn render_image(doc: &Document, stream: &Stream, format: &str) -> Option<Vec<u8>> {
    let lower = format.to_ascii_lowercase();
    let is_jpeg = matches!(lower.as_str(), "jpeg" | "jpg");
    if is_jpeg {
        if let Some(jpeg) = passthrough_jpeg(stream) {
            return Some(jpeg);
        }
    }
    let (_width, _height, pixels) = imgcomp::decode_image(stream)?;
    let mut out = Vec::new();
    if lower == "png" {
        let pixels = match smask_pixels(doc, stream) {
            Some(alpha) => with_alpha(pixels, &alpha),
            None => pixels,
        };
        let rgba = to_rgba(pixels);
        let (width, height) = rgba.dimensions();
        PngEncoder::new(&mut out)
            .write_image(rgba.as_raw(), width, height, ExtendedColorType::Rgba8)
            .ok()?;
    } else {
        let rgb = to_rgb(pixels);
        let (width, height) = rgb.dimensions();
        if is_jpeg {
            // ImageIO's default JPEG quality is ~0.75, the encoder default.
            JpegEncoder::new(&mut out)
                .write_image(rgb.as_raw(), width, height, ExtendedColorType::Rgb8)
                .ok()?;
        } else {
            let mut cursor = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgb8(rgb)
                .write_to(&mut cursor, image_format(format))
                .ok()?;
            return Some(cursor.into_inner());
        }
    }
    Some(out)
}

fn image_format(format: &str) -> image::ImageFormat {
    match format.to_ascii_lowercase().as_str() {
        "jpeg" | "jpg" => image::ImageFormat::Jpeg,
        "tiff" | "tif" => image::ImageFormat::Tiff,
        "bmp" => image::ImageFormat::Bmp,
        "webp" => image::ImageFormat::WebP,
        "gif" => image::ImageFormat::Gif,
        _ => image::ImageFormat::Png,
    }
}

/// The `/SMask` stream's luminance as an alpha mask, like PDFBox applying
/// the soft mask in `PDImageXObject.getImage`. `None` when absent or
/// undecodable — `supported` has already ruled the latter out.
fn smask_pixels(doc: &Document, stream: &Stream) -> Option<GrayImage> {
    let value = stream.dict.get(b"SMask").ok()?;
    if value.as_name().ok().is_some_and(|n| n == b"None") {
        return None;
    }
    let mask = resolve_stream(doc, value)?;
    let (_w, _h, pixels) = imgcomp::decode_image(mask)?;
    Some(match pixels {
        Pixels::Gray(gray) => gray,
        Pixels::Rgb(rgb) => {
            let (width, height) = rgb.dimensions();
            let mut out = GrayImage::new(width, height);
            for (src, dst) in rgb.pixels().zip(out.pixels_mut()) {
                let [r, g, b] = src.0;
                *dst =
                    image::Luma(
                        [((r as u32 * 299 + g as u32 * 587 + b as u32 * 114) / 1000) as u8],
                    );
            }
            out
        }
        Pixels::Rgba(rgba) => {
            let (width, height) = rgba.dimensions();
            let mut out = GrayImage::new(width, height);
            for (src, dst) in rgba.pixels().zip(out.pixels_mut()) {
                let [r, g, b, _a] = src.0;
                *dst =
                    image::Luma(
                        [((r as u32 * 299 + g as u32 * 587 + b as u32 * 114) / 1000) as u8],
                    );
            }
            out
        }
    })
}

/// Base pixels plus a soft-mask alpha channel — the ARGB raster Java's
/// `TYPE_INT_ARGB` PNG target receives. The mask wins over any alpha the
/// base image already carried, per the PDF spec.
fn with_alpha(pixels: Pixels, alpha: &GrayImage) -> Pixels {
    let (width, height) = alpha.dimensions();
    let mut out = RgbaImage::new(width, height);
    match pixels {
        Pixels::Gray(gray) => {
            for y in 0..height {
                for x in 0..width {
                    let l = gray.get_pixel(x, y).0[0];
                    let a = alpha.get_pixel(x, y).0[0];
                    out.put_pixel(x, y, image::Rgba([l, l, l, a]));
                }
            }
        }
        Pixels::Rgb(rgb) => {
            for y in 0..height {
                for x in 0..width {
                    let [r, g, b] = rgb.get_pixel(x, y).0;
                    let a = alpha.get_pixel(x, y).0[0];
                    out.put_pixel(x, y, image::Rgba([r, g, b, a]));
                }
            }
        }
        Pixels::Rgba(rgba) => {
            for y in 0..height {
                for x in 0..width {
                    let [r, g, b, _] = rgba.get_pixel(x, y).0;
                    let a = alpha.get_pixel(x, y).0[0];
                    out.put_pixel(x, y, image::Rgba([r, g, b, a]));
                }
            }
        }
    }
    Pixels::Rgba(out)
}

fn to_rgba(pixels: Pixels) -> RgbaImage {
    match pixels {
        Pixels::Gray(gray) => {
            let (width, height) = gray.dimensions();
            let mut out = RgbaImage::new(width, height);
            for (src, dst) in gray.pixels().zip(out.pixels_mut()) {
                let [l] = src.0;
                *dst = image::Rgba([l, l, l, 255]);
            }
            out
        }
        Pixels::Rgb(rgb) => {
            let (width, height) = rgb.dimensions();
            let mut out = RgbaImage::new(width, height);
            for (src, dst) in rgb.pixels().zip(out.pixels_mut()) {
                let [r, g, b] = src.0;
                *dst = image::Rgba([r, g, b, 255]);
            }
            out
        }
        Pixels::Rgba(rgba) => rgba,
    }
}

fn to_rgb(pixels: Pixels) -> RgbImage {
    match pixels {
        Pixels::Gray(gray) => {
            let (width, height) = gray.dimensions();
            let mut out = RgbImage::new(width, height);
            for (src, dst) in gray.pixels().zip(out.pixels_mut()) {
                let [l] = src.0;
                *dst = image::Rgb([l, l, l]);
            }
            out
        }
        Pixels::Rgb(rgb) => rgb,
        Pixels::Rgba(rgba) => {
            let (width, height) = rgba.dimensions();
            let mut out = RgbImage::new(width, height);
            for (src, dst) in rgba.pixels().zip(out.pixels_mut()) {
                let [r, g, b, _a] = src.0;
                *dst = image::Rgb([r, g, b]);
            }
            out
        }
    }
}

/// Per-document dedup state, the port of Java's `ImageFingerprints`: a
/// content key memoised per image object id, with a per-document counter
/// fallback for images whose object graph cannot be walked.
#[derive(Default)]
struct Fingerprints {
    keys: HashMap<ObjectId, u64>,
    extracted: HashSet<u64>,
    unfingerprintable: u64,
    failure_logged: bool,
}

impl Fingerprints {
    /// `true` the first time this image's content is seen in the document.
    fn is_first_occurrence(&mut self, doc: &Document, value: &Object, stream: &Stream) -> bool {
        let key = match value.as_reference() {
            Ok(id) => match self.keys.get(&id) {
                Some(&key) => key,
                None => {
                    let key = self.fingerprint(doc, stream);
                    self.keys.insert(id, key);
                    key
                }
            },
            // Streams are indirect in valid PDFs; a direct one simply is not
            // memoised, matching Java's per-COS-object identity fallback.
            Err(_) => self.fingerprint(doc, stream),
        };
        self.extracted.insert(key)
    }

    fn fingerprint(&mut self, doc: &Document, stream: &Stream) -> u64 {
        match content_key(doc, stream) {
            Ok(key) => key,
            Err(()) => {
                if !self.failure_logged {
                    self.failure_logged = true;
                    tracing::warn!(
                        "could not fingerprint embedded image, deduplicating by identity"
                    );
                }
                self.unfingerprintable += 1;
                self.unfingerprintable
            }
        }
    }
}

/// Content fingerprint identifying one embedded image: the encoded stream
/// plus the dictionary entries decoding depends on, with indirect
/// references resolved — two copies of the same image match while two
/// images that merely share encoded bytes do not. Java digests with
/// SHA-256; any stable hash serves here since the key never leaves the
/// process.
fn content_key(doc: &Document, stream: &Stream) -> Result<u64, ()> {
    let mut hasher = DefaultHasher::new();
    let mut walk = KeyWalk::new();
    digest_stream(&mut hasher, doc, stream, &mut walk, 0)?;
    let stencil = stream
        .dict
        .get(b"ImageMask")
        .ok()
        .and_then(|o| o.as_bool().ok())
        .unwrap_or(false);
    if !stencil {
        let color_space = stream.dict.get(b"ColorSpace").map_err(|_| ())?;
        digest_value(&mut hasher, doc, color_space, &mut walk, 0)?;
    }
    Ok(hasher.finish())
}

struct KeyWalk {
    expanded: HashSet<u32>,
    remaining: usize,
}

impl KeyWalk {
    fn new() -> Self {
        Self {
            expanded: HashSet::new(),
            remaining: MAX_KEY_NODES,
        }
    }

    fn enter(&mut self) -> Result<(), ()> {
        self.remaining = self.remaining.checked_sub(1).ok_or(())?;
        Ok(())
    }
}

fn digest_value(
    hasher: &mut impl Hasher,
    doc: &Document,
    value: &Object,
    walk: &mut KeyWalk,
    depth: usize,
) -> Result<(), ()> {
    if matches!(value, Object::Null) || depth > MAX_KEY_DEPTH {
        hasher.write(b"x");
        return Ok(());
    }
    walk.enter()?;
    match value {
        Object::Reference(id) => {
            if !walk.expanded.insert(id.0) {
                hasher.write(b"c");
                return Ok(());
            }
            match doc.get_object(*id) {
                Ok(object) => digest_value(hasher, doc, object, walk, depth + 1)?,
                Err(_) => hasher.write(b"x"),
            }
        }
        Object::Stream(stream) => digest_stream(hasher, doc, stream, walk, depth)?,
        Object::Dictionary(dict) => {
            hasher.write(b"d");
            digest_dict_entries(hasher, doc, dict, walk, depth)?;
        }
        Object::Array(items) => {
            hasher.write(b"a");
            for item in items {
                digest_value(hasher, doc, item, walk, depth + 1)?;
            }
        }
        Object::String(bytes, _) => {
            hasher.write(b"t");
            hasher.write(bytes);
        }
        Object::Name(name) => {
            hasher.write(b"n");
            hasher.write(name);
        }
        Object::Integer(number) => {
            hasher.write(b"v");
            hasher.write(number.to_string().as_bytes());
        }
        Object::Real(number) => {
            hasher.write(b"v");
            hasher.write(number.to_string().as_bytes());
        }
        Object::Boolean(flag) => {
            hasher.write(b"v");
            hasher.write(if *flag { &b"true"[..] } else { &b"false"[..] });
        }
        Object::Null => unreachable!(),
    }
    Ok(())
}

fn digest_stream(
    hasher: &mut impl Hasher,
    doc: &Document,
    stream: &Stream,
    walk: &mut KeyWalk,
    depth: usize,
) -> Result<(), ()> {
    hasher.write(b"s");
    hasher.write(&stream.content);
    digest_dict_entries(hasher, doc, &stream.dict, walk, depth)
}

/// Dictionary entries in sorted-key order, like Java's `digestDictionary`:
/// `/Length` is always skipped, and image dictionaries are limited to the
/// decode-relevant keys. Values are digested at `depth + 1`.
fn digest_dict_entries(
    hasher: &mut impl Hasher,
    doc: &Document,
    dict: &Dictionary,
    walk: &mut KeyWalk,
    depth: usize,
) -> Result<(), ()> {
    let image_dict = dict
        .get(b"Subtype")
        .ok()
        .and_then(|o| o.as_name().ok())
        .is_some_and(|n| n == b"Image");
    let mut keys: Vec<&[u8]> = dict
        .iter()
        .map(|(key, _)| key.as_slice())
        .filter(|key| {
            *key != b"Length" && !(image_dict && !DECODE_RELEVANT_IMAGE_KEYS.contains(key))
        })
        .collect();
    keys.sort_unstable();
    for key in keys {
        hasher.write(key);
        let value = dict.get(key).map_err(|_| ())?;
        digest_value(hasher, doc, value, walk, depth + 1)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit;
    use lopdf::{dictionary, Stream};
    use std::io::Read;

    /// 2×1 DeviceRGB image, FlateDecode-compressed raw samples.
    fn pdf_with_flate_image() -> Vec<u8> {
        let mut pdf = testkit::Pdf::new();
        let mut stream = Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => 2,
                "Height" => 1,
                "ColorSpace" => "DeviceRGB",
                "BitsPerComponent" => 8,
            },
            vec![255, 0, 0, 0, 255, 0],
        );
        let _ = stream.compress();
        let id = pdf.add(Object::Stream(stream));
        pdf.edit_page(|page| {
            page.set(
                "Resources",
                dictionary! {
                    "XObject" => dictionary! {
                        "Im1" => Object::Reference(id),
                    },
                },
            );
        });
        pdf.bytes()
    }

    fn zip_names(bytes: &[u8]) -> Vec<String> {
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect()
    }

    #[test]
    fn supported_accepts_decodable_subset() {
        let bytes = pdf_with_flate_image();
        assert!(supported("doc.pdf", &bytes).unwrap());
    }

    #[test]
    fn supported_rejects_out_of_subset_images() {
        // JPXDecode is outside imgcomp's filter set — the request must go
        // to Java rather than emit a ZIP missing the image.
        let mut pdf = testkit::Pdf::new();
        let stream = Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => 1,
                "Height" => 1,
                "ColorSpace" => "DeviceRGB",
                "BitsPerComponent" => 8,
                "Filter" => "JPXDecode",
            },
            vec![0, 0, 0],
        );
        let id = pdf.add(Object::Stream(stream));
        pdf.edit_page(|page| {
            page.set(
                "Resources",
                dictionary! {
                    "XObject" => dictionary! {
                        "Im1" => Object::Reference(id),
                    },
                },
            );
        });
        let bytes = pdf.bytes();
        assert!(!supported("doc.pdf", &bytes).unwrap());
    }

    #[test]
    fn extract_writes_named_png_entries() {
        let bytes = pdf_with_flate_image();
        let zip_bytes = extract("report.final.pdf", &bytes, "png").unwrap();
        assert_eq!(zip_names(&zip_bytes), vec!["report.final_page_1_1.png"]);

        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip_bytes)).unwrap();
        let mut entry = archive.by_name("report.final_page_1_1.png").unwrap();
        let mut png = Vec::new();
        entry.read_to_end(&mut png).unwrap();
        let decoded = image::load_from_memory_with_format(&png, image::ImageFormat::Png).unwrap();
        assert_eq!(decoded.width(), 2);
        assert_eq!(decoded.height(), 1);
    }

    #[test]
    fn extract_dedupes_repeated_references() {
        let mut pdf = testkit::Pdf::new();
        let mut stream = Stream::new(testkit::image_xobject(), vec![0, 0, 0]);
        let _ = stream.compress();
        let id = pdf.add(Object::Stream(stream));
        pdf.edit_page(|page| {
            page.set(
                "Resources",
                dictionary! {
                    "XObject" => dictionary! {
                        "Im1" => Object::Reference(id),
                        "Im2" => Object::Reference(id),
                    },
                },
            );
        });
        let bytes = pdf.bytes();
        let zip_bytes = extract("doc.pdf", &bytes, "png").unwrap();
        assert_eq!(zip_names(&zip_bytes), vec!["doc_page_1_1.png"]);
    }

    #[test]
    fn extract_skips_images_inside_form_xobjects() {
        // Java only reads the page's own /XObject table.
        let mut pdf = testkit::Pdf::new();
        let mut inner = Stream::new(testkit::image_xobject(), vec![0, 0, 0]);
        let _ = inner.compress();
        let inner_id = pdf.add(Object::Stream(inner));
        let mut form = Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Form",
                "BBox" => Object::Array(vec![
                    Object::Integer(0), Object::Integer(0),
                    Object::Integer(1), Object::Integer(1),
                ]),
                "Resources" => dictionary! {
                    "XObject" => dictionary! {
                        "Im1" => Object::Reference(inner_id),
                    },
                },
            },
            b"q 1 0 0 1 0 0 cm /Im1 Do Q".to_vec(),
        );
        let _ = form.compress();
        let form_id = pdf.add(Object::Stream(form));
        pdf.edit_page(|page| {
            page.set(
                "Resources",
                dictionary! {
                    "XObject" => dictionary! {
                        "Fm1" => Object::Reference(form_id),
                    },
                },
            );
        });
        let bytes = pdf.bytes();
        // The Form XObject itself is not an image; nothing is extracted.
        assert!(zip_names(&extract("doc.pdf", &bytes, "png").unwrap()).is_empty());
        // And the lone Form entry is not "supported" as an image either —
        // image_stream filters it out before the support check.
        assert!(supported("doc.pdf", &bytes).unwrap());
    }

    #[test]
    fn jpeg_format_uses_jpeg_entries() {
        let bytes = pdf_with_flate_image();
        let zip_bytes = extract("doc.pdf", &bytes, "jpeg").unwrap();
        assert_eq!(zip_names(&zip_bytes), vec!["doc_page_1_1.jpeg"]);
    }

    /// A DCT stream carrying `/DecodeParms` (e.g. `/ColorTransform`)
    /// changes pixel interpretation the Rust decoder cannot model —
    /// the request must reach Java.
    #[test]
    fn supported_rejects_dct_with_decode_parms() {
        let build = |extra: Dictionary| {
            let mut pdf = testkit::Pdf::new();
            let mut dict = dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => 1,
                "Height" => 1,
                "ColorSpace" => "DeviceRGB",
                "BitsPerComponent" => 8,
                "Filter" => "DCTDecode",
            };
            for (k, v) in extra.iter() {
                dict.set(k.clone(), v.clone());
            }
            let id = pdf.add(Object::Stream(Stream::new(dict, vec![0xFF, 0xD8, 0xFF])));
            pdf.edit_page(|page| {
                page.set(
                    "Resources",
                    dictionary! {
                        "XObject" => dictionary! {
                            "Im1" => Object::Reference(id),
                        },
                    },
                );
            });
            pdf.bytes()
        };
        assert!(!supported(
            "doc.pdf",
            &build(dictionary! {
                "DecodeParms" => dictionary! { "ColorTransform" => 0 },
            })
        )
        .unwrap());
        assert!(supported("doc.pdf", &build(Dictionary::new())).unwrap());
    }

    #[test]
    fn extract_rejects_non_pdf_input() {
        assert!(extract("doc.pdf", b"not a pdf", "png").is_err());
        assert!(supported("doc.pdf", b"not a pdf").is_err());
    }
}
