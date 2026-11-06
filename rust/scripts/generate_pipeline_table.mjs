#!/usr/bin/env node
// Generates rust/src/pipeline_table.rs from the checked-in ToolIO table
// (frontend/src/editor/core/types/toolIO.ts), which is itself generated from
// the Java OpenAPI spec's x-stirling-io declarations (@ToolIO annotations).
//
// The pipeline needs exactly what ToolIORegistry serves Java's
// PipelineProcessor: isMultiInput (arity MISO/MIMO) and the accepted input
// extensions (ToolFormat extension union; empty means unrestricted "ALL").
//
// Regenerate after Java @ToolIO declarations change:
//   task frontend:tool-models && node rust/scripts/generate_pipeline_table.mjs

import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = join(here, "..", "..");
const ioPath = join(repo, "frontend/src/editor/core/types/toolIO.ts");
const outPath = join(repo, "rust/src/pipeline_table.rs");

const source = readFileSync(ioPath, "utf8");
const marker = "export const TOOL_IO: ToolIOTable = ";
const start = source.indexOf(marker);
if (start < 0) throw new Error("TOOL_IO declaration not found in toolIO.ts");
const literalStart = start + marker.length;
// The literal ends at the first "\n};" after it — entries never contain that.
const end = source.indexOf("\n};", literalStart);
if (end < 0) throw new Error("TOOL_IO literal end not found");
// eval is safe: the file is generated, the slice is a plain object literal.
const table = eval(`(${source.slice(literalStart, end + 2)})`);

// ToolFormat.java extension lists, verbatim.
const FORMAT_EXTENSIONS = {
  PDF: ["pdf"],
  PDF_ENCRYPTED: ["pdf"],
  IMAGE: ["png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff", "svg", "psd", "ai", "eps"],
  ZIP: ["zip", "rar", "7z", "tar", "gz", "bz2", "xz", "lz", "lzma", "z"],
  WORD: ["doc", "docx", "odt", "rtf"],
  PPT: ["ppt", "pptx", "odp"],
  EXCEL: ["xls", "xlsx", "ods"],
  CSV: ["csv"],
  HTML: ["html", "htm", "xhtml"],
  XML: ["xml", "xsd", "xsl"],
  JSON: ["json", "jsonl"],
  TEXT: ["txt", "text", "md", "markdown"],
  MARKDOWN: ["md", "markdown"],
  JAVASCRIPT: ["js", "jsx"],
  EBOOK: ["epub", "mobi", "azw3", "fb2", "txt", "docx"],
  EMAIL: ["eml", "msg"],
  POSTSCRIPT: ["ps", "eps"],
  PCL: ["pcl", "pxl"],
  XPS: ["xps", "oxps"],
  VIDEO: ["mp4", "webm", "avi", "mov", "mkv"],
  CBZ: ["cbz"],
  CBR: ["cbr"],
  ANY: [],
  NONE: [],
};

const paths = Object.keys(table).sort();
const rows = paths.map((path) => {
  const spec = table[path];
  const multiInput = spec.arity === "MISO" || spec.arity === "MIMO";
  // ToolIOSpec.acceptedExtensions(): ANY short-circuits to empty; otherwise
  // the union of the accepts formats' extensions, distinct. Empty → "ALL".
  let extensions = [];
  if (!spec.accepts.includes("ANY")) {
    extensions = [...new Set(spec.accepts.flatMap((f) => FORMAT_EXTENSIONS[f] ?? []))];
  }
  const exts = extensions.map((e) => `"${e}"`).join(", ");
  return `    ("${path}", OpMeta { multi_input: ${multiInput}, extensions: &[${exts}] }),`;
});

const out = `//! GENERATED FILE — do not edit by hand.
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
${rows.join("\n")}
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
`;

writeFileSync(outPath, out);
console.log(`wrote ${outPath} (${paths.length} operations)`);
