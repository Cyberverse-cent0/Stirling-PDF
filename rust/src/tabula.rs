//! Lattice-mode table extraction, a port of Tabula 1.0.5's
//! `SpreadsheetExtractionAlgorithm` — the engine behind Java's
//! `/convert/pdf/csv` and `/convert/pdf/xlsx` (Tabula path).
//!
//! All geometry lives in "tabula space": the crop box's top-left is the
//! origin and y grows downward, matching PDFBox's `pageTransform`.
//! Rulings come from stroked/filled paths whose segments are axis
//! aligned after a 1° snap; cells are the rectangles those rulings
//! enclose; text is assigned to cells by containment.
//!
//! Deliberate divergences from Java, all in measurement precision:
//! Java stores coordinates as `float`, this port keeps `f64` (the
//! `feq` 0.01 tolerance absorbs the difference in practice); bidi
//! classification is approximated by Unicode range checks rather than
//! `Character.getDirectionality`; glyph heights use the AFM
//! ascent/descent metrics in `fontmetrics` instead of PDFBox's
//! per-font bounding-box walk. Pages are also capped at
//! `MAX_RULINGS` path rulings — Java has no bound, but the
//! intersection sweep is quadratic and unbounded input could hang.

use std::collections::{BTreeMap, HashMap, HashSet};

use lopdf::content::{Content, Operation};
use lopdf::{Dictionary, Document, Object, ObjectId};
use unicode_normalization::UnicodeNormalization;

use crate::fontmetrics;
use crate::pdfdoc;
use crate::text::{self, CharItem};

// ---------------------------------------------------------------------------
// Shared math (Utils.java)
// ---------------------------------------------------------------------------

fn feq(a: f64, b: f64) -> bool {
    (a - b).abs() < 0.01
}

fn within(first: f64, second: f64, variance: f64) -> bool {
    second < first + variance && second > first - variance
}

/// `Utils.round(d, 2)`: decimal half-up on the shortest round-trip
/// representation, like `BigDecimal(Double.toString(d))`.
fn round2(d: f64) -> f64 {
    // Past 1e15 the cents digits are below f64 resolution anyway, and
    // `int * 100` would overflow i64 — return the value unrounded
    // rather than collapsing it toward zero.
    if !d.is_finite() || d.abs() >= 1e15 {
        return d;
    }
    let s = format!("{:.10}", d.abs());
    let Some((int, frac)) = s.split_once('.') else {
        return if d.is_sign_negative() { -d } else { d };
    };
    let digits: Vec<u8> = frac.bytes().take(3).map(|b| b - b'0').collect();
    let Ok(int) = int.parse::<i64>() else {
        return d;
    };
    let cents: i64 = int * 100
        + i64::from(digits.first().copied().unwrap_or(0)) * 10
        + i64::from(digits.get(1).copied().unwrap_or(0))
        + i64::from(digits.get(2).copied().unwrap_or(0) >= 5);
    let v = cents as f64 / 100.0;
    if d.is_sign_negative() {
        -v
    } else {
        v
    }
}

fn overlap(y1: f64, h1: f64, y2: f64, h2: f64) -> bool {
    within(y1, y2, 0.1) || (y2 <= y1 && y2 >= y1 - h1) || (y1 <= y2 && y1 >= y2 - h2)
}

/// Ordering key for the Y-first / X-first point comparators: rounded to
/// 2 decimals like `Utils.round`, then exact `Double.compare`.
fn cmp_f64(a: f64, b: f64) -> std::cmp::Ordering {
    a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal)
}

fn point_cmp_yx(a: (f64, f64), b: (f64, f64)) -> std::cmp::Ordering {
    cmp_f64(round2(a.0), round2(b.0)).then(cmp_f64(round2(a.1), round2(b.1)))
}

fn point_cmp_xy(a: (f64, f64), b: (f64, f64)) -> std::cmp::Ordering {
    cmp_f64(round2(a.1), round2(b.1)).then(cmp_f64(round2(a.0), round2(b.0)))
}

/// `Rectangle.ILL_DEFINED_ORDER` for plain rectangles (`isLtrDominant`
/// is always 0, so the RTL branch never fires).
fn ill_defined_order(a: &Rect, b: &Rect, a_ltr: i32, b_ltr: i32) -> std::cmp::Ordering {
    if a == b {
        return std::cmp::Ordering::Equal;
    }
    let overlap = (a.bottom().min(b.bottom()) - a.y.max(b.y)).max(0.0);
    if overlap > 0.4 {
        if a_ltr == -1 && b_ltr == -1 {
            cmp_f64(b.x, a.x)
        } else {
            cmp_f64(a.x, b.x)
        }
    } else {
        cmp_f64(a.bottom(), b.bottom())
    }
}

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

/// Axis-aligned rect in tabula space: `x`/`y` are left/top.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl Rect {
    fn right(&self) -> f64 {
        self.x + self.w
    }
    fn bottom(&self) -> f64 {
        self.y + self.h
    }
    /// `Rectangle2D.intersects`: strict overlap, touching edges don't count.
    fn intersects(&self, o: &Rect) -> bool {
        self.x < o.right() && self.right() > o.x && self.y < o.bottom() && self.bottom() > o.y
    }
    /// `Rectangle2D.contains(Rectangle2D)`: fully inside, edges may touch.
    fn contains(&self, o: &Rect) -> bool {
        o.x >= self.x && o.y >= self.y && o.right() <= self.right() && o.bottom() <= self.bottom()
    }
    fn union(&self, o: &Rect) -> Rect {
        let x = self.x.min(o.x);
        let y = self.y.min(o.y);
        Rect {
            x,
            y,
            w: self.right().max(o.right()) - x,
            h: self.bottom().max(o.bottom()) - y,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Pt {
    x: f64,
    y: f64,
}

fn pt_key(p: Pt) -> (u64, u64) {
    // -0.0 and 0.0 compare equal in Java; normalise before bit-casting.
    let nx = if p.x == 0.0 { 0.0 } else { p.x };
    let ny = if p.y == 0.0 { 0.0 } else { p.y };
    (nx.to_bits(), ny.to_bits())
}

fn unpt_key(k: (u64, u64)) -> Pt {
    Pt {
        x: f64::from_bits(k.0),
        y: f64::from_bits(k.1),
    }
}

/// Bit key matching `Rect`'s derived `==` (with -0.0 folded onto 0.0),
/// for hash-based dedup. NaN fields must be excluded by the caller:
/// `==` is false for them even when the bits are identical.
fn rect_bits(r: &Rect) -> (u64, u64, u64, u64) {
    (
        coord_bits(r.x),
        coord_bits(r.y),
        coord_bits(r.w),
        coord_bits(r.h),
    )
}

// ---------------------------------------------------------------------------
// Ruling (Ruling.java / Line2D.Float)
// ---------------------------------------------------------------------------

const PERPENDICULAR_EXPAND: f64 = 2.0;
const COLLINEAR_EXPAND: f64 = 1.0;
const MIN_RULING_LENGTH: f64 = 0.01;

/// A line segment; endpoints stay in insertion order because Java's
/// `getTop`/`getLeft`/`getBottom`/`getRight` return `y1`/`x1`/`y2`/`x2`,
/// not min/max.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Ruling {
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
}

impl Ruling {
    fn new(x1: f64, y1: f64, x2: f64, y2: f64) -> Self {
        let mut r = Self { x1, y1, x2, y2 };
        r.normalize();
        r
    }

    fn normalize(&mut self) {
        let angle = (self.y2 - self.y1)
            .atan2(self.x2 - self.x1)
            .to_degrees()
            .rem_euclid(360.0);
        if within(angle, 0.0, 1.0) || within(angle, 180.0, 1.0) {
            self.y2 = self.y1;
        } else if within(angle, 90.0, 1.0) || within(angle, 270.0, 1.0) {
            self.x2 = self.x1;
        }
    }

    fn length(&self) -> f64 {
        ((self.x1 - self.x2).powi(2) + (self.y1 - self.y2).powi(2)).sqrt()
    }
    fn vertical(&self) -> bool {
        self.length() > 0.0 && feq(self.x1, self.x2)
    }
    fn horizontal(&self) -> bool {
        self.length() > 0.0 && feq(self.y1, self.y2)
    }
    fn position(&self) -> f64 {
        if self.vertical() {
            self.x1
        } else {
            self.y1
        }
    }
    fn start(&self) -> f64 {
        if self.vertical() {
            self.y1
        } else {
            self.x1
        }
    }
    fn end(&self) -> f64 {
        if self.vertical() {
            self.y2
        } else {
            self.x2
        }
    }
    fn set_start(&mut self, v: f64) {
        if self.vertical() {
            self.y1 = v;
        } else {
            self.x1 = v;
        }
    }
    fn set_end(&mut self, v: f64) {
        if self.vertical() {
            self.y2 = v;
        } else {
            self.x2 = v;
        }
    }
    fn set_start_end(&mut self, start: f64, end: f64) {
        if self.vertical() {
            self.y1 = start;
            self.y2 = end;
        } else {
            self.x1 = start;
            self.x2 = end;
        }
    }
    fn perpendicular_to(&self, other: &Ruling) -> bool {
        self.vertical() == other.horizontal()
    }

    fn expand(&self, amount: f64) -> Ruling {
        let mut r = *self;
        r.set_start(self.start() - amount);
        r.set_end(self.end() + amount);
        r
    }

    /// `Line2D.intersectsLine`: proper or touching intersection,
    /// collinear overlap counts.
    fn intersects_line(&self, o: &Ruling) -> bool {
        seg_intersects(
            (self.x1, self.y1),
            (self.x2, self.y2),
            (o.x1, o.y1),
            (o.x2, o.y2),
        )
    }

    /// `Line2D.intersects(Rectangle2D)`.
    fn intersects_rect(&self, r: &Rect) -> bool {
        let inside = |x: f64, y: f64| x >= r.x && x <= r.right() && y >= r.y && y <= r.bottom();
        if inside(self.x1, self.y1) || inside(self.x2, self.y2) {
            return true;
        }
        let corners = [
            ((r.x, r.y), (r.right(), r.y)),
            ((r.right(), r.y), (r.right(), r.bottom())),
            ((r.right(), r.bottom()), (r.x, r.bottom())),
            ((r.x, r.bottom()), (r.x, r.y)),
        ];
        corners
            .iter()
            .any(|(a, b)| seg_intersects((self.x1, self.y1), (self.x2, self.y2), *a, *b))
    }

    fn nearly_intersects(&self, other: &Ruling, expand_amount: f64) -> bool {
        if self.intersects_line(other) {
            return true;
        }
        if self.perpendicular_to(other) {
            self.expand(PERPENDICULAR_EXPAND).intersects_line(other)
        } else {
            self.expand(expand_amount)
                .intersects_line(&other.expand(expand_amount))
        }
    }

    /// `intersectionPoint`: the crossing of an expanded horizontal and
    /// vertical ruling, or `None` when they don't meet.
    fn intersection_point(&self, other: &Ruling) -> Option<Pt> {
        let this_l = self.expand(PERPENDICULAR_EXPAND);
        let other_l = other.expand(PERPENDICULAR_EXPAND);
        if !this_l.intersects_line(&other_l) {
            return None;
        }
        if this_l.horizontal() && other_l.vertical() {
            Some(Pt {
                x: other_l.x1,
                y: this_l.y1,
            })
        } else if this_l.vertical() && other_l.horizontal() {
            Some(Pt {
                x: this_l.x1,
                y: other_l.y1,
            })
        } else {
            // Java throws IllegalArgumentException here; callers only
            // ever pair horizontal with vertical rulings.
            None
        }
    }

    /// Cohen-Sutherland clip against a rect; returns the clipped ruling
    /// (re-normalized like `new Ruling(...)`) or the original when the
    /// clip rejects it — mirroring `Ruling.intersect`.
    fn intersect(&self, clip: &Rect) -> Ruling {
        match clip_line((self.x1, self.y1), (self.x2, self.y2), clip) {
            Some((p1, p2)) => Ruling::new(p1.0, p1.1, p2.0, p2.1),
            None => *self,
        }
    }

    /// `Ruling.findIntersections` sweep: every horizontal×vertical
    /// crossing, mapped to the expanded ruling pair. Keyed by
    /// `(y_bits, x_bits)` like the Java `TreeMap<Point2D>`'s (y, x)
    /// ordering; lookups are exact-match so bit order is irrelevant.
    fn find_intersections(
        horizontals: &[Ruling],
        verticals: &[Ruling],
    ) -> BTreeMap<(u64, u64), (Ruling, Ruling)> {
        #[derive(Clone, Copy, PartialEq)]
        enum Ev {
            HLeft,
            V,
            HRight,
        }
        let mut events: Vec<(f64, Ev, Ruling)> = Vec::new();
        for h in horizontals {
            events.push((h.x1 - PERPENDICULAR_EXPAND, Ev::HLeft, *h));
            events.push((h.x2 + PERPENDICULAR_EXPAND, Ev::HRight, *h));
        }
        for v in verticals {
            events.push((v.x1, Ev::V, *v));
        }
        let rank = |e: Ev| match e {
            Ev::HLeft => 0,
            Ev::V => 1,
            Ev::HRight => 2,
        };
        events.sort_by(|a, b| {
            if feq(a.0, b.0) {
                rank(a.1).cmp(&rank(b.1))
            } else {
                cmp_f64(a.0, b.0)
            }
        });

        // Active horizontals keyed by top edge; a TreeMap in Java, so a
        // second ruling at the same top replaces the first.
        let mut active: BTreeMap<u64, Ruling> = BTreeMap::new();
        let mut out: BTreeMap<(u64, u64), (Ruling, Ruling)> = BTreeMap::new();
        for (_, ev, r) in events {
            match ev {
                Ev::HLeft => {
                    active.insert(r.y1.to_bits(), r);
                }
                Ev::HRight => {
                    active.remove(&r.y1.to_bits());
                }
                Ev::V => {
                    for h in active.values() {
                        if let Some(p) = h.intersection_point(&r) {
                            out.insert(
                                (p.y.to_bits(), p.x.to_bits()),
                                (
                                    h.expand(PERPENDICULAR_EXPAND),
                                    r.expand(PERPENDICULAR_EXPAND),
                                ),
                            );
                        }
                    }
                }
            }
        }
        out
    }

    /// `Ruling.collapseOrientedRulings`: merge same-position rulings
    /// whose (expanded) segments nearly intersect.
    fn collapse_oriented(rulings: &[Ruling]) -> Vec<Ruling> {
        let mut lines: Vec<Ruling> = rulings.to_vec();
        lines.sort_by(|a, b| {
            let diff = a.position() - b.position();
            if diff == 0.0 {
                cmp_f64(a.start(), b.start())
            } else {
                cmp_f64(diff, 0.0)
            }
        });
        let mut rv: Vec<Ruling> = Vec::new();
        for next in lines {
            let merge = match rv.last() {
                Some(last) => {
                    feq(next.position(), last.position())
                        && last.nearly_intersects(&next, COLLINEAR_EXPAND)
                }
                None => false,
            };
            if merge {
                let last = rv.last_mut().unwrap();
                let last_start = last.start();
                let last_end = last.end();
                let last_flipped = last_start > last_end;
                let next_flipped = next.start() > next.end();
                let different = next_flipped != last_flipped;
                let next_s = if different { next.end() } else { next.start() };
                let next_e = if different { next.start() } else { next.end() };
                let new_start = if last_flipped {
                    next_s.max(last_start)
                } else {
                    next_s.min(last_start)
                };
                let new_end = if last_flipped {
                    next_e.min(last_end)
                } else {
                    next_e.max(last_end)
                };
                last.set_start_end(new_start, new_end);
            } else if next.length() == 0.0 {
                continue;
            } else {
                rv.push(next);
            }
        }
        rv
    }
}

// ---------------------------------------------------------------------------
// Segment math
// ---------------------------------------------------------------------------

fn orient(a: (f64, f64), b: (f64, f64), p: (f64, f64)) -> f64 {
    (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0)
}

fn on_segment(a: (f64, f64), b: (f64, f64), p: (f64, f64)) -> bool {
    p.0 >= a.0.min(b.0) - 1e-9
        && p.0 <= a.0.max(b.0) + 1e-9
        && p.1 >= a.1.min(b.1) - 1e-9
        && p.1 <= a.1.max(b.1) + 1e-9
}

fn seg_intersects(a: (f64, f64), b: (f64, f64), c: (f64, f64), d: (f64, f64)) -> bool {
    let (o1, o2, o3, o4) = (
        orient(a, b, c),
        orient(a, b, d),
        orient(c, d, a),
        orient(c, d, b),
    );
    if ((o1 > 0.0 && o2 < 0.0) || (o1 < 0.0 && o2 > 0.0))
        && ((o3 > 0.0 && o4 < 0.0) || (o3 < 0.0 && o4 > 0.0))
    {
        return true;
    }
    (o1.abs() < 1e-9 && on_segment(a, b, c))
        || (o2.abs() < 1e-9 && on_segment(a, b, d))
        || (o3.abs() < 1e-9 && on_segment(c, d, a))
        || (o4.abs() < 1e-9 && on_segment(c, d, b))
}

/// CohenSutherlandClipping.clip: returns the clipped endpoints, or
/// `None` when the segment lies entirely outside.
fn clip_line(a: (f64, f64), b: (f64, f64), r: &Rect) -> Option<((f64, f64), (f64, f64))> {
    const LEFT: i32 = 1;
    const RIGHT: i32 = 2;
    const BOTTOM: i32 = 4;
    const TOP: i32 = 8;
    const INSIDE: i32 = 0;

    let region = |x: f64, y: f64| -> i32 {
        let mut reg = if x < r.x {
            LEFT
        } else if x > r.right() {
            RIGHT
        } else {
            INSIDE
        };
        if y < r.y {
            reg |= BOTTOM;
        } else if y > r.bottom() {
            reg |= TOP;
        }
        reg
    };

    let mut p1 = (a.0, a.1);
    let mut p2 = (b.0, b.1);
    let mut r1 = region(p1.0, p1.1);
    let mut r2 = region(p2.0, p2.1);
    let vertical = p1.0 == p2.0;
    let slope = if vertical {
        0.0
    } else {
        (p2.1 - p1.1) / (p2.0 - p1.0)
    };
    let delta = |v1: f64, v2: f64| {
        if (v1 - v2).abs() < 0.01 {
            0.0
        } else {
            v1 - v2
        }
    };

    while r1 != INSIDE || r2 != INSIDE {
        if (r1 & r2) != 0 {
            return None;
        }
        let outside_reg = if r1 == INSIDE { r2 } else { r1 };
        let (ox, oy);
        if (outside_reg & LEFT) != 0 {
            ox = r.x;
            oy = delta(ox, p1.0) * slope + p1.1;
        } else if (outside_reg & RIGHT) != 0 {
            ox = r.right();
            oy = delta(ox, p1.0) * slope + p1.1;
        } else if (outside_reg & BOTTOM) != 0 {
            oy = r.y;
            ox = if vertical {
                p1.0
            } else {
                delta(oy, p1.1) / slope + p1.0
            };
        } else {
            oy = r.bottom();
            ox = if vertical {
                p1.0
            } else {
                delta(oy, p1.1) / slope + p1.0
            };
        }
        if outside_reg == r1 {
            p1 = (ox, oy);
            r1 = region(p1.0, p1.1);
        } else {
            p2 = (ox, oy);
            r2 = region(p2.0, p2.1);
        }
    }
    Some((p1, p2))
}

// ---------------------------------------------------------------------------
// Text elements (TextElement.java / TextStripper.java)
// ---------------------------------------------------------------------------

/// A positioned glyph in tabula space. `ltr` caches the dominant-
/// direction vote used by `ILL_DEFINED_ORDER` and `mergeWords`.
#[derive(Clone, Debug)]
struct TextEl {
    text: String,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    font_size: f64,
    font: String,
    space_w: f64,
    dir: f64,
}

impl TextEl {
    fn right(&self) -> f64 {
        self.x + self.w
    }
    fn bottom(&self) -> f64 {
        self.y + self.h
    }
    fn rect(&self) -> Rect {
        Rect {
            x: self.x,
            y: self.y,
            w: self.w,
            h: self.h,
        }
    }
    fn overlap_ratio(&self, o: &TextEl) -> f64 {
        let iw = (self.right().min(o.right()) - self.x.max(o.x)).max(0.0);
        let ih = (self.bottom().min(o.bottom()) - self.y.max(o.y)).max(0.0);
        let intersection = iw * ih;
        let union = self.w * self.h + o.w * o.h - intersection;
        if union <= 0.0 {
            0.0
        } else {
            intersection / union
        }
    }
}

// Rough bidi classification: 0 = LTR bucket (strong L + numbers +
// separators), 1 = RTL, 2 = neutral. `Character.getDirectionality`
// has ~20 classes; mergeWords only distinguishes these three buckets
// plus the whitespace-attach rule.
fn bidi_class(c: char) -> u8 {
    let u = c as u32;
    let rtl = matches!(u,
        0x0590..=0x05FF | 0x0600..=0x06FF | 0x0700..=0x074F | 0x0750..=0x077F
        | 0x0780..=0x07BF | 0x07C0..=0x07FF | 0x0800..=0x083F | 0x0840..=0x085F
        | 0x0860..=0x086F | 0x08A0..=0x08FF | 0xFB1D..=0xFB4F | 0xFB50..=0xFDFF
        | 0xFE70..=0xFEFF | 0x10800..=0x10FFF | 0x1E800..=0x1EFFF);
    if rtl {
        return 1;
    }
    if c.is_alphanumeric() || matches!(c, ',' | '.' | '\'' | ':' | ';') {
        return 0;
    }
    2
}

fn is_whitespace(c: char) -> bool {
    c.is_whitespace()
}

/// `isLtrDominant`: counts strong-L vs strong-R characters only;
/// returns 1 LTR, 0 neutral, -1 RTL like Java's `Integer.compare`.
fn ltr_dominant(els: &[TextEl]) -> i32 {
    let mut ltr = 0i64;
    let mut rtl = 0i64;
    for el in els {
        for c in el.text.chars() {
            let u = c as u32;
            let is_rtl = matches!(u,
                0x0590..=0x05FF | 0x0600..=0x06FF | 0x0700..=0x074F | 0x0750..=0x077F
                | 0x0780..=0x07BF | 0x07C0..=0x07FF | 0x0800..=0x083F | 0x0840..=0x085F
                | 0x0860..=0x086F | 0x08A0..=0x08FF | 0xFB1D..=0xFB4F | 0xFB50..=0xFDFF
                | 0xFE70..=0xFEFF | 0x10800..=0x10FFF | 0x1E800..=0x1EFFF);
            if is_rtl {
                rtl += 1;
            } else if c.is_alphabetic() {
                ltr += 1;
            }
        }
    }
    ltr.cmp(&rtl) as i32
}

/// `isPrintable`: rejects ISO control chars and the Specials block.
fn is_printable(c: char) -> bool {
    let u = c as u32;
    !c.is_control() && !(0xFFF0..=0xFFFF).contains(&u) && !(0xFDD0..=0xFDEF).contains(&u)
}

/// A merged run of text elements — `TextChunk`. `ltr` is computed once
/// at construction since chunks are immutable afterwards.
#[derive(Clone, Debug)]
struct Chunk {
    rect: Rect,
    els: Vec<TextEl>,
    ltr: i32,
}

impl Chunk {
    fn new(els: Vec<TextEl>) -> Self {
        let mut rect = Rect::default();
        if let Some(first) = els.first() {
            rect = first.rect();
            for el in els.iter().skip(1) {
                rect = rect.union(&el.rect());
            }
        }
        let ltr = ltr_dominant(&els);
        Self { rect, els, ltr }
    }

    /// `TextChunk.getText`: concat + NFKC + trim.
    fn text(&self) -> String {
        if self.els.is_empty() {
            return String::new();
        }
        let joined: String = self.els.iter().map(|e| e.text.as_str()).collect();
        joined.nfkc().collect::<String>().trim().to_string()
    }
}

/// `mergeWords`: ported from `TextElement.mergeWords` with an empty
/// vertical-ruling list (the lattice path never passes rulings, so the
/// `acrossVerticalRuling` branch is dead and omitted).
fn merge_words(elements: Vec<TextEl>) -> Vec<Chunk> {
    let mut iter = elements.into_iter();
    let Some(first) = iter.next() else {
        return Vec::new();
    };
    let first_rect = first.rect();
    let mut chunks: Vec<Chunk> = vec![Chunk::new(vec![first])];

    let mut prev_ave_char_width = first_rect.w;
    let mut end_of_last_text_x = first_rect.x + first_rect.w;
    let mut max_y_for_line = first_rect.bottom();
    let mut max_height_for_line = first_rect.h;
    let mut last_word_spacing = -1.0f64;

    for chr in iter {
        let chr_rect = chr.rect();
        // Snapshot the previous element's fields: `current` is mutably
        // borrowed for the pushes below, so the TextEl itself cannot
        // be held across them. The font String is cloned lazily, only
        // when a synthetic space is actually inserted.
        let (
            same_text,
            overlap_ratio,
            space_at_same_pos,
            font_or_size_diff,
            prev_ends_space,
            prev_x,
            prev_y,
            prev_w,
            prev_h,
            prev_font_size,
            prev_space_w,
            prev_dir,
        ) = {
            let prev = chunks.last().unwrap().els.last().unwrap();
            (
                chr.text == prev.text,
                prev.overlap_ratio(&chr),
                chr.text == " " && feq(prev.x, chr.x) && feq(prev.y, chr.y),
                chr.font != prev.font || !feq(chr.font_size, prev.font_size),
                prev.text.ends_with(' '),
                prev.x,
                prev.y,
                prev.w,
                prev.h,
                prev.font_size,
                prev.space_w,
                prev.dir,
            )
        };

        // Same char overlapping → skip.
        if same_text && overlap_ratio > 0.5 {
            continue;
        }
        // Space overlapping prev char at same position → skip.
        if space_at_same_pos {
            continue;
        }

        // Font or size change resets the running average.
        if font_or_size_diff {
            prev_ave_char_width = -1.0;
        }

        let word_spacing = chr.space_w;
        let delta_space = if word_spacing.is_nan() || word_spacing == 0.0 {
            f64::MAX
        } else if last_word_spacing < 0.0 {
            word_spacing * 0.5
        } else {
            ((word_spacing + last_word_spacing) / 2.0) * 0.5
        };

        let char_count = chr.text.chars().count().max(1) as f64;
        let average_char_width = if prev_ave_char_width < 0.0 {
            chr_rect.w / char_count
        } else {
            (prev_ave_char_width + chr_rect.w / char_count) / 2.0
        };
        let delta_char_width = average_char_width * 0.3;

        let mut expected_start_of_next_word_x = -f64::MAX;
        if end_of_last_text_x != -1.0 {
            expected_start_of_next_word_x = end_of_last_text_x + delta_char_width.min(delta_space);
        }

        let mut same_line = true;
        if !overlap(
            chr_rect.bottom(),
            chr_rect.h,
            max_y_for_line,
            max_height_for_line,
        ) {
            expected_start_of_next_word_x = -f64::MAX;
            max_y_for_line = -f64::MAX;
            max_height_for_line = -1.0;
            same_line = false;
        }
        end_of_last_text_x = chr_rect.x + chr_rect.w;

        // Synthetic space element when a word gap is detected.
        let mut sp: Option<(f64, f64)> = None;
        if same_line && expected_start_of_next_word_x < chr.x && !prev_ends_space {
            let s = TextEl {
                text: " ".to_string(),
                x: prev_x,
                y: prev_y,
                w: expected_start_of_next_word_x - prev_x,
                h: prev_h,
                font_size: prev_font_size,
                font: chunks.last().unwrap().els.last().unwrap().font.clone(),
                space_w: prev_space_w,
                dir: prev_dir,
            };
            let s_rect = s.rect();
            sp = Some((s.x, s.w));
            let current = chunks.last_mut().unwrap();
            current.els.push(s);
            current.rect = current.rect.union(&s_rect);
        }

        max_y_for_line = chr_rect.bottom().max(max_y_for_line);
        max_height_for_line = max_height_for_line.max(chr_rect.h);

        let dist = chr.x - sp.map_or(prev_x + prev_w, |(sx, sw)| sx + sw);
        // NaN wordSpacing makes the Java comparison false → new chunk.
        let current = chunks.last_mut().unwrap();
        if same_line
            && (if dist < 0.0 {
                vertically_overlaps(&current.rect, &chr_rect)
            } else {
                dist < word_spacing
            })
        {
            current.els.push(chr);
            current.rect = current.rect.union(&chr_rect);
        } else {
            chunks.push(Chunk::new(vec![chr]));
        }
        last_word_spacing = word_spacing;
        prev_ave_char_width = if let Some((_, sw)) = sp {
            (average_char_width + sw) / 2.0
        } else {
            average_char_width
        };
    }

    // groupByDirectionality per chunk.
    chunks.into_iter().map(group_by_directionality).collect()
}

/// `Rectangle.verticallyOverlaps`: raw overlap height > 0, no tolerance.
fn vertically_overlaps(a: &Rect, b: &Rect) -> bool {
    (a.bottom().min(b.bottom()) - a.y.max(b.y)) > 0.0
}

/// `TextChunk.groupByDirectionality`: split into directional runs,
/// reverse RTL runs, and reverse run order for RTL-dominant chunks.
fn group_by_directionality(chunk: Chunk) -> Chunk {
    if chunk.els.is_empty() {
        return chunk;
    }
    let is_ltr_dominant = chunk.ltr != -1;
    let mut chunks: Vec<Vec<TextEl>> = Vec::new();
    let mut buff: Vec<TextEl> = Vec::new();
    let mut buff_dir = 2u8;

    for te in &chunk.els {
        let te_dir = te.text.chars().next().map(bidi_class).unwrap_or(2);
        if buff.is_empty() {
            buff.push(te.clone());
            buff_dir = te_dir;
        } else {
            if buff_dir == 2 {
                buff_dir = te_dir;
            }
            if te_dir == buff_dir || te_dir == 2 {
                let ws = te.text.chars().next().is_some_and(is_whitespace);
                let opposite = if is_ltr_dominant { 1 } else { 0 };
                if ws && buff_dir == opposite {
                    buff.insert(0, te.clone());
                } else {
                    buff.push(te.clone());
                }
            } else {
                if buff_dir == 1 {
                    buff.reverse();
                }
                chunks.push(std::mem::take(&mut buff));
                buff_dir = te_dir;
                buff.push(te.clone());
            }
        }
    }
    if buff_dir == 1 {
        buff.reverse();
    }
    chunks.push(buff);
    if !is_ltr_dominant {
        chunks.reverse();
    }
    let mut els = Vec::new();
    for g in chunks {
        els.extend(g);
    }
    Chunk::new(els)
}

// ---------------------------------------------------------------------------
// Cells and tables
// ---------------------------------------------------------------------------

/// A ruled cell: rect + assigned text chunks (`Cell`).
#[derive(Clone, Debug)]
struct Cell {
    rect: Rect,
    chunks: Vec<Chunk>,
}

impl Cell {
    /// `Cell.getText(true)`: chunks sorted by `ILL_DEFINED_ORDER`,
    /// `\r` between chunks whose top advances, then trim.
    fn text(&self) -> String {
        if self.chunks.is_empty() {
            return String::new();
        }
        // Sort indices, not chunks: cloning would deep-copy every
        // TextEl's Strings per cell render.
        let mut order: Vec<usize> = (0..self.chunks.len()).collect();
        order.sort_by(|&a, &b| {
            ill_defined_order(
                &self.chunks[a].rect,
                &self.chunks[b].rect,
                self.chunks[a].ltr,
                self.chunks[b].ltr,
            )
        });
        let mut sb = String::new();
        let mut cur_top = self.chunks[order[0]].rect.y;
        for &i in &order {
            let tc = &self.chunks[i];
            if tc.rect.y > cur_top {
                sb.push('\r');
            }
            sb.push_str(&tc.text());
            cur_top = tc.rect.y;
        }
        sb.trim().to_string()
    }

    /// `RectangularTextContainer.merge`: prepend or append the other's
    /// chunks by `ILL_DEFINED_ORDER`, then union the rects.
    fn absorb(&mut self, other: &Cell) {
        let ord = ill_defined_order(&self.rect, &other.rect, 0, 0);
        if ord == std::cmp::Ordering::Less {
            self.chunks.extend(other.chunks.iter().cloned());
        } else {
            let mut merged = other.chunks.clone();
            merged.extend(self.chunks.iter().cloned());
            self.chunks = merged;
        }
        self.rect = self.rect.union(&other.rect);
    }
}

/// The extracted grid: `cells[(row, col)]`, missing slots render empty.
#[derive(Default)]
struct Table {
    row_count: usize,
    col_count: usize,
    cells: BTreeMap<(usize, usize), Cell>,
}

impl Table {
    fn add(&mut self, cell: Cell, row: usize, col: usize) {
        self.row_count = self.row_count.max(row + 1);
        self.col_count = self.col_count.max(col + 1);
        let key = (row, col);
        match self.cells.get_mut(&key) {
            Some(old) => {
                // `chunk.merge(old)`: the new chunk absorbs the old one.
                let mut merged = cell;
                merged.absorb(old);
                *old = merged;
            }
            None => {
                self.cells.insert(key, cell);
            }
        }
    }

    /// Rows of raw cell texts — `\r` preserved; the CSV path applies
    /// `TabulaTableParser.normaliseText` on top.
    fn rows(&self) -> Vec<Vec<String>> {
        let mut rows = Vec::with_capacity(self.row_count);
        for r in 0..self.row_count {
            let mut row = Vec::with_capacity(self.col_count);
            for c in 0..self.col_count {
                row.push(self.cells.get(&(r, c)).map(Cell::text).unwrap_or_default());
            }
            rows.push(row);
        }
        rows
    }
}

// ---------------------------------------------------------------------------
// findCells / findSpreadsheetsFromCells
// ---------------------------------------------------------------------------

/// `SpreadsheetExtractionAlgorithm.findCells`: cells are the rectangles
/// bounded by four intersection points sharing the same rulings.
fn find_cells(h: &[Ruling], v: &[Ruling]) -> Vec<Cell> {
    let intersections = Ruling::find_intersections(h, v);
    let key_of = |x: f64, y: f64| (y.to_bits(), x.to_bits());
    let mut points: Vec<(f64, f64)> = intersections
        .keys()
        .map(|&(yb, xb)| (f64::from_bits(xb), f64::from_bits(yb)))
        .collect();
    // Y_FIRST comparator: rounded y, then rounded x.
    points.sort_by(|a, b| point_cmp_yx(*a, *b));

    // Column/row indexes so each top-left candidate scans only the
    // intersections sharing its x or y, not every later point. Java's
    // loop is O(points²); the `idx >= i` filter preserves its
    // `points[i..]` suffix bound exactly (stable-sort order can place
    // a coordinate-matching point before i on round2 ties).
    type PointIndex = HashMap<u64, Vec<(usize, (f64, f64))>>;
    let mut by_x: PointIndex = HashMap::new();
    let mut by_y: PointIndex = HashMap::new();
    for (idx, &p) in points.iter().enumerate() {
        by_x.entry(coord_bits(p.0)).or_default().push((idx, p));
        by_y.entry(coord_bits(p.1)).or_default().push((idx, p));
    }

    let lookup = |p: (f64, f64)| intersections.get(&key_of(p.0, p.1));
    let mut cells = Vec::new();
    for (i, &tl) in points.iter().enumerate() {
        let Some(hv) = lookup(tl) else { continue };
        let x_points: &[(usize, (f64, f64))] = by_x
            .get(&coord_bits(tl.0))
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let y_points: &[(usize, (f64, f64))] = by_y
            .get(&coord_bits(tl.1))
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        'outer: for &(j, xp) in x_points {
            if j < i || xp.1.partial_cmp(&tl.1) != Some(std::cmp::Ordering::Greater) {
                continue;
            }
            let Some(xpv) = lookup(xp) else { continue };
            if hv.1 != xpv.1 {
                continue;
            }
            for &(k, yp) in y_points {
                if k < i || yp.0.partial_cmp(&tl.0) != Some(std::cmp::Ordering::Greater) {
                    continue;
                }
                let Some(ypv) = lookup(yp) else { continue };
                if hv.0 != ypv.0 {
                    continue;
                }
                let br = (yp.0, xp.1);
                if let Some(brv) = lookup(br) {
                    if brv.0 == xpv.0 && brv.1 == ypv.1 {
                        cells.push(Cell {
                            rect: Rect {
                                x: tl.0,
                                y: tl.1,
                                w: br.0 - tl.0,
                                h: br.1 - tl.1,
                            },
                            chunks: Vec::new(),
                        });
                    }
                }
                break 'outer;
            }
        }
    }
    cells
}

/// Bit key for exact coordinate equality like Java's `==`, with -0.0
/// folded onto 0.0 (they compare equal). NaN coordinates never satisfy
/// the callers' strict inequalities, so their bit pattern is moot.
fn coord_bits(v: f64) -> u64 {
    (if v == 0.0 { 0.0 } else { v }).to_bits()
}

/// `findSpreadsheetsFromCells`: symmetric-difference boundary tracing
/// produces one bounding rect per connected cell cluster.
fn find_spreadsheets(cells: &[Cell]) -> Vec<Rect> {
    // Dedup on exact rect equality, like `new HashSet<>(cells)`.
    // Bit-keyed set is O(n); NaN rects fall back to the O(n²) scan
    // because `==` is always false for them (duplicates are kept).
    let has_nan = cells
        .iter()
        .any(|c| c.rect.x.is_nan() || c.rect.y.is_nan() || c.rect.w.is_nan() || c.rect.h.is_nan());
    let mut deduped: Vec<Cell> = Vec::new();
    if has_nan {
        for c in cells {
            if !deduped.iter().any(|d| d.rect == c.rect) {
                deduped.push(c.clone());
            }
        }
    } else {
        let mut seen = HashSet::new();
        for c in cells {
            if seen.insert(rect_bits(&c.rect)) {
                deduped.push(c.clone());
            }
        }
    }
    deduped.sort_by(|a, b| ill_defined_order(&a.rect, &b.rect, 0, 0));

    // Symmetric difference of corner points: shared vertices cancel.
    let mut point_set: HashSet<(u64, u64)> = HashSet::new();
    for c in &deduped {
        let r = &c.rect;
        for pt in [
            Pt { x: r.x, y: r.y },
            Pt {
                x: r.right(),
                y: r.y,
            },
            Pt {
                x: r.right(),
                y: r.bottom(),
            },
            Pt {
                x: r.x,
                y: r.bottom(),
            },
        ] {
            let k = pt_key(pt);
            if !point_set.remove(&k) {
                point_set.insert(k);
            }
        }
    }
    let n = point_set.len();
    if n == 0 {
        return Vec::new();
    }

    let mut points_x: Vec<Pt> = point_set.iter().map(|&k| unpt_key(k)).collect();
    points_x.sort_by(|a, b| point_cmp_xy((a.x, a.y), (b.x, b.y)));
    let mut points_y: Vec<Pt> = point_set.iter().map(|&k| unpt_key(k)).collect();
    points_y.sort_by(|a, b| point_cmp_yx((a.x, a.y), (b.x, b.y)));

    // Pair consecutive points within each feq-run. Java indexes
    // i+1 blindly and would throw on an odd run — that propagates to an
    // empty extraction, mirrored here.
    let mut edges_h: HashMap<(u64, u64), (u64, u64)> = HashMap::new();
    let mut i = 0;
    while i < n {
        let curr_y = points_y[i].y;
        while i < n && feq(points_y[i].y, curr_y) {
            if i + 1 >= n {
                return Vec::new();
            }
            let a = pt_key(points_y[i]);
            let b = pt_key(points_y[i + 1]);
            edges_h.insert(a, b);
            edges_h.insert(b, a);
            i += 2;
        }
    }
    let mut edges_v: HashMap<(u64, u64), (u64, u64)> = HashMap::new();
    i = 0;
    while i < n {
        let curr_x = points_x[i].x;
        while i < n && feq(points_x[i].x, curr_x) {
            if i + 1 >= n {
                return Vec::new();
            }
            let a = pt_key(points_x[i]);
            let b = pt_key(points_x[i + 1]);
            edges_v.insert(a, b);
            edges_v.insert(b, a);
            i += 2;
        }
    }

    // Trace polygons. `edges_h` keys are BTreeMap-ordered for
    // determinism; Java's HashMap order only changes traversal start,
    // not the resulting rect set.
    let mut edges_h: BTreeMap<(u64, u64), (u64, u64)> =
        edges_h.into_iter().collect::<BTreeMap<_, _>>();
    let mut edges_v: HashMap<(u64, u64), (u64, u64)> = edges_v;
    let mut polygons: Vec<Vec<Pt>> = Vec::new();
    let mut guard = 0usize;
    while let Some((&first_key, _)) = edges_h.iter().next() {
        guard += 1;
        if guard > 100_000 {
            break;
        }
        let mut polygon = vec![unpt_key(first_key)];
        edges_h.remove(&first_key);
        let mut horiz = true; // last edge direction: H means next lookup is vertical
        loop {
            let curr = *polygon.last().unwrap();
            let next = if horiz {
                match edges_v.remove(&pt_key(curr)) {
                    Some(n) => {
                        horiz = false;
                        n
                    }
                    None => return Vec::new(),
                }
            } else {
                match edges_h.remove(&pt_key(curr)) {
                    Some(n) => {
                        horiz = true;
                        n
                    }
                    None => return Vec::new(),
                }
            };
            let next_pt = unpt_key(next);
            if next_pt == polygon[0] {
                break;
            }
            polygon.push(next_pt);
        }
        for pt in &polygon {
            edges_h.remove(&pt_key(*pt));
            edges_v.remove(&pt_key(*pt));
        }
        polygons.push(polygon);
    }

    let mut rects = Vec::new();
    for poly in polygons {
        let mut top = f64::MAX;
        let mut left = f64::MAX;
        let mut bottom = f64::MIN;
        let mut right = f64::MIN;
        for pt in &poly {
            top = top.min(pt.y);
            left = left.min(pt.x);
            bottom = bottom.max(pt.y);
            right = right.max(pt.x);
        }
        rects.push(Rect {
            x: left,
            y: top,
            w: right - left,
            h: bottom - top,
        });
    }
    rects.sort_by(|a, b| ill_defined_order(a, b, 0, 0));
    rects
}

// ---------------------------------------------------------------------------
// Page transform and content walking
// ---------------------------------------------------------------------------

type Matrix = [f64; 6];
const IDENTITY: Matrix = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

fn mat_mul(m: Matrix, n: Matrix) -> Matrix {
    [
        m[0] * n[0] + m[2] * n[1],
        m[1] * n[0] + m[3] * n[1],
        m[0] * n[2] + m[2] * n[3],
        m[1] * n[2] + m[3] * n[3],
        m[0] * n[4] + m[2] * n[5] + m[4],
        m[1] * n[4] + m[3] * n[5] + m[5],
    ]
}

fn apply(m: Matrix, x: f64, y: f64) -> (f64, f64) {
    (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5])
}

fn num(object: &Object) -> Option<f64> {
    match object {
        Object::Integer(i) => Some(*i as f64),
        Object::Real(r) => Some(*r as f64),
        _ => None,
    }
}

fn matrix_operand(operands: &[Object]) -> Option<Matrix> {
    if operands.len() < 6 {
        return None;
    }
    let mut m = [0.0; 6];
    for (i, slot) in m.iter_mut().enumerate() {
        *slot = num(&operands[i])?;
    }
    Some(m)
}

/// The `/Resources` chain for path walking: page resources plus a form
/// XObject's own (with fallback to the caller's, like `text.rs`).
struct Resources<'a> {
    doc: &'a Document,
    dicts: Vec<&'a Dictionary>,
}

impl<'a> Resources<'a> {
    fn section(&self, name: &[u8]) -> Option<&'a Dictionary> {
        for dict in &self.dicts {
            if let Ok(value) = dict.get(name) {
                if let Ok((_, Object::Dictionary(section))) = self.doc.dereference(value) {
                    return Some(section);
                }
            }
        }
        None
    }

    fn xobject(&self, name: &[u8]) -> Option<(ObjectId, &'a Dictionary)> {
        let value = self.section(b"XObject")?.get(name).ok()?;
        let (id, object) = self.doc.dereference(value).ok()?;
        match object {
            Object::Stream(stream) => Some((id?, &stream.dict)),
            _ => None,
        }
    }

    fn of_form(&self, form: &'a Dictionary) -> Self {
        let mut dicts = Vec::new();
        if let Ok(value) = form.get(b"Resources") {
            if let Ok((_, Object::Dictionary(dict))) = self.doc.dereference(value) {
                dicts.push(dict);
            }
        }
        dicts.extend(self.dicts.iter().copied());
        Self {
            doc: self.doc,
            dicts,
        }
    }
}

fn page_resources<'a>(doc: &'a Document, page_id: ObjectId) -> Resources<'a> {
    let mut dicts = Vec::new();
    if let Ok((inline, ids)) = doc.get_page_resources(page_id) {
        if let Some(dict) = inline {
            dicts.push(dict);
        }
        for id in ids {
            if let Ok(dict) = doc.get_dictionary(id) {
                dicts.push(dict);
            }
        }
    }
    Resources { doc, dicts }
}

/// Path commands in user space (post-CTM); the page transform is
/// applied when segments are emitted, like `getPathIterator(pageTransform)`.
#[derive(Clone, Copy, Debug)]
enum Cmd {
    MoveTo(Pt),
    LineTo(Pt),
    Close,
    Curve,
}

struct PathWalk {
    ctm: Matrix,
    stack: Vec<(Matrix, Option<Rect>, bool)>,
    cmds: Vec<Cmd>,
    current: Pt,
    subpath_start: Pt,
    clip: Option<Rect>,
    clip_pending: bool,
    rulings: Vec<Ruling>,
    ruling_limit_hit: bool,
    pt: Matrix,
}

impl PathWalk {
    fn point(&self, x: f64, y: f64) -> Pt {
        let (ux, uy) = apply(self.ctm, x, y);
        let (tx, ty) = apply(self.pt, ux, uy);
        Pt { x: tx, y: ty }
    }

    fn walk(&mut self, resources: &Resources, operations: &[Operation], depth: usize) {
        for op in operations {
            let operands = &op.operands;
            match op.operator.as_str() {
                "q" => self.stack.push((self.ctm, self.clip, self.clip_pending)),
                "Q" => {
                    if let Some((ctm, clip, pending)) = self.stack.pop() {
                        self.ctm = ctm;
                        self.clip = clip;
                        self.clip_pending = pending;
                    }
                }
                "cm" => {
                    if let Some(m) = matrix_operand(operands) {
                        self.ctm = mat_mul(self.ctm, m);
                    }
                }
                "m" => {
                    if let (Some(x), Some(y)) = (
                        operands.first().and_then(num),
                        operands.get(1).and_then(num),
                    ) {
                        let p = self.point(x, y);
                        self.cmds.push(Cmd::MoveTo(p));
                        self.current = p;
                        self.subpath_start = p;
                    }
                }
                "l" => {
                    if let (Some(x), Some(y)) = (
                        operands.first().and_then(num),
                        operands.get(1).and_then(num),
                    ) {
                        let p = self.point(x, y);
                        self.cmds.push(Cmd::LineTo(p));
                        self.current = p;
                    }
                }
                "c" | "v" | "y" => {
                    // Mark the path as curved; the filter rejects it at
                    // paint time. Track the endpoint for subsequent ops.
                    self.cmds.push(Cmd::Curve);
                    let idx = if op.operator == "c" { 4 } else { 2 };
                    if let (Some(x), Some(y)) = (
                        operands.get(idx).and_then(num),
                        operands.get(idx + 1).and_then(num),
                    ) {
                        self.current = self.point(x, y);
                    }
                }
                "h" => {
                    self.cmds.push(Cmd::Close);
                    self.current = self.subpath_start;
                }
                "re" => {
                    if operands.len() >= 4 {
                        if let (Some(x), Some(y), Some(w), Some(h)) = (
                            operands.first().and_then(num),
                            operands.get(1).and_then(num),
                            operands.get(2).and_then(num),
                            operands.get(3).and_then(num),
                        ) {
                            // Each corner through the CTM, like PDFBox's
                            // AppendRectangleToPath.
                            let pts = [
                                self.point(x, y),
                                self.point(x + w, y),
                                self.point(x + w, y + h),
                                self.point(x, y + h),
                            ];
                            self.cmds.push(Cmd::MoveTo(pts[0]));
                            self.cmds.push(Cmd::LineTo(pts[1]));
                            self.cmds.push(Cmd::LineTo(pts[2]));
                            self.cmds.push(Cmd::LineTo(pts[3]));
                            self.cmds.push(Cmd::Close);
                            self.current = pts[3];
                            self.subpath_start = pts[0];
                        }
                    }
                }
                "S" | "f" | "F" | "f*" | "B" | "B*" => self.paint(),
                "s" | "b" | "b*" => {
                    self.cmds.push(Cmd::Close);
                    self.paint();
                }
                "n" => self.end_path(),
                "W" | "W*" => self.clip_pending = true,
                "sh" => {}
                "Do" => self.do_xobject(resources, operands, depth),
                _ => {}
            }
        }
    }

    fn do_xobject(&mut self, resources: &Resources, operands: &[Object], depth: usize) {
        const MAX_DEPTH: usize = 16;
        if depth >= MAX_DEPTH {
            return;
        }
        let Some(name) = operands.first().and_then(|o| o.as_name().ok()) else {
            return;
        };
        let Some((id, dict)) = resources.xobject(name) else {
            return;
        };
        let is_form = dict
            .get(b"Subtype")
            .ok()
            .and_then(|s| s.as_name().ok())
            .is_some_and(|s| s == &b"Form"[..]);
        if !is_form {
            return;
        }
        let saved_ctm = self.ctm;
        let saved_clip = self.clip;
        let saved_pending = self.clip_pending;
        let saved_cmds = std::mem::take(&mut self.cmds);
        if let Some(m) = dict
            .get(b"Matrix")
            .ok()
            .and_then(|v| v.as_array().ok())
            .and_then(|a| matrix_operand(a))
        {
            self.ctm = mat_mul(self.ctm, m);
        }
        let form_resources = resources.of_form(dict);
        let content = resources
            .doc
            .get_object(id)
            .ok()
            .and_then(|o| match o {
                Object::Stream(s) => s.decompressed_content().ok(),
                _ => None,
            })
            .and_then(|bytes| Content::decode(&bytes).ok());
        if let Some(content) = content {
            self.walk(&form_resources, &content.operations, depth + 1);
        }
        self.ctm = saved_ctm;
        self.clip = saved_clip;
        self.clip_pending = saved_pending;
        self.cmds = saved_cmds;
    }

    /// `endPath`: apply a pending clip, then reset the path.
    fn end_path(&mut self) {
        if self.clip_pending {
            // Intersect the clip with the path's bounds; an empty path
            // empties the clip like `intersectClippingPath` does.
            let bounds = path_bounds(&self.cmds);
            self.clip = match (self.clip, bounds) {
                (Some(c), Some(b)) => Some(c.intersect_opt(&b)),
                _ => None,
            };
            self.clip_pending = false;
        }
        self.cmds.clear();
    }

    /// `strokeOrFillPath`: emit rulings for line-only paths.
    fn paint(&mut self) {
        // filterPathBySegmentType: must start with MoveTo and contain
        // only Move/Line/Close. Painting consumes the path either way.
        let cmds = std::mem::take(&mut self.cmds);
        let valid = matches!(cmds.first(), Some(Cmd::MoveTo(_)))
            && cmds.iter().all(|c| !matches!(c, Cmd::Curve));
        if !valid {
            return;
        }
        // Java's bookkeeping: the first MOVETO is consumed by
        // `getStartPoint` (coords rounded to 2 decimals) before the
        // segment loop; every iteration ends with startPoint = endPoint.
        let Cmd::MoveTo(first) = cmds[0] else {
            return;
        };
        let mut start_point = Pt {
            x: round2(first.x),
            y: round2(first.y),
        };
        let mut last_move = start_point;
        let mut end_point = first;
        for cmd in &cmds[1..] {
            match *cmd {
                Cmd::MoveTo(p) => {
                    last_move = p;
                    end_point = p;
                }
                Cmd::LineTo(p) => {
                    let line = line_between(start_point, p);
                    self.verify_line(line);
                    end_point = p;
                }
                Cmd::Close => {
                    let line = line_between(end_point, last_move);
                    self.verify_line(line);
                }
                Cmd::Curve => {}
            }
            start_point = end_point;
        }
    }

    fn verify_line(&mut self, (p1, p2): (Pt, Pt)) {
        // An emptied clip (`None`) rejects every ruling.
        let Some(clip) = self.clip else { return };
        let line = Ruling::new(p1.x, p1.y, p2.x, p2.y);
        if !line.intersects_rect(&clip) {
            return;
        }
        let ruling = line.intersect(&clip);
        if ruling.length() <= MIN_RULING_LENGTH {
            return;
        }
        if self.rulings.len() >= MAX_RULINGS {
            if !self.ruling_limit_hit {
                self.ruling_limit_hit = true;
                tracing::warn!("tabula: page exceeds {MAX_RULINGS} rulings; ignoring the rest");
            }
            return;
        }
        self.rulings.push(ruling);
    }
}

impl Rect {
    fn intersect_opt(&self, o: &Rect) -> Rect {
        let x = self.x.max(o.x);
        let y = self.y.max(o.y);
        let r = self.right().min(o.right());
        let b = self.bottom().min(o.bottom());
        Rect {
            x,
            y,
            w: (r - x).max(0.0),
            h: (b - y).max(0.0),
        }
    }
}

fn path_bounds(cmds: &[Cmd]) -> Option<Rect> {
    let mut b: Option<Rect> = None;
    for c in cmds {
        let p = match *c {
            Cmd::MoveTo(p) | Cmd::LineTo(p) => Some(p),
            _ => None,
        };
        if let Some(p) = p {
            let r = Rect {
                x: p.x,
                y: p.y,
                w: 0.0,
                h: 0.0,
            };
            b = Some(match b {
                Some(old) => old.union(&r),
                None => r,
            });
        }
    }
    b
}

/// `getLineBetween` + `PointComparator`: order endpoints by
/// (round2(y), round2(x)).
fn line_between(a: Pt, b: Pt) -> (Pt, Pt) {
    let ka = (round2(a.x), round2(a.y));
    let kb = (round2(b.x), round2(b.y));
    if point_cmp_yx(ka, kb) == std::cmp::Ordering::Less {
        (a, b)
    } else {
        (b, a)
    }
}

// ---------------------------------------------------------------------------
// Page-level extraction
// ---------------------------------------------------------------------------

/// `ObjectExtractorStreamEngine`'s page transform: crop-box origin,
/// y-flip, then rotation for 90°/270° pages. Returns the matrix plus
/// the displayed page size.
fn page_transform(doc: &Document, page_id: ObjectId) -> Option<(Matrix, f64, f64)> {
    let boxes = pdfdoc::page_boxes(doc, page_id).ok()?;
    let crop = boxes.crop;
    let (llx, lly, urx, ury) = (
        crop[0] as f64,
        crop[1] as f64,
        crop[2] as f64,
        crop[3] as f64,
    );
    let (cw, ch) = (urx - llx, ury - lly);
    let rot = pdfdoc::page_rotation(doc, page_id).unwrap_or(0);
    let translate = |x: f64, y: f64| [1.0, 0.0, 0.0, 1.0, x, y];
    let scale_flip = [1.0, 0.0, 0.0, -1.0, 0.0, 0.0];
    let to_origin = translate(-llx, -lly);
    let pt = if rot.abs() == 90 || rot.abs() == 270 {
        let rad = (rot as f64) * std::f64::consts::PI / 180.0;
        let (s, c) = rad.sin_cos();
        // Java2D rotate(θ): x' = cos·x − sin·y, y' = sin·x + cos·y.
        let r: Matrix = [c, s, -s, c, 0.0, 0.0];
        mat_mul(mat_mul(r, scale_flip), to_origin)
    } else {
        mat_mul(mat_mul(translate(0.0, ch), scale_flip), to_origin)
    };
    let (w, h) = if rot.abs() == 90 || rot.abs() == 270 {
        (ch, cw)
    } else {
        (cw, ch)
    };
    Some((pt, w, h))
}

/// `Utils.snapPoints`: cluster ruling endpoints within the min-char
/// thresholds and move each cluster to its mean. Java's quirk — the
/// last point in sorted order never joins a group — is preserved.
fn snap_points(rulings: &mut [Ruling], x_threshold: f64, y_threshold: f64) {
    if rulings.is_empty() {
        return;
    }
    // points[2i], points[2i+1] are ruling i's endpoints, in order.
    let mut pts: Vec<Pt> = rulings
        .iter()
        .flat_map(|r| [Pt { x: r.x1, y: r.y1 }, Pt { x: r.x2, y: r.y2 }])
        .collect();

    let snap = |pts: &mut Vec<Pt>, get: fn(&Pt) -> f64, set: fn(&mut Pt, f64), threshold: f64| {
        let mut order: Vec<usize> = (0..pts.len()).collect();
        order.sort_by(|&a, &b| cmp_f64(get(&pts[a]), get(&pts[b])));
        // group indices in sorted order; group key = first member's coord
        let mut groups: Vec<Vec<usize>> = vec![vec![order[0]]];
        for &idx in order.iter().skip(1).take(order.len().saturating_sub(2)) {
            let last = groups.last().unwrap();
            if (get(&pts[idx]) - get(&pts[last[0]])).abs() < threshold {
                groups.last_mut().unwrap().push(idx);
            } else {
                groups.push(vec![idx]);
            }
        }
        for group in groups {
            let avg = group.iter().map(|&i| get(&pts[i])).sum::<f64>() / group.len() as f64;
            for &i in &group {
                set(&mut pts[i], avg);
            }
        }
    };

    snap(&mut pts, |p| p.x, |p, v| p.x = v, x_threshold);
    snap(&mut pts, |p| p.y, |p, v| p.y = v, y_threshold);

    for (i, r) in rulings.iter_mut().enumerate() {
        r.x1 = pts[2 * i].x;
        r.y1 = pts[2 * i].y;
        r.x2 = pts[2 * i + 1].x;
        r.y2 = pts[2 * i + 1].y;
    }
}

/// Pages with more rulings than this are truncated: the
/// horizontal×vertical intersection sweep is roughly quadratic in the
/// worst case, so unbounded adversarial content could hang extraction.
const MAX_RULINGS: usize = 20_000;

/// Raw rulings from the page's path operations, in tabula space.
fn page_rulings(doc: &Document, page_id: ObjectId, pt: Matrix, w: f64, h: f64) -> Vec<Ruling> {
    if !(w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0) {
        return Vec::new();
    }
    let resources = page_resources(doc, page_id);
    let mut walk = PathWalk {
        ctm: IDENTITY,
        stack: Vec::new(),
        cmds: Vec::new(),
        current: Pt::default(),
        subpath_start: Pt::default(),
        clip: Some(Rect {
            x: 0.0,
            y: 0.0,
            w,
            h,
        }),
        clip_pending: false,
        rulings: Vec::new(),
        ruling_limit_hit: false,
        pt,
    };
    if let Ok(content) = doc.get_and_decode_page_content(page_id) {
        walk.walk(&resources, &content.operations, 0);
    }
    walk.rulings
}

/// `Page.getRulings`: snap, split, collapse.
fn clean_rulings(
    mut rulings: Vec<Ruling>,
    min_char_w: f64,
    min_char_h: f64,
) -> (Vec<Ruling>, Vec<Ruling>) {
    snap_points(&mut rulings, min_char_w, min_char_h);
    let vertical: Vec<Ruling> = rulings.iter().copied().filter(|r| r.vertical()).collect();
    let horizontal: Vec<Ruling> = rulings.iter().copied().filter(|r| r.horizontal()).collect();
    (
        Ruling::collapse_oriented(&vertical),
        Ruling::collapse_oriented(&horizontal),
    )
}

/// Build tabula-space text elements from per-glyph `CharItem`s, with
/// `TextStripper.writeString`'s filtering (printable check, NBSP→space,
/// oversized-blank rejection) and `computeFontHeight`-style heights.
fn text_elements(chars: &[CharItem], pt: Matrix) -> (Vec<TextEl>, f64, f64) {
    let mut elements = Vec::new();
    let mut min_w = f64::MAX;
    let mut min_h = f64::MAX;
    let mut count_height = 0usize;
    let mut total_height = 0.0f64;

    for ch in chars {
        let text = if ch.text == "\u{a0}" {
            " ".to_string()
        } else {
            ch.text.clone()
        };
        // Printable check per character.
        if !text.chars().all(is_printable) {
            continue;
        }
        // Glyph box in user space: baseline origin, advance along `dir`,
        // height perpendicular (above the baseline for dir=0).
        let theta = ch.direction.to_radians();
        let (sin_t, cos_t) = theta.sin_cos();
        let (sin_u, cos_u) = (theta + std::f64::consts::FRAC_PI_2).sin_cos();
        let (a, d) = fontmetrics::ascent_descent(&ch.font);
        let height_factor = ((a - d) / 2.0 / 1000.0).max(0.01);
        let glyph_h = height_factor * ch.font_size;
        let corners = [
            (ch.x, ch.y),
            (ch.x + ch.w * cos_t, ch.y + ch.w * sin_t),
            (ch.x + glyph_h * cos_u, ch.y + glyph_h * sin_u),
            (
                ch.x + ch.w * cos_t + glyph_h * cos_u,
                ch.y + ch.w * sin_t + glyph_h * sin_u,
            ),
        ];
        let mut min_x = f64::MAX;
        let mut min_y = f64::MAX;
        let mut max_x = f64::MIN;
        let mut max_y = f64::MIN;
        for &(ux, uy) in &corners {
            let (tx, ty) = apply(pt, ux, uy);
            min_x = min_x.min(tx);
            min_y = min_y.min(ty);
            max_x = max_x.max(tx);
            max_y = max_y.max(ty);
        }
        let el = TextEl {
            text,
            x: round2(min_x),
            y: round2(min_y),
            w: round2(max_x - min_x),
            h: round2(max_y - min_y),
            font_size: ch.font_size,
            font: ch.font.clone(),
            space_w: ch.space_w,
            dir: ch.direction,
        };
        min_w = min_w.min(el.w);
        min_h = min_h.min(el.h);
        count_height += 1;
        total_height += el.h;
        let avg_height = total_height / count_height as f64;
        // Oversized blank rejection (TextStripper.writeString).
        if el.text.trim().is_empty()
            && ((avg_height > 0.0 && el.h >= avg_height * 6.0)
                || el.font_size > 40.0
                || el.font_size < 2.0)
        {
            continue;
        }
        elements.push(el);
    }
    if elements.is_empty() {
        (elements, f64::MAX, f64::MAX)
    } else {
        (elements, min_w, min_h)
    }
}

/// Sort key for `text_in_rect`'s candidate list: NaN right edges sort
/// last so `partition_point` stays monotonic. The exact `<=` check
/// still runs in the filter, so this only widens the scanned prefix.
fn right_sort_key(e: &TextEl) -> f64 {
    let v = e.right();
    if v.is_nan() {
        f64::INFINITY
    } else {
        v
    }
}

/// Elements fully inside `rect`, sorted by `ILL_DEFINED_ORDER` —
/// `RectangleSpatialIndex.contains` + the TextElement comparator.
/// `candidates` must be sorted by `right_sort_key`; the leading index
/// preserves the caller's element order for stable tie-breaks.
fn text_in_rect(candidates: &[(usize, &TextEl)], rect: &Rect) -> Vec<TextEl> {
    let r = rect;
    let k = candidates.partition_point(|&(_, e)| right_sort_key(e) <= r.right());
    let mut out: Vec<(usize, TextEl)> = candidates[..k]
        .iter()
        .filter(|(_, e)| {
            let er = e.rect();
            er.x >= r.x && er.y >= r.y && er.right() <= r.right() && er.bottom() <= r.bottom()
        })
        .map(|&(i, e)| (i, e.clone()))
        .collect();
    out.sort_by(|a, b| ill_defined_order(&a.1.rect(), &b.1.rect(), 0, 0).then(a.0.cmp(&b.0)));
    out.into_iter().map(|(_, e)| e).collect()
}

/// `rowsOfCells`: sort by top, group while consecutive tops are feq —
/// `lastTop` updates every cell, so grouping chains.
fn group_rows(cells: Vec<&Cell>) -> Vec<Vec<&Cell>> {
    let mut sorted = cells;
    sorted.sort_by(|a, b| cmp_f64(a.rect.y, b.rect.y));
    let mut rows: Vec<Vec<&Cell>> = Vec::new();
    let mut last_top = f64::NAN;
    for c in sorted {
        let top = c.rect.y;
        let same = rows.last().is_some() && feq(top, last_top);
        if same {
            rows.last_mut().unwrap().push(c);
        } else {
            rows.push(vec![c]);
        }
        last_top = top;
    }
    rows
}

/// `TableWithRulingLines.addCells`: the spatial index holds only the
/// area's overlapping cells; group them into rows by top edge (chain
/// `feq` like `rowsOfCells`), offset each row's first column by the
/// deepest contained row below it.
fn build_table(area_cells: Vec<&Cell>) -> Table {
    if area_cells.is_empty() {
        return Table::default();
    }
    let si_bounds = area_cells.iter().fold(None::<Rect>, |acc, c| {
        Some(match acc {
            Some(r) => r.union(&c.rect),
            None => c.rect,
        })
    });
    let Some(si_bounds) = si_bounds else {
        return Table::default();
    };

    let rows = group_rows(area_cells);
    let probe_cells: Vec<&Cell> = rows.iter().flat_map(|r| r.iter().copied()).collect();
    let contains = |r: &Rect| -> Vec<&Cell> {
        probe_cells
            .iter()
            .copied()
            .filter(|c| r.contains(&c.rect))
            .collect()
    };

    let mut table = Table::default();
    for (i, row) in rows.iter().enumerate() {
        let first = row[0];
        let probe = Rect {
            x: si_bounds.x,
            y: first.rect.bottom(),
            w: first.rect.x - si_bounds.x,
            h: si_bounds.bottom() - first.rect.bottom(),
        };
        let others = group_rows(contains(&probe));
        let start_column = others.iter().map(Vec::len).max().unwrap_or(0);
        for (col, &cell) in (start_column..).zip(row.iter()) {
            table.add(cell.clone(), i, col);
        }
    }
    table
}

/// One spreadsheet's worth of output: rows of raw cell texts.
#[derive(Debug)]
pub struct ExtractedTable {
    pub rows: Vec<Vec<String>>,
}

// ---------------------------------------------------------------------------
// Borderless (text-edge) detection
// ---------------------------------------------------------------------------
//
// Java's `SpreadsheetExtractionAlgorithm` derives cell boundaries from
// aligned text as well as rulings. The port here is the conservative end
// of that idea: a table is a run of >=2 consecutive text lines that each
// split into >=2 cells at gaps wider than any word space, separated by
// x-ranges that stay empty across every line of the run. Prose has no
// such persistent gutters, so it does not qualify.

/// A word-level box: `merge_words` chunks are line runs that may contain
/// synthetic space elements, so column gaps only appear between words.
#[derive(Clone, Debug)]
struct WordBox {
    rect: Rect,
    text: String,
}

struct BorderlessLine {
    bbox: Rect,
    words: Vec<WordBox>,
    qualifies: bool,
}

fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| cmp_f64(*a, *b));
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        Some((values[mid - 1] + values[mid]) / 2.0)
    } else {
        Some(values[mid])
    }
}

/// Column gaps must clear a normal inter-word space but stay under a
/// real gutter; both terms scale with the page's own font metrics.
fn column_gap_threshold(elements: &[TextEl]) -> f64 {
    let mut spaces: Vec<f64> = elements
        .iter()
        .map(|e| e.space_w)
        .filter(|w| *w > 0.0)
        .collect();
    let mut widths: Vec<f64> = elements.iter().map(|e| e.w).filter(|w| *w > 0.0).collect();
    let space = median(&mut spaces).unwrap_or(3.0);
    let width = median(&mut widths).unwrap_or(space);
    (3.0 * space).max(1.5 * width).max(1.0)
}

fn is_horizontal(el: &TextEl) -> bool {
    let d = el.dir.rem_euclid(360.0);
    d < 20.0 || d > 340.0 || (160.0..=200.0).contains(&d)
}

/// Group horizontal characters into baseline lines: sorted by top edge,
/// a char joins the previous line while it still overlaps it vertically
/// by at least half its own height.
fn text_lines(elements: &[TextEl]) -> Vec<BorderlessLine> {
    let mut sorted: Vec<TextEl> = elements
        .iter()
        .filter(|e| is_horizontal(e) && !e.text.trim().is_empty())
        .cloned()
        .collect();
    sorted
        .sort_by(|a, b| cmp_f64(round2(a.y), round2(b.y)).then(cmp_f64(round2(a.x), round2(b.x))));
    let mut raw: Vec<(Rect, Vec<TextEl>)> = Vec::new();
    for el in sorted {
        let r = el.rect();
        if let Some((bbox, els)) = raw.last_mut() {
            let overlap_h = (r.bottom().min(bbox.bottom()) - r.y.max(bbox.y)).max(0.0);
            if overlap_h >= 0.5 * r.h.min(bbox.h) {
                *bbox = bbox.union(&r);
                els.push(el);
                continue;
            }
        }
        raw.push((r, vec![el]));
    }
    raw.into_iter()
        .map(|(bbox, mut els)| {
            els.sort_by(|a, b| cmp_f64(round2(a.x), round2(b.x)));
            let mut words = Vec::new();
            for chunk in merge_words(els) {
                words.extend(chunk_words(&chunk));
            }
            words.sort_by(|a, b| cmp_f64(round2(a.rect.x), round2(b.rect.x)));
            BorderlessLine {
                bbox,
                words,
                qualifies: false,
            }
        })
        .collect()
}

/// Split a line-run chunk at its whitespace elements into word boxes.
fn chunk_words(chunk: &Chunk) -> Vec<WordBox> {
    let mut words = Vec::new();
    let mut cur: Vec<&TextEl> = Vec::new();
    let flush = |cur: &mut Vec<&TextEl>, words: &mut Vec<WordBox>| {
        if cur.is_empty() {
            return;
        }
        let mut rect = cur[0].rect();
        let mut text = String::new();
        for el in cur.iter() {
            rect = rect.union(&el.rect());
            text.push_str(&el.text);
        }
        words.push(WordBox {
            rect,
            text: text.trim().to_string(),
        });
        cur.clear();
    };
    for el in &chunk.els {
        if el.text.trim().is_empty() {
            flush(&mut cur, &mut words);
        } else {
            cur.push(el);
        }
    }
    flush(&mut cur, &mut words);
    words.retain(|w| !w.text.is_empty());
    words
}

fn has_column_gap(words: &[WordBox], min_gap: f64) -> bool {
    words
        .windows(2)
        .any(|w| w[1].rect.x - w[0].rect.right() >= min_gap)
}

/// Text-edge tables on one page, each paired with its bounding rect so
/// the caller can drop detections that a ruled table already covers.
fn borderless_tables(elements: &[TextEl]) -> Vec<(ExtractedTable, Rect)> {
    let min_gap = column_gap_threshold(elements);
    let mut lines = text_lines(elements);
    for line in &mut lines {
        line.qualifies = line.words.len() >= 2 && has_column_gap(&line.words, min_gap);
    }
    let mut fonts: Vec<f64> = elements.iter().map(|e| e.font_size).collect();
    let med_font = median(&mut fonts).unwrap_or(10.0);
    // Row whitespace scales with font size, not glyph-box height: a
    // blank line roughly doubles the leading, so ~3 font sizes separates
    // tables while ordinary row padding stays well under it.
    let max_row_gap = 3.0 * med_font;

    let mut tables = Vec::new();
    let mut region: Vec<usize> = Vec::new();
    let mut prev_bottom = f64::NAN;
    let flush = |region: &mut Vec<usize>,
                 lines: &[BorderlessLine],
                 tables: &mut Vec<(ExtractedTable, Rect)>| {
        if region.len() >= 2 {
            if let Some(t) = table_from_region(region, lines, min_gap) {
                tables.push(t);
            }
        }
        region.clear();
    };
    for (i, line) in lines.iter().enumerate() {
        if line.qualifies {
            let row_gap = if prev_bottom.is_nan() {
                0.0
            } else {
                line.bbox.y - prev_bottom
            };
            if !region.is_empty() && row_gap > max_row_gap {
                flush(&mut region, &lines, &mut tables);
            }
            region.push(i);
        } else {
            flush(&mut region, &lines, &mut tables);
        }
        prev_bottom = line.bbox.bottom();
    }
    flush(&mut region, &lines, &mut tables);
    tables
}

/// Columns are the x-bands between whitespace channels that no word in
/// the region crosses; rows keep only lines that still split into >=2
/// cells at those bands.
fn table_from_region(
    region: &[usize],
    lines: &[BorderlessLine],
    min_gap: f64,
) -> Option<(ExtractedTable, Rect)> {
    let mut intervals: Vec<(f64, f64)> = Vec::new();
    let mut left = f64::MAX;
    let mut right = f64::MIN;
    let mut top = f64::MAX;
    let mut bottom = f64::MIN;
    for &i in region {
        for w in &lines[i].words {
            intervals.push((w.rect.x, w.rect.right()));
            left = left.min(w.rect.x);
            right = right.max(w.rect.right());
        }
        top = top.min(lines[i].bbox.y);
        bottom = bottom.max(lines[i].bbox.bottom());
    }
    if left.partial_cmp(&right) != Some(std::cmp::Ordering::Less) {
        return None;
    }
    intervals.sort_by(|a, b| cmp_f64(a.0, b.0));
    // Union of every word's x-extent; its complement inside the region
    // is the set of vertical whitespace channels.
    let mut gutters: Vec<(f64, f64)> = Vec::new();
    let mut cur = left;
    for &(x0, x1) in &intervals {
        if x0 > cur {
            gutters.push((cur, x0));
        }
        cur = cur.max(x1);
    }
    if right > cur {
        gutters.push((cur, right));
    }
    gutters.retain(|g| g.1 - g.0 >= min_gap);
    if gutters.is_empty() {
        return None;
    }
    // Column bands = region extent minus the gutters.
    let mut bands: Vec<(f64, f64)> = Vec::new();
    let mut bx = left;
    for &(g0, g1) in &gutters {
        if g0 > bx {
            bands.push((bx, g0));
        }
        bx = bx.max(g1);
    }
    if right > bx {
        bands.push((bx, right));
    }
    if bands.len() < 2 {
        return None;
    }

    // A word belongs to the band it overlaps most — center-in-gutter
    // spanning cells still land somewhere. Words entirely inside a
    // gutter (wide merged cells) fall back to the nearest band.
    let band_for = |w: &WordBox| -> Option<usize> {
        let mut best: Option<(usize, f64)> = None;
        for (b, &(b0, b1)) in bands.iter().enumerate() {
            let ov = w.rect.right().min(b1) - w.rect.x.max(b0);
            if ov > 0.0 && best.is_none_or(|(_, bo)| ov > bo) {
                best = Some((b, ov));
            }
        }
        if let Some((b, _)) = best {
            return Some(b);
        }
        let cx = (w.rect.x + w.rect.right()) / 2.0;
        bands
            .iter()
            .position(|&(b0, b1)| cx >= b0 && cx < b1)
            .or_else(|| {
                bands
                    .iter()
                    .enumerate()
                    .min_by(|(_, a), (_, b)| {
                        let da = if cx < a.0 { a.0 - cx } else { cx - a.1 };
                        let db = if cx < b.0 { b.0 - cx } else { cx - b.1 };
                        cmp_f64(da, db)
                    })
                    .map(|(i, _)| i)
            })
    };

    let mut rows: Vec<Vec<String>> = Vec::new();
    for &i in region {
        let mut cells: Vec<Vec<&WordBox>> = vec![Vec::new(); bands.len()];
        for w in &lines[i].words {
            if let Some(b) = band_for(w) {
                cells[b].push(w);
            }
        }
        let row: Vec<String> = cells
            .iter()
            .map(|ws| {
                ws.iter()
                    .map(|w| w.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect();
        // Rows that collapse to one cell are spanning/title rows — keep
        // them so merged-cell text is not silently lost.
        if row.iter().any(|c| !c.is_empty()) {
            rows.push(row);
        }
    }
    if rows.len() < 2 {
        return None;
    }
    // Drop bands that never received text; require >=2 surviving
    // columns or this was aligned prose, not a table.
    let used: Vec<usize> = (0..bands.len())
        .filter(|&b| rows.iter().any(|r| !r[b].is_empty()))
        .collect();
    if used.len() < 2 {
        return None;
    }
    let ncols = used.len();
    let rows = rows
        .into_iter()
        .map(|r| used.iter().map(|&b| r[b].clone()).collect::<Vec<_>>())
        .map(|r: Vec<String>| {
            let mut r = r;
            r.resize(ncols, String::new());
            r
        })
        .collect();
    let bounds = Rect {
        x: left,
        y: top,
        w: right - left,
        h: bottom - top,
    };
    Some((ExtractedTable { rows }, bounds))
}

/// Lattice extraction for one page — `SpreadsheetExtractionAlgorithm.extract`.
pub fn extract_tables(doc: &Document, page_id: ObjectId) -> Vec<ExtractedTable> {
    let Some((pt, page_w, page_h)) = page_transform(doc, page_id) else {
        return Vec::new();
    };
    let chars = text::page_chars(doc, page_id);
    let (elements, min_char_w, min_char_h) = text_elements(&chars, pt);

    let rulings = page_rulings(doc, page_id, pt, page_w, page_h);
    let mut tables = Vec::new();
    let mut lattice_areas: Vec<Rect> = Vec::new();
    if !rulings.is_empty() {
        let (vertical, horizontal) = clean_rulings(rulings, min_char_w, min_char_h);
        // extract() re-splits and re-collapses the already-oriented lists;
        // collapse is idempotent so one pass suffices.
        let vertical = Ruling::collapse_oriented(&vertical);
        let horizontal = Ruling::collapse_oriented(&horizontal);

        let mut cells = find_cells(&horizontal, &vertical);
        let areas = find_spreadsheets(&cells);
        lattice_areas = areas.clone();

        // Candidates sorted by right edge so each cell's containment scan
        // visits only the prefix that can satisfy `right <= cell.right`.
        let mut candidates: Vec<(usize, &TextEl)> = elements.iter().enumerate().collect();
        candidates.sort_by(|a, b| cmp_f64(right_sort_key(a.1), right_sort_key(b.1)));

        for area in areas {
            let mut overlapping_idx = Vec::new();
            for (i, c) in cells.iter_mut().enumerate() {
                if c.rect.intersects(&area) {
                    let inside = text_in_rect(&candidates, &c.rect);
                    c.chunks = merge_words(inside);
                    overlapping_idx.push(i);
                }
            }
            let overlapping: Vec<&Cell> = overlapping_idx.iter().map(|&i| &cells[i]).collect();
            let table = build_table(overlapping);
            tables.push(ExtractedTable { rows: table.rows() });
        }
    }

    // Text-edge pass covers borderless tables; a detection whose bounds
    // intersect a ruled spreadsheet is the same table seen twice, so
    // the lattice result wins.
    for (table, bounds) in borderless_tables(&elements) {
        if lattice_areas.iter().any(|a| a.intersects(&bounds)) {
            continue;
        }
        tables.push(table);
    }
    tables
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit;
    use lopdf::{dictionary, Document, Object};

    /// One-page PDF whose content stream is `ops`, with Helvetica 10pt
    /// available as `/F1` for `BT … Tj ET` runs.
    fn pdf_with_ops(ops: &str) -> Vec<u8> {
        let mut pdf = testkit::Pdf::new();
        let font = pdf.add(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let content = pdf.add_stream(dictionary! {}, ops.as_bytes());
        pdf.edit_page(|page| {
            page.set("Contents", Object::Reference(content));
            page.set(
                "Resources",
                dictionary! {
                    "Font" => dictionary! {
                        "F1" => Object::Reference(font),
                    },
                },
            );
        });
        pdf.bytes()
    }

    /// A 2×2 ruled grid: verticals at x=100/200/300, horizontals at
    /// y=500/600/700 (user space, y up), one letter per cell.
    fn ruled_grid_ops() -> &'static str {
        "100 500 m 100 700 l S \
         200 500 m 200 700 l S \
         300 500 m 300 700 l S \
         100 500 m 300 500 l S \
         100 600 m 300 600 l S \
         100 700 m 300 700 l S \
         BT /F1 10 Tf 110 650 Td (a) Tj ET \
         BT /F1 10 Tf 210 650 Td (b) Tj ET \
         BT /F1 10 Tf 110 550 Td (c) Tj ET \
         BT /F1 10 Tf 210 550 Td (d) Tj ET"
    }

    fn tables_of(bytes: &[u8]) -> Vec<ExtractedTable> {
        let doc = Document::load_mem(bytes).unwrap();
        let page_id = *doc.get_pages().values().next().unwrap();
        extract_tables(&doc, page_id)
    }

    #[test]
    fn ruled_grid_extracts_cell_text() {
        let tables = tables_of(&pdf_with_ops(ruled_grid_ops()));
        assert_eq!(tables.len(), 1, "expected one table: {tables:?}");
        assert_eq!(
            tables[0].rows,
            vec![
                vec!["a".to_string(), "b".to_string()],
                vec!["c".to_string(), "d".to_string()],
            ]
        );
    }

    /// Borderless table: two aligned columns of text, no rulings.
    /// Column x positions: col1 at 100, col2 at 300 — the ~150pt gap
    /// dwarfs the ~3pt word space, so gutters separate cleanly.
    fn borderless_grid_ops() -> &'static str {
        "BT /F1 10 Tf 100 700 Td (Name) Tj ET \
         BT /F1 10 Tf 300 700 Td (Age) Tj ET \
         BT /F1 10 Tf 100 680 Td (Alice) Tj ET \
         BT /F1 10 Tf 300 680 Td (30) Tj ET \
         BT /F1 10 Tf 100 660 Td (Bob Smith) Tj ET \
         BT /F1 10 Tf 300 660 Td (25) Tj ET"
    }

    #[test]
    fn borderless_grid_extracts_cells() {
        let tables = tables_of(&pdf_with_ops(borderless_grid_ops()));
        assert_eq!(tables.len(), 1, "expected one table: {tables:?}");
        assert_eq!(
            tables[0].rows,
            vec![
                vec!["Name".to_string(), "Age".to_string()],
                vec!["Alice".to_string(), "30".to_string()],
                vec!["Bob Smith".to_string(), "25".to_string()],
            ]
        );
    }

    #[test]
    fn prose_lines_yield_no_borderless_table() {
        // Three lines of ordinary prose: word gaps stay under the
        // column threshold, so no line qualifies.
        let ops = "BT /F1 10 Tf 100 700 Td (The quick brown fox) Tj ET \
                   BT /F1 10 Tf 100 680 Td (jumps over the lazy dog) Tj ET \
                   BT /F1 10 Tf 100 660 Td (and runs far away) Tj ET";
        assert!(tables_of(&pdf_with_ops(ops)).is_empty());
    }

    #[test]
    fn single_column_lines_yield_no_borderless_table() {
        // Aligned left edges but only one column of text per line.
        let ops = "BT /F1 10 Tf 100 700 Td (first item) Tj ET \
                   BT /F1 10 Tf 100 680 Td (second item) Tj ET \
                   BT /F1 10 Tf 100 660 Td (third item) Tj ET";
        assert!(tables_of(&pdf_with_ops(ops)).is_empty());
    }

    #[test]
    fn text_without_rulings_yields_no_tables() {
        let ops = "BT /F1 10 Tf 100 600 Td (lonely) Tj ET";
        assert!(tables_of(&pdf_with_ops(ops)).is_empty());
    }

    #[test]
    fn rulings_without_text_yield_no_tables() {
        // Java's TextStripper leaves minCharWidth/Height at Float.MAX_VALUE
        // on a textless page; snapPoints then collapses every ruling to a
        // point, so lattice mode finds nothing — same as here.
        let ops = "100 500 m 100 700 l S 200 500 m 200 700 l S \
                   100 500 m 200 500 l S 100 600 m 200 600 l S \
                   100 700 m 200 700 l S";
        assert!(tables_of(&pdf_with_ops(ops)).is_empty());
    }

    #[test]
    fn split_collinear_rulings_still_form_cells() {
        // Each grid edge drawn as two touching segments — Java's
        // collapseOrientedRulings must merge them into single rulings.
        let ops = "100 500 m 100 600 l S 100 600 m 100 700 l S \
                   200 500 m 200 700 l S \
                   100 500 m 200 500 l S \
                   100 700 m 200 700 l S \
                   BT /F1 10 Tf 110 650 Td (x) Tj ET";
        let tables = tables_of(&pdf_with_ops(ops));
        assert_eq!(tables.len(), 1, "{tables:?}");
        assert_eq!(tables[0].rows[0][0], "x");
    }

    #[test]
    fn ruling_normalize_snaps_near_axis_lines() {
        // A line 0.5° off horizontal still counts as a ruling.
        let ops = "100 500 m 200 501.7 l S \
                   100 500 m 200 500 l S \
                   100 600 m 200 600 l S \
                   100 500 m 100 600 l S \
                   200 500 m 200 600 l S \
                   BT /F1 10 Tf 110 550 Td (t) Tj ET";
        let tables = tables_of(&pdf_with_ops(ops));
        assert_eq!(tables.len(), 1, "{tables:?}");
    }

    #[test]
    fn round2_preserves_huge_coordinates() {
        // |d| >= ~9e16 used to overflow `int * 100` and collapse the
        // coordinate toward zero; past 1e15 rounding is below f64
        // resolution anyway, so the value passes through.
        for d in [1e15, 1e17, 1e300, -1e17] {
            assert_eq!(round2(d), d);
        }
        assert_eq!(round2(1.005), 1.01);
        assert_eq!(round2(-2.345), -2.35);
    }

    #[test]
    fn ruling_cap_bounds_adversarial_pages() {
        // More stroked segments than MAX_RULINGS: extraction must
        // terminate with a truncated ruling set, not hang in the
        // quadratic intersection sweep.
        let mut ops = String::new();
        for i in 0..(MAX_RULINGS + 100) {
            ops.push_str(&format!("10 {} m 20 {} l S ", 100 + i % 500, 100 + i % 500));
        }
        let bytes = pdf_with_ops(&ops);
        let doc = Document::load_mem(&bytes).unwrap();
        let page_id = *doc.get_pages().values().next().unwrap();
        let (pt, w, h) = page_transform(&doc, page_id).unwrap();
        let rulings = page_rulings(&doc, page_id, pt, w, h);
        assert_eq!(rulings.len(), MAX_RULINGS);
        // And the full pipeline still returns promptly.
        let _ = extract_tables(&doc, page_id);
    }

    #[test]
    fn large_grid_extracts_every_cell() {
        // 20×15 ruled grid — exercises the indexed find_cells path;
        // every cell carries its own label. Cell pitch must clear the
        // ~612pt page width and fit the label at 6pt.
        let mut ops = String::new();
        for c in 0..=20 {
            ops.push_str(&format!("{} 175 m {} 550 l S ", 50 + 25 * c, 50 + 25 * c));
        }
        for r in 0..=15 {
            let y = 550 - 25 * r;
            ops.push_str(&format!("50 {y} m 550 {y} l S "));
        }
        for r in 0..15 {
            for c in 0..20 {
                ops.push_str(&format!(
                    "BT /F1 6 Tf {} {} Td (t{r}_{c}) Tj ET ",
                    54 + 25 * c,
                    543 - 25 * r
                ));
            }
        }
        let tables = tables_of(&pdf_with_ops(&ops));
        assert_eq!(tables.len(), 1, "{tables:?}");
        assert_eq!(tables[0].rows.len(), 15);
        assert_eq!(tables[0].rows[0].len(), 20);
        assert_eq!(tables[0].rows[0][0], "t0_0");
        assert_eq!(tables[0].rows[14][19], "t14_19");
        assert!(tables[0].rows.iter().flatten().all(|c| !c.is_empty()));
    }

    #[test]
    #[ignore = "perf probe: dense ruling grid"]
    fn dense_grid_perf() {
        // 7 760 cells from 81×98 rulings (~7 900 intersection
        // points) — the regime where the old O(points²) find_cells
        // scan dominated.
        let (cols, rows) = (80usize, 97usize);
        let mut ops = String::new();
        for c in 0..=cols {
            let x = 50 + 6 * c as i32;
            ops.push_str(&format!("{x} 50 m {x} 780 l S "));
        }
        for r in 0..=rows {
            let y = 50 + 7 * r as i32;
            ops.push_str(&format!("50 {y} m {} {y} l S ", 50 + 6 * cols as i32));
        }
        ops.push_str("BT /F1 5 Tf 52 60 Td (x) Tj ET");
        let bytes = pdf_with_ops(&ops);
        let doc = Document::load_mem(&bytes).unwrap();
        let page_id = *doc.get_pages().values().next().unwrap();
        let start = std::time::Instant::now();
        let tables = extract_tables(&doc, page_id);
        eprintln!(
            "dense grid: {:?} → {} tables",
            start.elapsed(),
            tables.len()
        );
        assert!(!tables.is_empty());
    }

    #[test]
    fn cell_text_joins_chunks_with_cr() {
        // Two chunks at different tops inside one cell → '\r' between.
        let cell = Cell {
            rect: Rect {
                x: 0.0,
                y: 0.0,
                w: 10.0,
                h: 10.0,
            },
            chunks: vec![
                Chunk {
                    rect: Rect {
                        x: 0.0,
                        y: 8.0,
                        w: 5.0,
                        h: 2.0,
                    },
                    els: vec![TextEl {
                        text: "top".into(),
                        x: 0.0,
                        y: 8.0,
                        w: 5.0,
                        h: 2.0,
                        font_size: 10.0,
                        font: "F".into(),
                        space_w: 2.0,
                        dir: 0.0,
                    }],
                    ltr: 1,
                },
                Chunk {
                    rect: Rect {
                        x: 0.0,
                        y: 2.0,
                        w: 5.0,
                        h: 2.0,
                    },
                    els: vec![TextEl {
                        text: "bottom".into(),
                        x: 0.0,
                        y: 2.0,
                        w: 5.0,
                        h: 2.0,
                        font_size: 10.0,
                        font: "F".into(),
                        space_w: 2.0,
                        dir: 0.0,
                    }],
                    ltr: 1,
                },
            ],
        };
        assert_eq!(cell.text(), "bottom\rtop");
    }
}
