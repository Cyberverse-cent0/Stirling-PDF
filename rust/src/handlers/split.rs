//! Split endpoints (`/api/v1/general/split-*`): page-range cuts, auto
//! size/count splits, and bookmark-based chapter splits — all returning ZIP
//! archives. Runs on the `lopdf` object model via `pdfdoc`.

use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Write};

use axum::response::Response;
use lopdf::{Dictionary, Object, Stream};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::error::{AppError, AppResult};
use crate::handlers::jobs::AppState;
use crate::handlers::merge::{file_response, pdf_response};
use crate::page_select;
use crate::pdfdoc;
use crate::upload::{output_name, param, param_bool, remove_extension, Upload, FILE_FIELD};

/// Loads the first upload and returns it with its raw bytes (needed to build
/// per-segment documents).
fn source(uploads: Vec<Upload>) -> AppResult<(String, Vec<u8>)> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    Ok((upload.name, upload.bytes.to_vec()))
}

/// Builds a one-segment document containing `order` (0-based page indices)
/// and returns its serialised bytes.
fn segment_bytes(source_name: &str, source_bytes: &[u8], order: &[u32]) -> AppResult<Vec<u8>> {
    let mut doc = pdfdoc::load(source_name, source_bytes)?;
    pdfdoc::subset(&mut doc, order)?;
    pdfdoc::prune_orphaned_form_fields(&mut doc)?;
    pdfdoc::save(&mut doc)
}

pub(crate) fn zip_response(entries: Vec<(String, Vec<u8>)>, zip_name: &str) -> AppResult<Response> {
    let mut archive = Cursor::new(Vec::new());
    {
        let mut zip = ZipWriter::new(&mut archive);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, bytes) in entries {
            zip.start_file(name, options)
                .map_err(|e| AppError::Pdf(e.to_string()))?;
            zip.write_all(&bytes)
                .map_err(|e| AppError::Pdf(e.to_string()))?;
        }
        zip.finish().map_err(|e| AppError::Pdf(e.to_string()))?;
    }
    Ok(file_response(
        archive.into_inner(),
        zip_name,
        // Java answers every multi-file endpoint with octet-stream, not
        // application/zip (`WebResponseUtils` uses APPLICATION_OCTET_STREAM).
        "application/octet-stream",
    ))
}

/// `POST /api/v1/general/split-by-size-or-count` — auto-split into a ZIP of
/// `<base>_N.pdf` segments. `splitType`: 0 = max size per doc, 1 = pages per
/// doc, 2 = number of docs. `splitValue`: size string (`"500KB"`, bare number
/// = MB) or a positive count.
#[stirling_macros::endpoint]
fn split_by_size_or_count(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let (name, bytes) = source(uploads)?;
    let split_type: i64 = param(&params, "splitType", "0").trim().parse().unwrap_or(0);
    let split_value = param(&params, "splitValue", "10MB").to_string();

    let document = pdfdoc::load(&name, &bytes)?;
    let total_pages = pdfdoc::page_ids(&document).len();
    let ranges = match split_type {
        0 => size_ranges(&name, &bytes, total_pages, &split_value)?,
        1 => page_count_ranges(total_pages, positive_count(&split_value)?),
        2 => doc_count_ranges(total_pages, positive_count(&split_value)?),
        _ => {
            return Err(AppError::BadRequest(format!(
                "Invalid argument: split type: {split_type}"
            )))
        }
    };

    let base = remove_extension(Some(&name));
    let mut entries = Vec::new();
    for (index, (start, end)) in ranges.into_iter().enumerate() {
        let order: Vec<u32> = (start as u32..=end as u32).collect();
        entries.push((
            format!("{base}_{}.pdf", index + 1),
            segment_bytes(&name, &bytes, &order)?,
        ));
    }
    zip_response(entries, &format!("{base}.zip"))
}

/// `GeneralUtils.convertSizeToBytes(sizeStr, "MB")`: bare number = MB,
/// units B/KB/MB/GB/TB are 1024-based.
fn size_to_bytes(value: &str) -> Result<u64, String> {
    let trimmed = value.trim();
    let split = trimmed
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(trimmed.len());
    let (number, unit) = trimmed.split_at(split);
    let multiplier = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "MB" => 1u64 << 20,
        "B" => 1,
        "KB" => 1 << 10,
        "GB" => 1 << 30,
        "TB" => 1 << 40,
        _ => return Err(format!("invalid size unit: {unit}")),
    };
    let amount: f64 = number
        .trim()
        .parse()
        .map_err(|_| format!("invalid size value: {value}"))?;
    Ok((amount * multiplier as f64) as u64)
}

fn positive_count(value: &str) -> Result<usize, AppError> {
    match value.trim().parse::<usize>() {
        Ok(count) if count > 0 => Ok(count),
        _ => Err(AppError::BadRequest(
            "splitValue must be a positive integer for page-count or document-count splitting"
                .into(),
        )),
    }
}

/// Contiguous inclusive 0-based ranges of `pages_per_doc` pages.
fn page_count_ranges(total_pages: usize, pages_per_doc: usize) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = 0;
    while start < total_pages {
        let end = (start + pages_per_doc - 1).min(total_pages - 1);
        ranges.push((start, end));
        start = end + 1;
    }
    ranges
}

/// `totalPages` split as evenly as possible into `document_count` docs.
fn doc_count_ranges(total_pages: usize, document_count: usize) -> Vec<(usize, usize)> {
    let per = total_pages / document_count;
    let extra = total_pages % document_count;
    let mut ranges = Vec::new();
    let mut cursor = 0;
    for i in 0..document_count {
        let pages = per + usize::from(i < extra);
        if pages == 0 {
            continue;
        }
        let end = cursor + pages - 1;
        ranges.push((cursor, end));
        cursor = end + 1;
    }
    ranges
}

/// Java `computeSizeRanges`: greedily packs pages under `max_bytes`, probing
/// the real saved size every 5 pages / ≥20 pages / at lookahead, with a
/// lookahead fit when the probe lands under 75% of the limit.
fn size_ranges(
    name: &str,
    bytes: &[u8],
    total_pages: usize,
    split_value: &str,
) -> AppResult<Vec<(usize, usize)>> {
    let max_bytes = size_to_bytes(split_value).map_err(AppError::BadRequest)? as usize;
    let segment_size = |from: usize, to: usize| -> AppResult<usize> {
        let order: Vec<u32> = (from as u32..=to as u32).collect();
        segment_bytes(name, bytes, &order).map(|b| b.len())
    };

    let mut ranges = Vec::new();
    let mut range_start = 0usize;
    let mut range_end = 0usize;
    let mut page_index = 0usize;

    while page_index < total_pages {
        range_end = page_index;
        let page_added = range_end - range_start + 1;
        let should_check =
            page_added.is_multiple_of(5) || page_index == total_pages - 1 || page_added >= 20;
        if !should_check {
            page_index += 1;
            continue;
        }
        let actual = segment_size(range_start, range_end)?;
        if actual > max_bytes {
            if page_added > 1 {
                range_end = page_index - 1;
                page_index -= 1;
            }
            ranges.push((range_start, range_end));
            range_start = range_end + 1;
            range_end = range_start - 1;
        } else if page_index + 1 < total_pages && actual * 4 < max_bytes * 3 {
            let lookahead = 5usize.min(total_pages - page_index - 1);
            let mut extra = 0usize;
            for i in 0..lookahead {
                if segment_size(range_start, page_index + 1 + i)? > max_bytes {
                    break;
                }
                extra += 1;
            }
            page_index += extra;
            range_end = page_index;
        }
        page_index += 1;
    }
    if range_end >= range_start && range_end < total_pages {
        ranges.push((range_start, range_end));
    }
    Ok(ranges)
}

struct Chapter {
    title: String,
    start_page: usize,
    end_page: usize,
}

/// `POST /api/v1/general/split-pdf-by-chapters` — splits on internal PDF
/// bookmarks/outline entries and returns a ZIP of `<NN> <title>.pdf` files.
#[stirling_macros::endpoint]
fn split_by_chapters(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let (name, bytes) = source(uploads)?;
    let include_metadata = param(&params, "includeMetadata", "false") == "true";
    let allow_duplicates = param(&params, "allowDuplicates", "false") == "true";
    let bookmark_level: isize = param(&params, "bookmarkLevel", "1")
        .trim()
        .parse()
        .map_err(|_| AppError::BadRequest("bookmarkLevel must be an integer".into()))?;
    if bookmark_level < 0 {
        return Err(AppError::BadRequest("bookmarkLevel must be >= 0".into()));
    }
    let bookmark_level = bookmark_level as usize;

    let document = pdfdoc::load(&name, &bytes)?;
    let total_pages = pdfdoc::page_ids(&document).len();
    let roots = pdfdoc::outline(&document)?;

    let mut chapters = Vec::new();
    for node in &roots {
        collect_bookmarks(node, &mut chapters, 0, bookmark_level);
    }
    if chapters.is_empty() {
        return Err(AppError::BadRequest(
            "No PDF bookmarks/outline found in document".into(),
        ));
    }
    assign_end_pages(&mut chapters, total_pages);
    if !allow_duplicates {
        merge_same_page_chapters(&mut chapters);
    }

    // Source /Info for includeMetadata, resolved once.
    let source_info = if include_metadata {
        document
            .trailer
            .get(b"Info")
            .ok()
            .and_then(|i| document.dereference(i).ok().map(|(_, o)| o))
            .and_then(|o| o.as_dict().ok().cloned())
    } else {
        None
    };

    let base = remove_extension(Some(&name));
    let width = chapters.len().to_string().len();
    let mut entries = Vec::new();
    for (index, chapter) in chapters.iter().enumerate() {
        let (from, to) = clamp_range(chapter, total_pages);
        let order: Vec<u32> = (from as u32..=to as u32).collect();
        let mut segment = pdfdoc::load(&name, &bytes)?;
        pdfdoc::subset(&mut segment, &order)?;
        pdfdoc::prune_orphaned_form_fields(&mut segment)?;
        if let Some(info) = &source_info {
            segment
                .trailer
                .set("Info", lopdf::Object::Dictionary(info.clone()));
        }
        pdfdoc::touch_loaded_info(&mut segment);
        entries.push((
            format!("{:0width$} {}.pdf", index, chapter.title, width = width),
            pdfdoc::save(&mut segment)?,
        ));
    }
    zip_response(entries, &format!("{base}.zip"))
}

/// Java `collectBookmarks`: emits nodes with an internal destination
/// (`page.is_some()`), recursing into children while `level < max_level`.
/// Children of non-internal nodes are skipped entirely, like Java's
/// `continue`.
fn collect_bookmarks(
    node: &pdfdoc::OutlineNode,
    out: &mut Vec<Chapter>,
    level: usize,
    max_level: usize,
) {
    if let Some(page) = node.page {
        out.push(Chapter {
            title: node.title.replace('/', ""),
            start_page: page,
            end_page: 0,
        });
        if level < max_level {
            for child in &node.children {
                collect_bookmarks(child, out, level + 1, max_level);
            }
        }
    }
}

fn assign_end_pages(chapters: &mut [Chapter], total_pages: usize) {
    for i in 0..chapters.len() {
        let next = chapters[i + 1..]
            .iter()
            .find(|c| c.start_page >= chapters[i].start_page)
            .map(|c| c.start_page)
            .unwrap_or(total_pages);
        chapters[i].end_page = next;
    }
}

/// Java `mergeBookmarksThatCorrespondToSamePage`: zero-length chapters get
/// their titles folded into the next real chapter.
fn merge_same_page_chapters(chapters: &mut Vec<Chapter>) {
    let mut merged_title = String::new();
    let mut remove = vec![false; chapters.len()];
    for (i, chapter) in chapters.iter_mut().enumerate() {
        if chapter.start_page == chapter.end_page {
            merged_title.push_str(&chapter.title);
            merged_title.push(' ');
            remove[i] = true;
        } else {
            if !merged_title.is_empty() {
                // Java replaces the chapter title with the folded prefix
                // verbatim — the chapter's own title is discarded, and the
                // trailing space survives.
                if merged_title.chars().count() > 255 {
                    merged_title = merged_title.chars().take(253).collect::<String>() + "...";
                }
                chapter.title = merged_title.clone();
            }
            merged_title.clear();
        }
    }
    let mut i = 0;
    chapters.retain(|_| {
        let keep = !remove[i];
        i += 1;
        keep
    });
}

fn clamp_range(chapter: &Chapter, total_pages: usize) -> (usize, usize) {
    let from = chapter.start_page.min(total_pages - 1);
    let raw_end = if chapter.start_page == chapter.end_page {
        chapter.end_page
    } else {
        chapter.end_page - 1
    };
    let to = raw_end.max(from).min(total_pages - 1);
    (from, to)
}

const MAX_SECTION_PAGES: usize = 100_000;

fn division_param(params: &[(String, String)], name: &str, default: &str) -> AppResult<usize> {
    let value = param(params, name, default);
    match value.trim().parse::<i64>() {
        Ok(divisions) if (0..=50).contains(&divisions) => Ok(divisions as usize),
        _ => Err(AppError::BadRequest(format!(
            "{name} must be an integer between 0 and 50"
        ))),
    }
}

fn select_section_pages(
    mode: &str,
    page_numbers: &str,
    total_pages: usize,
) -> AppResult<HashSet<usize>> {
    match mode {
        "CUSTOM" => {
            if page_numbers.trim().is_empty() {
                return Err(AppError::BadRequest(
                    "page numbers is required for custom mode".into(),
                ));
            }
            page_select::require_valid_page_selection(page_numbers, total_pages)
                .map_err(AppError::BadRequest)?;
            Ok(page_select::parse_page_list(page_numbers, total_pages)
                .map_err(AppError::BadRequest)?
                .into_iter()
                .collect())
        }
        "SPLIT_ALL" => Ok((0..total_pages).collect()),
        "SPLIT_ALL_EXCEPT_FIRST" => Ok((1..total_pages).collect()),
        "SPLIT_ALL_EXCEPT_LAST" => Ok((0..total_pages.saturating_sub(1)).collect()),
        "SPLIT_ALL_EXCEPT_FIRST_AND_LAST" => Ok((1..total_pages.saturating_sub(1)).collect()),
        _ => Err(AppError::BadRequest(format!("Invalid split mode: {mode}"))),
    }
}

/// Imports `page_index` into `dst` and turns it into one section page when
/// `section` is `(horiz_index, vert_index, horiz_count, vert_count)`.
fn append_output_page(
    dst: &mut lopdf::Document,
    src: &mut lopdf::Document,
    page_index: usize,
    section: Option<(usize, usize, usize, usize)>,
) -> AppResult<()> {
    pdfdoc::import_pages(dst, src, &[page_index])?;
    let new_page = *pdfdoc::page_ids(dst)
        .last()
        .ok_or_else(|| AppError::Pdf("page import produced no page".into()))?;
    // Java's LayerUtility form import drops page annotations.
    pdfdoc::clear_annots(dst, new_page)?;

    let Some((i, j, horiz, verti)) = section else {
        return Ok(());
    };
    let src_pages = pdfdoc::page_ids(src);
    let src_id = src_pages[page_index];
    let media = pdfdoc::media_box(src, src_id)?;
    let sub_width = (media[2] - media[0]) / horiz as f64;
    let sub_height = (media[3] - media[1]) / verti as f64;
    let tx = -(media[0] + i as f64 * sub_width);
    let ty = -(media[1] + (verti - 1 - j) as f64 * sub_height);
    pdfdoc::set_page_box(dst, new_page, [0.0, 0.0, sub_width, sub_height])?;
    pdfdoc::wrap_page_content(
        dst,
        new_page,
        &pdfdoc::section_prefix(sub_width, sub_height, tx, ty),
    )
}

/// `POST /api/v1/general/split-pdf-by-sections` — splits selected pages into
/// `horizontalDivisions + 1` by `verticalDivisions + 1` clipped sections.
/// `merge=true` returns one PDF; otherwise a ZIP of one-page PDFs named
/// `<base>_split_<page>_<section>.pdf`.
#[stirling_macros::endpoint]
fn split_by_sections(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let (name, bytes) = source(uploads)?;
    let mut source = pdfdoc::load(&name, &bytes)?;
    let total_pages = pdfdoc::page_ids(&source).len();

    let split_mode = param(&params, "splitMode", "SPLIT_ALL").to_string();
    let page_numbers = param(&params, "pageNumbers", "").to_string();
    let pages_to_split = select_section_pages(&split_mode, &page_numbers, total_pages)?;
    let horizontal = division_param(&params, "horizontalDivisions", "0")? + 1;
    let vertical = division_param(&params, "verticalDivisions", "1")? + 1;
    if pages_to_split.len() * horizontal * vertical > MAX_SECTION_PAGES {
        return Err(AppError::BadRequest(format!(
            "Invalid argument: the divisions would produce more than {MAX_SECTION_PAGES} pages"
        )));
    }
    let merge = param_bool(&params, "merge");
    let filename = format!("{}_split", remove_extension(Some(&name)));

    if merge {
        let mut merged = pdfdoc::new_document();
        for page_index in 0..total_pages {
            if pages_to_split.contains(&page_index) {
                for i in 0..horizontal {
                    for j in 0..vertical {
                        append_output_page(
                            &mut merged,
                            &mut source,
                            page_index,
                            Some((i, j, horizontal, vertical)),
                        )?;
                    }
                }
            } else {
                append_output_page(&mut merged, &mut source, page_index, None)?;
            }
        }
        pdfdoc::carry_info(&source, &mut merged);
        let bytes = pdfdoc::save(&mut merged)?;
        return Ok(pdf_response(bytes, &format!("{filename}.pdf")));
    }

    let mut entries = Vec::new();
    for page_index in 0..total_pages {
        let page_num = page_index + 1;
        if pages_to_split.contains(&page_index) {
            for i in 0..horizontal {
                for j in 0..vertical {
                    let mut section_doc = pdfdoc::new_document();
                    append_output_page(
                        &mut section_doc,
                        &mut source,
                        page_index,
                        Some((i, j, horizontal, vertical)),
                    )?;
                    pdfdoc::default_info(&mut section_doc);
                    let section_num = i * vertical + j + 1;
                    entries.push((
                        format!("{filename}_{page_num}_{section_num}.pdf"),
                        pdfdoc::save(&mut section_doc)?,
                    ));
                }
            }
        } else {
            let mut page_doc = pdfdoc::new_document();
            append_output_page(&mut page_doc, &mut source, page_index, None)?;
            pdfdoc::default_info(&mut page_doc);
            entries.push((
                format!("{filename}_{page_num}_1.pdf"),
                pdfdoc::save(&mut page_doc)?,
            ));
        }
    }
    zip_response(entries, &format!("{filename}.zip"))
}

/// `POST /api/v1/general/split-pages` → `application/zip` of
/// `<base>_1.pdf`, `<base>_2.pdf`, …
#[stirling_macros::endpoint]
fn split_pages(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let (name, bytes) = source(uploads)?;
    // SplitPagesRequest.pageNumbers defaults to "all" and lists split points.
    let page_numbers = param(&params, "pageNumbers", "all").to_string();

    let document = pdfdoc::load(&name, &bytes)?;
    let total_pages = pdfdoc::page_ids(&document).len();
    page_select::require_valid_page_selection(&page_numbers, total_pages)
        .map_err(AppError::BadRequest)?;
    let mut split_points =
        page_select::parse_page_list(&page_numbers, total_pages).map_err(AppError::BadRequest)?;
    // Java always cuts after the last page so the tail isn't dropped.
    if total_pages > 0 && !split_points.contains(&(total_pages - 1)) {
        split_points.push(total_pages - 1);
    }

    let base = remove_extension(Some(&name));
    let mut entries = Vec::new();
    let mut previous = 0usize;
    for (index, &split) in split_points.iter().enumerate() {
        let order: Vec<u32> = (previous as u32..=split as u32).collect();
        entries.push((
            format!("{base}_{}.pdf", index + 1),
            segment_bytes(&name, &bytes, &order)?,
        ));
        previous = split + 1;
    }
    zip_response(entries, &output_name(Some(&name), "_split.zip"))
}

const MAX_POSTER_PAGES: usize = 100_000;

/// `PDRectangle` page sizes in points. `POINTS_PER_MM` is
/// `1 / (10 * 2.54) * 72` in PDFBox's float arithmetic.
fn poster_page_size(name: &str) -> AppResult<(f64, f64)> {
    let points_per_mm = 72.0 / 25.4;
    let size = match name {
        "A4" => (210.0 * points_per_mm, 297.0 * points_per_mm),
        "Letter" => (612.0, 792.0),
        "A3" => (297.0 * points_per_mm, 420.0 * points_per_mm),
        "A5" => (148.0 * points_per_mm, 210.0 * points_per_mm),
        "Legal" => (612.0, 1008.0),
        "Tabloid" => (792.0, 1224.0),
        _ => {
            return Err(AppError::BadRequest(format!("Invalid page size: {name}")));
        }
    };
    Ok(size)
}

/// `PosterPdfRequest` grid factors: `@Min(1)`/`@Max(10)` become 400s.
fn poster_factor(params: &[(String, String)], name: &str) -> AppResult<usize> {
    let raw = param(params, name, "2");
    let factor: i64 = raw
        .trim()
        .parse()
        .map_err(|_| AppError::BadRequest(format!("invalid {name}: {raw}")))?;
    if factor < 1 {
        return Err(AppError::BadRequest(format!("{name} must be at least 1")));
    }
    if factor > 10 {
        return Err(AppError::BadRequest(format!("{name} must not exceed 10")));
    }
    Ok(factor as usize)
}

/// `POST /api/v1/general/split-for-poster-print` — ports
/// `PosterPdfController`: each source page becomes an `xFactor × yFactor`
/// grid of target-size pages, each cell showing one slice of the page
/// imported as a Form XObject.
#[stirling_macros::endpoint]
fn poster_pdf(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let (name, bytes) = source(uploads)?;

    // Bean validation (@Min/@Max) runs before the controller body, so the
    // factors are checked before the PDF is even loaded.
    let x_factor = poster_factor(&params, "xFactor")?;
    let y_factor = poster_factor(&params, "yFactor")?;
    let right_to_left = param_bool(&params, "rightToLeft");

    let src = pdfdoc::load(&name, &bytes)?;
    let page_ids = pdfdoc::page_ids(&src);

    let (target_w, target_h) = poster_page_size(param(&params, "pageSize", "A4"))?;

    if page_ids
        .len()
        .saturating_mul(x_factor)
        .saturating_mul(y_factor)
        > MAX_POSTER_PAGES
    {
        return Err(AppError::BadRequest(format!(
            "Invalid argument: the poster grid would produce more than {MAX_POSTER_PAGES} pages"
        )));
    }

    let mut out = pdfdoc::new_document();
    let mut map = HashMap::new();
    for (form_count, &page_id) in page_ids.iter().enumerate() {
        let (form_id, view) = pdfdoc::poster_page_form(&mut out, &src, page_id, &mut map)?;
        let rotation = pdfdoc::page_rotation(&src, page_id)?;
        let (mut source_w, mut source_h) = (view[2] - view[0], view[3] - view[1]);
        if matches!(rotation, 90 | 270) {
            std::mem::swap(&mut source_w, &mut source_h);
        }
        let cell_w = source_w / x_factor as f64;
        let cell_h = source_h / y_factor as f64;
        let form_name = format!("Fm{form_count}");

        for row in 0..y_factor {
            for col in 0..x_factor {
                let actual_col = if right_to_left {
                    x_factor - 1 - col
                } else {
                    col
                };
                let crop_x = actual_col as f64 * cell_w;
                // Row 0 shows the TOP of the source page, like
                // SplitPdfBySectionsController.
                let crop_y = (y_factor - 1 - row) as f64 * cell_h;

                // Uniform scale to fit the cell into the target page,
                // centred — the same math as the Java content stream.
                let scale = (target_w / cell_w).min(target_h / cell_h);
                let offset_x = (target_w - cell_w * scale) / 2.0;
                let offset_y = (target_h - cell_h * scale) / 2.0;

                let num = |v: f64| format!("{}", v as f32);
                let content = format!(
                    "q\n1 0 0 1 {} {} cm\n{} 0 0 {} 0 0 cm\n1 0 0 1 {} {} cm\n/{} Do\nQ\n",
                    num(offset_x),
                    num(offset_y),
                    num(scale),
                    num(scale),
                    num(-crop_x),
                    num(-crop_y),
                    form_name
                );
                let content_id = out.new_object_id();
                out.objects.insert(
                    content_id,
                    Object::Stream(Stream::new(Dictionary::new(), content.into_bytes())),
                );

                let mut xobjects = Dictionary::new();
                xobjects.set(form_name.clone(), form_id);
                let mut resources = Dictionary::new();
                resources.set("XObject", Object::Dictionary(xobjects));
                let mut page = Dictionary::new();
                page.set("Type", "Page");
                page.set(
                    "MediaBox",
                    Object::Array(
                        [0.0, 0.0, target_w, target_h]
                            .iter()
                            .map(|v| Object::Real(*v as f32))
                            .collect(),
                    ),
                );
                page.set("Resources", Object::Dictionary(resources));
                page.set("Contents", content_id);
                pdfdoc::append_page(&mut out, page)?;
            }
        }
    }
    pdfdoc::carry_info(&src, &mut out);

    let base = remove_extension(Some(&name));
    let entries = vec![(format!("{base}_poster.pdf"), pdfdoc::save(&mut out)?)];
    zip_response(entries, &format!("{base}_poster.zip"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdfdoc::OutlineNode;

    #[test]
    fn size_to_bytes_handles_java_units() {
        assert_eq!(size_to_bytes("10").unwrap(), 10 << 20);
        assert_eq!(size_to_bytes("10MB").unwrap(), 10 << 20);
        assert_eq!(size_to_bytes("512KB").unwrap(), 512 << 10);
        assert_eq!(size_to_bytes("1GB").unwrap(), 1 << 30);
        assert_eq!(size_to_bytes("2TB").unwrap(), 2 << 40);
        assert_eq!(size_to_bytes("100B").unwrap(), 100);
        assert_eq!(size_to_bytes(" 5 mb ").unwrap(), 5 << 20);
        assert!(size_to_bytes("10PB").is_err());
        assert!(size_to_bytes("abc").is_err());
        assert!(size_to_bytes("").is_err());
    }

    #[test]
    fn positive_count_rejects_zero_and_junk() {
        assert_eq!(positive_count("3").unwrap(), 3);
        assert!(positive_count("0").is_err());
        assert!(positive_count("-1").is_err());
        assert!(positive_count("abc").is_err());
    }

    #[test]
    fn page_count_ranges_tile_the_document() {
        assert_eq!(page_count_ranges(10, 4), vec![(0, 3), (4, 7), (8, 9)]);
        assert_eq!(page_count_ranges(5, 5), vec![(0, 4)]);
        assert!(page_count_ranges(0, 4).is_empty());
        // Exact division leaves no short tail.
        assert_eq!(page_count_ranges(8, 4), vec![(0, 3), (4, 7)]);
    }

    #[test]
    fn doc_count_ranges_distribute_remainder() {
        // 10 pages / 3 docs → 4, 3, 3 (first docs get the extras).
        assert_eq!(doc_count_ranges(10, 3), vec![(0, 3), (4, 6), (7, 9)]);
        assert_eq!(doc_count_ranges(6, 2), vec![(0, 2), (3, 5)]);
        // More docs than pages → empty docs are skipped.
        assert_eq!(doc_count_ranges(2, 4), vec![(0, 0), (1, 1)]);
    }

    #[test]
    fn division_param_bounds_sections() {
        let p = |v: &str| vec![("horizontalDivisions".to_string(), v.to_string())];
        assert_eq!(
            division_param(&p("3"), "horizontalDivisions", "2").unwrap(),
            3
        );
        // Java binds these as int model attributes — a non-numeric value is
        // a 400, not a silent default.
        assert!(division_param(&p("x"), "horizontalDivisions", "2").is_err());
        assert!(division_param(&p("51"), "horizontalDivisions", "2").is_err());
        assert!(division_param(&p("-1"), "horizontalDivisions", "2").is_err());
    }

    #[test]
    fn select_section_pages_modes() {
        assert_eq!(
            select_section_pages("SPLIT_ALL", "", 5).unwrap(),
            (0..5).collect::<HashSet<_>>()
        );
        assert_eq!(
            select_section_pages("SPLIT_ALL_EXCEPT_FIRST", "", 5).unwrap(),
            (1..5).collect::<HashSet<_>>()
        );
        assert_eq!(
            select_section_pages("SPLIT_ALL_EXCEPT_LAST", "", 5).unwrap(),
            (0..4).collect::<HashSet<_>>()
        );
        assert_eq!(
            select_section_pages("SPLIT_ALL_EXCEPT_FIRST_AND_LAST", "", 5).unwrap(),
            (1..4).collect::<HashSet<_>>()
        );
        assert_eq!(
            select_section_pages("CUSTOM", "1,3", 5).unwrap(),
            [0usize, 2].into_iter().collect::<HashSet<_>>()
        );
        assert!(select_section_pages("CUSTOM", "", 5).is_err());
        assert!(select_section_pages("CUSTOM", "99", 5).is_err());
        assert!(select_section_pages("BOGUS", "", 5).is_err());
    }

    #[test]
    fn assign_end_pages_chains_to_next_start() {
        let mut chapters = vec![
            Chapter {
                title: "a".into(),
                start_page: 0,
                end_page: 0,
            },
            Chapter {
                title: "b".into(),
                start_page: 2,
                end_page: 0,
            },
            Chapter {
                title: "c".into(),
                start_page: 5,
                end_page: 0,
            },
        ];
        assign_end_pages(&mut chapters, 8);
        assert_eq!(chapters[0].end_page, 2);
        assert_eq!(chapters[1].end_page, 5);
        assert_eq!(chapters[2].end_page, 8);
    }

    #[test]
    fn merge_same_page_chapters_folds_zero_length_titles() {
        let mut chapters = vec![
            Chapter {
                title: "Intro".into(),
                start_page: 0,
                end_page: 0,
            },
            Chapter {
                title: "Part".into(),
                start_page: 0,
                end_page: 3,
            },
            Chapter {
                title: "Tail".into(),
                start_page: 3,
                end_page: 3,
            },
        ];
        merge_same_page_chapters(&mut chapters);
        // Zero-length chapters are removed; their titles replace the next
        // real chapter's title ("Intro " keeps its trailing space; trailing
        // "Tail" has no following chapter and is dropped).
        assert_eq!(chapters.len(), 1);
        assert_eq!(chapters[0].title, "Intro ");
    }

    #[test]
    fn clamp_range_bounds_to_document() {
        let chapter = Chapter {
            title: "x".into(),
            start_page: 2,
            end_page: 5,
        };
        assert_eq!(clamp_range(&chapter, 10), (2, 4));
        // Single-page chapter keeps its page.
        let single = Chapter {
            title: "x".into(),
            start_page: 3,
            end_page: 3,
        };
        assert_eq!(clamp_range(&single, 10), (3, 3));
        // Out-of-range start clamps to the last page.
        let over = Chapter {
            title: "x".into(),
            start_page: 20,
            end_page: 25,
        };
        assert_eq!(clamp_range(&over, 10), (9, 9));
    }

    #[test]
    fn collect_bookmarks_respects_level_and_internal_pages() {
        fn node(title: &str, page: Option<usize>, children: Vec<OutlineNode>) -> OutlineNode {
            OutlineNode {
                title: title.into(),
                page,
                children,
            }
        }
        let tree = [node(
            "Part",
            Some(0),
            vec![node("Chapter", Some(1), vec![])],
        )];
        let mut out = Vec::new();
        collect_bookmarks(&tree[0], &mut out, 0, 10);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].title, "Part");
        assert_eq!(out[1].title, "Chapter");

        // max_level=1 still descends one level (the check is on the
        // parent's level); max_level=0 keeps only the root.
        let mut out = Vec::new();
        collect_bookmarks(&tree[0], &mut out, 0, 1);
        assert_eq!(out.len(), 2);
        let mut out = Vec::new();
        collect_bookmarks(&tree[0], &mut out, 0, 0);
        assert_eq!(out.len(), 1);

        // Nodes without a page destination are skipped with their subtree.
        let detached = [node("NoDest", None, vec![node("Child", Some(0), vec![])])];
        let mut out = Vec::new();
        collect_bookmarks(&detached[0], &mut out, 0, 10);
        assert!(out.is_empty());

        // '/' is stripped from titles like Java's filename sanitising.
        let slash = [node("a/b", Some(0), vec![])];
        let mut out = Vec::new();
        collect_bookmarks(&slash[0], &mut out, 0, 10);
        assert_eq!(out[0].title, "ab");
    }

    #[test]
    fn poster_page_size_matches_java_paper_sizes() {
        let mm = 72.0 / 25.4;
        assert_eq!(poster_page_size("Letter").unwrap(), (612.0, 792.0));
        assert_eq!(poster_page_size("A4").unwrap(), (210.0 * mm, 297.0 * mm));
        assert_eq!(poster_page_size("Legal").unwrap(), (612.0, 1008.0));
        assert!(poster_page_size("B5").is_err());
    }

    #[test]
    fn poster_factor_enforces_java_validation_bounds() {
        let p = |v: &str| vec![("xFactor".to_string(), v.to_string())];
        assert_eq!(poster_factor(&p("3"), "xFactor").unwrap(), 3);
        // Non-numeric input fails Spring's int binding → 400.
        assert!(poster_factor(&p("x"), "xFactor").is_err());
        assert!(poster_factor(&p("0"), "xFactor").is_err());
        assert!(poster_factor(&p("11"), "xFactor").is_err());
        assert!(poster_factor(&p("-2"), "xFactor").is_err());
    }

    #[test]
    fn size_ranges_respects_max_bytes() {
        // A generous limit packs the whole document into one range.
        let mut pdf = crate::testkit::Pdf::new();
        for _ in 0..4 {
            pdf.add_page([0, 0, 612, 792], lopdf::Dictionary::new());
        }
        let bytes = pdf.bytes();
        let ranges = size_ranges("in.pdf", &bytes, 5, "50MB").unwrap();
        assert_eq!(ranges, vec![(0, 4)]);

        // An absurdly small limit still returns covering ranges — the
        // loop's minimum-progress guarantee must not spin forever.
        let ranges = size_ranges("in.pdf", &bytes, 5, "1B").unwrap();
        assert!(!ranges.is_empty());
        let covered: usize = ranges.iter().map(|(a, b)| b - a + 1).sum();
        assert!(covered >= 5, "ranges {ranges:?} lost pages");
    }
}
