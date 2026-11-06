//! Visual page additions that need no renderer: image and QR stamps
//! (`misc/add-stamp`) and image watermarks (`security/add-watermark` with
//! `watermarkType=image`). Text stamps embed the NotoSans TTFs as CID fonts —
//! outside what lopdf writes — and rasterising modes need a renderer, so
//! those requests are rejected rather than approximated.

use std::collections::HashMap;

use axum::extract::{Multipart, Query, State};
use axum::response::Response;
use chrono::{Datelike, Local, NaiveDate, NaiveDateTime, Timelike};
use image::GenericImageView;
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};
use rand::RngExt as _;

use crate::error::{AppError, AppResult};
use crate::handlers::jobs::AppState;
use crate::handlers::merge::pdf_response;
use crate::handlers::misc::{append_page_content, dispatch};
use crate::page_select;
use crate::pdfdoc;
use crate::upload::{output_name, param, param_bool, receive_files_multipart, Upload, FILE_FIELD};

fn pdf_err(e: lopdf::Error) -> AppError {
    AppError::Pdf(e.to_string())
}

/// Numbers go through PDFBox's `DecimalFormat` (max 5 fraction digits);
/// integral values print without a fraction.
fn pdf_num(value: f64) -> String {
    let rounded = (value * 100000.0).round() / 100000.0;
    if rounded.fract() == 0.0 && rounded.abs() < 9.0e15 {
        format!("{}", rounded as i64)
    } else {
        format!("{rounded}")
    }
}

fn float_param(params: &[(String, String)], name: &str, default: f64) -> AppResult<f64> {
    crate::upload::param_parse(params, name, default)
}

/// Java request models bind these as `int`; out-of-range values are a
/// binding error (400) there, so parse `i32` rather than `i64`.
fn int_param(params: &[(String, String)], name: &str, default: i32) -> AppResult<i32> {
    crate::upload::param_parse(params, name, default)
}

/// `StampController`/`WatermarkController` reject filenames carrying `..`
/// or a leading `/` before any PDF work happens.
fn invalid_file_name(name: &str) -> bool {
    name.contains("..") || name.starts_with('/')
}

/// `embed_image` decodes through the `image` crate, so every format it
/// recognises converts natively; SVG and other payloads outside its
/// codec set are rejected.
fn is_native_raster(bytes: &[u8]) -> bool {
    image::guess_format(bytes).is_ok()
}

/// A decoded raster ready to embed: flate-compressed samples plus an
/// optional alpha mask, mirroring `LosslessFactory.createFromImage`.
struct EmbeddedImage {
    id: ObjectId,
    width: u32,
    height: u32,
}

fn embed_image(doc: &mut Document, bytes: &[u8]) -> AppResult<EmbeddedImage> {
    let decoded = image::load_from_memory(bytes)
        .map_err(|_| AppError::BadRequest("image must be a supported image".into()))?;
    let (width, height) = decoded.dimensions();

    let (color_space, samples, smask_samples) = if decoded.color().has_alpha() {
        let rgba = decoded.to_rgba8();
        let mut rgb = Vec::with_capacity(rgba.width() as usize * rgba.height() as usize * 3);
        let mut alpha = Vec::with_capacity(rgba.width() as usize * rgba.height() as usize);
        for px in rgba.pixels() {
            rgb.extend_from_slice(&[px[0], px[1], px[2]]);
            alpha.push(px[3]);
        }
        (b"DeviceRGB".as_slice(), rgb, Some(alpha))
    } else if matches!(
        decoded.color(),
        image::ColorType::L8 | image::ColorType::La8 | image::ColorType::L16
    ) {
        let gray_img = decoded.to_luma8();
        (b"DeviceGray".as_slice(), gray_img.into_raw(), None)
    } else {
        let rgb_img = decoded.to_rgb8();
        (b"DeviceRGB".as_slice(), rgb_img.into_raw(), None)
    };

    let id = image_stream(doc, width, height, color_space, samples)?;
    if let Some(alpha) = smask_samples {
        let mask = image_stream(doc, width, height, b"DeviceGray", alpha)?;
        if let Some(Object::Stream(stream)) = doc.objects.get_mut(&id) {
            stream.dict.set("SMask", Object::Reference(mask));
        }
    }
    Ok(EmbeddedImage { id, width, height })
}

fn image_stream(
    doc: &mut Document,
    width: u32,
    height: u32,
    color_space: &[u8],
    samples: Vec<u8>,
) -> AppResult<ObjectId> {
    let mut dict = Dictionary::new();
    dict.set("Type", Object::Name(b"XObject".to_vec()));
    dict.set("Subtype", Object::Name(b"Image".to_vec()));
    dict.set("Width", Object::Integer(width as i64));
    dict.set("Height", Object::Integer(height as i64));
    dict.set("ColorSpace", Object::Name(color_space.to_vec()));
    dict.set("BitsPerComponent", Object::Integer(8));
    let mut stream = Stream::new(dict, samples);
    let _ = stream.compress();
    let id = doc.new_object_id();
    doc.objects.insert(id, Object::Stream(stream));
    Ok(id)
}

/// Where the effective `/Resources` dictionary lives, so a mutation can be
/// written back to the same object PDFBox's inheritable-attribute lookup
/// would have mutated.
enum ResourceLoc {
    /// `/Resources` is an indirect reference; the dictionary is that object.
    Ref(ObjectId),
    /// `/Resources` is a direct dictionary on this object's dictionary.
    Inline(ObjectId),
}

fn resources_dict(
    doc: &Document,
    page_id: ObjectId,
) -> AppResult<Option<(ResourceLoc, Dictionary)>> {
    let Some((holder, slot)) = pdfdoc::page_resources_holder(doc, page_id) else {
        return Ok(None);
    };
    match slot {
        Object::Reference(id) => {
            let dict = doc
                .get_object(id)
                .map_err(pdf_err)
                .ok()
                .and_then(|o| pdfdoc::dict_of(o))
                .cloned();
            Ok(dict.map(|d| (ResourceLoc::Ref(id), d)))
        }
        Object::Dictionary(dict) => Ok(Some((ResourceLoc::Inline(holder), dict))),
        _ => Ok(None),
    }
}

/// `PDResources.createKey`: the section dictionary's size seeds the
/// counter, then the first `prefix<n>` not already present wins — it is
/// not necessarily the lowest free index.
fn next_resource_name(dict: &Dictionary, prefix: &str) -> String {
    let mut n = dict.len();
    loop {
        n += 1;
        let key = format!("{prefix}{n}");
        if !dict.has(key.as_bytes()) {
            return key;
        }
    }
}

/// `PDResources.add(section, prefix, object)`: registers `value` under the
/// next free resource name in the page's effective `/Resources`,
/// mutating shared (inherited or referenced) dictionaries in place like
/// PDFBox does. Returns the resource name for content-stream use.
fn register_page_resource(
    doc: &mut Document,
    page_id: ObjectId,
    section: &[u8],
    prefix: &str,
    value: Object,
) -> AppResult<String> {
    let (loc, mut resources) = match resources_dict(doc, page_id)? {
        Some(found) => found,
        // No usable resources anywhere: PDFBox puts a fresh dictionary
        // on the page itself.
        None => (ResourceLoc::Inline(page_id), Dictionary::new()),
    };

    let (section_ref, mut section_dict) = match resources.get(section).ok().cloned() {
        Some(Object::Reference(id)) => {
            let dict = doc
                .get_object(id)
                .map_err(pdf_err)
                .ok()
                .and_then(pdfdoc::dict_of)
                .cloned()
                .unwrap_or_default();
            (Some(id), dict)
        }
        Some(Object::Dictionary(dict)) => (None, dict),
        _ => (None, Dictionary::new()),
    };

    let name = next_resource_name(&section_dict, prefix);
    section_dict.set(name.as_str(), value);

    match section_ref {
        Some(id) => match doc.objects.get_mut(&id).and_then(pdfdoc::dict_mut) {
            Some(dict) => *dict = section_dict,
            None => {
                doc.objects.insert(id, Object::Dictionary(section_dict));
            }
        },
        None => resources.set(section, Object::Dictionary(section_dict)),
    }

    match loc {
        ResourceLoc::Ref(id) => {
            doc.objects.insert(id, Object::Dictionary(resources));
        }
        ResourceLoc::Inline(holder) => {
            doc.get_dictionary_mut(holder)
                .map_err(pdf_err)?
                .set("Resources", Object::Dictionary(resources));
        }
    }
    Ok(name)
}

fn graphics_state(doc: &mut Document, page_id: ObjectId, opacity: f64) -> AppResult<String> {
    let mut state = Dictionary::new();
    state.set("ca", Object::Real(opacity as f32));
    register_page_resource(doc, page_id, b"ExtGState", "gs", Object::Dictionary(state))
}

fn image_resource(
    doc: &mut Document,
    page_id: ObjectId,
    image: &EmbeddedImage,
) -> AppResult<String> {
    register_page_resource(doc, page_id, b"XObject", "Im", Object::Reference(image.id))
}

/// `Matrix.getRotateInstance(radians, 0, 0)` as six PDF `cm` operands.
fn rotate_cm(rotation: f64) -> String {
    let radians = rotation.to_radians();
    let (sin, cos) = radians.sin_cos();
    format!(
        "{} {} {} {} 0 0 cm",
        pdf_num(cos),
        pdf_num(sin),
        pdf_num(-sin),
        pdf_num(cos)
    )
}

fn margin_factor(custom_margin: &str) -> f64 {
    match custom_margin.to_ascii_lowercase().as_str() {
        "small" => 0.02,
        "medium" => 0.035,
        "large" => 0.05,
        "x-large" => 0.075,
        _ => 0.035,
    }
}

/// `StampController.enterStampBox` origin: explicit overrides win when both
/// are nonnegative; otherwise the 1-9 grid position. The result is clamped
/// inside the media box using the unrotated stamp size. `edge_margin` is
/// `marginFactor * (pageW + pageH) / 2` already evaluated by the caller.
fn stamp_origin(
    media: [f64; 4],
    position: i32,
    width: f64,
    height: f64,
    edge_margin: f64,
    override_x: f64,
    override_y: f64,
) -> (f64, f64) {
    let (llx, lly, urx, ury) = (media[0], media[1], media[2], media[3]);
    let page_w = urx - llx;
    let page_h = ury - lly;
    // i64 keeps `position - 1` from overflowing at i32::MIN; Rust `/` and
    // `%` truncate toward zero like Java's int arithmetic.
    let pos = position as i64;
    let (x, y) = if override_x >= 0.0 && override_y >= 0.0 {
        (override_x, override_y)
    } else {
        let x = match pos % 3 {
            1 => llx + edge_margin,
            2 => llx + (page_w - width) / 2.0,
            0 => urx - width - edge_margin,
            _ => 0.0,
        };
        // QR and image stamps share `calculateImagePositionY`.
        let y = match (pos - 1) / 3 {
            0 => ury - edge_margin - height,
            1 => lly + (page_h - height) / 2.0,
            2 => lly + edge_margin,
            _ => lly,
        };
        (x, y)
    };
    (
        x.clamp(llx, (urx - width).max(llx)),
        y.clamp(lly, (ury - height).max(lly)),
    )
}

// QR stamp constants mirror `StampController`: the byte cap is UTF-8 length
// (QR byte mode), the quiet zone is 4 modules per side, and EC level M.
const MAX_QR_CONTENT_BYTES: usize = 1000;
const QR_QUIET_ZONE_MODULES: usize = 4;
const ESCAPED_AT_PLACEHOLDER: &str = "\u{E000}ESCAPED_AT\u{E000}";
const MAX_STAMP_DATE_FORMAT_LEN: usize = 50;

/// `StampController.requireEncodableQrContent`: blank or over the byte cap
/// is a 400. Runs on the raw text up front and again per page after
/// placeholder expansion — `@author` can resolve to nothing and `@filename`
/// can grow the text past the cap.
fn require_qr_content(content: &str) -> AppResult<()> {
    if content.trim().is_empty() {
        return Err(AppError::BadRequest(
            "Stamp text must be provided when stamp type is 'qr'".into(),
        ));
    }
    if content.len() > MAX_QR_CONTENT_BYTES {
        return Err(AppError::BadRequest(format!(
            "QR code content must be at most {MAX_QR_CONTENT_BYTES} bytes of UTF-8 text"
        )));
    }
    Ok(())
}

/// `QRCodeWriter.encode(content, QR_CODE, 0, 0, hints)` at EC level M with a
/// 4-module margin: returns the module count *including* the quiet zone and
/// row-major dark flags, matching the `BitMatrix` Java iterates.
fn qr_modules(content: &str) -> AppResult<(usize, Vec<bool>)> {
    let qr = qrcode::QrCode::with_error_correction_level(content, qrcode::EcLevel::M)
        .map_err(|_| AppError::BadRequest("QR code content is too long to encode".into()))?;
    let core = qr.width();
    let total = core + 2 * QR_QUIET_ZONE_MODULES;
    let colors = qr.to_colors();
    let mut modules = vec![false; total * total];
    for row in 0..core {
        for col in 0..core {
            if colors[row * core + col] == qrcode::Color::Dark {
                modules[(row + QR_QUIET_ZONE_MODULES) * total + col + QR_QUIET_ZONE_MODULES] = true;
            }
        }
    }
    Ok((total, modules))
}

struct QrStampLayout<'a> {
    modules: usize,
    matrix: &'a [bool],
    size: f64,
    x: f64,
    y: f64,
    rotation: f64,
    opacity: f64,
}

/// `StampController.addQrStamp`: white square then black module runs as
/// filled vector rects, inside the same translate/rotate box as image
/// stamps so it stays sharp at any zoom.
fn qr_stamp_ops(
    doc: &mut Document,
    page_id: ObjectId,
    stamp: QrStampLayout<'_>,
) -> AppResult<String> {
    let QrStampLayout {
        modules,
        matrix,
        size,
        x,
        y,
        rotation,
        opacity,
    } = stamp;
    let gs = graphics_state(doc, page_id, opacity)?;
    let module = size / modules as f64;
    let mut ops = format!(
        "/{gs} gs\nq 1 0 0 1 {} {} cm\n{}\n1 1 1 rg\n0 0 {} {} re\nf\n0 0 0 rg\n",
        pdf_num(x),
        pdf_num(y),
        rotate_cm(rotation),
        pdf_num(size),
        pdf_num(size),
    );
    for row in 0..modules {
        // PDF y grows upward; QR row 0 is the top row.
        let top = size - (row as f64 + 1.0) * module;
        let mut col = 0;
        while col < modules {
            if !matrix[row * modules + col] {
                col += 1;
                continue;
            }
            let start = col;
            while col < modules && matrix[row * modules + col] {
                col += 1;
            }
            ops.push_str(&format!(
                "{} {} {} {} re\n",
                pdf_num(start as f64 * module),
                pdf_num(top),
                pdf_num((col - start) as f64 * module),
                pdf_num(module),
            ));
        }
    }
    ops.push_str("f\nQ\n");
    Ok(ops)
}

/// `StampController.processStampText`: expands `@` placeholders. `@@`
/// escapes a literal `@`; `@date{fmt}` takes a Java `DateTimeFormatter`
/// pattern; the plain tokens substitute per-page values. Replacements run
/// in Java's order, so an earlier token inside a later value (e.g. `@page`
/// inside `@author`) is still substituted — same as `String.replace`.
fn process_stamp_text(
    stamp_text: &str,
    page_number: usize,
    total_pages: usize,
    filename: &str,
    doc: &Document,
) -> AppResult<String> {
    if stamp_text.is_empty() {
        return Ok(String::new());
    }
    let mut result = stamp_text.replace("@@", ESCAPED_AT_PLACEHOLDER);

    let now = Local::now().naive_local();
    let current_date = now.format("%Y-%m-%d").to_string();
    let current_time = now.format("%H:%M:%S").to_string();
    let current_date_time = now.format("%Y-%m-%d %H:%M:%S").to_string();

    // Java strips the extension only when the last dot is not first.
    let filename_without_ext = match filename.rfind('.') {
        Some(dot) if dot > 0 => &filename[..dot],
        _ => filename,
    };
    let author = pdfdoc::info_value(doc, "Author").unwrap_or_default();
    let title = pdfdoc::info_value(doc, "Title").unwrap_or_default();
    let subject = pdfdoc::info_value(doc, "Subject").unwrap_or_default();
    // Java takes UUID.randomUUID().toString().substring(0, 8): 4 bytes hex.
    let mut uuid_bytes = [0u8; 4];
    rand::rng().fill(&mut uuid_bytes);
    let uuid: String = uuid_bytes.iter().map(|b| format!("{b:02x}")).collect();

    result = expand_custom_dates(&result, &now)?;

    result = result
        .replace("@datetime", &current_date_time)
        .replace("@date", &current_date)
        .replace("@time", &current_time)
        .replace("@year", &now.format("%Y").to_string())
        .replace("@month", &format!("{:02}", now.month()))
        .replace("@day", &format!("{:02}", now.day()))
        .replace("@page_number", &page_number.to_string())
        .replace("@page_count", &total_pages.to_string())
        .replace("@total_pages", &total_pages.to_string())
        .replace("@page", &page_number.to_string())
        .replace("@filename_full", filename)
        .replace("@filename", filename_without_ext)
        .replace("@author", &author)
        .replace("@title", &title)
        .replace("@subject", &subject)
        .replace("@uuid", &uuid)
        .replace(ESCAPED_AT_PLACEHOLDER, "@");
    Ok(result)
}

/// Replaces every `@date{fmt}` with the formatted date, like the Java
/// controller's `CUSTOM_DATE_PATTERN` pass. An inner pattern longer than
/// 50 chars or containing `}` fails the regex and stays literal — except the
/// `@date` prefix still gets substituted by the plain-token pass later.
fn expand_custom_dates(text: &str, now: &NaiveDateTime) -> AppResult<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find("@date{") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + "@date{".len()..];
        match after.find('}') {
            Some(close)
                if (1..=MAX_STAMP_DATE_FORMAT_LEN).contains(&after[..close].chars().count()) =>
            {
                out.push_str(&format_custom_date(&after[..close], now)?);
                rest = &after[close + 1..];
            }
            _ => {
                out.push_str("@date{");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// `StampController.processCustomDateFormat`: over-long and unsafe patterns
/// render fixed error strings; a pattern Java's `ofPattern` would reject
/// renders `[invalid format: <fmt>]`. Timezone fields (`z`, `Z`, `X`)
/// cannot format a `LocalDateTime` in Java either — that throws a
/// `DateTimeException`, surfacing as a 500, so it errors here too.
fn format_custom_date(fmt: &str, now: &NaiveDateTime) -> AppResult<String> {
    if fmt.chars().count() > MAX_STAMP_DATE_FORMAT_LEN {
        return Ok("[invalid format: too long]".into());
    }
    if !fmt.chars().all(is_safe_date_format_char) {
        return Ok("[invalid format]".into());
    }
    match java_date_format(now, fmt) {
        Ok(s) => Ok(s),
        Err(JavaDateError::Invalid) => Ok(format!("[invalid format: {fmt}]")),
        Err(JavaDateError::UnsupportedZone) => Err(AppError::Internal(anyhow::anyhow!(
            "date format field needs a zone: {fmt}"
        ))),
    }
}

/// `SAFE_DATE_FORMAT_PATTERN` in `StampController`.
fn is_safe_date_format_char(c: char) -> bool {
    matches!(
        c,
        'y' | 'M'
            | 'd'
            | 'H'
            | 'h'
            | 'm'
            | 's'
            | 'S'
            | '/'
            | '-'
            | ':'
            | ' '
            | '.'
            | ','
            | '\''
            | '+'
            | 'E'
            | 'G'
            | 'u'
            | 'w'
            | 'W'
            | 'D'
            | 'F'
            | 'z'
            | 'Z'
            | 'X'
            | 'a'
    ) || c.is_whitespace()
}

#[derive(Debug)]
enum JavaDateError {
    /// `DateTimeFormatter.ofPattern` rejected the pattern.
    Invalid,
    /// The pattern needs zone/offset data a `LocalDateTime` does not have.
    UnsupportedZone,
}

/// Subset of `DateTimeFormatter.ofPattern` covering the letters
/// `SAFE_DATE_FORMAT_PATTERN` admits, evaluated against a `LocalDateTime`
/// like Java does.
fn java_date_format(now: &NaiveDateTime, pattern: &str) -> Result<String, JavaDateError> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            if chars.get(i + 1) == Some(&'\'') {
                out.push('\'');
                i += 2;
                continue;
            }
            i += 1;
            let mut closed = false;
            while i < chars.len() {
                if chars[i] == '\'' {
                    if chars.get(i + 1) == Some(&'\'') {
                        out.push('\'');
                        i += 2;
                        continue;
                    }
                    closed = true;
                    i += 1;
                    break;
                }
                out.push(chars[i]);
                i += 1;
            }
            if !closed {
                return Err(JavaDateError::Invalid);
            }
            continue;
        }
        if c.is_ascii_alphabetic() {
            let mut count = 1;
            while chars.get(i + count) == Some(&c) {
                count += 1;
            }
            out.push_str(&date_field(now, c, count)?);
            i += count;
        } else {
            out.push(c);
            i += 1;
        }
    }
    Ok(out)
}

fn pad_num(value: u32, width: usize) -> String {
    format!("{value:0>width$}")
}

fn date_field(now: &NaiveDateTime, c: char, count: usize) -> Result<String, JavaDateError> {
    let date = now.date();
    let time = now.time();
    Ok(match c {
        // 'y' is year-of-era and 'u' proleptic year; identical for AD dates.
        // Two letters give the reduced year, anything else pads to the count.
        'y' | 'u' => {
            let year = date.year();
            if count == 2 {
                pad_num(year.rem_euclid(100) as u32, 2)
            } else {
                pad_num(year as u32, count)
            }
        }
        'M' => match count {
            1 => date.month().to_string(),
            2 => pad_num(date.month(), 2),
            3 => now.format("%b").to_string(),
            4 => now.format("%B").to_string(),
            _ => now
                .format("%B")
                .to_string()
                .chars()
                .next()
                .unwrap_or_default()
                .to_string(),
        },
        'd' => pad_num(date.day(), count),
        'D' => pad_num(date.ordinal(), count),
        'H' => pad_num(time.hour(), count),
        'h' => pad_num((time.hour() + 11) % 12 + 1, count),
        'm' => pad_num(time.minute(), count),
        's' => pad_num(time.second(), count),
        'S' => {
            let digits = format!("{:09}", time.nanosecond());
            let mut frac: String = digits.chars().take(count.min(9)).collect();
            if count > 9 {
                frac.push_str(&"0".repeat(count - 9));
            }
            frac
        }
        'E' => match count {
            1..=3 => now.format("%a").to_string(),
            4 => now.format("%A").to_string(),
            _ => now
                .format("%A")
                .to_string()
                .chars()
                .next()
                .unwrap_or_default()
                .to_string(),
        },
        'G' => {
            let ad = date.year() > 0;
            match (count, ad) {
                (4, true) => "Anno Domini".into(),
                (4, false) => "Before Christ".into(),
                (n, true) if n >= 5 => "A".into(),
                (n, false) if n >= 5 => "B".into(),
                (_, true) => "AD".into(),
                (_, false) => "BC".into(),
            }
        }
        'a' => {
            let pm = time.hour() >= 12;
            match (count >= 5, pm) {
                (true, true) => "p".into(),
                (true, false) => "a".into(),
                (_, true) => "PM".into(),
                (_, false) => "AM".into(),
            }
        }
        // Java 'w' is week-of-week-based-year under the default locale's
        // week rules (US: Sunday start, week 1 contains Jan 1). Chrono has
        // no equivalent; this is the Sunday-start/minimalDays=1 formula.
        'w' => {
            let jan1 = NaiveDate::from_ymd_opt(date.year(), 1, 1).ok_or(JavaDateError::Invalid)?;
            let jan1_sunday0 = jan1.weekday().num_days_from_sunday() as i64;
            let week = (i64::from(date.ordinal()) - 1 + jan1_sunday0) / 7 + 1;
            week.to_string()
        }
        'W' | 'F' => pad_num((date.day() - 1) / 7 + 1, count),
        'z' | 'Z' | 'X' => return Err(JavaDateError::UnsupportedZone),
        _ => return Err(JavaDateError::Invalid),
    })
}

/// `POST /api/v1/misc/add-stamp` — `stampType=image` and `stampType=qr`
/// are served natively; text stamps (NotoSans TTF embedding) are not
/// implemented and rejected.
pub async fn add_stamp(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, extras, params) = receive_files_multipart(multipart, &["stampImage"]).await?;
    let stamp_image = extras
        .iter()
        .find(|(field, _)| field == "stampImage")
        .map(|(_, upload)| upload);

    let stamp_type = param(&params, "stampType", "");
    let has_stamp_image = stamp_image.is_some_and(|u| !u.bytes.is_empty());
    let resolved_image =
        stamp_type.trim().is_empty() && has_stamp_image || stamp_type.eq_ignore_ascii_case("image");
    if stamp_type.eq_ignore_ascii_case("qr") {
        // Java checks the input filename before stamp-type validation.
        if let Some(upload) = uploads.first() {
            if invalid_file_name(&upload.name) {
                return Err(AppError::BadRequest(format!(
                    "Invalid PDF file path: {}",
                    upload.name
                )));
            }
        }
        let stamp_text = param(&params, "stampText", "").to_string();
        require_qr_content(&stamp_text)?;
        return dispatch(state, query, uploads, params, move |_s, _q, u, p| {
            qr_stamp_work(u, p, stamp_text)
        })
        .await;
    }
    if !resolved_image {
        return Err(AppError::BadRequest(
            "text stamps are not implemented; only stampType=image and stampType=qr are supported"
                .into(),
        ));
    }

    let Some(image) = stamp_image else {
        return Err(AppError::BadRequest(
            "Stamp image file must be provided when stamp type is 'image'".into(),
        ));
    };
    if image.bytes.is_empty() {
        return Err(AppError::BadRequest(
            "Stamp image file must be provided when stamp type is 'image'".into(),
        ));
    }
    if let Some(upload) = uploads.first() {
        if invalid_file_name(&upload.name) {
            return Err(AppError::BadRequest(format!(
                "Invalid PDF file path: {}",
                upload.name
            )));
        }
    }
    if invalid_file_name(&image.name) {
        return Err(AppError::BadRequest(format!(
            "Invalid stamp image file path: {}",
            image.name
        )));
    }
    if !is_native_raster(&image.bytes) {
        return Err(AppError::BadRequest(
            "stampImage must be a supported raster image".into(),
        ));
    }

    // Move the image upload into the blocking work closure.
    let image = Upload {
        name: image.name.clone(),
        bytes: image.bytes.clone(),
        content_type: image.content_type.clone(),
    };
    dispatch(state, query, uploads, params, move |_s, _q, u, p| {
        stamp_image_work(u, p, image)
    })
    .await
}

fn stamp_image_work(
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
    image_upload: Upload,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };

    let position = int_param(&params, "position", 5)?;
    let font_size = float_param(&params, "fontSize", 40.0)?;
    let opacity = float_param(&params, "opacity", 0.5)?;
    let rotation = float_param(&params, "rotation", 0.0)?;
    let override_x = float_param(&params, "overrideX", -1.0)?;
    let override_y = float_param(&params, "overrideY", -1.0)?;
    let margin = margin_factor(param(&params, "customMargin", "medium"));
    // PDFWithPageNums treats a blank pageNumbers as "all".
    let page_numbers = param(&params, "pageNumbers", "all");
    let page_numbers = if page_numbers.trim().is_empty() {
        "all"
    } else {
        page_numbers
    };

    let mut document = pdfdoc::load(&upload.name, &upload.bytes)?;
    let page_ids = pdfdoc::page_ids(&document);
    let total_pages = page_ids.len();
    page_select::require_valid_page_selection(page_numbers, total_pages)
        .map_err(AppError::BadRequest)?;
    let selected =
        page_select::parse_page_list(page_numbers, total_pages).map_err(AppError::BadRequest)?;

    let image = embed_image(&mut document, &image_upload.bytes)?;
    let aspect = image.width as f64 / image.height as f64;
    let desired_height = font_size;
    let desired_width = desired_height * aspect;

    for index in selected {
        let page_id = page_ids[index];
        let media = pdfdoc::media_box(&document, page_id)?;
        let page_w = media[2] - media[0];
        let page_h = media[3] - media[1];
        let edge_margin = margin * (page_w + page_h) / 2.0;
        let (x, y) = stamp_origin(
            media,
            position,
            desired_width,
            desired_height,
            edge_margin,
            override_x,
            override_y,
        );

        let gs = graphics_state(&mut document, page_id, opacity)?;
        let im = image_resource(&mut document, page_id, &image)?;
        let ops = format!(
            "/{gs} gs\nq 1 0 0 1 {} {} cm {} q {} 0 0 {} 0 0 cm /{im} Do Q Q\n",
            pdf_num(x),
            pdf_num(y),
            rotate_cm(rotation),
            pdf_num(desired_width),
            pdf_num(desired_height),
        );
        append_page_content(&mut document, page_id, &ops)?;
    }

    pdfdoc::touch_loaded_info(&mut document);
    let bytes = pdfdoc::save(&mut document)?;
    Ok(pdf_response(
        bytes,
        &output_name(Some(&upload.name), "_stamped.pdf"),
    ))
}

/// `stampType=qr`: the stamp text is expanded per page (`@page`,
/// `@filename`, …) then drawn as a vector QR — white square plus black
/// module rects — using the same placement rules as image stamps.
fn qr_stamp_work(
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
    stamp_text: String,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };

    let position = int_param(&params, "position", 5)?;
    let font_size = float_param(&params, "fontSize", 40.0)?;
    let opacity = float_param(&params, "opacity", 0.5)?;
    let rotation = float_param(&params, "rotation", 0.0)?;
    let override_x = float_param(&params, "overrideX", -1.0)?;
    let override_y = float_param(&params, "overrideY", -1.0)?;
    let margin = margin_factor(param(&params, "customMargin", "medium"));
    let page_numbers = param(&params, "pageNumbers", "all");
    let page_numbers = if page_numbers.trim().is_empty() {
        "all"
    } else {
        page_numbers
    };
    // Java: `size > 0 ? size : 40f`; NaN falls back the same way.
    let size = if font_size > 0.0 { font_size } else { 40.0 };

    let mut document = pdfdoc::load(&upload.name, &upload.bytes)?;
    let page_ids = pdfdoc::page_ids(&document);
    let total_pages = page_ids.len();
    page_select::require_valid_page_selection(page_numbers, total_pages)
        .map_err(AppError::BadRequest)?;
    let selected =
        page_select::parse_page_list(page_numbers, total_pages).map_err(AppError::BadRequest)?;

    for index in selected {
        let page_id = page_ids[index];
        let media = pdfdoc::media_box(&document, page_id)?;
        let page_w = media[2] - media[0];
        let page_h = media[3] - media[1];
        let edge_margin = margin * (page_w + page_h) / 2.0;

        // Expanded and re-validated per page: a placeholder can resolve to
        // nothing or grow the text past the byte cap.
        let content =
            process_stamp_text(&stamp_text, index + 1, total_pages, &upload.name, &document)?;
        require_qr_content(&content)?;
        let (modules, matrix) = qr_modules(&content)?;
        let (x, y) = stamp_origin(
            media,
            position,
            size,
            size,
            edge_margin,
            override_x,
            override_y,
        );
        let ops = qr_stamp_ops(
            &mut document,
            page_id,
            QrStampLayout {
                modules,
                matrix: &matrix,
                size,
                x,
                y,
                rotation,
                opacity,
            },
        )?;
        append_page_content(&mut document, page_id, &ops)?;
    }

    pdfdoc::touch_loaded_info(&mut document);
    let bytes = pdfdoc::save(&mut document)?;
    Ok(pdf_response(
        bytes,
        &output_name(Some(&upload.name), "_stamped.pdf"),
    ))
}

/// `POST /api/v1/security/add-watermark` — only `watermarkType=image`
/// without `convertPDFToImage` is served natively; text watermarks and
/// the rasterising conversion are not implemented and rejected.
pub async fn add_watermark(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    multipart: Multipart,
) -> AppResult<Response> {
    let (uploads, extras, params) = receive_files_multipart(multipart, &["watermarkImage"]).await?;
    if param_bool(&params, "convertPDFToImage") {
        return Err(AppError::BadRequest(
            "add-watermark convertPDFToImage rasterises every page, which the Rust backend does not implement".into(),
        ));
    }
    let watermark_image = extras
        .iter()
        .find(|(field, _)| field == "watermarkImage")
        .map(|(_, upload)| upload);

    let watermark_type = param(&params, "watermarkType", "");
    let resolved_image = watermark_type.trim().is_empty()
        && watermark_image.is_some_and(|u| !u.bytes.is_empty())
        || watermark_type.eq_ignore_ascii_case("image");
    if !resolved_image {
        return Err(AppError::BadRequest(
            "text watermarks are not implemented; only watermarkType=image is supported".into(),
        ));
    }

    let Some(image) = watermark_image else {
        return Err(AppError::BadRequest(
            "watermarkImage must be provided when watermarkType is 'image'".into(),
        ));
    };
    if image.bytes.is_empty() {
        return Err(AppError::BadRequest(
            "watermarkImage must be provided when watermarkType is 'image'".into(),
        ));
    }
    if let Some(upload) = uploads.first() {
        if invalid_file_name(&upload.name) {
            // Java throws SecurityException here, surfacing as a 500.
            return Err(AppError::Internal(anyhow::anyhow!(
                "Invalid file path in pdfFile"
            )));
        }
    }
    if invalid_file_name(&image.name) {
        return Err(AppError::Internal(anyhow::anyhow!(
            "Invalid file path in watermarkImage"
        )));
    }
    if !is_native_raster(&image.bytes) {
        return Err(AppError::BadRequest(
            "watermarkImage must be a supported raster image".into(),
        ));
    }

    let image = Upload {
        name: image.name.clone(),
        bytes: image.bytes.clone(),
        content_type: image.content_type.clone(),
    };
    dispatch(state, query, uploads, params, move |_s, _q, u, p| {
        watermark_image_work(u, p, image)
    })
    .await
}

fn watermark_image_work(
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
    image_upload: Upload,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };

    // @DecimalMin(1.0) / @Min(0) validation on the Java model.
    let font_size = float_param(&params, "fontSize", 12.0)?;
    if font_size < 1.0 {
        return Err(AppError::BadRequest(
            "Font size must be at least 1.0".into(),
        ));
    }
    let rotation = float_param(&params, "rotation", 0.0)?;
    let opacity = float_param(&params, "opacity", 0.5)?;
    let width_spacer = int_param(&params, "widthSpacer", 50)?;
    if width_spacer < 0 {
        return Err(AppError::BadRequest(
            "Width spacer must be non-negative".into(),
        ));
    }
    let height_spacer = int_param(&params, "heightSpacer", 50)?;
    if height_spacer < 0 {
        return Err(AppError::BadRequest(
            "Height spacer must be non-negative".into(),
        ));
    }

    let mut document = pdfdoc::load(&upload.name, &upload.bytes)?;
    let image = embed_image(&mut document, &image_upload.bytes)?;
    let aspect = image.width as f64 / image.height as f64;
    let tile_w = font_size * aspect;
    let tile_h = font_size;
    let rotate = rotate_cm(rotation);

    for page_id in pdfdoc::page_ids(&document) {
        let media = pdfdoc::media_box(&document, page_id)?;
        let page_w = media[2] - media[0];
        let page_h = media[3] - media[1];
        // Java's (int) cast truncates toward zero, like `as i64` on f64.
        let rows = (((page_h + height_spacer as f64) / (tile_h + height_spacer as f64)) as i64)
            .clamp(0, 10_000) as usize;
        let cols = (((page_w + width_spacer as f64) / (tile_w + width_spacer as f64)) as i64)
            .clamp(0, 10_000) as usize;

        let gs = graphics_state(&mut document, page_id, opacity)?;
        let im = image_resource(&mut document, page_id, &image)?;

        let mut ops = format!("/{gs} gs\n");
        for row in 0..rows {
            for col in 0..cols {
                let x = col as f64 * (tile_w + width_spacer as f64);
                let y = row as f64 * (tile_h + height_spacer as f64);
                let cx = x + tile_w / 2.0;
                let cy = y + tile_h / 2.0;
                ops.push_str(&format!(
                    "q 1 0 0 1 {} {} cm {} 1 0 0 1 {} {} cm q {} 0 0 {} 0 0 cm /{im} Do Q Q\n",
                    pdf_num(cx),
                    pdf_num(cy),
                    rotate,
                    pdf_num(-tile_w / 2.0),
                    pdf_num(-tile_h / 2.0),
                    pdf_num(tile_w),
                    pdf_num(tile_h),
                ));
            }
        }
        append_page_content(&mut document, page_id, &ops)?;
    }

    pdfdoc::touch_loaded_info(&mut document);
    let bytes = pdfdoc::save(&mut document)?;
    Ok(pdf_response(
        bytes,
        &output_name(Some(&upload.name), "_watermarked.pdf"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_file_name_rejects_traversal_and_absolute() {
        assert!(invalid_file_name(".."));
        assert!(invalid_file_name("../x.png"));
        assert!(invalid_file_name("a/../b.png"));
        assert!(invalid_file_name("/abs.png"));
        assert!(!invalid_file_name("stamp.png"));
        assert!(!invalid_file_name("dir/stamp.png"));
    }

    #[test]
    fn native_raster_accepts_every_image_codec() {
        let png = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0];
        let jpeg = [0xFF, 0xD8, 0xFF, 0xE0, 0, 0];
        let gif = *b"GIF89a....";
        let bmp = *b"BM........";
        assert!(is_native_raster(&png));
        assert!(is_native_raster(&jpeg));
        assert!(is_native_raster(&gif));
        assert!(is_native_raster(&bmp));
        assert!(!is_native_raster(b""));
        assert!(!is_native_raster(
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>"
        ));
    }
}

#[cfg(test)]
mod util_tests {
    use super::*;

    #[test]
    fn pdf_num_matches_pdfbox_decimal_format() {
        assert_eq!(pdf_num(42.0), "42");
        assert_eq!(pdf_num(-3.0), "-3");
        assert_eq!(pdf_num(0.5), "0.5");
        // Rounded to 5 fraction digits like PDFBox's DecimalFormat.
        assert_eq!(pdf_num(1.0 / 3.0), "0.33333");
        assert_eq!(pdf_num(612.0), "612");
    }

    #[test]
    fn float_and_int_params_apply_defaults_and_validate() {
        let p = |v: &str| vec![("opacity".to_string(), v.to_string())];
        assert_eq!(float_param(&[], "opacity", 0.5).unwrap(), 0.5);
        assert_eq!(float_param(&p(""), "opacity", 0.5).unwrap(), 0.5);
        assert_eq!(float_param(&p("0.25"), "opacity", 0.5).unwrap(), 0.25);
        assert!(float_param(&p("abc"), "opacity", 0.5).is_err());

        // int params parse i32 like Java's Integer binding — i64-range
        // values are a 400, not a silent wrap.
        let q = |v: &str| vec![("pages".to_string(), v.to_string())];
        assert_eq!(int_param(&[], "pages", 4).unwrap(), 4);
        assert_eq!(int_param(&q("12"), "pages", 4).unwrap(), 12);
        assert!(int_param(&q("9999999999"), "pages", 4).is_err());
        assert!(int_param(&q("1.5"), "pages", 4).is_err());
    }

    #[test]
    fn invalid_file_name_rejects_java_blocked_names() {
        assert!(invalid_file_name("../etc/passwd"));
        assert!(invalid_file_name("a/../b.png"));
        assert!(invalid_file_name("/abs/path.png"));
        assert!(!invalid_file_name("normal.png"));
        assert!(!invalid_file_name("dir/file.png")); // only '..' and leading '/' blocked
    }

    #[test]
    fn is_native_raster_covers_the_image_codec_set() {
        // Minimal valid headers.
        let png = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0];
        assert!(is_native_raster(&png));
        let jpeg = [0xFF, 0xD8, 0xFF, 0xE0, 0, 0, 0, 0];
        assert!(is_native_raster(&jpeg));
        // GIF/BMP/WebP/TIFF decode natively now that the `image`
        // features cover them — only non-raster payloads are rejected.
        assert!(is_native_raster(b"GIF89a...."));
        assert!(is_native_raster(b"BM........"));
        assert!(!is_native_raster(b"not an image"));
    }

    #[test]
    fn rotate_cm_produces_pdf_matrix() {
        // 0° is identity.
        assert_eq!(rotate_cm(0.0), "1 0 0 1 0 0 cm");
        // 90° CCW: cos=0, sin=1 → "0 1 -1 0 0 0 cm" (modulo fp noise).
        let cm = rotate_cm(90.0);
        assert!(cm.starts_with('0') || cm.starts_with("6.12"));
        assert!(cm.contains("-1"));
    }

    #[test]
    fn margin_factor_maps_named_margins() {
        assert_eq!(margin_factor("small"), 0.02);
        assert_eq!(margin_factor("medium"), 0.035);
        assert_eq!(margin_factor("large"), 0.05);
        assert_eq!(margin_factor("x-large"), 0.075);
        assert_eq!(margin_factor("X-LARGE"), 0.075);
        // Unknown/blank falls back to medium like Java's default.
        assert_eq!(margin_factor("huge"), 0.035);
        assert_eq!(margin_factor(""), 0.035);
    }
}

#[cfg(test)]
mod qr_tests {
    use super::*;
    use crate::testkit;
    use lopdf::{dictionary, Object};

    /// Decodes the module matrix the way a scanner would: `true` cells are
    /// dark modules. rqrr expects the bare symbol — the 4-module quiet zone
    /// `qr_modules` bakes in for rendering is stripped here.
    fn decode_matrix(content: &str) -> String {
        let (size, matrix) = qr_modules(content).unwrap();
        let core = size - 2 * QR_QUIET_ZONE_MODULES;
        let grid = rqrr::SimpleGrid::from_func(core, |x, y| {
            matrix[(y + QR_QUIET_ZONE_MODULES) * size + x + QR_QUIET_ZONE_MODULES]
        });
        rqrr::Grid::new(grid).decode().unwrap().1
    }

    #[test]
    fn qr_matrix_roundtrips_through_decoder() {
        for content in [
            "https://example.com/path?q=1",
            "Hello, QR stamp!",
            "unicode: こんにちは世界 🌍",
            "specials: \\ \" ' \n\t €",
            &"a".repeat(900),
        ] {
            assert_eq!(decode_matrix(content), content, "content: {content:?}");
        }
    }

    #[test]
    fn qr_content_validation_matches_java() {
        assert!(require_qr_content("  ").is_err());
        assert!(require_qr_content("").is_err());
        assert!(require_qr_content(&"x".repeat(1001)).is_err());
        // The cap is UTF-8 bytes, not chars: 500 two-byte chars = 1000 ok,
        // 501 = 1002 rejected.
        assert!(require_qr_content(&"é".repeat(500)).is_ok());
        assert!(require_qr_content(&"é".repeat(501)).is_err());
        assert!(require_qr_content(&"x".repeat(1000)).is_ok());
    }

    fn stamp_doc() -> Document {
        let mut pdf = testkit::Pdf::new();
        pdf.set_info(dictionary! {
            "Title" => Object::string_literal("My Doc"),
            "Author" => Object::string_literal("Jane"),
            "Subject" => Object::string_literal("Testing"),
        });
        pdfdoc::load("in.pdf", &pdf.bytes()).unwrap()
    }

    #[test]
    fn stamp_text_expands_placeholders() {
        let doc = stamp_doc();
        let out = process_stamp_text(
            "@title by @author — @filename on p@page/@total_pages @@literal @uuid",
            2,
            5,
            "report.final.pdf",
            &doc,
        )
        .unwrap();
        assert!(out.starts_with("My Doc by Jane — report.final on p2/5 @literal "));
        // @uuid is 8 hex chars.
        let uuid = out.rsplit(' ').next().unwrap();
        assert_eq!(uuid.len(), 8);
        assert!(uuid.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn stamp_text_expands_date_tokens() {
        let doc = stamp_doc();
        let out = process_stamp_text("@date @time @datetime", 1, 1, "f.pdf", &doc).unwrap();
        let parts: Vec<&str> = out.split(' ').collect();
        // @datetime expands to "yyyy-MM-dd HH:mm:ss" — a space inside, so
        // the joined string splits into four tokens, matching Java.
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0].len(), 10); // yyyy-MM-dd
        assert_eq!(parts[1].len(), 8); // HH:mm:ss
        assert_eq!(parts[2].len(), 10); // @datetime's date half
        assert_eq!(parts[3].len(), 8); // @datetime's time half
    }

    #[test]
    fn custom_date_format_matches_java_patterns() {
        let now = NaiveDate::from_ymd_opt(2026, 11, 6)
            .unwrap()
            .and_hms_opt(13, 5, 7)
            .unwrap();
        assert_eq!(java_date_format(&now, "yyyy-MM-dd").unwrap(), "2026-11-06");
        assert_eq!(java_date_format(&now, "dd/MM/yyyy").unwrap(), "06/11/2026");
        assert_eq!(java_date_format(&now, "HH:mm:ss").unwrap(), "13:05:07");
        assert_eq!(java_date_format(&now, "h a").unwrap(), "1 PM");
        assert_eq!(java_date_format(&now, "EEE, MMM d").unwrap(), "Fri, Nov 6");
        assert_eq!(
            java_date_format(&now, "yyyy 'year' MM 'month'").unwrap(),
            "2026 year 11 month"
        );
        assert_eq!(java_date_format(&now, "D").unwrap(), "310");
        assert_eq!(java_date_format(&now, "SSS").unwrap(), "000");
        // Unclosed quote is an ofPattern failure.
        assert!(matches!(
            java_date_format(&now, "yyyy'"),
            Err(JavaDateError::Invalid)
        ));
        // Zone fields cannot format a LocalDateTime — Java throws too.
        assert!(matches!(
            java_date_format(&now, "HH:mm z"),
            Err(JavaDateError::UnsupportedZone)
        ));
    }

    #[test]
    fn custom_date_format_guards() {
        let now = NaiveDate::from_ymd_opt(2026, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        assert_eq!(
            format_custom_date(&"y".repeat(51), &now).unwrap(),
            "[invalid format: too long]"
        );
        assert_eq!(
            format_custom_date("yyyy;DROP", &now).unwrap(),
            "[invalid format]"
        );
        // A quoted 'z' is a literal, not a zone field.
        assert_eq!(format_custom_date("HH 'z'", &now).unwrap(), "00 z");
        assert!(format_custom_date("HH z", &now).is_err());
    }

    #[test]
    fn expand_custom_dates_replaces_and_leaves_invalid_literal() {
        let now = NaiveDate::from_ymd_opt(2026, 11, 6)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        assert_eq!(
            expand_custom_dates("d=@date{yyyy}", &now).unwrap(),
            "d=2026"
        );
        // Over-long inner stays literal for the @date token pass to mangle,
        // exactly like the failed Java regex.
        let long = format!("@date{{{}}}", "y".repeat(51));
        assert_eq!(expand_custom_dates(&long, &now).unwrap(), long);
    }

    #[test]
    fn stamp_origin_grid_and_clamp() {
        let media = [0.0, 0.0, 612.0, 792.0];
        // Center (5), 40pt stamp, medium margin.
        let (x, y) = stamp_origin(media, 5, 40.0, 40.0, 24.57, -1.0, -1.0);
        assert!((x - 286.0).abs() < 1e-6);
        assert!((y - 376.0).abs() < 1e-6);
        // Overrides win when both are nonnegative.
        let (x, y) = stamp_origin(media, 1, 40.0, 40.0, 24.57, 100.0, 200.0);
        assert_eq!((x, y), (100.0, 200.0));
        // A stamp larger than the page clamps to the media box origin.
        let (x, y) = stamp_origin(media, 5, 2000.0, 2000.0, 24.57, -1.0, -1.0);
        assert_eq!((x, y), (0.0, 0.0));
    }

    fn qr_params(extra: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut params: Vec<(String, String)> = [
            ("stampType", "qr"),
            ("stampText", "Hello QR"),
            ("position", "5"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        params.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        params
    }

    fn qr_upload() -> Vec<Upload> {
        vec![Upload {
            name: "in.pdf".into(),
            bytes: crate::upload::UploadBytes::from_vec(testkit::Pdf::new().bytes()),
            content_type: None,
        }]
    }

    /// Rasterizes the `re` fill ops a QR stamp wrote into the page content
    /// stream back into a bitmap and decodes it — proof the vector QR in the
    /// PDF is actually readable, not just well-formed.
    async fn decode_pdf_stamp_qr(bytes: &[u8], stamp_text: &str) -> String {
        let doc = Document::load_mem(bytes).unwrap();
        let scale = 10.0f64;
        let mut raster: Vec<u8> = Vec::new();
        let mut width = 0usize;
        let mut height = 0usize;
        for object in doc.objects.values() {
            let Object::Stream(stream) = object else {
                continue;
            };
            let Ok(content) = stream.decompressed_content() else {
                continue;
            };
            let Ok(text) = std::str::from_utf8(&content) else {
                continue;
            };
            if !text.contains(" re") {
                continue;
            }
            // Track the nonstroking colour; only rects under `0 0 0 rg`
            // are modules (the `1 1 1 rg` background square is not).
            let mut black = false;
            let mut rects: Vec<(f64, f64, f64, f64)> = Vec::new();
            for line in text.lines() {
                let line = line.trim();
                if line == "1 1 1 rg" {
                    black = false;
                } else if line == "0 0 0 rg" {
                    black = true;
                } else if black && line.ends_with(" re") {
                    let nums: Vec<f64> = line
                        .trim_end_matches(" re")
                        .split_whitespace()
                        .filter_map(|t| t.parse().ok())
                        .collect();
                    if nums.len() == 4 {
                        rects.push((nums[0], nums[1], nums[2], nums[3]));
                    }
                }
            }
            if rects.is_empty() {
                continue;
            }
            let max_x = rects.iter().map(|r| r.0 + r.2).fold(0.0, f64::max);
            let max_y = rects.iter().map(|r| r.1 + r.3).fold(0.0, f64::max);
            width = (max_x * scale).ceil() as usize;
            height = (max_y * scale).ceil() as usize;
            raster = vec![255u8; width * height];
            for (x, y, w, h) in rects {
                let x0 = (x * scale) as usize;
                let x1 = ((x + w) * scale).ceil() as usize;
                let y0 = (y * scale) as usize;
                let y1 = ((y + h) * scale).ceil() as usize;
                for py in y0..y1.min(height) {
                    // PDF y is up; image rows are down.
                    let row = height - 1 - py;
                    for px in x0..x1.min(width) {
                        raster[row * width + px] = 0;
                    }
                }
            }
        }
        assert!(
            !raster.is_empty(),
            "no QR drawing ops found in {stamp_text}"
        );
        let pixels = raster.clone();
        let mut prepared =
            rqrr::PreparedImage::prepare_from_greyscale(width, height, move |x, y| {
                pixels[y * width + x]
            });
        let grids = prepared.detect_grids();
        for grid in grids {
            if let Ok((_, decoded)) = grid.decode() {
                return decoded;
            }
        }
        panic!("QR in stamped PDF did not decode");
    }

    #[tokio::test]
    async fn qr_stamp_produces_decodable_pdf() {
        let response = qr_stamp_work(qr_upload(), qr_params(&[]), "Hello QR".into()).unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        assert!(bytes.starts_with(b"%PDF"));
        assert_eq!(decode_pdf_stamp_qr(&bytes, "Hello QR").await, "Hello QR");
    }

    #[tokio::test]
    async fn qr_stamp_expands_page_placeholders() {
        let mut pdf = testkit::Pdf::new();
        pdf.add_page([0, 0, 612, 792], Dictionary::new());
        let upload = vec![Upload {
            name: "doc.pdf".into(),
            bytes: crate::upload::UploadBytes::from_vec(pdf.bytes()),
            content_type: None,
        }];
        let response = qr_stamp_work(
            upload,
            qr_params(&[
                ("pageNumbers", "2"),
                ("stampText", "page @page of @total_pages"),
            ]),
            "page @page of @total_pages".into(),
        )
        .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        assert_eq!(
            decode_pdf_stamp_qr(&bytes, "page 2 of 2").await,
            "page 2 of 2"
        );
    }

    #[test]
    fn qr_stamp_rejects_blank_and_oversized() {
        assert!(qr_stamp_work(qr_upload(), qr_params(&[]), "   ".into()).is_err());
        let big = "x".repeat(1001);
        assert!(qr_stamp_work(qr_upload(), qr_params(&[]), big).is_err());
    }

    #[test]
    fn qr_stamp_validates_params_and_pages() {
        // Bad int param → 400 like Java's binding error.
        assert!(qr_stamp_work(qr_upload(), qr_params(&[("position", "abc")]), "x".into()).is_err());
        // Out-of-range page selection → 400.
        assert!(
            qr_stamp_work(qr_upload(), qr_params(&[("pageNumbers", "9")]), "x".into()).is_err()
        );
        // Missing fileInput → 400.
        assert!(qr_stamp_work(vec![], qr_params(&[]), "x".into()).is_err());
    }
}
