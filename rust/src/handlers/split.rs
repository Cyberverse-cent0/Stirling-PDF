//! Split endpoints (`/api/v1/general/split-*`): page-range cuts, auto
//! size/count splits, and bookmark-based chapter splits — all returning ZIP
//! archives. Runs on the `lopdf` object model via `pdfdoc`.

use std::collections::HashMap;
use std::io::{Cursor, Write};

use axum::response::Response;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::error::{AppError, AppResult};
use crate::handlers::jobs::AppState;
use crate::handlers::merge::file_response;
use crate::page_select;
use crate::pdfdoc;
use crate::upload::{output_name, param, remove_extension, Upload, FILE_FIELD};

/// Loads the first upload and returns it with its raw bytes (needed to build
/// per-segment documents).
fn source(uploads: Vec<Upload>) -> AppResult<(String, Vec<u8>)> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    Ok((upload.name, upload.bytes))
}

/// Builds a one-segment document containing `order` (0-based page indices)
/// and returns its serialised bytes.
fn segment_bytes(source_name: &str, source_bytes: &[u8], order: &[u32]) -> AppResult<Vec<u8>> {
    let mut doc = pdfdoc::load(source_name, source_bytes)?;
    pdfdoc::subset(&mut doc, order)?;
    pdfdoc::save(&mut doc)
}

fn zip_response(entries: Vec<(String, Vec<u8>)>, zip_name: &str) -> AppResult<Response> {
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
        "application/zip",
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
        if let Some(info) = &source_info {
            segment
                .trailer
                .set("Info", lopdf::Object::Dictionary(info.clone()));
        }
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
                let mut title = merged_title.trim_end().to_string();
                if title.chars().count() > 255 {
                    title = title.chars().take(253).collect::<String>() + "...";
                }
                chapter.title = title;
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
