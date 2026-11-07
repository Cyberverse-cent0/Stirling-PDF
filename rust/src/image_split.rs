//! Native port of Stirling's `split_photos.py` (OpenCV): estimate the
//! background colour from five sample points, add a constant border,
//! threshold everything that differs from it, dilate, find the external
//! contours, filter by area, then crop, deskew and write each photo as
//! PNG. Replaces the Python/cv2 sidecar so the endpoint needs no
//! interpreter.
//!
//! Fidelity notes vs the script:
//! - `split_photos` calls `find_photo_boundaries(image, bg, tolerance)`
//!   without forwarding `min_area`/`min_contour_area`, so those CLI
//!   arguments are silently ignored and the defaults (10000/500) always
//!   apply — reproduced here.
//! - `contourArea` is approximated by tracing each component's outer
//!   boundary (Moore neighbourhood tracing) and taking the shoelace
//!   area, matching cv2's polygon-area semantics closely for solid blobs.
//! - Canny uses Sobel 3×3 without cv2's internal pre-smoothing; Hough is
//!   the standard accumulator at 1 px / 1° resolution, threshold 200.

use image::{imageops, DynamicImage, GrayImage, Rgb, RgbImage};

/// `split_photos.py` CLI defaults as bound by `ExtractImageScansRequest`.
#[derive(Clone, Copy)]
pub struct SplitParams {
    pub tolerance: i32,
    pub angle_threshold: i32,
    pub border_size: i32,
}

/// One detected photo crop, named like the script's `<input>_<n>.png`.
#[derive(Debug)]
pub struct SplitResult {
    /// Script-side output name; the HTTP layer renames entries itself, so
    /// only tests read this today.
    #[allow(dead_code)]
    pub name: String,
    pub png: Vec<u8>,
}

const MIN_AREA: i64 = 10000;
const MIN_CONTOUR_AREA: i64 = 500;
/// Hard pixel ceiling: below it the pipeline's per-pixel buffers stay in
/// the hundreds-of-MB range per image; above it a crafted upload could
/// exhaust memory. Checked in `split_photos` and again by the HTTP layer
/// before decode work starts.
pub const MAX_IMAGE_PIXELS: u64 = 100_000_000;
/// `borderSize` is user input; anything past this is meaningless on a
/// scan and only inflates the bordered copy.
const MAX_BORDER_SIZE: i32 = 4096;
/// Deskew runs Canny + Hough + a bicubic warp — ~20 bytes per pixel of
/// scratch. Past this the crop is returned unrotated rather than risking
/// memory exhaustion on huge photos.
const MAX_DESKEW_PIXELS: u64 = 48_000_000;
/// Moore tracing is capped so a pathological component cannot allocate
/// an unbounded boundary vector; past the budget the component's pixel
/// count stands in for contour area (equal for solid blobs, which is
/// what the 500 px² threshold cares about).
const MAX_TRACE_STEPS: usize = 1 << 22;

/// Runs the whole pipeline on a decoded RGB image.
pub fn split_photos(image: &RgbImage, input_base: &str, p: SplitParams) -> Vec<SplitResult> {
    let (w, h) = image.dimensions();
    if w == 0 || h == 0 || w as u64 * h as u64 > MAX_IMAGE_PIXELS {
        return Vec::new();
    }
    let border = p.border_size.clamp(0, MAX_BORDER_SIZE) as u32;
    let bg = estimate_background_color(image);
    let bordered = add_border(image, bg, border);
    let mut boundaries = find_photo_boundaries(&bordered, bg, p.tolerance);
    boundaries.sort_by_key(|b| (b.y, b.x));

    let mut out = Vec::new();
    for (idx, b) in boundaries.iter().enumerate() {
        let mut crop = imageops::crop_imm(&bordered, b.x, b.y, b.w, b.h).to_image();
        crop = auto_rotate(&crop, p.angle_threshold);
        if border > 0 && crop.height() > 2 * border && crop.width() > 2 * border {
            crop = imageops::crop_imm(
                &crop,
                border,
                border,
                crop.width() - 2 * border,
                crop.height() - 2 * border,
            )
            .to_image();
        }
        if crop.width() == 0 || crop.height() == 0 {
            continue;
        }
        let mut buf = std::io::Cursor::new(Vec::new());
        if DynamicImage::ImageRgb8(crop)
            .write_to(&mut buf, image::ImageFormat::Png)
            .is_err()
        {
            continue;
        }
        out.push(SplitResult {
            name: format!("{input_base}_{}.png", idx + 1),
            png: buf.into_inner(),
        });
    }
    out
}

#[derive(Clone, Copy)]
struct Boundary {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

/// Median of the five corner/centre samples, per channel — cv2's
/// `np.median(colors, axis=0)` on BGR; channel order is irrelevant because
/// every later use is symmetric per channel.
fn estimate_background_color(image: &RgbImage) -> Rgb<u8> {
    let (w, h) = image.dimensions();
    let corners = [(0u32, 0u32), (w - 1, 0), (w - 1, h - 1), (0, h - 1)];
    let centre = image.get_pixel(w / 2, h / 2);
    let mut px = [0u8; 3];
    for (c, dst) in px.iter_mut().enumerate() {
        let mut v: Vec<u8> = corners
            .iter()
            .map(|&(x, y)| image.get_pixel(x, y).0[c])
            .chain(std::iter::once(centre.0[c]))
            .collect();
        v.sort_unstable();
        *dst = v[2];
    }
    Rgb(px)
}

fn add_border(image: &RgbImage, color: Rgb<u8>, size: u32) -> RgbImage {
    let (w, h) = image.dimensions();
    let nw = w.saturating_add(2 * size);
    let nh = h.saturating_add(2 * size);
    let mut out = RgbImage::from_pixel(nw, nh, color);
    imageops::overlay(&mut out, image, size as i64, size as i64);
    out
}

/// `inRange` + `bitwise_not` + two 5×5 dilations + external contours,
/// filtered by bounding-box and contour area.
fn find_photo_boundaries(image: &RgbImage, bg: Rgb<u8>, tolerance: i32) -> Vec<Boundary> {
    let (w, h) = image.dimensions();
    let mut mask = vec![0u8; (w * h) as usize];
    let lo = |c: u8| (c as i32 - tolerance).clamp(0, 255) as u8;
    let hi = |c: u8| (c as i32 + tolerance).clamp(0, 255) as u8;
    for (i, p) in image.pixels().enumerate() {
        let inside = (lo(bg.0[0])..=hi(bg.0[0])).contains(&p.0[0])
            && (lo(bg.0[1])..=hi(bg.0[1])).contains(&p.0[1])
            && (lo(bg.0[2])..=hi(bg.0[2])).contains(&p.0[2]);
        mask[i] = if inside { 0 } else { 255 };
    }
    dilate(&mut mask, w, h, 5, 2);

    let mut labels = vec![0u32; (w * h) as usize];
    let mut next = 1u32;
    // Two-pass union-find over 8-connected foreground.
    let mut parent: Vec<u32> = vec![0];
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) as usize;
            if mask[i] == 0 {
                continue;
            }
            let mut roots = Vec::new();
            for (dx, dy) in [(-1i32, -1i32), (0, -1), (1, -1), (-1, 0)] {
                let nx = x as i32 + dx;
                let ny = y as i32 + dy;
                if nx < 0 || ny < 0 {
                    continue;
                }
                let ni = (ny as u32 * w + nx as u32) as usize;
                if mask[ni] != 0 && labels[ni] != 0 {
                    roots.push(find(&mut parent, labels[ni]));
                }
            }
            match roots.first() {
                None => {
                    parent.push(next);
                    labels[i] = next;
                    next += 1;
                }
                Some(&r) => {
                    labels[i] = r;
                    for &other in &roots[1..] {
                        union(&mut parent, r, other);
                    }
                }
            }
        }
    }

    // Flatten labels to roots and gather per-component stats.
    let mut stats: std::collections::HashMap<u32, (u32, u32, u32, u32, i64)> =
        std::collections::HashMap::new();
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) as usize;
            if mask[i] == 0 || labels[i] == 0 {
                continue;
            }
            let root = find(&mut parent, labels[i]);
            labels[i] = root;
            let e = stats.entry(root).or_insert((x, y, x, y, 0));
            e.0 = e.0.min(x);
            e.1 = e.1.min(y);
            e.2 = e.2.max(x);
            e.3 = e.3.max(y);
            e.4 += 1;
        }
    }

    let mut out = Vec::new();
    for (min_x, min_y, max_x, max_y, px) in stats.values() {
        let bw = max_x - min_x + 1;
        let bh = max_y - min_y + 1;
        if (bw as i64) * (bh as i64) < MIN_AREA {
            continue;
        }
        let contour_area =
            component_contour_area(&labels, w, h, (*min_x, *min_y, *max_x, *max_y), *px);
        if contour_area >= MIN_CONTOUR_AREA {
            out.push(Boundary {
                x: *min_x,
                y: *min_y,
                w: bw,
                h: bh,
            });
        }
    }
    out
}

fn find(parent: &mut [u32], mut x: u32) -> u32 {
    while parent[x as usize] != x {
        parent[x as usize] = parent[parent[x as usize] as usize];
        x = parent[x as usize];
    }
    x
}

fn union(parent: &mut [u32], a: u32, b: u32) {
    let (ra, rb) = (find(parent, a), find(parent, b));
    if ra != rb {
        parent[rb as usize] = ra;
    }
}

/// 5×5 square dilation via separable horizontal+vertical max passes.
fn dilate(mask: &mut [u8], w: u32, h: u32, size: u32, iterations: u32) {
    let r = (size / 2) as i32;
    let mut tmp = vec![0u8; mask.len()];
    for _ in 0..iterations {
        // Horizontal.
        for y in 0..h {
            for x in 0..w {
                let mut m = 0u8;
                for dx in -r..=r {
                    let nx = x as i32 + dx;
                    if nx >= 0 && nx < w as i32 {
                        m = m.max(mask[(y * w + nx as u32) as usize]);
                    }
                }
                tmp[(y * w + x) as usize] = m;
            }
        }
        // Vertical.
        for y in 0..h {
            for x in 0..w {
                let mut m = 0u8;
                for dy in -r..=r {
                    let ny = y as i32 + dy;
                    if ny >= 0 && ny < h as i32 {
                        m = m.max(tmp[(ny as u32 * w + x) as usize]);
                    }
                }
                mask[(y * w + x) as usize] = m;
            }
        }
    }
}

/// Outer-boundary polygon area of one labelled component via Moore
/// neighbour tracing + shoelace — stands in for `cv2.contourArea`.
fn component_contour_area(
    labels: &[u32],
    w: u32,
    h: u32,
    bbox: (u32, u32, u32, u32),
    px_count: i64,
) -> i64 {
    let (min_x, min_y, max_x, max_y) = bbox;
    // Find the component's root label inside its bbox.
    let mut root = 0u32;
    'outer: for y in min_y..=max_y {
        for x in min_x..=max_x {
            let l = labels[(y * w + x) as usize];
            if l != 0 {
                root = l;
                break 'outer;
            }
        }
    }
    if root == 0 {
        return 0;
    }

    // Moore tracing: start at the topmost pixel, walk clockwise.
    let mut boundary: Vec<(i64, i64)> = Vec::new();
    let (mut cx, mut cy) = (min_x as i64, min_y as i64);
    // Find first pixel of the component scanning the bbox.
    'scan: for y in min_y..=max_y {
        for x in min_x..=max_x {
            if labels[(y * w + x) as usize] == root {
                cx = x as i64;
                cy = y as i64;
                break 'scan;
            }
        }
    }
    // 8 neighbours clockwise from the left (west) direction.
    const DIRS: [(i64, i64); 8] = [
        (-1, 0),
        (-1, -1),
        (0, -1),
        (1, -1),
        (1, 0),
        (1, 1),
        (0, 1),
        (-1, 1),
    ];
    let mut dir = 6usize; // entered from the west (index of (0,1) backtrack)
    let start = (cx, cy);
    let mut steps = 0usize;
    let trace_limit = ((max_x - min_x + 2) * (max_y - min_y + 2)) as usize;
    let trace_limit = trace_limit.min(MAX_TRACE_STEPS);
    loop {
        boundary.push((cx, cy));
        // Search clockwise from backtrack+1 for the next component pixel.
        let mut found = false;
        for k in 1..=8 {
            let d = (dir + k) % 8;
            let nx = cx + DIRS[d].0;
            let ny = cy + DIRS[d].1;
            if nx >= 0 && ny >= 0 && nx < w as i64 && ny < h as i64 {
                let l = labels[(ny as u32 * w + nx as u32) as usize];
                if l == root {
                    // Backtrack direction is opposite of travel.
                    dir = (d + 4) % 8;
                    cx = nx;
                    cy = ny;
                    found = true;
                    break;
                }
            }
        }
        if !found || (cx == start.0 && cy == start.1 && steps > 0) {
            break;
        }
        steps += 1;
        if steps > trace_limit {
            // Pathological perimeter (e.g. a comb-shaped component):
            // pixel count approximates contour area closely enough for
            // the MIN_CONTOUR_AREA threshold and bounds the allocation.
            return px_count;
        }
    }

    // Shoelace over the traced boundary.
    let mut area2 = 0i64;
    for i in 0..boundary.len() {
        let (x1, y1) = boundary[i];
        let (x2, y2) = boundary[(i + 1) % boundary.len()];
        area2 += x1 * y2 - x2 * y1;
    }
    area2.abs() / 2
}

// ---------------------------------------------------------------------------
// auto_rotate: Canny(50,150) → HoughLines(1px,1°,threshold 200) → median
// angle → warpAffine(INTER_CUBIC, BORDER_REPLICATE)
// ---------------------------------------------------------------------------

fn auto_rotate(image: &RgbImage, angle_threshold: i32) -> RgbImage {
    if !within_deskew_budget(image.width(), image.height()) {
        return image.clone();
    }
    let gray = to_gray(image);
    let edges = canny(&gray, 50, 150);
    let Some(angle) = hough_median_angle(&edges) else {
        return image.clone();
    };
    if angle.abs() < angle_threshold as f64 {
        return image.clone();
    }
    rotate_replicate(image, angle)
}

fn within_deskew_budget(w: u32, h: u32) -> bool {
    w as u64 * h as u64 <= MAX_DESKEW_PIXELS
}

fn to_gray(image: &RgbImage) -> GrayImage {
    let mut out = GrayImage::new(image.width(), image.height());
    for (x, y, p) in image.enumerate_pixels() {
        // cv2 COLOR_BGR2GRAY integer weights.
        let v = (299 * p.0[0] as u32 + 587 * p.0[1] as u32 + 114 * p.0[2] as u32) / 1000;
        out.put_pixel(x, y, image::Luma([v as u8]));
    }
    out
}

/// Sobel 3×3 gradients → magnitude → non-max suppression → hysteresis
/// thresholding at (low, high).
fn canny(gray: &GrayImage, low: u8, high: u8) -> GrayImage {
    let (w, h) = gray.dimensions();
    if w < 3 || h < 3 {
        return GrayImage::new(w, h);
    }
    let at = |x: i32, y: i32| -> f32 { gray.get_pixel(x as u32, y as u32).0[0] as f32 };
    let mut mag = vec![0f32; (w * h) as usize];
    let mut dir = vec![0f32; (w * h) as usize];
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let gx = -at(x as i32 - 1, y as i32 - 1)
                - 2.0 * at(x as i32 - 1, y as i32)
                - at(x as i32 - 1, y as i32 + 1)
                + at(x as i32 + 1, y as i32 - 1)
                + 2.0 * at(x as i32 + 1, y as i32)
                + at(x as i32 + 1, y as i32 + 1);
            let gy = -at(x as i32 - 1, y as i32 - 1)
                - 2.0 * at(x as i32, y as i32 - 1)
                - at(x as i32 + 1, y as i32 - 1)
                + at(x as i32 - 1, y as i32 + 1)
                + 2.0 * at(x as i32, y as i32 + 1)
                + at(x as i32 + 1, y as i32 + 1);
            let i = (y * w + x) as usize;
            mag[i] = (gx * gx + gy * gy).sqrt();
            dir[i] = gy.atan2(gx);
        }
    }

    // Non-max suppression along the gradient direction.
    let mut nms = vec![0f32; mag.len()];
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let i = (y * w + x) as usize;
            let m = mag[i];
            if m == 0.0 {
                continue;
            }
            let a = dir[i];
            // Project onto the two neighbour axes.
            let (dx, dy) = (a.cos(), a.sin());
            let interp = |ox: f32, oy: f32| -> f32 {
                // Bilinear sample of mag at (x+ox, y+oy).
                let fx = x as f32 + ox;
                let fy = y as f32 + oy;
                let x0 = fx.floor().max(0.0) as u32;
                let y0 = fy.floor().max(0.0) as u32;
                let x1 = (x0 + 1).min(w - 1);
                let y1 = (y0 + 1).min(h - 1);
                let tx = (fx - x0 as f32).clamp(0.0, 1.0);
                let ty = (fy - y0 as f32).clamp(0.0, 1.0);
                let m00 = mag[(y0 * w + x0) as usize];
                let m10 = mag[(y0 * w + x1) as usize];
                let m01 = mag[(y1 * w + x0) as usize];
                let m11 = mag[(y1 * w + x1) as usize];
                (m00 * (1.0 - tx) + m10 * tx) * (1.0 - ty) + (m01 * (1.0 - tx) + m11 * tx) * ty
            };
            let n1 = interp(dx, dy);
            let n2 = interp(-dx, -dy);
            if m >= n1 && m >= n2 {
                nms[i] = m;
            }
        }
    }

    // Hysteresis: strong seeds grow to weak neighbours.
    let mut out = GrayImage::new(w, h);
    let mut stack = Vec::new();
    for (i, &v) in nms.iter().enumerate() {
        if v >= high as f32 {
            stack.push(i);
        }
    }
    let mut visited = vec![false; nms.len()];
    while let Some(i) = stack.pop() {
        if visited[i] {
            continue;
        }
        visited[i] = true;
        if nms[i] < low as f32 {
            continue;
        }
        out.put_pixel(i as u32 % w, i as u32 / w, image::Luma([255]));
        let x = (i as u32 % w) as i32;
        let y = (i as u32 / w) as i32;
        for dy in -1..=1 {
            for dx in -1..=1 {
                let nx = x + dx;
                let ny = y + dy;
                if nx < 0 || ny < 0 || nx >= w as i32 || ny >= h as i32 {
                    continue;
                }
                let ni = (ny as u32 * w + nx as u32) as usize;
                if !visited[ni] && nms[ni] >= low as f32 {
                    stack.push(ni);
                }
            }
        }
    }
    out
}

/// Standard Hough transform at 1 px / 1° resolution, threshold 200;
/// returns the median of `theta·180/π − 90` over detected lines.
fn hough_median_angle(edges: &GrayImage) -> Option<f64> {
    let (w, h) = edges.dimensions();
    // u64 math: w*w overflows u32 for images wider than 65535 px.
    let diag = ((w as u64 * w as u64 + h as u64 * h as u64) as f64).sqrt();
    let rho_max = diag.ceil() as i32;
    let n_rho = (2 * rho_max + 1) as usize;
    let mut acc = vec![0u32; n_rho * 180];
    let mut pixels = Vec::new();
    for (x, y, p) in edges.enumerate_pixels() {
        if p.0[0] != 0 {
            pixels.push((x as f64, y as f64));
        }
    }
    if pixels.is_empty() {
        return None;
    }
    for &(x, y) in &pixels {
        for t in 0..180 {
            let theta = t as f64 * std::f64::consts::PI / 180.0;
            let rho = x * theta.cos() + y * theta.sin();
            let r = (rho.round() as i32 + rho_max) as usize;
            acc[r * 180 + t] += 1;
        }
    }
    let mut angles: Vec<f64> = Vec::new();
    for r in 0..n_rho {
        for t in 0..180 {
            if acc[r * 180 + t] >= 200 {
                angles.push(t as f64 - 90.0);
            }
        }
    }
    if angles.is_empty() {
        return None;
    }
    angles.sort_by(f64::total_cmp);
    let n = angles.len();
    Some(if n % 2 == 1 {
        angles[n / 2]
    } else {
        (angles[n / 2 - 1] + angles[n / 2]) / 2.0
    })
}

/// `getRotationMatrix2D` + `warpAffine(INTER_CUBIC, BORDER_REPLICATE)`
/// keeping the canvas size — positive angle rotates counter-clockwise in
/// PDF-style y-up math, which in image space is the script's behaviour.
fn rotate_replicate(image: &RgbImage, angle_deg: f64) -> RgbImage {
    let (w, h) = image.dimensions();
    let (w_f, h_f) = (w as f64, h as f64);
    let cx = w_f / 2.0;
    let cy = h_f / 2.0;
    let theta = angle_deg.to_radians();
    let (sn, cs) = theta.sin_cos();
    // cv2 getRotationMatrix2D: [α β (1-α)cx - βcy; -β α βcx + (1-α)cy].
    // warpAffine maps dst→src with this matrix directly.
    let (a, b) = (cs, sn);
    let m11 = a;
    let m12 = b;
    let m13 = (1.0 - a) * cx - b * cy;
    let m21 = -b;
    let m22 = a;
    let m23 = b * cx + (1.0 - a) * cy;

    let mut out = RgbImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let sx = m11 * x as f64 + m12 * y as f64 + m13;
            let sy = m21 * x as f64 + m22 * y as f64 + m23;
            out.put_pixel(x, y, sample_bicubic_replicate(image, sx, sy));
        }
    }
    out
}

/// Keys-cubic (a = −0.5, INTER_CUBIC) sample with BORDER_REPLICATE.
fn sample_bicubic_replicate(image: &RgbImage, x: f64, y: f64) -> Rgb<u8> {
    let (w, h) = image.dimensions();
    let clamp = |v: f64, max: u32| -> f64 {
        if v < 0.0 {
            0.0
        } else if v > (max - 1) as f64 {
            (max - 1) as f64
        } else {
            v
        }
    };
    let cx = clamp(x, w);
    let cy = clamp(y, h);
    let x0 = cx.floor() as i64;
    let y0 = cy.floor() as i64;
    let fx = cx - x0 as f64;
    let fy = cy - y0 as f64;
    let mut acc = [0f64; 3];
    for (c, dst) in acc.iter_mut().enumerate() {
        let mut sum = 0.0;
        for j in -1..=2 {
            for i in -1..=2 {
                let sx = (x0 + i).clamp(0, w as i64 - 1) as u32;
                let sy = (y0 + j).clamp(0, h as i64 - 1) as u32;
                sum += cubic_weight(fx - i as f64)
                    * cubic_weight(fy - j as f64)
                    * image.get_pixel(sx, sy).0[c] as f64;
            }
        }
        *dst = sum;
    }
    Rgb([
        acc[0].round().clamp(0.0, 255.0) as u8,
        acc[1].round().clamp(0.0, 255.0) as u8,
        acc[2].round().clamp(0.0, 255.0) as u8,
    ])
}

fn cubic_weight(t: f64) -> f64 {
    let t = t.abs();
    if t < 1.0 {
        1.5 * t * t * t - 2.5 * t * t + 1.0
    } else if t < 2.0 {
        -0.5 * t * t * t + 2.5 * t * t - 4.0 * t + 2.0
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> SplitParams {
        SplitParams {
            tolerance: 30,
            angle_threshold: 10,
            border_size: 1,
        }
    }

    /// A 0×0 image must not underflow the corner sampling.
    #[test]
    fn empty_image_returns_no_crops() {
        let img = RgbImage::new(0, 0);
        assert!(split_photos(&img, "x", params()).is_empty());
    }

    /// `borderSize` is unclamped user input — i32::MAX must not overflow
    /// the bordered-copy dimensions or allocate unboundedly.
    #[test]
    fn absurd_border_size_is_clamped() {
        let mut img = RgbImage::from_pixel(200, 200, Rgb([255, 255, 255]));
        for x in 40..160 {
            for y in 40..160 {
                img.put_pixel(x, y, Rgb([20, 20, 20]));
            }
        }
        let p = SplitParams {
            border_size: i32::MAX,
            ..params()
        };
        let results = split_photos(&img, "x", p);
        // The single photo is still found; the border is capped, not fatal.
        assert_eq!(results.len(), 1);
    }

    /// w*w overflowed u32 for images wider than 65535 px — the Hough
    /// diagonal must be computed in u64.
    #[test]
    fn hough_handles_images_wider_than_u16_range() {
        let mut edges = GrayImage::new(70_000, 8);
        for x in (0..70_000).step_by(97) {
            edges.put_pixel(x, 4, image::Luma([255]));
        }
        // Must not panic; a sparse edge set may or may not cross threshold.
        let _ = hough_median_angle(&edges);
    }

    /// The deskew budget boundary is inclusive and overflow-safe.
    #[test]
    fn deskew_budget_boundary() {
        assert!(within_deskew_budget(8000, 6000)); // 48 MP exactly
        assert!(!within_deskew_budget(8000, 6001));
        // u64 math: the product must not wrap to a small in-budget value.
        assert!(!within_deskew_budget(u32::MAX, u32::MAX));
    }
}
