use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;

use crate::services::process::{find_executable, run_with_timeout};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineKind {
    Ghostscript,
    Qpdf,
    Tesseract,
    Libreoffice,
    ImageMagick,
    Pandoc,
    Java,
    Python,
    Uv,
    Node,
}

impl EngineKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ghostscript => "ghostscript",
            Self::Qpdf => "qpdf",
            Self::Tesseract => "tesseract",
            Self::Libreoffice => "libreoffice",
            Self::ImageMagick => "imagemagick",
            Self::Pandoc => "pandoc",
            Self::Java => "java",
            Self::Python => "python",
            Self::Uv => "uv",
            Self::Node => "node",
        }
    }

    /// Executables probed on PATH, in order.
    pub(crate) fn candidates(self) -> &'static [&'static str] {
        match self {
            Self::Ghostscript => &["gs", "gswin64c", "gswin32c"],
            Self::Qpdf => &["qpdf"],
            Self::Tesseract => &["tesseract"],
            Self::Libreoffice => &["soffice", "libreoffice"],
            Self::ImageMagick => &["magick", "convert"],
            Self::Pandoc => &["pandoc"],
            Self::Java => &["java"],
            Self::Python => &["python3", "python"],
            Self::Uv => &["uv"],
            Self::Node => &["node"],
        }
    }

    /// Version flags tried in order; first that exits 0 wins.
    fn version_args(self) -> &'static [&'static [&'static str]] {
        match self {
            Self::Ghostscript => &[&["--version"]],
            Self::Qpdf => &[&["--version"]],
            Self::Tesseract => &[&["--version"], &["-v"]],
            Self::Libreoffice => &[&["--version"]],
            Self::ImageMagick => &[&["--version"], &["-version"]],
            Self::Pandoc => &[&["--version"]],
            Self::Java => &[&["-version"]],
            Self::Python => &[&["--version"], &["-V"]],
            Self::Uv => &[&["--version"]],
            Self::Node => &[&["--version"]],
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EngineStatus {
    pub kind: EngineKind,
    pub label: String,
    pub path: Option<PathBuf>,
    pub version: Option<String>,
    pub version_error: Option<String>,
}

impl EngineStatus {
    pub fn installed(&self) -> bool {
        self.path.is_some()
    }
}

pub const ALL_ENGINES: &[EngineKind] = &[
    EngineKind::Ghostscript,
    EngineKind::Qpdf,
    EngineKind::Tesseract,
    EngineKind::Libreoffice,
    EngineKind::ImageMagick,
    EngineKind::Pandoc,
    EngineKind::Java,
    EngineKind::Python,
    EngineKind::Uv,
    EngineKind::Node,
];

/// External tools the Rust backend shells out to. Everything else in the
/// table is dev tooling the CLI itself or adjacent workflows use.
pub const PDF_ENGINES: &[EngineKind] = &[
    EngineKind::Ghostscript,
    EngineKind::Qpdf,
    EngineKind::Tesseract,
    EngineKind::Libreoffice,
    EngineKind::ImageMagick,
];

pub fn detect(kind: EngineKind) -> EngineStatus {
    let mut status = EngineStatus {
        kind,
        label: kind.label().to_string(),
        path: None,
        version: None,
        version_error: None,
    };
    for candidate in kind.candidates() {
        if let Some(path) = find_executable(candidate) {
            status.path = Some(path.clone());
            match probe_version(&path, kind) {
                Ok(v) => status.version = Some(v),
                Err(e) => status.version_error = Some(e.to_string()),
            }
            break;
        }
    }
    status
}

pub fn detect_all() -> Vec<EngineStatus> {
    ALL_ENGINES.iter().copied().map(detect).collect()
}

fn probe_version(exe: &std::path::Path, kind: EngineKind) -> anyhow::Result<String> {
    let mut last_err = anyhow::anyhow!("no version probe succeeded");
    for args in kind.version_args() {
        let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        match run_with_timeout(exe, &argv, None, Duration::from_secs(10)) {
            Ok(out) if out.status.success() => {
                let text = String::from_utf8_lossy(&out.stdout);
                let text = if text.trim().is_empty() {
                    String::from_utf8_lossy(&out.stderr).to_string()
                } else {
                    text.to_string()
                };
                let first = text.lines().next().unwrap_or("").trim().to_string();
                if !first.is_empty() {
                    return Ok(first);
                }
            }
            Ok(out) => {
                last_err = anyhow::anyhow!("exit status {}", out.status);
            }
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}
