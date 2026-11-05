//! Image recompression pass for `misc/compress-pdf`, porting
//! `CompressController.compressImagesInPDF` onto `lopdf` + the `image` crate.
//!
//! Coverage vs Java/PDFBox: DCTDecode (JPEG) and FlateDecode/LZW/ASCII85/
//! ASCIIHex/RunLength/Brotli raw-sample images in DeviceGray, DeviceRGB and
//! DeviceCMYK at 8 bits per component are decoded, optionally resized and
//! re-encoded. JPXDecode, CCITTFaxDecode, JBIG2Decode, exotic filter chains,
//! other colour spaces and sub-8-bit depths are left untouched — Java would
//! recompress them, so those files diverge.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use image::codecs::jpeg::JpegEncoder;
use image::{
    DynamicImage, ExtendedColorType, GrayImage, ImageBuffer, Luma, Pixel, Rgb, RgbImage, RgbaImage,
};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};

use crate::error::{AppError, AppResult};
use crate::pdfdoc::{self, DictPath};

/// Java skips images at or below this size on either axis unless grayscale
/// conversion is requested (`MIN_WIDTH`/`MIN_HEIGHT` in CompressController).
const MIN_DIMENSION: u32 = 400;

/// Pixel ceiling for decoding one embedded image. Java derives this from
/// `system.maxImageMegapixels` or a quarter of the heap capped at 1 GiB;
/// 100 MP keeps a decoded RGBA raster under ~400 MB.
const MAX_IMAGE_PIXELS: u64 = 100_000_000;

/// Filters `Stream::decompressed_content` can decode; anything else is
/// skipped rather than recompressed wrongly.
const DECODABLE_FILTERS: &[&[u8]] = &[
    b"FlateDecode",
    b"LZWDecode",
    b"ASCII85Decode",
    b"ASCIIHexDecode",
    b"RunLengthDecode",
    b"BrotliDecode",
];

enum Pixels {
    Gray(GrayImage),
    Rgb(RgbImage),
    Rgba(RgbaImage),
}

impl Pixels {
    fn dimensions(&self) -> (u32, u32) {
        match self {
            Pixels::Gray(i) => i.dimensions(),
            Pixels::Rgb(i) => i.dimensions(),
            Pixels::Rgba(i) => i.dimensions(),
        }
    }
}

/// A resource-dictionary `/XObject` entry pointing at an image stream.
struct Candidate {
    /// Dictionary holding the `name → image` entry.
    path: DictPath,
    name: Vec<u8>,
    /// Image XObject object id.
    image: ObjectId,
    identity: u64,
}

/// Recompresses every image XObject in `bytes` like
/// `CompressController.compressImagesInPDF` and returns the re-saved PDF.
pub fn compress_images(
    name: &str,
    bytes: &[u8],
    scale: f64,
    jpeg_quality: f32,
    grayscale: bool,
) -> AppResult<Vec<u8>> {
    let mut doc = pdfdoc::load(name, bytes)?;
    let candidates = collect_candidates(&doc)?;
    if candidates.is_empty() {
        return pdfdoc::save(&mut doc);
    }

    // Group references by image identity so each unique image is decoded and
    // re-encoded once, like Java's ImageIdentity map.
    let mut groups: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, c) in candidates.iter().enumerate() {
        groups.entry(c.identity).or_default().push(i);
    }

    let mut replacements: HashMap<u64, (ObjectId, Option<ObjectId>)> = HashMap::new();
    for (identity, indexes) in groups {
        let image_id = candidates[indexes[0]].image;
        if let Some(pair) = compress_unique(&mut doc, image_id, scale, jpeg_quality, grayscale) {
            replacements.insert(identity, pair);
        }
    }

    for candidate in &candidates {
        let Some((image, _smask)) = replacements.get(&candidate.identity) else {
            continue;
        };
        let name = candidate.name.clone();
        let image = *image;
        pdfdoc::update_at(&mut doc, &candidate.path, move |dict| {
            dict.set(name, Object::Reference(image));
        })?;
    }

    pdfdoc::save(&mut doc)
}

/// Finds every image XObject reachable from page resources, plus images one
/// level inside form XObjects — the same two-level scan as Java's
/// `findImages`/`checkFormForImages`.
fn collect_candidates(doc: &Document) -> AppResult<Vec<Candidate>> {
    let mut out = Vec::new();
    for page_id in pdfdoc::page_ids(doc) {
        let Some((_holder, resources)) = pdfdoc::page_resources_holder(doc, page_id) else {
            continue;
        };
        let Some((path, xobjects)) = xobject_entries(doc, page_id, &resources) else {
            continue;
        };
        for (name, value) in xobjects.iter() {
            let Ok(xid) = value.as_reference() else {
                continue;
            };
            match xobject_subtype(doc, xid).as_deref() {
                Some(b"Image") => {
                    let identity = object_identity(doc, xid);
                    out.push(Candidate {
                        path: path.clone(),
                        name: name.clone(),
                        image: xid,
                        identity,
                    });
                }
                Some(b"Form") => {
                    // One level of form nesting, like Java: the form stream's
                    // own /Resources /XObject entries.
                    let Some(form_dict) = doc
                        .get_object(xid)
                        .ok()
                        .and_then(pdfdoc::dict_of)
                        .cloned()
                    else {
                        continue;
                    };
                    let Ok(fresources) = form_dict.get(b"Resources").cloned() else {
                        continue;
                    };
                    let Some((fpath, fimages)) = xobject_entries(doc, xid, &fresources) else {
                        continue;
                    };
                    for (fname, fvalue) in fimages.iter() {
                        let Ok(img) = fvalue.as_reference() else {
                            continue;
                        };
                        if xobject_subtype(doc, img).as_deref() != Some(b"Image") {
                            continue;
                        }
                        out.push(Candidate {
                            path: fpath.clone(),
                            name: fname.clone(),
                            image: img,
                            identity: object_identity(doc, img),
                        });
                    }
                }
                _ => {}
            }
        }
    }
    Ok(out)
}

/// The `/XObject` dictionary of a `/Resources` value plus the path that
/// locates it for writes. `holder` is the object id whose dictionary
/// contains the `/Resources` key (a page-tree node or a form XObject).
fn xobject_entries(
    doc: &Document,
    holder: ObjectId,
    resources: &Object,
) -> Option<(DictPath, Dictionary)> {
    let (root, mut keys) = match resources {
        Object::Reference(id) => (*id, Vec::new()),
        Object::Dictionary(_) => (holder, vec![b"Resources".to_vec()]),
        _ => return None,
    };
    let resources_dict = pdfdoc::dict_at(doc, &DictPath {
        root,
        keys: keys.clone(),
    })?;
    let xobjects = resources_dict.get(b"XObject").ok()?.clone();
    match xobjects {
        Object::Reference(xid) => {
            let dict = pdfdoc::dict_at(doc, &DictPath {
                root: xid,
                keys: Vec::new(),
            })?;
            Some((
                DictPath {
                    root: xid,
                    keys: Vec::new(),
                },
                dict,
            ))
        }
        Object::Dictionary(dict) => {
            keys.push(b"XObject".to_vec());
            Some((DictPath { root, keys }, dict))
        }
        _ => None,
    }
}

fn xobject_subtype(doc: &Document, id: ObjectId) -> Option<Vec<u8>> {
    let dict = pdfdoc::dict_of(doc.get_object(id).ok()?)?;
    dict.get(b"Subtype")
        .ok()?
        .as_name()
        .ok()
        .map(|n| n.to_vec())
}

/// Content-based identity for deduplication: the stream dictionary plus the
/// encoded bytes. Two resource entries pointing at equal images collapse to
/// one recompression, matching Java's `ImageIdentity` intent.
fn object_identity(doc: &Document, id: ObjectId) -> u64 {
    let mut hasher = DefaultHasher::new();
    if let Ok(Object::Stream(stream)) = doc.get_object(id) {
        format!("{:?}", stream.dict).hash(&mut hasher);
        stream.content.hash(&mut hasher);
    }
    hasher.finish()
}

/// Decodes, recompresses and re-inserts one unique image. Returns the new
/// image object id plus an optional soft-mask object id.
fn compress_unique(
    doc: &mut Document,
    image_id: ObjectId,
    scale: f64,
    jpeg_quality: f32,
    grayscale: bool,
) -> Option<(ObjectId, Option<ObjectId>)> {
    let (width, height, pixels, encoded_len, decode) = {
        let Object::Stream(stream) = doc.get_object(image_id).ok()? else {
            return None;
        };
        let encoded_len = stream.content.len();
        let decode = stream.dict.get(b"Decode").ok().cloned();
        let (width, height, pixels) = decode_image(stream)?;
        (width, height, pixels, encoded_len, decode)
    };

    let Some(processed) = process_image(pixels, width, height, scale, jpeg_quality, grayscale)
    else {
        return None;
    };

    match processed {
        Pixels::Gray(gray) => {
            let bytes = jpeg_encode(&gray, ExtendedColorType::L8, jpeg_quality);
            if bytes.len() >= encoded_len && !grayscale {
                return None;
            }
            let id = make_image_object(
                doc,
                width,
                height,
                b"DeviceGray",
                b"DCTDecode",
                bytes,
                decode,
            );
            Some((id, None))
        }
        Pixels::Rgb(rgb) => {
            let bytes = jpeg_encode(&rgb, ExtendedColorType::Rgb8, jpeg_quality);
            if bytes.len() >= encoded_len && !grayscale {
                return None;
            }
            let id = make_image_object(
                doc,
                width,
                height,
                b"DeviceRGB",
                b"DCTDecode",
                bytes,
                decode,
            );
            Some((id, None))
        }
        Pixels::Rgba(rgba) => {
            // Java encodes alpha images as PNG and PDFBox splits them into an
            // RGB image plus an /SMask; the equivalent here is flate-compressed
            // raw samples for both.
            let (rgb, alpha) = split_rgba(&rgba);
            let (rgb_bytes, rgb_flate) = flate_content(rgb.as_raw());
            let (alpha_bytes, alpha_flate) = flate_content(alpha.as_raw());
            if rgb_bytes.len() + alpha_bytes.len() >= encoded_len && !grayscale {
                return None;
            }
            let image = make_image_object(
                doc,
                width,
                height,
                b"DeviceRGB",
                if rgb_flate { b"FlateDecode" } else { b"" },
                rgb_bytes,
                decode,
            );
            let smask = make_image_object(
                doc,
                width,
                height,
                b"DeviceGray",
                if alpha_flate { b"FlateDecode" } else { b"" },
                alpha_bytes,
                None,
            );
            // Attach the mask to the image dictionary.
            if let Some(Object::Stream(stream)) = doc.objects.get_mut(&image) {
                stream.dict.set("SMask", smask);
            }
            Some((image, Some(smask)))
        }
    }
}

/// Port of `processAndCompressImage`: dimension gates, scale adjustment,
/// grayscale conversion, bicubic-ish resize.
fn process_image(
    pixels: Pixels,
    width: u32,
    height: u32,
    scale: f64,
    _jpeg_quality: f32,
    grayscale: bool,
) -> Option<Pixels> {
    if (width <= MIN_DIMENSION || height <= MIN_DIMENSION) && !grayscale {
        return None;
    }

    let pixels = if grayscale {
        to_gray(pixels)
    } else {
        pixels
    };

    let adjusted = if width > 3000 || height > 3000 {
        scale.min(0.75)
    } else if width < 1000 || height < 1000 {
        scale.max(0.9)
    } else {
        scale
    };

    let new_width = ((width as f64 * adjusted) as u32).max(MIN_DIMENSION);
    let new_height = ((height as f64 * adjusted) as u32).max(MIN_DIMENSION);

    if !grayscale
        && (new_width as f64 / width as f64 > 0.95)
        && (new_height as f64 / height as f64 > 0.95)
    {
        return None;
    }

    Some(resize(pixels, new_width, new_height))
}

fn to_gray(pixels: Pixels) -> Pixels {
    match pixels {
        Pixels::Gray(g) => Pixels::Gray(g),
        Pixels::Rgb(r) => Pixels::Gray(image::imageops::grayscale(&r)),
        Pixels::Rgba(r) => Pixels::Gray(image::imageops::grayscale(&r)),
    }
}

fn resize(pixels: Pixels, width: u32, height: u32) -> Pixels {
    match pixels {
        Pixels::Gray(g) => Pixels::Gray(image::imageops::resize(
            &g,
            width,
            height,
            image::imageops::FilterType::CatmullRom,
        )),
        Pixels::Rgb(r) => Pixels::Rgb(image::imageops::resize(
            &r,
            width,
            height,
            image::imageops::FilterType::CatmullRom,
        )),
        Pixels::Rgba(r) => Pixels::Rgba(image::imageops::resize(
            &r,
            width,
            height,
            image::imageops::FilterType::CatmullRom,
        )),
    }
}

/// Decodes an image XObject stream into pixels. Returns `(width, height,
/// pixels)` or `None` for anything outside the supported subset.
fn decode_image(stream: &Stream) -> Option<(u32, u32, Pixels)> {
    let dict = &stream.dict;
    if dict
        .get(b"ImageMask")
        .ok()
        .and_then(|o| o.as_bool().ok())
        == Some(true)
    {
        return None;
    }
    let width = dict.get(b"Width").ok()?.as_i64().ok()? as u32;
    let height = dict.get(b"Height").ok()?.as_i64().ok()? as u32;
    if width == 0 || height == 0 || width as u64 * height as u64 > MAX_IMAGE_PIXELS {
        return None;
    }

    let filters = stream.filters().ok()?;
    if filters.len() == 1 && filters[0] == b"DCTDecode" {
        return decode_jpeg(stream.content.as_slice(), width, height);
    }
    if !filters.iter().all(|f| DECODABLE_FILTERS.contains(f)) {
        return None;
    }
    decode_raw(stream, width, height)
}

/// DCTDecode payloads are JPEG files; PDFBox skips one leading line feed, so
/// do the same before handing the bytes to the decoder.
fn decode_jpeg(bytes: &[u8], width: u32, height: u32) -> Option<(u32, u32, Pixels)> {
    let mut data = bytes;
    if data.first() == Some(&b'\n') && data.get(1) == Some(&0xFF) {
        data = &data[1..];
    }
    let image = image::load_from_memory_with_format(data, image::ImageFormat::Jpeg).ok()?;
    Some((width, height, dynamic_to_pixels(image)))
}

fn dynamic_to_pixels(image: DynamicImage) -> Pixels {
    if image.color().has_alpha() {
        Pixels::Rgba(image.to_rgba8())
    } else if matches!(
        image.color(),
        image::ColorType::L8 | image::ColorType::La8
    ) {
        Pixels::Gray(image.to_luma8())
    } else {
        Pixels::Rgb(image.to_rgb8())
    }
}

/// Raw sample decoding for FlateDecode-style filters. `decompressed_content`
/// applies `/DecodeParms` predictors, so the result is plain interleaved
/// samples — provided the colour space is a plain Device* one at 8 bpc.
fn decode_raw(stream: &Stream, width: u32, height: u32) -> Option<(u32, u32, Pixels)> {
    let dict = &stream.dict;
    let bits = dict.get(b"BitsPerComponent").ok()?.as_i64().ok()?;
    if bits != 8 {
        return None;
    }
    let components = colorspace_components(dict.get(b"ColorSpace").ok()?)?;
    let raw = stream.decompressed_content().ok()?;
    let expected = width as usize * height as usize * components;
    if raw.len() < expected {
        return None;
    }
    let samples = &raw[..expected];
    let pixels = match components {
        1 => Pixels::Gray(
            ImageBuffer::<Luma<u8>, Vec<u8>>::from_raw(width, height, samples.to_vec())?,
        ),
        3 => Pixels::Rgb(RgbImage::from_raw(width, height, samples.to_vec())?),
        4 => Pixels::Rgb(cmyk_to_rgb(width, height, samples)),
        _ => return None,
    };
    Some((width, height, pixels))
}

/// Component count for the Device* colour spaces; anything else (ICCBased,
/// Indexed, Separation, …) returns `None` and the image is skipped.
fn colorspace_components(color_space: &Object) -> Option<usize> {
    match color_space {
        Object::Name(name) => match name.as_slice() {
            b"DeviceGray" => Some(1),
            b"DeviceRGB" => Some(3),
            b"DeviceCMYK" => Some(4),
            _ => None,
        },
        Object::Array(items) => match items.first() {
            Some(Object::Name(name)) => colorspace_components(&Object::Name(name.clone())),
            _ => None,
        },
        _ => None,
    }
}

/// Naive CMYK → RGB, matching what a straightforward decoder produces;
/// Java/PDFBox applies its own colour management, so exact values may differ.
fn cmyk_to_rgb(width: u32, height: u32, samples: &[u8]) -> RgbImage {
    let mut out = RgbImage::new(width, height);
    for (px, chunk) in out.pixels_mut().zip(samples.as_chunks::<4>().0) {
        let (c, m, y, k) = (
            255u16 - chunk[0] as u16,
            255u16 - chunk[1] as u16,
            255u16 - chunk[2] as u16,
            255u16 - chunk[3] as u16,
        );
        *px = Rgb([
            ((c * k + 128) / 255) as u8,
            ((m * k + 128) / 255) as u8,
            ((y * k + 128) / 255) as u8,
        ]);
    }
    out
}

fn split_rgba(rgba: &RgbaImage) -> (RgbImage, GrayImage) {
    let (width, height) = rgba.dimensions();
    let mut rgb = RgbImage::new(width, height);
    let mut alpha = GrayImage::new(width, height);
    for (src, (dst_rgb, dst_a)) in rgba
        .pixels()
        .zip(rgb.pixels_mut().zip(alpha.pixels_mut()))
    {
        let [r, g, b, a] = src.0;
        *dst_rgb = Rgb([r, g, b]);
        *dst_a = Luma([a]);
    }
    (rgb, alpha)
}

fn jpeg_encode<P: Pixel<Subpixel = u8>>(
    image: &ImageBuffer<P, Vec<u8>>,
    color: ExtendedColorType,
    quality: f32,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut out, (quality * 100.0).clamp(0.0, 100.0) as u8);
    let (width, height) = image.dimensions();
    // encode() consumes the encoder; ignore the result — a failed encode
    // yields truncated bytes that simply lose the size comparison.
    let _ = encoder.encode(image.as_raw(), width, height, color);
    out
}

/// Flate-compresses raw samples the way `Stream::compress` does, returning
/// the bytes plus whether a `/Filter` was actually set.
fn flate_content(raw: &[u8]) -> (Vec<u8>, bool) {
    let mut stream = Stream::new(Dictionary::new(), raw.to_vec());
    let _ = stream.compress();
    let filtered = stream.dict.has(b"Filter");
    (stream.content, filtered)
}

fn make_image_object(
    doc: &mut Document,
    width: u32,
    height: u32,
    color_space: &[u8],
    filter: &[u8],
    content: Vec<u8>,
    decode: Option<Object>,
) -> ObjectId {
    let mut dict = Dictionary::new();
    dict.set("Type", "XObject");
    dict.set("Subtype", "Image");
    dict.set("Width", width as i64);
    dict.set("Height", height as i64);
    dict.set("ColorSpace", Object::Name(color_space.to_vec()));
    dict.set("BitsPerComponent", 8);
    if !filter.is_empty() {
        dict.set("Filter", Object::Name(filter.to_vec()));
    }
    if let Some(decode) = decode {
        dict.set("Decode", decode);
    }
    let id = doc.new_object_id();
    doc.objects.insert(id, Object::Stream(Stream::new(dict, content)));
    id
}
