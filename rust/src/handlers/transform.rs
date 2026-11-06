//! Page-geometry transforms backed by content wrapping: scale and manual crop
//! adjust the page box and wrap the existing content stream in a
//! save/clip/restore prefix rather than authoring a Form XObject.

use std::collections::{HashMap, HashSet};

use axum::response::Response;
use lopdf::Object;

use crate::error::{AppError, AppResult};
use crate::handlers::jobs::AppState;
use crate::handlers::merge::pdf_response;
use crate::page_select;
use crate::pdfdoc;
use crate::upload::{output_name, param, param_bool, Upload, FILE_FIELD};

fn float_param(params: &[(String, String)], name: &str) -> AppResult<Option<f64>> {
    let raw = param(params, name, "");
    if raw.trim().is_empty() {
        return Ok(None);
    }
    raw.trim()
        .parse::<f64>()
        .map(Some)
        .map_err(|_| AppError::BadRequest(format!("invalid {name}: {raw}")))
}

fn page_size(params: &[(String, String)]) -> AppResult<(f64, f64)> {
    let target = param(params, "pageSize", "KEEP");
    let (mut width, mut height) = match target {
        "KEEP" => return Ok((0.0, 0.0)),
        "A0" => (2383.937, 3370.394),
        "A1" => (1683.78, 2383.937),
        "A2" => (1190.551, 1683.78),
        "A3" => (841.89, 1190.551),
        "A4" => (595.276, 841.89),
        "A5" => (419.528, 595.276),
        "A6" => (297.638, 419.528),
        "LETTER" => (612.0, 792.0),
        "LEGAL" => (612.0, 1008.0),
        _ => {
            return Err(AppError::BadRequest(format!(
                "Invalid page size format: {target}"
            )))
        }
    };
    if param(params, "orientation", "PORTRAIT").eq_ignore_ascii_case("LANDSCAPE") {
        std::mem::swap(&mut width, &mut height);
    }
    Ok((width, height))
}

/// `POST /api/v1/general/scale-pages` — fits each page into `pageSize`
/// (or keeps its own size) centred, scaled by `scaleFactor`.
///
/// Mirrors `ScalePagesController`: the target is a fresh page, so the
/// output has no `/Rotate` and no `/CropBox`, and the source content is
/// clipped to its view box exactly like PDFBox's form XObject BBox.
#[stirling_macros::endpoint]
fn scale_pages(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    let scale_factor = float_param(&params, "scaleFactor")?.unwrap_or(1.0);
    let page_size_param = param(&params, "pageSize", "KEEP");
    // Java's keepsPageSize treats null, blank and "KEEP" alike.
    let keep_size = page_size_param.trim().is_empty() || page_size_param == "KEEP";
    let (fixed_width, fixed_height) = if keep_size {
        (0.0, 0.0)
    } else {
        page_size(&params)?
    };

    let mut document = pdfdoc::load(&upload.name, &upload.bytes)?;
    let page_ids = pdfdoc::page_ids(&document);
    if page_ids.is_empty() {
        return Err(AppError::BadRequest("PDF has no pages".into()));
    }

    for page_id in page_ids {
        let boxes = pdfdoc::page_boxes(&document, page_id)?;
        let media = [
            f64::from(boxes.media[0]),
            f64::from(boxes.media[1]),
            f64::from(boxes.media[2]),
            f64::from(boxes.media[3]),
        ];
        let view = [
            f64::from(boxes.crop[0]),
            f64::from(boxes.crop[1]),
            f64::from(boxes.crop[2]),
            f64::from(boxes.crop[3]),
        ];
        let source_width = (media[2] - media[0]).abs();
        let source_height = (media[3] - media[1]).abs();
        if source_width <= 0.0 || source_height <= 0.0 {
            return Err(AppError::BadRequest(
                "page has a zero-sized MediaBox".into(),
            ));
        }
        // KEEP resizes each page to its own MediaBox, so mixed-size
        // documents stay mixed, like the Java controller.
        let (target_width, target_height) = if keep_size {
            (source_width, source_height)
        } else {
            (fixed_width, fixed_height)
        };
        let scale = (target_width / source_width).min(target_height / source_height) * scale_factor;
        let x = (target_width - source_width * scale) / 2.0;
        let y = (target_height - source_height * scale) / 2.0;

        let view_width = view[2] - view[0];
        let view_height = view[3] - view[1];
        if view_width <= 0.0 || view_height <= 0.0 {
            return Err(AppError::BadRequest(
                "page has a zero-sized CropBox or MediaBox".into(),
            ));
        }
        let rotation = pdfdoc::page_rotation(&document, page_id)?;
        let form = pdfdoc::form_matrix(media, view, rotation);
        // Source user space → target page space: the controller's
        // translate-then-scale content stream, then the form matrix.
        let mut at = pdfdoc::Affine::identity();
        at.translate(x, y);
        at.scale(scale, scale);
        at.concat(&form);
        let matrix = at.pdf_cm();

        // The form's BBox clips to the view box; mapped through `matrix`
        // that is the bounding box of the transformed corners (90°
        // rotations keep it axis-aligned).
        let (mut x0, mut y0, mut x1, mut y1) = (
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        );
        for (cx, cy) in [
            (view[0], view[1]),
            (view[2], view[1]),
            (view[0], view[3]),
            (view[2], view[3]),
        ] {
            let px = matrix[0] * cx + matrix[2] * cy + matrix[4];
            let py = matrix[1] * cx + matrix[3] * cy + matrix[5];
            x0 = x0.min(px);
            y0 = y0.min(py);
            x1 = x1.max(px);
            y1 = y1.max(py);
        }
        let prefix = format!(
            "q\n{} {} {} {} re\nW\nn\n{} {} {} {} {} {} cm\n",
            pdfdoc::pdf_num(x0),
            pdfdoc::pdf_num(y0),
            pdfdoc::pdf_num(x1 - x0),
            pdfdoc::pdf_num(y1 - y0),
            pdfdoc::pdf_num(matrix[0]),
            pdfdoc::pdf_num(matrix[1]),
            pdfdoc::pdf_num(matrix[2]),
            pdfdoc::pdf_num(matrix[3]),
            pdfdoc::pdf_num(matrix[4]),
            pdfdoc::pdf_num(matrix[5]),
        );
        pdfdoc::wrap_page_content(&mut document, page_id, &prefix)?;
        pdfdoc::set_page_box(
            &mut document,
            page_id,
            [0.0, 0.0, target_width, target_height],
        )?;
        // Java builds a fresh PDPage: no /Rotate, and /CropBox falls back
        // to the new MediaBox. Setting both explicitly also overrides any
        // value inherited from the page tree, which removing the keys
        // would not.
        let page = document
            .get_dictionary_mut(page_id)
            .map_err(|e| AppError::Pdf(e.to_string()))?;
        page.set(
            "CropBox",
            Object::Array(
                [0.0, 0.0, target_width, target_height]
                    .iter()
                    .map(|v| Object::Real(*v as f32))
                    .collect(),
            ),
        );
        page.set("Rotate", Object::Integer(0));
        pdfdoc::scale_annots(&mut document, page_id, matrix)?;
    }
    pdfdoc::restamp_info(&mut document);

    let bytes = pdfdoc::save(&mut document)?;
    Ok(pdf_response(
        bytes,
        &output_name(Some(&upload.name), "_scaled.pdf"),
    ))
}

/// `POST /api/v1/general/crop` — clips the selected pages to `x/y/width/height`
/// in PDF user space. `autoCrop` is rejected: it needs rasterisation.
#[stirling_macros::endpoint]
fn crop_pdf(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    if param_bool(&params, "autoCrop") {
        return Err(AppError::BadRequest(
            "auto-crop is not supported by the Rust backend".into(),
        ));
    }
    let remove_data = params
        .iter()
        .rev()
        .find(|(name, _)| name == "removeDataOutsideCrop")
        .map(|(_, value)| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "true" | "1" | "on" | "yes"
            )
        })
        .unwrap_or(true);
    if remove_data {
        return Err(AppError::BadRequest(
            "removeDataOutsideCrop=true is not supported by the Rust backend; crop is clip-only"
                .into(),
        ));
    }
    let x = float_param(&params, "x")?.ok_or_else(|| {
        AppError::BadRequest("crop coordinates (x, y, width, height) are required".into())
    })?;
    let y = float_param(&params, "y")?.ok_or_else(|| {
        AppError::BadRequest("crop coordinates (x, y, width, height) are required".into())
    })?;
    let width = float_param(&params, "width")?.ok_or_else(|| {
        AppError::BadRequest("crop coordinates (x, y, width, height) are required".into())
    })?;
    let height = float_param(&params, "height")?.ok_or_else(|| {
        AppError::BadRequest("crop coordinates (x, y, width, height) are required".into())
    })?;
    if width <= 0.0 || height <= 0.0 {
        return Err(AppError::BadRequest("invalid crop size".into()));
    }

    let page_numbers = param(&params, "pageNumbers", "all").to_string();
    let mut document = pdfdoc::load(&upload.name, &upload.bytes)?;
    let page_ids = pdfdoc::page_ids(&document);
    let total_pages = page_ids.len();
    page_select::require_valid_page_selection(&page_numbers, total_pages)
        .map_err(AppError::BadRequest)?;
    let selected: HashSet<usize> = page_select::parse_page_list(&page_numbers, total_pages)
        .map_err(AppError::BadRequest)?
        .into_iter()
        .collect();

    for (index, page_id) in page_ids.into_iter().enumerate() {
        if !selected.contains(&index) {
            continue;
        }
        pdfdoc::wrap_page_content(
            &mut document,
            page_id,
            &pdfdoc::crop_prefix(x, y, width, height),
        )?;
        pdfdoc::set_page_box(&mut document, page_id, [x, y, x + width, y + height])?;
        pdfdoc::clip_annots(&mut document, page_id, [x, y, x + width, y + height])?;
    }
    pdfdoc::restamp_info(&mut document);

    let bytes = pdfdoc::save(&mut document)?;
    Ok(pdf_response(
        bytes,
        &output_name(Some(&upload.name), "_cropped.pdf"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit;
    use lopdf::{dictionary, Document, Object, ObjectId};

    fn upload(bytes: Vec<u8>) -> Vec<Upload> {
        vec![Upload {
            name: "in.pdf".into(),
            bytes: crate::upload::UploadBytes::from_vec(bytes),
            content_type: None,
        }]
    }

    fn annot(subtype: &str, rect: [f64; 4]) -> Object {
        Object::Dictionary(dictionary! {
            "Type" => "Annot",
            "Subtype" => subtype,
            "Rect" => Object::Array(rect.iter().map(|v| Object::Real(*v as f32)).collect()),
        })
    }

    fn pdf_with_annots(rects: &[(&str, [f64; 4])]) -> (testkit::Pdf, Vec<ObjectId>) {
        let mut pdf = testkit::Pdf::new();
        let mut ids = Vec::new();
        for (subtype, rect) in rects {
            ids.push(pdf.add(annot(subtype, *rect)));
        }
        pdf.edit_page(|page| {
            page.set(
                "Annots",
                Object::Array(ids.iter().map(|id| Object::Reference(*id)).collect()),
            );
        });
        (pdf, ids)
    }

    fn rect_of(doc: &Document, item: &Object) -> Option<[f64; 4]> {
        let dict = pdfdoc::peek_dict(doc, item)?;
        let rect = dict.get(b"Rect").ok()?;
        let (_, resolved) = doc.dereference(rect).ok()?;
        let Object::Array(nums) = resolved else {
            return None;
        };
        let mut out = [0.0; 4];
        for (index, num) in nums.iter().take(4).enumerate() {
            out[index] = match num {
                Object::Integer(v) => *v as f64,
                Object::Real(v) => *v as f64,
                _ => return None,
            };
        }
        Some(out)
    }

    fn page_annots(doc: &Document) -> Vec<(String, Option<[f64; 4]>)> {
        let page_id = doc.get_pages().into_values().next().unwrap();
        let page = doc.get_dictionary(page_id).unwrap();
        let Some(annots) = page.get(b"Annots").ok() else {
            return Vec::new();
        };
        let (_, resolved) = doc.dereference(annots).unwrap();
        let Object::Array(items) = resolved else {
            return Vec::new();
        };
        items
            .iter()
            .filter_map(|item| {
                let dict = pdfdoc::peek_dict(doc, item)?;
                let subtype = dict
                    .get(b"Subtype")
                    .ok()?
                    .as_name()
                    .ok()
                    .map(|n| String::from_utf8_lossy(n).into_owned())?;
                Some((subtype, rect_of(doc, item)))
            })
            .collect()
    }

    #[test]
    fn crop_rejects_remove_data_outside_crop() {
        let (mut pdf, _) = pdf_with_annots(&[]);
        let bytes = pdf.bytes();
        let params = vec![
            ("x".to_string(), "0".to_string()),
            ("y".to_string(), "0".to_string()),
            ("width".to_string(), "100".to_string()),
            ("height".to_string(), "100".to_string()),
            ("autoCrop".to_string(), "false".to_string()),
            ("removeDataOutsideCrop".to_string(), "true".to_string()),
        ];
        let error =
            crop_pdf_work(AppState::new(), HashMap::new(), upload(bytes), params).unwrap_err();
        assert!(error.to_string().contains("removeDataOutsideCrop"));
    }

    #[test]
    fn crop_rejects_auto_crop() {
        let (mut pdf, _) = pdf_with_annots(&[]);
        let bytes = pdf.bytes();
        let params = vec![
            ("x".to_string(), "0".to_string()),
            ("y".to_string(), "0".to_string()),
            ("width".to_string(), "100".to_string()),
            ("height".to_string(), "100".to_string()),
            ("autoCrop".to_string(), "true".to_string()),
            ("removeDataOutsideCrop".to_string(), "false".to_string()),
        ];
        let error =
            crop_pdf_work(AppState::new(), HashMap::new(), upload(bytes), params).unwrap_err();
        assert!(error.to_string().contains("auto-crop"));
    }

    #[tokio::test]
    async fn scale_keeps_link_and_widget_annotations_with_transformed_rects() {
        let (mut pdf, _) = pdf_with_annots(&[
            ("Link", [100.0, 100.0, 200.0, 200.0]),
            ("Widget", [400.0, 400.0, 500.0, 500.0]),
            ("Text", [300.0, 300.0, 320.0, 320.0]),
        ]);
        let bytes = pdf.bytes();
        let params = vec![
            ("pageSize".to_string(), "A4".to_string()),
            ("scaleFactor".to_string(), "1".to_string()),
        ];
        let response =
            scale_pages_work(AppState::new(), HashMap::new(), upload(bytes), params).unwrap();
        let doc = testkit::response_document(response).await;
        let annots = page_annots(&doc);
        assert_eq!(annots.len(), 2);
        assert!(annots.iter().any(|(kind, _)| kind == "Link"));
        assert!(annots.iter().any(|(kind, _)| kind == "Widget"));

        let scale = f64::min(595.276_f64 / 612.0, 841.89_f64 / 792.0);
        let x = (595.276 - 612.0 * scale) / 2.0;
        let y = (841.89 - 792.0 * scale) / 2.0;
        let link = annots
            .iter()
            .find(|(kind, _)| kind == "Link")
            .and_then(|(_, rect)| *rect)
            .unwrap();
        assert!((link[0] - (scale * 100.0 + x)).abs() < 0.5);
        assert!((link[1] - (scale * 100.0 + y)).abs() < 0.5);
        assert!((link[2] - (scale * 200.0 + x)).abs() < 0.5);
        assert!((link[3] - (scale * 200.0 + y)).abs() < 0.5);
    }

    #[tokio::test]
    async fn crop_drops_annotations_outside_the_crop_box() {
        let (mut pdf, _) = pdf_with_annots(&[
            ("Link", [150.0, 150.0, 250.0, 250.0]),
            ("Link", [500.0, 500.0, 560.0, 560.0]),
            ("Text", [120.0, 120.0, 130.0, 130.0]),
        ]);
        let bytes = pdf.bytes();
        let params = vec![
            ("x".to_string(), "100".to_string()),
            ("y".to_string(), "100".to_string()),
            ("width".to_string(), "200".to_string()),
            ("height".to_string(), "200".to_string()),
            ("pageNumbers".to_string(), "all".to_string()),
            ("autoCrop".to_string(), "false".to_string()),
            ("removeDataOutsideCrop".to_string(), "false".to_string()),
        ];
        let response =
            crop_pdf_work(AppState::new(), HashMap::new(), upload(bytes), params).unwrap();
        let doc = testkit::response_document(response).await;
        let annots = page_annots(&doc);
        assert_eq!(annots.len(), 1);
        assert_eq!(annots[0].0, "Link");
        assert_eq!(annots[0].1, Some([150.0, 150.0, 250.0, 250.0]));
    }

    fn page_ids(doc: &Document) -> Vec<ObjectId> {
        doc.get_pages().into_values().collect()
    }

    fn box_of(doc: &Document, page_id: ObjectId, key: &[u8]) -> Option<[f64; 4]> {
        let page = doc.get_dictionary(page_id).ok()?;
        let object = page.get(key).ok()?;
        let (_, resolved) = doc.dereference(object).ok()?;
        let Object::Array(items) = resolved else {
            return None;
        };
        let mut out = [0.0; 4];
        for (index, item) in items.iter().take(4).enumerate() {
            out[index] = match item {
                Object::Integer(v) => *v as f64,
                Object::Real(v) => *v as f64,
                _ => return None,
            };
        }
        Some(out)
    }

    fn page_media(doc: &Document, index: usize) -> [f64; 4] {
        box_of(doc, page_ids(doc)[index], b"MediaBox").unwrap()
    }

    fn page_rotate(doc: &Document, index: usize) -> i64 {
        let page = doc.get_dictionary(page_ids(doc)[index]).unwrap();
        page.get(b"Rotate")
            .ok()
            .and_then(|o| o.as_i64().ok())
            .unwrap_or(0)
    }

    fn page_stream(doc: &Document, index: usize) -> String {
        let bytes = doc.get_page_content(page_ids(doc)[index]);
        String::from_utf8_lossy(&bytes).into_owned()
    }

    async fn scale(doc_bytes: Vec<u8>, params: Vec<(&str, &str)>) -> Document {
        let params = params
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let response =
            scale_pages_work(AppState::new(), HashMap::new(), upload(doc_bytes), params).unwrap();
        testkit::response_document(response).await
    }

    #[tokio::test]
    async fn scale_letter_to_a4_centres_content_on_a4_page() {
        let mut pdf = testkit::Pdf::new();
        let stream = pdf.add_stream(dictionary! {}, b"0 0 612 792 re f");
        pdf.edit_page(|page| {
            page.set("Contents", Object::Reference(stream));
        });
        let doc = scale(pdf.bytes(), vec![("pageSize", "A4")]).await;
        assert_eq!(page_ids(&doc).len(), 1);
        let media = page_media(&doc, 0);
        assert!((media[2] - 595.276).abs() < 0.01);
        assert!((media[3] - 841.89).abs() < 0.01);
        assert_eq!(media[0], 0.0);
        assert_eq!(media[1], 0.0);

        let scale = f64::min(595.276_f64 / 612.0, 841.89_f64 / 792.0);
        let x = (595.276 - 612.0 * scale) / 2.0;
        let y = (841.89 - 792.0 * scale) / 2.0;
        let stream = page_stream(&doc, 0);
        // The clip mirrors the form BBox and the cm carries translate·scale.
        assert!(stream.contains(" re\nW\nn\n"));
        assert!(stream.contains(&format!(
            "{} {} {} {} {} {} cm",
            scale, 0.0, 0.0, scale, x, y
        )));
    }

    #[tokio::test]
    async fn scale_keep_preserves_mixed_page_sizes() {
        let mut pdf = testkit::Pdf::new();
        pdf.add_page([0, 0, 300, 400], dictionary! {});
        let doc = scale(pdf.bytes(), vec![("pageSize", "KEEP")]).await;
        assert_eq!(page_ids(&doc).len(), 2);
        let first = page_media(&doc, 0);
        let second = page_media(&doc, 1);
        assert!((first[2] - 612.0).abs() < 0.01 && (first[3] - 792.0).abs() < 0.01);
        assert!((second[2] - 300.0).abs() < 0.01 && (second[3] - 400.0).abs() < 0.01);
    }

    #[tokio::test]
    async fn scale_blank_page_size_keeps_each_page_size() {
        let mut pdf = testkit::Pdf::new();
        pdf.add_page([0, 0, 300, 400], dictionary! {});
        let doc = scale(pdf.bytes(), vec![("pageSize", "")]).await;
        assert_eq!(page_ids(&doc).len(), 2);
        assert!((page_media(&doc, 1)[2] - 300.0).abs() < 0.01);
    }

    #[tokio::test]
    async fn scale_bakes_page_rotation_into_content() {
        let mut pdf = testkit::Pdf::new();
        pdf.edit_page(|page| {
            page.set(
                "MediaBox",
                Object::Array(vec![0.into(), 0.into(), 792.into(), 612.into()]),
            );
            page.set("Rotate", Object::Integer(90));
        });
        let doc = scale(pdf.bytes(), vec![("pageSize", "KEEP")]).await;
        let media = page_media(&doc, 0);
        assert!((media[2] - 792.0).abs() < 0.01);
        assert!((media[3] - 612.0).abs() < 0.01);
        assert_eq!(page_rotate(&doc, 0), 0);
        // PDFBox's form matrix for a 792x612 /Rotate 90 page.
        let stream = page_stream(&doc, 0);
        assert!(stream.contains("1.2941176470588236"));
        assert!(stream.contains("-0.7727272727272727"));
        assert!(stream.contains(" 612 cm") || stream.contains("612.0 cm"));
    }

    #[tokio::test]
    async fn scale_crop_box_clips_content_like_the_form_bbox() {
        let mut pdf = testkit::Pdf::new();
        pdf.edit_page(|page| {
            page.set(
                "CropBox",
                Object::Array(vec![100.into(), 100.into(), 500.into(), 700.into()]),
            );
        });
        let doc = scale(pdf.bytes(), vec![("pageSize", "KEEP")]).await;
        // Java's fresh page exposes CropBox == MediaBox.
        let crop = box_of(&doc, page_ids(&doc)[0], b"CropBox").unwrap();
        let media = page_media(&doc, 0);
        assert_eq!(crop, media);
        // viewBox LL (100,100) maps by mediaLL - 2·viewLL = -200,-200.
        let stream = page_stream(&doc, 0);
        assert!(stream.contains("-200 -200 cm"));
        // Clip = view box shifted by the same -200,-200.
        assert!(stream.contains("-100 -100 400 600 re"));
    }

    #[tokio::test]
    async fn scale_orientation_swaps_named_size() {
        let mut pdf = testkit::Pdf::new();
        let doc = scale(
            pdf.bytes(),
            vec![("pageSize", "A4"), ("orientation", "LANDSCAPE")],
        )
        .await;
        let media = page_media(&doc, 0);
        assert!((media[2] - 841.89).abs() < 0.01);
        assert!((media[3] - 595.276).abs() < 0.01);
    }

    #[tokio::test]
    async fn scale_rejects_unknown_page_size() {
        let mut pdf = testkit::Pdf::new();
        let params = vec![("pageSize".to_string(), "A9".to_string())];
        let error = scale_pages_work(AppState::new(), HashMap::new(), upload(pdf.bytes()), params)
            .unwrap_err();
        assert!(error.to_string().contains("Invalid page size format: A9"));
    }

    #[tokio::test]
    async fn scale_rejects_non_numeric_scale_factor() {
        let mut pdf = testkit::Pdf::new();
        let params = vec![("scaleFactor".to_string(), "big".to_string())];
        let error = scale_pages_work(AppState::new(), HashMap::new(), upload(pdf.bytes()), params)
            .unwrap_err();
        assert!(error.to_string().contains("invalid scaleFactor"));
    }
}
