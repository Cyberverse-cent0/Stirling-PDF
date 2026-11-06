//! `/api/v1/misc/scanner-effect` — `ScannerEffectController`: render each
//! page, then border/rotate/feather/blur/grade it into a fake scan and
//! re-embed it as an image-only PDF.

use std::collections::HashMap;

use axum::response::Response;
use image::RgbImage;
use lopdf::Document;

use crate::error::{AppError, AppResult};
use crate::handlers::convert::{commit_page, PageImage};
use crate::handlers::jobs::AppState;
use crate::handlers::merge::pdf_response;
use crate::handlers::misc::BlankRasterizer;
use crate::pdfdoc;
use crate::upload::{output_name, param, param_bool, param_f64, Upload, FILE_FIELD};

/// Java's `ApplicationProperties.system.maxDPI` default; the Rust settings
/// tree has no `system.maxDPI` counterpart yet.
const MAX_SAFE_DPI: i64 = 500;
const MAX_IMAGE_SIDE: u32 = 8192;
const MAX_IMAGE_PIXELS: u64 = 16_777_216;

#[derive(Clone, Copy, PartialEq)]
enum Quality {
    Low,
    Medium,
    High,
}

#[derive(Clone, Copy)]
enum RotationPreset {
    None,
    Slight,
    Moderate,
    Severe,
}

impl RotationPreset {
    fn value(self) -> i64 {
        match self {
            Self::None => 0,
            Self::Slight => 2,
            Self::Moderate => 5,
            Self::Severe => 8,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Colorspace {
    Grayscale,
    Color,
}

/// `ScannerEffectRequest` field defaults; quality presets overwrite the
/// five advanced fields unless `advancedEnabled` is set.
struct Params {
    quality: Quality,
    rotation: RotationPreset,
    colorspace: Colorspace,
    advanced_enabled: bool,
    brightness: f32,
    contrast: f32,
    blur: f32,
    noise: f32,
    resolution: i64,
    border: i64,
    rotate: i64,
    rotate_variance: i64,
    yellowish: bool,
}

impl Params {
    fn from_params(params: &[(String, String)]) -> Self {
        let quality = match param(params, "quality", "high") {
            "low" => Quality::Low,
            "medium" => Quality::Medium,
            _ => Quality::High,
        };
        let rotation = match param(params, "rotation", "slight") {
            "none" => RotationPreset::None,
            "moderate" => RotationPreset::Moderate,
            "severe" => RotationPreset::Severe,
            _ => RotationPreset::Slight,
        };
        let colorspace = match param(params, "colorspace", "grayscale") {
            "color" => Colorspace::Color,
            _ => Colorspace::Grayscale,
        };
        let mut p = Self {
            quality,
            rotation,
            colorspace,
            advanced_enabled: param_bool(params, "advancedEnabled"),
            brightness: param_f64(params, "brightness", 1.0) as f32,
            contrast: param_f64(params, "contrast", 1.0) as f32,
            blur: param_f64(params, "blur", 1.0) as f32,
            noise: param_f64(params, "noise", 8.0) as f32,
            resolution: param_f64(params, "resolution", 300.0) as i64,
            border: param_f64(params, "border", 20.0) as i64,
            rotate: param_f64(params, "rotate", 0.0) as i64,
            rotate_variance: param_f64(params, "rotateVariance", 2.0) as i64,
            yellowish: param_bool(params, "yellowish"),
        };
        if !p.advanced_enabled {
            match p.quality {
                Quality::High => {
                    p.blur = 0.10;
                    p.noise = 1.0;
                    p.brightness = 1.03;
                    p.contrast = 1.06;
                    p.resolution = 150;
                }
                Quality::Medium => {
                    p.blur = 0.10;
                    p.noise = 1.0;
                    p.brightness = 1.06;
                    p.contrast = 1.12;
                    p.resolution = 100;
                }
                Quality::Low => {
                    p.blur = 0.9;
                    p.noise = 2.5;
                    p.brightness = 1.08;
                    p.contrast = 1.15;
                    p.resolution = 75;
                }
            }
        }
        p
    }
}

/// `POST /api/v1/misc/scanner-effect`.
#[stirling_macros::endpoint]
fn scanner_effect(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    let p = Params::from_params(&params);
    if p.resolution > MAX_SAFE_DPI {
        return Err(AppError::BadRequest(format!(
            "DPI value {} exceeds maximum safe limit of {}. High DPI values can cause memory \
             issues and crashes. Please use a lower DPI value.",
            p.resolution, MAX_SAFE_DPI
        )));
    }
    let bytes: &[u8] = &upload.bytes;
    let rasterizer = BlankRasterizer::probe(&upload.name, bytes)
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("Ghostscript is required")))?;
    let source = pdfdoc::load(&upload.name, bytes)?;
    let page_ids = pdfdoc::page_ids(&source);
    if page_ids.is_empty() {
        return Err(AppError::BadRequest(
            "The provided PDF contains no pages to process.".into(),
        ));
    }

    let mut out = pdfdoc::new_document();
    for (index, &page_id) in page_ids.iter().enumerate() {
        let page = process_page(&rasterizer, &source, page_id, index as u32, &p)?;
        let image = PageImage::Raw {
            width: page.image.width(),
            height: page.image.height(),
            gray: page.gray,
            samples: page.image_samples(),
            smask: None,
        };
        // Java draws the processed scan over the original page box,
        // scaled to cover and centred — offsets may be negative.
        let content = format!(
            "q {} 0 0 {} {} {} cm /Im1 Do Q",
            page.draw_w, page.draw_h, page.offset_x, page.offset_y
        )
        .into_bytes();
        commit_page(&mut out, &image, page.orig_w, page.orig_h, content)?;
    }
    pdfdoc::touch_loaded_info(&mut out);
    let pdf = pdfdoc::save(&mut out)?;
    Ok(pdf_response(
        pdf,
        &output_name(Some(&upload.name), "_scanner_effect.pdf"),
    ))
}

struct ProcessedPage {
    image: RgbImage,
    gray: bool,
    orig_w: f32,
    orig_h: f32,
    offset_x: f32,
    offset_y: f32,
    draw_w: f32,
    draw_h: f32,
}

impl ProcessedPage {
    fn image_samples(&self) -> Vec<u8> {
        if self.gray {
            self.image
                .pixels()
                .map(|p| ((p.0[0] as u32 + p.0[1] as u32 + p.0[2] as u32) / 3) as u8)
                .collect()
        } else {
            self.image.as_raw().clone()
        }
    }
}

/// `ScannerEffectController.calculateSafeResolution`: shrink the render
/// DPI until the raster fits the 8192-side / 16 MP ceilings, floor 72.
fn safe_resolution(page_w_pts: f64, page_h_pts: f64, resolution: i64) -> i64 {
    let projected_w = (page_w_pts * resolution as f64 / 72.0).ceil() as i64;
    let projected_h = (page_h_pts * resolution as f64 / 72.0).ceil() as i64;
    let projected_pixels = projected_w.max(0) as u64 * projected_h.max(0) as u64;
    if projected_w <= MAX_IMAGE_SIDE as i64
        && projected_h <= MAX_IMAGE_SIDE as i64
        && projected_pixels <= MAX_IMAGE_PIXELS
    {
        return resolution;
    }
    let width_scale = MAX_IMAGE_SIDE as f64 / projected_w.max(1) as f64;
    let height_scale = MAX_IMAGE_SIDE as f64 / projected_h.max(1) as f64;
    let pixel_scale = (MAX_IMAGE_PIXELS as f64 / projected_pixels.max(1) as f64).sqrt();
    let min_scale = width_scale.min(height_scale).min(pixel_scale);
    (resolution as f64 * min_scale).max(72.0) as i64
}

fn process_page(
    rasterizer: &BlankRasterizer,
    doc: &Document,
    page_id: lopdf::ObjectId,
    index: u32,
    p: &Params,
) -> AppResult<ProcessedPage> {
    // Java reads the MediaBox (not CropBox) and rebuilds the page as
    // PDRectangle(w, h) — origin discarded.
    let mb = pdfdoc::media_box(doc, page_id)?;
    let page_w = mb[2] - mb[0];
    let page_h = mb[3] - mb[1];
    let dpi = safe_resolution(page_w, page_h, p.resolution);
    let rendered = rasterizer
        .render(index + 1, dpi as u32)
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("page {} render failed", index + 1)))?
        .to_rgb8();

    let mut image = to_colorspace(&rendered, p.colorspace);
    let gradient = random_gradient();
    let composed = add_border(&image, p.border.max(0) as u32, &gradient);
    image = composed;

    let base_rotation = p.rotation.value() + p.rotate;
    let rotation = if base_rotation == 0 && p.rotate_variance == 0 {
        0.0
    } else {
        base_rotation as f64 + (rand::random::<f64>() * 2.0 - 1.0) * p.rotate_variance as f64
    };
    let rotated = rotate_image(&image, rotation.to_radians(), &gradient);

    // Cover-fit the rotated scan onto the original page box, in points.
    let (rot_w, rot_h) = (rotated.width() as f64, rotated.height() as f64);
    let scale = (page_w / rot_w).max(page_h / rot_h);
    let draw_w = (rot_w * scale) as f32;
    let draw_h = (rot_h * scale) as f32;
    let offset_x = ((page_w - draw_w as f64) / 2.0) as f32;
    let offset_y = ((page_h - draw_h as f64) / 2.0) as f32;

    let feather = (rot_w.min(rot_h) * 0.02).round().max(10.0) as u32;
    let softened = soften_edges(&rotated, feather, &gradient);
    let blurred = gaussian_blur(&softened, p.blur);
    let adjusted = grade(&blurred, p.brightness, p.contrast, p.yellowish, p.noise);

    Ok(ProcessedPage {
        gray: p.colorspace == Colorspace::Grayscale,
        orig_w: page_w as f32,
        orig_h: page_h as f32,
        offset_x,
        offset_y,
        draw_w,
        draw_h,
        image: adjusted,
    })
}

fn to_colorspace(image: &RgbImage, colorspace: Colorspace) -> RgbImage {
    if colorspace == Colorspace::Color {
        return image.clone();
    }
    let mut out = RgbImage::new(image.width(), image.height());
    for (dst, src) in out.pixels_mut().zip(image.pixels()) {
        let g = ((src.0[0] as u32 + src.0[1] as u32 + src.0[2] as u32) / 3) as u8;
        *dst = image::Rgb([g, g, g]);
    }
    out
}

struct Gradient {
    vertical: bool,
    start: [u8; 3],
    end: [u8; 3],
}

fn random_gradient() -> Gradient {
    let channel = |g: f64| (g * 255.0).round() as u8;
    let start_grey = 0.6 + rand::random::<f64>() * 0.3;
    let end_grey = 0.6 + rand::random::<f64>() * 0.3;
    Gradient {
        vertical: rand::random::<bool>(),
        start: [channel(start_grey); 3],
        end: [channel(end_grey); 3],
    }
}

/// `createGradientLUT` + `fillWithGradient`: the border/rotation
/// background is a per-row (vertical) or per-column grey ramp.
fn gradient_lut(len: u32, gradient: &Gradient) -> Vec<[u8; 3]> {
    let size = len.max(1) as usize;
    (0..size)
        .map(|i| {
            let frac = i as f32 / (size - 1).max(1) as f32;
            let mut px = [0u8; 3];
            for (c, slot) in px.iter_mut().enumerate() {
                *slot = (gradient.start[c] as f32
                    + (gradient.end[c] as f32 - gradient.start[c] as f32) * frac)
                    .round() as u8;
            }
            px
        })
        .collect()
}

fn add_border(image: &RgbImage, border: u32, gradient: &Gradient) -> RgbImage {
    let (w, h) = (image.width() + 2 * border, image.height() + 2 * border);
    let lut = if gradient.vertical {
        gradient_lut(h, gradient)
    } else {
        gradient_lut(w, gradient)
    };
    let mut out = RgbImage::new(w, h);
    for (x, y, px) in out.enumerate_pixels_mut() {
        *px = image::Rgb(
            lut[if gradient.vertical {
                y as usize
            } else {
                x as usize
            }],
        );
    }
    image::imageops::overlay(&mut out, image, border as i64, border as i64);
    out
}

/// `rotateImage`: bounding-box canvas, gradient background, image spun
/// about its centre. Bilinear sampling stands in for Java's bicubic
/// `AffineTransform` — output is stochastic anyway.
fn rotate_image(image: &RgbImage, radians: f64, gradient: &Gradient) -> RgbImage {
    if radians == 0.0 {
        return image.clone();
    }
    let (w, h) = (image.width() as f64, image.height() as f64);
    let (sin, cos) = (radians.sin().abs(), radians.cos().abs());
    let rot_w = (w * cos + h * sin).floor().max(1.0) as u32;
    let rot_h = (h * cos + w * sin).floor().max(1.0) as u32;

    let lut = if gradient.vertical {
        gradient_lut(rot_h, gradient)
    } else {
        gradient_lut(rot_w, gradient)
    };
    let mut out = RgbImage::new(rot_w, rot_h);
    for y in 0..rot_h {
        for x in 0..rot_w {
            let bg = lut[if gradient.vertical {
                y as usize
            } else {
                x as usize
            }];
            out.put_pixel(x, y, image::Rgb(bg));
        }
    }

    // Inverse map: output pixel -> input pixel, rotation about the shared
    // centre after the (rotW-w)/2,(rotH-h)/2 translation.
    let cx_out = rot_w as f64 / 2.0;
    let cy_out = rot_h as f64 / 2.0;
    let cx_in = w / 2.0;
    let cy_in = h / 2.0;
    let (sin_r, cos_r) = radians.sin_cos();
    for y in 0..rot_h {
        for x in 0..rot_w {
            let dx = x as f64 + 0.5 - cx_out;
            let dy = y as f64 + 0.5 - cy_out;
            let sx = cos_r * dx + sin_r * dy + cx_in - 0.5;
            let sy = -sin_r * dx + cos_r * dy + cy_in - 0.5;
            let px = bilinear(image, sx, sy);
            if let Some(px) = px {
                out.put_pixel(x, y, px);
            }
        }
    }
    out
}

fn bilinear(image: &RgbImage, x: f64, y: f64) -> Option<image::Rgb<u8>> {
    if x < 0.0 || y < 0.0 || x > image.width() as f64 - 1.0 || y > image.height() as f64 - 1.0 {
        return None;
    }
    let x0 = x.floor() as u32;
    let y0 = y.floor() as u32;
    let x1 = (x0 + 1).min(image.width() - 1);
    let y1 = (y0 + 1).min(image.height() - 1);
    let (fx, fy) = (x - x0 as f64, y - y0 as f64);
    let p00 = image.get_pixel(x0, y0).0;
    let p10 = image.get_pixel(x1, y0).0;
    let p01 = image.get_pixel(x0, y1).0;
    let p11 = image.get_pixel(x1, y1).0;
    let mut out = [0u8; 3];
    for c in 0..3 {
        let top = p00[c] as f64 * (1.0 - fx) + p10[c] as f64 * fx;
        let bottom = p01[c] as f64 * (1.0 - fx) + p11[c] as f64 * fx;
        out[c] = (top * (1.0 - fy) + bottom * fy).round().clamp(0.0, 255.0) as u8;
    }
    Some(image::Rgb(out))
}

/// `softenEdges`: feather the border band toward the gradient
/// background by distance-to-edge.
fn soften_edges(image: &RgbImage, radius: u32, gradient: &Gradient) -> RgbImage {
    let (w, h) = (image.width(), image.height());
    let lut = if gradient.vertical {
        gradient_lut(h, gradient)
    } else {
        gradient_lut(w, gradient)
    };
    let mut out = RgbImage::new(w, h);
    for (x, y, px) in out.enumerate_pixels_mut() {
        let dx = x.min(w - 1 - x);
        let dy = y.min(h - 1 - y);
        let d = dx.min(dy);
        let bg = lut[if gradient.vertical {
            y as usize
        } else {
            x as usize
        }];
        let alpha = if d < radius {
            d as f32 / radius.max(1) as f32
        } else {
            1.0
        };
        let fg = image.get_pixel(x, y).0;
        let mut blended = [0u8; 3];
        for c in 0..3 {
            blended[c] = (fg[c] as f32 * alpha + bg[c] as f32 * (1.0 - alpha)).round() as u8;
        }
        *px = image::Rgb(blended);
    }
    out
}

/// `applyGaussianBlur`: two rounds of horizontal+vertical box blur,
/// approximating the Java gaussian the same way.
fn gaussian_blur(image: &RgbImage, sigma: f32) -> RgbImage {
    if sigma <= 0.0 {
        return image.clone();
    }
    let scaled = sigma * image.width().min(image.height()) as f32 / 1000.0;
    let radius = (scaled * 2.0).ceil().max(1.0) as i64;
    let mut cur = image.clone();
    for _ in 0..2 {
        cur = box_blur(&cur, radius, true);
        cur = box_blur(&cur, radius, false);
    }
    cur
}

fn box_blur(image: &RgbImage, radius: i64, horizontal: bool) -> RgbImage {
    let (w, h) = (image.width() as i64, image.height() as i64);
    let diameter = (radius * 2 + 1) as f64;
    let mut out = RgbImage::new(w as u32, h as u32);
    if horizontal {
        for y in 0..h {
            let mut sums = [0i64; 3];
            for x in -radius..=radius {
                let px = image.get_pixel(x.clamp(0, w - 1) as u32, y as u32).0;
                for c in 0..3 {
                    sums[c] += px[c] as i64;
                }
            }
            for x in 0..w {
                let px = &mut out.get_pixel_mut(x as u32, y as u32).0;
                for c in 0..3 {
                    px[c] = (sums[c] as f64 / diameter) as u8;
                }
                let left = image
                    .get_pixel((x - radius).clamp(0, w - 1) as u32, y as u32)
                    .0;
                let right = image
                    .get_pixel((x + radius + 1).clamp(0, w - 1) as u32, y as u32)
                    .0;
                for c in 0..3 {
                    sums[c] += right[c] as i64 - left[c] as i64;
                }
            }
        }
    } else {
        for x in 0..w {
            let mut sums = [0i64; 3];
            for y in -radius..=radius {
                let px = image.get_pixel(x as u32, y.clamp(0, h - 1) as u32).0;
                for c in 0..3 {
                    sums[c] += px[c] as i64;
                }
            }
            for y in 0..h {
                let px = &mut out.get_pixel_mut(x as u32, y as u32).0;
                for c in 0..3 {
                    px[c] = (sums[c] as f64 / diameter) as u8;
                }
                let top = image
                    .get_pixel(x as u32, (y - radius).clamp(0, h - 1) as u32)
                    .0;
                let bottom = image
                    .get_pixel(x as u32, (y + radius + 1).clamp(0, h - 1) as u32)
                    .0;
                for c in 0..3 {
                    sums[c] += bottom[c] as i64 - top[c] as i64;
                }
            }
        }
    }
    out
}

/// `applyAllEffectsSinglePass`: contrast/brightness grade, optional
/// yellowing, then gaussian sensor noise.
fn grade(
    image: &RgbImage,
    brightness: f32,
    contrast: f32,
    yellowish: bool,
    noise: f32,
) -> RgbImage {
    let (w, h) = (image.width(), image.height());
    let mut out = RgbImage::new(w, h);
    let scaled_strength = noise * w.min(h) as f32 / 1000.0;
    let apply_noise = scaled_strength > 0.0;
    let contrast_offset = 128.0 - 128.0 * contrast;
    let inv_765 = 1.0 / 765.0;
    for (dst, src) in out.pixels_mut().zip(image.pixels()) {
        // Java's `(int)` casts truncate toward zero before the clamp.
        let mut px = [0i32; 3];
        for (c, slot) in px.iter_mut().enumerate() {
            *slot = (((src.0[c] as f32 * contrast + contrast_offset) * brightness) as i32)
                .clamp(0, 255);
        }
        if yellowish {
            let bright = (px[0] + px[1] + px[2]) as f32 * inv_765;
            px[0] = (px[0] + ((255 - px[0]) as f32 * 0.18 * bright) as i32).min(255);
            px[1] = (px[1] + ((255 - px[1]) as f32 * 0.12 * bright) as i32).min(255);
            px[2] = (px[2] * (1.0 - 0.25 * bright) as i32).max(0);
        }
        if apply_noise {
            for v in px.iter_mut() {
                *v = (*v + (gaussian() * scaled_strength) as i32).clamp(0, 255);
            }
        }
        *dst = image::Rgb([px[0] as u8, px[1] as u8, px[2] as u8]);
    }
    out
}

/// Box-Muller standard normal — `ThreadLocalRandom.nextGaussian`.
fn gaussian() -> f32 {
    let u1: f64 = rand::random::<f64>().max(f64::MIN_POSITIVE);
    let u2: f64 = rand::random();
    ((-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_resolution_shrinks_oversized_pages() {
        // Letter at 300 DPI: 2550x3300 px = 8.4 MP — under the pixel cap.
        assert_eq!(safe_resolution(612.0, 792.0, 300), 300);
        // A0 poster at 300 DPI blows past both caps and gets scaled down.
        let dpi = safe_resolution(2384.0, 3370.0, 300);
        assert!((72..300).contains(&dpi));
        // Exactly at the pixel cap stays.
        assert_eq!(safe_resolution(4096.0, 4096.0, 72), 72);
    }

    #[test]
    fn grade_clamps_and_preserves_neutral() {
        let img = RgbImage::from_pixel(2, 2, image::Rgb([128, 128, 128]));
        let out = grade(&img, 1.0, 1.0, false, 0.0);
        // contrast=1 → offset 0; 128*1*1 = 128.
        assert_eq!(out.get_pixel(0, 0).0, [128, 128, 128]);
    }
}
