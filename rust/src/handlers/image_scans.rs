//! `/api/v1/misc/extract-image-scans` — `ExtractImageScansController`:
//! rasterise each PDF page (or take the uploaded image as-is), then run
//! the native port of `split_photos.py` (`image_split`) on every image
//! and return the detected photo crops — one PNG, or a zip when several
//! are found. No Python/OpenCV dependency.

use std::collections::HashMap;

use axum::response::Response;
use image::{DynamicImage, RgbImage};

use crate::error::{AppError, AppResult};
use crate::handlers::jobs::AppState;
use crate::handlers::merge::file_response;
use crate::handlers::misc::BlankRasterizer;
use crate::handlers::split::zip_response;
use crate::image_split::{self, SplitParams, SplitResult, MAX_IMAGE_PIXELS};
use crate::pdfdoc;
use crate::upload::{output_name, param, Upload, FILE_FIELD};

/// Java's `SCAN_RENDER_DPI`; `ExtractImageScansController` renders at
/// `Math.min(SCAN_RENDER_DPI, system.maxDPI)`.
const SCAN_RENDER_DPI: u32 = 300;

/// Each gs render is a subprocess (~100+ MB peak); each split holds
/// per-pixel buffers. Batches bound both the process and memory fan-out.
const RENDER_WORKERS: usize = 8;
const SPLIT_WORKERS: usize = 4;

/// Run `f` over `items` on at most `workers` threads per batch,
/// preserving item order; a panicking task yields `None` for its slot.
fn run_bounded<T, R, F>(items: &[T], workers: usize, f: F) -> Vec<Option<R>>
where
    F: Fn(&T) -> R + Sync,
    T: Sync,
    R: Send,
{
    let mut out = Vec::with_capacity(items.len());
    for chunk in items.chunks(workers.max(1)) {
        let results: Vec<Option<F::Output>> = std::thread::scope(|scope| {
            chunk
                .iter()
                .map(|item| scope.spawn(|| f(item)))
                .collect::<Vec<_>>()
                .into_iter()
                .map(|h| h.join().ok())
                .collect()
        });
        out.extend(results);
    }
    out
}

/// `Math.min(SCAN_RENDER_DPI, system.maxDPI)` with Java's 500 default
/// already in the settings field; clamped positive because a `maxDPI`
/// of 0 would ask Ghostscript for a 0-DPI render.
fn scan_dpi() -> u32 {
    let max = crate::settings::current()
        .system
        .max_dpi
        .clamp(1, i64::from(u32::MAX)) as u32;
    SCAN_RENDER_DPI.min(max)
}

/// `ExtractImageScansRequest` field defaults. `minArea` and
/// `minContourArea` are accepted for API compatibility but ignored —
/// `split_photos.py` never forwards them to `find_photo_boundaries`, so
/// the script's own defaults (10000/500) always apply there and here.
struct Params {
    angle_threshold: i32,
    tolerance: i32,
    border_size: i32,
}

impl Params {
    fn from_params(params: &[(String, String)]) -> Self {
        // Parse every documented field so unknown/malformed values fall
        // back per-field exactly like Spring's int binding does.
        let _ = param(params, "minArea", "");
        let _ = param(params, "minContourArea", "");
        let int =
            |name: &str, fallback: i32| param(params, name, "").trim().parse().unwrap_or(fallback);
        Self {
            angle_threshold: int("angleThreshold", 10),
            tolerance: int("tolerance", 30),
            border_size: int("borderSize", 1),
        }
    }

    fn split(&self) -> SplitParams {
        SplitParams {
            tolerance: self.tolerance,
            angle_threshold: self.angle_threshold,
            border_size: self.border_size,
        }
    }
}

/// `POST /api/v1/misc/extract-image-scans`.
#[stirling_macros::endpoint]
fn extract_image_scans(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    let p = Params::from_params(&params);
    let base = upload.name.clone();

    let images = input_images(&upload)?;
    if images.is_empty() {
        return Err(AppError::BadRequest("No images detected".into()));
    }

    let p = p.split();
    // Splitting is pure CPU on disjoint images — fan out per page in
    // bounded batches so a many-page PDF cannot spawn unbounded threads,
    // and flatten in page order so zip entries keep Java's ordering.
    let per_image: Vec<Vec<SplitResult>> = run_bounded(&images, SPLIT_WORKERS, |image| {
        image_split::split_photos(image, "scan", p)
    })
    .into_iter()
    .map(|r| r.unwrap_or_default())
    .collect();
    let crops: Vec<Vec<u8>> = per_image
        .into_iter()
        .flatten()
        .map(|result| result.png)
        .collect();

    match crops.len() {
        0 => Err(AppError::BadRequest("No images detected".into())),
        1 => Ok(file_response(
            crops.into_iter().next().unwrap(),
            &output_name(Some(&base), ".png"),
            "image/png",
        )),
        _ => {
            let entries = crops
                .into_iter()
                .enumerate()
                .map(|(i, bytes)| {
                    (
                        output_name(Some(&base), &format!("_processed_{}.png", i + 1)),
                        bytes,
                    )
                })
                .collect();
            zip_response(entries, &output_name(Some(&base), "_processed.zip"))
        }
    }
}

/// Every image the splitter should see: PDF pages rendered at scan DPI,
/// or the uploaded file decoded directly when it is not a PDF.
fn input_images(upload: &Upload) -> AppResult<Vec<RgbImage>> {
    let is_pdf = upload
        .name
        .rsplit_once('.')
        .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("pdf"));
    if !is_pdf {
        let dynamic = image::load_from_memory(&upload.bytes).map_err(|_| {
            AppError::BadRequest("The uploaded file is not a supported image format".into())
        })?;
        check_pixel_budget(dynamic.width(), dynamic.height())?;
        return Ok(vec![dynamic.to_rgb8()]);
    }

    let doc = pdfdoc::load(&upload.name, &upload.bytes)?;
    let page_count = pdfdoc::page_ids(&doc).len();
    if page_count == 0 {
        return Ok(Vec::new());
    }
    let rasterizer = BlankRasterizer::probe(&upload.bytes)
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("Ghostscript is required")))?;
    let rasterizer = &rasterizer;
    let dpi = scan_dpi();
    // Each gs run writes its own `page-{n}-{dpi}.png`; bounded batches
    // keep a many-page PDF from forking unbounded subprocesses, and
    // joining in spawn order keeps page order.
    let pages: Vec<u32> = (1..=page_count as u32).collect();
    let rendered: Vec<Option<DynamicImage>> =
        run_bounded(&pages, RENDER_WORKERS, |&page| rasterizer.render(page, dpi))
            .into_iter()
            .map(|r| r.flatten())
            .collect();
    rendered
        .into_iter()
        .enumerate()
        .map(|(i, image)| {
            let image = image
                .ok_or_else(|| AppError::Pdf(format!("ghostscript failed on page {}", i + 1)))?;
            check_pixel_budget(image.width(), image.height())?;
            Ok(image_to_rgb(&image))
        })
        .collect()
}

/// Reject images whose decode would exceed the splitter's pixel budget —
/// checked before the multi-buffer pipeline allocates per-pixel scratch.
fn check_pixel_budget(width: u32, height: u32) -> AppResult<()> {
    if width as u64 * height as u64 > MAX_IMAGE_PIXELS {
        return Err(AppError::BadRequest(
            "image exceeds the maximum supported size".into(),
        ));
    }
    Ok(())
}

fn image_to_rgb(image: &DynamicImage) -> RgbImage {
    image.to_rgb8()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};

    fn upload(name: &str, bytes: &[u8]) -> Upload {
        Upload {
            name: name.to_string(),
            bytes: crate::upload::UploadBytes::from_vec(bytes.to_vec()),
            content_type: None,
        }
    }

    #[test]
    fn params_use_java_defaults() {
        let p = Params::from_params(&[]);
        assert_eq!(p.angle_threshold, 10);
        assert_eq!(p.tolerance, 30);
        assert_eq!(p.border_size, 1);
    }

    #[test]
    fn params_parse_overrides() {
        let params = vec![
            ("angleThreshold".to_string(), "25".to_string()),
            ("borderSize".to_string(), "3".to_string()),
            ("minArea".to_string(), "1".to_string()),
        ];
        let p = Params::from_params(&params);
        assert_eq!(p.angle_threshold, 25);
        assert_eq!(p.border_size, 3);
        assert_eq!(p.tolerance, 30);
    }

    #[test]
    fn invalid_image_is_rejected() {
        let up = upload("scan.png", b"not an image");
        let err = input_images(&up).unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn run_bounded_preserves_order_and_swallows_panics() {
        let items: Vec<u32> = (0..10).collect();
        let out = run_bounded(&items, 3, |&n| {
            if n == 4 {
                panic!("boom");
            }
            n * 2
        });
        assert_eq!(out.len(), 10);
        for (i, slot) in out.iter().enumerate() {
            if i == 4 {
                assert!(slot.is_none());
            } else {
                assert_eq!(*slot, Some(i as u32 * 2));
            }
        }
    }

    #[test]
    fn pixel_budget_rejects_oversized() {
        assert!(check_pixel_budget(10_000, 10_000).is_ok()); // exactly 100 MP
        assert!(check_pixel_budget(10_001, 10_000).is_err());
    }

    #[test]
    fn png_input_decodes_to_rgb() {
        let mut img = RgbImage::new(4, 4);
        img.put_pixel(0, 0, Rgb([1, 2, 3]));
        let mut buf = std::io::Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        let up = upload("scan.png", &buf.into_inner());
        let images = input_images(&up).unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].dimensions(), (4, 4));
    }

    /// White scan with two dark photo rectangles — each must be found as
    /// a separate crop, in reading order. Rectangles clear the script's
    /// fixed `min_area` (10000 px²) after the 5×5 dilation.
    #[test]
    fn splits_multiple_photos_in_order() {
        let mut img = RgbImage::from_pixel(400, 260, Rgb([255, 255, 255]));
        for x in 20..140 {
            for y in 20..160 {
                img.put_pixel(x, y, Rgb([30, 30, 30]));
            }
        }
        for x in 240..360 {
            for y in 100..240 {
                img.put_pixel(x, y, Rgb([30, 30, 30]));
            }
        }
        let results = image_split::split_photos(
            &img,
            "scan",
            SplitParams {
                tolerance: 30,
                angle_threshold: 10,
                border_size: 1,
            },
        );
        assert_eq!(results.len(), 2, "expected two crops: {results:?}");
        assert!(results[0].name.ends_with("_1.png"));
        assert!(results[1].name.ends_with("_2.png"));
        // Decode to prove the payloads are real PNGs.
        for r in &results {
            assert!(image::load_from_memory(&r.png).is_ok());
        }
    }

    #[test]
    fn uniform_image_yields_no_crops() {
        let img = RgbImage::from_pixel(200, 200, Rgb([200, 200, 200]));
        let results = image_split::split_photos(
            &img,
            "scan",
            SplitParams {
                tolerance: 30,
                angle_threshold: 10,
                border_size: 1,
            },
        );
        assert!(results.is_empty());
    }

    #[test]
    fn small_specks_below_min_area_are_ignored() {
        let mut img = RgbImage::from_pixel(300, 300, Rgb([255, 255, 255]));
        for x in 50..70 {
            for y in 50..70 {
                img.put_pixel(x, y, Rgb([0, 0, 0]));
            }
        }
        let results = image_split::split_photos(
            &img,
            "scan",
            SplitParams {
                tolerance: 30,
                angle_threshold: 10,
                border_size: 0,
            },
        );
        assert!(results.is_empty());
    }
}
