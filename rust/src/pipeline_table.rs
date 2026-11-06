//! GENERATED FILE — do not edit by hand.
//! Source: frontend/src/editor/core/types/toolIO.ts (generated from the Java
//! OpenAPI spec's x-stirling-io declarations, i.e. @ToolIO annotations).
//! Regenerate: task frontend:tool-models && node rust/scripts/generate_pipeline_table.mjs
//!
//! Pipeline-facing slice of ToolIORegistry: isMultiInput and the accepted
//! input extensions (getExtensionTypes(output=false)). Endpoints absent from
//! the table declare no @ToolIO, so Java treats them as single-input with no
//! extension restriction — OpMeta::default reproduces that.

/// What the pipeline needs to know about one operation path.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpMeta {
    /// ToolArity MISO/MIMO: every matching file goes into a single request.
    pub multi_input: bool,
    /// ToolIOSpec.acceptedExtensions(); empty means unrestricted ("ALL").
    pub extensions: &'static [&'static str],
}

static TABLE: &[(&str, OpMeta)] = &[
    (
        "/api/v1/ai/tools/classify-and-label",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/convert/cbr/pdf",
        OpMeta {
            multi_input: false,
            extensions: &["cbr"],
        },
    ),
    (
        "/api/v1/convert/cbz/pdf",
        OpMeta {
            multi_input: false,
            extensions: &["cbz"],
        },
    ),
    (
        "/api/v1/convert/ebook/pdf",
        OpMeta {
            multi_input: false,
            extensions: &["epub", "mobi", "azw3", "fb2", "txt", "docx"],
        },
    ),
    (
        "/api/v1/convert/eml/pdf",
        OpMeta {
            multi_input: false,
            extensions: &["eml", "msg"],
        },
    ),
    (
        "/api/v1/convert/file/pdf",
        OpMeta {
            multi_input: false,
            extensions: &[],
        },
    ),
    (
        "/api/v1/convert/html/pdf",
        OpMeta {
            multi_input: false,
            extensions: &[
                "html", "htm", "xhtml", "zip", "rar", "7z", "tar", "gz", "bz2", "xz", "lz", "lzma",
                "z",
            ],
        },
    ),
    (
        "/api/v1/convert/img/pdf",
        OpMeta {
            multi_input: true,
            extensions: &[
                "png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff", "svg", "psd", "ai",
                "eps",
            ],
        },
    ),
    (
        "/api/v1/convert/markdown/pdf",
        OpMeta {
            multi_input: false,
            extensions: &[
                "md", "markdown", "zip", "rar", "7z", "tar", "gz", "bz2", "xz", "lz", "lzma", "z",
            ],
        },
    ),
    (
        "/api/v1/convert/pdf/cbr",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/convert/pdf/cbz",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/convert/pdf/csv",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/convert/pdf/epub",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/convert/pdf/html",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/convert/pdf/img",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/convert/pdf/markdown",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/convert/pdf/pdfa",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/convert/pdf/presentation",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/convert/pdf/text",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/convert/pdf/ua",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/convert/pdf/vector",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/convert/pdf/word",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/convert/pdf/xlsx",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/convert/pdf/xml",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/convert/svg/pdf",
        OpMeta {
            multi_input: true,
            extensions: &[
                "png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff", "svg", "psd", "ai",
                "eps",
            ],
        },
    ),
    (
        "/api/v1/convert/url/pdf",
        OpMeta {
            multi_input: false,
            extensions: &[],
        },
    ),
    (
        "/api/v1/convert/vector/pdf",
        OpMeta {
            multi_input: false,
            extensions: &["ps", "eps"],
        },
    ),
    (
        "/api/v1/docparse/ingest",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/filter/filter-contains-image",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/filter/filter-contains-text",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/filter/filter-file-size",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/filter/filter-page-count",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/filter/filter-page-rotation",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/filter/filter-page-size",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/form/form-detection/detect",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/general/booklet-imposition",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/general/crop",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/general/edit-table-of-contents",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/general/extract-bookmarks",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/general/merge-pdfs",
        OpMeta {
            multi_input: true,
            extensions: &[
                "pdf", "png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff", "svg", "psd",
                "ai", "eps",
            ],
        },
    ),
    (
        "/api/v1/general/multi-page-layout",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/general/overlay-pdfs",
        OpMeta {
            multi_input: true,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/general/pdf-to-single-page",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/general/rearrange-pages",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/general/remove-image-pdf",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/general/remove-pages",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/general/rotate-pdf",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/general/scale-pages",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/general/split-by-size-or-count",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/general/split-for-poster-print",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/general/split-pages",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/general/split-pdf-by-chapters",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/general/split-pdf-by-sections",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/integration/external-api-call",
        OpMeta {
            multi_input: false,
            extensions: &[],
        },
    ),
    (
        "/api/v1/integration/purview-apply-label",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/integration/purview-read-label",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/misc/add-attachments",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/misc/add-comments",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/misc/add-image",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/misc/add-page-numbers",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/misc/add-stamp",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/misc/auto-rename",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/misc/auto-rotate-pdf",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/misc/auto-split-pdf",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/misc/batch-process-attachments",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/misc/compress-pdf",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/misc/create-portfolio",
        OpMeta {
            multi_input: true,
            extensions: &[],
        },
    ),
    (
        "/api/v1/misc/decompress-pdf",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/misc/delete-attachment",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/misc/extract-attachments",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/misc/extract-image-scans",
        OpMeta {
            multi_input: false,
            extensions: &[
                "pdf", "png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff", "svg", "psd",
                "ai", "eps",
            ],
        },
    ),
    (
        "/api/v1/misc/extract-images",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/misc/extract-single-attachment",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/misc/flatten",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/misc/flatten-portfolio",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/misc/list-attachments",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/misc/ocr-pdf",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/misc/remove-blanks",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/misc/rename-attachment",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/misc/repair",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/misc/replace-invert-pdf",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/misc/scanner-effect",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/misc/show-javascript",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/misc/unlock-pdf-forms",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/misc/update-metadata",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/security/accessibility-report",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/security/add-password",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/security/add-watermark",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/security/auto-redact",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/security/cert-sign",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/security/get-info-on-pdf",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/security/redact",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/security/redact-execute",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/security/remove-cert-sign",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/security/remove-password",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/security/sanitize-pdf",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/security/timestamp-pdf",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/security/validate-compliance",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/security/validate-signature",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
    (
        "/api/v1/security/verify-pdf",
        OpMeta {
            multi_input: false,
            extensions: &["pdf"],
        },
    ),
];

/// Metadata for an operation path, or the no-declaration default.
pub fn meta(path: &str) -> OpMeta {
    TABLE
        .binary_search_by(|(p, _)| (*p).cmp(path))
        .map(|i| TABLE[i].1)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_sorted_for_binary_search() {
        assert!(TABLE.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn known_endpoints_resolve() {
        let merge = meta("/api/v1/general/merge-pdfs");
        assert!(merge.multi_input);
        assert!(merge.extensions.contains(&"pdf"));
        assert!(merge.extensions.contains(&"png"));

        let split = meta("/api/v1/general/split-pages");
        assert!(!split.multi_input);
        assert_eq!(split.extensions, &["pdf"]);

        // ANY input short-circuits to unrestricted.
        assert!(meta("/api/v1/misc/create-portfolio").extensions.is_empty());
    }

    #[test]
    fn undeclared_endpoints_get_defaults() {
        let m = meta("/api/v1/form/fill");
        assert!(!m.multi_input);
        assert!(m.extensions.is_empty());
    }
}
