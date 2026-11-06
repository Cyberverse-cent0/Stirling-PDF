//! Ports of the General API page-layout endpoints: N-up `multi-page-layout`
//! and `booklet-imposition` (`split-for-poster-print` lives in `split.rs`).
//! Both place source pages onto fresh output pages as Form XObjects,
//! mirroring PDFBox's `LayerUtility.importPageAsForm` + content-stream
//! transforms.

use std::collections::HashMap;

use axum::response::Response;
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};

use crate::error::{AppError, AppResult};
use crate::handlers::jobs::AppState;
use crate::handlers::merge::{parse_bool, pdf_response};
use crate::pdfdoc;
use crate::upload::{first_upload, output_name, param, param_bool, Upload};

/// `PDRectangle` sizes as PDFBox 3.0.8 computes them: millimetre constants
/// go through a float `POINTS_PER_MM`, inch constants are exact.
const PT_PER_MM: f32 = 1.0 / (10.0 * 2.54) * 72.0;

fn mm(width: f32, height: f32) -> (f64, f64) {
    ((width * PT_PER_MM) as f64, (height * PT_PER_MM) as f64)
}

/// Integer multipart param with a Java field default; absent or blank falls
/// back, non-numeric is a 400 like Spring's binding failure.
fn int_param(params: &[(String, String)], name: &str, default: i64) -> AppResult<i64> {
    crate::upload::param_parse(params, name, default)
}

fn float_param(params: &[(String, String)], name: &str, default: f64) -> AppResult<f64> {
    crate::upload::param_parse(params, name, default)
}

/// Boolean multipart param with a Java field default. Blank binds to null in
/// Spring, which reads as false for `Boolean.TRUE.equals(...)` fields.
fn bool_param(params: &[(String, String)], name: &str, default: bool) -> AppResult<bool> {
    let Some((_, value)) = params.iter().rev().find(|(n, _)| n == name) else {
        return Ok(default);
    };
    if value.trim().is_empty() {
        return Ok(false);
    }
    parse_bool(value, name)
}

/// String param where blank means the Java-side default (the controllers
/// all do `null/empty → default` before matching).
fn choice_param<'a>(params: &'a [(String, String)], name: &str, default: &'a str) -> &'a str {
    let raw = param(params, name, default);
    if raw.trim().is_empty() {
        default
    } else {
        raw
    }
}

/// Content-stream number: integers print without a fraction, matching how
/// PDFBox writes whole-valued floats.
fn num(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 9.0e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

fn translate(x: f64, y: f64) -> String {
    format!("1 0 0 1 {} {} cm\n", num(x), num(y))
}

fn scale_cm(s: f64) -> String {
    format!("{} 0 0 {} 0 0 cm\n", num(s), num(s))
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> String {
    format!("{} {} {} {} re\nS\n", num(x), num(y), num(w), num(h))
}

/// Appends a page to `doc`: media box, resources with the given Form
/// XObjects, and the content stream.
fn push_output_page(
    doc: &mut Document,
    width: f64,
    height: f64,
    content: &str,
    xobjects: &[(String, ObjectId)],
) -> AppResult<()> {
    let mut xobject_dict = Dictionary::new();
    for (name, id) in xobjects {
        xobject_dict.set(name.clone(), Object::Reference(*id));
    }
    let mut resources = Dictionary::new();
    resources.set("XObject", Object::Dictionary(xobject_dict));

    let content_id = doc.new_object_id();
    doc.objects.insert(
        content_id,
        Object::Stream(Stream::new(Dictionary::new(), content.as_bytes().to_vec())),
    );

    let mut page = Dictionary::new();
    page.set("Type", "Page");
    page.set(
        "MediaBox",
        Object::Array(vec![
            Object::Real(0.0),
            Object::Real(0.0),
            Object::Real(width as f32),
            Object::Real(height as f32),
        ]),
    );
    page.set("Resources", Object::Dictionary(resources));
    page.set("Contents", Object::Reference(content_id));
    pdfdoc::append_page(doc, page).map(|_| ())
}

/// `POST /api/v1/general/multi-page-layout` — packs `pagesPerSheet` (or
/// `rows`×`cols`) source pages onto each A4 sheet, scaled to fit their
/// cell and optionally bordered.
///
/// Divergence: `GeneralFormCopyUtils.copyAndTransformFormFields` is not
/// ported — widget annotations are not copied onto the laid-out pages.
#[stirling_macros::endpoint]
fn multi_page_layout(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let upload = first_upload(uploads)?;

    let mode = choice_param(&params, "mode", "DEFAULT");
    let (rows, cols, pages_per_sheet) = match mode {
        "DEFAULT" => {
            let pages = int_param(&params, "pagesPerSheet", 2)?;
            let root = (pages as f64).sqrt() as i64;
            if pages <= 0 || (pages != 2 && pages != root * root) {
                return Err(AppError::BadRequest(
                    "Invalid pagesPerSheet format: must be 2 or a perfect square".into(),
                ));
            }
            if pages == 2 {
                (1i64, 2i64, 2i64)
            } else {
                (root, root, pages)
            }
        }
        "CUSTOM" => {
            let rows = int_param(&params, "rows", 1)?;
            let cols = int_param(&params, "cols", 2)?;
            if rows <= 0 || cols <= 0 {
                return Err(AppError::BadRequest(
                    "Invalid rows and cols format: only strictly positive values are allowed"
                        .into(),
                ));
            }
            (rows, cols, rows.saturating_mul(cols))
        }
        _ => {
            return Err(AppError::BadRequest(
                "Invalid mode format: only 'DEFAULT' and 'CUSTOM' are supported".into(),
            ))
        }
    };

    const MAX_PAGES: i64 = 100_000;
    const MAX_COLS: i64 = 300;
    const MAX_ROWS: i64 = 300;
    if pages_per_sheet > MAX_PAGES {
        return Err(AppError::BadRequest(format!(
            "Invalid pagesPerSheet format: must be less than {MAX_PAGES}"
        )));
    }
    if cols > MAX_COLS {
        return Err(AppError::BadRequest(format!(
            "Invalid cols format: must be less than {MAX_COLS}"
        )));
    }
    if rows > MAX_ROWS {
        return Err(AppError::BadRequest(format!(
            "Invalid rows format: must be less than {MAX_ROWS}"
        )));
    }

    let orientation = choice_param(&params, "orientation", "PORTRAIT");
    let (page_width, page_height) = match orientation {
        "PORTRAIT" => mm(210.0, 297.0),
        "LANDSCAPE" => mm(297.0, 210.0),
        _ => {
            return Err(AppError::BadRequest(
                "Invalid orientation format: only 'PORTRAIT' and 'LANDSCAPE' are supported".into(),
            ))
        }
    };

    let arrangement = choice_param(&params, "arrangement", "BY_ROWS");
    if arrangement != "BY_ROWS" && arrangement != "BY_COLUMNS" {
        return Err(AppError::BadRequest(
            "Invalid arrangement format: only 'BY_ROWS' and 'BY_COLUMNS' are supported".into(),
        ));
    }
    let reading_direction = choice_param(&params, "readingDirection", "LTR");
    if reading_direction != "LTR" && reading_direction != "RTL" {
        return Err(AppError::BadRequest(
            "Invalid readingDirection format: only 'LTR' and 'RTL' are supported".into(),
        ));
    }

    let add_border = param_bool(&params, "addBorder");
    let top_margin = int_param(&params, "topMargin", 0)?;
    let bottom_margin = int_param(&params, "bottomMargin", 0)?;
    let left_margin = int_param(&params, "leftMargin", 0)?;
    let right_margin = int_param(&params, "rightMargin", 0)?;
    let inner_margin = int_param(&params, "innerMargin", 0)?;
    if top_margin < 0
        || bottom_margin < 0
        || left_margin < 0
        || right_margin < 0
        || inner_margin < 0
    {
        return Err(AppError::BadRequest(
            "Invalid margins format: only positive values are allowed".into(),
        ));
    }
    let border_width = int_param(&params, "borderWidth", 1)?;
    let border_width = if border_width == 0 { 1 } else { border_width };
    if add_border && border_width <= 0 {
        return Err(AppError::BadRequest(
            "Invalid borderWidth format: only strictly positive values are allowed when addBorder is true"
                .into(),
        ));
    }

    let source = pdfdoc::load(&upload.name, &upload.bytes)?;
    let page_ids = pdfdoc::page_ids(&source);
    let total_pages = page_ids.len();

    let (cols_f, rows_f) = (cols as f64, rows as f64);
    let cell_width = (page_width - left_margin as f64 - right_margin as f64) / cols_f;
    let cell_height = (page_height - top_margin as f64 - bottom_margin as f64) / rows_f;
    if cell_width <= 0.0 || cell_height <= 0.0 {
        return Err(AppError::BadRequest(
            "Invalid margin/layout configuration: resulting cell size is non-positive".into(),
        ));
    }
    let inner_width = cell_width - 2.0 * inner_margin as f64;
    let inner_height = cell_height - 2.0 * inner_margin as f64;
    if inner_width <= 0.0 || inner_height <= 0.0 {
        return Err(AppError::BadRequest(
            "Invalid inner margin: resulting inner content area is non-positive".into(),
        ));
    }

    let mut dst = pdfdoc::new_document();
    let mut map = HashMap::new();
    let pages_per_sheet = pages_per_sheet as usize;
    let cols = cols as usize;
    let rows = rows as usize;

    let mut first = 0usize;
    while first < total_pages {
        let mut content = String::new();
        if add_border {
            content.push_str(&format!("{} w\n0 0 0 RG\n", num(border_width as f64)));
        }
        let mut xobjects = Vec::new();

        for j in 0..pages_per_sheet {
            let Some(&page_id) = page_ids.get(first + j) else {
                break;
            };
            let media = pdfdoc::page_boxes(&source, page_id)?.media;
            let rect_w = (media[2] - media[0]) as f64;
            let rect_h = (media[3] - media[1]) as f64;
            let scale = (inner_width / rect_w).min(inner_height / rect_h);

            let (row_index, col_index) = if arrangement == "BY_ROWS" {
                let row = j / cols;
                let col = if reading_direction == "LTR" {
                    j % cols
                } else {
                    cols - 1 - (j % cols)
                };
                (row, col)
            } else {
                let row = j % rows;
                let col = if reading_direction == "LTR" {
                    j / rows
                } else {
                    cols - 1 - (j / rows)
                };
                (row, col)
            };

            let x = left_margin as f64
                + col_index as f64 * cell_width
                + inner_margin as f64
                + (inner_width - rect_w * scale) / 2.0;
            let y = page_height
                - top_margin as f64
                - ((row_index + 1) as f64 * cell_height
                    - inner_margin as f64
                    - (inner_height - rect_h * scale) / 2.0);

            let name = format!("Fm{j}");
            let form = pdfdoc::import_page_as_form(&mut dst, &source, page_id, &mut map)?;
            xobjects.push((name.clone(), form));

            content.push_str("q\n");
            content.push_str(&translate(x, y));
            content.push_str(&scale_cm(scale));
            content.push_str(&format!("/{name} Do\n"));
            content.push_str("Q\n");

            if add_border {
                content.push_str(&rect(x, y, rect_w * scale, rect_h * scale));
            }
        }

        push_output_page(&mut dst, page_width, page_height, &content, &xobjects)?;
        first += pages_per_sheet;
    }

    pdfdoc::carry_info(&source, &mut dst);
    let bytes = pdfdoc::save(&mut dst)?;
    Ok(pdf_response(
        bytes,
        &output_name(Some(&upload.name), "_multi_page_layout.pdf"),
    ))
}

/// `padToMultipleOf4` from `BookletImpositionController`.
fn pad_to_multiple_of_4(n: i64) -> i64 {
    (n + 3) / 4 * 4
}

/// One booklet side: the two page indices placed left and right (`-1` =
/// blank) and whether it is a back side.
struct BookletSide {
    left: i64,
    right: i64,
    // Stored for parity with Java's Side.isBack; the ordering math never reads it.
    #[allow(dead_code)]
    is_back: bool,
}

/// `saddleStitchSides`: signature-ordered (left, right) pairs for saddle
/// stitching, padded to a multiple of 4 with blank cells.
fn saddle_stitch_sides(
    total_pages: i64,
    double_sided: bool,
    duplex_pass: &str,
    flip_on_short_edge: bool,
) -> Vec<BookletSide> {
    let padded = pad_to_multiple_of_4(total_pages);
    let sheets = padded / 4;
    let mut sides = Vec::new();

    for s in 0..sheets {
        let a = padded - 1 - s * 2;
        let b = s * 2;
        let c = s * 2 + 1;
        let d = padded - 2 - s * 2;
        let clamp = |index: i64| if index < total_pages { index } else { -1 };
        let (a, b, c, d) = (clamp(a), clamp(b), clamp(c), clamp(d));

        let include_front = duplex_pass == "BOTH" || duplex_pass == "FIRST";
        let include_back = duplex_pass == "BOTH" || duplex_pass == "SECOND";

        if include_front {
            sides.push(BookletSide {
                left: a,
                right: b,
                is_back: false,
            });
        }
        if include_back {
            // flipOnShortEdge only applies to automatic duplex; manual
            // duplex users flip the stack themselves, like Java.
            let (left, right) = if double_sided && flip_on_short_edge {
                (d, c)
            } else {
                (c, d)
            };
            sides.push(BookletSide {
                left,
                right,
                is_back: true,
            });
        }
    }
    sides
}

/// The `drawCell` transform sequence for one booklet cell: scale-to-fit with
/// rotation compensation, then the Form XObject. Blank cells (`page_index`
/// -1) emit only their border. Appends to `content` and returns the form
/// registration for non-blank cells.
#[allow(clippy::too_many_arguments)]
fn booklet_cell(
    source: &Document,
    dst: &mut Document,
    map: &mut HashMap<ObjectId, ObjectId>,
    page_ids: &[ObjectId],
    page_index: i64,
    cell_x: f64,
    cell_y: f64,
    cell_w: f64,
    cell_h: f64,
    add_border: bool,
    name: &str,
    content: &mut String,
) -> AppResult<Option<(String, ObjectId)>> {
    if page_index < 0 {
        if add_border {
            content.push_str(&rect(cell_x, cell_y, cell_w, cell_h));
        }
        return Ok(None);
    }

    let page_id = page_ids[page_index as usize];
    let crop = pdfdoc::page_boxes(source, page_id)?.crop;
    let crop_w = (crop[2] - crop[0]) as f64;
    let crop_h = (crop[3] - crop[1]) as f64;
    let rotation = pdfdoc::page_rotation(source, page_id)?;

    let quarter_turn = matches!(rotation, 90 | 270);
    let (fit_w, fit_h) = if quarter_turn {
        (crop_h, crop_w)
    } else {
        (crop_w, crop_h)
    };
    let scale = (cell_w / fit_w).min(cell_h / fit_h);
    let drawn_w = fit_w * scale;
    let drawn_h = fit_h * scale;
    let tx = cell_x + (cell_w - drawn_w) / 2.0 - crop[0] as f64 * scale;
    let ty = cell_y + (cell_h - drawn_h) / 2.0 - crop[1] as f64 * scale;

    let form = pdfdoc::import_page_as_form(dst, source, page_id, map)?;

    content.push_str("q\n");
    content.push_str(&translate(tx, ty));
    content.push_str(&scale_cm(scale));
    match rotation {
        90 => {
            content.push_str("0 1 -1 0 0 0 cm\n");
            content.push_str(&translate(0.0, -crop_w));
        }
        180 => {
            content.push_str("-1 0 0 -1 0 0 cm\n");
            content.push_str(&translate(-crop_w, -crop_h));
        }
        270 => {
            content.push_str("0 -1 1 0 0 0 cm\n");
            content.push_str(&translate(-crop_h, 0.0));
        }
        _ => {}
    }
    content.push_str(&format!("/{name} Do\n"));
    content.push_str("Q\n");

    if add_border {
        content.push_str(&rect(cell_x, cell_y, cell_w, cell_h));
    }
    Ok(Some((name.to_string(), form)))
}

/// `POST /api/v1/general/booklet-imposition` — saddle-stitch booklet:
/// signature-ordered pages imposed two-up on landscape sheets.
#[stirling_macros::endpoint]
fn booklet_imposition(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let upload = first_upload(uploads)?;

    let pages_per_sheet = int_param(&params, "pagesPerSheet", 2)?;
    if pages_per_sheet != 2 {
        return Err(AppError::BadRequest(
            "Booklet printing uses 2 pages per side (landscape). For 4-up, use the N-up feature."
                .into(),
        ));
    }
    let add_border = param_bool(&params, "addBorder");
    let spine_location = param(&params, "spineLocation", "LEFT");
    let add_gutter = param_bool(&params, "addGutter");
    let gutter_size = float_param(&params, "gutterSize", 12.0)?;
    let double_sided = bool_param(&params, "doubleSided", true)?;
    let duplex_pass = param(&params, "duplexPass", "BOTH");
    let flip_on_short_edge = param_bool(&params, "flipOnShortEdge");

    let source = pdfdoc::load(&upload.name, &upload.bytes)?;
    let page_ids = pdfdoc::page_ids(&source);
    let total_pages = page_ids.len();
    if total_pages == 0 {
        return Err(AppError::BadRequest("PDF has no pages".into()));
    }

    // Paper size comes from the first page's crop box, forced to landscape.
    let first_crop = pdfdoc::page_boxes(&source, page_ids[0])?.crop;
    let crop_w = (first_crop[2] - first_crop[0]) as f64;
    let crop_h = (first_crop[3] - first_crop[1]) as f64;
    let (page_width, page_height) = (crop_h, crop_w);

    let mut gutter = gutter_size;
    if gutter < 0.0 {
        gutter = 0.0;
    }
    if gutter >= page_width / 2.0 {
        gutter = page_width / 2.0 - 1.0;
    }

    let sides = saddle_stitch_sides(
        total_pages as i64,
        double_sided,
        duplex_pass,
        flip_on_short_edge,
    );

    let rtl = spine_location.eq_ignore_ascii_case("RIGHT");
    let (left_col, right_col) = if rtl {
        (1usize, 0usize)
    } else {
        (0usize, 1usize)
    };
    let cell_w = page_width / 2.0;
    let cell_h = page_height;
    let g = if add_gutter { gutter } else { 0.0 };
    let left_cell_x = left_col as f64 * cell_w + g / 2.0;
    let right_cell_x = right_col as f64 * cell_w - g / 2.0;
    let cell_content_w = cell_w - g / 2.0;

    let mut dst = pdfdoc::new_document();
    let mut map = HashMap::new();

    for side in &sides {
        let mut content = String::new();
        if add_border {
            content.push_str("1.5 w\n0 0 0 RG\n");
        }
        let mut xobjects = Vec::new();

        for (index, (page_index, cell_x, cell_content_w)) in [
            (side.left, left_cell_x, cell_content_w),
            (side.right, right_cell_x, cell_content_w),
        ]
        .into_iter()
        .enumerate()
        {
            let name = format!("Fm{index}");
            if let Some(entry) = booklet_cell(
                &source,
                &mut dst,
                &mut map,
                &page_ids,
                page_index,
                cell_x,
                0.0,
                cell_content_w,
                cell_h,
                add_border,
                &name,
                &mut content,
            )? {
                xobjects.push(entry);
            }
        }

        push_output_page(&mut dst, page_width, page_height, &content, &xobjects)?;
    }

    pdfdoc::carry_info(&source, &mut dst);
    let bytes = pdfdoc::save(&mut dst)?;
    // Java runs the name through `Filenames.toSimpleFileName` first.
    let base_name = upload
        .name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&upload.name);
    Ok(pdf_response(
        bytes,
        &output_name(Some(base_name), "_booklet.pdf"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit;
    use lopdf::{dictionary, Object};

    fn upload(bytes: Vec<u8>) -> Vec<Upload> {
        vec![Upload {
            name: "in.pdf".into(),
            bytes: crate::upload::UploadBytes::from_vec(bytes),
            content_type: None,
        }]
    }

    fn params(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// A document with `count` letter pages, each with a tiny content
    /// stream so the Form XObject import has something to wrap.
    fn letter_pdf(count: usize) -> testkit::Pdf {
        let mut pdf = testkit::Pdf::new();
        let content = pdf.add_stream(Dictionary::new(), b"0 0 m\n");
        pdf.edit_page(|page| {
            page.set("Contents", Object::Reference(content));
        });
        for _ in 1..count {
            let content = pdf.add_stream(Dictionary::new(), b"0 0 m\n");
            pdf.add_page(
                [0, 0, 612, 792],
                dictionary! {
                    "Contents" => Object::Reference(content),
                },
            );
        }
        pdf
    }

    /// Media box of the `index`-th page (0-based) of a response document.
    fn page_media(doc: &Document, index: usize) -> [f64; 4] {
        let ids: Vec<ObjectId> = doc.get_pages().into_values().collect();
        let media = pdfdoc::page_boxes(doc, ids[index]).unwrap().media;
        [
            media[0] as f64,
            media[1] as f64,
            media[2] as f64,
            media[3] as f64,
        ]
    }

    /// The page's content stream decoded to text.
    fn page_content(doc: &Document, index: usize) -> String {
        let ids: Vec<ObjectId> = doc.get_pages().into_values().collect();
        let bytes = doc.get_page_content(ids[index]);
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[tokio::test]
    async fn multi_page_layout_packs_four_pages_onto_one_a4_sheet() {
        let mut pdf = letter_pdf(4);
        let bytes = pdf.bytes();

        let response = multi_page_layout_work(
            AppState::new(),
            HashMap::new(),
            upload(bytes),
            params(&[("mode", "DEFAULT"), ("pagesPerSheet", "4")]),
        )
        .unwrap();
        let doc = testkit::response_document(response).await;

        assert_eq!(doc.get_pages().len(), 1);
        // A4 portrait in PDFBox float points.
        let media = page_media(&doc, 0);
        assert!((media[2] - 595.2756).abs() < 0.01);
        assert!((media[3] - 841.8898).abs() < 0.01);

        let content = page_content(&doc, 0);
        for name in ["Fm0", "Fm1", "Fm2", "Fm3"] {
            assert!(content.contains(&format!("/{name} Do")), "missing {name}");
        }
    }

    #[tokio::test]
    async fn multi_page_layout_custom_grid_and_partial_last_sheet() {
        let mut pdf = letter_pdf(5);
        let bytes = pdf.bytes();

        let response = multi_page_layout_work(
            AppState::new(),
            HashMap::new(),
            upload(bytes),
            params(&[
                ("mode", "CUSTOM"),
                ("rows", "1"),
                ("cols", "2"),
                ("orientation", "LANDSCAPE"),
            ]),
        )
        .unwrap();
        let doc = testkit::response_document(response).await;

        // 5 pages at 2-up → 3 sheets; the last holds a single cell.
        assert_eq!(doc.get_pages().len(), 3);
        let media = page_media(&doc, 0);
        assert!((media[2] - 841.8898).abs() < 0.01);
        assert!((media[3] - 595.2756).abs() < 0.01);
        assert!(page_content(&doc, 2).contains("/Fm0 Do"));
        assert!(!page_content(&doc, 2).contains("/Fm1 Do"));
    }

    #[tokio::test]
    async fn multi_page_layout_rejects_bad_grid_parameters() {
        let mut pdf = letter_pdf(2);
        let bytes = pdf.bytes();

        for bad in [
            params(&[("pagesPerSheet", "3")]),
            params(&[("mode", "CUSTOM"), ("rows", "0"), ("cols", "2")]),
            params(&[("mode", "CUSTOM"), ("rows", "2"), ("cols", "301")]),
            params(&[("orientation", "SQUARE")]),
            params(&[("arrangement", "SPIRAL")]),
            params(&[("readingDirection", "TTB")]),
            params(&[("addBorder", "true"), ("borderWidth", "-2")]),
        ] {
            let result = multi_page_layout_work(
                AppState::new(),
                HashMap::new(),
                upload(bytes.clone()),
                bad.clone(),
            );
            assert!(result.is_err(), "expected rejection for {bad:?}");
        }
    }

    #[tokio::test]
    async fn booklet_imposition_orders_sides_and_blanks() {
        let mut pdf = letter_pdf(5);
        let bytes = pdf.bytes();

        let response =
            booklet_imposition_work(AppState::new(), HashMap::new(), upload(bytes), params(&[]))
                .unwrap();
        let doc = testkit::response_document(response).await;

        // 5 pages pad to 8 → 2 sheets × front+back = 4 sides.
        assert_eq!(doc.get_pages().len(), 4);
        // Paper is the crop box forced to landscape: 792 × 612.
        let media = page_media(&doc, 0);
        assert!((media[2] - 792.0).abs() < 0.01);
        assert!((media[3] - 612.0).abs() < 0.01);

        // Front of sheet 1 places padded page 7 (blank) left and page 0
        // right: only one form is drawn.
        assert_eq!(page_content(&doc, 0).matches(" Do\n").count(), 1);
    }

    #[tokio::test]
    async fn booklet_imposition_rejects_non_two_up() {
        let mut pdf = letter_pdf(2);
        let bytes = pdf.bytes();

        let result = booklet_imposition_work(
            AppState::new(),
            HashMap::new(),
            upload(bytes),
            params(&[("pagesPerSheet", "4")]),
        );
        assert!(result.is_err());
    }

    #[test]
    fn saddle_stitch_orders_front_and_back_sides() {
        // 8 pages → 2 sheets: fronts (7,0),(5,2), backs (1,6),(3,4).
        let sides = saddle_stitch_sides(8, true, "BOTH", false);
        let pairs: Vec<(i64, i64)> = sides.iter().map(|s| (s.left, s.right)).collect();
        assert_eq!(pairs, vec![(7, 0), (1, 6), (5, 2), (3, 4)]);
        assert!(sides
            .iter()
            .map(|s| s.is_back)
            .eq([false, true, false, true]));
    }

    #[test]
    fn saddle_stitch_blanks_past_the_end_and_honours_duplex_pass() {
        // 5 pages → padded to 8 → sheets=2; second sheet's front is (3,-1→? ).
        let sides = saddle_stitch_sides(5, true, "BOTH", false);
        assert_eq!(sides.len(), 4);
        // N=8: s=1 → a=5→-1? a=8-1-2=5 ≥5 → -1; b=2; c=3; d=8-2-2=4.
        assert_eq!((sides[2].left, sides[2].right), (-1, 2));
        assert_eq!((sides[3].left, sides[3].right), (3, 4));

        let fronts = saddle_stitch_sides(8, true, "FIRST", false);
        assert!(fronts.iter().all(|s| !s.is_back));
        let backs = saddle_stitch_sides(8, true, "SECOND", false);
        assert!(backs.iter().all(|s| s.is_back));
    }

    #[test]
    fn saddle_stitch_short_edge_swaps_back_sides_only_when_double_sided() {
        let swapped = saddle_stitch_sides(8, true, "BOTH", true);
        assert_eq!((swapped[1].left, swapped[1].right), (6, 1));
        let manual = saddle_stitch_sides(8, false, "BOTH", true);
        assert_eq!((manual[1].left, manual[1].right), (1, 6));
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    fn params(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn mm_converts_millimetres_to_points() {
        let (w, h) = mm(210.0, 297.0);
        // 210mm ≈ 595.28pt, 297mm ≈ 841.89pt.
        assert!((w - 595.28).abs() < 0.01);
        assert!((h - 841.89).abs() < 0.01);
    }

    #[test]
    fn int_param_validates_like_spring_binding() {
        assert_eq!(int_param(&params(&[("rows", "3")]), "rows", 1).unwrap(), 3);
        assert_eq!(int_param(&[], "rows", 1).unwrap(), 1);
        assert_eq!(int_param(&params(&[("rows", "  ")]), "rows", 1).unwrap(), 1);
        assert!(int_param(&params(&[("rows", "abc")]), "rows", 1).is_err());
    }

    #[test]
    fn bool_param_blank_is_false_and_absent_is_default() {
        assert!(bool_param(&params(&[("flag", "true")]), "flag", false).unwrap());
        assert!(!bool_param(&params(&[("flag", "false")]), "flag", true).unwrap());
        // Blank binds to null in Spring → false regardless of default.
        assert!(!bool_param(&params(&[("flag", "")]), "flag", true).unwrap());
        assert!(bool_param(&[], "flag", true).unwrap());
        assert!(bool_param(&params(&[("flag", "bogus")]), "flag", false).is_err());
    }

    #[test]
    fn choice_param_treats_blank_as_default() {
        assert_eq!(
            choice_param(&params(&[("mode", "CUSTOM")]), "mode", "DEFAULT"),
            "CUSTOM"
        );
        assert_eq!(
            choice_param(&params(&[("mode", "  ")]), "mode", "DEFAULT"),
            "DEFAULT"
        );
        assert_eq!(choice_param(&[], "mode", "DEFAULT"), "DEFAULT");
    }

    #[test]
    fn content_stream_formatting_matches_pdfbox() {
        assert_eq!(num(612.0), "612");
        assert_eq!(num(1.5), "1.5");
        assert_eq!(translate(10.0, -5.5), "1 0 0 1 10 -5.5 cm\n");
        assert_eq!(scale_cm(2.0), "2 0 0 2 0 0 cm\n");
        assert_eq!(rect(1.0, 2.0, 3.0, 4.0), "1 2 3 4 re\nS\n");
    }

    #[test]
    fn first_upload_requires_file_input() {
        assert!(first_upload(vec![]).is_err());
        let upload = first_upload(vec![Upload {
            name: "a.pdf".into(),
            bytes: crate::upload::UploadBytes::from_vec(vec![]),
            content_type: None,
        }])
        .unwrap();
        assert_eq!(upload.name, "a.pdf");
    }
}
