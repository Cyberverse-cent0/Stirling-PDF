//! Signature-form fonts. Java serves `classpath:static/fonts/*.woff2`
//! (baked into the jar) and `customFiles/static/fonts/*` through one
//! resource handler; Rust embeds the former via `include_bytes!` and
//! reads the latter from the static root at request time.

use std::path::Path;

/// `(file_name, bytes)` for the fonts Java baked into `static/fonts/`.
/// Only metadata is needed by `/ui-data/sign`; `web::serve` uses `bytes`
/// to answer `/fonts/*` when no custom override exists.
pub const BUNDLED: &[(&str, &[u8])] = &[
    (
        "Arimo-Regular.woff2",
        include_bytes!("../assets/fonts/Arimo-Regular.woff2"),
    ),
    (
        "DancingScript-Regular.woff2",
        include_bytes!("../assets/fonts/DancingScript-Regular.woff2"),
    ),
    (
        "Estonia.woff2",
        include_bytes!("../assets/fonts/Estonia.woff2"),
    ),
    (
        "IndieFlower-Regular.woff2",
        include_bytes!("../assets/fonts/IndieFlower-Regular.woff2"),
    ),
    (
        "Tangerine.woff2",
        include_bytes!("../assets/fonts/Tangerine.woff2"),
    ),
    (
        "Tinos-Regular.woff2",
        include_bytes!("../assets/fonts/Tinos-Regular.woff2"),
    ),
];

pub fn bytes(name: &str) -> Option<&'static [u8]> {
    BUNDLED.iter().find(|(n, _)| *n == name).map(|(_, b)| *b)
}

/// `FontResource.getFormatFromExtension` — the CSS `format()` hint the
/// sign canvas passes to `FontFace`.
pub fn format_for(extension: &str) -> &'static str {
    match extension {
        "ttf" => "truetype",
        "woff" => "woff",
        "woff2" => "woff2",
        "eot" => "embedded-opentype",
        "svg" => "svg",
        _ => "",
    }
}

/// `{name, extension, type}` for one font file, mirroring
/// `UIDataController.FontResource`: name/extension split at the last dot.
pub fn describe(file_name: &str) -> serde_json::Value {
    let (name, extension) = match file_name.rfind('.') {
        Some(i) if i > 0 => (&file_name[..i], &file_name[i + 1..]),
        _ => (file_name, ""),
    };
    serde_json::json!({
        "name": name,
        "extension": extension,
        "type": format_for(extension),
    })
}

/// Custom fonts under `<static root>/fonts/` — Java's
/// `customFiles/static/fonts/*` half of `getFontNames`.
pub fn custom_fonts(static_root: &Path) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(static_root.join("fonts")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                out.push(describe(&entry.file_name().to_string_lossy()));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_fonts_are_embedded_non_empty() {
        assert!(!BUNDLED.is_empty());
        for (name, data) in BUNDLED {
            assert!(name.ends_with(".woff2"), "{name}");
            assert!(!data.is_empty(), "{name}");
            assert_eq!(bytes(name), Some(*data));
        }
    }

    #[test]
    fn bytes_lookup_is_exact() {
        assert!(bytes("Arimo-Regular.woff2").is_some());
        assert!(bytes("arimo-regular.woff2").is_none());
        assert!(bytes("nope.ttf").is_none());
    }

    #[test]
    fn format_for_matches_java_extensions() {
        assert_eq!(format_for("ttf"), "truetype");
        assert_eq!(format_for("woff"), "woff");
        assert_eq!(format_for("woff2"), "woff2");
        assert_eq!(format_for("eot"), "embedded-opentype");
        assert_eq!(format_for("svg"), "svg");
        assert_eq!(format_for("otf"), "");
        assert_eq!(format_for(""), "");
    }

    #[test]
    fn describe_splits_at_last_dot() {
        let v = describe("My Font.ttf");
        assert_eq!(v["name"], "My Font");
        assert_eq!(v["extension"], "ttf");
        assert_eq!(v["type"], "truetype");

        let v = describe("archive.tar.gz");
        assert_eq!(v["name"], "archive.tar");
        assert_eq!(v["extension"], "gz");
        assert_eq!(v["type"], "");

        let v = describe("noextension");
        assert_eq!(v["name"], "noextension");
        assert_eq!(v["extension"], "");
        assert_eq!(v["type"], "");

        // A leading dot is not an extension separator (`i > 0` guard).
        let v = describe(".hidden");
        assert_eq!(v["name"], ".hidden");
        assert_eq!(v["extension"], "");
    }

    #[test]
    fn custom_fonts_lists_files_under_fonts_dir() {
        let dir = tempfile::tempdir().unwrap();
        let fonts = dir.path().join("fonts");
        std::fs::create_dir_all(&fonts).unwrap();
        std::fs::write(fonts.join("Custom.woff2"), b"wOF2").unwrap();
        std::fs::write(fonts.join("notes.txt"), b"not a font").unwrap();
        std::fs::create_dir_all(fonts.join("nested")).unwrap();

        let out = custom_fonts(dir.path());
        assert_eq!(out.len(), 2);
        let names: Vec<&str> = out.iter().map(|v| v["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"Custom"));
        assert!(names.contains(&"notes"));
    }

    #[test]
    fn custom_fonts_missing_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(custom_fonts(dir.path()).is_empty());
    }
}
