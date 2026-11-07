//! Package-manager specs for `stirling engines install`. Every entry is
//! best-effort: distros rename packages often, so an unknown engine or
//! manager prints manual instructions instead of guessing.

use crate::services::engine_detector::{EngineKind, ALL_ENGINES};

/// `(manager argv prefix, package)` — the caller joins them into the
/// command it prints and runs. `None` means no known mapping.
pub fn install_spec(kind: EngineKind) -> Option<Vec<String>> {
    if cfg!(windows) {
        return winget_spec(kind).map(|id| vec!["winget".into(), "install".into(), id.into()]);
    }
    if cfg!(target_os = "macos") {
        return brew_spec(kind).map(brew_args);
    }
    linux_spec(kind)
}

fn brew_args(pkg: &str) -> Vec<String> {
    let mut args = vec!["brew".to_string(), "install".to_string()];
    if let Some(cask) = pkg.strip_prefix("--cask ") {
        args.push("--cask".to_string());
        args.push(cask.to_string());
    } else {
        args.push(pkg.to_string());
    }
    args
}

#[cfg(target_os = "linux")]
fn linux_spec(kind: EngineKind) -> Option<Vec<String>> {
    let release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let ids = os_release_ids(&release);
    let (manager, install_arg) = if ids.iter().any(|id| {
        matches!(
            id.as_str(),
            "alpine" | "postmarketos" | "gentoo" | "arch" | "manjaro" | "endeavouros"
        )
    }) {
        // apk covers Alpine; the arch-family names fall through to pacman
        // only when 'apk' is absent — handled below.
        if ids.iter().any(|id| id == "alpine" || id == "postmarketos") {
            ("apk", "add")
        } else {
            return pacman_spec(kind);
        }
    } else if ids.iter().any(|id| {
        matches!(
            id.as_str(),
            "debian" | "ubuntu" | "linuxmint" | "pop" | "elementary" | "kali" | "raspbian"
        )
    }) {
        ("apt-get", "install")
    } else if ids.iter().any(|id| {
        matches!(
            id.as_str(),
            "fedora" | "rhel" | "centos" | "rocky" | "almalinux" | "ol" | "amzn" | "nobara"
        )
    }) {
        ("dnf", "install")
    } else {
        return None;
    };
    let pkg = match (manager, kind) {
        ("apt-get", EngineKind::Ghostscript) => "ghostscript",
        ("apt-get", EngineKind::Qpdf) => "qpdf",
        ("apt-get", EngineKind::Tesseract) => "tesseract-ocr",
        ("apt-get", EngineKind::Libreoffice) => "libreoffice",
        ("apt-get", EngineKind::ImageMagick) => "imagemagick",
        ("apt-get", EngineKind::Pandoc) => "pandoc",
        ("apt-get", EngineKind::Java) => "default-jre-headless",
        ("apt-get", EngineKind::Python) => "python3",
        ("apt-get", EngineKind::Node) => "nodejs",
        ("apt-get", _) => return None,
        ("dnf", EngineKind::Ghostscript) => "ghostscript",
        ("dnf", EngineKind::Qpdf) => "qpdf",
        ("dnf", EngineKind::Tesseract) => "tesseract",
        ("dnf", EngineKind::Libreoffice) => "libreoffice-core",
        ("dnf", EngineKind::ImageMagick) => "ImageMagick",
        ("dnf", EngineKind::Pandoc) => "pandoc",
        ("dnf", EngineKind::Java) => "java-21-openjdk-headless",
        ("dnf", EngineKind::Python) => "python3",
        ("dnf", EngineKind::Node) => "nodejs",
        ("dnf", _) => return None,
        ("apk", EngineKind::Ghostscript) => "ghostscript",
        ("apk", EngineKind::Qpdf) => "qpdf",
        ("apk", EngineKind::Tesseract) => "tesseract-ocr",
        ("apk", EngineKind::Libreoffice) => "libreoffice",
        ("apk", EngineKind::ImageMagick) => "imagemagick",
        ("apk", EngineKind::Pandoc) => "pandoc",
        ("apk", EngineKind::Java) => "openjdk17-jre",
        ("apk", EngineKind::Python) => "python3",
        ("apk", EngineKind::Node) => "nodejs",
        ("apk", _) => return None,
        _ => return None,
    };
    Some(vec![
        "sudo".into(),
        manager.into(),
        install_arg.into(),
        "-y".into(),
        pkg.into(),
    ])
}

#[cfg(not(target_os = "linux"))]
fn linux_spec(_kind: EngineKind) -> Option<Vec<String>> {
    None
}

#[cfg(target_os = "linux")]
fn pacman_spec(kind: EngineKind) -> Option<Vec<String>> {
    let pkg = match kind {
        EngineKind::Ghostscript => "ghostscript",
        EngineKind::Qpdf => "qpdf",
        EngineKind::Tesseract => "tesseract",
        EngineKind::Libreoffice => "libreoffice-fresh",
        EngineKind::ImageMagick => "imagemagick",
        EngineKind::Pandoc => "pandoc",
        EngineKind::Java => "jre-openjdk",
        EngineKind::Python => "python",
        EngineKind::Node => "nodejs",
        _ => return None,
    };
    Some(vec![
        "sudo".into(),
        "pacman".into(),
        "-S".into(),
        "--noconfirm".into(),
        pkg.into(),
    ])
}

#[cfg(not(target_os = "linux"))]
fn pacman_spec(_kind: EngineKind) -> Option<Vec<String>> {
    None
}

/// `ID` and `ID_LIKE` values from os-release, split on whitespace.
#[cfg(target_os = "linux")]
fn os_release_ids(release: &str) -> Vec<String> {
    let mut ids = Vec::new();
    for line in release.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if !matches!(key, "ID" | "ID_LIKE") {
            continue;
        }
        let value = value.trim().trim_matches('"');
        ids.extend(value.split_whitespace().map(str::to_string));
    }
    ids
}

fn brew_spec(kind: EngineKind) -> Option<&'static str> {
    Some(match kind {
        EngineKind::Ghostscript => "ghostscript",
        EngineKind::Qpdf => "qpdf",
        EngineKind::Tesseract => "tesseract",
        EngineKind::Libreoffice => "--cask libreoffice",
        EngineKind::ImageMagick => "imagemagick",
        EngineKind::Pandoc => "pandoc",
        EngineKind::Java => "openjdk",
        EngineKind::Python => "python@3.13",
        EngineKind::Uv => "uv",
        EngineKind::Node => "node",
    })
}

fn winget_spec(kind: EngineKind) -> Option<&'static str> {
    Some(match kind {
        EngineKind::Ghostscript => "ArtifexSoftware.Ghostscript",
        EngineKind::Tesseract => "UB-Mannheim.TesseractOCR",
        EngineKind::Libreoffice => "TheDocumentFoundation.LibreOffice",
        EngineKind::ImageMagick => "ImageMagick.ImageMagick",
        EngineKind::Pandoc => "JohnMacFarlane.Pandoc",
        EngineKind::Java => "EclipseAdoptium.Temurin.21.JDK",
        EngineKind::Python => "Python.Python.3.12",
        EngineKind::Uv => "astral-sh.uv",
        EngineKind::Node => "OpenJS.NodeJS.LTS",
        EngineKind::Qpdf => return None,
    })
}

/// Resolve a user-supplied engine name to a kind: label or executable
/// alias, case-insensitive.
pub fn kind_named(name: &str) -> Option<EngineKind> {
    let needle = name.trim().to_lowercase();
    ALL_ENGINES
        .iter()
        .copied()
        .find(|kind| kind.label() == needle || kind.candidates().contains(&needle.as_str()))
}
