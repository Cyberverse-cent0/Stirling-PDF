//! `POST /api/v1/convert/*` — conversion endpoints. Image→PDF runs
//! natively on lopdf + the `image` crate, including CMYK JPEGs and
//! EXIF orientation; payloads outside the enabled codec set (SVG,
//! multi-frame TIFF, …) fail with a clear error. Everything
//! that needs an external engine (Ghostscript, LibreOffice, WeasyPrint,
//! pdftohtml, rsvg-convert/inkscape) runs through `crate::convert`'s
//! sidecar runner; when the engine is absent the request fails rather
//! than silently degrading. Handlers that delegate inspect the parsed
//! request before dispatch, like `misc::extract_images`.

use std::collections::{HashMap, HashSet};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use axum::extract::{Multipart, OriginalUri, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use base64::Engine as _;
use image::{GenericImage, GenericImageView};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::convert::{self, EngineSpec};
use crate::error::{AppError, AppResult};
use crate::handlers::jobs::{self, AppState};
use crate::handlers::merge::{file_response, pdf_response};
use crate::handlers::misc::dispatch;
use crate::page_select;
use crate::pdfdoc;
use crate::settings;
use crate::tabula;
use crate::text;
use crate::upload::{
    first_upload, output_name, param, param_bool, receive_multipart, Upload, FILE_FIELD,
};

/// `POST /api/v1/convert/img/pdf` — `PdfUtils.imageToPdf`. Every format
/// the `image` crate decodes converts natively (JPEG keeps its
/// `/DCTDecode` passthrough); undecodable input fails in the work fn
/// rather than round-tripping anywhere. The whole request fails when
/// any part is unsupported — Java's `MultipartFile[]` semantics are
/// all-or-nothing.
pub async fn img_pdf(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    _headers: HeaderMap,
    _uri: OriginalUri,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, params) = receive_multipart(multipart).await?;
    if uploads.is_empty() || !uploads.iter().all(native_supported) {
        return Err(AppError::BadRequest(
            "one or more uploaded files are not a supported image format".into(),
        ));
    }
    dispatch(state, query, uploads, params, img_pdf_work).await
}

fn img_pdf_work(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    // Java normalizes on isEmpty()/isBlank(), so an explicitly blank form
    // field gets the default rather than falling through to no draw branch.
    let fit = param(&params, "fitOption", "fillPage");
    let fit = if fit.is_empty() { "fillPage" } else { fit };
    let color = param(&params, "colorType", "color");
    let color = if color.trim().is_empty() {
        "color"
    } else {
        color
    };
    let auto_rotate = param_bool(&params, "autoRotate");

    let mut doc = pdfdoc::new_document();
    for upload in &uploads {
        let image = decode_page_image(upload, color)?;
        append_image_page(&mut doc, &image, fit, auto_rotate)?;
    }
    let bytes = pdfdoc::save(&mut doc)?;
    let name = output_name(uploads.first().map(|u| u.name.as_str()), "_converted.pdf");
    Ok(pdf_response(bytes, &name))
}

/// A decoded image ready to become one PDF page. `Jpeg` keeps the
/// original stream for `/DCTDecode` passthrough — re-encoding would lose
/// quality Java preserves via `JPEGFactory`.
pub(crate) enum PageImage {
    Jpeg {
        data: Vec<u8>,
        width: u32,
        height: u32,
        gray: bool,
    },
    Raw {
        width: u32,
        height: u32,
        gray: bool,
        samples: Vec<u8>,
        smask: Option<Vec<u8>>,
    },
}

impl PageImage {
    fn width(&self) -> u32 {
        match self {
            Self::Jpeg { width, .. } | Self::Raw { width, .. } => *width,
        }
    }
    fn height(&self) -> u32 {
        match self {
            Self::Jpeg { height, .. } | Self::Raw { height, .. } => *height,
        }
    }
    fn gray(&self) -> bool {
        match self {
            Self::Jpeg { gray, .. } | Self::Raw { gray, .. } => *gray,
        }
    }
}

fn is_jpeg(bytes: &[u8]) -> bool {
    bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF
}

#[cfg(test)]
fn is_png(bytes: &[u8]) -> bool {
    bytes.len() >= 8 && bytes[..8] == [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]
}

/// Anything `image::guess_format` recognises is attempted; undecodable
/// payloads surface as a 400 inside `decode_page_image`. JPEGs keep
/// their fast paths there — this gate only rejects formats with no
/// decoder at all.
fn native_supported(upload: &Upload) -> bool {
    image::guess_format(&upload.bytes).is_ok()
}

fn decode_page_image(upload: &Upload, color_type: &str) -> AppResult<PageImage> {
    let bytes: &[u8] = &upload.bytes;
    // Java's convertColorType switch is exact-match: only the two
    // quantize values change the image; everything else stays color.
    let wants_color = !matches!(color_type, "greyscale" | "blackwhite");
    if is_jpeg(bytes) && wants_color {
        // The `/DCTDecode` passthrough is only valid for the plain cases:
        // CMYK needs `/Decode` inversion and EXIF orientation needs the
        // pixels rotated, so both take the decode path like Java's
        // `loadImageWithExifOrientation`.
        if let Some((width, height, components)) = jpeg_sof(bytes) {
            if components != 4 && exif_orientation(bytes).is_none_or(|o| o == 1) {
                return Ok(PageImage::Jpeg {
                    data: bytes.to_vec(),
                    width,
                    height,
                    gray: components == 1,
                });
            }
        }
    }
    let dynamic = image::load_from_memory(bytes).map_err(|e| {
        AppError::BadRequest(format!("'{}' is not a readable image: {e}", upload.name))
    })?;
    let dynamic = if is_jpeg(bytes) {
        apply_exif_orientation(dynamic, bytes)
    } else {
        dynamic
    };
    Ok(raw_from_dynamic(&dynamic, color_type))
}

/// EXIF orientation values 2–8 mapped onto `DynamicImage` transforms —
/// the same correction `loadImageWithExifOrientation` applies in Java.
fn apply_exif_orientation(img: image::DynamicImage, jpeg: &[u8]) -> image::DynamicImage {
    match exif_orientation(jpeg) {
        Some(2) => img.fliph(),
        Some(3) => img.rotate180(),
        Some(4) => img.flipv(),
        Some(5) => img.fliph().rotate270(),
        Some(6) => img.rotate90(),
        Some(7) => img.fliph().rotate90(),
        Some(8) => img.rotate270(),
        _ => img,
    }
}

/// `ImageProcessingUtils.convertColorType`: `greyscale`/`blackwhite`
/// quantize to gray (alpha dropped, as Java's `TYPE_BYTE_*` targets do);
/// anything else keeps color. `blackwhite` thresholds luminance at the
/// midpoint, matching `TYPE_BYTE_BINARY`'s two-level raster.
fn raw_from_dynamic(img: &image::DynamicImage, color_type: &str) -> PageImage {
    let (width, height) = img.dimensions();
    match color_type {
        "greyscale" => PageImage::Raw {
            width,
            height,
            gray: true,
            samples: img.to_luma8().into_raw(),
            smask: None,
        },
        "blackwhite" => PageImage::Raw {
            width,
            height,
            gray: true,
            samples: img
                .to_luma8()
                .into_raw()
                .into_iter()
                .map(|v| if v >= 128 { 255 } else { 0 })
                .collect(),
            smask: None,
        },
        _ => {
            let rgba = img.to_rgba8();
            let raw = rgba.as_raw();
            let mut rgb = Vec::with_capacity((width * height * 3) as usize);
            let mut alpha = Vec::with_capacity((width * height) as usize);
            let mut has_transparency = false;
            for px in raw.as_chunks::<4>().0 {
                rgb.extend_from_slice(&px[..3]);
                alpha.push(px[3]);
                has_transparency |= px[3] != 255;
            }
            PageImage::Raw {
                width,
                height,
                gray: false,
                samples: rgb,
                smask: has_transparency.then_some(alpha),
            }
        }
    }
}

/// `PdfUtils.addImageToDocument`: A4 page (swapped for landscape under
/// `autoRotate`), `fitDocumentToImage`/`fitDocumentToPage` size the page to
/// the image, `fillPage` stretches, `maintainAspectRatio` centers an
/// aspect-fit draw; any other `fitOption` yields a blank page, as Java's
/// empty content stream does.
fn append_image_page(
    doc: &mut Document,
    image: &PageImage,
    fit_option: &str,
    auto_rotate: bool,
) -> AppResult<()> {
    const A4_W: f32 = 595.28; // PDRectangle.A4: 210pt x 297pt
    const A4_H: f32 = 841.89;

    let iw = image.width() as f32;
    let ih = image.height() as f32;
    let mut page_w = A4_W;
    let mut page_h = A4_H;
    if auto_rotate && iw > ih {
        std::mem::swap(&mut page_w, &mut page_h);
    }
    if matches!(fit_option, "fitDocumentToImage" | "fitDocumentToPage") {
        page_w = iw;
        page_h = ih;
    }

    let content = page_draw(fit_option, page_w, page_h, iw, ih);
    commit_page(doc, image, page_w, page_h, content)
}

/// Writes `content` (a page content stream) plus the image XObject onto a
/// new page of the given size and attaches it to the page tree. Shared by
/// the image→PDF fit logic and the SVG viewport path.
pub(crate) fn commit_page(
    doc: &mut Document,
    image: &PageImage,
    page_w: f32,
    page_h: f32,
    content: Vec<u8>,
) -> AppResult<()> {
    let image_id = image_object(doc, image)?;
    let mut xobject = Dictionary::new();
    xobject.set("Im1", Object::Reference(image_id));
    let mut resources_dict = Dictionary::new();
    resources_dict.set("XObject", Object::Dictionary(xobject));

    let content_id = add_object(doc, Object::Stream(Stream::new(Dictionary::new(), content)));
    let page_id = add_object(
        doc,
        Object::Dictionary({
            let mut page = Dictionary::new();
            page.set("Type", "Page");
            page.set(
                "MediaBox",
                Object::Array(vec![
                    Object::Real(0.0),
                    Object::Real(0.0),
                    Object::Real(page_w),
                    Object::Real(page_h),
                ]),
            );
            page.set("Resources", Object::Dictionary(resources_dict));
            page.set("Contents", Object::Reference(content_id));
            page
        }),
    );
    attach_page(doc, page_id)
}

/// PDF user space has its origin at the lower-left, matching PDFBox's
/// `drawImage(image, x, y, w, h)`; a `cm` + `Do` pair is the equivalent
/// PDF operator sequence. Unknown `fitOption` values produce the empty
/// content stream Java writes when no `drawImage` branch matches.
fn page_draw(fit_option: &str, page_w: f32, page_h: f32, iw: f32, ih: f32) -> Vec<u8> {
    let draw = match fit_option {
        "fillPage" | "fitDocumentToImage" | "fitDocumentToPage" => {
            format!("q {page_w} 0 0 {page_h} 0 0 cm /Im1 Do Q")
        }
        "maintainAspectRatio" => {
            let scale = if iw / ih > page_w / page_h {
                page_w / iw
            } else {
                page_h / ih
            };
            let dw = iw * scale;
            let dh = ih * scale;
            let dx = (page_w - dw) / 2.0;
            let dy = (page_h - dh) / 2.0;
            format!("q {dw} 0 0 {dh} {dx} {dy} cm /Im1 Do Q")
        }
        _ => String::new(),
    };
    draw.into_bytes()
}

fn image_object(doc: &mut Document, image: &PageImage) -> AppResult<ObjectId> {
    let mut dict = Dictionary::new();
    dict.set("Type", "XObject");
    dict.set("Subtype", "Image");
    dict.set("Width", image.width() as i64);
    dict.set("Height", image.height() as i64);
    dict.set(
        "ColorSpace",
        if image.gray() {
            "DeviceGray"
        } else {
            "DeviceRGB"
        },
    );
    dict.set("BitsPerComponent", 8i64);

    let (samples, filter) = match image {
        PageImage::Jpeg { data, .. } => (data.clone(), Some("DCTDecode")),
        PageImage::Raw { samples, .. } => (samples.clone(), None),
    };
    if let Some(f) = filter {
        dict.set("Filter", f);
    }
    if let PageImage::Raw {
        smask: Some(alpha),
        width,
        height,
        ..
    } = image
    {
        let mut smask_dict = Dictionary::new();
        smask_dict.set("Type", "XObject");
        smask_dict.set("Subtype", "Image");
        smask_dict.set("Width", *width as i64);
        smask_dict.set("Height", *height as i64);
        smask_dict.set("ColorSpace", "DeviceGray");
        smask_dict.set("BitsPerComponent", 8i64);
        let id = add_object(doc, Object::Stream(Stream::new(smask_dict, alpha.clone())));
        dict.set("SMask", Object::Reference(id));
    }
    Ok(add_object(doc, Object::Stream(Stream::new(dict, samples))))
}

fn add_object(doc: &mut Document, object: Object) -> ObjectId {
    let id = doc.new_object_id();
    doc.objects.insert(id, object);
    id
}

/// Appends `page_id` to the document's page tree, fixing `/Parent` on the
/// page and `/Kids`/`/Count` on the root — the shape `new_document` +
/// `import_pages` produce for imported pages.
fn attach_page(doc: &mut Document, page_id: ObjectId) -> AppResult<()> {
    let root = pages_root(doc)?;
    doc.get_dictionary_mut(page_id)
        .map_err(|e| AppError::Pdf(format!("page dict: {e}")))?
        .set("Parent", Object::Reference(root));
    let root_dict = doc.get_dictionary_mut(root).map_err(pdf_err)?;
    let mut kids = match root_dict.get(b"Kids") {
        Ok(Object::Array(kids)) => kids.clone(),
        _ => Vec::new(),
    };
    kids.push(Object::Reference(page_id));
    root_dict.set("Kids", Object::Array(kids));
    let count = match root_dict.get(b"Count") {
        Ok(Object::Integer(n)) => *n,
        _ => 0,
    };
    root_dict.set("Count", count + 1);
    Ok(())
}

fn pages_root(doc: &Document) -> AppResult<ObjectId> {
    let catalog_id = doc
        .trailer
        .get(b"Root")
        .ok()
        .and_then(|o| o.as_reference().ok())
        .ok_or_else(|| AppError::Pdf("catalog /Root is not an indirect reference".into()))?;
    let catalog = doc.get_dictionary(catalog_id).map_err(pdf_err)?;
    catalog
        .get(b"Pages")
        .ok()
        .and_then(|o| o.as_reference().ok())
        .ok_or_else(|| AppError::Pdf("catalog /Pages is not an indirect reference".into()))
}

fn pdf_err(e: lopdf::Error) -> AppError {
    AppError::Pdf(e.to_string())
}

/// JPEG start-of-frame scan: returns `(width, height, components)` from
/// the first SOF marker. `components` is 1 for grayscale, 3 for YCbCr/RGB,
/// 4 for CMYK — the value that decides `/ColorSpace` and the CMYK proxy.
fn jpeg_sof(data: &[u8]) -> Option<(u32, u32, usize)> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let mut i = 2usize;
    while i + 9 <= data.len() {
        if data[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = data[i + 1];
        if marker == 0x00 || marker == 0xFF {
            i += 1;
            continue;
        }
        // Standalone markers carry no length.
        if marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
            i += 2;
            continue;
        }
        let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
        if len < 2 {
            return None;
        }
        // SOF0-SOF15 except DHT (C4), JPG (C8), DAC (CC).
        if (0xC0..=0xCF).contains(&marker) && ![0xC4, 0xC8, 0xCC].contains(&marker) {
            let height = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u32;
            let width = u16::from_be_bytes([data[i + 7], data[i + 8]]) as u32;
            let components = data[i + 9] as usize;
            return Some((width, height, components));
        }
        i += 2 + len;
    }
    None
}

/// EXIF orientation from APP1, or `None` when absent/unparseable. Only
/// values other than 1 change rendering, so callers treat `None` as
/// "no rotation".
fn exif_orientation(data: &[u8]) -> Option<u16> {
    let mut i = 2usize;
    while i + 4 <= data.len() {
        if data[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = data[i + 1];
        if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            i += 2;
            continue;
        }
        if marker == 0xDA {
            return None; // scan data — no more metadata segments
        }
        let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
        if len < 2 || i + 2 + len > data.len() {
            return None;
        }
        if marker == 0xE1 && len >= 8 && data[i + 4..i + 10].starts_with(b"Exif\0\0") {
            return tiff_orientation(&data[i + 10..i + 2 + len]);
        }
        i += 2 + len;
    }
    None
}

fn tiff_orientation(tiff: &[u8]) -> Option<u16> {
    if tiff.len() < 8 {
        return None;
    }
    let (le, mut off) = match &tiff[..2] {
        b"II" => (true, 4usize),
        b"MM" => (false, 4usize),
        _ => return None,
    };
    let u16at = |o: usize| -> Option<u16> {
        let b = tiff.get(o..o + 2)?;
        Some(if le {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        })
    };
    let u32at = |o: usize| -> Option<u32> {
        let b = tiff.get(o..o + 4)?;
        Some(if le {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        })
    };
    if u16at(2) != Some(42) {
        return None;
    }
    let ifd0 = u32at(off)? as usize;
    off = ifd0;
    let count = u16at(off)? as usize;
    for n in 0..count {
        let entry = off + 2 + n * 12;
        if u16at(entry) == Some(0x0112) {
            // Type SHORT count 1: the value sits in the entry's first
            // two value bytes regardless of endianness of storage order.
            return u16at(entry + 8);
        }
    }
    None
}

/// `useStirlingOfficeConvert` request flag falling back to the admin
/// `system.stirlingOfficeConversion` default — last occurrence wins,
/// matching Spring's repeated-param binding.
fn office_convert_requested(params: &[(String, String)]) -> bool {
    params
        .iter()
        .rev()
        .find(|(n, _)| n == "useStirlingOfficeConvert")
        .map(|_| param_bool(params, "useStirlingOfficeConvert"))
        .unwrap_or_else(|| crate::settings::current().system.stirling_office_conversion)
}

fn run_engine(spec: &EngineSpec) -> AppResult<convert::EngineOutput> {
    let out = convert::run(spec)?;
    convert::require_success(&spec.program, &out)?;
    Ok(out)
}

fn zip_bytes(entries: &[(String, Vec<u8>)]) -> AppResult<Vec<u8>> {
    let mut writer = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, bytes) in entries {
        writer
            .start_file(
                name,
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
            )
            .map_err(|e| AppError::Internal(anyhow::anyhow!("zip entry: {e}")))?;
        writer
            .write_all(bytes)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("zip write: {e}")))?;
    }
    Ok(writer
        .finish()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("zip finish: {e}")))?
        .into_inner())
}

fn read_dir_files(dir: &Path) -> AppResult<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("read dir: {e}")))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    Ok(files)
}

// ---------------------------------------------------------------------------
// PDF → image (`/convert/pdf/img`, ConvertImgPDFController.convertToImage)
// ---------------------------------------------------------------------------

/// Rasterises through Ghostscript; formats without a gs device render to
/// PNG first and are transcoded via the `image` crate.
pub async fn pdf_to_img(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    _headers: HeaderMap,
    _uri: OriginalUri,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, params) = receive_multipart(multipart).await?;
    let format = param(&params, "imageFormat", "png").to_ascii_lowercase();
    if uploads.is_empty() || !PDF_IMAGE_FORMATS.contains(&format.as_str()) {
        return Err(AppError::BadRequest(format!(
            "unsupported imageFormat: {format}"
        )));
    }
    dispatch(state, query, uploads, params, pdf_to_img_work).await
}

/// Formats `/convert/pdf/img` emits. `png`/`jpeg` go straight out of the
/// Ghostscript device; the rest render to PNG and are transcoded through
/// the `image` crate (its WebP encoder is lossless-only — Java's PIL
/// path could emit lossy WebP, a documented divergence).
const PDF_IMAGE_FORMATS: &[&str] = &["png", "jpeg", "jpg", "tiff", "tif", "bmp", "webp", "gif"];

fn pdf_image_mime(format: &str) -> &'static str {
    match format {
        "png" => "image/png",
        "jpeg" | "jpg" => "image/jpeg",
        "tiff" | "tif" => "image/tiff",
        "bmp" => "image/bmp",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => "application/octet-stream",
    }
}

fn pdf_to_img_work(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let upload = first_upload(uploads)?;
    let format = param(&params, "imageFormat", "png").to_ascii_lowercase();
    let color = param(&params, "colorType", "color");
    let single = param(&params, "singleOrMultiple", "multiple") == "single";

    // Java defaults a missing/non-positive dpi to 300 and rejects above
    // the configured ceiling (ApplicationProperties maxDPI, default 500).
    let dpi_raw = param(&params, "dpi", "300");
    let dpi: i64 = dpi_raw
        .trim()
        .parse()
        .map_err(|_| AppError::BadRequest(format!("invalid dpi: {dpi_raw}")))?;
    let dpi = if dpi <= 0 { 300 } else { dpi };
    if dpi > 500 {
        return Err(AppError::BadRequest(format!(
            "DPI value {dpi} exceeds maximum safe limit of 500"
        )));
    }

    let mut doc = pdfdoc::load(&upload.name, &upload.bytes)?;
    let page_numbers = param(&params, "pageNumbers", "").trim();
    if !page_numbers.is_empty() && !page_numbers.eq_ignore_ascii_case("all") {
        let total = pdfdoc::page_ids(&doc).len();
        page_select::require_valid_page_selection(page_numbers, total)
            .map_err(AppError::BadRequest)?;
        let order: Vec<u32> = page_select::parse_page_list(page_numbers, total)
            .map_err(AppError::BadRequest)?
            .into_iter()
            .map(|i| i as u32)
            .collect();
        pdfdoc::subset(&mut doc, &order)?;
    }
    let pdf_bytes = pdfdoc::save(&mut doc)?;

    let dir =
        tempfile::tempdir().map_err(|e| AppError::Internal(anyhow::anyhow!("tempdir: {e}")))?;
    let input = dir.path().join("input.pdf");
    std::fs::write(&input, &pdf_bytes)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("write pdf: {e}")))?;

    // Formats without a Ghostscript device render through the PNG
    // devices and are transcoded afterwards, keeping the colorType
    // semantics identical across output formats.
    let direct = matches!(format.as_str(), "png" | "jpeg" | "jpg");
    let gs_format = if direct {
        if format == "png" {
            "png"
        } else {
            "jpeg"
        }
    } else {
        "png"
    };
    let device = match (gs_format, color) {
        ("png", "greyscale") => "pnggray",
        ("png", "blackwhite") => "pngmono",
        ("png", _) => "png16m",
        ("jpeg", "greyscale") => "jpeggray",
        _ => "jpeg",
    };
    let pattern = dir.path().join(format!("page-%d.{gs_format}"));
    let spec = EngineSpec::new("gs")
        .args([
            "-dNOPAUSE".to_string(),
            "-dBATCH".to_string(),
            "-dQUIET".to_string(),
            "-dSAFER".to_string(),
            format!("-sDEVICE={device}"),
            format!("-r{dpi}"),
            format!("-sOutputFile={}", pattern.display()),
        ])
        .arg(input.to_string_lossy().to_string())
        .timeout(std::time::Duration::from_secs(600));
    run_engine(&spec)?;

    let mut pages: Vec<PathBuf> = read_dir_files(dir.path())?
        .into_iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("page-"))
        })
        .collect();
    pages.sort_by_key(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n[5..].split('.').next()?.parse::<u32>().ok())
            .unwrap_or(0)
    });
    if pages.is_empty() {
        return Err(AppError::Pdf("ghostscript produced no page images".into()));
    }

    let base = output_name(Some(&upload.name), "");
    if single {
        let bytes = stitch_pages(&pages, &format, color)?;
        return Ok(file_response(
            bytes,
            &format!("{base}.{format}"),
            pdf_image_mime(&format),
        ));
    }

    let mut entries = Vec::with_capacity(pages.len());
    for (i, page) in pages.iter().enumerate() {
        let bytes = if direct {
            std::fs::read(page)
                .map_err(|e| AppError::Internal(anyhow::anyhow!("read rendered page: {e}")))?
        } else {
            transcode_page(page, &format)?
        };
        entries.push((format!("{base}_{}.{}", i + 1, format), bytes));
    }
    Ok(file_response(
        zip_bytes(&entries)?,
        &format!("{base}_convertedToImages.zip"),
        "application/octet-stream",
    ))
}

/// Java's `single` mode stacks every rendered page vertically, centred on
/// the widest page. Ghostscript already rasterised; this only repacks.
fn stitch_pages(pages: &[PathBuf], format: &str, color: &str) -> AppResult<Vec<u8>> {
    use image::{DynamicImage, ImageBuffer, Luma, Rgba};

    let color = !matches!(color, "greyscale" | "blackwhite");
    if color {
        let mut images = Vec::with_capacity(pages.len());
        let mut max_w = 0u32;
        let mut total_h = 0u32;
        for page in pages {
            let img = image::open(page)
                .map_err(|e| AppError::Internal(anyhow::anyhow!("decode rendered page: {e}")))?
                .to_rgba8();
            max_w = max_w.max(img.width());
            total_h += img.height();
            images.push(img);
        }
        let mut canvas = ImageBuffer::<Rgba<u8>, Vec<u8>>::new(max_w, total_h);
        let mut y = 0u32;
        for img in images {
            let x = (max_w - img.width()) / 2;
            canvas
                .copy_from(&img, x, y)
                .map_err(|e| AppError::Internal(anyhow::anyhow!("stitch image: {e}")))?;
            y += img.height();
        }
        let mut out = std::io::Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(canvas)
            .write_to(&mut out, image_format(format))
            .map_err(|e| AppError::Internal(anyhow::anyhow!("encode image: {e}")))?;
        Ok(out.into_inner())
    } else {
        let mut images = Vec::with_capacity(pages.len());
        let mut max_w = 0u32;
        let mut total_h = 0u32;
        for page in pages {
            let img = image::open(page)
                .map_err(|e| AppError::Internal(anyhow::anyhow!("decode rendered page: {e}")))?
                .to_luma8();
            max_w = max_w.max(img.width());
            total_h += img.height();
            images.push(img);
        }
        let mut canvas = ImageBuffer::<Luma<u8>, Vec<u8>>::new(max_w, total_h);
        let mut y = 0u32;
        for img in images {
            let x = (max_w - img.width()) / 2;
            canvas
                .copy_from(&img, x, y)
                .map_err(|e| AppError::Internal(anyhow::anyhow!("stitch image: {e}")))?;
            y += img.height();
        }
        let mut out = std::io::Cursor::new(Vec::new());
        DynamicImage::ImageLuma8(canvas)
            .write_to(&mut out, image_format(format))
            .map_err(|e| AppError::Internal(anyhow::anyhow!("encode image: {e}")))?;
        Ok(out.into_inner())
    }
}

// ---------------------------------------------------------------------------
// PDF → CBZ (`/convert/pdf/cbz`, ConvertImgPDFController.convertPdfToCbz +
// PdfToCbzUtils)
// ---------------------------------------------------------------------------

/// `POST /api/v1/convert/pdf/cbz` — rasterise each page through Ghostscript
/// and zip the PNGs as `page_%03d.png`, matching `PdfToCbzUtils`. Java
/// renders via PDFBox; pixels differ between renderers but the container
/// format and naming are identical.
pub async fn pdf_to_cbz(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    _headers: HeaderMap,
    _uri: OriginalUri,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, params) = receive_multipart(multipart).await?;
    if uploads.is_empty() {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    }
    dispatch(state, query, uploads, params, pdf_to_cbz_work).await
}

fn pdf_to_cbz_work(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let upload = first_upload(uploads)?;
    let filename = upload
        .name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&upload.name);
    let ext = filename
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    if !filename.trim().is_empty() && ext != "pdf" {
        return Err(AppError::BadRequest(format!(
            "'{filename}' is not a PDF file"
        )));
    }
    if filename.trim().is_empty() {
        return Err(AppError::BadRequest("file input has no name".into()));
    }

    let mut doc = pdfdoc::load(&upload.name, &upload.bytes)?;
    if pdfdoc::page_ids(&doc).is_empty() {
        return Err(AppError::BadRequest("PDF document has no pages".into()));
    }
    let pdf_bytes = pdfdoc::save(&mut doc)?;

    // The request field defaults to 150 when `dpi` is absent; only an
    // explicit non-positive value falls through to the controller's 300.
    let dpi_raw = param(&params, "dpi", "150");
    let dpi: i64 = dpi_raw
        .trim()
        .parse()
        .map_err(|_| AppError::BadRequest(format!("invalid dpi: {dpi_raw}")))?;
    let dpi = if dpi <= 0 { 300 } else { dpi };

    let dir =
        tempfile::tempdir().map_err(|e| AppError::Internal(anyhow::anyhow!("tempdir: {e}")))?;
    let pages = gs_render_pages_png(dir.path(), &pdf_bytes, dpi)?;

    // `page_%03d.png` naming, zero-padded to three digits like Java's
    // String.format — %03d only pads below 1000, wider indices keep all
    // digits, matching Java for documents past 999 pages.
    let mut entries = Vec::with_capacity(pages.len());
    for (i, page) in pages.iter().enumerate() {
        let bytes = std::fs::read(page)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("read rendered page: {e}")))?;
        entries.push((format!("page_{:03}.png", i + 1), bytes));
    }
    let base = output_name(Some(&upload.name), "");
    Ok(file_response(
        zip_bytes(&entries)?,
        &format!("{base}_converted.cbz"),
        "application/octet-stream",
    ))
}

fn image_format(format: &str) -> image::ImageFormat {
    match format {
        "png" => image::ImageFormat::Png,
        "jpeg" | "jpg" => image::ImageFormat::Jpeg,
        "tiff" | "tif" => image::ImageFormat::Tiff,
        "bmp" => image::ImageFormat::Bmp,
        "webp" => image::ImageFormat::WebP,
        "gif" => image::ImageFormat::Gif,
        _ => image::ImageFormat::Png,
    }
}

/// Ghostscript has no tiff/bmp/webp/gif devices on this path, so those
/// formats arrive as PNG rasters and are re-encoded here — WebP stays
/// lossless, matching the encoder's only mode (Java's PIL path could
/// emit lossy).
fn transcode_page(page: &Path, format: &str) -> AppResult<Vec<u8>> {
    let img = image::open(page)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("decode rendered page: {e}")))?;
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image_format(format))
        .map_err(|e| AppError::Internal(anyhow::anyhow!("encode {format}: {e}")))?;
    Ok(out.into_inner())
}

/// Ghostscript `png16m` rasterisation shared by `/convert/pdf/cbz` and
/// `/convert/pdf/cbr` (`PdfToCbzUtils`/`PdfToCbrUtils` both render every
/// page to `page_%03d.png`-shaped output). `dir` must outlive the
/// returned paths — the caller owns the tempdir.
fn gs_render_pages_png(dir: &Path, pdf_bytes: &[u8], dpi: i64) -> AppResult<Vec<PathBuf>> {
    let input = dir.join("input.pdf");
    std::fs::write(&input, pdf_bytes)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("write pdf: {e}")))?;
    let pattern = dir.join("page-%d.png");
    let spec = EngineSpec::new("gs")
        .args([
            "-dNOPAUSE".to_string(),
            "-dBATCH".to_string(),
            "-dQUIET".to_string(),
            "-dSAFER".to_string(),
            "-sDEVICE=png16m".to_string(),
            format!("-r{dpi}"),
            format!("-sOutputFile={}", pattern.display()),
        ])
        .arg(input.to_string_lossy().to_string())
        .timeout(std::time::Duration::from_secs(600));
    run_engine(&spec)?;

    let mut pages: Vec<PathBuf> = read_dir_files(dir)?
        .into_iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("page-"))
        })
        .collect();
    pages.sort_by_key(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n[5..].split('.').next()?.parse::<u32>().ok())
            .unwrap_or(0)
    });
    if pages.is_empty() {
        return Err(AppError::Pdf("ghostscript produced no page images".into()));
    }
    Ok(pages)
}

/// One archive image → one PDF page, shared by the CBZ and CBR builders.
/// JPEGs keep their `/DCTDecode` stream like Java's `createFromByteArray`;
/// everything else decodes through `image`. Returns `Ok(false)` for
/// payloads no decoder accepts — Java logs and skips the same way.
/// `Ok(Err)` propagates genuine PDF-write failures.
fn commit_archive_image(doc: &mut Document, bytes: &[u8]) -> AppResult<bool> {
    let image = if is_jpeg(bytes) {
        match jpeg_sof(bytes) {
            Some((w, h, c)) if w > 0 && h > 0 && c != 4 => PageImage::Jpeg {
                data: bytes.to_vec(),
                width: w,
                height: h,
                gray: c == 1,
            },
            // CMYK JPEGs have no `/ColorSpace` mapping for the
            // `/DCTDecode` passthrough — decode to RGB like Java's
            // ImageIO path does, skipping only what won't decode.
            _ => {
                let Ok(dynamic) = image::load_from_memory(bytes) else {
                    return Ok(false);
                };
                raw_from_dynamic(&dynamic, "color")
            }
        }
    } else {
        let Ok(dynamic) = image::load_from_memory(bytes) else {
            return Ok(false);
        };
        raw_from_dynamic(&dynamic, "color")
    };
    let (w, h) = (image.width() as f32, image.height() as f32);
    commit_image_page(doc, &image, w, h)?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// CBR ↔ PDF (`/convert/cbr/pdf`, `/convert/pdf/cbr`,
// ConvertImgPDFController.convertCbrToPdf/convertPdfToCbr +
// CbrUtils/PdfToCbrUtils)
// ---------------------------------------------------------------------------

/// RAR extractors in preference order. Java decodes archives in-process
/// with junrar and needs no sidecar; Rust has no pure-Rust RAR reader, so
/// the first available CLI wins and a missing set is a hard error —
/// standalone deployments must install one of these.
const RAR_EXTRACTORS: &[&str] = &["unrar", "unar", "bsdtar", "7z"];

fn rar_extractor_available() -> bool {
    RAR_EXTRACTORS
        .iter()
        .any(|cmd| convert::find_executable(cmd, &[]).is_some())
}

/// Extraction argv per tool, run inside `dest`. `unrar`/`unar`/`bsdtar`
/// all preserve archive paths under the destination; `7z` needs its
/// `-o<dir>` switch glued to the path.
fn rar_extract_specs(archive: &Path, dest: &Path) -> Vec<EngineSpec> {
    let archive = archive.to_string_lossy().to_string();
    let dest_s = dest.to_string_lossy().to_string();
    vec![
        EngineSpec::new("unrar")
            .args(["x", "-o+"])
            .arg(&archive)
            .cwd(dest.to_path_buf())
            .timeout(std::time::Duration::from_secs(300)),
        EngineSpec::new("unar")
            .args(["-o", &dest_s, "-f"])
            .arg(&archive)
            .timeout(std::time::Duration::from_secs(300)),
        EngineSpec::new("bsdtar")
            .args(["-xf", &archive, "-C", &dest_s])
            .timeout(std::time::Duration::from_secs(300)),
        EngineSpec::new("7z")
            .args(["x", "-y", &format!("-o{dest_s}"), &archive])
            .timeout(std::time::Duration::from_secs(300)),
    ]
}

/// First extractor that exits 0 wins; `false` means every present
/// candidate failed — the caller reports Java's invalid-format error.
fn extract_rar_archive(archive: &Path, dest: &Path) -> bool {
    for spec in rar_extract_specs(archive, dest) {
        if convert::find_executable(&spec.program, &spec.path_dirs).is_none() {
            continue;
        }
        if let Ok(out) = convert::run(&spec) {
            if !out.timed_out && out.code == Some(0) {
                return true;
            }
        }
    }
    false
}

/// Recursive counterpart of [`read_dir_files`]: returns `(archive-relative
/// path, filesystem path)` pairs for regular files, `/`-separated like
/// the entry names Java's `FileHeader.getFileName()` yields.
fn collect_files_recursive(dir: &Path, prefix: &str, out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if path.is_dir() {
            collect_files_recursive(&path, &rel, out);
        } else {
            out.push((rel, path));
        }
    }
}

/// `POST /api/v1/convert/cbr/pdf` — `CbrUtils.convertCbrToPdf`: extract
/// the archive, natural-sort the image entries, one PDF page per image
/// sized to the pixel dimensions. Java decodes RAR in-process (junrar);
/// Rust needs one of [`RAR_EXTRACTORS`] on PATH — there is no Java
/// fallback, so a missing extractor is a hard dependency error.
pub async fn cbr_pdf(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, params) = receive_multipart(multipart).await?;
    if !rar_extractor_available() {
        return Err(AppError::Internal(anyhow::anyhow!(
            "missing dependency: install one of {} to convert CBR/RAR archives",
            RAR_EXTRACTORS.join(", ")
        )));
    }
    dispatch(state, query, uploads, params, cbr_pdf_work).await
}

fn cbr_pdf_work(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let upload = first_upload(uploads)?;
    let filename = upload
        .name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&upload.name);
    if filename.trim().is_empty() {
        return Err(AppError::BadRequest("file input has no name".into()));
    }
    let ext = filename
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    if !matches!(ext.as_str(), "cbr" | "rar") {
        return Err(AppError::BadRequest(format!(
            "'{filename}' is not a CBR file"
        )));
    }

    let dir =
        tempfile::tempdir().map_err(|e| AppError::Internal(anyhow::anyhow!("tempdir: {e}")))?;
    let archive = dir.path().join("input.cbr");
    let extract_dir = dir.path().join("extracted");
    std::fs::create_dir_all(&extract_dir)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("extract dir: {e}")))?;
    std::fs::write(&archive, upload.bytes.as_ref())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("write archive: {e}")))?;

    if !extract_rar_archive(&archive, &extract_dir) {
        return Err(AppError::BadRequest(format!(
            "'{filename}' is not a valid CBR/RAR archive"
        )));
    }

    let mut files = Vec::new();
    collect_files_recursive(&extract_dir, "", &mut files);
    let mut images: Vec<(String, PathBuf)> = files
        .into_iter()
        .filter(|(name, _)| cbz_image_entry(name))
        .collect();
    if images.is_empty() {
        return Err(AppError::BadRequest(format!(
            "'{filename}' contains no images"
        )));
    }
    images.sort_by(|a, b| natural_order_cmp(&a.0, &b.0));

    let mut doc = pdfdoc::new_document();
    for (_, path) in &images {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        commit_archive_image(&mut doc, &bytes)?;
    }
    if pdfdoc::page_ids(&doc).is_empty() {
        return Err(AppError::BadRequest(format!(
            "'{filename}' contains no readable images"
        )));
    }
    let mut bytes = pdfdoc::save(&mut doc)?;

    // `optimizeForEbook` runs Ghostscript's ebook preset; Java silently
    // disables it when gs is absent and falls back to the unoptimized
    // file when the run fails — both reproduced here.
    if param_bool(&params, "optimizeForEbook") && convert::find_executable("gs", &[]).is_some() {
        let out_dir = dir.path().join("optimized");
        std::fs::create_dir_all(&out_dir)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("optimize dir: {e}")))?;
        let input = dir.path().join("optimize_in.pdf");
        let output = out_dir.join("out.pdf");
        if std::fs::write(&input, &bytes).is_ok() {
            let spec = EngineSpec::new("gs")
                .arg("-sDEVICE=pdfwrite")
                .arg("-dPDFSETTINGS=/ebook")
                .arg("-dFastWebView=true")
                .arg("-dNOPAUSE")
                .arg("-dQUIET")
                .arg("-dBATCH")
                .arg(format!("-sOutputFile={}", output.display()))
                .arg(input.to_string_lossy().to_string());
            if let Ok(out) = convert::run(&spec) {
                if out.code == Some(0) {
                    if let Ok(optimized) = std::fs::read(&output) {
                        bytes = optimized;
                    }
                }
            }
        }
    }

    Ok(pdf_response(
        bytes,
        &output_name(Some(&upload.name), "_converted.pdf"),
    ))
}

/// `POST /api/v1/convert/pdf/cbr` — `PdfToCbrUtils.convertPdfToCbr`:
/// rasterise every page through Ghostscript, then pack the PNGs with the
/// `rar` CLI exactly like Java (`rar a -m5 -ep1 output.cbr page_*.png`).
/// The `rar` group probe in `endpointconfig` already 403s this endpoint
/// when the binary is absent, matching Java's group-enable semantics.
pub async fn pdf_to_cbr(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, params) = receive_multipart(multipart).await?;
    dispatch(state, query, uploads, params, pdf_to_cbr_work).await
}

fn pdf_to_cbr_work(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let upload = first_upload(uploads)?;
    let filename = upload
        .name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&upload.name);
    let ext = filename
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    if !filename.trim().is_empty() && ext != "pdf" {
        return Err(AppError::BadRequest(format!(
            "'{filename}' is not a PDF file"
        )));
    }
    if filename.trim().is_empty() {
        return Err(AppError::BadRequest("file input has no name".into()));
    }

    let mut doc = pdfdoc::load(&upload.name, &upload.bytes)?;
    if pdfdoc::page_ids(&doc).is_empty() {
        return Err(AppError::BadRequest("PDF document has no pages".into()));
    }
    let pdf_bytes = pdfdoc::save(&mut doc)?;

    // The request field defaults to 150 when `dpi` is absent; only an
    // explicit non-positive value falls through to the controller's 300.
    let dpi_raw = param(&params, "dpi", "150");
    let dpi: i64 = dpi_raw
        .trim()
        .parse()
        .map_err(|_| AppError::BadRequest(format!("invalid dpi: {dpi_raw}")))?;
    let dpi = if dpi <= 0 { 300 } else { dpi };

    let dir =
        tempfile::tempdir().map_err(|e| AppError::Internal(anyhow::anyhow!("tempdir: {e}")))?;
    let pages = gs_render_pages_png(dir.path(), &pdf_bytes, dpi)?;

    // Java renames the gs output to `page_%03d.png` and archives by bare
    // filename with `-ep1` (exclude paths) so members store flat names.
    let mut members = Vec::with_capacity(pages.len());
    for (i, page) in pages.iter().enumerate() {
        let name = format!("page_{:03}.png", i + 1);
        let target = dir.path().join(&name);
        std::fs::rename(page, &target)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("rename page image: {e}")))?;
        members.push(name);
    }
    let mut rar_args: Vec<String> = vec![
        "a".to_string(),
        "-m5".to_string(),
        "-ep1".to_string(),
        "output.cbr".to_string(),
    ];
    rar_args.extend(members);
    let spec = EngineSpec::new("rar")
        .args(rar_args)
        .cwd(dir.path().to_path_buf())
        .timeout(std::time::Duration::from_secs(600));
    run_engine(&spec)?;

    let cbr = dir.path().join("output.cbr");
    let bytes = std::fs::read(&cbr)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("rar produced no output.cbr: {e}")))?;
    if bytes.is_empty() {
        return Err(AppError::Internal(anyhow::anyhow!(
            "rar produced an empty output.cbr"
        )));
    }
    let base = output_name(Some(&upload.name), "");
    Ok(file_response(
        bytes,
        &format!("{base}_converted.cbr"),
        "application/octet-stream",
    ))
}

// ---------------------------------------------------------------------------
// SVG → PDF (`/convert/svg/pdf`, ConvertSvgToPDF)
// ---------------------------------------------------------------------------

/// Java sanitises the SVG DOM before rendering; the Rust path rejects the
/// same active-content markers up front instead. The renderers used here
/// (librsvg, Inkscape) do not execute script either way.
pub async fn svg_to_pdf(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, params) = receive_multipart(multipart).await?;
    dispatch(state, query, uploads, params, svg_to_pdf_work).await
}

fn svg_to_pdf_work(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let combine = param_bool(&params, "combineIntoSinglePdf");

    let mut svgs: Vec<(String, Vec<u8>)> = Vec::new();
    for upload in &uploads {
        if upload.bytes.is_empty() {
            continue;
        }
        if !upload.name.to_lowercase().ends_with(".svg") {
            continue;
        }
        let bytes: &[u8] = upload.bytes.as_ref();
        let lower = String::from_utf8_lossy(bytes).to_lowercase();
        for marker in [
            "<script",
            "javascript:",
            "<foreignobject",
            "<iframe",
            "<object",
            "<embed",
            "vbscript:",
        ] {
            if lower.contains(marker) {
                return Err(AppError::BadRequest(format!(
                    "'{}' contains disallowed SVG content ({marker})",
                    upload.name
                )));
            }
        }
        svgs.push((upload.name.clone(), bytes.to_vec()));
    }
    if svgs.is_empty() {
        return Err(AppError::BadRequest("No valid SVG files were found".into()));
    }

    let mut pdfs = Vec::with_capacity(svgs.len());
    for (name, bytes) in &svgs {
        pdfs.push(svg_single_pdf(name, bytes)?);
    }

    if combine {
        let mut doc = pdfdoc::new_document();
        for (i, bytes) in pdfs.iter().enumerate() {
            let mut src = pdfdoc::load(&svgs[i].0, bytes)?;
            let pages = pdfdoc::page_ids(&src);
            let order: Vec<usize> = (0..pages.len()).collect();
            pdfdoc::import_pages(&mut doc, &mut src, &order)?;
        }
        let bytes = pdfdoc::save(&mut doc)?;
        let name = output_name(svgs.first().map(|(n, _)| n.as_str()), "_combined.pdf");
        return Ok(pdf_response(bytes, &name));
    }

    if pdfs.len() == 1 {
        return Ok(pdf_response(
            pdfs.remove(0),
            &output_name(Some(&svgs[0].0), ".pdf"),
        ));
    }

    let mut entries = Vec::with_capacity(pdfs.len());
    for ((name, _), pdf) in svgs.iter().zip(pdfs) {
        entries.push((output_name(Some(name), ".pdf"), pdf));
    }
    let zip_name = output_name(svgs.first().map(|(n, _)| n.as_str()), "_converted_svgs.zip");
    Ok(file_response(
        zip_bytes(&entries)?,
        &zip_name,
        "application/zip",
    ))
}

/// One SVG → one single-page PDF. Prefers `rsvg-convert` (raster, page
/// sized to the SVG viewport like Batik's document size); falls back to
/// `inkscape`, which exports a real vector PDF.
fn svg_single_pdf(name: &str, bytes: &[u8]) -> AppResult<Vec<u8>> {
    let dir =
        tempfile::tempdir().map_err(|e| AppError::Internal(anyhow::anyhow!("tempdir: {e}")))?;
    let input = dir.path().join("input.svg");
    std::fs::write(&input, bytes)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("write svg: {e}")))?;

    if convert::find_executable("rsvg-convert", &[]).is_some() {
        let png = dir.path().join("out.png");
        let spec = EngineSpec::new("rsvg-convert")
            .arg("-o")
            .arg(png.to_string_lossy().to_string())
            .arg(input.to_string_lossy().to_string());
        run_engine(&spec)?;
        let png_bytes = std::fs::read(&png)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("read rsvg output: {e}")))?;
        let dynamic = image::load_from_memory(&png_bytes)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("decode rsvg png: {e}")))?;
        let (w_pt, h_pt) = svg_viewport_pt(bytes);
        let mut doc = pdfdoc::new_document();
        let image = raw_from_dynamic(&dynamic, "color");
        commit_image_page(&mut doc, &image, w_pt, h_pt)?;
        return pdfdoc::save(&mut doc);
    }

    if convert::find_executable("inkscape", &[]).is_some() {
        let pdf = dir.path().join("out.pdf");
        let mut spec = EngineSpec::new("inkscape")
            .arg(input.to_string_lossy().to_string())
            .arg("--export-type=pdf")
            .arg(format!("--export-filename={}", pdf.display()));
        let out = convert::run(&spec)?;
        if out.code != Some(0) || !pdf.exists() {
            // Inkscape <1.0 uses -o instead of --export-filename.
            let pdf2 = dir.path().join("out2.pdf");
            spec = EngineSpec::new("inkscape")
                .arg(input.to_string_lossy().to_string())
                .arg("-o")
                .arg(pdf2.to_string_lossy().to_string());
            run_engine(&spec)?;
            return std::fs::read(&pdf2)
                .map_err(|e| AppError::Internal(anyhow::anyhow!("read inkscape output: {e}")));
        }
        return std::fs::read(&pdf)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("read inkscape output: {e}")));
    }

    Err(AppError::Internal(anyhow::anyhow!(
        "SVG to PDF requires rsvg-convert or inkscape on PATH (input: {name})"
    )))
}

/// SVG viewport in PDF points. `width`/`height` win over `viewBox`; units
/// convert at 96 CSS px per inch (0.75 pt/px), matching Batik's sizing.
/// Missing dimensions fall back to the Java defaults (595x842, A4-ish).
fn svg_viewport_pt(bytes: &[u8]) -> (f32, f32) {
    let text = String::from_utf8_lossy(bytes);
    let tag_end = text.find('>').unwrap_or(text.len());
    let tag = &text[..tag_end];
    let width = svg_attr(tag, "width").and_then(svg_unit_to_pt);
    let height = svg_attr(tag, "height").and_then(svg_unit_to_pt);
    if let (Some(w), Some(h)) = (width, height) {
        if w > 0.0 && h > 0.0 {
            return (w, h);
        }
    }
    if let Some(view_box) = svg_attr(tag, "viewBox") {
        let nums: Vec<f32> = view_box
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter_map(|s| s.parse::<f32>().ok())
            .collect();
        if nums.len() == 4 && nums[2] > 0.0 && nums[3] > 0.0 {
            return (nums[2] * 0.75, nums[3] * 0.75);
        }
    }
    (595.0, 842.0)
}

fn svg_attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("{name}=");
    // Require whitespace before the name so `stroke-width=` can't match
    // `width=`.
    let mut search_from = 0usize;
    let pos = loop {
        let found = tag[search_from..].find(&needle)? + search_from;
        if found == 0 || tag.as_bytes()[found - 1].is_ascii_whitespace() {
            break found;
        }
        search_from = found + needle.len();
        if search_from >= tag.len() {
            return None;
        }
    };
    let rest = &tag[pos + needle.len()..];
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let end = rest[1..].find(quote)? + 1;
    Some(&rest[1..end])
}

fn svg_unit_to_pt(value: &str) -> Option<f32> {
    let value = value.trim();
    let num_end = value
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+'))
        .unwrap_or(value.len());
    let num: f32 = value[..num_end].parse().ok()?;
    let unit = value[num_end..].trim().to_ascii_lowercase();
    let factor = match unit.as_str() {
        "" | "px" => 0.75,
        "pt" => 1.0,
        "pc" => 12.0,
        "in" => 72.0,
        "cm" => 72.0 / 2.54,
        "mm" => 72.0 / 25.4,
        "q" => 72.0 / 101.6,
        _ => return None, // %, em, ex — viewport-relative, not resolvable here
    };
    Some(num * factor)
}

// ---------------------------------------------------------------------------
// CBZ → PDF (`/convert/cbz/pdf`, ConvertImgPDFController.convertCbzToPdf +
// CbzUtils.convertCbzToPdf)
// ---------------------------------------------------------------------------

/// Java's `getImageFilePattern`: `.*\.(jpg|jpeg|png|gif|bmp|webp)$`.
fn cbz_image_entry(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".jpg", ".jpeg", ".png", ".gif", ".bmp", ".webp"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

/// `POST /api/v1/convert/cbz/pdf` — unzip, natural-sort the image entries,
/// one PDF page per image sized to the pixel dimensions (Java's
/// `PDRectangle(image.width, image.height)` + `drawImage(img, 0, 0)`).
pub async fn cbz_pdf(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    _headers: HeaderMap,
    _uri: OriginalUri,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, params) = receive_multipart(multipart).await?;
    dispatch(state, query, uploads, params, cbz_pdf_work).await
}

fn cbz_pdf_work(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let upload = first_upload(uploads)?;
    let filename = upload
        .name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&upload.name);
    if filename.trim().is_empty() {
        return Err(AppError::BadRequest("file input has no name".into()));
    }
    let ext = filename
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    if !matches!(ext.as_str(), "cbz" | "zip") {
        return Err(AppError::BadRequest(format!(
            "'{filename}' is not a CBZ file"
        )));
    }

    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(upload.bytes.as_ref()))
        .map_err(|e| AppError::BadRequest(format!("'{filename}' is not a valid CBZ: {e}")))?;
    if archive.is_empty() {
        return Err(AppError::BadRequest(format!(
            "'{filename}' is an empty CBZ archive"
        )));
    }

    // `by_index` borrows `archive` mutably per call, so the names are
    // collected in a loop — an iterator chain would try to return
    // `ZipFile`s that outlive each closure call.
    let mut names = Vec::new();
    for i in 0..archive.len() {
        if let Ok(entry) = archive.by_index(i) {
            if !entry.is_dir() && cbz_image_entry(entry.name()) {
                names.push(entry.name().to_string());
            }
        }
    }
    if names.is_empty() {
        return Err(AppError::BadRequest(format!(
            "'{filename}' contains no images"
        )));
    }
    names.sort_by(|a, b| natural_order_cmp(a, b));

    let mut doc = pdfdoc::new_document();
    for name in &names {
        let Ok(mut entry) = archive.by_name(name) else {
            continue;
        };
        let mut bytes = Vec::new();
        if entry.read_to_end(&mut bytes).is_err() {
            continue;
        }
        commit_archive_image(&mut doc, &bytes)?;
    }
    if pdfdoc::page_ids(&doc).is_empty() {
        return Err(AppError::BadRequest(format!(
            "'{filename}' contains no readable images"
        )));
    }
    let mut bytes = pdfdoc::save(&mut doc)?;

    // `optimizeForEbook` runs Ghostscript's ebook preset; Java silently
    // disables it when gs is absent and falls back to the unoptimized
    // file when the run fails — both reproduced here.
    if param_bool(&params, "optimizeForEbook") && convert::find_executable("gs", &[]).is_some() {
        let dir =
            tempfile::tempdir().map_err(|e| AppError::Internal(anyhow::anyhow!("tempdir: {e}")))?;
        let input = dir.path().join("in.pdf");
        let output = dir.path().join("out.pdf");
        if std::fs::write(&input, &bytes).is_ok() {
            let spec = EngineSpec::new("gs")
                .arg("-sDEVICE=pdfwrite")
                .arg("-dPDFSETTINGS=/ebook")
                .arg("-dFastWebView=true")
                .arg("-dNOPAUSE")
                .arg("-dQUIET")
                .arg("-dBATCH")
                .arg(format!("-sOutputFile={}", output.display()))
                .arg(input.to_string_lossy().to_string());
            if let Ok(out) = convert::run(&spec) {
                if out.code == Some(0) {
                    if let Ok(optimized) = std::fs::read(&output) {
                        bytes = optimized;
                    }
                }
            }
        }
    }

    Ok(pdf_response(
        bytes,
        &output_name(Some(&upload.name), "_converted.pdf"),
    ))
}

/// `CbzUtils.NaturalOrderComparator`: digit runs compare numerically,
/// everything else lexically, chunk by chunk.
fn natural_order_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    fn chunk(s: &str, mut i: usize) -> (&str, usize) {
        let bytes = s.as_bytes();
        let digit = bytes[i].is_ascii_digit();
        let start = i;
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() == digit {
            i += 1;
        }
        (&s[start..i], i)
    }
    let (mut ca, mut cb) = (a, b);
    loop {
        if ca.is_empty() || cb.is_empty() {
            // Java compares the original full lengths once either side is
            // exhausted — "a1" sorts before "a01" even though the digit
            // chunks compare equal.
            return a.len().cmp(&b.len());
        }
        let (cha, na) = chunk(ca, 0);
        let (chb, nb) = chunk(cb, 0);
        let ord = match (cha.parse::<u64>(), chb.parse::<u64>()) {
            (Ok(x), Ok(y))
                if cha.bytes().all(|b| b.is_ascii_digit())
                    && chb.bytes().all(|b| b.is_ascii_digit()) =>
            {
                x.cmp(&y)
            }
            _ => cha.cmp(chb),
        };
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
        ca = &ca[na..];
        cb = &cb[nb..];
    }
}

// ---------------------------------------------------------------------------
// Office → PDF (`/convert/file/pdf`, ConvertOfficeController)
// ---------------------------------------------------------------------------

/// `FilenameUtils.getExtension` + the Java validation pattern
/// (`^[a-zA-Z0-9]{2,4}$`, case-insensitive).
fn office_extension(filename: &str) -> Option<String> {
    let base = filename.rsplit(['/', '\\']).next().unwrap_or(filename);
    let dot = base.rfind('.')?;
    let ext = &base[dot + 1..];
    (ext.len() >= 2 && ext.len() <= 4 && ext.bytes().all(|b| b.is_ascii_alphanumeric()))
        .then(|| ext.to_ascii_lowercase())
}

/// `POST /api/v1/convert/file/pdf` — LibreOffice/`unoconvert` via
/// `office_convert`. `useStirlingOfficeConvert` selects Java's in-process
/// converter, which has no Rust equivalent; the flag is accepted but the
/// LibreOffice path always runs (documented divergence).
pub async fn office_to_pdf(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, params) = receive_multipart(multipart).await?;
    dispatch(state, query, uploads, params, office_to_pdf_work).await
}

fn office_to_pdf_work(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let _ = param(&params, "useStirlingOfficeConvert", "");
    let upload = first_upload(uploads)?;
    let filename = upload
        .name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&upload.name);
    if filename.trim().is_empty() {
        return Err(AppError::BadRequest("file input has no name".into()));
    }
    let ext = office_extension(filename)
        .ok_or_else(|| AppError::BadRequest(format!("Invalid file extension: {filename}")))?;
    let base = filename
        .rsplit_once('.')
        .map(|(b, _)| b)
        .filter(|b| !b.trim().is_empty())
        .unwrap_or("input");

    let dir =
        tempfile::tempdir().map_err(|e| AppError::Internal(anyhow::anyhow!("tempdir: {e}")))?;
    let input_dir = dir.path().join("in");
    let out_dir = dir.path().join("out");
    std::fs::create_dir_all(&input_dir)
        .and_then(|_| std::fs::create_dir_all(&out_dir))
        .map_err(|e| AppError::Internal(anyhow::anyhow!("workdirs: {e}")))?;
    let input = input_dir.join(format!("{base}.{ext}"));
    std::fs::write(&input, upload.bytes.as_ref())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("write input: {e}")))?;

    let pdf = office_convert(&input, &out_dir, "pdf", None)?;
    // Java reloads the result through the PDF factory before responding;
    // the round-trip also rejects engines that emitted non-PDF bytes.
    let mut doc = pdfdoc::load("converted.pdf", &pdf)?;
    let bytes = pdfdoc::save(&mut doc)?;
    Ok(pdf_response(
        bytes,
        &output_name(Some(&upload.name), "_convertedToPDF.pdf"),
    ))
}

/// LibreOffice conversion shared by `/file/pdf` and `/pdf/*` office
/// outputs: `unoconvert` first (the shim Java prefers), `soffice` as
/// fallback. `infilter` is the `--infilter=`/`--input-filter=` value Java
/// passes for PDF inputs (`writer_pdf_import`, `impress_pdf_import`).
/// Returns the produced file's bytes.
fn office_convert(
    input: &Path,
    out_dir: &Path,
    to: &str,
    infilter: Option<&str>,
) -> AppResult<Vec<u8>> {
    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("input")
        .to_string();
    let ext = to.split(':').next().unwrap_or(to);

    // unoconvert --convert-to <to> [--input-filter=<f>] <in> <out>
    let unoconvert = settings::engine_program("unoconvert");
    if convert::find_executable(&unoconvert, &[]).is_some() {
        let out_path = out_dir.join(format!("{stem}.{ext}"));
        let spec = EngineSpec::new(&unoconvert)
            .arg("--convert-to")
            .arg(to)
            .args(infilter.map(|f| format!("--input-filter={f}")))
            .arg(input.to_string_lossy().to_string())
            .arg(out_path.to_string_lossy().to_string())
            .timeout(std::time::Duration::from_secs(300));
        if let Ok(out) = convert::run(&spec) {
            if out.code == Some(0) && out_path.exists() {
                return std::fs::read(&out_path).map_err(|e| {
                    AppError::Internal(anyhow::anyhow!("read unoconvert output: {e}"))
                });
            }
        }
    }

    // soffice -env:UserInstallation=<profile> --headless --nologo
    //   [--infilter=<f>] --convert-to <to> --outdir <dir> <in>
    let profile =
        tempfile::tempdir().map_err(|e| AppError::Internal(anyhow::anyhow!("profile dir: {e}")))?;
    let profile_uri = format!("file://{}", profile.path().display());
    let mut spec = EngineSpec::new(&settings::engine_program("soffice"))
        .arg(format!("-env:UserInstallation={profile_uri}"))
        .arg("--headless")
        .arg("--nologo");
    if let Some(f) = infilter {
        spec = spec.arg(format!("--infilter={f}"));
    }
    spec = spec
        .arg("--convert-to")
        .arg(to)
        .arg("--outdir")
        .arg(out_dir.to_string_lossy().to_string())
        .arg(input.to_string_lossy().to_string())
        .timeout(std::time::Duration::from_secs(600));
    run_engine(&spec)?;

    let expected = out_dir.join(format!("{stem}.{ext}"));
    let produced = if expected.exists() {
        expected
    } else {
        // LibreOffice may deviate on exotic names — Java falls back to any
        // produced file in the output directory.
        read_dir_files(out_dir)?
            .into_iter()
            .find(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case(ext))
            })
            .ok_or_else(|| {
                AppError::Internal(anyhow::anyhow!(
                    "LibreOffice produced no {ext} file for {}",
                    input.display()
                ))
            })?
    };
    let bytes = std::fs::read(&produced)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("read office output: {e}")))?;
    if bytes.is_empty() {
        return Err(AppError::Internal(anyhow::anyhow!(
            "LibreOffice produced an empty {ext} file"
        )));
    }
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// PDF → Office/text (`/convert/pdf/{word,presentation,xml,text}`,
// ConvertPDFToOffice)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum PdfToOffice {
    Word,
    Presentation,
    Xml,
    Text,
}

macro_rules! pdf_to_office_route {
    ($name:ident, $kind:expr) => {
        pub async fn $name(
            state: State<AppState>,
            query: Query<HashMap<String, String>>,
            multipart: Multipart,
        ) -> AppResult<Response> {
            let (uploads, params) = receive_multipart(multipart).await?;
            let kind = $kind;
            dispatch(state.0, query.0, uploads, params, move |s, q, u, p| {
                pdf_to_office_work(s, q, u, p, kind)
            })
            .await
        }
    };
}

pdf_to_office_route!(pdf_to_word, PdfToOffice::Word);
pdf_to_office_route!(pdf_to_presentation, PdfToOffice::Presentation);
pdf_to_office_route!(pdf_to_xml, PdfToOffice::Xml);
pdf_to_office_route!(pdf_to_text, PdfToOffice::Text);

fn pdf_to_office_work(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
    kind: PdfToOffice,
) -> AppResult<Response> {
    let upload = first_upload(uploads)?;
    let format = param(&params, "outputFormat", "");
    let is_pdf = upload
        .content_type
        .as_deref()
        .is_some_and(|ct| ct == "application/pdf");

    // txt is the one format Java extracts natively (PDFBox text stripper);
    // every office format goes through LibreOffice and requires a typed
    // PDF upload, matching PDFToFile's content-type gate.
    if kind == PdfToOffice::Text && format == "txt" {
        let doc = pdfdoc::load(&upload.name, &upload.bytes)?;
        let pages = pdfdoc::page_ids(&doc).len() as u32;
        // lopdf's extract_text keys pages by get_pages()' 1-based numbers.
        let indices: Vec<u32> = (1..=pages).collect();
        let text = doc
            .extract_text(&indices)
            .map_err(|e| AppError::Pdf(format!("text extraction: {e}")))?;
        let name = output_name(Some(&upload.name), ".txt");
        return Ok(file_response(text.into_bytes(), &name, "text/plain"));
    }

    let (allowed, filter): (&[&str], &str) = match kind {
        PdfToOffice::Word => (&["doc", "docx", "odt"], "writer_pdf_import"),
        PdfToOffice::Presentation => (&["ppt", "pptx", "odp"], "impress_pdf_import"),
        PdfToOffice::Xml => (&["xml"], "writer_pdf_import"),
        PdfToOffice::Text => (&["rtf"], "writer_pdf_import"),
    };
    if format.is_empty() || !allowed.contains(&format) {
        return Err(AppError::BadRequest("unsupported outputFormat".into()));
    }
    if !is_pdf {
        return Err(AppError::BadRequest("input must be a PDF file".into()));
    }

    let dir =
        tempfile::tempdir().map_err(|e| AppError::Internal(anyhow::anyhow!("tempdir: {e}")))?;
    let input_dir = dir.path().join("in");
    let out_dir = dir.path().join("out");
    std::fs::create_dir_all(&input_dir)
        .and_then(|_| std::fs::create_dir_all(&out_dir))
        .map_err(|e| AppError::Internal(anyhow::anyhow!("workdirs: {e}")))?;
    let stem = output_name(Some(&upload.name), "");
    let input = input_dir.join(format!("{stem}.pdf"));
    std::fs::write(&input, upload.bytes.as_ref())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("write input: {e}")))?;

    // The converted bytes are re-read from `out_dir` below; the runner's
    // return value is only a success signal here.
    office_convert(&input, &out_dir, format, Some(filter))?;

    let files = read_dir_files(&out_dir)?
        .into_iter()
        .filter(|p| p != &input)
        .collect::<Vec<_>>();
    if files.len() == 1 {
        let bytes = std::fs::read(&files[0])
            .map_err(|e| AppError::Internal(anyhow::anyhow!("read output: {e}")))?;
        return Ok(file_response(
            bytes,
            &format!("{stem}.{format}"),
            "application/octet-stream",
        ));
    }
    let mut entries = Vec::with_capacity(files.len());
    for file in files {
        let bytes = std::fs::read(&file)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("read output: {e}")))?;
        let name = file
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("output")
            .to_string();
        entries.push((name, bytes));
    }
    Ok(file_response(
        zip_bytes(&entries)?,
        &format!("{stem}To{format}.zip"),
        "application/octet-stream",
    ))
}

// ---------------------------------------------------------------------------
// HTML → PDF (`/convert/html/pdf`, ConvertHtmlToPDF)
// ---------------------------------------------------------------------------

/// WeasyPrint is the only engine Java uses here (the Stirling Office
/// Convert branch has no Rust port; `useStirlingOfficeConvert=true` still
/// produces a PDF via WeasyPrint — documented divergence). HTML
/// sanitisation is not ported; `--allowed-protocols data` keeps the same
/// resource restriction Java applies to sanitised input.
pub async fn html_to_pdf(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, params) = receive_multipart(multipart).await?;
    dispatch(state, query, uploads, params, html_to_pdf_work).await
}

fn html_to_pdf_work(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let _ = param(&params, "useStirlingOfficeConvert", "");
    let _ = param(&params, "zoom", "1");
    let upload = first_upload(uploads)?;
    let filename = upload
        .name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&upload.name);
    if !filename.ends_with(".html") && !filename.ends_with(".zip") {
        return Err(AppError::BadRequest(
            "File must be in .html or .zip format".into(),
        ));
    }

    let dir =
        tempfile::tempdir().map_err(|e| AppError::Internal(anyhow::anyhow!("tempdir: {e}")))?;
    let html = if filename.ends_with(".zip") {
        html_from_zip(upload.bytes.as_ref())?
    } else {
        String::from_utf8_lossy(upload.bytes.as_ref()).into_owned()
    };
    let bytes = html_to_pdf_bytes(&html, dir.path())?;
    Ok(pdf_response(bytes, &output_name(Some(filename), ".pdf")))
}

/// The shared WeasyPrint invocation: HTML string → PDF bytes, round-tripped
/// through the PDF factory like Java's `createNewBytesBasedOnOldDocument`.
pub(crate) fn html_to_pdf_bytes(html: &str, dir: &std::path::Path) -> AppResult<Vec<u8>> {
    let input = dir.join("input.html");
    let output = dir.join("output.pdf");
    std::fs::write(&input, html)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("write html: {e}")))?;

    let spec = EngineSpec::new(&settings::engine_program("weasyprint"))
        .args([
            "-e".to_string(),
            "utf-8".to_string(),
            "-v".to_string(),
            "--pdf-forms".to_string(),
            "--allowed-protocols".to_string(),
            "data".to_string(),
            input.to_string_lossy().to_string(),
            output.to_string_lossy().to_string(),
        ])
        .timeout(std::time::Duration::from_secs(300));
    run_engine(&spec)?;

    let pdf = std::fs::read(&output)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("read weasyprint output: {e}")))?;
    let mut doc = pdfdoc::load("weasyprint.pdf", &pdf)?;
    pdfdoc::save(&mut doc)
}

/// `FileToPdf.htmlFromZip`: pick `index.html`/`index.htm`, else the
/// shallowest `.html`/`.htm` entry, then inline local `<img>` sources as
/// data URIs so WeasyPrint's `data`-only protocol allowlist can render
/// them.
fn html_from_zip(zip_bytes: &[u8]) -> AppResult<String> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip_bytes))
        .map_err(|e| AppError::BadRequest(format!("invalid zip: {e}")))?;

    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| AppError::BadRequest(format!("zip entry: {e}")))?;
        if file.is_dir() {
            continue;
        }
        let name = file.name().to_string();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|e| AppError::BadRequest(format!("zip read {name}: {e}")))?;
        entries.push((name, bytes));
    }

    let main = entries
        .iter()
        .map(|(n, _)| n.clone())
        .find(|n| {
            let l = n.to_lowercase();
            l == "index.html" || l == "index.htm"
        })
        .or_else(|| {
            entries
                .iter()
                .map(|(n, _)| n.clone())
                .filter(|n| {
                    let l = n.to_lowercase();
                    l.ends_with(".html") || l.ends_with(".htm")
                })
                .min_by_key(|n| (n.matches('/').count(), n.clone()))
        })
        .ok_or_else(|| AppError::BadRequest("zip contains no HTML file".into()))?;

    let html_bytes = &entries
        .iter()
        .find(|(n, _)| *n == main)
        .map(|(_, b)| b)
        .expect("main entry present")[..];
    let mut html = String::from_utf8_lossy(html_bytes).into_owned();

    let slash = main.rfind('/').map(|i| i + 1).unwrap_or(0);
    let base_dir = &main[..slash];
    html = inline_zip_images(&html, base_dir, &entries);
    Ok(html)
}

const IMAGE_MIME: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("bmp", "image/bmp"),
];

/// Replaces `<img src="relative/path">` with `data:` URIs resolved
/// against the zip's entry names. Remote/absolute/`data:` refs are left
/// untouched, matching `FileToPdf.toDataUri`.
fn inline_zip_images(html: &str, base_dir: &str, entries: &[(String, Vec<u8>)]) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(pos) = rest.find("<img") {
        out.push_str(&rest[..pos]);
        let tag_start = pos;
        let Some(tag_end_rel) = rest[tag_start..].find('>') else {
            out.push_str(&rest[tag_start..]);
            return out;
        };
        let tag_end = tag_start + tag_end_rel + 1;
        let tag = &rest[tag_start..tag_end];
        out.push_str(&inline_img_tag(tag, base_dir, entries));
        rest = &rest[tag_end..];
    }
    out.push_str(rest);
    out
}

fn inline_img_tag(tag: &str, base_dir: &str, entries: &[(String, Vec<u8>)]) -> String {
    // Find src="..." or src='...' inside the tag.
    let lower = tag.to_lowercase();
    let Some(src_pos) = lower.find("src=") else {
        return tag.to_string();
    };
    let after = &tag[src_pos + 4..];
    let after = after.trim_start();
    let Some(quote) = after.chars().next().filter(|c| *c == '"' || *c == '\'') else {
        return tag.to_string();
    };
    let Some(end) = after[1..].find(quote) else {
        return tag.to_string();
    };
    let reference = &after[1..1 + end];

    let data_uri = zip_image_data_uri(reference, base_dir, entries);
    let Some(data_uri) = data_uri else {
        return tag.to_string();
    };
    let value_start = src_pos + 4 + (tag[src_pos + 4..].len() - after.len()) + 1;
    let mut result = String::with_capacity(tag.len() + data_uri.len());
    result.push_str(&tag[..value_start]);
    result.push_str(&data_uri);
    result.push_str(&tag[value_start + reference.len()..]);
    result
}

fn zip_image_data_uri(
    reference: &str,
    base_dir: &str,
    entries: &[(String, Vec<u8>)],
) -> Option<String> {
    if reference.is_empty() || reference.starts_with("//") {
        return None;
    }
    // Any scheme (data:, http:, ...) is left alone.
    if reference.find(':').is_some_and(|i| {
        reference[..i]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    }) {
        return None;
    }
    let path = reference.split(['?', '#']).next()?;
    let joined = if path.starts_with('/') {
        path.trim_start_matches('/').to_string()
    } else {
        format!("{base_dir}{path}")
    };
    // Resolve . and .. segments without a filesystem.
    let mut parts: Vec<&str> = Vec::new();
    for seg in joined.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    let key = parts.join("/");
    let ext = key.rsplit('.').next()?.to_lowercase();
    let mime = IMAGE_MIME.iter().find(|(e, _)| *e == ext)?.1;
    let bytes = &entries.iter().find(|(n, _)| *n == key)?.1;
    Some(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

// ---------------------------------------------------------------------------
// URL → PDF (`/convert/url/pdf`, ConvertWebsiteToPDF)
// ---------------------------------------------------------------------------

/// Every pre-conversion rejection in Java is a 303 to the React
/// `/url-to-pdf` route with an `?error=` key it localises. The context path
/// `ServletUriComponentsBuilder` would prepend is not modelled here.
fn url_error_redirect(reason: &str) -> Response {
    Response::builder()
        .status(axum::http::StatusCode::SEE_OTHER)
        .header(
            axum::http::header::LOCATION,
            format!("/url-to-pdf?error={reason}"),
        )
        .body(axum::body::Body::empty())
        .expect("static response parts")
}

/// `POST /api/v1/convert/url/pdf` — fetch a remote page and print it with
/// WeasyPrint. Java's `system.enableUrlToPDF` gate is enforced upstream by
/// `endpointconfig`/`api_guard`, which 403s like Java's interceptor.
pub async fn url_to_pdf(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, params) = receive_multipart(multipart).await?;
    dispatch(state, query, uploads, params, url_to_pdf_work).await
}

fn url_to_pdf_work(
    _state: AppState,
    _query: HashMap<String, String>,
    _uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let url = param(&params, "urlInput", "");
    if !url_format_valid(url) {
        return Ok(url_error_redirect("error.invalidUrlFormat"));
    }
    if !url_reachable(url) {
        return Ok(url_error_redirect("error.urlNotReachable"));
    }
    let html = fetch_remote_html(url)?;
    if contains_disallowed_uri_scheme(&html) {
        return Ok(url_error_redirect("error.disallowedUrlContent"));
    }

    let dir =
        tempfile::tempdir().map_err(|e| AppError::Internal(anyhow::anyhow!("tempdir: {e}")))?;
    let input = dir.path().join("url_input_.html");
    let output = dir.path().join("output_.pdf");
    std::fs::write(&input, &html)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("write html: {e}")))?;

    let spec = EngineSpec::new(&settings::engine_program("weasyprint"))
        .args([
            input.to_string_lossy().to_string(),
            "--base-url".to_string(),
            url.to_string(),
            "--pdf-forms".to_string(),
            "--allowed-protocols".to_string(),
            "data,http,https".to_string(),
            output.to_string_lossy().to_string(),
        ])
        .timeout(std::time::Duration::from_secs(300));
    run_engine(&spec)?;

    let pdf = std::fs::read(&output)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("read weasyprint output: {e}")))?;
    let mut doc = pdfdoc::load("url.pdf", &pdf)?;
    let bytes = pdfdoc::save(&mut doc)?;
    Ok(pdf_response(bytes, &convert_url_to_file_name(url)))
}

/// `GeneralUtils.isValidURL` || `RegexPatternUtils.getHttpUrlPattern`:
/// the endpoint accepts the URL when EITHER check passes.
fn url_format_valid(url: &str) -> bool {
    http_url_pattern_matches(url) || is_valid_url(url)
}

/// `^https?://.*` with `matches()` semantics — `.` excludes Java's line
/// terminators, so any of them after the prefix fails the pattern.
fn http_url_pattern_matches(url: &str) -> bool {
    let Some(tail) = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
    else {
        return false;
    };
    !tail.contains(['\n', '\r', '\u{85}', '\u{2028}', '\u{2029}'])
}

/// `Urls.create(url, HTTP_PROTOCOLS, DENY_COMMON_INFRASTRUCTURE_TARGETS)`:
/// parses as http/https and the host is not a known infrastructure target.
/// `java.net.URL` is more lenient than `url::Url` (e.g. odd hosts); those
/// inputs fall through to the reachability check, which resolves them.
fn is_valid_url(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }
    let host = parsed.host_str().unwrap_or_default().trim();
    !matches!(
        host,
        "192.168.1.1" | "3232235777" | "169.254.169.254" | "2852039166"
    )
}

/// `GeneralUtils.isURLReachable`: HEAD probe, no redirects, 5s connect /
/// 5s read (modelled as a 10s total), reachable on any 2xx/3xx.
fn url_reachable(url_str: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(url_str) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    if host.trim().is_empty() || is_disallowed_network_location(host) {
        return false;
    }
    let client = match reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    client
        .head(url)
        .send()
        .map(|r| (200..=399).contains(&r.status().as_u16()))
        .unwrap_or(false)
}

/// `GeneralUtils.isDisallowedNetworkLocation`: literal IPs are checked
/// directly; names resolve via DNS and are rejected when resolution fails,
/// yields >20 addresses, or any address is sensitive.
fn is_disallowed_network_location(host: &str) -> bool {
    use std::net::ToSocketAddrs;
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return is_sensitive_address(&ip);
    }
    let addrs: Vec<std::net::IpAddr> = match (host, 0).to_socket_addrs() {
        Ok(it) => it.map(|sa| sa.ip()).collect(),
        Err(_) => return true,
    };
    const MAX_DNS_ADDRESSES: usize = 20;
    addrs.len() > MAX_DNS_ADDRESSES || addrs.iter().any(is_sensitive_address)
}

/// `GeneralUtils.isSensitiveAddress`: loopback, link-local, site-local,
/// multicast, unspecified, plus the private/reserved ranges
/// `isPrivateOrReservedIPv4` enumerates. IPv6 ULA and v4-mapped/compatible
/// addresses defer to the embedded IPv4 rules.
fn is_sensitive_address(ip: &std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            v4.is_unspecified()
                || v4.is_loopback()
                || is_link_local_v4(v4)
                || is_site_local_v4(v4)
                || v4.is_multicast()
                || is_private_or_reserved_v4(&v4.octets())
        }
        IpAddr::V6(v6) => {
            if v6.is_unspecified() || v6.is_loopback() || v6.is_multicast() {
                return true;
            }
            let o = v6.octets();
            // fe80::/10 link-local, fec0::/10 site-local, fc00::/7 ULA.
            if o[0] == 0xfe && (o[1] & 0xc0) == 0x80 {
                return true;
            }
            if o[0] == 0xfe && (o[1] & 0xc0) == 0xc0 {
                return true;
            }
            if (o[0] & 0xfe) == 0xfc {
                return true;
            }
            let mapped = o[..10].iter().all(|&b| b == 0) && o[10] == 0xff && o[11] == 0xff;
            let compatible = o[..12].iter().all(|&b| b == 0);
            if mapped || compatible {
                return is_private_or_reserved_v4(&o[12..16]);
            }
            false
        }
    }
}

fn is_link_local_v4(v4: &std::net::Ipv4Addr) -> bool {
    v4.octets()[0] == 169 && v4.octets()[1] == 254
}

fn is_site_local_v4(v4: &std::net::Ipv4Addr) -> bool {
    v4.octets()[0] == 192 && v4.octets()[1] == 168
}

/// `GeneralUtils.isPrivateOrReservedIPv4` — the exact range list Java
/// enforces, including documentation/benchmark ranges a plain
/// `is_private` check would miss.
fn is_private_or_reserved_v4(a: &[u8]) -> bool {
    if a.len() != 4 {
        return true;
    }
    let (first, second, third) = (a[0] as u16, a[1] as u16, a[2] as u16);
    if first == 0 || first == 127 {
        return true;
    }
    if first == 100 && (64..=127).contains(&second) {
        return true;
    }
    if first == 169 && second == 254 {
        return true;
    }
    if first == 172 && (16..=31).contains(&second) {
        return true;
    }
    if first == 192 && second == 0 && third == 0 {
        return true;
    }
    if first == 192 && second == 0 && third == 2 {
        return true;
    }
    if first == 192 && second == 168 {
        return true;
    }
    if first == 198 && (second == 18 || second == 19) {
        return true;
    }
    if first == 198 && second == 51 && third == 100 {
        return true;
    }
    if first == 203 && second == 0 && third == 113 {
        return true;
    }
    if first == 10 {
        return true;
    }
    first >= 224
}

/// `fetchRemoteHtml`: GET with no redirects, 10s connect / 20s total,
/// Stirling UA, body decoded as UTF-8 with replacement like
/// `BodyHandlers.ofString(UTF_8)`. Status >= 300 is an error, matching
/// Java's `IOException` → 500.
fn fetch_remote_html(url: &str) -> AppResult<String> {
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("http client: {e}")))?;
    let resp = client
        .get(url)
        .header(reqwest::header::USER_AGENT, "Stirling-PDF/URL-to-PDF")
        .send()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("error.httpRequestFailed: {e}")))?;
    if resp.status().as_u16() >= 300 {
        return Err(AppError::Internal(anyhow::anyhow!(
            "error.httpRequestFailed: Failed to retrieve remote HTML. Status: {}",
            resp.status().as_u16()
        )));
    }
    let bytes = resp
        .bytes()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("error.httpRequestFailed: {e}")))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// `containsDisallowedUriScheme`: normalise (lowercase, numeric HTML
/// entities, named entities, percent-decode) then look for
/// `file:` references that would let WeasyPrint read local files.
fn contains_disallowed_uri_scheme(html: &str) -> bool {
    let normalized = normalize_for_scheme_detection(html);
    find_file_scheme(&normalized)
}

fn normalize_for_scheme_detection(html: &str) -> String {
    let lower = html.to_lowercase();
    let decoded = decode_numeric_html_entities(&lower);
    let decoded = decoded
        .replace("&colon;", ":")
        .replace("&sol;", "/")
        .replace("&frasl;", "/");
    percent_decode(&decoded)
}

/// `&#(x?[0-9a-f]+);` on already-lowercased input. `(char)` truncation in
/// Java keeps the low 16 bits; lone surrogates degrade to U+FFFD here.
fn decode_numeric_html_entities(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(pos) = rest.find("&#") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 2..];
        let hex = after.starts_with('x');
        let digits_start = if hex { 1 } else { 0 };
        let digit_len = after[digits_start..]
            .find(|c: char| !c.is_ascii_hexdigit())
            .unwrap_or(after.len() - digits_start);
        let has_semi = digit_len > 0 && after[digits_start + digit_len..].starts_with(';');
        if has_semi {
            let digits = &after[digits_start..digits_start + digit_len];
            if let Ok(value) = usize::from_str_radix(digits, if hex { 16 } else { 10 }) {
                let unit = (value & 0xFFFF) as u32;
                out.push(char::from_u32(unit).unwrap_or('\u{FFFD}'));
                rest = &after[digits_start + digit_len + 1..];
                continue;
            }
        }
        out.push_str("&#");
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Single-pass `%XX` decode; malformed sequences stay literal, matching
/// Java's `percentDecode`.
fn percent_decode(content: &str) -> String {
    let bytes = content.as_bytes();
    let mut out = String::with_capacity(content.len());
    let mut last = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push_str(&content[last..i]);
                out.push(char::from_u32(hi * 16 + lo).expect("<= 0xFF"));
                i += 3;
                last = i;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&content[last..]);
    out
}

/// `(?<![a-z0-9_])file\s*:(?:/{1,3}|%2f|%5c|%3a|&#x2f;|&#47;)` on the
/// normalised text. The entity/percent alternatives survive only through
/// double-encoding (single-pass decode), so they are matched literally.
fn find_file_scheme(text: &str) -> bool {
    let bytes = text.as_bytes();
    let is_boundary =
        |i: usize| i == 0 || !bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_';
    let mut i = 0usize;
    while i + 4 <= bytes.len() {
        if &bytes[i..i + 4] == b"file" && is_boundary(i) {
            let mut j = i + 4;
            while j < bytes.len() && matches!(bytes[j], b' ' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r')
            {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b':' {
                let rest = &bytes[j + 1..];
                if rest.first() == Some(&b'/')
                    || rest.starts_with(b"%2f")
                    || rest.starts_with(b"%5c")
                    || rest.starts_with(b"%3a")
                    || rest.starts_with(b"&#x2f;")
                    || rest.starts_with(b"&#47;")
                {
                    return true;
                }
            }
        }
        i += 1;
    }
    false
}

/// `ConvertWebsiteToPDF.convertURLToFileName`: URL → filesystem-safe base
/// name, capped at 50 chars, `.pdf` appended.
fn convert_url_to_file_name(url: &str) -> String {
    let mut safe: String = url
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .take(50)
        .collect();
    if safe.trim().is_empty() {
        safe = reqwest::Url::parse(url)
            .ok()
            .and_then(|u| {
                u.host_str().map(|h| {
                    h.chars()
                        .map(|c| {
                            if c.is_ascii_alphanumeric() || c == '_' {
                                c
                            } else {
                                '_'
                            }
                        })
                        .collect::<String>()
                })
            })
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "document".to_string());
    }
    // `getNonAlnumUnderscorePattern` then `getMultipleUnderscoresPattern`:
    // ASCII-only after this point, runs of '_' collapse to one.
    safe = safe
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    safe = collapse_underscores(&safe);
    safe = safe.trim_matches('_').to_string();
    if safe.is_empty() {
        safe = "document".to_string();
    }
    safe = safe.chars().take(50).collect();
    format!("{safe}.pdf")
}

/// `getMultipleUnderscoresPattern` (`_+` → `_`).
fn collapse_underscores(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_us = false;
    for c in s.chars() {
        if c == '_' {
            if !prev_us {
                out.push('_');
            }
            prev_us = true;
        } else {
            out.push(c);
            prev_us = false;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Markdown → PDF (`/convert/markdown/pdf`, ConvertMarkdownToPdf)
// ---------------------------------------------------------------------------

/// `POST /api/v1/convert/markdown/pdf` — Markdown (or a ZIP of Markdown +
/// assets) → HTML via commonmark → WeasyPrint. `useStirlingOfficeConvert`
/// is accepted but ignored, same divergence as `html_to_pdf`.
pub async fn markdown_to_pdf(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, params) = receive_multipart(multipart).await?;
    dispatch(state, query, uploads, params, markdown_to_pdf_work).await
}

fn markdown_to_pdf_work(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let _ = param(&params, "useStirlingOfficeConvert", "");
    let upload = first_upload(uploads)?;
    let filename = upload
        .name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&upload.name)
        .to_string();
    let lower = filename.to_lowercase();
    let is_zip = lower.ends_with(".zip");
    let is_markdown = lower.ends_with(".md");
    if !is_zip && !is_markdown {
        return Err(AppError::BadRequest(
            "File must be in Markdown or ZIP format".into(),
        ));
    }

    let html = if is_zip {
        let entries = extract_markdown_zip(upload.bytes.as_ref())?;
        let key = find_markdown_key(&entries)
            .ok_or_else(|| AppError::BadRequest("ZIP must contain at least one .md file".into()))?;
        let markdown = std::str::from_utf8(&entries[&key])
            .map_err(|e| AppError::Internal(anyhow::anyhow!("read markdown: {e}")))?;
        let html = render_markdown_html(markdown)?;
        // `createHtmlZip` + `htmlFromZip`: the synthetic package holds
        // index.html at the root plus every non-.md entry, so images
        // resolve against an empty base dir.
        let assets: Vec<(String, Vec<u8>)> = entries
            .into_iter()
            .filter(|(name, _)| !name.to_lowercase().ends_with(".md"))
            .collect();
        inline_zip_images(&html, "", &assets)
    } else {
        let markdown = String::from_utf8_lossy(upload.bytes.as_ref()).into_owned();
        render_markdown_html(&markdown)?
    };

    let dir =
        tempfile::tempdir().map_err(|e| AppError::Internal(anyhow::anyhow!("tempdir: {e}")))?;
    let input = dir.path().join("converted.html");
    let output = dir.path().join("output.pdf");
    std::fs::write(&input, &html)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("write html: {e}")))?;

    // SANITIZED_ALLOWED_PROTOCOLS vs UNSANITIZED per `disableSanitize`;
    // the jsoup policy itself is not ported (same gap as html_to_pdf).
    let protocols = if crate::settings::current().system.disable_sanitize {
        "data,http,https"
    } else {
        "data"
    };
    let spec = EngineSpec::new(&settings::engine_program("weasyprint"))
        .args([
            "-e".to_string(),
            "utf-8".to_string(),
            "-v".to_string(),
            "--pdf-forms".to_string(),
            "--allowed-protocols".to_string(),
            protocols.to_string(),
            input.to_string_lossy().to_string(),
            output.to_string_lossy().to_string(),
        ])
        .timeout(std::time::Duration::from_secs(300));
    run_engine(&spec)?;

    let pdf = std::fs::read(&output)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("read weasyprint output: {e}")))?;
    let mut doc = pdfdoc::load("markdown.pdf", &pdf)?;
    let bytes = pdfdoc::save(&mut doc)?;
    Ok(pdf_response(bytes, &output_name(Some(&filename), ".pdf")))
}

/// commonmark-java + `TablesExtension` → HTML. comrak supplies the
/// CommonMark parser; only the table extension is enabled to match Java's
/// extension list. Raw HTML passes through like commonmark-java's renderer
/// (the downstream jsoup policy is the unported part).
fn render_markdown_html(markdown: &str) -> AppResult<String> {
    let mut options = comrak::Options::default();
    options.extension.table = true;
    options.render.r#unsafe = true;
    let arena = comrak::Arena::new();
    let root = comrak::parse_document(&arena, markdown, &options);
    let mut html = String::new();
    comrak::format_html(root, &options, &mut html)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("markdown render: {e}")))?;
    // TableAttributeProvider / CodeBlockAttributeProvider: comrak emits
    // bare `<table>`/`<pre>` tags for those nodes, so the attributes are
    // patched in afterwards. A literal `<table>` in raw HTML gets the
    // class too — Java's provider only tags real TableBlocks.
    let html = html.replace("<table>", "<table class=\"table table-striped\">");
    let html = html.replace(
        "<pre>",
        "<pre style=\"white-space: pre-wrap; word-wrap: break-word\">",
    );
    Ok(html)
}

/// `FileToPdf.normalizeEntryPath`: resolve `.`/`..` segments; `None` when
/// the path escapes the root, matching Java's null return.
fn normalize_entry_path(name: &str) -> Option<String> {
    let normalized = name.replace('\\', "/");
    let mut segments: Vec<&str> = Vec::new();
    for segment in normalized.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." {
            if segments.is_empty() {
                return None;
            }
            segments.pop();
        } else {
            segments.push(segment);
        }
    }
    if segments.is_empty() {
        None
    } else {
        Some(segments.join("/"))
    }
}

const ZIP_MAX_ENTRY_BYTES: u64 = 100 * 1024 * 1024;
const ZIP_MAX_TOTAL_BYTES: u64 = 500 * 1024 * 1024;
const ZIP_MAX_ENTRIES: usize = 10_000;

/// Extract a Markdown package ZIP into memory under `ZipBombGuard`'s
/// limits, keyed by `normalizeEntryPath`. Last duplicate wins like Java's
/// `Map.put`.
fn extract_markdown_zip(bytes: &[u8]) -> AppResult<HashMap<String, Vec<u8>>> {
    use std::io::Read as _;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| AppError::BadRequest(format!("invalid zip: {e}")))?;
    let mut entries = HashMap::new();
    let mut total = 0u64;
    let mut count = 0usize;
    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| AppError::BadRequest(format!("zip entry: {e}")))?;
        if file.is_dir() {
            continue;
        }
        let Some(key) = normalize_entry_path(file.name()) else {
            continue;
        };
        count += 1;
        if count > ZIP_MAX_ENTRIES {
            return Err(AppError::Internal(anyhow::anyhow!(
                "Archive exceeds the maximum of {ZIP_MAX_ENTRIES} entries"
            )));
        }
        let mut data = Vec::new();
        file.by_ref()
            .take(ZIP_MAX_ENTRY_BYTES + 1)
            .read_to_end(&mut data)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("zip read: {e}")))?;
        if data.len() as u64 > ZIP_MAX_ENTRY_BYTES {
            return Err(AppError::Internal(anyhow::anyhow!(
                "Archive entry exceeds the maximum allowed size of {ZIP_MAX_ENTRY_BYTES} bytes"
            )));
        }
        total += data.len() as u64;
        if total > ZIP_MAX_TOTAL_BYTES {
            return Err(AppError::Internal(anyhow::anyhow!(
                "Archive exceeds the maximum total size of {ZIP_MAX_TOTAL_BYTES} bytes"
            )));
        }
        entries.insert(key, data);
    }
    Ok(entries)
}

/// `findMarkdownFile`: root `index.md` first (case-sensitive, like
/// `File.exists`), else the first `.md` in walk order — approximated by
/// sorted order since Java's is filesystem-dependent.
fn find_markdown_key(entries: &HashMap<String, Vec<u8>>) -> Option<String> {
    if entries.contains_key("index.md") {
        return Some("index.md".to_string());
    }
    entries
        .keys()
        .filter(|k| k.to_lowercase().ends_with(".md"))
        .min()
        .cloned()
}

// ---------------------------------------------------------------------------
// eBook → PDF (`/convert/ebook/pdf`, ConvertEbookToPDFController)
// ---------------------------------------------------------------------------

const EBOOK_EXTENSIONS: &[&str] = &["epub", "mobi", "azw3", "fb2", "txt", "docx"];

/// `POST /api/v1/convert/ebook/pdf` — Calibre `ebook-convert` sidecar.
/// `useStirlingOfficeConvert` (Stirling Office Convert engine) and `.docx`
/// inputs (the OOXML external-reference sanitizer) are rejected — neither
/// engine nor sanitizer is ported.
pub async fn ebook_to_pdf(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    _headers: HeaderMap,
    _uri: OriginalUri,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, params) = receive_multipart(multipart).await?;
    let is_docx = uploads.first().is_some_and(|u| {
        let name = u.name.rsplit(['/', '\\']).next().unwrap_or(&u.name);
        name.rsplit('.')
            .next()
            .is_some_and(|e| e.eq_ignore_ascii_case("docx"))
    });
    if office_convert_requested(&params) {
        return Err(AppError::BadRequest(
            "useStirlingOfficeConvert selects the Stirling Office Convert \
             engine, which the Rust backend does not implement"
                .into(),
        ));
    }
    if is_docx {
        return Err(AppError::BadRequest(
            "docx inputs require the OOXML external-reference sanitizer, \
             which the Rust backend does not implement"
                .into(),
        ));
    }
    dispatch(state, query, uploads, params, ebook_to_pdf_work).await
}

fn ebook_to_pdf_work(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let upload = first_upload(uploads)?;
    let filename = upload
        .name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&upload.name)
        .to_string();

    // `FilenameUtils.getExtension`/`getBaseName`: text after/before the
    // last dot, with dotfiles and trailing dots counting as no extension.
    let dot = filename
        .rfind('.')
        .filter(|&i| i > 0 && i + 1 < filename.len());
    let (base_name, extension) = match dot {
        Some(i) => (&filename[..i], &filename[i + 1..]),
        None => ("", ""),
    };
    if extension.is_empty() {
        return Err(AppError::BadRequest("Unable to determine file type".into()));
    }
    let lower_extension = extension.to_lowercase();
    if !EBOOK_EXTENSIONS.contains(&lower_extension.as_str()) {
        return Err(AppError::BadRequest(format!(
            "Unsupported eBook file extension: {extension}"
        )));
    }
    let base_name = if base_name.trim().is_empty() {
        "document"
    } else {
        base_name
    };

    let mut optimize_for_ebook = param_bool(&params, "optimizeForEbook");
    if optimize_for_ebook && !crate::endpointconfig::is_group_enabled("Ghostscript") {
        optimize_for_ebook = false;
    }

    let dir =
        tempfile::tempdir().map_err(|e| AppError::Internal(anyhow::anyhow!("tempdir: {e}")))?;
    let input = dir.path().join(format!("{base_name}.{lower_extension}"));
    let output = dir.path().join(format!("{base_name}.pdf"));
    std::fs::write(&input, upload.bytes.as_ref())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("write input: {e}")))?;

    // `processExecutor.timeoutMinutes.calibreTimeoutMinutes`, default 30.
    let mut spec = EngineSpec::new(&settings::engine_program("ebook-convert"))
        .args([
            input.to_string_lossy().to_string(),
            output.to_string_lossy().to_string(),
        ])
        .cwd(dir.path().to_path_buf())
        .timeout(std::time::Duration::from_secs(30 * 60));
    if param_bool(&params, "embedAllFonts") {
        spec = spec.arg("--embed-all-fonts");
    }
    if param_bool(&params, "includeTableOfContents") {
        spec = spec.arg("--pdf-add-toc");
    }
    if param_bool(&params, "includePageNumbers") {
        spec = spec.arg("--pdf-page-numbers");
    }
    run_engine(&spec)?;

    let pdf = std::fs::read(&output).map_err(|_| {
        AppError::Internal(anyhow::anyhow!(
            "The ebook converter did not produce a PDF output"
        ))
    })?;
    if pdf.is_empty() {
        return Err(AppError::Internal(anyhow::anyhow!(
            "The ebook converter did not produce a PDF output"
        )));
    }

    // `optimizePdfWithGhostscript` failures fall back to the unoptimized
    // PDF like Java's catch-and-write-original path.
    let mut bytes = if optimize_for_ebook {
        gs_ebook_optimize(&pdf).unwrap_or(pdf)
    } else {
        pdf
    };
    if !optimize_for_ebook {
        let mut doc = pdfdoc::load("ebook.pdf", &bytes)?;
        bytes = pdfdoc::save(&mut doc)?;
    }

    Ok(pdf_response(
        bytes,
        &output_name(Some(&filename), "_convertedToPDF.pdf"),
    ))
}

/// `GeneralUtils.optimizePdfWithGhostscript`: Ghostscript pdfwrite with
/// the ebook preset. `None` on any engine failure — the caller keeps the
/// original bytes.
fn gs_ebook_optimize(pdf: &[u8]) -> Option<Vec<u8>> {
    let dir = tempfile::tempdir().ok()?;
    let input = dir.path().join("gs_input.pdf");
    let output = dir.path().join("gs_output.pdf");
    std::fs::write(&input, pdf).ok()?;
    let spec = EngineSpec::new("gs")
        .args([
            "-sDEVICE=pdfwrite".to_string(),
            "-dPDFSETTINGS=/ebook".to_string(),
            "-dFastWebView=true".to_string(),
            "-dNOPAUSE".to_string(),
            "-dQUIET".to_string(),
            "-dBATCH".to_string(),
            format!("-sOutputFile={}", output.display()),
            input.to_string_lossy().to_string(),
        ])
        .timeout(std::time::Duration::from_secs(300));
    let out = convert::run(&spec).ok()?;
    if out.timed_out || out.code != Some(0) {
        return None;
    }
    std::fs::read(&output).ok().filter(|b| !b.is_empty())
}

// ---------------------------------------------------------------------------
// PDF → eBook (`/convert/pdf/epub`, ConvertPDFToEpubController)
// ---------------------------------------------------------------------------

/// `POST /api/v1/convert/pdf/epub` — Calibre `ebook-convert` PDF→EPUB/AZW3.
/// `useStirlingOfficeConvert` is rejected (Office Convert engine
/// unported); the Calibre group probe in `endpointconfig` 403s when the
/// binary is absent, matching Java's group-enable check.
pub async fn pdf_to_epub(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    _headers: HeaderMap,
    _uri: OriginalUri,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, params) = receive_multipart(multipart).await?;
    if office_convert_requested(&params) {
        return Err(AppError::BadRequest(
            "useStirlingOfficeConvert selects the Stirling Office Convert              engine, which the Rust backend does not implement"
                .into(),
        ));
    }
    dispatch(state, query, uploads, params, pdf_to_epub_work).await
}

fn pdf_to_epub_work(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let upload = first_upload(uploads)?;
    let filename = upload
        .name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&upload.name)
        .to_string();

    let dot = filename
        .rfind('.')
        .filter(|&i| i > 0 && i + 1 < filename.len());
    let (base_name, extension) = match dot {
        Some(i) => (&filename[..i], &filename[i + 1..]),
        None => ("", ""),
    };
    if extension.is_empty() {
        return Err(AppError::BadRequest("Unable to determine file type".into()));
    }
    if !extension.eq_ignore_ascii_case("pdf") {
        return Err(AppError::BadRequest("Input file must be a PDF".into()));
    }
    let base_name = if base_name.trim().is_empty() {
        "document"
    } else {
        base_name
    };

    // `detectChapters` defaults true and only an explicit false disables.
    let detect_chapters = !params.iter().any(|(n, v)| {
        n == "detectChapters"
            && matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "false" | "off" | "no" | "0"
            )
    });
    // `TargetDevice`/`OutputFormat` field defaults; the request model
    // initialises targetDevice to TABLET_PHONE_IMAGES so --output-profile
    // is always emitted.
    let kindle = params
        .iter()
        .any(|(n, v)| n == "targetDevice" && v.trim().eq_ignore_ascii_case("KINDLE_EINK_TEXT"));
    let (out_ext, media_type, format_name) = if params
        .iter()
        .any(|(n, v)| n == "outputFormat" && v.trim().eq_ignore_ascii_case("AZW3"))
    {
        ("azw3", "application/vnd.amazon.ebook", "AZW3")
    } else {
        ("epub", "application/epub+zip", "EPUB")
    };

    let dir =
        tempfile::tempdir().map_err(|e| AppError::Internal(anyhow::anyhow!("tempdir: {e}")))?;
    let input = dir.path().join(format!("{base_name}.pdf"));
    let output = dir.path().join(format!("{base_name}.{out_ext}"));
    std::fs::write(&input, upload.bytes.as_ref())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("write input: {e}")))?;

    let mut spec = EngineSpec::new(&settings::engine_program("ebook-convert"))
        .args([
            input.to_string_lossy().to_string(),
            output.to_string_lossy().to_string(),
            // Calibre's own engine reads pdftohtml's XML output; header and
            // footer detection stays off like Java's golden defaults.
            "--pdf-engine".to_string(),
            "calibre".to_string(),
            "--pdf-header-skip".to_string(),
            "0".to_string(),
            "--pdf-footer-skip".to_string(),
            "0".to_string(),
            "--enable-heuristics".to_string(),
            "--insert-blank-line".to_string(),
            "--filter-css".to_string(),
            "font-family,color,background-color,margin-left,margin-right".to_string(),
        ])
        .cwd(dir.path().to_path_buf())
        .timeout(std::time::Duration::from_secs(30 * 60));
    if detect_chapters {
        spec = spec.args([
            "--chapter".to_string(),
            "//h:*[re:test(., '\\s*Chapter\\s+', 'i')]".to_string(),
        ]);
    }
    spec = spec.args([
        "--output-profile".to_string(),
        if kindle { "kindle" } else { "tablet" }.to_string(),
    ]);
    run_engine(&spec)?;

    let bytes = std::fs::read(&output).map_err(|_| {
        AppError::Internal(anyhow::anyhow!(
            "Calibre did not produce a {format_name} output"
        ))
    })?;
    if bytes.is_empty() {
        return Err(AppError::Internal(anyhow::anyhow!(
            "Calibre did not produce a {format_name} output"
        )));
    }

    Ok(file_response(
        bytes,
        &output_name(
            Some(&filename),
            &format!("_convertedTo{format_name}.{out_ext}"),
        ),
        media_type,
    ))
}

// ---------------------------------------------------------------------------
// PDF ↔ vector (`/convert/vector/pdf`, `/convert/pdf/vector`,
// PdfVectorExportController)
// ---------------------------------------------------------------------------

/// `POST /api/v1/convert/vector/pdf` — Ghostscript `pdfwrite` for PS/EPS;
/// a `.pdf` upload passes through unchanged, everything else is rejected.
pub async fn vector_to_pdf(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, params) = receive_multipart(multipart).await?;
    dispatch(state, query, uploads, params, vector_to_pdf_work).await
}

fn vector_to_pdf_work(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let _ = param(&params, "outputFormat", "");
    let prepress = param_bool(&params, "prepress");
    let upload = first_upload(uploads)?;
    let filename = upload
        .name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&upload.name);
    let ext = filename
        .rsplit_once('.')
        .map(|(_, e)| e.to_lowercase())
        .unwrap_or_default();

    let dir =
        tempfile::tempdir().map_err(|e| AppError::Internal(anyhow::anyhow!("tempdir: {e}")))?;
    let input = dir.path().join(format!("input.{ext}"));
    std::fs::write(&input, upload.bytes.as_ref())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("write input: {e}")))?;
    let output = dir.path().join("output.pdf");

    let pdf = if matches!(ext.as_str(), "ps" | "eps" | "epsf") {
        let mut spec = EngineSpec::new("gs")
            .args([
                "-sDEVICE=pdfwrite".to_string(),
                "-dNOPAUSE".to_string(),
                "-dBATCH".to_string(),
                "-dSAFER".to_string(),
                "-dCompatibilityLevel=1.4".to_string(),
            ])
            .timeout(std::time::Duration::from_secs(300));
        if prepress {
            spec = spec.arg("-dPDFSETTINGS=/prepress");
        }
        spec = spec
            .arg(format!("-sOutputFile={}", output.display()))
            .arg(input.to_string_lossy().to_string());
        run_engine(&spec)?;
        std::fs::read(&output)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("read gs output: {e}")))?
    } else if ext == "pdf" {
        upload.bytes.to_vec()
    } else {
        return Err(AppError::BadRequest(format!(
            "Unsupported Ghostscript input format {ext}"
        )));
    };

    Ok(pdf_response(
        pdf,
        &output_name(Some(filename), "_converted.pdf"),
    ))
}

/// `POST /api/v1/convert/pdf/vector` — Ghostscript vector devices
/// (eps2write/ps2write/pxlcolor/xpswrite).
pub async fn pdf_to_vector(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, params) = receive_multipart(multipart).await?;
    dispatch(state, query, uploads, params, pdf_to_vector_work).await
}

fn pdf_to_vector_work(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let upload = first_upload(uploads)?;
    let filename = upload
        .name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&upload.name);
    let format = {
        let f = param(&params, "outputFormat", "");
        if f.is_empty() { "eps" } else { f }.to_lowercase()
    };
    let device = match format.as_str() {
        "eps" => "eps2write",
        "ps" => "ps2write",
        "pcl" => "pxlcolor",
        "xps" => "xpswrite",
        _ => {
            return Err(AppError::BadRequest(format!(
                "Unsupported output format: {format}"
            )))
        }
    };

    let dir =
        tempfile::tempdir().map_err(|e| AppError::Internal(anyhow::anyhow!("tempdir: {e}")))?;
    let input = dir.path().join("input.pdf");
    let output = dir.path().join(format!("output.{format}"));
    std::fs::write(&input, upload.bytes.as_ref())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("write input: {e}")))?;

    let spec = EngineSpec::new("gs")
        .args([
            format!("-sDEVICE={device}"),
            "-dNOPAUSE".to_string(),
            "-dBATCH".to_string(),
            "-dSAFER".to_string(),
            format!("-sOutputFile={}", output.display()),
            input.to_string_lossy().to_string(),
        ])
        .timeout(std::time::Duration::from_secs(300));
    run_engine(&spec)?;

    let bytes = std::fs::read(&output)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("read gs output: {e}")))?;
    let mime = match format.as_str() {
        "eps" | "ps" => "application/postscript",
        "pcl" => "application/vnd.hp-PCL",
        "xps" => "application/vnd.ms-xpsdocument",
        _ => "application/octet-stream",
    };
    Ok(file_response(
        bytes,
        &output_name(Some(filename), &format!("_converted.{format}")),
        mime,
    ))
}

// ---------------------------------------------------------------------------
// PDF → HTML (`/convert/pdf/html`, ConvertPDFToHtml → PDFToFile.processPdfToHtml)
// ---------------------------------------------------------------------------

/// `pdftohtml -c` produces the page HTML plus extracted assets; Java zips
/// the whole output directory. The Stirling Office Convert branch is not
/// ported (`useStirlingOfficeConvert` accepted, WeasyPrint-free path only).
pub async fn pdf_to_html(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, params) = receive_multipart(multipart).await?;
    dispatch(state, query, uploads, params, pdf_to_html_work).await
}

fn pdf_to_html_work(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let _ = param(&params, "useStirlingOfficeConvert", "");
    let upload = first_upload(uploads)?;
    if upload.content_type.as_deref() != Some("application/pdf") {
        return Err(AppError::BadRequest("input must be a PDF file".into()));
    }
    let filename = upload
        .name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&upload.name);
    let stem = filename
        .rsplit_once('.')
        .map(|(b, _)| b)
        .filter(|b| !b.is_empty())
        .unwrap_or("output");

    let dir =
        tempfile::tempdir().map_err(|e| AppError::Internal(anyhow::anyhow!("tempdir: {e}")))?;
    let input = dir.path().join("input.pdf");
    std::fs::write(&input, upload.bytes.as_ref())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("write input: {e}")))?;

    // Java runs pdftohtml inside the output dir with the base name as the
    // output prefix; -c produces complex (per-element) output.
    let spec = EngineSpec::new("pdftohtml")
        .arg("-c")
        .arg(input.to_string_lossy().to_string())
        .arg(stem)
        .cwd(dir.path())
        .timeout(std::time::Duration::from_secs(300));
    run_engine(&spec)?;

    let mut entries = Vec::new();
    for file in read_dir_files(dir.path())? {
        if file == input {
            continue;
        }
        let bytes = std::fs::read(&file)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("read pdftohtml output: {e}")))?;
        entries.push((
            file.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("output")
                .to_string(),
            bytes,
        ));
    }
    if entries.is_empty() {
        return Err(AppError::Pdf("pdftohtml produced no output".into()));
    }
    let name = format!("{stem}ToHtml.zip");
    Ok(file_response(
        zip_bytes(&entries)?,
        &name,
        "application/octet-stream",
    ))
}

/// Commits an image as a full-page image XObject on a page of `doc`'s
/// given size — the SVG path's equivalent of `append_image_page` with an
/// explicit viewport (fill draw).
fn commit_image_page(
    doc: &mut Document,
    image: &PageImage,
    page_w: f32,
    page_h: f32,
) -> AppResult<()> {
    let content = format!("q {page_w} 0 0 {page_h} 0 0 cm /Im1 Do Q").into_bytes();
    commit_page(doc, image, page_w, page_h, content)
}

// ---------------------------------------------------------------------------
// PDF → PDF/A & PDF/X (`/convert/pdf/pdfa`, ConvertPDFToPDFA)
// ---------------------------------------------------------------------------

/// The two ICC profiles Java writes into its working dir: `sRGB.icc` is the
/// shipped `/icc/sRGB2014.icc` resource; `Gray.icc` is the JDK's built-in
/// `ICC_Profile.getInstance(CS_GRAY)` output, embedded verbatim.
const PDFA_SRGB_ICC: &[u8] = include_bytes!("../../assets/icc/srgb.icc");
const PDFA_GRAY_ICC: &[u8] = include_bytes!("../../assets/icc/gray.icc");

/// `PdfaProfile`: the `outputFormat` tokens Java accepts. Level A still runs
/// Ghostscript at the part's B conformance — the tagger is proprietary-only,
/// so the OSS Java build also returns level B under a `_b` filename.
struct PdfaProfile {
    part: u32,
    compatibility_level: &'static str,
    display_name: &'static str,
}

impl PdfaProfile {
    /// `PdfaProfile.fromRequest`: unknown or blank tokens fall back to
    /// PDF/A-2b rather than erroring.
    fn from_param(token: &str) -> Self {
        let normalized = token.trim().to_lowercase();
        let (part, compatibility_level, display_name) = match normalized.as_str() {
            "pdfa-1" => (1, "1.4", "PDF/A-1b"),
            "pdfa-3" | "pdfa-3b" => (3, "1.7", "PDF/A-3b"),
            "pdfa-1a" => (1, "1.4", "PDF/A-1a"),
            "pdfa-2a" => (2, "1.7", "PDF/A-2a"),
            "pdfa-3a" => (3, "1.7", "PDF/A-3a"),
            _ => (2, "1.7", "PDF/A-2b"),
        };
        Self {
            part,
            compatibility_level,
            display_name,
        }
    }
}

/// `outputFormat.toLowerCase().startsWith("pdfx")` — routes to the
/// Ghostscript-only PDF/X path.
fn is_pdfx_format(output_format: &str) -> bool {
    output_format
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("pdfx"))
}

enum PdfaWork {
    Respond(Response),
    /// Ghostscript failed or is absent; the handler reports it as an
    /// internal error since no Java fallback exists.
    GsFailed(String),
}

// `Response` is not `Debug`; tests only need the variant name.
impl std::fmt::Debug for PdfaWork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Respond(_) => f.write_str("Respond(..)"),
            Self::GsFailed(msg) => f.debug_tuple("GsFailed").field(msg).finish(),
        }
    }
}

/// `POST /api/v1/convert/pdf/pdfa` — PDF/A or PDF/X via Ghostscript,
/// validated by the veraPDF CLI when installed.
///
/// Java's pipeline has three engines; this port covers the preferred one.
/// Stirling Office Convert (`useStirlingOfficeConvert` or the admin
/// default) is rejected — that engine has no Rust port. Ghostscript
/// failures surface as errors; Java's PDFBox-sanitizer fallback chain
/// is gone with the Java backend. `strict=true` is native: the
/// Ghostscript output must pass veraPDF validation or the request
/// fails, matching `verifyStrictCompliance`; without the `verapdf` CLI
/// a strict request fails with a clear error rather than silently
/// skipping validation. The PDFBox sanitizer Java runs before
/// Ghostscript (font, annotation, separation-colour and transparency
/// repairs) is not ported; Ghostscript handles most repairs itself.
pub async fn pdf_to_pdfa(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    _headers: HeaderMap,
    _uri: OriginalUri,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, params) = receive_multipart(multipart).await?;

    if office_convert_requested(&params) {
        return Err(AppError::BadRequest(
            "useStirlingOfficeConvert selects the Stirling Office Convert \
             engine, which the Rust backend does not implement"
                .into(),
        ));
    }

    let async_req = jobs::async_requested(&query, &params);
    let attempt = async move {
        let outcome = tokio::task::spawn_blocking(move || pdfa_work(uploads, params))
            .await
            .map_err(|e| AppError::Internal(anyhow::Error::new(e)))?;
        match outcome? {
            PdfaWork::Respond(response) => Ok(response),
            PdfaWork::GsFailed(detail) => Err(AppError::Internal(anyhow::anyhow!(
                "PDF/A conversion failed: {detail}"
            ))),
        }
    };

    if async_req {
        jobs::submit(&state, attempt).await
    } else {
        attempt.await
    }
}

fn pdfa_work(uploads: Vec<Upload>, params: Vec<(String, String)>) -> AppResult<PdfaWork> {
    let upload = first_upload(uploads)?;
    if upload.content_type.as_deref() != Some("application/pdf") {
        return Err(AppError::BadRequest("input must be a PDF file".into()));
    }

    let original = upload
        .name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&upload.name);
    let original = if original.trim().is_empty() {
        "output.pdf"
    } else {
        original
    };
    // Java truncates at the last dot rather than using remove_extension:
    // ".x" yields an empty base and "a." keeps "a".
    let base = original
        .rfind('.')
        .map(|i| &original[..i])
        .unwrap_or(original);

    match pdfa_convert(upload.bytes.as_ref(), &params)? {
        Ok((bytes, suffix)) => Ok(PdfaWork::Respond(pdf_response(
            bytes,
            &format!("{base}{suffix}"),
        ))),
        Err(detail) => Ok(PdfaWork::GsFailed(detail)),
    }
}

/// The Ghostscript PDF/A (or PDF/X) pass plus the optional veraPDF
/// check, shared by `/convert/pdf/pdfa` and the `convertToPdfA3b`
/// post-processing on endpoints that emit a PDF natively. `params`
/// carries the same `outputFormat`/`strict` fields the endpoint
/// accepts. `Ok(Ok((bytes, suffix)))` on success, `Ok(Err(detail))`
/// when Ghostscript itself failed; validation problems are hard errors.
pub(crate) fn pdfa_convert(
    pdf: &[u8],
    params: &[(String, String)],
) -> AppResult<Result<(Vec<u8>, String), String>> {
    let output_format = param(params, "outputFormat", "");
    let is_pdfx = is_pdfx_format(output_format);
    let profile = PdfaProfile::from_param(output_format);

    let dir =
        tempfile::tempdir().map_err(|e| AppError::Internal(anyhow::anyhow!("tempdir: {e}")))?;
    let input = dir.path().join("input.pdf");
    std::fs::write(&input, pdf)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("write input: {e}")))?;

    let outcome = if is_pdfx {
        gs_pdfx(dir.path(), &input).map(|bytes| (bytes, "_PDFX.pdf".to_string()))
    } else {
        gs_pdfa(dir.path(), &input, &profile)
            .map(|bytes| (bytes, format!("_PDFA-{}b.pdf", profile.part)))
    };
    let strict = param_bool(params, "strict");
    match outcome {
        Ok((bytes, suffix)) => {
            validate_pdfa(&bytes, is_pdfx, &profile, strict, dir.path())?;
            Ok(Ok((bytes, suffix)))
        }
        Err(detail) => Ok(Err(detail)),
    }
}

/// `convertToPdfA3b` post-processing for endpoints whose output is a
/// PDF produced in-process: the same Ghostscript PDF/A pass Java ran
/// as a pipeline stage. Callers inject `outputFormat=pdfa-3b` — the
/// flag names the profile, so it does not come from the request.
#[allow(dead_code)]
pub(crate) fn pdfa_post_process(pdf: &[u8], params: &[(String, String)]) -> AppResult<Vec<u8>> {
    match pdfa_convert(pdf, params)? {
        Ok((bytes, _)) => Ok(bytes),
        Err(detail) => Err(AppError::Internal(anyhow::anyhow!(
            "PDF/A conversion failed: {detail}"
        ))),
    }
}

/// Optional veraPDF CLI, resolved once at first use through the same
/// `customPaths.operations` override every other engine honours. It is a
/// validator, not a converter, so it is deliberately not an
/// `ENGINE_PROBES` entry — its absence must not disable the endpoint.
fn verapdf_program() -> Option<PathBuf> {
    static VERAPDF: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    VERAPDF
        .get_or_init(|| {
            let program = settings::engine_program("verapdf");
            convert::find_executable(&program, &[])
        })
        .clone()
}

/// veraPDF `--flavour` token for the requested profile. Level-A outputs
/// are still validated against the requested level — an untagged B-level
/// result fails an `a` flavour exactly as Java's strict check would.
fn verapdf_flavour(profile: &PdfaProfile) -> String {
    let level = profile.display_name.chars().last().unwrap_or('b');
    format!("{}{}", profile.part, level)
}

enum VeraOutcome {
    Compliant,
    NonCompliant(String),
    /// CLI absent — only reachable when `strict` is false.
    Unavailable,
    /// CLI present but failed to run or produced no verdict.
    Error(String),
}

fn run_verapdf(pdf: &[u8], flavour: Option<&str>, dir: &Path) -> VeraOutcome {
    let Some(program) = verapdf_program() else {
        return VeraOutcome::Unavailable;
    };
    let input = dir.join("verapdf_input.pdf");
    if let Err(e) = std::fs::write(&input, pdf) {
        return VeraOutcome::Error(format!("write veraPDF input: {e}"));
    }
    let mut spec = EngineSpec::new(program.to_string_lossy().as_ref())
        .args(["--format".to_string(), "text".to_string()])
        .timeout(std::time::Duration::from_secs(10 * 60));
    if let Some(f) = flavour {
        spec = spec.args(["--flavour".to_string(), f.to_string()]);
    }
    spec = spec.args([input.to_string_lossy().to_string()]);
    let out = match convert::run(&spec) {
        Ok(out) => out,
        Err(e) => return VeraOutcome::Error(e.to_string()),
    };
    let report = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let non_compliant_marker = report.contains("Compliance: false")
        || report.contains("compliant: false")
        || out.code == Some(1);
    if non_compliant_marker && !report.contains("Compliance: true") {
        return VeraOutcome::NonCompliant(report);
    }
    if out.code == Some(0) {
        VeraOutcome::Compliant
    } else if out.timed_out {
        VeraOutcome::Error("veraPDF timed out".into())
    } else {
        VeraOutcome::Error(format!(
            "veraPDF exited with {:?}: {}",
            out.code,
            report.trim().chars().take(500).collect::<String>()
        ))
    }
}

/// `validateAndWarnPdfA` + `verifyStrictCompliance`: Ghostscript producing
/// a PDF is not proof of compliance, so every output is checked when the
/// CLI exists. Non-strict violations are logged like Java's warn-only
/// preflight; strict violations fail the request with the report.
fn validate_pdfa(
    bytes: &[u8],
    is_pdfx: bool,
    profile: &PdfaProfile,
    strict: bool,
    dir: &Path,
) -> AppResult<()> {
    if is_pdfx {
        // veraPDF's PDF/X flavours are version-specific (`x-1a`, `x-4`, …)
        // and Java's PDF/X strict path is a separate code path; rather than
        // guess a flavour, strict PDF/X fails honestly until ported.
        if strict {
            return Err(AppError::BadRequest(
                "Strict validation for PDF/X output is not supported by the Rust port".into(),
            ));
        }
        return Ok(());
    }
    match run_verapdf(bytes, Some(&verapdf_flavour(profile)), dir) {
        VeraOutcome::Compliant => Ok(()),
        VeraOutcome::Unavailable => {
            if strict {
                Err(AppError::Internal(anyhow::anyhow!(
                    "strict PDF/A validation requires the veraPDF CLI (`verapdf`); \
                     install it or drop strict=true"
                )))
            } else {
                tracing::info!("veraPDF CLI not found; skipping PDF/A validation");
                Ok(())
            }
        }
        VeraOutcome::Error(detail) => {
            if strict {
                Err(AppError::Internal(anyhow::anyhow!(
                    "PDF/A validation could not run: {detail}"
                )))
            } else {
                tracing::warn!("PDF/A validation unavailable: {detail}");
                Ok(())
            }
        }
        VeraOutcome::NonCompliant(report) => {
            let summary = report.trim().chars().take(2000).collect::<String>();
            if strict {
                Err(AppError::BadRequest(format!(
                    "Strict PDF/A mode enabled: the output is not compliant with {}. \
                     Details: {summary}",
                    profile.display_name
                )))
            } else {
                tracing::warn!(
                    "PDF/A output failed veraPDF validation ({}): {summary}",
                    profile.display_name
                );
                Ok(())
            }
        }
    }
}

/// `convertWithGhostscript`: ICC profiles + `PDFA_def.ps` + the optional
/// qpdf pre-clean, then the `gs` pdfwrite invocation.
fn gs_pdfa(dir: &Path, input: &Path, profile: &PdfaProfile) -> Result<Vec<u8>, String> {
    let (rgb, gray) = write_icc_profiles(dir)?;
    let def = write_pdfa_def(dir, &rgb, profile)?;

    let mut gs_input = input.to_path_buf();
    // PDF/A-1 gets the extra CIDSet clean before normalisation.
    if profile.part == 1 {
        if let Some(cleaned) = qpdf_transform(
            &gs_input,
            &[
                "--remove-unreferenced-resources=yes",
                "--normalize-content=y",
                "--object-streams=preserve",
            ],
            "cidset_cleaned_input.pdf",
        ) {
            gs_input = cleaned;
        }
    }
    if let Some(normalized) = qpdf_transform(
        &gs_input,
        &["--normalize-content=y", "--object-streams=preserve"],
        "normalized_input.pdf",
    ) {
        gs_input = normalized;
    }

    let output = dir.join("gs_output.pdf");
    let spec = EngineSpec::new(&settings::engine_program("gs"))
        .args(pdfa_gs_args(
            dir, &gs_input, &output, &rgb, &gray, &def, profile,
        ))
        .timeout(std::time::Duration::from_secs(30 * 60));
    run_gs(&spec, &output)
}

/// `convertWithGhostscriptX`: same profiles, no def file, print-quality
/// downsampling tuned for PDF/X.
fn gs_pdfx(dir: &Path, input: &Path) -> Result<Vec<u8>, String> {
    let (rgb, gray) = write_icc_profiles(dir)?;
    let output = dir.join("gs_output_pdfx.pdf");
    let spec = EngineSpec::new(&settings::engine_program("gs"))
        .args(pdfx_gs_args(dir, input, &output, &rgb, &gray))
        .timeout(std::time::Duration::from_secs(30 * 60));
    run_gs(&spec, &output)
}

/// Shared tail of both Ghostscript paths: a non-zero exit or a missing
/// output file is a conversion failure, like Java's `IOException` that
/// triggers the fallback chain.
fn run_gs(spec: &EngineSpec, output: &Path) -> Result<Vec<u8>, String> {
    let out = convert::run(spec).map_err(|e| e.to_string())?;
    if out.timed_out {
        return Err(format!("'{}' timed out", spec.program));
    }
    if out.code != Some(0) {
        return Err(format!("'{}' exited with {:?}", spec.program, out.code));
    }
    std::fs::read(output).map_err(|e| format!("Ghostscript did not produce an output file: {e}"))
}

fn write_icc_profiles(dir: &Path) -> Result<(PathBuf, PathBuf), String> {
    let rgb = dir.join("sRGB.icc");
    let gray = dir.join("Gray.icc");
    std::fs::write(&rgb, PDFA_SRGB_ICC)
        .and_then(|_| std::fs::write(&gray, PDFA_GRAY_ICC))
        .map_err(|e| format!("write ICC profiles: {e}"))?;
    Ok((rgb, gray))
}

/// `createPdfaDefFile`: the pdfmark prefix that stamps the title and embeds
/// the sRGB output intent every PDF/A file needs.
fn write_pdfa_def(dir: &Path, rgb: &Path, profile: &PdfaProfile) -> Result<PathBuf, String> {
    let def = dir.join("PDFA_def.ps");
    let title = format!("Converted to {}", profile.display_name);
    let rgb_path = rgb.to_string_lossy().replace('\\', "/");
    let content = format!(
        r#"% This is a sample prefix file for creating a PDF/A document.
% Feel free to modify entries marked with "Customize".

% Define entries in the document Info dictionary.
[/Title ({title})
 /DOCINFO pdfmark

% Define an ICC profile.
[/_objdef {{icc_PDFA}} /type /stream /OBJ pdfmark
[{{icc_PDFA}} <<
  /N 3
>> /PUT pdfmark
[{{icc_PDFA}} ({rgb_path}) (r) file /PUT pdfmark

% Define the output intent dictionary.
[/_objdef {{OutputIntent_PDFA}} /type /dict /OBJ pdfmark
[{{OutputIntent_PDFA}} <<
  /Type /OutputIntent
  /S /GTS_PDFA1
  /DestOutputProfile {{icc_PDFA}}
  /OutputConditionIdentifier (sRGB IEC61966-2.1)
  /Info (sRGB IEC61966-2.1)
  /RegistryName (http://www.color.org)
>> /PUT pdfmark
[{{Catalog}} <</OutputIntents [ {{OutputIntent_PDFA}} ]>> /PUT pdfmark
"#
    );
    std::fs::write(&def, content).map_err(|e| format!("write PDFA_def.ps: {e}"))?;
    Ok(def)
}

/// `buildGhostscriptCommand` verbatim — permit lists, device flags and the
/// def-file-then-input order all matter to Ghostscript's PDF/A writer.
fn pdfa_gs_args(
    dir: &Path,
    input: &Path,
    output: &Path,
    rgb: &Path,
    gray: &Path,
    def: &Path,
    profile: &PdfaProfile,
) -> Vec<String> {
    let work = dir.to_string_lossy().to_string();
    vec![
        format!("--permit-file-read={work}"),
        format!("--permit-file-read={}", rgb.display()),
        format!("--permit-file-read={}", gray.display()),
        format!("--permit-file-read={}", input.display()),
        format!("--permit-file-read={}", def.display()),
        format!("--permit-file-write={work}"),
        format!("-dPDFA={}", profile.part),
        "-dPDFACompatibilityPolicy=1".into(),
        format!("-dCompatibilityLevel={}", profile.compatibility_level),
        "-sDEVICE=pdfwrite".into(),
        "-sColorConversionStrategy=RGB".into(),
        "-dProcessColorModel=/DeviceRGB".into(),
        format!("-sOutputICCProfile={}", rgb.display()),
        format!("-sDefaultRGBProfile={}", rgb.display()),
        format!("-sDefaultGrayProfile={}", gray.display()),
        "-dEmbedAllFonts=true".into(),
        "-dSubsetFonts=true".into(),
        "-dCompressFonts=true".into(),
        "-dNOSUBSTFONTS=false".into(),
        "-dNOPAUSE".into(),
        "-dBATCH".into(),
        "-dNOOUTERSAVE".into(),
        format!("-sOutputFile={}", output.display()),
        def.to_string_lossy().to_string(),
        input.to_string_lossy().to_string(),
    ]
}

/// `buildGhostscriptCommandX`: PDF/X-2008 at compatibility 1.6 with
/// print-resolution downsampling and no def file.
fn pdfx_gs_args(dir: &Path, input: &Path, output: &Path, rgb: &Path, gray: &Path) -> Vec<String> {
    let work = dir.to_string_lossy().to_string();
    vec![
        format!("--permit-file-read={work}"),
        format!("--permit-file-read={}", rgb.display()),
        format!("--permit-file-read={}", gray.display()),
        format!("--permit-file-read={}", input.display()),
        format!("--permit-file-write={work}"),
        "-dPDFX=2008".into(),
        "-dCompatibilityLevel=1.6".into(),
        "-sDEVICE=pdfwrite".into(),
        "-sColorConversionStrategy=RGB".into(),
        format!("-sOutputICCProfile={}", rgb.display()),
        format!("-sDefaultRGBProfile={}", rgb.display()),
        format!("-sDefaultGrayProfile={}", gray.display()),
        "-dEmbedAllFonts=true".into(),
        "-dSubsetFonts=true".into(),
        "-dCompressFonts=true".into(),
        "-dNOSUBSTFONTS=false".into(),
        "-dColorImageDownsampleType=/Bicubic".into(),
        "-dColorImageResolution=300".into(),
        "-dGrayImageDownsampleType=/Bicubic".into(),
        "-dGrayImageResolution=300".into(),
        "-dMonoImageDownsampleType=/Bicubic".into(),
        "-dMonoImageResolution=1200".into(),
        "-dNOPAUSE".into(),
        "-dBATCH".into(),
        "-dNOOUTERSAVE".into(),
        format!("-sOutputFile={}", output.display()),
        input.to_string_lossy().to_string(),
    ]
}

/// `normalizePdfWithQpdf`/`cleanCidSetWithQpdf`: best-effort — a missing
/// binary, non-zero exit or absent output yields `None` and the caller
/// proceeds with the previous file, like Java.
fn qpdf_transform(input: &Path, flags: &[&str], out_name: &str) -> Option<PathBuf> {
    convert::find_executable("qpdf", &[])?;
    let out = input.parent()?.join(out_name);
    let mut args: Vec<String> = flags.iter().map(|s| s.to_string()).collect();
    args.push(input.to_string_lossy().to_string());
    args.push(out.to_string_lossy().to_string());
    let spec = EngineSpec::new("qpdf")
        .args(args)
        .timeout(std::time::Duration::from_secs(300));
    let result = convert::run(&spec).ok()?;
    if result.timed_out || result.code != Some(0) || !out.exists() {
        return None;
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// PDF → CSV / XLSX (Tabula lattice extraction, `TabulaTableParser` +
// `SpreadsheetExtractionAlgorithm`)
// ---------------------------------------------------------------------------

/// One extracted table with its 1-based page number — the per-fragment
/// record `ExtractCSVController` zips or returns directly.
struct TableFragment {
    page: u32,
    /// 1-based index among the tables found on this page — Java's `i + 1`
    /// in `generateEntryName` / `"Page N Table M"`.
    table_index: usize,
    rows: Vec<Vec<String>>,
}

/// Selected pages as 1-based numbers in request order —
/// `PDFWithPageNums.getPageNumbersList(document, true)`.
fn selected_pages_1based(params: &[(String, String)], total: usize) -> AppResult<Vec<u32>> {
    let mut selection = param(params, "pageNumbers", "all").trim();
    if selection.is_empty() {
        selection = "all";
    }
    page_select::require_valid_page_selection(selection, total).map_err(AppError::BadRequest)?;
    Ok(page_select::parse_page_list(selection, total)
        .map_err(AppError::BadRequest)?
        .into_iter()
        .map(|i| i as u32 + 1)
        .collect())
}

/// Runs lattice extraction over the selected pages. A page that yields
/// no tables contributes nothing, matching `TabulaTableParser`'s
/// catch-and-skip behaviour.
fn extract_table_fragments(doc: &Document, page_numbers: &[u32]) -> Vec<TableFragment> {
    let page_ids = pdfdoc::page_ids(doc);
    let mut fragments = Vec::new();
    for &page_num in page_numbers {
        let Some(&page_id) = page_ids.get(page_num as usize - 1) else {
            continue;
        };
        let tables = tabula::extract_tables(doc, page_id);
        for (i, table) in tables.into_iter().enumerate() {
            fragments.push(TableFragment {
                page: page_num,
                table_index: i + 1,
                rows: table.rows,
            });
        }
    }
    fragments
}

/// `TabulaTableParser.normaliseText`: collapse Tabula's `\r` line joins
/// to spaces and trim.
fn normalise_cell_text(raw: &str) -> String {
    raw.replace("\r\n", " ")
        .replace(['\n', '\r'], " ")
        .trim()
        .to_string()
}

/// `CsvSanitizer.sanitizeCell`: values a spreadsheet would evaluate as
/// formulas get a `'` prefix; plain numbers stay untouched.
fn sanitize_csv_cell(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    let candidate = value.trim_start_matches(java_is_whitespace);
    if candidate.is_empty() || !"=+-@".contains(candidate.chars().next().unwrap()) {
        return value.to_string();
    }
    if is_plain_number(candidate) {
        return value.to_string();
    }
    format!("'{value}")
}

/// Java's `Character.isWhitespace` — Unicode space/line/paragraph
/// separators except the non-breaking ones, plus ASCII control
/// whitespace. Rust's `char::is_whitespace` differs on `\u0085`,
/// `\u00A0` and `\u2007`, which Java does not strip.
fn java_is_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'..='\u{000D}'
            | ' '
            | '\u{001C}'..='\u{001F}'
            | '\u{1680}'
            | '\u{2000}'..='\u{2006}'
            | '\u{2008}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{205F}'
            | '\u{3000}'
    )
}

/// `PLAIN_NUMBER` = `[-+]?[0-9]+(?:[.,][0-9]+)*`, full match.
fn is_plain_number(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    if matches!(b.first(), Some(b'-') | Some(b'+')) {
        i = 1;
    }
    let mut digits = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
        digits += 1;
    }
    if digits == 0 {
        return false;
    }
    while i < b.len() {
        if !matches!(b[i], b'.' | b',') {
            return false;
        }
        i += 1;
        let mut group = 0;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
            group += 1;
        }
        if group == 0 {
            return false;
        }
    }
    true
}

/// One fragment as CSV: `CSVFormat.EXCEL` with `QuoteMode.ALL` and
/// escape `"` — every field double-quoted, inner quotes doubled,
/// CRLF record separators, UTF-8.
fn csv_fragment_bytes(fragment: &TableFragment) -> Vec<u8> {
    let mut out = String::new();
    for row in &fragment.rows {
        for (i, cell) in row.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let value = sanitize_csv_cell(&normalise_cell_text(cell));
            out.push('"');
            out.push_str(&value.replace('"', "\"\""));
            out.push('"');
        }
        out.push_str("\r\n");
    }
    out.into_bytes()
}

/// `POST /api/v1/convert/pdf/csv` — `ExtractCSVController.pdfToCsv`.
pub async fn pdf_to_csv(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, params) = receive_multipart(multipart).await?;
    dispatch(state, query, uploads, params, pdf_to_csv_work).await
}

fn pdf_to_csv_work(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let upload = first_upload(uploads)?;
    let doc = pdfdoc::load(&upload.name, &upload.bytes)?;
    let total = pdfdoc::page_ids(&doc).len();
    let pages = selected_pages_1based(&params, total)?;
    let base = output_name(Some(&upload.name), "");

    let fragments = extract_table_fragments(&doc, &pages);
    if fragments.is_empty() {
        return Ok(nothing_to_extract(&doc, &pages));
    }
    if fragments.len() == 1 {
        let csv = csv_fragment_bytes(&fragments[0]);
        return Ok(file_response(
            csv,
            &format!("{base}_extracted.csv"),
            "text/csv",
        ));
    }
    let entries: Vec<(String, Vec<u8>)> = fragments
        .iter()
        .map(|f| {
            (
                format!("{base}_p{}_t{}.csv", f.page, f.table_index),
                csv_fragment_bytes(f),
            )
        })
        .collect();
    let zip = zip_bytes(&entries)?;
    Ok(file_response(
        zip,
        &format!("{base}_extracted.zip"),
        "application/octet-stream",
    ))
}

/// `POST /api/v1/convert/pdf/xlsx` —
/// `ConvertPDFToExcelController.pdfToExcel`, Tabula path. The
/// Office-Convert branch (`useStirlingOfficeConvert`, or the admin
/// `system.stirlingOfficeConversion` default) selects an engine the
/// Rust backend does not implement and is rejected like the other
/// converters' office branches.
pub async fn pdf_to_xlsx(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    _headers: HeaderMap,
    _uri: OriginalUri,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, params) = receive_multipart(multipart).await?;
    if office_convert_requested(&params) {
        return Err(AppError::BadRequest(
            "useStirlingOfficeConvert selects the Stirling Office Convert              engine, which the Rust backend does not implement"
                .into(),
        ));
    }
    dispatch(state, query, uploads, params, pdf_to_xlsx_work).await
}

fn pdf_to_xlsx_work(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let upload = first_upload(uploads)?;
    let doc = pdfdoc::load(&upload.name, &upload.bytes)?;
    let total = pdfdoc::page_ids(&doc).len();
    let pages = selected_pages_1based(&params, total)?;
    let base = output_name(Some(&upload.name), "");

    let fragments = extract_table_fragments(&doc, &pages);
    if fragments.is_empty() {
        return Ok(nothing_to_extract(&doc, &pages));
    }
    let workbook = xlsx_workbook(&fragments);
    Ok(file_response(
        workbook,
        &format!("{base}.xlsx"),
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    ))
}

/// Java's `NothingToExtractException` for an extraction that found
/// nothing: scanned pages (images, no text layer) get `NO_TEXT_LAYER`,
/// everything else `NO_TABLES_FOUND` — both 422, never an empty file.
fn nothing_to_extract(doc: &Document, page_numbers: &[u32]) -> Response {
    let detail = if looks_image_only(doc, page_numbers) {
        "The selected pages are scanned images with no text to read tables from. Run OCR on the PDF first, then try again."
    } else {
        "No tables were found in the selected pages, so no file was returned."
    };
    crate::error::nothing_to_extract(detail)
}

/// `PdfUtils.looksImageOnly`: every selected page has no extractable
/// text and at least one draws an image XObject.
fn looks_image_only(doc: &Document, page_numbers: &[u32]) -> bool {
    let page_ids = pdfdoc::page_ids(doc);
    let mut any_image = false;
    for &page_num in page_numbers {
        let Some(&page_id) = page_ids.get(page_num as usize - 1) else {
            continue;
        };
        let has_text = text::page_chars(doc, page_id)
            .iter()
            .any(|c| c.text.chars().any(|ch| !java_is_whitespace(ch)));
        if has_text {
            return false;
        }
        if !any_image {
            any_image = page_has_image(doc, page_id);
        }
    }
    any_image
}

/// `PdfUtils.hasImagesOnPage`: an `/XObject` that is an image, directly
/// or nested inside Form XObjects' own `/Resources`.
fn page_has_image(doc: &Document, page_id: ObjectId) -> bool {
    let Some((_holder, resources)) = pdfdoc::page_resources_holder(doc, page_id) else {
        return false;
    };
    let mut visited = HashSet::new();
    resources_have_image(doc, &resources, &mut visited)
}

fn resources_have_image(doc: &Document, resources: &Object, visited: &mut HashSet<usize>) -> bool {
    let Some(dict) = pdfdoc::peek_dict(doc, resources) else {
        return false;
    };
    // Java's visited set keys on the COSDictionary; the borrowed
    // Dictionary's address plays the same role within this document.
    if !visited.insert(dict as *const Dictionary as usize) {
        return false;
    }
    let Ok(xobjects) = dict.get(b"XObject") else {
        return false;
    };
    let Some(xobject_dict) = pdfdoc::peek_dict(doc, xobjects) else {
        return false;
    };
    for (_name, value) in xobject_dict.iter() {
        let object = match value {
            Object::Reference(id) => doc.get_object(*id).unwrap_or(value),
            other => other,
        };
        let Object::Stream(stream) = object else {
            continue;
        };
        let Some(subtype) = stream
            .dict
            .get(b"Subtype")
            .ok()
            .and_then(|o| o.as_name().ok())
        else {
            continue;
        };
        if subtype == b"Image" {
            return true;
        }
        if subtype == b"Form" {
            if let Ok(form_resources) = stream.dict.get(b"Resources") {
                if resources_have_image(doc, form_resources, visited) {
                    return true;
                }
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// PDF → Markdown (`/convert/pdf/markdown`, ConvertPDFToMarkdown)
// ---------------------------------------------------------------------------

/// `POST /api/v1/convert/pdf/markdown` — the proprietary layout-aware
/// pipeline (`AdvancedPdfMarkdownConverter`) ported in `pdftext`. The
/// Office-Convert branch has no Rust port and is rejected.
pub async fn pdf_to_markdown(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    _headers: HeaderMap,
    _uri: OriginalUri,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, params) = receive_multipart(multipart).await?;
    if office_convert_requested(&params) {
        return Err(AppError::BadRequest(
            "useStirlingOfficeConvert selects the Stirling Office Convert              engine, which the Rust backend does not implement"
                .into(),
        ));
    }
    dispatch(state, query, uploads, params, pdf_to_markdown_work).await
}

fn pdf_to_markdown_work(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    _params: Vec<(String, String)>,
) -> AppResult<Response> {
    let upload = first_upload(uploads)?;
    let doc = pdfdoc::load(&upload.name, &upload.bytes)?;
    let markdown = crate::pdftext::converter::convert(&doc);
    let base = output_name(Some(&upload.name), "");
    Ok(file_response(
        markdown.into_bytes(),
        &format!("{base}.md"),
        "text/markdown",
    ))
}

/// Sheet names follow `writeTabula`: "Page N" for a lone table on a
/// page, "Page N Table M" otherwise, deduplicated like POI's
/// `getUniqueSheetName` (names here never approach Excel's limits).
fn sheet_name_for(fragments: &[TableFragment], index: usize) -> String {
    let f = &fragments[index];
    let on_page = fragments.iter().filter(|g| g.page == f.page).count();
    if on_page == 1 {
        format!("Page {}", f.page)
    } else {
        format!("Page {} Table {}", f.page, f.table_index)
    }
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // A literal CR normalizes to LF in XML parsers; cell text
            // keeps `\r` as the chunk separator, so it must be escaped.
            '\r' => out.push_str("&#13;"),
            _ => out.push(c),
        }
    }
    out
}

fn col_name(mut n: usize) -> String {
    n += 1;
    let mut s = Vec::new();
    while n > 0 {
        n -= 1;
        s.push((b'A' + (n % 26) as u8) as char);
        n /= 26;
    }
    s.iter().rev().collect()
}

/// Minimal multi-sheet OOXML workbook — same inline-string shape as
/// the form extractor's writer, one worksheet per table fragment.
fn xlsx_workbook(fragments: &[TableFragment]) -> Vec<u8> {
    let mut used: Vec<String> = Vec::new();
    let names: Vec<String> = (0..fragments.len())
        .map(|i| unique_sheet_name(sheet_name_for(fragments, i), &mut used))
        .collect();

    let mut sheets_xml = Vec::with_capacity(fragments.len());
    for f in fragments {
        let mut xml = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
             <worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><sheetData>",
        );
        for (r, row) in f.rows.iter().enumerate() {
            xml.push_str(&format!("<row r=\"{}\">", r + 1));
            for (c, cell) in row.iter().enumerate() {
                xml.push_str(&format!(
                    "<c r=\"{}{}\" t=\"inlineStr\"><is><t>{}</t></is></c>",
                    col_name(c),
                    r + 1,
                    xml_escape(cell)
                ));
            }
            xml.push_str("</row>");
        }
        xml.push_str("</sheetData></worksheet>");
        sheets_xml.push(xml);
    }

    let mut content_types = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
         <Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
         <Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
         <Default Extension=\"xml\" ContentType=\"application/xml\"/>\
         <Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>\
         <Override PartName=\"/xl/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml\"/>",
    );
    for i in 0..fragments.len() {
        content_types.push_str(&format!(
            "<Override PartName=\"/xl/worksheets/sheet{}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>",
            i + 1
        ));
    }
    content_types.push_str("</Types>");

    let mut workbook = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
         <workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" \
         xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><sheets>",
    );
    for (i, name) in names.iter().enumerate() {
        workbook.push_str(&format!(
            "<sheet name=\"{}\" sheetId=\"{}\" r:id=\"rId{}\"/>",
            xml_escape(name),
            i + 1,
            i + 1
        ));
    }
    workbook.push_str("</sheets></workbook>");

    let mut rels = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
         <Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
    );
    for i in 0..fragments.len() {
        rels.push_str(&format!(
            "<Relationship Id=\"rId{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet{}.xml\"/>",
            i + 1,
            i + 1
        ));
    }
    rels.push_str(&format!(
        "<Relationship Id=\"rId{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/>",
        fragments.len() + 1
    ));
    rels.push_str("</Relationships>");

    let styles = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
         <styleSheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">\
         <fonts count=\"1\"><font><sz val=\"11\"/><name val=\"Calibri\"/></font></fonts>\
         <fills count=\"1\"><fill><patternFill patternType=\"none\"/></fill></fills>\
         <borders count=\"1\"><border/></borders>\
         <cellStyleXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\"/></cellStyleXfs>\
         <cellXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\"/></cellXfs>\
         </styleSheet>";

    let root_rels = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
         <Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
         <Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"xl/workbook.xml\"/>\
         </Relationships>";

    let mut entries: Vec<(String, String)> = vec![
        ("[Content_Types].xml".into(), content_types),
        ("_rels/.rels".into(), root_rels.into()),
        ("xl/workbook.xml".into(), workbook),
        ("xl/_rels/workbook.xml.rels".into(), rels),
        ("xl/styles.xml".into(), styles.into()),
    ];
    for (i, xml) in sheets_xml.iter().enumerate() {
        entries.push((format!("xl/worksheets/sheet{}.xml", i + 1), xml.clone()));
    }

    let mut zip = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for (name, body) in &entries {
        zip.start_file(name, options).expect("zip entry");
        zip.write_all(body.as_bytes()).expect("zip write");
    }
    zip.finish().expect("zip finish").into_inner()
}

/// POI `getUniqueSheetName`: `" (n)"` suffixes on collision, truncated
/// to Excel's 31-char limit. Sheet names here are `"Page N[ Table M]"`
/// — already Excel-safe, so only the dedupe loop matters in practice.
fn unique_sheet_name(base: String, used: &mut Vec<String>) -> String {
    let mut candidate = base.clone();
    let mut count = 1;
    while used.iter().any(|u| u == &candidate) {
        let suffix = format!(" ({count})");
        let keep = 31usize.saturating_sub(suffix.chars().count());
        candidate = format!("{}{suffix}", base.chars().take(keep).collect::<String>());
        count += 1;
    }
    used.push(candidate.clone());
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use std::cmp::Ordering;

    #[test]
    fn office_extension_matches_java_validation() {
        assert_eq!(office_extension("report.docx"), Some("docx".into()));
        assert_eq!(office_extension("/tmp/a.PPTX"), Some("pptx".into()));
        assert_eq!(office_extension("C:\\dir\\sheet.xlsx"), Some("xlsx".into()));
        assert_eq!(office_extension("noext"), None);
        assert_eq!(office_extension("a.b"), None); // < 2 chars
        assert_eq!(office_extension("a.12345"), None); // > 4 chars
        assert_eq!(office_extension("a.c++"), None); // non-alphanumeric
        assert_eq!(office_extension(".hidden"), None); // extension > 4 chars
        assert_eq!(office_extension("a.doc"), Some("doc".into()));
    }

    #[test]
    fn svg_units_convert_to_points() {
        assert_eq!(svg_unit_to_pt("100"), Some(75.0));
        assert_eq!(svg_unit_to_pt("72pt"), Some(72.0));
        assert_eq!(svg_unit_to_pt("6pc"), Some(72.0));
        assert_eq!(svg_unit_to_pt("1in"), Some(72.0));
        for (value, expected) in [("25.4mm", 72.0), ("2.54cm", 72.0), ("101.6q", 72.0)] {
            let got = svg_unit_to_pt(value).unwrap();
            assert!((got - expected).abs() < 0.01, "{value} → {got}");
        }
        assert_eq!(svg_unit_to_pt("100px"), Some(75.0));
        // Viewport-relative units cannot resolve here.
        assert_eq!(svg_unit_to_pt("50%"), None);
        assert_eq!(svg_unit_to_pt("2em"), None);
        assert_eq!(svg_unit_to_pt("abc"), None);
    }

    #[test]
    fn svg_viewport_prefers_width_height_then_viewbox() {
        let sized = br#"<svg width="100" height="200" viewBox="0 0 10 10">"#;
        assert_eq!(svg_viewport_pt(sized), (75.0, 150.0));
        let view_box = br#"<svg viewBox="0 0 100 200">"#;
        assert_eq!(svg_viewport_pt(view_box), (75.0, 150.0));
        let bare = br#"<svg>"#;
        assert_eq!(svg_viewport_pt(bare), (595.0, 842.0));
        // stroke-width must not satisfy the width attribute; with no
        // usable width/height/viewBox the Java defaults apply.
        let stroke = br#"<svg stroke-width="10" height="50">"#;
        assert_eq!(svg_viewport_pt(stroke), (595.0, 842.0));
    }

    #[test]
    fn natural_order_compares_digit_runs_numerically() {
        assert_eq!(natural_order_cmp("img2", "img10"), Ordering::Less);
        assert_eq!(natural_order_cmp("img10", "img2"), Ordering::Greater);
        assert_eq!(natural_order_cmp("a", "a"), Ordering::Equal);
        // Java's exhausted-side rule: full lengths decide, so "a1" < "a01".
        assert_eq!(natural_order_cmp("a1", "a01"), Ordering::Less);
        assert_eq!(natural_order_cmp("page-2", "page-10"), Ordering::Less);
    }

    #[test]
    fn cbz_image_entry_matches_java_pattern() {
        for name in [
            "a.jpg",
            "b.JPEG",
            "c.png",
            "d.gif",
            "e.bmp",
            "f.webp",
            "dir/g.png",
        ] {
            assert!(cbz_image_entry(name), "{name} should match");
        }
        for name in ["a.txt", "a.pdf", "a.png.txt", "a"] {
            assert!(!cbz_image_entry(name), "{name} should not match");
        }
    }

    #[test]
    fn magic_byte_checks() {
        assert!(is_png(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]));
        assert!(!is_png(&[0x89, 0x50]));
        assert!(is_jpeg(&[0xFF, 0xD8, 0xFF, 0xE0]));
        assert!(!is_jpeg(&[0xFF, 0xD8, 0x00]));
        assert!(matches!(image_format("png"), image::ImageFormat::Png));
        assert!(matches!(image_format("jpeg"), image::ImageFormat::Jpeg));
        assert!(matches!(image_format("webp"), image::ImageFormat::WebP));
        assert!(matches!(image_format("anything"), image::ImageFormat::Png));
    }

    #[test]
    fn jpeg_sof_reads_dimensions_and_components() {
        // Minimal JPEG: SOI + SOF0 (8-bit, 200x100, 3 components).
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08];
        jpeg.extend_from_slice(&[0x00, 0x64, 0x00, 0xC8, 0x03]);
        jpeg.extend_from_slice(&[0u8; 9]); // component specs
        assert_eq!(jpeg_sof(&jpeg), Some((200, 100, 3)));
        assert_eq!(jpeg_sof(b"not a jpeg"), None);
        assert_eq!(jpeg_sof(&[0xFF, 0xD8]), None);
    }

    // ---------- CBZ → PDF ----------

    fn cbz_upload(name: &str, entries: &[(&str, Vec<u8>)]) -> Upload {
        let pairs: Vec<(String, Vec<u8>)> = entries
            .iter()
            .map(|(n, b)| (n.to_string(), b.clone()))
            .collect();
        Upload {
            name: name.into(),
            bytes: crate::upload::UploadBytes::from_vec(zip_bytes(&pairs).unwrap()),
            content_type: None,
        }
    }

    fn solid_jpeg(width: u32, height: u32) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(width, height, image::Rgb([200, 30, 30]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Jpeg)
            .unwrap();
        out.into_inner()
    }

    fn solid_png(width: u32, height: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(width, height, image::Rgba([0, 0, 200, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    fn page_widths(bytes: &[u8]) -> Vec<f64> {
        let doc = Document::load_mem(bytes).unwrap();
        pdfdoc::page_ids(&doc)
            .iter()
            .map(|id| {
                let b = pdfdoc::media_box(&doc, *id).unwrap();
                b[2] - b[0]
            })
            .collect()
    }

    #[tokio::test]
    async fn cbz_pdf_orders_pages_naturally() {
        // Lexicographic order would give 10x10, 30x30, 20x20; the natural
        // comparator must produce page1, page2, page10 → 10, 20, 30 wide.
        let upload = cbz_upload(
            "book.cbz",
            &[
                ("page10.jpg", solid_jpeg(30, 30)),
                ("page1.jpg", solid_jpeg(10, 10)),
                ("page2.jpg", solid_jpeg(20, 20)),
            ],
        );
        let res = cbz_pdf_work(AppState::new(), HashMap::new(), vec![upload], vec![]).unwrap();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(page_widths(&body), vec![10.0, 20.0, 30.0]);
    }

    #[tokio::test]
    async fn cbz_pdf_mixes_jpeg_and_png() {
        let upload = cbz_upload(
            "mixed.zip",
            &[
                ("b.png", solid_png(15, 25)),
                ("a.jpg", solid_jpeg(10, 10)),
                ("readme.txt", b"ignored".to_vec()),
            ],
        );
        let res = cbz_pdf_work(AppState::new(), HashMap::new(), vec![upload], vec![]).unwrap();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let doc = Document::load_mem(&body).unwrap();
        assert_eq!(pdfdoc::page_ids(&doc).len(), 2);
        // "a.jpg" < "b.png" under natural order → JPEG page first.
        assert_eq!(page_widths(&body), vec![10.0, 15.0]);
    }

    #[tokio::test]
    async fn cbz_pdf_rejects_invalid_and_empty_archives() {
        let bad = Upload {
            name: "bad.cbz".into(),
            bytes: crate::upload::UploadBytes::from_vec(b"not a zip".to_vec()),
            content_type: None,
        };
        assert!(cbz_pdf_work(AppState::new(), HashMap::new(), vec![bad], vec![]).is_err());

        let empty = Upload {
            name: "empty.cbz".into(),
            bytes: crate::upload::UploadBytes::from_vec(Vec::new()),
            content_type: None,
        };
        assert!(cbz_pdf_work(AppState::new(), HashMap::new(), vec![empty], vec![]).is_err());

        // A valid zip with no image entries hits Java's no-images error.
        let no_images = cbz_upload("noimg.cbz", &[("a.txt", b"x".to_vec())]);
        let err =
            cbz_pdf_work(AppState::new(), HashMap::new(), vec![no_images], vec![]).unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[tokio::test]
    async fn cbz_pdf_skips_corrupt_entries_and_errors_when_all_bad() {
        // One good JPEG plus a truncated "jpeg" that must be skipped like
        // Java's log-and-continue on PDFBox decode failure.
        let upload = cbz_upload(
            "partial.cbz",
            &[
                ("good.jpg", solid_jpeg(8, 8)),
                ("broken.jpg", vec![0xFF, 0xD8, 0xFF, 0xE0]),
            ],
        );
        let res = cbz_pdf_work(AppState::new(), HashMap::new(), vec![upload], vec![]).unwrap();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(page_widths(&body), vec![8.0]);

        let all_bad = cbz_upload("allbad.cbz", &[("x.jpg", vec![0xFF, 0xD8])]);
        assert!(cbz_pdf_work(AppState::new(), HashMap::new(), vec![all_bad], vec![]).is_err());
    }

    #[test]
    fn url_format_valid_matches_java_disjunction() {
        assert!(url_format_valid("https://example.com/page"));
        assert!(url_format_valid("http://example.com"));
        // Uppercase scheme fails the regex but passes Urls.create.
        assert!(url_format_valid("HTTP://example.com"));
        // Line terminators break the regex's `.*` but not URL parsing.
        assert!(url_format_valid("https://example.com/a\nb"));
        assert!(!url_format_valid("ftp://example.com"));
        assert!(!url_format_valid("example.com"));
        assert!(!url_format_valid(""));
    }

    #[test]
    fn is_valid_url_denies_infrastructure_hosts() {
        assert!(is_valid_url("https://example.com"));
        for host in [
            "http://192.168.1.1/",
            "http://3232235777/",
            "http://169.254.169.254/latest/meta-data",
            "http://2852039166/",
        ] {
            assert!(!is_valid_url(host), "{host}");
        }
    }

    #[test]
    fn sensitive_addresses_cover_java_ranges() {
        use std::net::IpAddr;
        for ip in [
            "0.0.0.0",
            "127.0.0.1",
            "169.254.1.1",
            "192.168.1.1",
            "10.1.2.3",
            "172.16.0.1",
            "100.64.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "198.18.0.1",
            "192.0.2.1",
            "224.0.0.1",
            "240.0.0.1",
            "::1",
            "::",
            "fe80::1",
            "fec0::1",
            "fc00::1",
            "fd00::1",
            "::ffff:127.0.0.1",
        ] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(is_sensitive_address(&ip), "{ip} should be sensitive");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(!is_sensitive_address(&ip), "{ip} should be allowed");
        }
        // IPv4-mapped public address defers to the embedded IPv4 rules.
        assert!(!is_sensitive_address(&"::ffff:8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn disallowed_network_location_blocks_private_literals() {
        assert!(is_disallowed_network_location("127.0.0.1"));
        assert!(is_disallowed_network_location("192.168.1.1"));
        assert!(is_disallowed_network_location("::1"));
        assert!(is_disallowed_network_location("nonexistent.invalid"));
    }

    #[test]
    fn file_scheme_detection_normalises_encodings() {
        for html in [
            r#"<a href="file:///etc/passwd">"#,
            "FILE:////etc/passwd",
            "file&#x3a;//etc/passwd",
            "file&colon;//etc/passwd",
            "fi&#108;e:/etc/passwd",
            "file%3a//etc/passwd",
            "file:%253a%252f%252fetc", // double-encoded → literal %2f/%3a survive
            "see file :/etc/passwd",
        ] {
            assert!(contains_disallowed_uri_scheme(html), "{html}");
        }
        for html in [
            "https://example.com/file.pdf",
            "profile:/settings",
            "myfile:///etc",
            "<a href=\"files/report.pdf\">",
        ] {
            assert!(!contains_disallowed_uri_scheme(html), "{html}");
        }
    }

    #[test]
    fn percent_decode_handles_malformed_sequences() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("%zz%2"), "%zz%2");
        assert_eq!(percent_decode("%25%32%66"), "%2f");
        // Java appends each byte as a char — %C3%A9 decodes to mojibake,
        // not "é"; the pattern only cares about ASCII "file:" text anyway.
        assert_eq!(percent_decode("caf%C3%A9"), "caf\u{C3}\u{A9}");
    }

    #[test]
    fn numeric_entities_decode_like_java() {
        // Input is already lowercased by normalize_for_scheme_detection.
        assert_eq!(decode_numeric_html_entities("&#47;&#x2f;"), "//");
        assert_eq!(decode_numeric_html_entities("&#x41;"), "A");
        assert_eq!(decode_numeric_html_entities("&#x;"), "&#x;");
        assert_eq!(decode_numeric_html_entities("&#65x;"), "&#65x;");
        // Entities decode after lowercasing, so &#X41; yields 'A', not 'a'.
        assert_eq!(normalize_for_scheme_detection("&#X41;"), "A");
    }

    #[test]
    fn url_filename_matches_java_sanitisation() {
        assert_eq!(
            convert_url_to_file_name("https://example.com/some/page.html"),
            "https_example_com_some_page_html.pdf"
        );
        assert_eq!(convert_url_to_file_name("https://a.b/c"), "https_a_b_c.pdf");
        // "!!!" sanitises to underscores which collapse and trim away.
        assert_eq!(convert_url_to_file_name("!!!"), "document.pdf");
        let long = format!("https://example.com/{}", "a".repeat(80));
        let name = convert_url_to_file_name(&long);
        assert!(name.ends_with(".pdf"));
        // Java caps the base at 50 chars before underscore collapsing, so
        // the final name can be shorter than 54.
        assert_eq!(name.len(), 52);
    }

    #[test]
    fn markdown_html_renders_tables_and_code_attributes() {
        let html = render_markdown_html(
            "# Title\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n```rust\nfn x() {}\n```\n",
        )
        .unwrap();
        assert!(html.contains("<h1>Title</h1>"), "{html}");
        assert!(
            html.contains("<table class=\"table table-striped\">"),
            "{html}"
        );
        assert!(
            html.contains("<pre style=\"white-space: pre-wrap; word-wrap: break-word\">"),
            "{html}"
        );
        // GFM extras Java does not enable stay off: no strikethrough.
        let plain = render_markdown_html("~~gone~~").unwrap();
        assert!(plain.contains("~~gone~~"), "{plain}");
    }

    #[test]
    fn normalize_entry_path_resolves_like_java() {
        assert_eq!(normalize_entry_path("a/b/c.png"), Some("a/b/c.png".into()));
        assert_eq!(normalize_entry_path("a/./b.png"), Some("a/b.png".into()));
        assert_eq!(normalize_entry_path("a/../b.png"), Some("b.png".into()));
        assert_eq!(normalize_entry_path("..\\b.png"), None);
        assert_eq!(normalize_entry_path("./"), None);
        assert_eq!(normalize_entry_path("a\\b.png"), Some("a/b.png".into()));
    }

    #[test]
    fn find_markdown_key_prefers_root_index() {
        let mut entries = HashMap::new();
        entries.insert("docs/guide.md".to_string(), Vec::new());
        entries.insert("index.md".to_string(), Vec::new());
        assert_eq!(find_markdown_key(&entries).as_deref(), Some("index.md"));
        entries.remove("index.md");
        entries.insert("README.MD".to_string(), Vec::new());
        // Case-insensitive .md match; sorted order approximates walk order.
        assert_eq!(find_markdown_key(&entries).as_deref(), Some("README.MD"));
        assert_eq!(find_markdown_key(&HashMap::new()), None);
    }

    // ---------- PDF/A & PDF/X ----------

    #[test]
    fn pdfa_profile_maps_java_tokens() {
        let cases: &[(&str, u32, &str)] = &[
            ("pdfa-1", 1, "PDF/A-1b"),
            ("pdfa", 2, "PDF/A-2b"),
            ("pdfa-2", 2, "PDF/A-2b"),
            ("pdfa-2b", 2, "PDF/A-2b"),
            ("pdfa-3", 3, "PDF/A-3b"),
            ("pdfa-3b", 3, "PDF/A-3b"),
            ("pdfa-1a", 1, "PDF/A-1a"),
            ("pdfa-2a", 2, "PDF/A-2a"),
            ("pdfa-3a", 3, "PDF/A-3a"),
            // Java's orElse default: anything unrecognized is PDF/A-2b.
            ("", 2, "PDF/A-2b"),
            ("bogus", 2, "PDF/A-2b"),
            ("pdfa-4", 2, "PDF/A-2b"),
        ];
        for (token, part, display) in cases {
            let profile = PdfaProfile::from_param(token);
            assert_eq!(profile.part, *part, "{token}");
            assert_eq!(profile.display_name, *display, "{token}");
        }
        // Level A keeps its display name (Java's title) even though the
        // output stays level B.
        assert_eq!(PdfaProfile::from_param(" PDFA-2A ").part, 2);
    }

    #[test]
    fn pdfx_detection_matches_java_prefix_rule() {
        for token in ["pdfx", "PDFX", "pdfx-1a", "pdfx-3"] {
            assert!(is_pdfx_format(token), "{token}");
        }
        for token in ["", "pdfa", "pdf", "xpdfx"] {
            assert!(!is_pdfx_format(token), "{token}");
        }
    }

    #[test]
    fn pdfa_def_file_carries_title_and_output_intent() {
        let dir = tempfile::tempdir().unwrap();
        let rgb = dir.path().join("sRGB.icc");
        let profile = PdfaProfile::from_param("pdfa-3b");
        let def = write_pdfa_def(dir.path(), &rgb, &profile).unwrap();
        let content = std::fs::read_to_string(&def).unwrap();
        assert!(
            content.contains("[/Title (Converted to PDF/A-3b)"),
            "{content}"
        );
        assert!(content.contains("/S /GTS_PDFA1"), "{content}");
        assert!(content.contains("/N 3"), "{content}");
        assert!(
            content.contains(&format!("({}) (r) file", rgb.display())),
            "{content}"
        );
        assert!(content.contains("/OutputIntents"), "{content}");
    }

    #[test]
    fn gs_args_mirror_java_command_order() {
        let dir = std::path::Path::new("/tmp/pdfa_wd");
        let input = std::path::Path::new("/tmp/pdfa_wd/in.pdf");
        let output = std::path::Path::new("/tmp/pdfa_wd/out.pdf");
        let rgb = std::path::Path::new("/tmp/pdfa_wd/sRGB.icc");
        let gray = std::path::Path::new("/tmp/pdfa_wd/Gray.icc");
        let def = std::path::Path::new("/tmp/pdfa_wd/PDFA_def.ps");
        let profile = PdfaProfile::from_param("pdfa-1");
        let args = pdfa_gs_args(dir, input, output, rgb, gray, def, &profile);
        assert_eq!(args[0], "--permit-file-read=/tmp/pdfa_wd");
        assert!(args.contains(&"-dPDFA=1".to_string()));
        assert!(args.contains(&"-dCompatibilityLevel=1.4".to_string()));
        assert!(args.contains(&"-dPDFACompatibilityPolicy=1".to_string()));
        // Def file precedes the input, like Java's ProcessExecutor argv.
        assert_eq!(args[args.len() - 2], "/tmp/pdfa_wd/PDFA_def.ps");
        assert_eq!(args[args.len() - 1], "/tmp/pdfa_wd/in.pdf");

        let xargs = pdfx_gs_args(dir, input, output, rgb, gray);
        assert!(xargs.contains(&"-dPDFX=2008".to_string()));
        assert!(xargs.contains(&"-dCompatibilityLevel=1.6".to_string()));
        assert!(xargs.contains(&"-dColorImageResolution=300".to_string()));
        assert_eq!(xargs[xargs.len() - 1], "/tmp/pdfa_wd/in.pdf");
    }

    fn pdfa_upload(name: &str, bytes: Vec<u8>, content_type: Option<&str>) -> Upload {
        Upload {
            name: name.into(),
            bytes: crate::upload::UploadBytes::from_vec(bytes),
            content_type: content_type.map(str::to_string),
        }
    }

    fn sample_pdf() -> Vec<u8> {
        crate::testkit::Pdf::new().bytes()
    }

    #[test]
    fn pdfa_work_requires_a_file_and_a_pdf_content_type() {
        let err = pdfa_work(vec![], vec![]).expect_err("empty uploads must fail");
        assert!(matches!(err, AppError::BadRequest(_)));

        for ct in [None, Some("text/plain"), Some("application/octet-stream")] {
            let upload = pdfa_upload("doc.pdf", sample_pdf(), ct);
            let err = pdfa_work(vec![upload], vec![]).expect_err("non-PDF must fail");
            assert!(matches!(err, AppError::BadRequest(_)), "{ct:?}");
        }
    }

    #[test]
    fn qpdf_transform_is_best_effort() {
        // Missing binary or a bad input both yield None, never an error —
        // Java's normalize/clean steps are equally optional.
        assert!(qpdf_transform(
            Path::new("/definitely/missing/input.pdf"),
            &["--normalize-content=y"],
            "out.pdf"
        )
        .is_none());
    }

    #[cfg(unix)]
    #[test]
    fn run_gs_maps_engine_failures() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.pdf");
        // Non-zero exit.
        let spec = EngineSpec::new("false");
        assert!(run_gs(&spec, &output).is_err());
        // Zero exit but no output file, like Java's missing-output check.
        let spec = EngineSpec::new("true");
        let err = run_gs(&spec, &output).unwrap_err();
        assert!(err.contains("did not produce an output file"), "{err}");
        // Missing engine entirely.
        let spec = EngineSpec::new("stirling-no-such-engine-pdfa");
        assert!(run_gs(&spec, &output).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pdfa_work_converts_with_installed_ghostscript() {
        if convert::find_executable("gs", &[]).is_none() {
            return;
        }
        let upload = pdfa_upload("doc.pdf", sample_pdf(), Some("application/pdf"));
        let result = pdfa_work(
            vec![upload],
            vec![("outputFormat".into(), "pdfa-2b".into())],
        )
        .unwrap();
        let PdfaWork::Respond(response) = result else {
            panic!("gs conversion failed");
        };
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body.starts_with(b"%PDF-"), "not a PDF: {:?}", &body[..8]);
        // The output declares PDF/A-2b in its XMP, like Java's gs path.
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("pdfaid"), "missing pdfaid XMP");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pdfa_work_pdfx_uses_pdfx_device() {
        if convert::find_executable("gs", &[]).is_none() {
            return;
        }
        let upload = pdfa_upload("doc.pdf", sample_pdf(), Some("application/pdf"));
        let result = pdfa_work(vec![upload], vec![("outputFormat".into(), "pdfx".into())]).unwrap();
        let PdfaWork::Respond(response) = result else {
            panic!("gs pdf/x conversion failed");
        };
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body.starts_with(b"%PDF-1.6"), "{:?}", &body[..8]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pdfa_work_engine_failure_is_forwardable_not_an_error() {
        if convert::find_executable("gs", &[]).is_none() {
            return;
        }
        // A non-PDF byte stream makes gs exit non-zero; the work result is
        // GsFailed so the handler surfaces it as an internal error.
        let upload = pdfa_upload(
            "bad.pdf",
            b"definitely not a pdf".to_vec(),
            Some("application/pdf"),
        );
        let result = pdfa_work(vec![upload], vec![]).unwrap();
        assert!(matches!(result, PdfaWork::GsFailed(_)));
    }

    // ---------- pdf/csv + pdf/xlsx (Tabula lattice) ----------

    /// A one-page PDF with a ruled 2×2 table in the upper-left corner:
    /// three verticals and three horizontals around cells holding A1,
    /// B1, A2, B2. Coordinates are PDF user space (y up).
    fn ruled_table_pdf() -> Vec<u8> {
        use lopdf::dictionary;
        let mut pdf = crate::testkit::Pdf::new();
        let font = pdf.add(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let ops = "\
            100 700 m 300 700 l S \
            100 650 m 300 650 l S \
            100 600 m 300 600 l S \
            100 700 m 100 600 l S \
            200 700 m 200 600 l S \
            300 700 m 300 600 l S \
            BT /F1 10 Tf 105 670 Td (A1) Tj ET \
            BT /F1 10 Tf 205 670 Td (B1) Tj ET \
            BT /F1 10 Tf 105 620 Td (A2) Tj ET \
            BT /F1 10 Tf 205 620 Td (B2) Tj ET";
        let content = pdf.add_stream(dictionary! {}, ops.as_bytes());
        pdf.edit_page(|page| {
            page.set("Contents", Object::Reference(content));
            page.set(
                "Resources",
                dictionary! {
                    "Font" => dictionary! {
                        "F1" => Object::Reference(font),
                    },
                },
            );
        });
        pdf.bytes()
    }

    #[test]
    fn tabula_extracts_ruled_table() {
        let doc = Document::load_mem(&ruled_table_pdf()).unwrap();
        let page_id = *doc.get_pages().values().next().unwrap();
        let tables = tabula::extract_tables(&doc, page_id);
        assert_eq!(tables.len(), 1, "expected one table, got {tables:?}");
        let flat: Vec<&str> = tables[0]
            .rows
            .iter()
            .flatten()
            .map(String::as_str)
            .collect();
        for expected in ["A1", "B1", "A2", "B2"] {
            assert!(flat.contains(&expected), "missing {expected} in {flat:?}");
        }
    }

    #[test]
    fn tabula_finds_no_table_without_rulings() {
        let doc = Document::load_mem(&crate::testkit::Pdf::new().bytes()).unwrap();
        let page_id = *doc.get_pages().values().next().unwrap();
        assert!(tabula::extract_tables(&doc, page_id).is_empty());
    }

    #[test]
    fn csv_fragment_quotes_and_sanitises() {
        let fragment = TableFragment {
            page: 1,
            table_index: 1,
            rows: vec![
                vec!["a,b".into(), "say \"hi\"".into()],
                vec!["=SUM(A1)".into(), "-12.5".into()],
                vec!["  @cmd".into(), "+1,000".into()],
            ],
        };
        let csv = String::from_utf8(csv_fragment_bytes(&fragment)).unwrap();
        // normaliseText trims before sanitizing, so leading whitespace is
        // gone by the time the formula check runs — same as Java.
        assert_eq!(
            csv,
            "\"a,b\",\"say \"\"hi\"\"\"\r\n\"'=SUM(A1)\",\"-12.5\"\r\n\"'@cmd\",\"+1,000\"\r\n"
        );
    }

    #[test]
    fn plain_number_matches_java_regex() {
        // The Java regex allows repeated separator groups, so "1.2.3"
        // is a plain number there too.
        for n in ["-12.5", "+1,000", "42", "1.000,50", "-0.5", "1.2.3"] {
            assert!(is_plain_number(n), "{n}");
        }
        for n in ["-x", "+", "1,", ".5", "12a", ""] {
            assert!(!is_plain_number(n), "{n}");
        }
    }

    #[test]
    fn xlsx_workbook_is_a_valid_ooxml_zip() {
        let fragments = vec![
            TableFragment {
                page: 1,
                table_index: 1,
                rows: vec![
                    vec!["H1".into(), "H2".into()],
                    vec!["a&b".into(), String::new()],
                ],
            },
            TableFragment {
                page: 2,
                table_index: 1,
                rows: vec![vec!["only".into()]],
            },
        ];
        let bytes = xlsx_workbook(&fragments);
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let names: Vec<String> = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
            .collect();
        for expected in [
            "[Content_Types].xml",
            "xl/workbook.xml",
            "xl/worksheets/sheet1.xml",
            "xl/worksheets/sheet2.xml",
        ] {
            assert!(names.iter().any(|n| n == expected), "{names:?}");
        }
        let mut workbook = String::new();
        zip.by_name("xl/workbook.xml")
            .unwrap()
            .read_to_string(&mut workbook)
            .unwrap();
        assert!(workbook.contains("Page 1"), "{workbook}");
        assert!(workbook.contains("Page 2"), "{workbook}");
        let mut sheet1 = String::new();
        zip.by_name("xl/worksheets/sheet1.xml")
            .unwrap()
            .read_to_string(&mut sheet1)
            .unwrap();
        assert!(sheet1.contains("a&amp;b"), "{sheet1}");
    }

    #[test]
    fn sheet_names_dedupe_across_pages() {
        // Two tables on page 1 force "Table M" naming; a third fragment
        // reusing the same base still gets a unique name.
        let fragments = vec![
            TableFragment {
                page: 1,
                table_index: 1,
                rows: vec![],
            },
            TableFragment {
                page: 1,
                table_index: 2,
                rows: vec![],
            },
        ];
        let mut used = Vec::new();
        assert_eq!(
            unique_sheet_name(sheet_name_for(&fragments, 0), &mut used),
            "Page 1 Table 1"
        );
        assert_eq!(
            unique_sheet_name(sheet_name_for(&fragments, 1), &mut used),
            "Page 1 Table 2"
        );
        assert_eq!(
            unique_sheet_name("Page 1 Table 1".into(), &mut used),
            "Page 1 Table 1 (1)"
        );
    }

    #[test]
    fn selected_pages_are_one_based_and_ordered() {
        let params = vec![("pageNumbers".to_string(), "3,1".to_string())];
        assert_eq!(selected_pages_1based(&params, 5).unwrap(), vec![3, 1]);
        let params = vec![("pageNumbers".to_string(), "".to_string())];
        assert_eq!(selected_pages_1based(&params, 2).unwrap(), vec![1, 2]);
        let params = vec![("pageNumbers".to_string(), "99".to_string())];
        assert!(selected_pages_1based(&params, 2).is_err());
    }

    /// A page whose only content is an image XObject in `/Resources` —
    /// no text ops, so `looksImageOnly` must classify it as a scan.
    fn scanned_pdf() -> Vec<u8> {
        use lopdf::dictionary;
        let mut pdf = crate::testkit::Pdf::new();
        let image = pdf.add_stream(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => 1,
                "Height" => 1,
                "BitsPerComponent" => 8,
                "ColorSpace" => "DeviceGray",
            },
            &[0u8],
        );
        pdf.edit_page(|page| {
            page.set(
                "Resources",
                dictionary! {
                    "XObject" => dictionary! {
                        "Im1" => Object::Reference(image),
                    },
                },
            );
        });
        pdf.bytes()
    }

    #[tokio::test]
    async fn nothing_to_extract_is_422_with_java_messages() {
        // Blank page: no text and no image → NO_TABLES_FOUND.
        let doc = Document::load_mem(&crate::testkit::Pdf::new().bytes()).unwrap();
        let res = nothing_to_extract(&doc, &[1]);
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["type"], "/errors/nothing-to-extract");
        assert_eq!(body["title"], "Nothing Found to Extract");
        assert_eq!(
            body["detail"],
            "No tables were found in the selected pages, so no file was returned."
        );

        // Image XObject and no text → NO_TEXT_LAYER.
        let doc = Document::load_mem(&scanned_pdf()).unwrap();
        let res = nothing_to_extract(&doc, &[1]);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            body["detail"],
            "The selected pages are scanned images with no text to read tables from. Run OCR on the PDF first, then try again."
        );
    }

    #[test]
    fn looks_image_only_needs_text_gone_and_an_image() {
        // Ruled table PDF has text → never image-only, even with an image.
        let doc = Document::load_mem(&ruled_table_pdf()).unwrap();
        assert!(!looks_image_only(&doc, &[1]));
        let doc = Document::load_mem(&scanned_pdf()).unwrap();
        assert!(looks_image_only(&doc, &[1]));
        // A page with text but no image is not a scan either.
        let doc = Document::load_mem(&ruled_table_pdf()).unwrap();
        assert!(!page_has_image(&doc, {
            let ids = pdfdoc::page_ids(&doc);
            ids[0]
        }));
    }
}
