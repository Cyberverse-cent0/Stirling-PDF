//! A page's ruling lines reduced to a grid, port of `RuleGrid.java`:
//! rules merged into levels, levels grouped into the components that each
//! describe one table. Both steps are bounded.

use super::page_rules::Rule;

/// Rules within this distance are the same drawn line (double strokes, overdraw).
pub const LEVEL_TOLERANCE: f32 = 2.5;

/// Slack when testing whether a horizontal and a vertical rule touch.
const TOUCH: f32 = 3.0;

/// Segments at one position further apart than this belong to different tables.
const CONTIGUOUS_GAP: f32 = 8.0;

/// Crossing tests past which a page is an operator flood rather than a readable grid.
const MAX_CROSSING_TESTS: u64 = 4_000_000;

/// Rule components past which the extra blocks cannot be real tables.
const MAX_COMPONENTS: usize = 256;

/// A group of rules at the same position: `pos` with the union of their extents.
#[derive(Debug, Clone, Copy)]
pub struct Level {
    pub pos: f32,
    pub lo: f32,
    pub hi: f32,
}

/// One connected component of crossing rules: the levels of each family it spans.
#[derive(Debug, Clone, Default)]
pub struct Component {
    pub h: Vec<Level>,
    pub v: Vec<Level>,
}

pub struct RuleGrid;

impl RuleGrid {
    /// Mirrors the module-level [`LEVEL_TOLERANCE`] for `RuleGrid::` callers.
    pub const LEVEL_TOLERANCE: f32 = crate::pdftext::rule_grid::LEVEL_TOLERANCE;

    /// Merges rules at the same position into levels, but only while
    /// contiguous, so two tables ruling at the same x are not bridged
    /// into one region.
    pub fn cluster(rules: &[Rule]) -> Vec<Level> {
        let mut sorted = rules.to_vec();
        sorted.sort_by(|a, b| a.pos.total_cmp(&b.pos).then(a.lo.total_cmp(&b.lo)));
        let mut out = Vec::new();
        let mut i = 0;
        while i < sorted.len() {
            let pos = sorted[i].pos;
            let mut j = i;
            while j < sorted.len() && sorted[j].pos - pos <= LEVEL_TOLERANCE {
                j += 1;
            }
            let mut same: Vec<Rule> = sorted[i..j].to_vec();
            same.sort_by(|a, b| a.lo.total_cmp(&b.lo));
            let mut lo = same[0].lo;
            let mut hi = same[0].hi;
            for rule in &same[1..] {
                if rule.lo <= hi + CONTIGUOUS_GAP {
                    hi = hi.max(rule.hi);
                } else {
                    out.push(Level { pos, lo, hi });
                    lo = rule.lo;
                    hi = rule.hi;
                }
            }
            out.push(Level { pos, lo, hi });
            i = j;
        }
        out
    }

    /// Connected components of crossing rules, read from one union-find
    /// array: an id array per component is O(components × levels) a rule
    /// flood can exhaust.
    pub fn partition(h_levels: &[Level], v_levels: &[Level]) -> Vec<Component> {
        let n = h_levels.len() + v_levels.len();
        if (h_levels.len() as u64) * (v_levels.len() as u64) > MAX_CROSSING_TESTS {
            tracing::debug!(
                h = h_levels.len(),
                v = v_levels.len(),
                "ruled-table partition skipped: rule-level crossing flood"
            );
            return Vec::new();
        }
        let mut parent: Vec<usize> = (0..n).collect();
        for i in 0..h_levels.len() {
            let h = &h_levels[i];
            for (j, v) in v_levels.iter().enumerate() {
                let crosses = v.pos >= h.lo - TOUCH
                    && v.pos <= h.hi + TOUCH
                    && h.pos >= v.lo - TOUCH
                    && h.pos <= v.hi + TOUCH;
                if crosses {
                    union(&mut parent, i, h_levels.len() + j);
                }
            }
        }
        // First-seen order, like the Java LinkedHashMap; roots are stable
        // once the union phase ends.
        let mut by_root: Vec<(usize, Component)> = Vec::new();
        for i in 0..n {
            let root = find(&mut parent, i);
            match by_root.iter().position(|(r, _)| *r == root) {
                Some(p) => by_root[p].1.push_level(i, h_levels, v_levels),
                None => {
                    // Past the cap the page is line art, not tables; keep the
                    // components already found whole rather than truncating
                    // them mid-scan.
                    if by_root.len() >= MAX_COMPONENTS {
                        continue;
                    }
                    by_root.push((root, Component::default()));
                    by_root
                        .last_mut()
                        .expect("just pushed")
                        .1
                        .push_level(i, h_levels, v_levels);
                }
            }
        }
        by_root.into_iter().map(|(_, c)| c).collect()
    }

    /// Partitioning depends only on rule geometry.
    pub fn component_count(horizontal: &[Rule], vertical: &[Rule]) -> usize {
        Self::partition(&Self::cluster(horizontal), &Self::cluster(vertical)).len()
    }
}

impl Component {
    fn push_level(&mut self, index: usize, h_levels: &[Level], v_levels: &[Level]) {
        if index < h_levels.len() {
            self.h.push(h_levels[index]);
        } else {
            self.v.push(v_levels[index - h_levels.len()]);
        }
    }
}

fn find(parent: &mut [usize], x: usize) -> usize {
    let mut x = x;
    while parent[x] != x {
        parent[x] = parent[parent[x]];
        x = parent[x];
    }
    x
}

fn union(parent: &mut [usize], a: usize, b: usize) {
    let ra = find(parent, a);
    let rb = find(parent, b);
    if ra != rb {
        parent[rb] = ra;
    }
}
