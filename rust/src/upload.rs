//! Shared multipart helpers for the PDF endpoints. Every migrated controller
//! takes `fileInput` (a `MultipartFile` in the Java models) plus optional
//! text parameters, so the extraction lives here once.

use axum::extract::multipart::Field;
use axum::extract::Multipart;

use crate::error::{AppError, AppResult};

pub struct Upload {
    pub name: String,
    pub bytes: Vec<u8>,
}

impl Upload {
    /// Reads a `fileInput` part into memory for the PDF loaders.
    pub async fn from_field(field: Field<'_>) -> AppResult<Self> {
        let name = field
            .file_name()
            .map(str::to_string)
            .unwrap_or_else(|| "unnamed".into());
        let bytes = field
            .bytes()
            .await
            .map_err(|e| AppError::BadRequest(format!("failed to read {name}: {e}")))?
            .to_vec();
        Ok(Self { name, bytes })
    }
}

/// Name of the file part every PDF endpoint binds (`PDFFile.fileInput`).
pub const FILE_FIELD: &str = "fileInput";

/// Reads the whole multipart body, collecting every `fileInput` upload and
/// every other field's text value. Single-file endpoints take the first
/// upload; merge takes them all.
pub async fn receive_multipart(
    mut multipart: Multipart,
) -> AppResult<(Vec<Upload>, Vec<(String, String)>)> {
    let mut uploads = Vec::new();
    let mut params = Vec::new();

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(format!("multipart error: {e}")))?
    {
        let name = field.name().unwrap_or_default().to_string();
        if name == FILE_FIELD {
            uploads.push(Upload::from_field(field).await?);
        } else {
            let text = field
                .text()
                .await
                .map_err(|e| AppError::BadRequest(format!("failed to read {name}: {e}")))?;
            params.push((name, text));
        }
    }
    Ok((uploads, params))
}

/// Looks up a text parameter; absent fields yield `fallback`, mirroring the
/// Java request models' field defaults.
pub fn param<'a>(params: &'a [(String, String)], name: &str, fallback: &'a str) -> &'a str {
    params
        .iter()
        .rev()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
        .unwrap_or(fallback)
}

/// Last-occurrence-wins boolean param; `true`/`1`/`on`/`yes` count, matching
/// how the migrated handlers have interpreted Spring's Boolean binding.
pub fn param_bool(params: &[(String, String)], name: &str) -> bool {
    params
        .iter()
        .rev()
        .find(|(n, _)| n == name)
        .is_some_and(|(_, v)| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "true" | "1" | "on" | "yes"
            )
        })
}

/// Port of `GeneralUtils.removeExtension`: `None` maps to `"default"`, a dot
/// only counts when it is neither first nor last character.
pub fn remove_extension(filename: Option<&str>) -> String {
    let Some(filename) = filename else {
        return "default".into();
    };
    if filename.is_empty() {
        return String::new();
    }
    match filename.rfind('.') {
        Some(dot) if dot > 0 && dot < filename.len() - 1 => filename[..dot].to_string(),
        _ => filename.to_string(),
    }
}

/// `<base><suffix>` output name, the shape of every
/// `GeneralUtils.generateFilename(file, "_<tool>.pdf")` call. `suffix`
/// includes the extension, e.g. `"_rotated.pdf"`.
pub fn output_name(original: Option<&str>, suffix: &str) -> String {
    format!("{}{suffix}", remove_extension(original))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_extensions_like_java() {
        assert_eq!(remove_extension(Some("report.final.pdf")), "report.final");
        assert_eq!(remove_extension(Some("noext")), "noext");
        assert_eq!(remove_extension(Some(".hidden")), ".hidden");
        assert_eq!(remove_extension(Some("file.")), "file.");
        assert_eq!(remove_extension(None), "default");
        assert_eq!(remove_extension(Some("")), "");
    }

    #[test]
    fn builds_output_names() {
        assert_eq!(output_name(Some("a.pdf"), "_rotated.pdf"), "a_rotated.pdf");
        assert_eq!(output_name(None, "_rotated.pdf"), "default_rotated.pdf");
    }
}
