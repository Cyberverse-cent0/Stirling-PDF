//! Shared multipart helpers for the PDF endpoints. Every migrated controller
//! takes `fileInput` (a `MultipartFile` in the Java models) plus optional
//! text parameters, so the extraction lives here once.

use std::ops::Deref;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use axum::extract::multipart::Field;
use axum::extract::Multipart;

use crate::error::{AppError, AppResult};

static MAX_PART_BYTES: OnceLock<u64> = OnceLock::new();

/// Per-part byte cap, `RUST_MAX_UPLOAD_BYTES` (default 2000 MB — Java's
/// `spring.servlet.multipart.max-file-size`). `main` installs the parsed
/// `Config::max_upload_bytes` at startup; the env read remains the fallback
/// for callers that never went through `Config`.
fn max_part_bytes() -> u64 {
    *MAX_PART_BYTES.get_or_init(|| {
        std::env::var("RUST_MAX_UPLOAD_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2000 * 1024 * 1024)
    })
}

/// Installs the configured upload cap. Must run before the first multipart
/// request; a cap already resolved from the env is not overwritten.
pub fn configure_max_part_bytes(bytes: u64) {
    let _ = MAX_PART_BYTES.set(bytes);
}

/// Streams one part into a temp file, enforcing the cap while reading so an
/// oversized upload aborts instead of buffering the whole body first.
/// Returns the byte count written.
async fn spool_part(
    field: &mut Field<'_>,
    file: &mut std::fs::File,
    label: &str,
) -> AppResult<u64> {
    use std::io::Write;
    let limit = max_part_bytes();
    let mut written = 0u64;
    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|e| AppError::BadRequest(format!("failed to read {label}: {e}")))?
    {
        written += chunk.len() as u64;
        if written > limit {
            return Err(AppError::PayloadTooLarge(format!(
                "{label} exceeds the upload size limit"
            )));
        }
        file.write_all(&chunk)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("spool upload: {e}")))?;
    }
    Ok(written)
}

/// Reads a small text parameter part into memory under the same cap.
async fn read_part_bytes(field: &mut Field<'_>, label: &str) -> AppResult<Vec<u8>> {
    let limit = max_part_bytes() as usize;
    let mut bytes = Vec::new();
    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|e| AppError::BadRequest(format!("failed to read {label}: {e}")))?
    {
        if bytes.len() + chunk.len() > limit {
            return Err(AppError::PayloadTooLarge(format!(
                "{label} exceeds the upload size limit"
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Disk-backed upload bytes. Multipart parts stream to a temp file while the
/// request is parsed — the same spool Java's `DiskFileItem` uses — so a
/// request never holds every upload in memory at once.
///
/// The `&[u8]` view materializes lazily on first access and is cached for the
/// `Upload`'s lifetime: `lopdf` parses the whole object graph into memory
/// anyway, so this bounds the parse phase without buffering every upload.
#[derive(Clone)]
pub struct UploadBytes {
    inner: Arc<UploadBytesInner>,
}

struct UploadBytesInner {
    file: tempfile::NamedTempFile,
    len: u64,
    cache: OnceLock<Vec<u8>>,
}

impl UploadBytes {
    fn new(mut file: tempfile::NamedTempFile, len: u64) -> Self {
        use std::io::Write;
        let _ = file.as_file_mut().flush();
        Self {
            inner: Arc::new(UploadBytesInner {
                file,
                len,
                cache: OnceLock::new(),
            }),
        }
    }

    /// In-memory uploads (tests, synthetic parts) still get a temp file so
    /// every `UploadBytes` has one code path.
    #[cfg(test)]
    pub fn from_vec(bytes: Vec<u8>) -> Self {
        let len = bytes.len() as u64;
        let mut file = tempfile::NamedTempFile::new().expect("tempfile for upload bytes");
        use std::io::Write;
        file.as_file_mut()
            .write_all(&bytes)
            .expect("write upload bytes");
        Self::new(file, len)
    }

    /// Length in bytes. `len()`/`is_empty()` are deliberately not defined
    /// here: calls deref to `[u8]` and keep their `usize` semantics.
    pub fn byte_len(&self) -> u64 {
        self.inner.len
    }

    /// Path of the spool file. The file is deleted when the last `UploadBytes`
    /// clone drops; callers needing it past that point must hold the clone.
    pub fn temp_path(&self) -> &Path {
        self.inner.file.path()
    }
}

impl Deref for UploadBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.inner
            .cache
            .get_or_init(|| std::fs::read(self.inner.file.path()).expect("read spooled upload"))
            .as_slice()
    }
}

impl AsRef<[u8]> for UploadBytes {
    fn as_ref(&self) -> &[u8] {
        self
    }
}

pub struct Upload {
    pub name: String,
    pub bytes: UploadBytes,
    /// The part's `Content-Type`, when the client sent one — portfolio
    /// creation copies it onto the embedded-file stream's `/Subtype`.
    pub content_type: Option<String>,
}

impl Upload {
    /// Spools a `fileInput` part to a temp file for the PDF loaders.
    pub async fn from_field(mut field: Field<'_>) -> AppResult<Self> {
        let name = field
            .file_name()
            .map(str::to_string)
            .unwrap_or_else(|| "unnamed".into());
        let content_type = field.content_type().map(str::to_string);
        let file = tempfile::NamedTempFile::new()
            .map_err(|e| AppError::Internal(anyhow::anyhow!("upload tempfile: {e}")))?;
        let mut writer = file;
        let len = spool_part(&mut field, writer.as_file_mut(), &name).await?;
        let _ = writer.as_file_mut().sync_all();
        Ok(Self {
            name,
            bytes: UploadBytes::new(writer, len),
            content_type,
        })
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
            let mut field = field;
            let bytes = read_part_bytes(&mut field, &name).await?;
            params.push((name, String::from_utf8_lossy(&bytes).into_owned()));
        }
    }
    Ok((uploads, params))
}

/// Like `receive_multipart` but also collects file parts under `extra_fields`
/// (e.g. `attachments` on add-attachments) instead of reading them as text.
/// Returns `(fileInput uploads, extra-field uploads by field name, params)`.
pub async fn receive_files_multipart(
    mut multipart: Multipart,
    extra_fields: &[&str],
) -> AppResult<(Vec<Upload>, Vec<(String, Upload)>, Vec<(String, String)>)> {
    let mut uploads = Vec::new();
    let mut extras = Vec::new();
    let mut params = Vec::new();

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(format!("multipart error: {e}")))?
    {
        let name = field.name().unwrap_or_default().to_string();
        if name == FILE_FIELD {
            uploads.push(Upload::from_field(field).await?);
        } else if extra_fields.contains(&name.as_str()) {
            extras.push((name, Upload::from_field(field).await?));
        } else {
            let mut field = field;
            let bytes = read_part_bytes(&mut field, &name).await?;
            params.push((name, String::from_utf8_lossy(&bytes).into_owned()));
        }
    }
    Ok((uploads, extras, params))
}

/// The first `fileInput` upload, or the 400 every single-file endpoint
/// returns when the part is absent.
pub fn first_upload(uploads: Vec<Upload>) -> AppResult<Upload> {
    uploads
        .into_iter()
        .next()
        .ok_or_else(|| AppError::BadRequest(format!("{FILE_FIELD} is required")))
}

/// `Filenames.toSimpleFileName`: basename of an uploaded filename.
pub fn simple_name(name: &str) -> &str {
    name.rsplit(['/', '\\']).next().unwrap_or(name)
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

/// Numeric param with the request model's default; unparsable values fall
/// back like Spring's relaxed binding does for blank fields.
pub fn param_f64(params: &[(String, String)], name: &str, fallback: f64) -> f64 {
    params
        .iter()
        .rev()
        .find(|(n, _)| n == name)
        .and_then(|(_, v)| v.trim().parse().ok())
        .unwrap_or(fallback)
}

/// Strict numeric multipart param: absent or blank yields `default`,
/// anything else must parse as `T` or the request is a 400 — Spring's
/// binding failure for a mistyped number.
pub fn param_parse<T: std::str::FromStr>(
    params: &[(String, String)],
    name: &str,
    default: T,
) -> AppResult<T> {
    let raw = param(params, name, "");
    if raw.trim().is_empty() {
        return Ok(default);
    }
    raw.trim()
        .parse()
        .map_err(|_| AppError::BadRequest(format!("invalid {name}: {raw}")))
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

    #[test]
    fn param_returns_last_occurrence() {
        let params = vec![
            ("pageNumbers".to_string(), "1-3".to_string()),
            ("other".to_string(), "x".to_string()),
            ("pageNumbers".to_string(), "5".to_string()),
        ];
        // Spring binds repeated params last-wins; `param` scans in reverse.
        assert_eq!(param(&params, "pageNumbers", ""), "5");
        assert_eq!(param(&params, "missing", "fallback"), "fallback");
        assert_eq!(param(&params, "other", ""), "x");
    }

    #[test]
    fn param_bool_accepts_spring_spellings() {
        let p = |v: &str| vec![("flag".to_string(), v.to_string())];
        for truthy in ["true", "TRUE", "1", "on", "yes", " true "] {
            assert!(param_bool(&p(truthy), "flag"), "{truthy}");
        }
        for falsy in ["false", "0", "off", "no", "", "maybe"] {
            assert!(!param_bool(&p(falsy), "flag"), "{falsy}");
        }
        assert!(!param_bool(&[], "flag"));
        // Last occurrence wins.
        let both = vec![
            ("flag".to_string(), "true".to_string()),
            ("flag".to_string(), "false".to_string()),
        ];
        assert!(!param_bool(&both, "flag"));
    }
}
