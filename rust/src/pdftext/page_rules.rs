//! Ruling lines of one page, port of `PageRules.java`: a long thin path
//! is a rule, and a box large in both axes contributes its edges.
//! jpdfium reports each painted path as a `PageObject` with bounds; here
//! the same bounds come from walking the content stream's path operators
//! under the CTM, descending into form XObjects the way `text.rs` does
//! for text.

use lopdf::content::{Content, Operation};
use lopdf::{Document, Object, ObjectId};

use crate::pdfdoc;
use crate::text::{
    apply, mat_mul, matrix_operand, num, Matrix, Resources, IDENTITY, MAX_XOBJECT_DEPTH,
};

/// A path this thin in its short axis is a rule rather than a filled area.
const RULE_THICKNESS: f32 = 3.0;

/// A rule shorter than this is decoration (tick marks, bullets, glyph art).
const MIN_RULE_LENGTH: f32 = 8.0;

/// Boxes larger than this in either axis are page furniture, not table structure.
const MAX_BOX: f32 = 1500.0;

/// Path count past which the page is skipped: a page drawing this many is a
/// chart or an operator flood, never a readable grid. Approximates the Java
/// `PageContentSummary.totalObjects` guard with the decoded operation count.
const MAX_PAGE_OBJECTS: usize = 20_000;

/// A straight rule: `pos` is its y (horizontal) or x (vertical), spanning lo..hi.
#[derive(Debug, Clone, Copy)]
pub struct Rule {
    pub pos: f32,
    pub lo: f32,
    pub hi: f32,
}

/// The ruling lines of one page, horizontals sorted top-down and
/// verticals left-to-right.
#[derive(Debug, Clone, Default)]
pub struct PageRules {
    pub horizontal: Vec<Rule>,
    pub vertical: Vec<Rule>,
}

impl PageRules {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.horizontal.is_empty() && self.vertical.is_empty()
    }

    /// Rules given directly rather than read from a page, sorted as
    /// [`PageRules::of_page`] leaves them.
    pub fn of(horizontal: Vec<Rule>, vertical: Vec<Rule>) -> Self {
        let mut h = horizontal;
        let mut v = vertical;
        h.sort_by(|a, b| b.pos.total_cmp(&a.pos));
        v.sort_by(|a, b| a.pos.total_cmp(&b.pos));
        Self {
            horizontal: h,
            vertical: v,
        }
    }

    /// Reads the ruling lines of a page's content stream. Any decode
    /// failure yields [`PageRules::empty`]: path enumeration is an
    /// optimisation, never a correctness requirement.
    pub fn of_page(doc: &Document, page_id: ObjectId) -> Self {
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
        let resources = Resources::new(doc, dicts);

        let mut scan = PathScan::new(doc);
        if let Ok(content) = doc.get_and_decode_page_content(page_id) {
            scan.walk(&resources, &content.operations, 0);
        }
        if scan.overflow || scan.boxes.len() < 2 {
            return Self::empty();
        }

        let (page_w, page_h) = page_size(doc, page_id);
        let mut h = Vec::new();
        let mut v = Vec::new();
        for b in scan.boxes {
            let w = b.w;
            let ht = b.h;
            if !w.is_finite() || !ht.is_finite() || w < 0.0 || ht < 0.0 {
                continue;
            }
            if w > MAX_BOX || ht > MAX_BOX {
                continue;
            }
            // Paths that run off the page are chart clipping or decoration,
            // never table structure.
            if page_w > 0.0
                && (b.x < -1.0 || b.y < -1.0 || b.x + w > page_w + 1.0 || b.y + ht > page_h + 1.0)
            {
                continue;
            }
            if ht <= RULE_THICKNESS && w >= MIN_RULE_LENGTH {
                h.push(Rule {
                    pos: b.y + ht / 2.0,
                    lo: b.x,
                    hi: b.x + w,
                });
            } else if w <= RULE_THICKNESS && ht >= MIN_RULE_LENGTH {
                v.push(Rule {
                    pos: b.x + w / 2.0,
                    lo: b.y,
                    hi: b.y + ht,
                });
            } else if w >= MIN_RULE_LENGTH && ht >= MIN_RULE_LENGTH {
                // A box: a table border, a cell outline or a shaded row fill.
                // Its edges bound cells exactly as drawn rules do, and many
                // generators draw grids as per-cell rectangles.
                h.push(Rule {
                    pos: b.y,
                    lo: b.x,
                    hi: b.x + w,
                });
                h.push(Rule {
                    pos: b.y + ht,
                    lo: b.x,
                    hi: b.x + w,
                });
                v.push(Rule {
                    pos: b.x,
                    lo: b.y,
                    hi: b.y + ht,
                });
                v.push(Rule {
                    pos: b.x + w,
                    lo: b.y,
                    hi: b.y + ht,
                });
            }
        }
        h.sort_by(|a, b| b.pos.total_cmp(&a.pos));
        v.sort_by(|a, b| a.pos.total_cmp(&b.pos));
        Self {
            horizontal: h,
            vertical: v,
        }
    }
}

/// Crop-box size in points; (0, 0) disables the on-page check, like the
/// Java fallback when the page size cannot be read.
fn page_size(doc: &Document, page_id: ObjectId) -> (f32, f32) {
    let Ok(boxes) = pdfdoc::page_boxes(doc, page_id) else {
        return (0.0, 0.0);
    };
    let c = boxes.crop;
    ((c[2] - c[0]).abs(), (c[3] - c[1]).abs())
}

/// Which operands a cubic path operator supplies; the missing control
/// point is the current point (`v`) or the end point (`y`).
#[derive(Clone, Copy)]
enum Cubic {
    Both,
    FirstCurrent,
    SecondIsEnd,
}

/// Axis-aligned bounds of one painted path, in default user space.
struct PathBox {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

/// Content-stream walker collecting painted-path bounds. Path
/// construction operators accumulate geometry in device space; painting
/// and clipping operators emit the accumulated bounds once per path.
struct PathScan<'a> {
    doc: &'a Document,
    ctm: Matrix,
    ctm_stack: Vec<Matrix>,
    // Current point and subpath start, already transformed to device space.
    cx: f64,
    cy: f64,
    sx: f64,
    sy: f64,
    have_current: bool,
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
    has_points: bool,
    emitted: bool,
    boxes: Vec<PathBox>,
    ops: usize,
    overflow: bool,
}

impl<'a> PathScan<'a> {
    fn new(doc: &'a Document) -> Self {
        Self {
            doc,
            ctm: IDENTITY,
            ctm_stack: Vec::new(),
            cx: 0.0,
            cy: 0.0,
            sx: 0.0,
            sy: 0.0,
            have_current: false,
            min_x: 0.0,
            min_y: 0.0,
            max_x: 0.0,
            max_y: 0.0,
            has_points: false,
            emitted: false,
            boxes: Vec::new(),
            ops: 0,
            overflow: false,
        }
    }

    fn walk(&mut self, resources: &Resources<'a>, operations: &[Operation], depth: usize) {
        for operation in operations {
            self.ops += 1;
            if self.ops > MAX_PAGE_OBJECTS {
                self.overflow = true;
                return;
            }
            let operands = &operation.operands;
            match operation.operator.as_str() {
                "q" => self.ctm_stack.push(self.ctm),
                "Q" => {
                    if let Some(ctm) = self.ctm_stack.pop() {
                        self.ctm = ctm;
                    }
                }
                "cm" => {
                    if let Some(m) = matrix_operand(operands) {
                        self.ctm = mat_mul(self.ctm, m);
                    }
                }
                "m" => self.path_move(operands),
                "l" => self.path_line(operands),
                "c" => self.path_cubic(operands, Cubic::Both),
                "v" => self.path_cubic(operands, Cubic::FirstCurrent),
                "y" => self.path_cubic(operands, Cubic::SecondIsEnd),
                "h" => {
                    if self.have_current {
                        // Subpath start is already device space.
                        self.extend(self.sx, self.sy);
                    }
                }
                "re" => self.path_rect(operands),
                "S" | "s" | "f" | "F" | "f*" | "B" | "B*" | "b" | "b*" | "n" => {
                    self.paint_path(true)
                }
                "W" | "W*" => self.paint_path(false),
                "Do" => self.show_xobject(resources, operands, depth),
                _ => {}
            }
        }
    }

    fn show_xobject(&mut self, resources: &Resources<'a>, operands: &[Object], depth: usize) {
        if depth >= MAX_XOBJECT_DEPTH {
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
        // Do executes the form under the concatenation of its /Matrix with
        // the CTM in effect, inside a saved graphics state.
        let saved_ctm = self.ctm;
        if let Some(m) = dict
            .get(b"Matrix")
            .ok()
            .and_then(|v| v.as_array().ok())
            .and_then(|a| matrix_operand(a))
        {
            self.ctm = mat_mul(self.ctm, m);
        }
        let form_resources = resources.of_form(dict);
        let content = self
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
    }

    fn move_to(&mut self, x: f64, y: f64) {
        let (px, py) = apply(self.ctm, x, y);
        self.cx = px;
        self.cy = py;
        self.sx = px;
        self.sy = py;
        self.have_current = true;
    }

    fn line_to(&mut self, x: f64, y: f64) {
        if !self.have_current {
            self.move_to(x, y);
            return;
        }
        let (px, py) = apply(self.ctm, x, y);
        self.extend(px, py);
    }

    fn extend(&mut self, x: f64, y: f64) {
        self.accumulate(x, y, x, y);
        self.cx = x;
        self.cy = y;
    }

    fn accumulate(&mut self, x0: f64, y0: f64, x1: f64, y1: f64) {
        if !self.has_points {
            self.min_x = x0.min(x1);
            self.min_y = y0.min(y1);
            self.max_x = x0.max(x1);
            self.max_y = y0.max(y1);
            self.has_points = true;
        } else {
            self.min_x = self.min_x.min(x0).min(x1);
            self.min_y = self.min_y.min(y0).min(y1);
            self.max_x = self.max_x.max(x0).max(x1);
            self.max_y = self.max_y.max(y0).max(y1);
        }
        self.emitted = false;
    }

    fn path_move(&mut self, operands: &[Object]) {
        let mut i = 0;
        let mut first = true;
        while i + 1 < operands.len() {
            let (Some(x), Some(y)) = (num(&operands[i]), num(&operands[i + 1])) else {
                break;
            };
            if first {
                self.move_to(x, y);
                first = false;
            } else {
                self.line_to(x, y);
            }
            i += 2;
        }
    }

    fn path_line(&mut self, operands: &[Object]) {
        let mut i = 0;
        while i + 1 < operands.len() {
            let (Some(x), Some(y)) = (num(&operands[i]), num(&operands[i + 1])) else {
                break;
            };
            self.line_to(x, y);
            i += 2;
        }
    }

    /// `c` takes two control points then the end; `v` reuses the current
    /// point as the first control; `y` reuses the end as the second.
    fn path_cubic(&mut self, operands: &[Object], kind: Cubic) {
        if !self.have_current {
            return;
        }
        let needed = match kind {
            Cubic::Both => 6,
            _ => 4,
        };
        if operands.len() < needed {
            return;
        }
        let pair =
            |i: usize| -> Option<(f64, f64)> { Some((num(&operands[i])?, num(&operands[i + 1])?)) };
        // Affine transforms commute with Bézier evaluation, so transforming
        // the control points gives the exact device-space curve; the
        // current point is already device space.
        let (q1, q2, q3) = match kind {
            Cubic::Both => match (pair(0), pair(2), pair(4)) {
                (Some(a), Some(b), Some(c)) => (
                    apply(self.ctm, a.0, a.1),
                    apply(self.ctm, b.0, b.1),
                    apply(self.ctm, c.0, c.1),
                ),
                _ => return,
            },
            Cubic::FirstCurrent => match (pair(0), pair(2)) {
                (Some(b), Some(c)) => (
                    (self.cx, self.cy),
                    apply(self.ctm, b.0, b.1),
                    apply(self.ctm, c.0, c.1),
                ),
                _ => return,
            },
            Cubic::SecondIsEnd => match (pair(0), pair(2)) {
                (Some(a), Some(c)) => {
                    let q = apply(self.ctm, c.0, c.1);
                    (apply(self.ctm, a.0, a.1), q, q)
                }
                _ => return,
            },
        };
        self.curve_to(q1, q2, q3);
    }

    fn curve_to(&mut self, p1: (f64, f64), p2: (f64, f64), p3: (f64, f64)) {
        if !self.have_current {
            return;
        }
        let p0 = (self.cx, self.cy);
        let (x0, x1) = cubic_axis(p0.0, p1.0, p2.0, p3.0);
        let (y0, y1) = cubic_axis(p0.1, p1.1, p2.1, p3.1);
        self.accumulate(x0, y0, x1, y1);
        self.cx = p3.0;
        self.cy = p3.1;
        self.has_points = true;
    }

    /// `re` is equivalent to `x y m x+w y l x+w y+h l x y+h l h`.
    fn path_rect(&mut self, operands: &[Object]) {
        if operands.len() < 4 {
            return;
        }
        let (Some(x), Some(y), Some(w), Some(h)) = (
            num(&operands[0]),
            num(&operands[1]),
            num(&operands[2]),
            num(&operands[3]),
        ) else {
            return;
        };
        self.move_to(x, y);
        self.line_to(x + w, y);
        self.line_to(x + w, y + h);
        self.line_to(x, y + h);
        self.line_to(x, y);
    }

    /// Emits the current path's bounds once; `consume` also ends the path,
    /// as every painting operator does. `W`/`W*` clip without consuming.
    fn paint_path(&mut self, consume: bool) {
        if self.has_points && !self.emitted {
            self.boxes.push(PathBox {
                x: self.min_x as f32,
                y: self.min_y as f32,
                w: (self.max_x - self.min_x) as f32,
                h: (self.max_y - self.min_y) as f32,
            });
            self.emitted = true;
        }
        if consume {
            self.have_current = false;
            self.has_points = false;
            self.emitted = false;
        }
    }
}

/// Min/max of one axis of the cubic Bézier through p0..p3, found from the
/// roots of its derivative plus the endpoints.
fn cubic_axis(p0: f64, p1: f64, p2: f64, p3: f64) -> (f64, f64) {
    let mut lo = p0.min(p3);
    let mut hi = p0.max(p3);
    // B'(t) = a t² + b t + c with these coefficients.
    let a = -3.0 * p0 + 9.0 * p1 - 9.0 * p2 + 3.0 * p3;
    let b = 6.0 * p0 - 12.0 * p1 + 6.0 * p2;
    let c = -3.0 * p0 + 3.0 * p1;
    for t in quad_roots(a, b, c) {
        if !(t > 0.0 && t < 1.0) {
            continue;
        }
        let mt = 1.0 - t;
        let v = mt * mt * mt * p0 + 3.0 * mt * mt * t * p1 + 3.0 * mt * t * t * p2 + t * t * t * p3;
        lo = lo.min(v);
        hi = hi.max(v);
    }
    (lo, hi)
}

fn quad_roots(a: f64, b: f64, c: f64) -> Vec<f64> {
    const EPS: f64 = 1e-12;
    if a.abs() < EPS {
        if b.abs() < EPS {
            return Vec::new();
        }
        return vec![-c / b];
    }
    let disc = b * b - 4.0 * a * c;
    if disc < 0.0 {
        return Vec::new();
    }
    let s = disc.sqrt();
    vec![(-b + s) / (2.0 * a), (-b - s) / (2.0 * a)]
}
