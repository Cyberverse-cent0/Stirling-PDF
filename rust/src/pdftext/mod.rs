//! Structured-text extraction and the PDF → Markdown pipeline, the Rust
//! counterpart of jpdfium's text model plus the proprietary
//! `stirling.software.proprietary.pdf` package.

pub mod column_accumulator;
pub mod column_layout;
pub mod column_ranges;
pub mod common_heading;
pub mod converter;
pub mod form_field;
pub mod form_values;
pub mod gfm_table;
pub mod glyph_stitcher;
pub mod heading;
pub mod line;
pub mod line_merger;
pub mod markdown_blocks;
pub mod markdown_text;
pub mod model;
pub mod page_images;
pub mod page_rules;
pub mod page_stitcher;
pub mod paragraph;
pub mod pdf_form_reader;
pub mod redact;
pub mod rule_grid;
pub mod ruled_rows;
pub mod ruled_tables;
pub mod shared;
pub mod table_block;
pub mod table_finder;
pub mod table_grid;
pub mod table_shape;
pub mod word_geometry;
