// SPDX-License-Identifier: MPL-2.0
//! Scroll motion and nested scrolling (ADR-520, UI design series d-12): the math behind
//! [`crate::ScrollView`].
//!
//! - **Fling velocity** comes from the drag samples of the last [`FLING_WINDOW`] seconds
//!   ([`DragSamples`]), not from the last frame, so a finger that stops before it lifts does not
//!   fling and a short flick is not lost to one slow frame.
//! - **Inertia** is exponential velocity decay, `v(t) = v0 e^(-t/tau)`, integrated in closed form
//!   ([`coast`]): the distance a fling covers does not depend on the frame rate.
//! - **Overscroll** ([`Overscroll::Bounce`]) maps a drag past the edge through a rubber-band curve
//!   ([`rubber_band`]) and returns with a critically damped spring ([`spring_step`], closed form,
//!   no overshoot).
//! - **Nested scroll views** form a chain from the innermost view outward ([`distribute`]): each
//!   view takes what it can of a wheel or drag delta on the axes it scrolls, and the rest passes
//!   to the next view out unless a view is [`ScrollChaining::Contain`].
//!
//! Everything here is plain functions over numbers, so it is tested without an app.

/// How far back (seconds) drag samples count toward the fling velocity.
pub const FLING_WINDOW: f32 = 0.1;
/// A fling slower than this (physical px per second) stops.
pub const MIN_FLING_SPEED: f32 = 4.0;
/// The fastest fling (physical px per second, per axis).
pub const MAX_FLING_SPEED: f32 = 12_000.0;
/// Rubber-band stiffness: the fraction of the viewport a drag past the edge can never reach is
/// approached as `1 - 1 / (x * RUBBER_BAND / d + 1)`.
pub const RUBBER_BAND: f32 = 0.55;
/// Angular frequency (radians per second) of the spring that returns an overscrolled view to its
/// edge; it settles to under a pixel in about half a second.
pub const SPRING_OMEGA: f32 = 14.0;
/// An overscroll smaller than this (physical px) with a slow spring snaps to the edge.
pub const SPRING_REST: f32 = 0.5;

const SAMPLES: usize = 16;

/// Which axes a scroll view scrolls. The other axis passes every delta to the enclosing view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollAxes {
    /// Both axes (the default, as before d-12).
    #[default]
    Both,
    /// Top to bottom only.
    Vertical,
    /// Left to right only.
    Horizontal,
}

impl ScrollAxes {
    /// Whether the view scrolls on `axis` (0 = horizontal, 1 = vertical).
    pub fn allows(self, axis: usize) -> bool {
        match self {
            Self::Both => true,
            Self::Vertical => axis == 1,
            Self::Horizontal => axis == 0,
        }
    }

    /// The agent and RON name (`"both"`, `"vertical"`, `"horizontal"`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Both => "both",
            Self::Vertical => "vertical",
            Self::Horizontal => "horizontal",
        }
    }

    /// Parses [`Self::name`].
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "both" => Some(Self::Both),
            "vertical" => Some(Self::Vertical),
            "horizontal" => Some(Self::Horizontal),
            _ => None,
        }
    }
}

/// What a scroll view does at the end of its content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Overscroll {
    /// Stop at the edge (the default).
    #[default]
    Clamp,
    /// Drags pull past the edge with growing resistance and flings run past it; a spring brings
    /// the content back.
    Bounce,
}

impl Overscroll {
    /// The agent name (`"clamp"`, `"bounce"`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Clamp => "clamp",
            Self::Bounce => "bounce",
        }
    }

    /// Parses [`Self::name`].
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "clamp" => Some(Self::Clamp),
            "bounce" => Some(Self::Bounce),
            _ => None,
        }
    }
}

/// Whether a delta a scroll view cannot use passes to the scroll view around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollChaining {
    /// Pass the rest outward (the default).
    #[default]
    Chain,
    /// Keep it: scrolling inside this view never moves the views around it.
    Contain,
}

impl ScrollChaining {
    /// The agent name (`"chain"`, `"contain"`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Chain => "chain",
            Self::Contain => "contain",
        }
    }

    /// Parses [`Self::name`].
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "chain" => Some(Self::Chain),
            "contain" => Some(Self::Contain),
            _ => None,
        }
    }
}

/// Where a scroll delta came from: wheels never pull past an edge, drags may.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollInput {
    /// Mouse wheel or trackpad.
    Wheel,
    /// Pointer or touch drag.
    Drag,
}

/// The recent drag movement of one scroll view: a ring of `(time, delta)` samples.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DragSamples {
    samples: [(f32, [f32; 2]); SAMPLES],
    len: usize,
    head: usize,
    began: f32,
}

impl DragSamples {
    /// Starts a gesture at time `now` (seconds on any steady clock).
    pub fn begin(&mut self, now: f32) {
        *self = Self {
            began: now,
            ..Self::default()
        };
    }

    /// Records that the view moved by `delta` (offset space) in the frame that ended at `now`.
    pub fn push(&mut self, now: f32, delta: [f32; 2]) {
        // A non-finite delta would make every later velocity (and the fling) NaN: it counts as 0.
        let delta = delta.map(|d| if d.is_finite() { d } else { 0.0 });
        self.samples[self.head] = (now, delta);
        self.head = (self.head + 1) % SAMPLES;
        self.len = (self.len + 1).min(SAMPLES);
    }

    /// Number of samples held (at most 16).
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether no sample was recorded since [`Self::begin`].
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Mean velocity (offset px per second) over the last `window` seconds before `now`, or since
    /// the gesture began when that is more recent. Clamped to [`MAX_FLING_SPEED`] per axis.
    pub fn velocity(&self, now: f32, window: f32) -> [f32; 2] {
        let start = (now - window).max(self.began);
        let span = now - start;
        if span <= 0.0 {
            return [0.0; 2];
        }
        let mut sum = [0.0f32; 2];
        for (t, d) in self.samples.iter().take(self.len) {
            if *t > start && *t <= now {
                sum[0] += d[0];
                sum[1] += d[1];
            }
        }
        [
            (sum[0] / span).clamp(-MAX_FLING_SPEED, MAX_FLING_SPEED),
            (sum[1] / span).clamp(-MAX_FLING_SPEED, MAX_FLING_SPEED),
        ]
    }
}

/// One step of inertia: a view moving at `velocity` decays with time constant `tau` for `dt`
/// seconds. Returns `(distance, new_velocity)`, both exact for `v(t) = v0 e^(-t/tau)`, so ten 10 ms
/// steps cover the same distance as one 100 ms step. `tau <= 0` stops at once.
pub fn coast(velocity: f32, dt: f32, tau: f32) -> (f32, f32) {
    if tau <= 0.0 {
        return (0.0, 0.0);
    }
    let k = (-dt.max(0.0) / tau).exp();
    (velocity * tau * (1.0 - k), velocity * k)
}

/// The whole distance a fling at `velocity` covers before it decays to nothing: `velocity * tau`.
pub fn fling_distance(velocity: f32, tau: f32) -> f32 {
    velocity * tau.max(0.0)
}

/// Rubber band: how far content shows past its edge (px) when dragged `raw` px past it in a view
/// `dim` px long. Starts 1:1 times [`RUBBER_BAND`], and never reaches `dim`.
pub fn rubber_band(raw: f32, dim: f32) -> f32 {
    // Written so that NaN (and an infinite `dim`, which would give inf/inf) returns 0.
    if !(dim > 0.0 && dim.is_finite() && raw > 0.0) {
        return 0.0;
    }
    (1.0 - 1.0 / (raw * RUBBER_BAND / dim + 1.0)) * dim
}

/// Inverse of [`rubber_band`]: the drag distance that shows `shown` px of overscroll.
pub fn rubber_band_inverse(shown: f32, dim: f32) -> f32 {
    if !(dim > 0.0 && dim.is_finite() && shown > 0.0) {
        return 0.0;
    }
    let shown = shown.min(dim * 0.999);
    dim / RUBBER_BAND * (1.0 / (1.0 - shown / dim) - 1.0)
}

/// A drag of `delta` on an offset that may already sit past `[0, max]`: inside the range it moves
/// 1:1, past an edge it follows [`rubber_band`] (`dim` is the viewport length on that axis).
pub fn rubber_offset(offset: f32, max: f32, delta: f32, dim: f32) -> f32 {
    let max = max.max(0.0);
    let raw = if offset < 0.0 {
        -rubber_band_inverse(-offset, dim)
    } else if offset > max {
        max + rubber_band_inverse(offset - max, dim)
    } else {
        offset
    };
    let raw = raw + delta;
    if raw < 0.0 {
        -rubber_band(-raw, dim)
    } else if raw > max {
        max + rubber_band(raw - max, dim)
    } else {
        raw
    }
}

/// One step of a critically damped spring pulling displacement `x` (velocity `v`) to 0 with
/// angular frequency `omega`. Exact: `x(t) = (x0 + (v0 + omega x0) t) e^(-omega t)`.
pub fn spring_step(x: f32, v: f32, dt: f32, omega: f32) -> (f32, f32) {
    let dt = dt.max(0.0);
    let b = v + omega * x;
    let e = (-omega * dt).exp();
    ((x + b * dt) * e, (v - omega * b * dt) * e)
}

/// How far `offset` sits past `[0, max]` (negative before the start, positive after the end).
pub fn overscroll_amount(offset: f32, max: f32) -> f32 {
    if offset < 0.0 {
        offset
    } else if offset > max.max(0.0) {
        offset - max.max(0.0)
    } else {
        0.0
    }
}

/// One scroll view in a chain, as [`distribute`] sees it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChainLink {
    /// Current offset (physical px).
    pub offset: [f32; 2],
    /// Largest in-range offset per axis (`content - viewport`, at least 0).
    pub max: [f32; 2],
    /// Viewport size (physical px), the rubber-band length.
    pub viewport: [f32; 2],
    /// Axes the view scrolls.
    pub axes: ScrollAxes,
    /// Edge behavior.
    pub overscroll: Overscroll,
    /// Whether leftovers pass outward.
    pub chaining: ScrollChaining,
}

/// Spreads an offset-space `delta` over a chain of nested scroll views, innermost first, and
/// returns each view's new offset (same order as `links`).
///
/// Per axis: a view that does not scroll the axis passes the whole delta on. An [`Overscroll::Bounce`]
/// view already past its edge takes the whole drag delta through the rubber band. Otherwise a view
/// takes what fits in `[0, max]` and passes the rest on, unless it is
/// [`ScrollChaining::Contain`]. A drag delta left over at the end of the chain pulls the innermost
/// bouncing view that saw it past its edge. Wheels never leave the range.
pub fn distribute(links: &[ChainLink], delta: [f32; 2], input: ScrollInput) -> Vec<[f32; 2]> {
    let mut out: Vec<[f32; 2]> = links.iter().map(|l| l.offset).collect();
    for (axis, &d) in delta.iter().enumerate() {
        // A NaN delta must not reach the offsets (clamping passes NaN through): it moves nothing.
        let mut rest = if d.is_nan() { 0.0 } else { d };
        let mut bounce: Option<usize> = None;
        for (i, link) in links.iter().enumerate() {
            if rest == 0.0 {
                break;
            }
            if !link.axes.allows(axis) {
                continue;
            }
            let max = link.max[axis].max(0.0);
            let offset = out[i][axis];
            let drag = input == ScrollInput::Drag;
            let bounces = drag && link.overscroll == Overscroll::Bounce;
            if bounces && bounce.is_none() {
                bounce = Some(i);
            }
            if bounces && overscroll_amount(offset, max) != 0.0 {
                out[i][axis] = rubber_offset(offset, max, rest, link.viewport[axis]);
                rest = 0.0;
                break;
            }
            // An offset out of range (content shrank under a clamping view) snaps in first; the snap
            // is not part of the delta.
            let start = offset.clamp(0.0, max);
            let next = (start + rest).clamp(0.0, max);
            rest -= next - start;
            out[i][axis] = next;
            if link.chaining == ScrollChaining::Contain {
                break;
            }
        }
        if rest != 0.0
            && let Some(i) = bounce
        {
            let link = &links[i];
            let max = link.max[axis].max(0.0);
            out[i][axis] = rubber_offset(out[i][axis], max, rest, link.viewport[axis]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(offset: [f32; 2], max: [f32; 2]) -> ChainLink {
        ChainLink {
            offset,
            max,
            viewport: [300.0, 300.0],
            axes: ScrollAxes::Both,
            overscroll: Overscroll::Clamp,
            chaining: ScrollChaining::Chain,
        }
    }

    #[test]
    fn velocity_is_the_mean_over_the_window() {
        let mut s = DragSamples::default();
        s.begin(0.0);
        // 10 ms frames moving 5 px each: 500 px/s.
        for i in 1..=20 {
            s.push(i as f32 * 0.01, [0.0, 5.0]);
        }
        // Evaluated between frames so no sample sits on the window edge.
        let v = s.velocity(0.205, FLING_WINDOW);
        assert!((v[1] - 500.0).abs() < 1.0, "{v:?}");
        assert_eq!(v[0], 0.0);
        // The finger rested for the last window: no fling.
        let rested = s.velocity(0.35, FLING_WINDOW);
        assert_eq!(rested, [0.0, 0.0]);
    }

    #[test]
    fn velocity_does_not_depend_on_the_frame_rate() {
        // The same 300 px/s drag sampled at 100 ms and at 10 ms.
        let mut coarse = DragSamples::default();
        coarse.begin(0.0);
        for i in 1..=3 {
            coarse.push(i as f32 * 0.1, [0.0, 30.0]);
        }
        let mut fine = DragSamples::default();
        fine.begin(0.0);
        for i in 1..=30 {
            fine.push(i as f32 * 0.01, [0.0, 3.0]);
        }
        let a = coarse.velocity(0.305, FLING_WINDOW);
        let b = fine.velocity(0.305, FLING_WINDOW);
        assert!(
            (a[1] - 300.0).abs() < 0.5 && (b[1] - 300.0).abs() < 0.5,
            "{a:?} {b:?}"
        );
    }

    #[test]
    fn a_short_flick_uses_its_own_duration() {
        // A 40 ms flick of 40 px: 1000 px/s, not 40 px over the whole 100 ms window.
        let mut s = DragSamples::default();
        s.begin(1.0);
        for i in 1..=4 {
            s.push(1.0 + i as f32 * 0.01, [10.0, 0.0]);
        }
        let now = 1.0 + 4.0 * 0.01;
        let v = s.velocity(now, FLING_WINDOW);
        assert!((v[0] - 1000.0).abs() < 1.0, "{v:?}");
    }

    #[test]
    fn velocity_is_capped_and_the_ring_wraps() {
        let mut s = DragSamples::default();
        s.begin(0.0);
        for i in 1..=40 {
            s.push(i as f32 * 0.001, [0.0, 1000.0]);
        }
        assert_eq!(s.len(), 16);
        assert_eq!(s.velocity(0.04, FLING_WINDOW)[1], MAX_FLING_SPEED);
        let mut empty = DragSamples::default();
        empty.begin(2.0);
        assert!(empty.is_empty());
        assert_eq!(empty.velocity(2.0, FLING_WINDOW), [0.0, 0.0]);
    }

    #[test]
    fn coasting_is_frame_rate_independent() {
        let (tau, v0) = (0.3, 1200.0);
        let (mut x_a, mut v_a) = (0.0, v0);
        for _ in 0..10 {
            let (d, v) = coast(v_a, 0.01, tau);
            x_a += d;
            v_a = v;
        }
        let (x_b, v_b) = coast(v0, 0.1, tau);
        assert!((x_a - x_b).abs() < 1e-2, "{x_a} vs {x_b}");
        assert!((v_a - v_b).abs() < 1e-2);
        // Over a long time it approaches v0 * tau.
        let (far, _) = coast(v0, 10.0, tau);
        assert!((far - fling_distance(v0, tau)).abs() < 1e-2);
        assert_eq!(coast(v0, 0.1, 0.0), (0.0, 0.0));
    }

    #[test]
    fn rubber_band_resists_and_inverts() {
        let dim = 400.0;
        // Small pulls start at RUBBER_BAND x the drag.
        assert!((rubber_band(1.0, dim) - RUBBER_BAND).abs() < 0.01);
        // Large pulls never reach the viewport length.
        assert!(rubber_band(1.0e6, dim) < dim);
        assert!(rubber_band(200.0, dim) < 200.0 * RUBBER_BAND);
        for raw in [1.0, 25.0, 300.0, 4000.0] {
            let shown = rubber_band(raw, dim);
            assert!((rubber_band_inverse(shown, dim) - raw).abs() / raw < 1e-3);
        }
        assert_eq!(rubber_band(-5.0, dim), 0.0);
        assert_eq!(rubber_band(5.0, 0.0), 0.0);
    }

    #[test]
    fn rubber_offset_moves_freely_inside_and_resists_outside() {
        assert_eq!(rubber_offset(100.0, 500.0, 50.0, 300.0), 150.0);
        let past = rubber_offset(480.0, 500.0, 100.0, 300.0);
        assert!(past > 500.0 && past < 580.0, "{past}");
        // Dragging back by the same amount returns exactly (the map is invertible).
        let back = rubber_offset(past, 500.0, -100.0, 300.0);
        assert!((back - 480.0).abs() < 1e-2, "{back}");
        let before = rubber_offset(0.0, 500.0, -60.0, 300.0);
        assert!(before < 0.0 && before > -60.0);
    }

    #[test]
    fn the_spring_settles_without_overshoot() {
        let (mut x, mut v) = (-80.0f32, 0.0f32);
        let mut steps = 0;
        while (x.abs() > SPRING_REST || v.abs() > MIN_FLING_SPEED) && steps < 1000 {
            let (nx, nv) = spring_step(x, v, 1.0 / 60.0, SPRING_OMEGA);
            assert!(
                nx <= 0.0,
                "a critically damped spring never crosses the edge ({nx})"
            );
            x = nx;
            v = nv;
            steps += 1;
        }
        assert!(steps < 60, "settles in well under a second ({steps} steps)");
        // Stepping is exact: two half steps equal one whole step.
        let (x1, v1) = spring_step(-80.0, 300.0, 0.1, SPRING_OMEGA);
        let (xh, vh) = spring_step(-80.0, 300.0, 0.05, SPRING_OMEGA);
        let (x2, v2) = spring_step(xh, vh, 0.05, SPRING_OMEGA);
        assert!((x1 - x2).abs() < 1e-3 && (v1 - v2).abs() < 1e-2);
    }

    #[test]
    fn inner_view_scrolls_first_and_the_rest_chains_outward() {
        let inner = link([0.0, 380.0], [0.0, 400.0]);
        let outer = link([0.0, 100.0], [0.0, 1000.0]);
        let out = distribute(&[inner, outer], [0.0, 50.0], ScrollInput::Wheel);
        assert_eq!(out, vec![[0.0, 400.0], [0.0, 130.0]]);
        // Fully inside the inner view's range: the outer view does not move.
        let out = distribute(&[inner, outer], [0.0, -30.0], ScrollInput::Wheel);
        assert_eq!(out, vec![[0.0, 350.0], [0.0, 100.0]]);
    }

    #[test]
    fn a_horizontal_strip_passes_vertical_deltas_out() {
        let strip = ChainLink {
            axes: ScrollAxes::Horizontal,
            ..link([10.0, 0.0], [900.0, 0.0])
        };
        let page = ChainLink {
            axes: ScrollAxes::Vertical,
            ..link([0.0, 0.0], [0.0, 2000.0])
        };
        let out = distribute(&[strip, page], [40.0, 70.0], ScrollInput::Drag);
        assert_eq!(out, vec![[50.0, 0.0], [0.0, 70.0]]);
    }

    #[test]
    fn contain_stops_the_chain() {
        let inner = ChainLink {
            chaining: ScrollChaining::Contain,
            ..link([0.0, 390.0], [0.0, 400.0])
        };
        let outer = link([0.0, 100.0], [0.0, 1000.0]);
        let out = distribute(&[inner, outer], [0.0, 50.0], ScrollInput::Wheel);
        assert_eq!(out, vec![[0.0, 400.0], [0.0, 100.0]]);
    }

    #[test]
    fn drags_rubber_band_only_when_nothing_outside_can_take_them() {
        let inner = ChainLink {
            overscroll: Overscroll::Bounce,
            ..link([0.0, 400.0], [0.0, 400.0])
        };
        let outer = link([0.0, 100.0], [0.0, 1000.0]);
        // The outer view still has room: it takes the drag; nothing bounces.
        let out = distribute(&[inner, outer], [0.0, 50.0], ScrollInput::Drag);
        assert_eq!(out, vec![[0.0, 400.0], [0.0, 150.0]]);
        // The outer view is at its end too: the inner view pulls past its edge.
        let outer_end = link([0.0, 1000.0], [0.0, 1000.0]);
        let out = distribute(&[inner, outer_end], [0.0, 50.0], ScrollInput::Drag);
        assert!(out[0][1] > 400.0 && out[0][1] < 450.0, "{out:?}");
        assert_eq!(out[1][1], 1000.0);
        // A wheel never leaves the range.
        let out = distribute(&[inner, outer_end], [0.0, 50.0], ScrollInput::Wheel);
        assert_eq!(out, vec![[0.0, 400.0], [0.0, 1000.0]]);
    }

    #[test]
    fn an_overscrolled_view_takes_the_whole_drag_back() {
        let inner = ChainLink {
            overscroll: Overscroll::Bounce,
            ..link([0.0, 430.0], [0.0, 400.0])
        };
        let outer = link([0.0, 100.0], [0.0, 1000.0]);
        let out = distribute(&[inner, outer], [0.0, -20.0], ScrollInput::Drag);
        assert!(out[0][1] < 430.0 && out[0][1] > 400.0, "{out:?}");
        assert_eq!(
            out[1][1], 100.0,
            "the outer view waits until the inner one is back in range"
        );
    }

    #[test]
    fn names_round_trip() {
        for a in [ScrollAxes::Both, ScrollAxes::Vertical, ScrollAxes::Horizontal] {
            assert_eq!(ScrollAxes::parse(a.name()), Some(a));
        }
        for o in [Overscroll::Clamp, Overscroll::Bounce] {
            assert_eq!(Overscroll::parse(o.name()), Some(o));
        }
        for c in [ScrollChaining::Chain, ScrollChaining::Contain] {
            assert_eq!(ScrollChaining::parse(c.name()), Some(c));
        }
        assert_eq!(ScrollAxes::parse("diagonal"), None);
    }
}
