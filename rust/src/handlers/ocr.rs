//! `/api/v1/misc/ocr-pdf` — `OCRController`: OCRmyPDF sidecar when the
//! `OCRmyPDF` group is enabled, per-page Tesseract fallback otherwise.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use axum::response::Response;
use lopdf::Document;

use crate::convert::{self, EngineSpec};
use crate::endpointconfig;
use crate::error::{AppError, AppResult};
use crate::handlers::jobs::AppState;
use crate::handlers::merge::pdf_response;
use crate::handlers::misc::BlankRasterizer;
use crate::pdfdoc;
use crate::settings;
use crate::text;
use crate::upload::{param, param_bool, remove_extension, simple_name, Upload, FILE_FIELD};

/// `OCRController.OCR_RENDER_DPI` — the Tesseract fallback rasterises at
/// 300 DPI (Java clamps to `system.maxDPI`, which has no Rust setting).
const OCR_RENDER_DPI: u32 = 300;
/// `OCRController.OCRMYPDF_PRIOR_OCR_FOUND` — ocrmypdf's exit code when the
/// input already carries a text layer.
const OCRMYPDF_PRIOR_OCR_FOUND: i32 = 6;
const ENGINE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// `POST /api/v1/misc/ocr-pdf`.
///
/// `languages` arrives as repeated multipart fields like Java's
/// `List<String>` binding; absent means the model default `["eng"]`.
#[stirling_macros::endpoint]
fn ocr_pdf(
    _state: AppState,
    _query: HashMap<String, String>,
    uploads: Vec<Upload>,
    params: Vec<(String, String)>,
) -> AppResult<Response> {
    let Some(upload) = uploads.into_iter().next() else {
        return Err(AppError::BadRequest(format!("{FILE_FIELD} is required")));
    };
    let selected: Vec<String> = params
        .iter()
        .filter(|(name, _)| name == "languages")
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect();
    let selected = if selected.is_empty() {
        vec!["eng".to_string()]
    } else {
        selected
    };

    let sidecar = param_bool(&params, "sidecar");
    let deskew = param_bool(&params, "deskew");
    let rotate_pages = param_bool(&params, "rotatePages");
    let clean = param_bool(&params, "clean");
    let clean_final = param_bool(&params, "cleanFinal");
    let remove_images_after = param_bool(&params, "removeImagesAfter");
    let ocr_type = param(&params, "ocrType", "skip-text").to_string();
    let ocr_render_type = param(&params, "ocrRenderType", "hocr").to_string();

    if ocr_render_type != "hocr" && ocr_render_type != "sandwich" {
        return Err(AppError::BadRequest(
            "Invalid OCR render type. Must be 'hocr' or 'sandwich'".into(),
        ));
    }

    // Java filters the request against the `.traineddata` files in the
    // resolved tessdata dir; an empty intersection is a 400.
    let available = tessdata_languages();
    let selected: Vec<String> = selected
        .into_iter()
        .filter(|lang| available.iter().any(|a| a == lang))
        .collect();
    if selected.is_empty() {
        return Err(AppError::BadRequest(
            "None of the selected OCR languages are installed".into(),
        ));
    }

    let dir =
        tempfile::tempdir().map_err(|e| AppError::Internal(anyhow::anyhow!("tempdir: {e}")))?;
    let input = dir.path().join("input.pdf");
    let output = dir.path().join("output.pdf");
    std::fs::write(&input, upload.bytes.as_ref())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("write input: {e}")))?;

    let sidecar_txt = sidecar.then(|| dir.path().join("sidecar.txt"));
    if endpointconfig::is_group_enabled("OCRmyPDF") {
        ocrmypdf(
            &input,
            &output,
            dir.path(),
            OcrOptions {
                sidecar_txt: sidecar_txt.as_deref(),
                languages: &selected,
                ocr_type: &ocr_type,
                ocr_render_type: &ocr_render_type,
            },
            OcrFlags {
                deskew,
                rotate_pages,
                clean,
                clean_final,
                remove_images_after,
            },
        )?;
    } else if endpointconfig::is_group_enabled("tesseract") {
        tesseract_fallback(
            &upload.name,
            &input,
            &output,
            dir.path(),
            &selected,
            &ocr_type,
        )?;
    } else {
        // The endpoint guard already 403s when both groups are disabled;
        // reaching here means a probe raced a removal.
        return Err(AppError::Internal(anyhow::anyhow!(
            "OCR tools are not installed"
        )));
    }

    let base = remove_extension(Some(simple_name(&upload.name)));
    let pdf_name = format!("{base}_OCR.pdf");
    if let Some(txt_path) = sidecar_txt {
        let pdf = std::fs::read(&output)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("read ocr output: {e}")))?;
        let txt = std::fs::read(&txt_path).unwrap_or_default();
        let entries = vec![(pdf_name.clone(), pdf), (format!("{base}_OCR.txt"), txt)];
        return crate::handlers::split::zip_response(entries, &format!("{base}_OCR.zip"));
    }
    let pdf = std::fs::read(&output)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("read ocr output: {e}")))?;
    Ok(pdf_response(pdf, &pdf_name))
}

struct OcrFlags {
    deskew: bool,
    rotate_pages: bool,
    clean: bool,
    clean_final: bool,
    remove_images_after: bool,
}

/// `processWithOcrMyPdf`: the full flag mapping, the multiprocessing
/// `--jobs 1` retry, exit-6 → 400, and the `-dFILTERIMAGE` post-pass.
struct OcrOptions<'a> {
    sidecar_txt: Option<&'a Path>,
    languages: &'a [String],
    ocr_type: &'a str,
    ocr_render_type: &'a str,
}

fn ocrmypdf(
    input: &Path,
    output: &Path,
    dir: &Path,
    options: OcrOptions<'_>,
    flags: OcrFlags,
) -> AppResult<()> {
    let OcrOptions {
        sidecar_txt,
        languages,
        ocr_type,
        ocr_render_type,
    } = options;
    let program = settings::engine_program("ocrmypdf");
    let mut spec = EngineSpec::new(&program)
        .args([
            "--verbose".to_string(),
            "2".to_string(),
            "--output-type".to_string(),
            "pdf".to_string(),
            "--pdf-renderer".to_string(),
            ocr_render_type.to_string(),
        ])
        .timeout(ENGINE_TIMEOUT);
    if let Some(txt) = sidecar_txt {
        spec = spec.args(["--sidecar".to_string(), txt.display().to_string()]);
    }
    if flags.deskew {
        spec = spec.arg("--deskew");
    }
    if flags.rotate_pages {
        spec = spec.arg("--rotate-pages");
    }
    if flags.clean {
        spec = spec.arg("--clean");
    }
    if flags.clean_final {
        spec = spec.arg("--clean-final");
    }
    if !ocr_type.is_empty() {
        spec = spec.arg(if ocr_type == "force-ocr" {
            "--force-ocr"
        } else {
            "--skip-text"
        });
    }
    spec = spec.arg("--invalidate-digital-signatures").args([
        "--language".to_string(),
        languages.join("+"),
        input.display().to_string(),
        output.display().to_string(),
    ]);

    let mut result = run_ocrmypdf(&spec);
    let messages = engine_messages(&result);
    if result.as_ref().map(|r| r.code != Some(0)).unwrap_or(true)
        && messages.contains("multiprocessing/synchronize.py")
        && messages.contains("OSError: [Errno 38] Function not implemented")
    {
        // ocrmypdf's multiprocessing fallback fails on kernels without
        // `futex` waiters; Java retries single-threaded the same way.
        let mut retry_args = spec.args.clone();
        retry_args.extend(["--jobs".to_string(), "1".to_string()]);
        let retry = EngineSpec {
            program: spec.program.clone(),
            args: retry_args,
            timeout: spec.timeout,
            path_dirs: spec.path_dirs.clone(),
            cwd: spec.cwd.clone(),
        };
        result = run_ocrmypdf(&retry);
    }
    let result = result?;
    match result.code {
        Some(OCRMYPDF_PRIOR_OCR_FOUND) => {
            return Err(AppError::BadRequest(
                "the PDF already contains text. Use ocrType 'skip-text' to OCR only pages \
                 without text, or 'force-ocr' to redo OCR on every page."
                    .into(),
            ));
        }
        Some(0) if !result.timed_out => {}
        _ => {
            return Err(AppError::Internal(anyhow::anyhow!(
                "OCRmyPDF failed with return code: {}",
                result.code.unwrap_or(-1)
            )));
        }
    }

    if flags.remove_images_after {
        let stripped = dir.join("output_no_images.pdf");
        let gs = EngineSpec::new(&settings::engine_program("gs"))
            .args([
                "-sDEVICE=pdfwrite".to_string(),
                "-dFILTERIMAGE".to_string(),
                "-o".to_string(),
                stripped.display().to_string(),
                output.display().to_string(),
            ])
            .timeout(Duration::from_secs(300));
        let out = convert::run(&gs)?;
        if out.code == Some(0) && !out.timed_out {
            std::fs::rename(&stripped, output)
                .map_err(|e| AppError::Internal(anyhow::anyhow!("replace output: {e}")))?;
        }
    }
    Ok(())
}

fn run_ocrmypdf(spec: &EngineSpec) -> AppResult<convert::EngineOutput> {
    convert::run(spec)
}

fn engine_messages(out: &AppResult<convert::EngineOutput>) -> String {
    out.as_ref()
        .map(|r| {
            format!(
                "{}{}",
                String::from_utf8_lossy(&r.stdout),
                String::from_utf8_lossy(&r.stderr)
            )
        })
        .unwrap_or_default()
}

/// `processWithTesseract`: render each page at 300 DPI, OCR the ones
/// `ocrType` selects, keep the rest as original pages, merge in order.
/// Without Ghostscript there is no rasteriser — unrenderable pages fall
/// back to their original content like Java's missing-output fallback.
fn tesseract_fallback(
    name: &str,
    input: &Path,
    output: &Path,
    dir: &Path,
    languages: &[String],
    ocr_type: &str,
) -> AppResult<()> {
    let document = pdfdoc::load(
        name,
        &std::fs::read(input)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("read input: {e}")))?,
    )?;
    let page_ids = pdfdoc::page_ids(&document);
    let rasterizer = BlankRasterizer::probe(&std::fs::read(input).unwrap_or_default());
    let tesseract = settings::engine_program("tesseract");
    let lang_arg = languages.join("+");

    let mut dst = pdfdoc::new_document();
    for (index, &page_id) in page_ids.iter().enumerate() {
        let has_text = text::page_items(&document, page_id)
            .iter()
            .any(|item| !item.text.trim().is_empty());
        let should_ocr = match ocr_type {
            "skip-text" => !has_text,
            _ => true,
        };
        let mut page_doc = None;
        if should_ocr {
            if let Some(png) = rasterizer
                .as_ref()
                .and_then(|r| r.render(index as u32 + 1, OCR_RENDER_DPI))
            {
                let image_path = dir.join(format!("page_{index}.png"));
                let base = dir.join(format!("page_{index}"));
                if png.save(&image_path).is_ok()
                    && tesseract_page(&tesseract, &image_path, &base, &lang_arg)?
                {
                    page_doc = Document::load(base.with_extension("pdf"))
                        .ok()
                        .map(|mut d| {
                            pdfdoc::touch_loaded_info(&mut d);
                            d
                        });
                }
            }
        }
        let mut src = match page_doc {
            Some(d) => d,
            None => {
                // Original page: Java saves it standalone then merges;
                // importing straight from the source document is the same
                // page set in order.
                let mut d = document.clone();
                pdfdoc::subset(&mut d, &[index as u32])?;
                d
            }
        };
        pdfdoc::import_pages(&mut dst, &mut src, &[0])?;
    }
    pdfdoc::touch_loaded_info(&mut dst);
    std::fs::write(output, pdfdoc::save(&mut dst)?)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("write output: {e}")))?;
    Ok(())
}

/// One `tesseract <img> <base> -l <langs> pdf` invocation; `Ok(false)`
/// when tesseract exits non-zero or produces no PDF — the caller keeps
/// the original page, matching Java's missing-output fallback.
fn tesseract_page(program: &str, image: &Path, base: &Path, languages: &str) -> AppResult<bool> {
    let out = convert::run(
        &EngineSpec::new(program)
            .args([
                image.display().to_string(),
                base.display().to_string(),
                "-l".to_string(),
                languages.to_string(),
                "pdf".to_string(),
            ])
            .timeout(Duration::from_secs(300)),
    )?;
    if out.timed_out || out.code != Some(0) {
        return Err(AppError::Internal(anyhow::anyhow!(
            "Tesseract command failed with exit code: {}",
            out.code.unwrap_or(-1)
        )));
    }
    Ok(base.with_extension("pdf").exists())
}

/// Top-level `*.traineddata` stems in the resolved tessdata dir, minus
/// `osd` — the same list `getAvailableTesseractLanguages` builds.
fn tessdata_languages() -> Vec<String> {
    let settings = settings::current();
    let dir = std::env::var("SYSTEM_TESSDATADIR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            (!settings.system.tessdata_dir.trim().is_empty())
                .then(|| settings.system.tessdata_dir.clone())
        })
        .or_else(|| std::env::var("TESSDATA_PREFIX").ok())
        .unwrap_or_else(|| "/usr/share/tesseract-ocr/5/tessdata".to_string());
    languages_in(Path::new(&dir))
}

fn languages_in(dir: &Path) -> Vec<String> {
    let mut languages = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".traineddata") {
                let lang = name.replace(".traineddata", "");
                if !lang.eq_ignore_ascii_case("osd") {
                    languages.push(lang);
                }
            }
        }
    }
    languages
}

/// `io.github.pixee.security.Filenames.toSimpleFileName` + extension strip.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages_in_lists_traineddata_stems() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("eng.traineddata"), b"").unwrap();
        std::fs::write(dir.path().join("osd.traineddata"), b"").unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"").unwrap();
        assert_eq!(languages_in(dir.path()), vec!["eng".to_string()]);
    }
}
