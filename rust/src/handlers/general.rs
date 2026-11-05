//! Ports of the stateless `GeneralApi` endpoints migrated so far:
//! `RotationController.rotatePDF`, `RearrangePagesPDFController` page
//! deletion/reordering, and `RemoveImagesController.removeImages`.

use std::collections::{HashMap, HashSet};

use axum::response::Response;
use lopdf::{Document, Object, ObjectId};

use crate::error::{AppError, AppResult};
use crate::handlers::jobs::AppState;
use crate::handlers::merge::pdf_response;
use crate::page_select;
use crate::pdfdoc;
use crate::upload::{output_name, param, Upload, FILE_FIELD};

/// `POST /api/v1/general/rotate-pdf` — adds `angle` (a multiple of 90) to
/// every page's existing rotation, like `PDPage.setRotation(getRotation()+a)`.
#[stirling_macros::endpoint]
fn rotate_pdf(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };

    // RotatePDFRequest.angle defaults to 90.
    let angle_raw = param(&params, "angle", "90");
    let angle: i64 = angle_raw
        .trim()
        .parse()
        .map_err(|_| AppError::BadRequest(format!("invalid angle: {angle_raw}")))?;
    if angle % 90 != 0 {
        return Err(AppError::BadRequest(
            "Angle must be a multiple of 90".into(),
        ));
    }

    let mut document = pdfdoc::load(&upload.name, &upload.bytes)?;
    for page_id in pdfdoc::page_ids(&document) {
        let current = pdfdoc::rotation(&document, page_id)?;
        pdfdoc::set_rotation(&mut document, page_id, current + angle)?;
    }
    let bytes = pdfdoc::save(&mut document)?;
    Ok(pdf_response(
        bytes,
        &output_name(Some(&upload.name), "_rotated.pdf"),
    ))
}

/// `POST /api/v1/general/remove-pages` — deletes the pages selected by
/// `pageNumbers` (`1,3,5-9`, `all`, `an+b`), keeping at least one page.
#[stirling_macros::endpoint]
fn remove_pages(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    // PDFWithPageNums.pageNumbers defaults to "all".
    let page_numbers = param(&params, "pageNumbers", "all").to_string();

    let mut document = pdfdoc::load(&upload.name, &upload.bytes)?;

    let total_pages = pdfdoc::page_ids(&document).len();
    page_select::require_valid_page_selection(&page_numbers, total_pages)
        .map_err(AppError::BadRequest)?;
    let mut indices =
        page_select::parse_page_list(&page_numbers, total_pages).map_err(AppError::BadRequest)?;

    // Java compares the de-duplicated count: removing every page is rejected.
    indices.sort_unstable_by(|a, b| b.cmp(a));
    indices.dedup();
    if indices.len() >= total_pages {
        return Err(AppError::BadRequest(
            "Invalid argument: pageNumbers must list the pages to remove and leave at least one page"
                .into(),
        ));
    }

    // Java's FormUtils.pruneOrphanedFormFields is not ported: fields whose
    // widgets sat on removed pages stay in the AcroForm as orphans.
    let removed: HashSet<usize> = indices.into_iter().collect();
    let keep: Vec<u32> = (0..total_pages as u32)
        .filter(|i| !removed.contains(&(*i as usize)))
        .collect();
    pdfdoc::subset(&mut document, &keep)?;

    let bytes = pdfdoc::save(&mut document)?;
    Ok(pdf_response(
        bytes,
        &output_name(Some(&upload.name), "_removed_pages.pdf"),
    ))
}

/// `POST /api/v1/general/rearrange-pages` — builds the output from
/// `pageNumbers` (pages to keep, in order) or a `customMode` preset.
#[stirling_macros::endpoint]
fn rearrange_pages(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    let page_numbers = param(&params, "pageNumbers", "all").to_string();
    let custom_mode = param(&params, "customMode", "");

    let mut document = pdfdoc::load(&upload.name, &upload.bytes)?;

    let total_pages = pdfdoc::page_ids(&document).len();
    let order = if !custom_mode.is_empty() && !custom_mode.eq_ignore_ascii_case("custom") {
        preset_order(custom_mode, total_pages, &page_numbers)?
    } else {
        page_select::require_valid_page_selection(&page_numbers, total_pages)
            .map_err(AppError::BadRequest)?;
        page_select::parse_page_list(&page_numbers, total_pages)
            .map_err(AppError::BadRequest)?
            .into_iter()
            .map(|i| i as u32)
            .collect()
    };

    if order.len() > 100_000 {
        return Err(AppError::BadRequest(
            "Invalid argument: rearranged document would exceed 100000 pages".into(),
        ));
    }

    // In-place rebuild keeps the AcroForm and outlines, so this no longer
    // loses form data the way the page-import backends did.
    pdfdoc::subset(&mut document, &order)?;
    let bytes = pdfdoc::save(&mut document)?;
    Ok(pdf_response(
        bytes,
        &output_name(Some(&upload.name), "_rearranged.pdf"),
    ))
}

/// Ports of the `RearrangePagesPDFController` preset orderings; indices are
/// 0-based source pages, repeats allowed (DUPLICATE).
fn preset_order(mode: &str, total_pages: usize, page_numbers: &str) -> AppResult<Vec<u32>> {
    let total = total_pages as u32;
    let pages: Vec<u32> = match mode.to_ascii_uppercase().as_str() {
        "REVERSE_ORDER" => (0..total).rev().collect(),
        "DUPLEX_SORT" => {
            let half = total_pages.div_ceil(2);
            let mut order = Vec::with_capacity(total_pages);
            for i in 1..=half {
                order.push((i - 1) as u32);
                if i <= total_pages - half {
                    order.push((total_pages - i) as u32);
                }
            }
            order
        }
        "BOOKLET_SORT" => (0..total_pages / 2)
            .flat_map(|i| [i as u32, (total_pages - i - 1) as u32])
            .collect(),
        "SIDE_STITCH_BOOKLET_SORT" => (0..total_pages.div_ceil(4))
            .flat_map(|i| {
                let begin = i * 4;
                [
                    (begin + 3).min(total_pages - 1) as u32,
                    begin.min(total_pages - 1) as u32,
                    (begin + 1).min(total_pages - 1) as u32,
                    (begin + 2).min(total_pages - 1) as u32,
                ]
            })
            .collect(),
        "ODD_EVEN_SPLIT" => (1..=total_pages)
            .step_by(2)
            .chain((2..=total_pages).step_by(2))
            .map(|i| (i - 1) as u32)
            .collect(),
        "REMOVE_FIRST" => {
            if total_pages <= 1 {
                Vec::new()
            } else {
                (1..total_pages).map(|i| i as u32).collect()
            }
        }
        "REMOVE_LAST" => (0..total_pages.saturating_sub(1))
            .map(|i| i as u32)
            .collect(),
        "REMOVE_FIRST_AND_LAST" => {
            if total_pages <= 2 {
                Vec::new()
            } else {
                (1..total_pages - 1).map(|i| i as u32).collect()
            }
        }
        "DUPLICATE" => duplicate_order(total_pages, page_numbers)?,
        _ => {
            return Err(AppError::BadRequest(format!(
                "Invalid argument: customMode must be one of {:?}",
                [
                    "CUSTOM",
                    "REVERSE_ORDER",
                    "DUPLEX_SORT",
                    "BOOKLET_SORT",
                    "SIDE_STITCH_BOOKLET_SORT",
                    "ODD_EVEN_SPLIT",
                    "REMOVE_FIRST",
                    "REMOVE_LAST",
                    "REMOVE_FIRST_AND_LAST",
                    "DUPLICATE"
                ]
            )))
        }
    };
    Ok(pages)
}

/// Java `duplicate()`: `pageNumbers` is the copy count (default 2), and the
/// result repeats every page that many times.
fn duplicate_order(total_pages: usize, page_numbers: &str) -> AppResult<Vec<u32>> {
    // Java parses with Integer.parseInt: non-numeric and overflowing values
    // fall back to the default of 2.
    let duplicate_count = match page_numbers.trim().parse::<i32>() {
        Ok(count) if count >= 1 => count as i64,
        _ => 2,
    };
    let max_duplicate_count = 100usize.max(total_pages.saturating_mul(3));
    if duplicate_count > max_duplicate_count as i64 {
        return Err(AppError::BadRequest(format!(
            "Invalid duplicateCount format: must not exceed {max_duplicate_count}"
        )));
    }
    if total_pages as i64 * duplicate_count > 100_000 {
        return Err(AppError::BadRequest(
            "Invalid argument: duplicating would produce more than 100000 pages".into(),
        ));
    }
    Ok((0..total_pages as u32)
        .flat_map(|page| std::iter::repeat_n(page, duplicate_count as usize))
        .collect())
}

/// `POST /api/v1/general/remove-image-pdf` — deletes every `/Subtype
/// /Image` XObject from each page's `/Resources`, recursing into `/Form`
/// XObjects like `RemoveImagesController.removeImagesFromResources`.
///
/// Divergence: Java logs a warning and keeps an XObject whose dictionary
/// can't be read; here unresolvable entries are kept silently. Inline
/// images (`BI`/`EI` in content streams) are untouched, same as Java.
#[stirling_macros::endpoint]
fn remove_images(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    _params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };

    let mut document = pdfdoc::load(&upload.name, &upload.bytes)?;
    let mut visited = HashSet::new();
    for page_id in pdfdoc::page_ids(&document) {
        let Some((holder, resources)) = pdfdoc::page_resources(&document, page_id)? else {
            continue;
        };
        let Some(cleaned) = clean_resources(&mut document, &resources, &mut visited) else {
            continue;
        };
        match &resources {
            Object::Reference(id) => {
                document.objects.insert(*id, cleaned);
            }
            _ => {
                if let Ok(holder_dict) = document.get_dictionary_mut(holder) {
                    holder_dict.set(b"Resources", cleaned);
                }
            }
        }
    }

    let bytes = pdfdoc::save(&mut document)?;
    Ok(pdf_response(
        bytes,
        &output_name(Some(&upload.name), "_images_removed.pdf"),
    ))
}

/// Removes `/Subtype /Image` entries from a `/Resources` dictionary,
/// recursing into `/Form` XObjects. Returns the cleaned object for the
/// caller to store, or `None` when there is nothing to process: non-dict
/// values, dictionaries without `/XObject`, and resource dicts already in
/// `visited` — the dedupe of shared indirect dicts Java does with an
/// identity set.
fn clean_resources(
    doc: &mut Document,
    resources: &Object,
    visited: &mut HashSet<ObjectId>,
) -> Option<Object> {
    if let Object::Reference(id) = resources {
        if !visited.insert(*id) {
            return None;
        }
    }
    let mut dict_obj = pdfdoc::resolve_owned(doc, resources)?;
    let xobjects = pdfdoc::dict_of(&dict_obj)?.get(b"XObject").ok()?.clone();
    let mut xobject_obj = pdfdoc::resolve_owned(doc, &xobjects)?;
    let xobject_dict = pdfdoc::dict_mut(&mut xobject_obj)?;

    // Rebuilt in order rather than `remove`d in place: `Dictionary::remove`
    // swap-removes, which would reorder the XObject dictionary.
    let mut kept: Vec<(Vec<u8>, Object)> = Vec::with_capacity(xobject_dict.len());
    for (name, entry) in xobject_dict.iter() {
        let name = name.clone();
        let entry = entry.clone();
        let Some(entry_dict) = pdfdoc::peek_dict(doc, &entry) else {
            kept.push((name, entry));
            continue;
        };
        // Owned so the borrow of `doc`/`entry` ends before the recursive
        // mutable pass below.
        let subtype = entry_dict
            .get(b"Subtype")
            .ok()
            .and_then(|o| o.as_name().ok())
            .map(<[u8]>::to_vec);
        match subtype.as_deref() {
            Some(b"Image") => {}
            Some(b"Form") => {
                let Some(mut form_obj) = pdfdoc::resolve_owned(doc, &entry) else {
                    kept.push((name, entry));
                    continue;
                };
                let Some(form_resources) =
                    pdfdoc::dict_mut(&mut form_obj).and_then(|d| d.get(b"Resources").ok().cloned())
                else {
                    kept.push((name, entry));
                    continue;
                };
                match clean_resources(doc, &form_resources, visited) {
                    Some(cleaned) => {
                        pdfdoc::store_object(
                            doc,
                            pdfdoc::dict_mut(&mut form_obj)?,
                            b"Resources",
                            &form_resources,
                            cleaned,
                        );
                        match &entry {
                            Object::Reference(id) => {
                                doc.objects.insert(*id, form_obj);
                                kept.push((name, entry));
                            }
                            _ => kept.push((name, form_obj)),
                        }
                    }
                    None => kept.push((name, entry)),
                }
            }
            _ => kept.push((name, entry)),
        }
    }
    *xobject_dict = kept.into_iter().collect();

    pdfdoc::store_object(
        doc,
        pdfdoc::dict_mut(&mut dict_obj)?,
        b"XObject",
        &xobjects,
        xobject_obj,
    );
    Some(dict_obj)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit;
    use lopdf::{dictionary, Document};

    fn upload(bytes: Vec<u8>) -> Vec<Upload> {
        vec![Upload {
            name: "in.pdf".into(),
            bytes,
        }]
    }

    /// `/Subtype` name of an XObject entry, following references.
    fn xobject_subtype(doc: &Document, entry: &Object) -> Option<Vec<u8>> {
        pdfdoc::peek_dict(doc, entry)
            .and_then(|d| d.get(b"Subtype").ok())
            .and_then(|s| s.as_name().ok())
            .map(<[u8]>::to_vec)
    }

    /// XObject name → subtype under a `/Resources` value.
    fn xobject_kinds(doc: &Document, resources: &Object) -> Vec<(String, Vec<u8>)> {
        let Some(resources_dict) = pdfdoc::peek_dict(doc, resources) else {
            return Vec::new();
        };
        let Some(xobjects) = resources_dict.get(b"XObject").ok() else {
            return Vec::new();
        };
        let (_, resolved) = doc.dereference(xobjects).unwrap();
        let Some(xobject_dict) = pdfdoc::dict_of(resolved) else {
            return Vec::new();
        };
        xobject_dict
            .iter()
            .map(|(name, entry)| {
                (
                    String::from_utf8_lossy(name).into_owned(),
                    xobject_subtype(doc, entry).unwrap_or_default(),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn removes_images_and_recurses_into_form_xobjects() {
        let mut pdf = testkit::Pdf::new();
        let im1 = pdf.add_stream(testkit::image_xobject(), &[255, 0, 0]);
        let im_nested = pdf.add_stream(testkit::image_xobject(), &[0, 255, 0]);
        let ps = pdf.add_stream(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "PS",
            },
            b"",
        );
        let form = pdf.add_stream(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Form",
                "BBox" => Object::Array(vec![0.into(), 0.into(), 10.into(), 10.into()]),
                "Resources" => dictionary! {
                    "XObject" => dictionary! {
                        "ImN" => Object::Reference(im_nested),
                    },
                },
            },
            b"q Q",
        );
        pdf.edit_page(|page| {
            page.set(
                "Resources",
                dictionary! {
                    "XObject" => dictionary! {
                        "Im1" => Object::Reference(im1),
                        "Fm1" => Object::Reference(form),
                        "Ps1" => Object::Reference(ps),
                    },
                },
            );
        });
        let bytes = pdf.bytes();

        let response =
            remove_images_work(AppState::new(None), HashMap::new(), upload(bytes), vec![]).unwrap();
        let doc = testkit::response_document(response).await;

        let page = doc.get_dictionary(pdf.page).unwrap();
        let resources = page.get(b"Resources").unwrap();
        let kinds = xobject_kinds(&doc, resources);
        assert_eq!(
            kinds,
            vec![
                ("Fm1".to_string(), b"Form".to_vec()),
                ("Ps1".to_string(), b"PS".to_vec()),
            ]
        );

        // The form XObject survives but its own image is gone.
        let form_entry = pdfdoc::peek_dict(&doc, resources)
            .and_then(|r| r.get(b"XObject").ok())
            .map(|xo| {
                let (_, resolved) = doc.dereference(xo).unwrap();
                pdfdoc::dict_of(resolved)
                    .unwrap()
                    .get(b"Fm1")
                    .unwrap()
                    .clone()
            })
            .unwrap();
        let form_dict = pdfdoc::peek_dict(&doc, &form_entry).unwrap();
        let form_resources = form_dict.get(b"Resources").unwrap();
        assert!(xobject_kinds(&doc, form_resources).is_empty());
    }

    #[tokio::test]
    async fn leaves_documents_without_images_untouched() {
        let mut pdf = testkit::Pdf::new();
        let bytes = pdf.bytes();

        let response =
            remove_images_work(AppState::new(None), HashMap::new(), upload(bytes), vec![]).unwrap();
        let doc = testkit::response_document(response).await;
        assert_eq!(doc.get_pages().len(), 1);
    }
}

/// `POST /api/v1/general/pdf-to-single-page` — stacks every page's
/// appearance into one tall page, like `ToSinglePageController`.
#[stirling_macros::endpoint]
fn pdf_to_single_page(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    _params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };

    let document = pdfdoc::load(&upload.name, &upload.bytes)?;
    let mut single = pdfdoc::to_single_page(&document)?;
    let bytes = pdfdoc::save(&mut single)?;
    Ok(pdf_response(
        bytes,
        &output_name(Some(&upload.name), "_singlePage.pdf"),
    ))
}
