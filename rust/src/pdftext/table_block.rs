//! A detected table. Each row is a list of source lines: usually one, but
//! more when a cell wraps. Port of `TableBlock.java`.

use super::line::SharedLine;
use super::shared::RowSource;
use super::table_grid;

#[derive(Debug, Clone)]
pub struct TableBlock {
    pub rows: Vec<Vec<SharedLine>>,
    pub top: f32,
    pub bottom: f32,
    /// Exact column bands from vertical rules; `None` projects columns
    /// from whitespace instead.
    pub cols: Option<Vec<[f32; 2]>>,
    pub ruled: bool,
    pub row_source: RowSource,
    /// 1-based page the block was detected on.
    pub page: usize,
}

impl TableBlock {
    /// A word-grid block: rows inferred from geometry, nothing drawn.
    pub fn new(rows: Vec<Vec<SharedLine>>, top: f32, bottom: f32, page: usize) -> Self {
        Self {
            rows,
            top,
            bottom,
            cols: None,
            ruled: false,
            row_source: RowSource::Words,
            page,
        }
    }

    /// A rules-derived block whose rows are not a drawn lattice.
    pub fn ruled(
        rows: Vec<Vec<SharedLine>>,
        top: f32,
        bottom: f32,
        cols: Vec<[f32; 2]>,
        page: usize,
    ) -> Self {
        Self {
            rows,
            top,
            bottom,
            cols: Some(cols),
            ruled: true,
            row_source: RowSource::RuleBounded,
            page,
        }
    }

    /// The canonical constructor, matching the Java record's.
    #[allow(clippy::too_many_arguments)]
    pub fn full(
        rows: Vec<Vec<SharedLine>>,
        top: f32,
        bottom: f32,
        cols: Option<Vec<[f32; 2]>>,
        ruled: bool,
        row_source: RowSource,
        page: usize,
    ) -> Self {
        Self {
            rows,
            top,
            bottom,
            cols,
            ruled,
            row_source,
            page,
        }
    }

    pub fn render(&self) -> String {
        table_grid::render(&self.rows, self.cols.as_deref(), self.row_source)
    }

    /// Cell grid for the layout guards; empty when the block fails the table guards.
    pub fn cells(&self) -> Vec<Vec<String>> {
        table_grid::cells(&self.rows, self.cols.as_deref(), self.row_source)
    }
}
