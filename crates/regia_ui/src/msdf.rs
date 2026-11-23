// SPDX-License-Identifier: MPL-2.0
//! Multi-channel signed distance fields (MSDF) for crisp text at any scale.
//!
//! Implements the published method of Viktor Chlumský's master's thesis *Shape Decomposition
//! for Multi-channel Distance Fields* (Czech Technical University in Prague, 2015; see
//! `docs/provenance.md` and `docs/msdf-glyphs.md`). A glyph outline is decomposed by
//! colouring its edges cyan/magenta/yellow so the two edges meeting at a sharp corner land in
//! different channels; each of the three channels then stores an independent signed distance
//! field over its own subset of edges, and the renderer reconstructs coverage as the median of
//! the three ([`median3`]). Corners that a single-channel field would round off stay sharp
//! because two of the three channels measure the two edges separately.
//!
//! The pieces, per the thesis: exact distance to line, quadratic and cubic Bézier segments by
//! robust polynomial root solving (recursive derivative bracketing plus bisection, § 2.2);
//! per channel, the nearest edge of that channel's colours (ties at a shared corner vertex go
//! to the edge the point is more perpendicular to) and its *signed* pseudo-distance, which
//! extends the edge beyond its endpoint so the isopleths mitre at corners (§ 2.3) — the
//! channels may disagree in sign near a corner, which is what keeps it sharp; the edge side
//! turned into inside/outside by the outline's orientation and checked against the winding
//! number at every texel; edge colouring by corner sharpness (ch. 3); and an
//! error-correction pass that finds channel clashes between neighbouring texels (a
//! reconstruction on the wrong side of the true outline) and falls back to the median there
//! (ch. 4).
//!
//! Outlines are plain contours of Bézier segments ([`Outline`]); [`parse_glyf_outline`]
//! recovers them — including composite glyphs — from the raw bytes of a TrueType font, so no
//! font-parser dependency is needed. [`generate`] rasterises an outline into an
//! [`MsdfBitmap`]: RGB `f32` texels where `0.5` sits exactly on the edge and the field spans
//! `range` texels on either side (`v = 0.5 + 0.5 · d / range`); [`MsdfBitmap::to_u8`]
//! quantises to bytes. [`MsdfBitmap::coverage`] is the CPU reference sampler used by the
//! tests and by callers who need coverage on the CPU: bilinear per channel, then median, then
//! a smoothstep — the same maths a UI shader runs.
//!
//! Everything is plain safe Rust with no dependencies: pure, deterministic (no maps, no
//! clocks, no seeds — byte-identical output for identical input on the same build), and total:
//! empty, degenerate and zero-length geometry yields a defined field, and NaN/∞ input returns
//! [`MsdfError::NotFinite`] rather than panicking.
//!
//! ```
//! use regia_ui::msdf::{Contour, Outline, Point2, Segment, generate};
//!
//! // A 16×16-unit square, y up, as it would come out of a font.
//! let pts = [(0.0, 0.0), (16.0, 0.0), (16.0, 16.0), (0.0, 16.0)];
//! let mut contour = Contour::default();
//! for w in pts.windows(2) {
//!     contour.segments.push(Segment::Line {
//!         from: Point2::new(w[0].0, w[0].1),
//!         to: Point2::new(w[1].0, w[1].1),
//!     });
//! }
//! contour.segments.push(Segment::Line {
//!     from: Point2::new(pts[3].0, pts[3].1),
//!     to: Point2::new(pts[0].0, pts[0].1),
//! });
//! let mut outline = Outline::default();
//! outline.contours.push(contour);
//!
//! // 32×32 texels, the field spreads over 4 texels, one texel per two font units.
//! if let Ok(bitmap) = generate(&outline, [32, 32], 4.0, [2.0, 2.0], [0.0, 0.0]) {
//!     assert_eq!((bitmap.width(), bitmap.height()), (32, 32));
//!     // The middle of the square is inside, so its median is above one half.
//!     assert!(bitmap.coverage(16.0, 16.0) > 0.5);
//! }
//! ```

/// Default threshold for the edge-colouring pass: a vertex whose direction turns by more than
/// this many degrees (deviation from a straight continuation) is a corner, and the two edges
/// meeting there get different colours (Chlumský 2015, ch. 3).
pub const DEFAULT_TURN_THRESHOLD_DEG: f64 = 3.0;

/// Maximum number of texels ([`generate`] rejects larger bitmaps so a bad size cannot
/// allocate without bound).
pub const MAX_TEXELS: u32 = 1 << 24;

/// The distance value stored for a channel that has no edges of its colour (e.g. the blue
/// channel of a shape whose every edge is yellow). It is far larger than any real distance so
/// the median never selects it, and it saturates to 0 or 255 in [`MsdfBitmap::to_u8`].
pub const DEAD_CHANNEL_DISTANCE: f64 = 1.0e6;

/// Maximum nesting of composite-glyph components ([`parse_glyf_outline`]).
const MAX_COMPONENT_DEPTH: u8 = 8;

/// Most segments one glyph may expand to, intermediate component copies included
/// ([`parse_glyf_outline`]): composites that reuse a component many times at every nesting
/// level would otherwise grow exponentially.
pub const MAX_GLYPH_SEGMENTS: usize = 1 << 20;

/// The deterministic colour rotation contours start on, so nested contours (an 'O' and its
/// hole) begin on different colours.
const COLOR_CYCLE: [EdgeColor; 3] = [EdgeColor::Yellow, EdgeColor::Cyan, EdgeColor::Magenta];

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

/// A 2-D point in font units (y up), the coordinate space of [`Outline`].
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Point2 {
    /// X coordinate.
    pub x: f64,
    /// Y coordinate.
    pub y: f64,
}

impl Point2 {
    /// Creates a point.
    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
    fn plus(self, o: Self) -> Self {
        Self::new(self.x + o.x, self.y + o.y)
    }
    fn minus(self, o: Self) -> Self {
        Self::new(self.x - o.x, self.y - o.y)
    }
    fn scaled(self, s: f64) -> Self {
        Self::new(self.x * s, self.y * s)
    }
    fn dot(self, o: Self) -> f64 {
        self.x * o.x + self.y * o.y
    }
    /// The z of the 3-D cross product: positive when `o` is to the left of `self`.
    fn cross(self, o: Self) -> f64 {
        self.x * o.y - self.y * o.x
    }
    fn len_sq(self) -> f64 {
        self.dot(self)
    }
}

/// Returns the unit vector of `v`, or `None` for a zero (or non-finite-length) vector.
fn unit(v: Point2) -> Option<Point2> {
    let l = v.len_sq().sqrt();
    if l > 0.0 && l.is_finite() {
        Some(v.scaled(1.0 / l))
    } else {
        None
    }
}

/// One edge of a contour. Each segment carries its own endpoints, so contours are chains of
/// segments where `segments[i].to` should coincide with `segments[i + 1].from`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Segment {
    /// A straight line.
    Line {
        /// Start point.
        from: Point2,
        /// End point.
        to: Point2,
    },
    /// A quadratic Bézier.
    Quadratic {
        /// Start point.
        from: Point2,
        /// Control point.
        control: Point2,
        /// End point.
        to: Point2,
    },
    /// A cubic Bézier.
    Cubic {
        /// Start point.
        from: Point2,
        /// First control point.
        control1: Point2,
        /// Second control point.
        control2: Point2,
        /// End point.
        to: Point2,
    },
}

impl Segment {
    /// Evaluates the curve at parameter `t` in `[0, 1]`.
    pub fn point_at(&self, t: f64) -> Point2 {
        match *self {
            Segment::Line { from, to } => from.plus(to.minus(from).scaled(t)),
            Segment::Quadratic { from, control, to } => {
                let u = 1.0 - t;
                let a = from.scaled(u * u);
                let b = control.scaled(2.0 * u * t);
                let c = to.scaled(t * t);
                a.plus(b).plus(c)
            }
            Segment::Cubic {
                from,
                control1,
                control2,
                to,
            } => {
                let u = 1.0 - t;
                let a = from.scaled(u * u * u);
                let b = control1.scaled(3.0 * u * u * t);
                let c = control2.scaled(3.0 * u * t * t);
                let d = to.scaled(t * t * t);
                a.plus(b).plus(c).plus(d)
            }
        }
    }

    /// The (unnormalised) tangent of the curve at parameter `t` in `[0, 1]`.
    pub fn tangent_at(&self, t: f64) -> Point2 {
        match *self {
            Segment::Line { from, to } => to.minus(from),
            Segment::Quadratic { from, control, to } => {
                let u = 1.0 - t;
                control
                    .minus(from)
                    .scaled(2.0 * u)
                    .plus(to.minus(control).scaled(2.0 * t))
            }
            Segment::Cubic {
                from,
                control1,
                control2,
                to,
            } => {
                let u = 1.0 - t;
                control1
                    .minus(from)
                    .scaled(3.0 * u * u)
                    .plus(control2.minus(control1).scaled(6.0 * u * t))
                    .plus(to.minus(control2).scaled(3.0 * t * t))
            }
        }
    }

    /// The two endpoints of the segment.
    pub fn endpoints(&self) -> (Point2, Point2) {
        match *self {
            Segment::Line { from, to } => (from, to),
            Segment::Quadratic { from, to, .. } => (from, to),
            Segment::Cubic { from, to, .. } => (from, to),
        }
    }

    /// True when the segment collapses to a single point and carries no geometry. Such
    /// segments are skipped by [`generate`]; they never make it panic.
    pub fn is_degenerate(&self) -> bool {
        match *self {
            Segment::Line { from, to } => from == to,
            Segment::Quadratic { from, control, to } => from == control && control == to,
            Segment::Cubic {
                from,
                control1,
                control2,
                to,
            } => from == control1 && control1 == control2 && control2 == to,
        }
    }

    /// The bounding box `(min_x, min_y, max_x, max_y)` of the segment. Bézier curves lie in
    /// the convex hull of their control points, so this box contains the curve and gives a
    /// valid lower bound of the distance to it.
    fn aabb(&self) -> (f64, f64, f64, f64) {
        let points: [Point2; 4] = match *self {
            Segment::Line { from, to } => [from, to, from, to],
            Segment::Quadratic { from, control, to } => [from, control, to, to],
            Segment::Cubic {
                from,
                control1,
                control2,
                to,
            } => [from, control1, control2, to],
        };
        let mut min_x = points[0].x;
        let mut max_x = points[0].x;
        let mut min_y = points[0].y;
        let mut max_y = points[0].y;
        for p in &points[1..] {
            min_x = min_x.min(p.x);
            max_x = max_x.max(p.x);
            min_y = min_y.min(p.y);
            max_y = max_y.max(p.y);
        }
        (min_x, min_y, max_x, max_y)
    }
}

/// Applies `f` to every point of the segment.
fn map_segment(seg: &Segment, f: impl Fn(Point2) -> Point2) -> Segment {
    match *seg {
        Segment::Line { from, to } => Segment::Line {
            from: f(from),
            to: f(to),
        },
        Segment::Quadratic { from, control, to } => Segment::Quadratic {
            from: f(from),
            control: f(control),
            to: f(to),
        },
        Segment::Cubic {
            from,
            control1,
            control2,
            to,
        } => Segment::Cubic {
            from: f(from),
            control1: f(control1),
            control2: f(control2),
            to: f(to),
        },
    }
}

/// A closed chain of segments. Fill rules use the winding number, so contours wound one way
/// add and contours wound the other way (holes) subtract.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Contour {
    /// The segments, in order along the contour.
    pub segments: Vec<Segment>,
}

/// A glyph outline: one or more contours in font units, y up.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outline {
    /// The contours. Nested contours with opposite winding produce holes ('O').
    pub contours: Vec<Contour>,
}

impl Outline {
    /// True when no contour carries any segment.
    pub fn is_empty(&self) -> bool {
        self.contours.iter().all(|c| c.segments.is_empty())
    }
}

/// True when every point of every segment is finite.
fn outline_finite(outline: &Outline) -> bool {
    let ok = |p: Point2| p.x.is_finite() && p.y.is_finite();
    outline.contours.iter().all(|c| {
        c.segments.iter().all(|s| match s {
            Segment::Line { from, to } => ok(*from) && ok(*to),
            Segment::Quadratic { from, control, to } => ok(*from) && ok(*control) && ok(*to),
            Segment::Cubic {
                from,
                control1,
                control2,
                to,
            } => ok(*from) && ok(*control1) && ok(*control2) && ok(*to),
        })
    })
}

// ---------------------------------------------------------------------------
// Edge colouring (Chlumský 2015, ch. 3)
// ---------------------------------------------------------------------------

/// The colour of an edge: which pair of the three distance fields (R, G, B) measures it.
///
/// Cyan edges are measured by G and B, magenta by R and B, yellow by R and G — every edge
/// appears in exactly two channels, and at a sharp corner the two edges share no channel pair,
/// so at least two fields measure the corner's two edges separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeColor {
    /// Measured by the G and B channels.
    Cyan,
    /// Measured by the R and B channels.
    Magenta,
    /// Measured by the R and G channels.
    Yellow,
}

impl EdgeColor {
    /// Which of the `(red, green, blue)` distance fields measure edges of this colour.
    pub fn channels(self) -> [bool; 3] {
        match self {
            EdgeColor::Cyan => [false, true, true],
            EdgeColor::Magenta => [true, false, true],
            EdgeColor::Yellow => [true, true, false],
        }
    }

    /// The next colour of the deterministic rotation Yellow → Cyan → Magenta → Yellow.
    pub fn next(self) -> Self {
        match self {
            EdgeColor::Yellow => EdgeColor::Cyan,
            EdgeColor::Cyan => EdgeColor::Magenta,
            EdgeColor::Magenta => EdgeColor::Yellow,
        }
    }
}

/// True when the turn from `prev` (at its end) to `cur` (at its start) is sharper than the
/// threshold: the dot product of the two unit tangents falls below `cos(threshold)`, so a
/// straight continuation (dot ≈ 1) never switches and a corner does.
fn corner_turn_sharp(prev: &Segment, cur: &Segment, cos_threshold: f64) -> bool {
    // `direction_at` falls back to the control polygon where a curve's tangent vanishes (a
    // straight line stored as a cubic with its controls on its endpoints).
    match (unit(direction_at(prev, 1.0)), unit(direction_at(cur, 0.0))) {
        (Some(a), Some(b)) => a.dot(b) < cos_threshold,
        _ => false,
    }
}

/// Colours the outline's edges so the two edges of every sharp corner get different colours
/// (Chlumský 2015, ch. 3.2, "edge colouring, simple variant").
///
/// Returns one colour per segment, per contour. Each contour starts on the next colour of the
/// [`EdgeColor::next`] rotation (so consecutive contours — an 'O' and its hole — differ);
/// walking the contour, a corner whose tangent turns more than `turn_threshold_degrees`
/// switches to the next colour, a smooth join keeps it. After the walk, the closing vertex is
/// repaired: if the wrap-around corner is sharp and the last and first edges ended up the same
/// colour (possible on an all-corner contour such as a square, where 3 colours must colour a
/// 4-cycle), the first edge is recoloured to a colour neither neighbour uses — always possible
/// with three colours.
pub fn color_outline(outline: &Outline, turn_threshold_degrees: f64) -> Vec<Vec<EdgeColor>> {
    let cos_threshold = if turn_threshold_degrees.is_finite() {
        turn_threshold_degrees.to_radians().cos()
    } else {
        // A non-finite threshold never switches colour.
        -2.0
    };
    outline
        .contours
        .iter()
        .enumerate()
        .map(|(ci, contour)| {
            let n = contour.segments.len();
            if n == 0 {
                return Vec::new();
            }
            let mut color = COLOR_CYCLE[ci % COLOR_CYCLE.len()];
            let mut colors = vec![EdgeColor::Yellow; n];
            // Zero-length segments carry no direction and are skipped by `generate`: corners
            // are judged between consecutive non-degenerate segments.
            let live: Vec<usize> = (0..n).filter(|&k| !contour.segments[k].is_degenerate()).collect();
            let m = live.len();
            for (j, &k) in live.iter().enumerate() {
                let prev = &contour.segments[live[(j + m - 1) % m]];
                if corner_turn_sharp(prev, &contour.segments[k], cos_threshold) {
                    color = color.next();
                }
                colors[k] = color;
            }
            // Repair the closing vertex of an all-corner contour.
            if m >= 2 {
                let (first, second, last) = (live[0], live[1], live[m - 1]);
                if corner_turn_sharp(&contour.segments[last], &contour.segments[first], cos_threshold)
                    && colors[last] == colors[first]
                {
                    for candidate in COLOR_CYCLE {
                        if candidate != colors[last] && candidate != colors[second] {
                            colors[first] = candidate;
                            break;
                        }
                    }
                }
            }
            colors
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Robust polynomial root finding (Chlumský 2015, § 2.2)
// ---------------------------------------------------------------------------

/// Strips trailing zero coefficients.
fn trim_zeros(coeffs: &[f64]) -> &[f64] {
    let mut n = coeffs.len();
    while n > 0 && coeffs[n - 1] == 0.0 {
        n -= 1;
    }
    &coeffs[..n]
}

/// All distinct real roots in `[0, 1]` of the polynomial with coefficients `coeffs` (lowest
/// degree first), ascending.
///
/// The solver recursively finds the roots of the derivative, which split `[0, 1]` into
/// intervals where the polynomial is monotone; in each interval a sign change is bracketed and
/// bisected to machine precision. This handles the distance polynomials of any degree without
/// closed forms (cubics give the quintic of § 2.2) and never divides by a leading zero.
/// Non-finite evaluations make every comparison false, so such intervals are simply skipped —
/// a defined fallback, never a panic.
fn real_roots01(coeffs: &[f64]) -> Vec<f64> {
    let c = trim_zeros(coeffs);
    if c.len() < 2 {
        return Vec::new();
    }
    if c.len() == 2 {
        let r = -c[0] / c[1];
        return if (0.0..=1.0).contains(&r) {
            vec![r]
        } else {
            Vec::new()
        };
    }
    let mut deriv: Vec<f64> = Vec::with_capacity(c.len() - 1);
    for (i, &a) in c.iter().enumerate().skip(1) {
        deriv.push(a * i as f64);
    }
    let crit = real_roots01(&deriv);
    let eval = |t: f64| -> f64 { c.iter().rev().fold(0.0, |acc, &a| acc * t + a) };
    let mut bounds: Vec<f64> = Vec::with_capacity(crit.len() + 2);
    bounds.push(0.0);
    bounds.extend_from_slice(&crit);
    bounds.push(1.0);
    let mut roots: Vec<f64> = Vec::new();
    for pair in bounds.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if b.partial_cmp(&a) != Some(std::cmp::Ordering::Greater) {
            continue;
        }
        let pa = eval(a);
        if pa == 0.0 {
            roots.push(a);
            continue;
        }
        if pa * eval(b) < 0.0 {
            let (mut lo, mut hi, mut flo) = (a, b, pa);
            for _ in 0..80 {
                let mid = 0.5 * (lo + hi);
                if !(mid > lo && mid < hi) {
                    break;
                }
                let fm = eval(mid);
                if fm == 0.0 {
                    lo = mid;
                    hi = mid;
                    break;
                }
                if fm * flo < 0.0 {
                    hi = mid;
                } else {
                    lo = mid;
                    flo = fm;
                }
            }
            roots.push(0.5 * (lo + hi));
        }
    }
    if eval(1.0) == 0.0 {
        roots.push(1.0);
    }
    roots.sort_unstable_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
    let mut out: Vec<f64> = Vec::with_capacity(roots.len());
    for r in roots {
        if out.last().copied().is_none_or(|l| (r - l).abs() > 1e-9) {
            out.push(r);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Exact distance to one segment (Chlumský 2015, § 2.2)
// ---------------------------------------------------------------------------

/// The closest point of the segment to `p` as `(squared distance, t)`. Lines use the clamped
/// projection; Béziers find every critical point of |p − γ(t)|² by solving
/// `(p − γ(t)) · γ'(t) = 0` — a cubic for quadratic segments, a quintic for cubics — and take
/// the best of the roots and both endpoints (ties keep the earlier candidate, so the result is
/// deterministic).
fn segment_closest(seg: &Segment, p: Point2) -> (f64, f64) {
    let (from, to) = seg.endpoints();
    let mut best = (p.minus(from).len_sq(), 0.0);
    let d_to = p.minus(to).len_sq();
    if d_to < best.0 {
        best = (d_to, 1.0);
    }
    let roots = match *seg {
        Segment::Line { from, to } => {
            let d = to.minus(from);
            let dd = d.dot(d);
            if dd > 0.0 {
                let t = (p.minus(from).dot(d) / dd).clamp(0.0, 1.0);
                let dist = p.minus(from.plus(d.scaled(t))).len_sq();
                if dist < best.0 {
                    best = (dist, t);
                }
            }
            return best;
        }
        Segment::Quadratic { from, control, to } => {
            // Power basis γ(t) = A + B·t + C·t².
            let b = control.minus(from).scaled(2.0);
            let c = from.minus(control.scaled(2.0)).plus(to);
            let q = p.minus(from);
            real_roots01(&[
                q.dot(b),
                2.0 * q.dot(c) - b.dot(b),
                -3.0 * b.dot(c),
                -2.0 * c.dot(c),
            ])
        }
        Segment::Cubic {
            from,
            control1,
            control2,
            to,
        } => {
            // Power basis γ(t) = A + B·t + C·t² + D·t³.
            let b = control1.minus(from).scaled(3.0);
            let c = from.minus(control1.scaled(2.0)).plus(control2).scaled(3.0);
            let d = control1
                .scaled(3.0)
                .minus(from)
                .minus(control2.scaled(3.0))
                .plus(to);
            let q = p.minus(from);
            real_roots01(&[
                q.dot(b),
                2.0 * q.dot(c) - b.dot(b),
                3.0 * q.dot(d) - 3.0 * b.dot(c),
                -4.0 * b.dot(d) - 2.0 * c.dot(c),
                -5.0 * c.dot(d),
                -3.0 * d.dot(d),
            ])
        }
    };
    for t in roots {
        let dist = p.minus(seg.point_at(t)).len_sq();
        if dist < best.0 {
            best = (dist, t);
        }
    }
    best
}

/// The exact minimum squared distance from `p` to the segment (see `segment_closest`).
#[cfg(test)]
fn segment_min_dist_sq(seg: &Segment, p: Point2) -> f64 {
    segment_closest(seg, p).0
}

/// A usable direction of the curve at `t`: the tangent, or — where it vanishes (a control
/// point on its endpoint) — the direction towards the next distinct control point.
fn direction_at(seg: &Segment, t: f64) -> Point2 {
    let d = seg.tangent_at(t);
    if d.len_sq() > 0.0 {
        return d;
    }
    let pts: [Point2; 4] = match *seg {
        Segment::Line { from, to } => [from, to, to, to],
        Segment::Quadratic { from, control, to } => [from, control, to, to],
        Segment::Cubic {
            from,
            control1,
            control2,
            to,
        } => [from, control1, control2, to],
    };
    if t < 0.5 {
        for q in &pts[1..] {
            let v = q.minus(pts[0]);
            if v.len_sq() > 0.0 {
                return v;
            }
        }
    } else {
        for q in pts[..3].iter().rev() {
            let v = pts[3].minus(*q);
            if v.len_sq() > 0.0 {
                return v;
            }
        }
    }
    pts[3].minus(pts[0])
}

// ---------------------------------------------------------------------------
// Per-channel distance and winding (Chlumský 2015, § 2.3)
// ---------------------------------------------------------------------------

/// One non-degenerate segment in texel space, with its bounding box for culling.
#[derive(Debug, Clone, Copy)]
struct PreparedEdge {
    seg: Segment,
    /// +1 when the filled side is left of the edge direction (`cross > 0`), −1 otherwise.
    polarity: f64,
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}

impl PreparedEdge {
    fn new(seg: Segment) -> Self {
        let (min_x, min_y, max_x, max_y) = seg.aabb();
        Self {
            seg,
            polarity: 1.0,
            min_x,
            min_y,
            max_x,
            max_y,
        }
    }
}

/// Two distances closer than this (in texels) are a tie, settled by orthogonality.
const TIE_EPSILON: f64 = 1e-9;

/// The signed pseudo-distance from `p` to the nearest edge of one channel (Chlumský 2015,
/// § 2.3): the edge is chosen by true distance, ties (the shared vertex of a corner) going to
/// the edge `p` lies more perpendicular to; the returned value is then that edge's signed
/// distance, replaced by the signed distance to the edge's tangent line when `p` lies beyond
/// the endpoint the closest point sits on (the pseudo-distance that makes isolines mitre at
/// corners). The edge side (`cross(dir, p − γ) > 0` is left) is turned into inside/outside by
/// the selected edge's contour polarity, so the result is positive inside.
fn channel_signed_distance(edges: &[PreparedEdge], p: Point2) -> f64 {
    // (distance, orthogonality, edge index, t)
    let mut best: Option<(f64, f64, usize, f64)> = None;
    for (i, e) in edges.iter().enumerate() {
        if let Some((bd, ..)) = best {
            let dx = (e.min_x - p.x).max(p.x - e.max_x).max(0.0);
            let dy = (e.min_y - p.y).max(p.y - e.max_y).max(0.0);
            let lower = (dx * dx + dy * dy).sqrt();
            if lower > bd + TIE_EPSILON {
                continue;
            }
        }
        let (dsq, t) = segment_closest(&e.seg, p);
        let d = dsq.sqrt();
        let ortho = match (unit(direction_at(&e.seg, t)), unit(p.minus(e.seg.point_at(t)))) {
            (Some(dir), Some(v)) => dir.cross(v).abs(),
            _ => 1.0,
        };
        let better = match best {
            None => true,
            Some((bd, bo, ..)) => d < bd - TIE_EPSILON || (d <= bd + TIE_EPSILON && ortho > bo),
        };
        if better {
            best = Some((d, ortho, i, t));
        }
    }
    let Some((d, _, i, t)) = best else {
        return f64::INFINITY;
    };
    let seg = &edges[i].seg;
    let pt = seg.point_at(t);
    let side = unit(direction_at(seg, t)).map_or(0.0, |dir| dir.cross(p.minus(pt)));
    let real = if side < 0.0 { -d } else { d };
    // Pseudo-distance beyond the endpoint the closest point sits on.
    let (from, to) = seg.endpoints();
    let ext = if t <= 0.0 {
        unit(direction_at(seg, 0.0)).and_then(|dir| {
            let v = p.minus(from);
            (v.dot(dir) < 0.0).then(|| dir.cross(v))
        })
    } else if t >= 1.0 {
        unit(direction_at(seg, 1.0)).and_then(|dir| {
            let v = p.minus(to);
            (v.dot(dir) > 0.0).then(|| dir.cross(v))
        })
    } else {
        None
    };
    let polarity = edges[i].polarity;
    match ext {
        Some(pd) if pd.abs() <= d => polarity * pd,
        _ => polarity * real,
    }
}

/// The side of `contour` that is filled, from the winding number of the whole outline just
/// left and just right of the middle of the contour's longest segment: `Some(1.0)` when the
/// left side (`cross > 0`) is inside and the right is not, `Some(-1.0)` for the reverse, and
/// `None` when the test cannot tell (no segment, or both sides inside or both outside, as in
/// a region where contours overlap).
fn contour_polarity(contour: &Contour, all: &[PreparedEdge]) -> Option<f64> {
    let mut best: Option<(f64, &Segment)> = None;
    for seg in contour.segments.iter().filter(|s| !s.is_degenerate()) {
        let (from, to) = seg.endpoints();
        let len = to
            .minus(from)
            .len_sq()
            .max(seg.point_at(0.5).minus(from).len_sq());
        if best.is_none_or(|(l, _)| len > l) {
            best = Some((len, seg));
        }
    }
    let (_, seg) = best?;
    let mid = seg.point_at(0.5);
    let dir = unit(direction_at(seg, 0.5))?;
    let step = 1e-3;
    let left = Point2::new(mid.x - dir.y * step, mid.y + dir.x * step);
    let right = Point2::new(mid.x + dir.y * step, mid.y - dir.x * step);
    match (winding_number(all, left) != 0, winding_number(all, right) != 0) {
        (true, false) => Some(1.0),
        (false, true) => Some(-1.0),
        _ => None,
    }
}

/// Twice the signed area of the outline (positive when the filled side is on the left of the
/// edge direction, `cross > 0`), by the shoelace sum over each segment sampled at 32 steps:
/// exact for lines, and accurate enough for the sign that `generate` needs.
fn outline_orientation(outline: &Outline) -> f64 {
    let mut twice_area = 0.0;
    for contour in &outline.contours {
        for seg in &contour.segments {
            let steps = match seg {
                Segment::Line { .. } => 1u32,
                _ => 32,
            };
            let mut prev = seg.point_at(0.0);
            for k in 1..=steps {
                let cur = seg.point_at(f64::from(k) / f64::from(steps));
                twice_area += prev.cross(cur);
                prev = cur;
            }
        }
    }
    twice_area
}

/// The parameters in `[0, 1)` where the segment crosses the horizontal line `y`.
fn y_crossing_params(seg: &Segment, y: f64) -> Vec<f64> {
    match *seg {
        Segment::Line { from, to } => real_roots01(&[from.y - y, to.y - from.y]),
        Segment::Quadratic { from, control, to } => {
            let b = control.minus(from).scaled(2.0);
            let c = from.minus(control.scaled(2.0)).plus(to);
            real_roots01(&[from.y - y, b.y, c.y])
        }
        Segment::Cubic {
            from,
            control1,
            control2,
            to,
        } => {
            let b = control1.minus(from).scaled(3.0);
            let c = from.minus(control1.scaled(2.0)).plus(control2).scaled(3.0);
            let d = control1
                .scaled(3.0)
                .minus(from)
                .minus(control2.scaled(3.0))
                .plus(to);
            real_roots01(&[from.y - y, b.y, c.y, d.y])
        }
    }
}

/// The winding number of the outline at `p` by crossings of the horizontal ray to +x (nonzero
/// rule). Every curve point is classified as above the ray (`y > p.y`) or not, with the exact
/// coordinates at the segment endpoints, so a contour vertex lying exactly on the ray is
/// classified the same way by both segments that share it: a pass-through counts once, and a
/// vertex that is a local extremum in y counts zero or twice with opposite signs. Along a
/// segment the class changes only at the roots of `y(t) = p.y`; each change right of `p`
/// counts +1 going above and −1 going below (a tangential touch does not change the class
/// and counts nothing).
fn winding_number(edges: &[PreparedEdge], p: Point2) -> i32 {
    let mut w = 0i32;
    for e in edges {
        let seg = &e.seg;
        let (from, to) = seg.endpoints();
        let roots = y_crossing_params(seg, p.y);
        // Breakpoints along t: 0, the roots, 1. Classes: the start point, the middle of each
        // interval between breakpoints, the end point.
        let mut breaks: Vec<f64> = Vec::with_capacity(roots.len() + 2);
        breaks.push(0.0);
        breaks.extend(roots.iter().copied().filter(|t| *t > 0.0 && *t < 1.0));
        breaks.push(1.0);
        let mut prev_above = from.y > p.y;
        for pair in breaks.windows(2) {
            let mid = 0.5 * (pair[0] + pair[1]);
            let above = seg.point_at(mid).y > p.y;
            if above != prev_above {
                // The class changed at the breakpoint `pair[0]`.
                if seg.point_at(pair[0]).x > p.x {
                    w += if above { 1 } else { -1 };
                }
                prev_above = above;
            }
        }
        let end_above = to.y > p.y;
        if end_above != prev_above && to.x > p.x {
            w += if end_above { 1 } else { -1 };
        }
    }
    w
}

// ---------------------------------------------------------------------------
// Bitmap, median, error correction, reference sampler (Chlumský 2015, ch. 4)
// ---------------------------------------------------------------------------

/// The median of three values (the order statistic, not the mean).
pub fn median3(a: f32, b: f32, c: f32) -> f32 {
    if a < b {
        if b < c {
            b
        } else if a < c {
            c
        } else {
            a
        }
    } else if a < c {
        a
    } else if b < c {
        c
    } else {
        b
    }
}

/// The median of three `f64` values, the reference-sampler variant of [`median3`].
pub fn median3_f64(a: f64, b: f64, c: f64) -> f64 {
    if a < b {
        if b < c {
            b
        } else if a < c {
            c
        } else {
            a
        }
    } else if a < c {
        a
    } else if b < c {
        c
    } else {
        b
    }
}

/// A generated MSDF: `width × height` RGB texels, row-major, row 0 at the top. Each channel
/// stores `0.5 + 0.5 · d / range`, where `d` is the signed distance in texels to the nearest
/// edge of that channel's colour — positive inside (coverage above one half), `0.5` exactly on
/// the edge, and `0` or `1` at `range` texels on each side of the edge. Channels with no edges hold
/// [`DEAD_CHANNEL_DISTANCE`] (saturated on output, never the median).
#[derive(Debug, Clone, PartialEq)]
pub struct MsdfBitmap {
    width: u32,
    height: u32,
    data: Vec<f32>,
}

impl MsdfBitmap {
    /// Wraps raw texel data (RGB triples, row-major, row 0 at the top). Returns `None` when
    /// the size is empty or the data length does not match `width × height × 3`.
    pub fn from_texels(width: u32, height: u32, data: Vec<f32>) -> Option<Self> {
        if width == 0 || height == 0 {
            return None;
        }
        let expected = u64::from(width) * u64::from(height) * 3;
        if u64::try_from(data.len()) != Ok(expected) {
            return None;
        }
        Some(Self { width, height, data })
    }

    /// The bitmap width in texels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// The bitmap height in texels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The `(r, g, b)` texel at `(x, y)`, or `None` outside the bitmap.
    pub fn texel(&self, x: u32, y: u32) -> Option<[f32; 3]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let i = (y as usize * self.width as usize + x as usize) * 3;
        Some([self.data[i], self.data[i + 1], self.data[i + 2]])
    }

    /// Overwrites the texel at `(x, y)`; `None` (and no change) outside the bitmap.
    pub fn set_texel(&mut self, x: u32, y: u32, rgb: [f32; 3]) -> Option<()> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let i = (y as usize * self.width as usize + x as usize) * 3;
        self.data[i] = rgb[0];
        self.data[i + 1] = rgb[1];
        self.data[i + 2] = rgb[2];
        Some(())
    }

    /// The raw RGB `f32` texels, row-major (row 0 at the top), three per texel.
    pub fn data_f32(&self) -> &[f32] {
        &self.data
    }

    /// The bitmap quantised to bytes: each channel clamped to `[0, 1]` and rounded to
    /// `0..=255` (so an on-edge texel is 128). Same length and order as [`Self::data_f32`].
    pub fn to_u8(&self) -> Vec<u8> {
        self.data
            .iter()
            .map(|&v| {
                let c = v.clamp(0.0, 1.0);
                (c * 255.0).round() as u8
            })
            .collect()
    }

    /// A shape-free, value-space clash heuristic (kept for callers that only have a bitmap;
    /// [`generate`] does not use it — it runs a shape-aware pass that checks the reconstruction
    /// against the outline). On the module's test shapes this heuristic changes 40–90 % of the
    /// texels without fixing anything, and softens corners slightly. The rule: for every
    /// pair of horizontally or vertically adjacent texels, if one channel moves against the
    /// median while changing less than the median does, that channel is measuring a different
    /// (farther) edge than the one the median follows — the classic colour-clash artifact at a
    /// sharp corner. The offending texel (the one where the channel disagrees with its own
    /// median more) has that channel replaced by the median of its other two channels. The
    /// detection runs on a snapshot, so the scan order is fixed and the pass is deterministic.
    pub fn correct_errors(&mut self) {
        let w = self.width as usize;
        let h = self.height as usize;
        if w == 0 || h == 0 {
            return;
        }
        let src = self.data.clone();
        let idx = |x: usize, y: usize| (y * w + x) * 3;
        let mut fixes: Vec<(usize, f32)> = Vec::new();
        for y in 0..h {
            for x in 0..w {
                for (nx, ny) in [(x + 1, y), (x, y + 1)] {
                    if nx >= w || ny >= h {
                        continue;
                    }
                    let (ia, ib) = (idx(x, y), idx(nx, ny));
                    let a = [src[ia], src[ia + 1], src[ia + 2]];
                    let b = [src[ib], src[ib + 1], src[ib + 2]];
                    let ma = median3(a[0], a[1], a[2]);
                    let mb = median3(b[0], b[1], b[2]);
                    let dm = mb - ma;
                    if dm == 0.0 || !dm.is_finite() {
                        continue;
                    }
                    for c in 0..3 {
                        let da = b[c] - a[c];
                        if da * dm < 0.0 && da.abs() < dm.abs() {
                            let offender = if (b[c] - mb).abs() > (a[c] - ma).abs() {
                                ib
                            } else {
                                ia
                            };
                            let others = if c == 0 {
                                [src[offender + 1], src[offender + 2]]
                            } else if c == 1 {
                                [src[offender], src[offender + 2]]
                            } else {
                                [src[offender], src[offender + 1]]
                            };
                            fixes.push((offender + c, 0.5 * (others[0] + others[1])));
                        }
                    }
                }
            }
        }
        for (i, v) in fixes {
            self.data[i] = v;
        }
    }

    /// Reconstructs coverage at continuous texel coordinates `(u, v)` (texel centers at
    /// `k + 0.5`, `v` growing down, the top-left corner of the bitmap at `(0, 0)`):
    /// bilinear filtering per channel with clamp-to-edge, then [`median3_f64`], then a
    /// smoothstep `t²(3 − 2t)` of the clamped median — the CPU reference of the shader-side
    /// reconstruction. Returns 0 for an empty bitmap.
    pub fn coverage(&self, u: f64, v: f64) -> f64 {
        let w = self.width as usize;
        let h = self.height as usize;
        if w == 0 || h == 0 || self.data.len() != w * h * 3 {
            return 0.0;
        }
        let cx = (u - 0.5).clamp(0.0, w as f64 - 1.0);
        let cy = (v - 0.5).clamp(0.0, h as f64 - 1.0);
        let x0 = cx.floor() as usize;
        let y0 = cy.floor() as usize;
        let x1 = (x0 + 1).min(w - 1);
        let y1 = (y0 + 1).min(h - 1);
        let fx = cx - x0 as f64;
        let fy = cy - y0 as f64;
        let at = |x: usize, y: usize| -> [f64; 3] {
            let i = (y * w + x) * 3;
            [
                f64::from(self.data[i]),
                f64::from(self.data[i + 1]),
                f64::from(self.data[i + 2]),
            ]
        };
        let (t00, t10, t01, t11) = (at(x0, y0), at(x1, y0), at(x0, y1), at(x1, y1));
        let mut ch = [0.0f64; 3];
        for k in 0..3 {
            let top = t00[k] * (1.0 - fx) + t10[k] * fx;
            let bot = t01[k] * (1.0 - fx) + t11[k] * fx;
            ch[k] = top * (1.0 - fy) + bot * fy;
        }
        let t = median3_f64(ch[0], ch[1], ch[2]).clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    }
}

// ---------------------------------------------------------------------------
// Generation
// ---------------------------------------------------------------------------

/// Errors of the MSDF module: malformed parameters and malformed fonts. Degenerate geometry
/// is not an error — it yields a defined field.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum MsdfError {
    /// A parameter or outline point is NaN or infinite.
    #[error("msdf: {what} is not a finite number")]
    NotFinite {
        /// What was not finite.
        what: &'static str,
    },
    /// A parameter has a value the method cannot use (e.g. a zero range or scale).
    #[error("msdf: invalid {what}: {value}")]
    Invalid {
        /// What was invalid.
        what: &'static str,
        /// The offending value.
        value: f64,
    },
    /// The bitmap size is empty or larger than 16777216 texels.
    #[error("msdf: bitmap size {width}x{height} is empty or larger than 16777216 texels")]
    BadSize {
        /// The requested width.
        width: u32,
        /// The requested height.
        height: u32,
    },
    /// The font's sfnt version is not one with `glyf` outlines (TrueType); CFF/OTF fonts are
    /// rejected.
    #[error("font: unsupported sfnt version {tag:#010x} (only TrueType `glyf` outlines)")]
    UnsupportedFont {
        /// The sfnt version tag found in the file.
        tag: u32,
    },
    /// A required table (`head`, `maxp`, `loca`, `glyf`) is missing.
    #[error("font: table `{table}` not found")]
    MissingTable {
        /// The missing table's name.
        table: &'static str,
    },
    /// A table or the header ends before the fields this module reads.
    #[error("font: table `{table}` is truncated or malformed")]
    TruncatedTable {
        /// The table that ended too early.
        table: &'static str,
    },
    /// The glyph index is not below `maxp`'s `numGlyphs`.
    #[error("font: glyph index {index} out of range (font has {count} glyphs)")]
    GlyphIndexOutOfRange {
        /// The requested glyph index.
        index: u16,
        /// The font's glyph count.
        count: u16,
    },
    /// A composite glyph nests components more than 8 levels deep (a cyclic reference).
    #[error("font: composite glyph nesting deeper than 8 levels")]
    CompositeDepth,
    /// A composite glyph matches components by point numbers instead of offsets, a legacy
    /// feature this parser does not support.
    #[error("font: composite glyph uses point-number component offsets, which are unsupported")]
    UnsupportedComposite,
    /// The glyph expands to more than `MAX_GLYPH_SEGMENTS` segments (a composite that reuses
    /// components so often that its outline would grow without useful bound).
    #[error("font: glyph expands to more than {limit} segments")]
    GlyphTooComplex {
        /// The segment budget that was exceeded.
        limit: usize,
    },
}

/// Maps an outline into texel space: `x' = (x − translate.x) · scale.x`,
/// `y' = height − (y − translate.y) · scale.y`, so row 0 of the bitmap is the top (font y up,
/// bitmap y down).
fn map_outline(outline: &Outline, height: f64, scale: [f64; 2], translate: [f64; 2]) -> Outline {
    let f = |p: Point2| {
        Point2::new(
            (p.x - translate[0]) * scale[0],
            height - (p.y - translate[1]) * scale[1],
        )
    };
    Outline {
        contours: outline
            .contours
            .iter()
            .map(|c| Contour {
                segments: c.segments.iter().map(|s| map_segment(s, f)).collect(),
            })
            .collect(),
    }
}

/// Generates the MSDF of `outline` without the error-correction pass (see [`generate`], which
/// runs it). Useful for tests that want to isolate the correction's effect.
///
/// * `size` — bitmap dimensions in texels;
/// * `range` — distance in texels the field spans on each side of the edge;
/// * `scale` — texels per font unit on each axis (both components non-zero; equal components
///   keep the shape's proportions, a negative component mirrors);
/// * `translate` — the font-space point that maps to the bitmap's bottom-left corner.
///
/// Distances are computed in texel space after the mapping, so `range` is in texels along
/// either axis regardless of `scale`.
pub fn generate_uncorrected(
    outline: &Outline,
    size: [u32; 2],
    range: f64,
    scale: [f64; 2],
    translate: [f64; 2],
) -> Result<MsdfBitmap, MsdfError> {
    generate_field(outline, size, range, scale, translate).map(|(bitmap, _)| bitmap)
}

/// The field of [`generate_uncorrected`] plus every non-degenerate edge in texel space (for
/// the shape-aware error correction).
fn generate_field(
    outline: &Outline,
    size: [u32; 2],
    range: f64,
    scale: [f64; 2],
    translate: [f64; 2],
) -> Result<(MsdfBitmap, Vec<PreparedEdge>), MsdfError> {
    let _span = tracing::trace_span!("msdf_generate").entered();
    let [width, height] = size;
    if width == 0 || height == 0 {
        return Err(MsdfError::BadSize { width, height });
    }
    if u64::from(width) * u64::from(height) > u64::from(MAX_TEXELS) {
        return Err(MsdfError::BadSize { width, height });
    }
    if !range.is_finite() {
        return Err(MsdfError::NotFinite { what: "range" });
    }
    if range <= 0.0 {
        return Err(MsdfError::Invalid {
            what: "range",
            value: range,
        });
    }
    if !scale[0].is_finite() || !scale[1].is_finite() {
        return Err(MsdfError::NotFinite { what: "scale" });
    }
    if !translate[0].is_finite() || !translate[1].is_finite() {
        return Err(MsdfError::NotFinite { what: "translate" });
    }
    if scale[0] == 0.0 || scale[1] == 0.0 {
        return Err(MsdfError::Invalid {
            what: "scale (zero component)",
            value: 0.0,
        });
    }
    if !outline_finite(outline) {
        return Err(MsdfError::NotFinite {
            what: "outline point",
        });
    }

    // Colour before the transform: corner angles are the design's, not the mapping's.
    let colors = color_outline(outline, DEFAULT_TURN_THRESHOLD_DEG);
    let texel = map_outline(outline, f64::from(height), scale, translate);

    let all: Vec<PreparedEdge> = texel
        .contours
        .iter()
        .flat_map(|c| c.segments.iter())
        .filter(|s| !s.is_degenerate())
        .map(|s| PreparedEdge::new(*s))
        .collect();
    // Which side of an edge is inside. The outline's orientation (positive signed area means
    // the filled side lies left of the edge direction) is the fallback; each contour then
    // checks its own side against the winding number, so a hole, a mirrored composite
    // component or an oppositely wound contour gets its own sign. The per-texel winding check
    // below repairs whatever is left.
    let glyph_polarity = if outline_orientation(&texel) < 0.0 {
        -1.0
    } else {
        1.0
    };
    let mut channels: [Vec<PreparedEdge>; 3] = Default::default();
    for (ci, contour) in texel.contours.iter().enumerate() {
        let cols = match colors.get(ci) {
            Some(c) => c,
            None => continue,
        };
        let polarity = contour_polarity(contour, &all).unwrap_or(glyph_polarity);
        for (seg, &color) in contour.segments.iter().zip(cols) {
            if seg.is_degenerate() {
                continue;
            }
            let mut pe = PreparedEdge::new(*seg);
            pe.polarity = polarity;
            for (k, on) in color.channels().into_iter().enumerate() {
                if on {
                    channels[k].push(pe);
                }
            }
        }
    }
    let mut data: Vec<f32> =
        Vec::with_capacity(usize::try_from(u64::from(width) * u64::from(height) * 3).unwrap_or(0));
    for row in 0..height {
        let py = f64::from(row) + 0.5;
        for col in 0..width {
            let p = Point2::new(f64::from(col) + 0.5, py);
            let inside = winding_number(&all, p) != 0;
            let dead = if inside {
                DEAD_CHANNEL_DISTANCE
            } else {
                -DEAD_CHANNEL_DISTANCE
            };
            let mut d = [dead; 3];
            for (slot, edges) in d.iter_mut().zip(&channels) {
                if !edges.is_empty() {
                    *slot = channel_signed_distance(edges, p);
                }
            }
            // Sign by winding number: where the median disagrees with the fill (a malformed
            // or self-overlapping outline), flip the live channels so the texel's median has
            // the winding's sign.
            let med = median3_f64(d[0], d[1], d[2]);
            if (inside && med < 0.0) || (!inside && med > 0.0) {
                for (slot, edges) in d.iter_mut().zip(&channels) {
                    if !edges.is_empty() {
                        *slot = -*slot;
                    }
                }
            }
            for dist in d {
                let v = 0.5 + 0.5 * dist / range;
                // Extreme (but finite) inputs can overflow the arithmetic; keep the texel
                // defined rather than storing a NaN.
                let v = if v.is_finite() {
                    v
                } else if v > 0.0 {
                    f64::INFINITY
                } else {
                    f64::NEG_INFINITY
                };
                data.push(v as f32);
            }
        }
    }
    Ok((MsdfBitmap { width, height, data }, all))
}

/// A reconstruction error below this many texels from the true edge is ordinary bilinear
/// interpolation error, not a channel clash.
const CLASH_MIN_DISTANCE: f64 = 0.5;

/// The exact signed distance (texels, positive inside) from `p` to the outline.
fn true_signed_distance(edges: &[PreparedEdge], p: Point2) -> f64 {
    let mut best = f64::INFINITY;
    for e in edges {
        let dx = (e.min_x - p.x).max(p.x - e.max_x).max(0.0);
        let dy = (e.min_y - p.y).max(p.y - e.max_y).max(0.0);
        if dx * dx + dy * dy > best {
            continue;
        }
        best = best.min(segment_closest(&e.seg, p).0);
    }
    let d = best.sqrt();
    if winding_number(edges, p) != 0 { d } else { -d }
}

/// The error-correction pass of Chlumský 2015, ch. 4, checked against the shape: a channel
/// clash shows up as a reconstruction whose median has the wrong side of the edge *between*
/// texels, where bilinear filtering mixes two texels whose channels measure different edges.
/// For every horizontal and vertical texel pair the midpoint (the average of the two texels)
/// and for every 2×2 cell its center (the average of four) are reconstructed and compared with
/// the exact signed distance of the outline there; where the sign is wrong, the true edge is
/// at least `CLASH_MIN_DISTANCE` texels away and every texel of the pair or cell is itself on
/// the right side (so only their mix is wrong), every texel of that pair or cell falls back to
/// its median in all three channels (a plain SDF texel, which cannot clash). Texels with no
/// clash keep their channels, so corners stay sharp. Detection runs on the uncorrected field
/// in a fixed scan order, so the pass is deterministic. Returns the number of texels changed.
fn correct_clashes(bitmap: &mut MsdfBitmap, edges: &[PreparedEdge], range: f64) -> usize {
    let w = bitmap.width as usize;
    let h = bitmap.height as usize;
    if edges.is_empty() || w == 0 || h == 0 || bitmap.data.len() != w * h * 3 {
        return 0;
    }
    let src = &bitmap.data;
    let at = |x: usize, y: usize| -> [f64; 3] {
        let i = (y * w + x) * 3;
        [f64::from(src[i]), f64::from(src[i + 1]), f64::from(src[i + 2])]
    };
    let mut flagged = vec![false; w * h];
    let mut check = |texels: &[(usize, usize)], q: Point2| {
        let mut sum = [0.0f64; 3];
        for &(x, y) in texels {
            let t = at(x, y);
            for (acc, v) in sum.iter_mut().zip(t) {
                *acc += v;
            }
        }
        let n = texels.len() as f64;
        let rec = (median3_f64(sum[0] / n, sum[1] / n, sum[2] / n) - 0.5) * 2.0 * range;
        if rec == 0.0 {
            return;
        }
        // A clash is an interpolation artifact: every texel is on one side and only their mix
        // is on the other. Checking that first keeps the exact-distance query rare.
        let mix_side = rec > 0.0;
        let centers_opposite = texels.iter().all(|&(x, y)| {
            let t = at(x, y);
            let m = median3_f64(t[0], t[1], t[2]) - 0.5;
            m != 0.0 && (m > 0.0) != mix_side
        });
        if !centers_opposite {
            return;
        }
        // The texels are on the true side and the mix is not (this also ignores a point whose
        // own winding test is unreliable, e.g. a ray through a vertex of a contour that is not
        // watertight, since the texel centers were checked by winding during generation).
        let truth = true_signed_distance(edges, q);
        if truth.abs() >= CLASH_MIN_DISTANCE && (truth > 0.0) != mix_side {
            for &(x, y) in texels {
                flagged[y * w + x] = true;
            }
        }
    };
    for y in 0..h {
        for x in 0..w {
            let (cx, cy) = (x as f64 + 0.5, y as f64 + 0.5);
            if x + 1 < w {
                check(&[(x, y), (x + 1, y)], Point2::new(cx + 0.5, cy));
            }
            if y + 1 < h {
                check(&[(x, y), (x, y + 1)], Point2::new(cx, cy + 0.5));
            }
            if x + 1 < w && y + 1 < h {
                check(
                    &[(x, y), (x + 1, y), (x, y + 1), (x + 1, y + 1)],
                    Point2::new(cx + 0.5, cy + 0.5),
                );
            }
        }
    }
    let mut changed = 0;
    for (i, flag) in flagged.into_iter().enumerate() {
        if flag {
            let t = &mut bitmap.data[i * 3..i * 3 + 3];
            let m = median3(t[0], t[1], t[2]);
            if t.iter().any(|&v| v != m) {
                changed += 1;
            }
            t.fill(m);
        }
    }
    changed
}

/// Generates the multi-channel signed distance field of `outline` (Chlumský 2015): edge
/// colouring, per-channel exact signed distance with pseudo-distance corner extension, sign by
/// winding number, then the error-correction pass.
///
/// See [`generate_uncorrected`] for the parameters. The result's texels are `0.5` on the edge
/// and reach `0` and `1` at `range` texels on each side (`v = 0.5 + 0.5 · d / range`);
/// reconstruct coverage with [`MsdfBitmap::coverage`]
/// or `median3` of the three channels. Deterministic: the same outline and parameters give
/// byte-identical output on the same build.
///
/// # Errors
///
/// [`MsdfError::NotFinite`] for NaN/∞ input, [`MsdfError::Invalid`] for a non-positive range
/// or a zero scale component, [`MsdfError::BadSize`] for an empty or oversized bitmap. Empty
/// and degenerate outlines are not errors — they produce an all-outside field.
pub fn generate(
    outline: &Outline,
    size: [u32; 2],
    range: f64,
    scale: [f64; 2],
    translate: [f64; 2],
) -> Result<MsdfBitmap, MsdfError> {
    let (mut bitmap, edges) = generate_field(outline, size, range, scale, translate)?;
    correct_clashes(&mut bitmap, &edges, range);
    Ok(bitmap)
}

// ---------------------------------------------------------------------------
// TrueType `glyf` outline parsing (public specification)
// ---------------------------------------------------------------------------

/// A glyph recovered from raw font bytes: its outline in font units (y up) and the font's
/// `unitsPerEm` (for the texels-per-font-unit mapping `scale = em_size_in_texels / upem`).
#[derive(Debug, Clone, PartialEq)]
pub struct FontGlyph {
    /// The glyph's outline, contours of quadratic Béziers and lines.
    pub outline: Outline,
    /// The font's `head.unitsPerEm`.
    pub units_per_em: u16,
}

/// Reads a big-endian `u16` at `off`.
fn rd_u16(b: &[u8], off: usize) -> Option<u16> {
    let hi = *b.get(off)?;
    let lo = *b.get(off + 1)?;
    Some((u16::from(hi) << 8) | u16::from(lo))
}

/// Reads a big-endian `i16` at `off`.
fn rd_i16(b: &[u8], off: usize) -> Option<i16> {
    rd_u16(b, off).map(|v| v as i16)
}

/// Reads a big-endian `u32` at `off`.
fn rd_u32(b: &[u8], off: usize) -> Option<u32> {
    let b0 = *b.get(off)?;
    let b1 = *b.get(off + 1)?;
    let b2 = *b.get(off + 2)?;
    let b3 = *b.get(off + 3)?;
    Some((u32::from(b0) << 24) | (u32::from(b1) << 16) | (u32::from(b2) << 8) | u32::from(b3))
}

/// The tables a glyph parse needs, already sliced out of the font.
struct Tables<'a> {
    loca: &'a [u8],
    glyf: &'a [u8],
    long_loca: bool,
    num_glyphs: u16,
}

/// Finds a top-level table by tag and returns its bytes, or `None` when absent or overrunning
/// the file.
fn table_range<'a>(data: &'a [u8], tag: &[u8; 4]) -> Option<&'a [u8]> {
    let num_tables = rd_u16(data, 4)?;
    for i in 0..num_tables {
        let rec = 12 + usize::from(i) * 16;
        if rd_u32(data, rec)? == u32::from_be_bytes(*tag) {
            let off = rd_u32(data, rec + 8)? as usize;
            let len = rd_u32(data, rec + 12)? as usize;
            let end = off.checked_add(len)?;
            return data.get(off..end);
        }
    }
    None
}

/// Reads one entry of the `loca` table as a byte offset into `glyf`.
fn loca_at(t: &Tables<'_>, index: u32) -> Option<usize> {
    if t.long_loca {
        rd_u32(t.loca, usize::try_from(index).ok()? * 4).map(|v| v as usize)
    } else {
        rd_u16(t.loca, usize::try_from(index).ok()? * 2).map(|v| usize::from(v) * 2)
    }
}

/// The `glyf` bytes of one glyph, `None` when the glyph is empty (`loca[i] == loca[i+1]`).
fn glyph_slice<'a>(t: &'a Tables<'a>, index: u16) -> Result<Option<&'a [u8]>, MsdfError> {
    let start = loca_at(t, u32::from(index)).ok_or(MsdfError::TruncatedTable { table: "loca" })?;
    let end = loca_at(t, u32::from(index) + 1).ok_or(MsdfError::TruncatedTable { table: "loca" })?;
    if start == end {
        return Ok(None);
    }
    if start > end {
        return Err(MsdfError::TruncatedTable { table: "loca" });
    }
    t.glyf
        .get(start..end)
        .ok_or(MsdfError::TruncatedTable { table: "glyf" })
        .map(Some)
}

/// An F2Dot14 fixed-point value.
fn f2dot14(v: i16) -> f64 {
    f64::from(v) / 16384.0
}

/// Charges `n` segments against the glyph's remaining budget.
fn charge(budget: &mut usize, n: usize) -> Result<(), MsdfError> {
    if n > *budget {
        return Err(MsdfError::GlyphTooComplex {
            limit: MAX_GLYPH_SEGMENTS,
        });
    }
    *budget -= n;
    Ok(())
}

/// Parses one glyph (simple or composite) by index, charging every segment it produces (and
/// every component copy) against `budget`.
fn parse_glyph(t: &Tables<'_>, index: u16, depth: u8, budget: &mut usize) -> Result<Outline, MsdfError> {
    if index >= t.num_glyphs {
        return Err(MsdfError::GlyphIndexOutOfRange {
            index,
            count: t.num_glyphs,
        });
    }
    let Some(g) = glyph_slice(t, index)? else {
        return Ok(Outline::default());
    };
    let n_contours = rd_i16(g, 0).ok_or(MsdfError::TruncatedTable { table: "glyf" })?;
    if n_contours >= 0 {
        let contours = parse_simple_glyph(g, n_contours as u16)?;
        charge(budget, contours.iter().map(|c| c.segments.len()).sum())?;
        Ok(Outline { contours })
    } else {
        parse_composite_glyph(g, t, depth, budget)
    }
}

/// Parses a simple glyph's body (after the 10-byte header) into contours.
fn parse_simple_glyph(g: &[u8], n_contours: u16) -> Result<Vec<Contour>, MsdfError> {
    let bad = MsdfError::TruncatedTable { table: "glyf" };
    let mut pos = 10usize;
    let mut end_pts: Vec<u16> = Vec::with_capacity(usize::from(n_contours));
    for _ in 0..n_contours {
        end_pts.push(rd_u16(g, pos).ok_or(bad)?);
        pos += 2;
    }
    let instr_len = rd_u16(g, pos).ok_or(bad)?;
    pos += 2 + usize::from(instr_len);
    if pos > g.len() {
        return Err(bad);
    }
    let n_points = match end_pts.last() {
        Some(&e) => usize::from(e) + 1,
        None => 0,
    };

    // Flags (with the repeat escape).
    let mut flags: Vec<u8> = vec![0; n_points];
    let mut i = 0usize;
    while i < n_points {
        let f = *g.get(pos).ok_or(bad)?;
        pos += 1;
        flags[i] = f;
        i += 1;
        if f & 0x08 != 0 {
            let extra = *g.get(pos).ok_or(bad)? as usize;
            pos += 1;
            for _ in 0..extra {
                if i >= n_points {
                    break;
                }
                flags[i] = f;
                i += 1;
            }
        }
    }

    // Coordinates, delta-coded.
    let mut xs: Vec<i32> = vec![0; n_points];
    let mut acc = 0i32;
    for (k, &f) in flags.iter().enumerate() {
        if f & 0x02 != 0 {
            let d = *g.get(pos).ok_or(bad)? as i32;
            pos += 1;
            acc += if f & 0x10 != 0 { d } else { -d };
        } else if f & 0x10 == 0 {
            acc += i32::from(rd_i16(g, pos).ok_or(bad)?);
            pos += 2;
        }
        xs[k] = acc;
    }
    let mut ys: Vec<i32> = vec![0; n_points];
    let mut acc = 0i32;
    for (k, &f) in flags.iter().enumerate() {
        if f & 0x04 != 0 {
            let d = *g.get(pos).ok_or(bad)? as i32;
            pos += 1;
            acc += if f & 0x20 != 0 { d } else { -d };
        } else if f & 0x20 == 0 {
            acc += i32::from(rd_i16(g, pos).ok_or(bad)?);
            pos += 2;
        }
        ys[k] = acc;
    }

    // Split into contours and turn TrueType point sequences into segments: consecutive
    // off-curve points imply on-curve midpoints, so between two on-curve points with k
    // off-curve points between them the contour runs k quadratic Béziers.
    let mut contours: Vec<Contour> = Vec::with_capacity(usize::from(n_contours));
    let mut start = 0usize;
    for &e in &end_pts {
        let e = usize::from(e);
        if e < start || e >= n_points {
            return Err(bad);
        }
        let pts: Vec<(f64, f64, bool)> = (start..=e)
            .map(|k| (f64::from(xs[k]), f64::from(ys[k]), flags[k] & 0x01 != 0))
            .collect();
        contours.push(Contour {
            segments: points_to_segments(&pts),
        });
        start = e + 1;
    }
    Ok(contours)
}

/// Converts one contour's `(x, y, on-curve)` points into segments. The walk starts at the
/// first on-curve point (a contour with none is skipped, as legacy rasterizers do) and closes
/// back onto it; trailing off-curve points become the closing quadratic(s).
fn points_to_segments(pts: &[(f64, f64, bool)]) -> Vec<Segment> {
    let n = pts.len();
    let Some(first_on) = pts.iter().position(|p| p.2) else {
        return Vec::new();
    };
    let p_at = |i: usize| Point2::new(pts[i % n].0, pts[i % n].1);
    let mut segments: Vec<Segment> = Vec::new();
    let mut cur = p_at(first_on);
    let mut pending: Vec<Point2> = Vec::new();
    for k in 1..=n {
        let (x, y, on) = pts[(first_on + k) % n];
        let p = Point2::new(x, y);
        if on {
            if pending.is_empty() {
                if p != cur {
                    segments.push(Segment::Line { from: cur, to: p });
                }
            } else if pending.len() == 1 {
                segments.push(Segment::Quadratic {
                    from: cur,
                    control: pending[0],
                    to: p,
                });
            } else {
                let last = pending.len() - 1;
                let mut a = cur;
                for j in 0..last {
                    let mid = pending[j].plus(pending[j + 1]).scaled(0.5);
                    segments.push(Segment::Quadratic {
                        from: a,
                        control: pending[j],
                        to: mid,
                    });
                    a = mid;
                }
                segments.push(Segment::Quadratic {
                    from: a,
                    control: pending[last],
                    to: p,
                });
            }
            pending.clear();
            cur = p;
        } else {
            pending.push(p);
        }
    }
    segments
}

/// Parses a composite glyph's body: components referenced by glyph index with offsets and a
/// 2×2 transform (uniform, x/y, or full), recursing into [`parse_glyph`].
fn parse_composite_glyph(
    g: &[u8],
    t: &Tables<'_>,
    depth: u8,
    budget: &mut usize,
) -> Result<Outline, MsdfError> {
    if depth >= MAX_COMPONENT_DEPTH {
        return Err(MsdfError::CompositeDepth);
    }
    let bad = MsdfError::TruncatedTable { table: "glyf" };
    let mut pos = 10usize;
    let mut outline = Outline::default();
    loop {
        let flags = rd_u16(g, pos).ok_or(bad)?;
        pos += 2;
        let component = rd_u16(g, pos).ok_or(bad)?;
        pos += 2;
        let (a1, a2);
        if flags & 0x0001 != 0 {
            a1 = i32::from(rd_i16(g, pos).ok_or(bad)?);
            pos += 2;
            a2 = i32::from(rd_i16(g, pos).ok_or(bad)?);
            pos += 2;
        } else {
            let b1 = *g.get(pos).ok_or(bad)?;
            pos += 1;
            let b2 = *g.get(pos).ok_or(bad)?;
            pos += 1;
            a1 = i32::from(b1 as i8);
            a2 = i32::from(b2 as i8);
        }
        let (mut ma, mut mb, mut mc, mut md) = (1.0f64, 0.0f64, 0.0f64, 1.0f64);
        if flags & 0x0008 != 0 {
            let s = f2dot14(rd_i16(g, pos).ok_or(bad)?);
            pos += 2;
            ma = s;
            md = s;
        } else if flags & 0x0040 != 0 {
            ma = f2dot14(rd_i16(g, pos).ok_or(bad)?);
            pos += 2;
            md = f2dot14(rd_i16(g, pos).ok_or(bad)?);
            pos += 2;
        } else if flags & 0x0080 != 0 {
            ma = f2dot14(rd_i16(g, pos).ok_or(bad)?);
            pos += 2;
            mb = f2dot14(rd_i16(g, pos).ok_or(bad)?);
            pos += 2;
            mc = f2dot14(rd_i16(g, pos).ok_or(bad)?);
            pos += 2;
            md = f2dot14(rd_i16(g, pos).ok_or(bad)?);
            pos += 2;
        }
        let (ex, ey) = if flags & 0x0002 != 0 {
            if flags & 0x0004 != 0 {
                (f64::from(a1).round(), f64::from(a2).round())
            } else {
                (f64::from(a1), f64::from(a2))
            }
        } else {
            // Point-number matching is a legacy feature; refuse instead of guessing.
            return Err(MsdfError::UnsupportedComposite);
        };
        let sub = parse_glyph(t, component, depth + 1, budget)?;
        charge(budget, sub.contours.iter().map(|c| c.segments.len()).sum())?;
        // SCALED_COMPONENT_OFFSET (0x0800) with UNSCALED_COMPONENT_OFFSET (0x1000) clear
        // transforms the XY arguments by the component 2×2 (public glyf spec); both bits or
        // neither leave the offset unscaled.
        let (ex, ey) = if flags & 0x0800 != 0 && flags & 0x1000 == 0 {
            (ma * ex + mc * ey, mb * ex + md * ey)
        } else {
            (ex, ey)
        };
        let map = |p: Point2| Point2::new(ma * p.x + mc * p.y + ex, mb * p.x + md * p.y + ey);
        for contour in &sub.contours {
            outline.contours.push(Contour {
                segments: contour.segments.iter().map(|s| map_segment(s, map)).collect(),
            });
        }
        if flags & 0x0020 == 0 {
            break;
        }
    }
    Ok(outline)
}

/// Recovers a glyph's quadratic outline from the raw bytes of a TrueType font (or an OpenType
/// font with TrueType outlines), including composite glyphs with component transforms. No
/// font crate is needed — this reads the `head`, `maxp`, `loca` and `glyf` tables straight out
/// of the sfnt bytes, per the public TrueType specification.
///
/// Coordinates come back in font units with y up, ready for [`generate`] with
/// `scale = em_size_in_texels / units_per_em` (texels per font unit).
///
/// # Errors
///
/// [`MsdfError::UnsupportedFont`] for sfnt versions without `glyf` (CFF/OTF), the table and
/// range errors for malformed bytes, [`MsdfError::CompositeDepth`] for cyclic composites,
/// [`MsdfError::UnsupportedComposite`] for point-number-matched components,
/// [`MsdfError::GlyphTooComplex`] past [`MAX_GLYPH_SEGMENTS`] segments. An empty glyph
/// (`.notdef` with no outline, `loca[i] == loca[i+1]`) parses to an empty outline, not an
/// error.
pub fn parse_glyf_outline(font: &[u8], glyph_index: u16) -> Result<FontGlyph, MsdfError> {
    let version = rd_u32(font, 0).ok_or(MsdfError::TruncatedTable { table: "sfnt header" })?;
    // 0x00010000 and 'true' are TrueType; 'OTTO' is CFF and has no glyf outlines.
    if version != 0x0001_0000 && version != 0x7472_7565 {
        return Err(MsdfError::UnsupportedFont { tag: version });
    }
    let head = table_range(font, b"head").ok_or(MsdfError::MissingTable { table: "head" })?;
    let maxp = table_range(font, b"maxp").ok_or(MsdfError::MissingTable { table: "maxp" })?;
    let loca = table_range(font, b"loca").ok_or(MsdfError::MissingTable { table: "loca" })?;
    let glyf = table_range(font, b"glyf").ok_or(MsdfError::MissingTable { table: "glyf" })?;
    if head.len() < 52 || maxp.len() < 6 {
        return Err(MsdfError::TruncatedTable { table: "head/maxp" });
    }
    let units_per_em = rd_u16(head, 18).ok_or(MsdfError::TruncatedTable { table: "head" })?;
    if units_per_em == 0 {
        return Err(MsdfError::Invalid {
            what: "unitsPerEm",
            value: 0.0,
        });
    }
    let index_to_loc_format = rd_i16(head, 50).ok_or(MsdfError::TruncatedTable { table: "head" })?;
    if index_to_loc_format != 0 && index_to_loc_format != 1 {
        return Err(MsdfError::Invalid {
            what: "indexToLocFormat",
            value: f64::from(index_to_loc_format),
        });
    }
    let num_glyphs = rd_u16(maxp, 4).ok_or(MsdfError::TruncatedTable { table: "maxp" })?;
    let tables = Tables {
        loca,
        glyf,
        long_loca: index_to_loc_format == 1,
        num_glyphs,
    };
    let mut budget = MAX_GLYPH_SEGMENTS;
    let outline = parse_glyph(&tables, glyph_index, 0, &mut budget)?;
    Ok(FontGlyph {
        outline,
        units_per_em,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- shape helpers (font space, y up) -----------------------------------

    fn line_contour(pts: &[(f64, f64)]) -> Contour {
        let n = pts.len();
        let mut segments = Vec::with_capacity(n);
        for k in 0..n {
            let a = pts[k];
            let b = pts[(k + 1) % n];
            segments.push(Segment::Line {
                from: Point2::new(a.0, a.1),
                to: Point2::new(b.0, b.1),
            });
        }
        Contour { segments }
    }

    fn square(x0: f64, y0: f64, w: f64) -> Outline {
        Outline {
            contours: vec![line_contour(&[
                (x0, y0),
                (x0 + w, y0),
                (x0 + w, y0 + w),
                (x0, y0 + w),
            ])],
        }
    }

    /// A circle as 4 tangent-continuous cubic arcs (kappa = 4/3·(√2 − 1)).
    fn circle(cx: f64, cy: f64, r: f64) -> Contour {
        let k = (4.0 / 3.0) * (2.0f64.sqrt() - 1.0) * r;
        let pt = |a: f64| (cx + r * a.cos(), cy + r * a.sin());
        let tan = |a: f64| (-a.sin(), a.cos());
        let mut segments = Vec::with_capacity(4);
        for q in 0..4u32 {
            let a0 = f64::from(q) * (std::f64::consts::FRAC_PI_2);
            let a1 = a0 + std::f64::consts::FRAC_PI_2;
            let (x0, y0) = pt(a0);
            let (x1, y1) = pt(a1);
            let (t0x, t0y) = tan(a0);
            let (t1x, t1y) = tan(a1);
            segments.push(Segment::Cubic {
                from: Point2::new(x0, y0),
                control1: Point2::new(x0 + k * t0x, y0 + k * t0y),
                control2: Point2::new(x1 - k * t1x, y1 - k * t1y),
                to: Point2::new(x1, y1),
            });
        }
        Contour { segments }
    }

    fn ring(cx: f64, cy: f64, r_out: f64, r_in: f64) -> Outline {
        // Outer CCW, inner reversed (CW): the hole subtracts by winding. Reversing the
        // segment order and each segment's direction turns the circle's traversal around.
        let mut inner = circle(cx, cy, r_in);
        inner.segments.reverse();
        for seg in &mut inner.segments {
            let (from, to) = seg.endpoints();
            if let Segment::Cubic {
                control1, control2, ..
            } = *seg
            {
                *seg = Segment::Cubic {
                    from: to,
                    control1: control2,
                    control2: control1,
                    to: from,
                };
            }
        }
        Outline {
            contours: vec![circle(cx, cy, r_out), inner],
        }
    }

    fn star(cx: f64, cy: f64, r_out: f64, r_in: f64) -> Outline {
        let mut pts = Vec::with_capacity(10);
        for k in 0..10usize {
            let r = if k % 2 == 0 { r_out } else { r_in };
            let a = -std::f64::consts::FRAC_PI_2 + (k as f64) * (std::f64::consts::PI / 5.0);
            pts.push((cx + r * a.cos(), cy + r * a.sin()));
        }
        Outline {
            contours: vec![line_contour(&pts)],
        }
    }

    fn triangle() -> Outline {
        Outline {
            contours: vec![line_contour(&[(10.4, 9.6), (37.6, 12.2), (22.5, 38.4)])],
        }
    }

    fn blob() -> Outline {
        let segments = vec![
            Segment::Cubic {
                from: Point2::new(10.3, 20.1),
                control1: Point2::new(10.3, 30.9),
                control2: Point2::new(18.5, 38.9),
                to: Point2::new(26.3, 38.9),
            },
            Segment::Quadratic {
                from: Point2::new(26.3, 38.9),
                control: Point2::new(36.1, 38.9),
                to: Point2::new(38.9, 24.5),
            },
            Segment::Cubic {
                from: Point2::new(38.9, 24.5),
                control1: Point2::new(41.5, 14.1),
                control2: Point2::new(30.9, 7.5),
                to: Point2::new(20.1, 7.5),
            },
            Segment::Quadratic {
                from: Point2::new(20.1, 7.5),
                control: Point2::new(12.3, 7.5),
                to: Point2::new(10.3, 20.1),
            },
        ];
        Outline {
            contours: vec![Contour { segments }],
        }
    }

    fn prepared(outline: &Outline) -> Vec<PreparedEdge> {
        outline
            .contours
            .iter()
            .flat_map(|c| c.segments.iter())
            .filter(|s| !s.is_degenerate())
            .map(|s| PreparedEdge::new(*s))
            .collect()
    }

    // -- scanline rasterizer (reference for the IoU tests) ------------------

    fn flatten(outline: &Outline, subs: usize) -> Vec<(f64, f64, f64, f64)> {
        let mut edges = Vec::new();
        for contour in &outline.contours {
            for seg in &contour.segments {
                let steps = match seg {
                    Segment::Line { .. } => 1,
                    _ => subs,
                };
                let mut prev = seg.point_at(0.0);
                for i in 1..=steps {
                    let t = i as f64 / steps as f64;
                    let pt = seg.point_at(t);
                    edges.push((prev.x, prev.y, pt.x, pt.y));
                    prev = pt;
                }
            }
        }
        edges
    }

    /// Analytic-ish scanline coverage: for each of `sub` scanlines per texel row, the exact
    /// x-crossings of the (flattened) outline integrate the inside width per texel.
    fn raster_coverage(outline: &Outline, w: usize, h: usize, sub: usize) -> Vec<f64> {
        let edges = flatten(outline, 64);
        let mut cov = vec![0.0f64; w * h];
        for row in 0..h {
            for s in 0..sub {
                let y = row as f64 + (s as f64 + 0.5) / sub as f64;
                let mut xs: Vec<f64> = Vec::new();
                for &(x0, y0, x1, y1) in &edges {
                    if (y0 <= y && y < y1) || (y1 <= y && y < y0) {
                        let t = (y - y0) / (y1 - y0);
                        xs.push(x0 + t * (x1 - x0));
                    }
                }
                xs.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                for pair in xs.chunks(2) {
                    if pair.len() == 2 {
                        let (xa, xb) = (pair[0], pair[1]);
                        let c0 = xa.floor().max(0.0) as usize;
                        let c1 = xb.ceil().min(w as f64) as usize;
                        for col in c0..c1 {
                            let l = (col as f64).max(xa);
                            let r = ((col + 1) as f64).min(xb);
                            if r > l {
                                cov[row * w + col] += (r - l) / sub as f64;
                            }
                        }
                    }
                }
            }
        }
        cov
    }

    fn iou(msdf: &MsdfBitmap, raster: &[f64], w: usize, h: usize) -> f64 {
        let mut inter = 0.0f64;
        let mut uni = 0.0f64;
        for y in 0..h {
            for x in 0..w {
                let a = msdf.coverage(x as f64 + 0.5, y as f64 + 0.5) > 0.5;
                let b = raster[y * w + x] > 0.5;
                if a || b {
                    uni += 1.0;
                }
                if a && b {
                    inter += 1.0;
                }
            }
        }
        if uni == 0.0 { 1.0 } else { inter / uni }
    }

    fn iou_of(outline: &Outline) -> f64 {
        let (w, h) = (48usize, 48usize);
        let bmp = generate(outline, [48, 48], 4.0, [1.0, 1.0], [0.0, 0.0])
            .unwrap_or_else(|e| panic!("generate failed: {e}"));
        let mapped = map_outline(outline, h as f64, [1.0, 1.0], [0.0, 0.0]);
        let raster = raster_coverage(&mapped, w, h, 16);
        iou(&bmp, &raster, w, h)
    }

    fn poly_mul(a: &[f64], b: &[f64]) -> Vec<f64> {
        let mut out = vec![0.0; a.len() + b.len() - 1];
        for (i, &x) in a.iter().enumerate() {
            for (j, &y) in b.iter().enumerate() {
                out[i + j] += x * y;
            }
        }
        out
    }

    // -- 1. median -----------------------------------------------------------

    #[test]
    fn median3_orders_values() {
        assert_eq!(median3(1.0, 2.0, 3.0), 2.0);
        assert_eq!(median3(3.0, 1.0, 2.0), 2.0);
        assert_eq!(median3(2.0, 3.0, 1.0), 2.0);
        assert_eq!(median3(5.0, 5.0, 1.0), 5.0);
        assert_eq!(median3(-1.0, -2.0, -3.0), -2.0);
        assert_eq!(median3_f64(0.25, 0.75, 0.5), 0.5);
        // With two equal values the median is that value regardless of the third.
        assert_eq!(median3(0.5, 0.5, 1.0e30), 0.5);
        assert_eq!(median3(0.5, 0.5, -1.0e30), 0.5);
    }

    #[test]
    fn median3_with_nan_returns_a_defined_value() {
        // Every NaN position yields one of the two finite inputs.
        assert_eq!(median3(f32::NAN, 1.0, 2.0), 2.0);
        assert_eq!(median3(1.0, f32::NAN, 2.0), 1.0);
        assert_eq!(median3(1.0, 2.0, f32::NAN), 1.0);
    }

    // -- 2. root finder ------------------------------------------------------

    #[test]
    fn root_finder_matches_known_roots() {
        // (t − 0.2)(t − 0.5)(t − 0.8)
        let cubic = poly_mul(&poly_mul(&[-0.2, 1.0], &[-0.5, 1.0]), &[-0.8, 1.0]);
        let roots = real_roots01(&cubic);
        assert_eq!(roots.len(), 3);
        for (got, want) in roots.iter().zip([0.2, 0.5, 0.8]) {
            assert!((got - want).abs() < 1e-9, "root {got} vs {want}");
        }
        // A quintic with three real roots and an irreducible quadratic factor.
        let lin = poly_mul(&poly_mul(&[-0.15, 1.0], &[-0.45, 1.0]), &[-0.7, 1.0]);
        let quintic = poly_mul(&lin, &[2.0, 0.0, 1.0]);
        let roots = real_roots01(&quintic);
        assert_eq!(roots.len(), 3);
        for (got, want) in roots.iter().zip([0.15, 0.45, 0.7]) {
            assert!((got - want).abs() < 1e-9);
        }
        // Roots outside [0, 1] are excluded.
        let shifted = poly_mul(&[-1.5, 1.0], &[-2.5, 1.0]);
        assert!(real_roots01(&shifted).is_empty());
    }

    #[test]
    fn root_finder_handles_repeated_roots_and_degenerate_polynomials() {
        // (t − 0.3)² (t − 0.7): the simple root is found; the double root may or may not be.
        let p = poly_mul(&poly_mul(&[-0.3, 1.0], &[-0.3, 1.0]), &[-0.7, 1.0]);
        let roots = real_roots01(&p);
        assert!(roots.iter().any(|r| (r - 0.7).abs() < 1e-6));
        for r in &roots {
            assert!((r - 0.3).abs() < 1e-6 || (r - 0.7).abs() < 1e-6);
        }
        // Zero polynomial, constants, and leading zeros give no roots or the right roots.
        assert!(real_roots01(&[0.0, 0.0, 0.0]).is_empty());
        assert!(real_roots01(&[5.0]).is_empty());
        assert_eq!(real_roots01(&[0.0, 2.0]), vec![0.0]);
        // t(4t − 1): roots 0 and 0.25, both in range.
        assert_eq!(real_roots01(&[0.0, -1.0, 4.0]), vec![0.0, 0.25]);
        assert!(real_roots01(&[1.0, 0.0, 4.0]).is_empty());
    }

    #[test]
    fn segment_min_distance_matches_brute_force() {
        // Cubic and quadratic: root-based minimum vs a 100k-sample scan.
        let cases: Vec<(Segment, Point2)> = vec![
            (
                Segment::Cubic {
                    from: Point2::new(1.0, 2.0),
                    control1: Point2::new(9.0, 3.0),
                    control2: Point2::new(4.0, 8.0),
                    to: Point2::new(7.0, 6.0),
                },
                Point2::new(3.5, 4.25),
            ),
            (
                Segment::Cubic {
                    from: Point2::new(0.0, 0.0),
                    control1: Point2::new(10.0, 0.0),
                    control2: Point2::new(-2.0, 6.0),
                    to: Point2::new(5.0, 5.0),
                },
                Point2::new(4.0, 1.0),
            ),
            (
                Segment::Quadratic {
                    from: Point2::new(0.0, 0.0),
                    control: Point2::new(6.0, 9.0),
                    to: Point2::new(10.0, 0.0),
                },
                Point2::new(5.0, 2.5),
            ),
        ];
        for (seg, p) in cases {
            let exact = segment_min_dist_sq(&seg, p).sqrt();
            let mut best = f64::INFINITY;
            let n = 100_000;
            for i in 0..=n {
                let d = seg.point_at(f64::from(i) / f64::from(n)).minus(p).len_sq().sqrt();
                best = best.min(d);
            }
            assert!(
                exact <= best + 1e-6,
                "exact {exact} should not exceed scan {best}"
            );
            assert!((exact - best).abs() < 1e-6, "exact {exact} vs scan {best}");
        }
    }

    // -- 3. winding ----------------------------------------------------------

    #[test]
    fn winding_number_classifies_inside_outside_and_holes() {
        let sq = prepared(&square(0.0, 0.0, 10.0));
        assert_ne!(winding_number(&sq, Point2::new(5.0, 5.0)), 0);
        assert_eq!(winding_number(&sq, Point2::new(15.0, 5.0)), 0);
        assert_eq!(winding_number(&sq, Point2::new(-3.0, -3.0)), 0);
        // The ring: inside the ring body is nonzero, inside the hole is zero.
        let o = prepared(&ring(24.0, 24.0, 16.0, 7.0));
        assert_ne!(winding_number(&o, Point2::new(24.0, 24.0 + 11.0)), 0);
        assert_eq!(winding_number(&o, Point2::new(24.0, 24.0)), 0);
    }

    #[test]
    fn winding_number_sign_flips_with_orientation() {
        let sq = prepared(&square(0.0, 0.0, 10.0));
        let inside = Point2::new(5.0, 5.0);
        let a = winding_number(&sq, inside);
        // Reverse every edge: same magnitude, opposite sign.
        let reversed: Vec<PreparedEdge> = sq
            .iter()
            .map(|e| {
                let (from, to) = e.seg.endpoints();
                let mut r = *e;
                r.seg = Segment::Line { from: to, to: from };
                r
            })
            .collect();
        let b = winding_number(&reversed, inside);
        assert_ne!(a, 0);
        assert_eq!(a, -b);
    }

    // -- 4. colouring --------------------------------------------------------

    #[test]
    fn coloring_switches_at_every_sharp_corner() {
        for outline in [square(8.2, 8.2, 15.6), star(24.0, 24.0, 17.0, 7.5)] {
            let colors = color_outline(&outline, DEFAULT_TURN_THRESHOLD_DEG);
            for cols in &colors {
                let n = cols.len();
                assert!(n >= 3);
                // Every vertex of a polygon is a sharp corner (turn ≥ 36° for the star,
                // 90° for the square), so adjacent edges must differ — including the wrap.
                for k in 0..n {
                    let next = (k + 1) % n;
                    assert_ne!(
                        cols[k], cols[next],
                        "edges {k} and {next} share a channel pair at a sharp corner"
                    );
                }
            }
        }
    }

    #[test]
    fn coloring_keeps_smooth_junctions_single_color() {
        let c = circle(24.0, 24.0, 14.0);
        let outline = Outline { contours: vec![c] };
        let colors = color_outline(&outline, DEFAULT_TURN_THRESHOLD_DEG);
        assert_eq!(colors.len(), 1);
        assert!(colors[0].iter().all(|&c| c == colors[0][0]));
    }

    #[test]
    fn coloring_starts_consecutive_contours_on_different_colors() {
        let colors = color_outline(&ring(24.0, 24.0, 16.0, 7.0), DEFAULT_TURN_THRESHOLD_DEG);
        assert_eq!(colors.len(), 2);
        assert_ne!(colors[0][0], colors[1][0]);
    }

    // -- 5. the headline properties ------------------------------------------

    #[test]
    fn square_corner_reconstruction_error_under_half_pixel() {
        // 32×32 texels, range 4; the square's corner sits at (8.2, 8.2) in image space
        // (font square 8.2..23.8 with the y flip is symmetric in 32 texels).
        let outline = square(8.2, 8.2, 15.6);
        let bmp = generate(&outline, [32, 32], 4.0, [1.0, 1.0], [0.0, 0.0])
            .unwrap_or_else(|e| panic!("generate failed: {e}"));
        // Sanity: inside is covered, far outside is not.
        assert!(bmp.coverage(16.0, 16.0) > 0.9);
        assert!(bmp.coverage(4.5, 4.5) < 0.1);
        // Walk the diagonal through the corner (8.2, 8.2) and bisect the 0.5 crossing.
        let cov = |t: f64| bmp.coverage(16.0 - 12.0 * t, 16.0 - 12.0 * t);
        let (mut lo, mut hi) = (0.0f64, 1.0f64);
        assert!(cov(lo) > 0.5 && cov(hi) < 0.5);
        for _ in 0..64 {
            let mid = 0.5 * (lo + hi);
            if cov(mid) > 0.5 {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let u_cross = 16.0 - 12.0 * 0.5 * (lo + hi);
        let err = (u_cross - 8.2).abs() * 2.0f64.sqrt();
        assert!(err < 0.5, "corner error {err} px (crossing at {u_cross})");
    }

    #[test]
    fn circle_distance_matches_analytic_everywhere() {
        let r = 14.0f64;
        let outline = Outline {
            contours: vec![circle(24.0, 24.0, r)],
        };
        let bmp = generate(&outline, [48, 48], 4.0, [1.0, 1.0], [0.0, 0.0])
            .unwrap_or_else(|e| panic!("generate failed: {e}"));
        for y in 0..48u32 {
            for x in 0..48u32 {
                let t = bmp.texel(x, y).unwrap_or([0.0; 3]);
                let m = f64::from(median3(t[0], t[1], t[2]));
                let d = (m - 0.5) * 2.0 * 4.0;
                // Image space has y down: the font center (24, 24) maps to (24, 48 − 24),
                // which is (24, 24) again for a 48-texel bitmap.
                let px = f64::from(x) + 0.5;
                let py = f64::from(y) + 0.5;
                // The field is positive inside, so the analytic reference is r − |p − c|.
                let analytic = r - ((px - 24.0) * (px - 24.0) + (py - 24.0) * (py - 24.0)).sqrt();
                assert!(
                    (d - analytic).abs() <= 1e-2,
                    "texel ({x},{y}): msdf {d} vs analytic {analytic}"
                );
            }
        }
    }

    #[test]
    fn circle_dead_channel_saturates_in_u8() {
        let outline = Outline {
            contours: vec![circle(24.0, 24.0, 14.0)],
        };
        let bmp = generate(&outline, [48, 48], 4.0, [1.0, 1.0], [0.0, 0.0])
            .unwrap_or_else(|e| panic!("generate failed: {e}"));
        let bytes = bmp.to_u8();
        // Contour 0 starts Yellow (R+G), so blue is the dead channel: +DEAD inside (255),
        // −DEAD outside (0) — the sign follows the winding like every other channel.
        let center = (24usize * 48 + 24) * 3 + 2;
        let corner = 2;
        assert_eq!(bytes[center], 255);
        assert_eq!(bytes[corner], 0);
    }

    #[test]
    fn o_hole_has_outside_sign_and_ring_is_inside() {
        let outline = ring(24.0, 24.0, 16.0, 7.0);
        let bmp = generate(&outline, [48, 48], 4.0, [1.0, 1.0], [0.0, 0.0])
            .unwrap_or_else(|e| panic!("generate failed: {e}"));
        // Hole center: median below 1/2 → strongly outside.
        let hole = bmp.texel(24, 24).unwrap_or([0.0; 3]);
        assert!(f64::from(median3(hole[0], hole[1], hole[2])) < 0.25);
        assert!(bmp.coverage(24.5, 24.5) < 0.25);
        // Ring body (radius 11.5 from the center, image y = 48 − 35.5 = 12.5).
        assert!(bmp.coverage(24.5, 12.5) > 0.75);
    }

    #[test]
    fn pseudo_distance_extends_edge_beyond_its_endpoint() {
        // One open line (4,0)→(14,0); the texel center at image (0.5, 14.5) is font
        // (0.5, 1.5): past the start endpoint by 3.5 along the edge and 1.5 off it.
        let outline = Outline {
            contours: vec![Contour {
                segments: vec![Segment::Line {
                    from: Point2::new(4.0, 0.0),
                    to: Point2::new(14.0, 0.0),
                }],
            }],
        };
        let bmp = generate(&outline, [16, 16], 4.0, [1.0, 1.0], [0.0, 0.0])
            .unwrap_or_else(|e| panic!("generate failed: {e}"));
        let t = bmp.texel(0, 14).unwrap_or([0.0; 3]);
        let m = f64::from(median3(t[0], t[1], t[2]));
        // Outside (winding 0) so d = −1.5 (the pseudo perpendicular distance), not the
        // Euclidean √(3.5² + 1.5²) ≈ 3.81.
        let expect = 0.5 - 0.5 * 1.5 / 4.0;
        assert!((m - expect).abs() < 1e-3, "median {m} vs expected {expect}");
        // Over the segment itself there is no extension: plain perpendicular distance.
        let t = bmp.texel(8, 15).unwrap_or([0.0; 3]);
        let m = f64::from(median3(t[0], t[1], t[2]));
        let expect = 0.5 - 0.5 * 0.5 / 4.0;
        assert!((m - expect).abs() < 1e-3, "median {m} vs expected {expect}");
    }

    // -- 6. IoU vs the scanline rasterizer ------------------------------------

    #[test]
    fn iou_square_above_098() {
        assert!(iou_of(&square(9.3, 9.3, 29.4)) > 0.98);
    }

    #[test]
    fn iou_circle_above_098() {
        let outline = Outline {
            contours: vec![circle(24.0, 24.0, 14.0)],
        };
        assert!(iou_of(&outline) > 0.98);
    }

    #[test]
    fn iou_triangle_above_098() {
        assert!(iou_of(&triangle()) > 0.98);
    }

    #[test]
    fn iou_ring_o_above_098() {
        assert!(iou_of(&ring(24.0, 24.0, 16.0, 7.0)) > 0.98);
    }

    #[test]
    fn iou_star_above_098() {
        assert!(iou_of(&star(24.0, 24.0, 17.0, 7.5)) > 0.98);
    }

    #[test]
    fn iou_mixed_segment_blob_above_098() {
        assert!(iou_of(&blob()) > 0.98);
    }

    // -- 7. degenerate and adversarial input ----------------------------------

    #[test]
    fn degenerate_zero_length_edges_do_not_panic() {
        // A square with every vertex duplicated by a zero-length line, an all-equal
        // quadratic and cubic, and a micro-edge.
        let mut contour = line_contour(&[(8.0, 8.0), (40.0, 8.0), (40.0, 40.0), (8.0, 40.0)]);
        contour.segments.push(Segment::Line {
            from: Point2::new(8.0, 40.0),
            to: Point2::new(8.0, 40.0),
        });
        contour.segments.push(Segment::Quadratic {
            from: Point2::new(8.0, 40.0),
            control: Point2::new(8.0, 40.0),
            to: Point2::new(8.0, 40.0),
        });
        contour.segments.push(Segment::Line {
            from: Point2::new(8.0, 40.0),
            to: Point2::new(8.0, 40.0 + 1e-9),
        });
        contour.segments.push(Segment::Cubic {
            from: Point2::new(8.0, 40.0 + 1e-9),
            control1: Point2::new(8.0, 40.0 + 1e-9),
            control2: Point2::new(8.0, 40.0 + 1e-9),
            to: Point2::new(8.0, 8.0),
        });
        let outline = Outline {
            contours: vec![contour],
        };
        let bmp = generate(&outline, [48, 48], 4.0, [1.0, 1.0], [0.0, 0.0]);
        assert!(bmp.is_ok());
        let bmp = bmp.unwrap_or_else(|e| panic!("generate failed: {e}"));
        assert!(bmp.coverage(24.0, 24.0) > 0.9);
        assert!(bmp.coverage(2.5, 2.5) < 0.1);
    }

    #[test]
    fn empty_outline_gives_all_outside_coverage() {
        let empty = Outline::default();
        let bmp = generate(&empty, [8, 8], 4.0, [1.0, 1.0], [0.0, 0.0])
            .unwrap_or_else(|e| panic!("generate failed: {e}"));
        for y in 0..8 {
            for x in 0..8 {
                assert_eq!(bmp.coverage(f64::from(x) + 0.5, f64::from(y) + 0.5), 0.0);
            }
        }
        // A contour that is a single point is equally empty.
        let dot = Outline {
            contours: vec![Contour {
                segments: vec![Segment::Line {
                    from: Point2::new(4.0, 4.0),
                    to: Point2::new(4.0, 4.0),
                }],
            }],
        };
        let bmp = generate(&dot, [8, 8], 4.0, [1.0, 1.0], [0.0, 0.0]);
        assert!(bmp.is_ok());
    }

    #[test]
    fn nan_and_nonpositive_inputs_return_errors() {
        let good = square(2.0, 2.0, 10.0);
        assert!(generate(&good, [16, 16], 4.0, [1.0, 1.0], [0.0, 0.0]).is_ok());
        // NaN / infinite points.
        for bad_pt in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut outline = square(2.0, 2.0, 10.0);
            outline.contours[0].segments[0] = Segment::Line {
                from: Point2::new(bad_pt, 2.0),
                to: Point2::new(12.0, 2.0),
            };
            assert!(matches!(
                generate(&outline, [16, 16], 4.0, [1.0, 1.0], [0.0, 0.0]),
                Err(MsdfError::NotFinite { .. })
            ));
        }
        // NaN / negative range, zero scale, NaN translate, empty and oversized sizes.
        assert!(matches!(
            generate(&good, [16, 16], f64::NAN, [1.0, 1.0], [0.0, 0.0]),
            Err(MsdfError::NotFinite { .. })
        ));
        assert!(matches!(
            generate(&good, [16, 16], -4.0, [1.0, 1.0], [0.0, 0.0]),
            Err(MsdfError::Invalid { .. })
        ));
        assert!(matches!(
            generate(&good, [16, 16], 4.0, [0.0, 1.0], [0.0, 0.0]),
            Err(MsdfError::Invalid { .. })
        ));
        assert!(matches!(
            generate(&good, [16, 16], 4.0, [1.0, 1.0], [f64::NAN, 0.0]),
            Err(MsdfError::NotFinite { .. })
        ));
        assert!(matches!(
            generate(&good, [0, 16], 4.0, [1.0, 1.0], [0.0, 0.0]),
            Err(MsdfError::BadSize { .. })
        ));
        assert!(matches!(
            generate(&good, [8192, 8192], 4.0, [1.0, 1.0], [0.0, 0.0]),
            Err(MsdfError::BadSize { .. })
        ));
    }

    // -- 8. determinism --------------------------------------------------------

    #[test]
    fn generation_is_byte_deterministic() {
        let outline = star(24.0, 24.0, 17.0, 7.5);
        let a = generate(&outline, [48, 48], 4.0, [1.0, 1.0], [0.0, 0.0])
            .unwrap_or_else(|e| panic!("generate failed: {e}"));
        let b = generate(&outline, [48, 48], 4.0, [1.0, 1.0], [0.0, 0.0])
            .unwrap_or_else(|e| panic!("generate failed: {e}"));
        assert_eq!(a.data_f32(), b.data_f32());
        assert_eq!(a.to_u8(), b.to_u8());
        assert_eq!(a, b);
    }

    // -- 9. bitmap plumbing -----------------------------------------------------

    #[test]
    fn u8_quantization_rounds_correctly() {
        let bmp = MsdfBitmap::from_texels(1, 1, vec![0.0, 0.5, 1.0])
            .unwrap_or_else(|| panic!("from_texels failed"));
        assert_eq!(bmp.to_u8(), vec![0, 128, 255]);
        // Values beyond [0, 1] saturate.
        let bmp = MsdfBitmap::from_texels(1, 1, vec![-3.0, 1.5, 0.98])
            .unwrap_or_else(|| panic!("from_texels failed"));
        assert_eq!(bmp.to_u8(), vec![0, 255, 250]);
        // Mismatched length is refused.
        assert!(MsdfBitmap::from_texels(2, 2, vec![0.0; 11]).is_none());
    }

    #[test]
    fn coverage_is_half_at_an_on_edge_texel() {
        let bmp =
            MsdfBitmap::from_texels(4, 4, vec![0.5; 48]).unwrap_or_else(|| panic!("from_texels failed"));
        let c = bmp.coverage(2.5, 2.5);
        assert!((c - 0.5).abs() < 1e-9, "coverage {c}");
        // Bilinear between texels 0.2 and 0.8 hits 0.5 → smoothstep(0.5) = 0.5.
        let bmp = MsdfBitmap::from_texels(2, 1, vec![0.2, 0.2, 0.2, 0.8, 0.8, 0.8])
            .unwrap_or_else(|| panic!("from_texels failed"));
        let c = bmp.coverage(1.0, 0.5);
        // The texels are f32: 0.2f32 and 0.8f32 average to 0.5000000075, which the smoothstep
        // (slope 1.5 at 0.5) turns into 0.5000000112, so the tolerance is f32-sized.
        assert!((c - 0.5).abs() < 1e-6, "coverage {c}");
    }

    #[test]
    fn texel_access_and_out_of_range() {
        let bmp =
            MsdfBitmap::from_texels(3, 2, vec![0.25; 18]).unwrap_or_else(|| panic!("from_texels failed"));
        assert_eq!(bmp.texel(2, 1), Some([0.25; 3]));
        assert_eq!(bmp.texel(3, 1), None);
        assert_eq!(bmp.texel(0, 2), None);
        let mut bmp = bmp;
        assert!(bmp.set_texel(3, 1, [0.0; 3]).is_none());
        assert!(bmp.set_texel(0, 0, [1.0; 3]).is_some());
        assert_eq!(bmp.texel(0, 0), Some([1.0; 3]));
    }

    // -- 10. error correction ---------------------------------------------------

    #[test]
    fn error_correction_fixes_a_fabricated_clash() {
        // a = (0.9, 0.2, 0.4), b = (0.95, 0.1, 0.05): medians 0.4 → 0.1 (moving down);
        // channel 0 moves up (da = +0.05 < |dm|) against the median, and b[0] disagrees
        // with its own median more, so b[0] becomes the mean of b's other channels.
        let mut bmp = MsdfBitmap::from_texels(2, 1, vec![0.9, 0.2, 0.4, 0.95, 0.1, 0.05])
            .unwrap_or_else(|| panic!("from_texels failed"));
        bmp.correct_errors();
        let t = bmp.texel(1, 0).unwrap_or([0.0; 3]);
        assert!((t[0] - 0.075).abs() < 1e-6, "b[0] = {}", t[0]);
        assert!((t[1] - 0.1).abs() < 1e-6);
        assert!((t[2] - 0.05).abs() < 1e-6);
        let t = bmp.texel(0, 0).unwrap_or([0.0; 3]);
        assert_eq!(t, [0.9, 0.2, 0.4]);
    }

    #[test]
    fn error_correction_leaves_a_smooth_circle_untouched() {
        let outline = Outline {
            contours: vec![circle(24.0, 24.0, 14.0)],
        };
        let mut bmp = generate_uncorrected(&outline, [48, 48], 4.0, [1.0, 1.0], [0.0, 0.0])
            .unwrap_or_else(|e| panic!("generate failed: {e}"));
        let before = bmp.data_f32().to_vec();
        bmp.correct_errors();
        assert_eq!(bmp.data_f32(), before.as_slice());
    }

    // -- 11. scale/translate mapping ---------------------------------------------

    #[test]
    fn scale_and_translate_map_edges_to_expected_texels() {
        // A 20×20 square at font (10,5); scale 2, translate 0: the right edge (font x = 30)
        // maps to image x = 60, the font midline y = 15 to image y = 64 − 30 = 34.
        let outline = square(10.0, 5.0, 20.0);
        let bmp = generate(&outline, [64, 64], 4.0, [2.0, 2.0], [0.0, 0.0])
            .unwrap_or_else(|e| panic!("generate failed: {e}"));
        let crossing = |from: f64, to: f64| -> f64 {
            let cov = |t: f64| bmp.coverage(from + (to - from) * t, 34.0);
            let (mut lo, mut hi) = (0.0f64, 1.0f64);
            assert!(cov(lo) > 0.5 && cov(hi) < 0.5);
            for _ in 0..64 {
                let mid = 0.5 * (lo + hi);
                if cov(mid) > 0.5 {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            from + (to - from) * 0.5 * (lo + hi)
        };
        let right = crossing(55.0, 65.0);
        assert!((right - 60.0).abs() < 0.1, "right crossing at {right}");
        let left = crossing(25.0, 15.0);
        assert!((left - 20.0).abs() < 0.1, "left crossing at {left}");
    }

    // -- 12. glyf parsing ---------------------------------------------------------

    fn u16b(v: u16) -> Vec<u8> {
        vec![(v >> 8) as u8, (v & 0xff) as u8]
    }
    fn i16b(v: i16) -> Vec<u8> {
        u16b(v as u16)
    }
    fn u32b(v: u32) -> Vec<u8> {
        vec![(v >> 24) as u8, (v >> 16) as u8, (v >> 8) as u8, v as u8]
    }
    fn cat(parts: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for p in parts {
            out.extend_from_slice(p);
        }
        out
    }

    /// A minimal 4-glyph TrueType font: 0 empty, 1 a 400-unit square, 2 that square as a
    /// composite (scale 0.5, offset (100, 50)), 3 an off-curve contour (implied midpoints).
    fn build_test_font() -> Vec<u8> {
        let glyph1 = cat(&[
            &i16b(1),
            &i16b(0),
            &i16b(0),
            &i16b(400),
            &i16b(400),
            &u16b(3),
            &u16b(0),
            &[0x01, 0x01, 0x01, 0x01],
            // x deltas, then y deltas: (0,0) → (400,0) → (400,400) → (0,400) → close.
            &i16b(0),
            &i16b(400),
            &i16b(0),
            &i16b(-400),
            &i16b(0),
            &i16b(0),
            &i16b(400),
            &i16b(0),
        ]);
        let glyph2 = cat(&[
            &i16b(-1),
            &i16b(0),
            &i16b(0),
            &i16b(0),
            &i16b(0),
            // ARG_1_AND_2_ARE_WORDS | ARGS_ARE_XY_VALUES | WE_HAVE_A_SCALE
            &u16b(0x0001 | 0x0002 | 0x0008),
            &u16b(1),
            &i16b(100),
            &i16b(50),
            &i16b(8192), // F2Dot14 of 0.5
        ]);
        let glyph3 = cat(&[
            &i16b(1),
            &i16b(0),
            &i16b(0),
            &i16b(300),
            &i16b(300),
            &u16b(3),
            &u16b(0),
            &[0x01, 0x00, 0x00, 0x01],
            &i16b(0),
            &i16b(200),
            &i16b(100),
            &i16b(0),
            &i16b(0),
            &i16b(0),
            &i16b(300),
            &i16b(-300),
        ]);
        build_font(&[Vec::new(), glyph1, glyph2, glyph3])
    }

    /// A minimal TrueType font holding `glyphs` (each a `glyf` record; empty = no outline),
    /// with short `loca` offsets. Every record must have an even length.
    fn build_font(glyphs: &[Vec<u8>]) -> Vec<u8> {
        let mut head = vec![0u8; 54];
        head[0..4].copy_from_slice(&u32b(0x0001_0000));
        head[18..20].copy_from_slice(&u16b(1000));
        head[50..52].copy_from_slice(&u16b(0));
        let maxp = cat(&[&u32b(0x0001_0000), &u16b(glyphs.len() as u16)]);
        let mut glyf = Vec::new();
        let mut loca = u16b(0);
        for g in glyphs {
            glyf.extend_from_slice(g);
            loca.extend_from_slice(&u16b((glyf.len() / 2) as u16));
        }

        let tables: [(&[u8; 4], Vec<u8>); 4] =
            [(b"glyf", glyf), (b"head", head), (b"loca", loca), (b"maxp", maxp)];
        let mut offset = 12 + 4 * 16;
        let mut records = Vec::new();
        let mut body = Vec::new();
        for (tag, data) in &tables {
            records.extend_from_slice(&cat(&[
                &tag[..],
                &u32b(0),
                &u32b(offset as u32),
                &u32b(data.len() as u32),
            ]));
            body.extend_from_slice(data);
            offset += data.len();
        }
        let mut font = cat(&[&u32b(0x0001_0000), &u16b(4), &u16b(0), &u16b(0), &u16b(0)]);
        font.extend_from_slice(&records);
        font.extend_from_slice(&body);
        font
    }

    #[test]
    fn glyf_parses_a_simple_square_glyph() {
        let font = build_test_font();
        let g = parse_glyf_outline(&font, 1).unwrap_or_else(|e| panic!("parse failed: {e}"));
        assert_eq!(g.units_per_em, 1000);
        let segs = &g.outline.contours[0].segments;
        assert_eq!(segs.len(), 4);
        assert_eq!(
            segs[0],
            Segment::Line {
                from: Point2::new(0.0, 0.0),
                to: Point2::new(400.0, 0.0)
            }
        );
        assert_eq!(
            segs[1],
            Segment::Line {
                from: Point2::new(400.0, 0.0),
                to: Point2::new(400.0, 400.0)
            }
        );
        assert_eq!(
            segs[3],
            Segment::Line {
                from: Point2::new(0.0, 400.0),
                to: Point2::new(0.0, 0.0)
            }
        );
        // The parsed square renders: coverage at its center is 1.
        let bmp = generate(&g.outline, [40, 40], 4.0, [0.1, 0.1], [-20.0, -20.0])
            .unwrap_or_else(|e| panic!("generate failed: {e}"));
        assert!(bmp.coverage(20.0, 20.0) > 0.9);
    }

    #[test]
    fn glyf_implied_oncurve_midpoints_become_quadratics() {
        let font = build_test_font();
        let g = parse_glyf_outline(&font, 3).unwrap_or_else(|e| panic!("parse failed: {e}"));
        let segs = &g.outline.contours[0].segments;
        assert_eq!(segs.len(), 3);
        assert_eq!(
            segs[0],
            Segment::Quadratic {
                from: Point2::new(0.0, 0.0),
                control: Point2::new(200.0, 0.0),
                to: Point2::new(250.0, 150.0),
            }
        );
        assert_eq!(
            segs[1],
            Segment::Quadratic {
                from: Point2::new(250.0, 150.0),
                control: Point2::new(300.0, 300.0),
                to: Point2::new(300.0, 0.0),
            }
        );
        assert_eq!(
            segs[2],
            Segment::Line {
                from: Point2::new(300.0, 0.0),
                to: Point2::new(0.0, 0.0)
            }
        );
    }

    #[test]
    fn glyf_parses_a_composite_glyph_with_transform() {
        let font = build_test_font();
        let g = parse_glyf_outline(&font, 2).unwrap_or_else(|e| panic!("parse failed: {e}"));
        let segs = &g.outline.contours[0].segments;
        assert_eq!(segs.len(), 4);
        // Glyph 1 scaled by 0.5 and offset by (100, 50).
        assert_eq!(
            segs[0],
            Segment::Line {
                from: Point2::new(100.0, 50.0),
                to: Point2::new(300.0, 50.0)
            }
        );
        assert_eq!(
            segs[2],
            Segment::Line {
                from: Point2::new(300.0, 250.0),
                to: Point2::new(100.0, 250.0)
            }
        );
    }

    #[test]
    fn glyf_rejects_bad_input_without_panicking() {
        let font = build_test_font();
        assert!(matches!(
            parse_glyf_outline(&font, 4),
            Err(MsdfError::GlyphIndexOutOfRange { index: 4, count: 4 })
        ));
        // Empty glyph 0 parses to an empty outline.
        let g = parse_glyf_outline(&font, 0).unwrap_or_else(|e| panic!("parse failed: {e}"));
        assert!(g.outline.is_empty());
        // Truncated font bytes fail somewhere in the table walk.
        assert!(parse_glyf_outline(&font[..40], 1).is_err());
        // Wrong sfnt version.
        let mut otf = font.clone();
        otf[0..4].copy_from_slice(&u32b(0x4F54_544F)); // 'OTTO'
        assert!(matches!(
            parse_glyf_outline(&otf, 1),
            Err(MsdfError::UnsupportedFont { tag: 0x4F54_544F })
        ));
        // Random garbage.
        assert!(parse_glyf_outline(&[9u8; 16], 0).is_err());
        assert!(parse_glyf_outline(&[], 0).is_err());
    }

    // -- 13. verification-lane additions (sprint-zai-verify-msdf) ---------------------

    #[test]
    fn glyf_composite_with_xy_scale_and_two_by_two_reads_every_component() {
        // Two components of the square: the first with WE_HAVE_AN_X_AND_Y_SCALE and
        // MORE_COMPONENTS, the second with WE_HAVE_A_TWO_BY_TWO. Each transform must consume
        // all of its F2Dot14 fields or the second component's header is misread.
        let square = build_test_font();
        let g1 = parse_glyf_outline(&square, 1).unwrap_or_else(|e| panic!("parse failed: {e}"));
        assert_eq!(g1.outline.contours.len(), 1);
        let glyph1 = cat(&[
            &i16b(1),
            &i16b(0),
            &i16b(0),
            &i16b(400),
            &i16b(400),
            &u16b(3),
            &u16b(0),
            &[0x01, 0x01, 0x01, 0x01],
            &i16b(0),
            &i16b(400),
            &i16b(0),
            &i16b(-400),
            &i16b(0),
            &i16b(0),
            &i16b(400),
            &i16b(0),
        ]);
        let composite = cat(&[
            &i16b(-1),
            &i16b(0),
            &i16b(0),
            &i16b(0),
            &i16b(0),
            // ARG_1_AND_2_ARE_WORDS | ARGS_ARE_XY_VALUES | MORE_COMPONENTS | X_AND_Y_SCALE
            &u16b(0x0001 | 0x0002 | 0x0020 | 0x0040),
            &u16b(1),
            &i16b(0),
            &i16b(0),
            &i16b(8192), // x scale 0.5
            &i16b(4096), // y scale 0.25
            // ARG_1_AND_2_ARE_WORDS | ARGS_ARE_XY_VALUES | WE_HAVE_A_TWO_BY_TWO
            &u16b(0x0001 | 0x0002 | 0x0080),
            &u16b(1),
            &i16b(1000),
            &i16b(0),
            &i16b(8192),  // xx 0.5
            &i16b(4096),  // xy 0.25 (y gains 0.25·x)
            &i16b(0),     // yx 0
            &i16b(16384), // yy 1.0
        ]);
        let font = build_font(&[Vec::new(), glyph1, composite]);
        let g = parse_glyf_outline(&font, 2).unwrap_or_else(|e| panic!("parse failed: {e}"));
        assert_eq!(g.outline.contours.len(), 2);
        assert_eq!(
            g.outline.contours[0].segments[0],
            Segment::Line {
                from: Point2::new(0.0, 0.0),
                to: Point2::new(200.0, 0.0)
            }
        );
        assert_eq!(
            g.outline.contours[0].segments[1],
            Segment::Line {
                from: Point2::new(200.0, 0.0),
                to: Point2::new(200.0, 100.0)
            }
        );
        assert_eq!(
            g.outline.contours[1].segments[0],
            Segment::Line {
                from: Point2::new(1000.0, 0.0),
                to: Point2::new(1200.0, 100.0)
            }
        );
        assert_eq!(
            g.outline.contours[1].segments[1],
            Segment::Line {
                from: Point2::new(1200.0, 100.0),
                to: Point2::new(1200.0, 500.0)
            }
        );
    }

    #[test]
    fn glyf_composite_scales_the_offset_only_when_the_spec_says_so() {
        // Audit ui1 correctness 3: SCALED_COMPONENT_OFFSET (0x0800) set while
        // UNSCALED_COMPONENT_OFFSET (0x1000) is clear transforms the XY arguments by the
        // component 2×2; both bits or neither leave the offset unscaled (public glyf spec).
        let glyph1 = cat(&[
            &i16b(1),
            &i16b(0),
            &i16b(0),
            &i16b(400),
            &i16b(400),
            &u16b(3),
            &u16b(0),
            &[0x01, 0x01, 0x01, 0x01],
            &i16b(0),
            &i16b(400),
            &i16b(0),
            &i16b(-400),
            &i16b(0),
            &i16b(0),
            &i16b(400),
            &i16b(0),
        ]);
        // WE_HAVE_A_SCALE of 0.5 with offset (100, 0): the scaled offset puts the child
        // origin at x = 50, the unscaled one at x = 100.
        let composite = |offset_bits: u16| {
            cat(&[
                &i16b(-1),
                &i16b(0),
                &i16b(0),
                &i16b(0),
                &i16b(0),
                // ARG_1_AND_2_ARE_WORDS | ARGS_ARE_XY_VALUES | WE_HAVE_A_SCALE [| offset bits]
                &u16b(0x0001 | 0x0002 | 0x0008 | offset_bits),
                &u16b(1),
                &i16b(100),
                &i16b(0),
                &i16b(8192), // uniform scale 0.5
            ])
        };
        let first_from = |offset_bits: u16| {
            let font = build_font(&[Vec::new(), glyph1.clone(), composite(offset_bits)]);
            let g = parse_glyf_outline(&font, 2).unwrap_or_else(|e| panic!("parse failed: {e}"));
            match g.outline.contours[0].segments[0] {
                Segment::Line { from, .. } => from,
                other => panic!("expected a line, got {other:?}"),
            }
        };
        assert_eq!(
            first_from(0x0800),
            Point2::new(50.0, 0.0),
            "SCALED_COMPONENT_OFFSET scales the offset by the 2×2"
        );
        assert_eq!(
            first_from(0x0000),
            Point2::new(100.0, 0.0),
            "neither bit leaves the offset unscaled"
        );
        assert_eq!(
            first_from(0x1000),
            Point2::new(100.0, 0.0),
            "UNSCALED_COMPONENT_OFFSET leaves the offset unscaled"
        );
        assert_eq!(
            first_from(0x1800),
            Point2::new(100.0, 0.0),
            "both bits leave the offset unscaled"
        );
    }

    #[test]
    fn winding_is_exact_when_the_ray_passes_through_a_vertex() {
        // Rays through a vertex: a pass-through vertex (the square's corner row), a local
        // y-extremum on the far side (a diamond's apex), both orientations.
        let diamond = Outline {
            contours: vec![line_contour(&[
                (10.0, 0.0),
                (20.0, 10.0),
                (10.0, 20.0),
                (0.0, 10.0),
            ])],
        };
        let mut reversed = diamond.clone();
        for contour in &mut reversed.contours {
            contour.segments.reverse();
            for seg in &mut contour.segments {
                let (from, to) = seg.endpoints();
                *seg = Segment::Line { from: to, to: from };
            }
        }
        for outline in [&diamond, &reversed] {
            let edges = prepared(outline);
            // Outside, left of the apex row y = 20 and the bottom vertex row y = 0.
            assert_eq!(winding_number(&edges, Point2::new(5.0, 20.0)), 0);
            assert_eq!(winding_number(&edges, Point2::new(-5.0, 0.0)), 0);
            // Inside, on the row of the side vertices (y = 10, which pass through).
            assert_ne!(winding_number(&edges, Point2::new(10.0, 10.0)), 0);
            assert_eq!(winding_number(&edges, Point2::new(-3.0, 10.0)), 0);
            assert_eq!(winding_number(&edges, Point2::new(25.0, 10.0)), 0);
        }
        // The star's inner vertex at 90° lies exactly on texel row 16.5 of a 48-texel bitmap;
        // the point left of it is inside the star in both orientations.
        let star_ccw = map_outline(&star(24.0, 24.0, 17.0, 7.5), 48.0, [1.0, 1.0], [0.0, 0.0]);
        let mut star_cw = star_ccw.clone();
        for contour in &mut star_cw.contours {
            contour.segments.reverse();
            for seg in &mut contour.segments {
                let (from, to) = seg.endpoints();
                *seg = Segment::Line { from: to, to: from };
            }
        }
        let p = Point2::new(16.5, 16.5);
        let a = winding_number(&prepared(&star_ccw), p);
        let b = winding_number(&prepared(&star_cw), p);
        assert_eq!(a.abs(), 1, "ccw winding {a}");
        assert_eq!(a, -b);
    }

    #[test]
    fn shape_aware_correction_repairs_an_injected_clash_and_nothing_else() {
        let outline = square(4.0, 4.0, 40.0);
        let (mut bmp, edges) = generate_field(&outline, [48, 48], 4.0, [1.0, 1.0], [0.0, 0.0])
            .unwrap_or_else(|e| panic!("generate failed: {e}"));
        let mut clean = bmp.clone();
        assert_eq!(
            correct_clashes(&mut clean, &edges, 4.0),
            0,
            "a square has no clash"
        );
        assert_eq!(clean, bmp);
        // Two texels deep inside whose medians are both inside (0.6) but whose midpoint
        // average (0.3, 0.6, 0.3) has an outside median: a fabricated channel clash.
        assert!(bmp.set_texel(20, 24, [0.6, 0.6, 0.0]).is_some());
        assert!(bmp.set_texel(21, 24, [0.0, 0.6, 0.6]).is_some());
        assert!(
            bmp.coverage(21.0, 24.5) < 0.5,
            "the clash shows between the texels"
        );
        let changed = correct_clashes(&mut bmp, &edges, 4.0);
        assert!(changed >= 2, "changed {changed}");
        assert_eq!(bmp.texel(20, 24), Some([0.6; 3]));
        assert_eq!(bmp.texel(21, 24), Some([0.6; 3]));
        assert!(bmp.coverage(21.0, 24.5) > 0.5, "repaired");
        // Texels far from the clash keep their channels.
        assert_eq!(bmp.texel(5, 5), clean.texel(5, 5));
    }

    #[test]
    fn oppositely_wound_neighbours_get_the_same_field() {
        // Two squares one texel apart; in the second outline the right square is wound the
        // other way (as a mirrored composite component would be). Under the nonzero rule both
        // are filled either way, and each contour's own inside side must be used: one
        // glyph-wide orientation (their areas cancel) would give the right square inverted
        // channels where they mix with the left square's edges.
        let left = line_contour(&[(4.0, 4.0), (22.0, 4.0), (22.0, 22.0), (4.0, 22.0)]);
        let right = line_contour(&[(23.0, 4.0), (41.0, 4.0), (41.0, 22.0), (23.0, 22.0)]);
        let right_cw = line_contour(&[(23.0, 4.0), (23.0, 22.0), (41.0, 22.0), (41.0, 4.0)]);
        let same = Outline {
            contours: vec![left.clone(), right],
        };
        let mixed = Outline {
            contours: vec![left, right_cw],
        };
        let a = generate(&same, [48, 48], 4.0, [1.0, 1.0], [0.0, 0.0])
            .unwrap_or_else(|e| panic!("generate failed: {e}"));
        let b = generate(&mixed, [48, 48], 4.0, [1.0, 1.0], [0.0, 0.0])
            .unwrap_or_else(|e| panic!("generate failed: {e}"));
        for y in 0..192u32 {
            for x in 0..192u32 {
                let (u, v) = ((f64::from(x) + 0.5) / 4.0, (f64::from(y) + 0.5) / 4.0);
                let (ca, cb) = (a.coverage(u, v), b.coverage(u, v));
                // The two windings colour the right square differently, which moves coverage
                // near its corners by a few thousandths; an inverted contour moves it by far more.
                assert!((ca - cb).abs() < 0.05, "({u}, {v}): {ca} vs {cb}");
            }
        }
    }

    #[test]
    fn lines_stored_as_cubics_keep_their_corners() {
        // A square whose sides are cubics with their controls on their endpoints: the tangent
        // vanishes at every corner, so corner detection must use the control polygon.
        let pts = [(8.2, 8.2), (23.8, 8.2), (23.8, 23.8), (8.2, 23.8)];
        let mut segments = Vec::new();
        for k in 0..4 {
            let (a, b) = (pts[k], pts[(k + 1) % 4]);
            segments.push(Segment::Cubic {
                from: Point2::new(a.0, a.1),
                control1: Point2::new(a.0, a.1),
                control2: Point2::new(b.0, b.1),
                to: Point2::new(b.0, b.1),
            });
        }
        let outline = Outline {
            contours: vec![Contour { segments }],
        };
        let colors = color_outline(&outline, DEFAULT_TURN_THRESHOLD_DEG);
        for k in 0..4 {
            assert_ne!(colors[0][k], colors[0][(k + 1) % 4], "corner {k}");
        }
        let bmp = generate(&outline, [32, 32], 4.0, [1.0, 1.0], [0.0, 0.0])
            .unwrap_or_else(|e| panic!("generate failed: {e}"));
        let cov = |t: f64| bmp.coverage(16.0 - 12.0 * t, 16.0 - 12.0 * t);
        let (mut lo, mut hi) = (0.0f64, 1.0f64);
        for _ in 0..64 {
            let mid = 0.5 * (lo + hi);
            if cov(mid) > 0.5 {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let err = (16.0 - 12.0 * 0.5 * (lo + hi) - 8.2).abs() * 2.0f64.sqrt();
        assert!(err < 0.5, "corner error {err} px");
    }

    #[test]
    fn corners_across_a_zero_length_segment_are_still_corners() {
        // A zero-length line between two edges meeting at 90°: the corner is judged between
        // the two real edges.
        let mut contour = line_contour(&[(8.0, 8.0), (40.0, 8.0), (40.0, 40.0), (8.0, 40.0)]);
        contour.segments.insert(
            1,
            Segment::Line {
                from: Point2::new(40.0, 8.0),
                to: Point2::new(40.0, 8.0),
            },
        );
        let outline = Outline {
            contours: vec![contour],
        };
        let colors = color_outline(&outline, DEFAULT_TURN_THRESHOLD_DEG);
        assert_ne!(colors[0][0], colors[0][2]);
    }

    #[test]
    fn exponential_composites_are_refused() {
        // Glyph 1 is the square; glyph k + 1 places glyph k sixteen times, seven levels deep:
        // 4 · 16⁷ ≈ 10⁹ segments. The parser must stop at its budget, not run out of memory.
        let square = cat(&[
            &i16b(1),
            &i16b(0),
            &i16b(0),
            &i16b(400),
            &i16b(400),
            &u16b(3),
            &u16b(0),
            &[0x01, 0x01, 0x01, 0x01],
            &i16b(0),
            &i16b(400),
            &i16b(0),
            &i16b(-400),
            &i16b(0),
            &i16b(0),
            &i16b(400),
            &i16b(0),
        ]);
        let mut glyphs = vec![Vec::new(), square];
        for level in 1..8u16 {
            let mut g = cat(&[&i16b(-1), &i16b(0), &i16b(0), &i16b(0), &i16b(0)]);
            for k in 0..16u16 {
                let more = if k < 15 { 0x0020 } else { 0 };
                g.extend_from_slice(&cat(&[
                    &u16b(0x0001 | 0x0002 | more),
                    &u16b(level),
                    &i16b(k as i16),
                    &i16b(0),
                ]));
            }
            glyphs.push(g);
        }
        let font = build_font(&glyphs);
        // One level is fine (64 segments).
        let g = parse_glyf_outline(&font, 2).unwrap_or_else(|e| panic!("parse failed: {e}"));
        assert_eq!(g.outline.contours.len(), 16);
        assert!(matches!(
            parse_glyf_outline(&font, 8),
            Err(MsdfError::GlyphTooComplex { .. })
        ));
    }

    #[test]
    fn channels_disagree_in_sign_beyond_a_corner() {
        // The defining MSDF property: just outside a square, beyond the top-right corner and
        // still within the right edge's half-plane, the channel that measures the right edge
        // but not the top edge reads "inside" (its pseudo-distance is to the right edge's
        // extended line), while the median reads "outside".
        let outline = square(8.2, 8.2, 15.6);
        let bmp = generate_uncorrected(&outline, [32, 32], 4.0, [1.0, 1.0], [0.0, 0.0])
            .unwrap_or_else(|e| panic!("generate failed: {e}"));
        // Texel center (22.5, 6.5): 1.3 left of the right edge x = 23.8, 1.7 above the top
        // edge y = 8.2 (image space, y down).
        let t = bmp.texel(22, 6).unwrap_or([0.0; 3]);
        let m = median3(t[0], t[1], t[2]);
        assert!(m < 0.5, "median {m} should be outside");
        let max = t[0].max(t[1]).max(t[2]);
        assert!(
            max > 0.5,
            "some channel should read inside beyond the corner: {t:?}"
        );
        // Its value is the pseudo-distance to the right edge's line: +1.3 texels.
        let expect = 0.5 + 0.5 * 1.3 / 4.0;
        assert!((f64::from(max) - expect).abs() < 1e-4, "{max} vs {expect}");
    }

    #[test]
    fn contour_orientation_does_not_change_the_median_field() {
        // TrueType outer contours run clockwise, PostScript ones counter-clockwise: both must
        // give the same distance field (the channel assignment may differ).
        let ccw = star(24.0, 24.0, 17.0, 7.5);
        let mut cw = ccw.clone();
        for contour in &mut cw.contours {
            contour.segments.reverse();
            for seg in &mut contour.segments {
                let (from, to) = seg.endpoints();
                *seg = Segment::Line { from: to, to: from };
            }
        }
        let a = generate_uncorrected(&ccw, [48, 48], 4.0, [1.0, 1.0], [0.0, 0.0])
            .unwrap_or_else(|e| panic!("generate failed: {e}"));
        let b = generate_uncorrected(&cw, [48, 48], 4.0, [1.0, 1.0], [0.0, 0.0])
            .unwrap_or_else(|e| panic!("generate failed: {e}"));
        for y in 0..48 {
            for x in 0..48 {
                let ta = a.texel(x, y).unwrap_or([0.0; 3]);
                let tb = b.texel(x, y).unwrap_or([0.0; 3]);
                let (ma, mb) = (median3(ta[0], ta[1], ta[2]), median3(tb[0], tb[1], tb[2]));
                assert!((ma - mb).abs() < 1e-5, "texel ({x},{y}): {ma} vs {mb}");
            }
        }
    }

    #[test]
    fn far_edge_extension_does_not_leak_into_the_field() {
        // A point outside a circle but on the tangent line of an arc join, far from that join:
        // the field must be the true distance to the circle, not the distance to the join's
        // tangent line (pseudo-distance applies only to the nearest edge).
        let r = 14.0f64;
        let outline = Outline {
            contours: vec![circle(24.0, 24.0, r)],
        };
        let bmp = generate(&outline, [48, 48], 16.0, [1.0, 1.0], [0.0, 0.0])
            .unwrap_or_else(|e| panic!("generate failed: {e}"));
        // Texel center (12.5, 9.5): the top join is (24, 10) with a horizontal tangent;
        // the point is 0.5 off that line but sqrt(11.5² + 14.5²) − 14 ≈ 4.507 from the circle.
        let t = bmp.texel(12, 9).unwrap_or([0.0; 3]);
        let d = (f64::from(median3(t[0], t[1], t[2])) - 0.5) * 2.0 * 16.0;
        let analytic = (11.5f64 * 11.5 + 14.5 * 14.5).sqrt() - r;
        assert!((d + analytic).abs() < 1e-2, "field {d} vs analytic {}", -analytic);
    }

    /// Prints the numbers the verification report quotes (IoU per shape with and without the
    /// error-correction pass, texels the pass changes, corner error, circle error, generation
    /// time). Run with `cargo test -p regia_ui --lib msdf::tests::report_numbers -- --ignored
    /// --nocapture`; it asserts nothing timing-related.
    #[test]
    #[ignore = "prints the verification report's numbers; run by hand"]
    fn report_numbers() {
        let shapes: [(&str, Outline); 6] = [
            ("square", square(9.3, 9.3, 29.4)),
            (
                "circle",
                Outline {
                    contours: vec![circle(24.0, 24.0, 14.0)],
                },
            ),
            ("triangle", triangle()),
            ("ring", ring(24.0, 24.0, 16.0, 7.0)),
            ("star", star(24.0, 24.0, 17.0, 7.5)),
            ("blob", blob()),
        ];
        for (name, outline) in &shapes {
            let raw = generate_uncorrected(outline, [48, 48], 4.0, [1.0, 1.0], [0.0, 0.0])
                .unwrap_or_else(|e| panic!("generate failed: {e}"));
            let (mut fixed, edges) = generate_field(outline, [48, 48], 4.0, [1.0, 1.0], [0.0, 0.0])
                .unwrap_or_else(|e| panic!("generate failed: {e}"));
            let clash_fixed = correct_clashes(&mut fixed, &edges, 4.0);
            let mut heur = raw.clone();
            heur.correct_errors();
            let mapped = map_outline(outline, 48.0, [1.0, 1.0], [0.0, 0.0]);
            let raster = raster_coverage(&mapped, 48, 48, 16);
            let changed = raw
                .data_f32()
                .as_chunks::<3>()
                .0
                .iter()
                .zip(heur.data_f32().as_chunks::<3>().0)
                .filter(|(a, b)| a != b)
                .count();
            // Edge placement error: max over 4x-supersampled points of |coverage − raster|
            // is too coarse; instead count pixels where the two disagree at 4x4 subsamples.
            let mut disagree = 0usize;
            let mut disagree_raw = 0usize;
            let mut disagree_heur = 0usize;
            let mut total = 0usize;
            let mut median_shift = 0.0f32;
            for (a, b) in raw
                .data_f32()
                .as_chunks::<3>()
                .0
                .iter()
                .zip(heur.data_f32().as_chunks::<3>().0)
            {
                let shift = (median3(a[0], a[1], a[2]) - median3(b[0], b[1], b[2])).abs();
                median_shift = median_shift.max(shift);
            }
            let fine = raster_coverage(&map_outline(outline, 192.0, [4.0, 4.0], [0.0, 0.0]), 192, 192, 4);
            for y in 0..192usize {
                for x in 0..192usize {
                    let u = (x as f64 + 0.5) / 4.0;
                    let v = (y as f64 + 0.5) / 4.0;
                    let a = fixed.coverage(u, v) > 0.5;
                    let b = fine[y * 192 + x] > 0.5;
                    total += 1;
                    if a != b {
                        disagree += 1;
                    }
                    if (raw.coverage(u, v) > 0.5) != b {
                        disagree_raw += 1;
                    }
                    if (heur.coverage(u, v) > 0.5) != b {
                        disagree_heur += 1;
                    }
                }
            }
            let start = std::time::Instant::now();
            let reps = 20u32;
            for _ in 0..reps {
                let _ = generate(outline, [48, 48], 4.0, [1.0, 1.0], [0.0, 0.0]);
            }
            let per = start.elapsed().as_secs_f64() * 1000.0 / f64::from(reps);
            println!(
                "{name:9} iou raw {:.4} generate {:.4} | shape-aware pass changed {clash_fixed} | \
                 value-space heuristic changed {changed}, max median shift {median_shift:.4} | \
                 4x subsample disagreement raw {disagree_raw} generate {disagree} heuristic \
                 {disagree_heur} of {total} | generate 48x48 {per:.2} ms",
                iou(&raw, &raster, 48, 48),
                iou(&fixed, &raster, 48, 48),
            );
        }
        // Circle: max |median distance − analytic| over all texels.
        let bmp = generate(
            &Outline {
                contours: vec![circle(24.0, 24.0, 14.0)],
            },
            [48, 48],
            4.0,
            [1.0, 1.0],
            [0.0, 0.0],
        )
        .unwrap_or_else(|e| panic!("generate failed: {e}"));
        let mut worst = 0.0f64;
        for y in 0..48u32 {
            for x in 0..48u32 {
                let t = bmp.texel(x, y).unwrap_or([0.0; 3]);
                let d = (f64::from(median3(t[0], t[1], t[2])) - 0.5) * 8.0;
                let px = f64::from(x) + 0.5 - 24.0;
                let py = f64::from(y) + 0.5 - 24.0;
                // The field is positive inside; the analytic distance is positive outside.
                worst = worst.max((d + ((px * px + py * py).sqrt() - 14.0)).abs());
            }
        }
        println!("circle max |field − analytic| = {worst:.5} texels");
        // Square corner: the diagonal crossing, as in the corner test.
        for (label, corrected) in [("raw", false), ("corrected", true)] {
            let outline = square(8.2, 8.2, 15.6);
            let bmp = if corrected {
                generate(&outline, [32, 32], 4.0, [1.0, 1.0], [0.0, 0.0])
            } else {
                generate_uncorrected(&outline, [32, 32], 4.0, [1.0, 1.0], [0.0, 0.0])
            }
            .unwrap_or_else(|e| panic!("generate failed: {e}"));
            let cov = |t: f64| bmp.coverage(16.0 - 12.0 * t, 16.0 - 12.0 * t);
            let (mut lo, mut hi) = (0.0f64, 1.0f64);
            for _ in 0..64 {
                let mid = 0.5 * (lo + hi);
                if cov(mid) > 0.5 {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            let u = 16.0 - 12.0 * 0.5 * (lo + hi);
            println!(
                "square corner ({label}) error {:.5} px",
                (u - 8.2).abs() * 2.0f64.sqrt()
            );
        }
    }
}
