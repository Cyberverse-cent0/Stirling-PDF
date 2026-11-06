//! Incremental [`column_ranges::find_with_support`] for a stitched table:
//! appending a page costs O(page), bit-for-bit identical to re-projecting
//! the lot. Port of `ColumnAccumulator.java`.

use super::column_ranges;
use super::line::{Line, SharedLine};

pub struct ColumnAccumulator {
    line_count: usize,
    min_x: f32,
    max_x: f32,
    total_width: f64,
    total_chars: usize,

    /// Coverage counts, `cov[i]` = lines covering absolute x-bucket
    /// `cov_base + i`.
    cov: Vec<i32>,
    cov_base: i32,

    /// Set once the x-span exceeds what `find_with_support` accepts; no
    /// histogram is then kept.
    oversized: bool,
    scratch: Vec<bool>,
}

impl ColumnAccumulator {
    pub fn of(rows: &[Vec<SharedLine>]) -> Self {
        let mut a = Self {
            line_count: 0,
            min_x: f32::MAX,
            max_x: f32::MIN,
            total_width: 0.0,
            total_chars: 0,
            cov: Vec::new(),
            cov_base: 0,
            oversized: false,
            scratch: Vec::new(),
        };
        for row in rows {
            for l in row {
                a.add_line(l);
            }
        }
        a
    }

    pub fn add_line(&mut self, l: &Line) {
        self.line_count += 1;
        let words = l.words();
        let mut line_lo = i32::MAX;
        let mut line_hi = i32::MIN;
        for w in &words {
            let x0 = w.x;
            let x1 = x0 + w.width;
            self.min_x = self.min_x.min(x0);
            self.max_x = self.max_x.max(x1);
            self.total_width += w.width as f64;
            self.total_chars += w.stripped().chars().count().max(1);
            let a = x0.floor() as i32;
            let b = x1.ceil() as i32;
            line_lo = line_lo.min(a);
            line_hi = line_hi.max(b);
        }
        // Mirrors column_ranges' guard: past this span it returns no columns, so the
        // histogram is dead weight and (with crafted coordinates) unboundedly large.
        if !self.oversized && (self.max_x - self.min_x) > 2000.0 {
            self.oversized = true;
            self.cov.clear();
            self.scratch.clear();
        }
        if self.oversized || line_hi <= line_lo {
            return;
        }
        self.ensure_range(line_lo, line_hi);
        let n = (line_hi - line_lo) as usize;
        if self.scratch.len() < n {
            self.scratch.resize(n, false);
        }
        self.scratch[..n].fill(false);
        for w in &words {
            let a = (w.x.floor() as i32).wrapping_sub(line_lo);
            let b = ((w.x + w.width).ceil() as i32).wrapping_sub(line_lo);
            for x in a..b {
                self.scratch[x as usize] = true;
            }
        }
        let off = line_lo.wrapping_sub(self.cov_base) as usize;
        for (x, s) in self.scratch[..n].iter().enumerate() {
            if *s {
                self.cov[off + x] += 1;
            }
        }
    }

    fn ensure_range(&mut self, lo: i32, hi: i32) {
        if self.cov.is_empty() {
            self.cov_base = lo.wrapping_sub(32);
            self.cov = vec![0; hi.wrapping_sub(lo) as usize + 64];
            return;
        }
        let have0 = self.cov_base;
        let have1 = self.cov_base.wrapping_add(self.cov.len() as i32);
        if lo >= have0 && hi <= have1 {
            return;
        }
        let new_base = have0.min(lo).wrapping_sub(32);
        let new_end = have1.max(hi).wrapping_add(32);
        let mut nc = vec![0i32; new_end.wrapping_sub(new_base) as usize];
        let dst = have0.wrapping_sub(new_base) as usize;
        nc[dst..dst + self.cov.len()].copy_from_slice(&self.cov);
        self.cov = nc;
        self.cov_base = new_base;
    }

    /// Exactly what [`column_ranges::find`] would return for the accumulated lines.
    pub fn columns(&self) -> Vec<[f32; 2]> {
        if self.oversized || self.max_x <= self.min_x || (self.max_x - self.min_x) > 2000.0 {
            return Vec::new();
        }
        let lo = self.min_x.floor() as i32;
        let span = (self.max_x.ceil() as i32)
            .wrapping_sub(lo)
            .wrapping_add(1)
            .clamp(0, 2001) as usize;
        let support = ((self.line_count as f32 * 0.35).round() as i32).max(2);
        let mut columns: Vec<[f32; 2]> = Vec::new();
        let mut start = -1i32;
        for x in 0..span as i32 {
            let idx = lo.wrapping_add(x).wrapping_sub(self.cov_base);
            let c = if idx >= 0 && (idx as usize) < self.cov.len() {
                self.cov[idx as usize]
            } else {
                0
            };
            let is_column = c >= support;
            if is_column && start < 0 {
                start = x;
            } else if !is_column && start >= 0 {
                columns.push([lo.wrapping_add(start) as f32, lo.wrapping_add(x) as f32]);
                start = -1;
            }
        }
        if start >= 0 {
            columns.push([
                lo.wrapping_add(start) as f32,
                lo.wrapping_add(span as i32) as f32,
            ]);
        }

        let char_width = if self.total_chars == 0 {
            6.0
        } else {
            (self.total_width / self.total_chars as f64) as f32
        };
        let min_gutter = column_ranges::GUTTER_FLOOR.max(char_width * column_ranges::GUTTER_CHARS);
        let mut merged: Vec<[f32; 2]> = Vec::new();
        for band in columns {
            if let Some(last) = merged.last_mut() {
                if band[0] - last[1] < min_gutter {
                    last[1] = band[1];
                    continue;
                }
            }
            merged.push(band);
        }
        merged
    }
}
