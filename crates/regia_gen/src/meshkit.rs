// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (shell-regia/game/src/world/meshkit.rs, commit d6ed37677), ADR-8012.
// Mark: adapt (D-02 class P3). Regia reimplemented the behavior; see the docs/provenance.md row (ADR-8044).
//! Rounded boxes, chamfered and knurled cylinders, annuli, tubes and sweeps (ADR-8044).
//!
//! Frames are right-handed and orthonormal. [`Frame::identity`] is the engine frame (+X right,
//! +Y up, −Z forward). [`Frame::xyz`] keeps local +Z along world +Z, which is what the mesh
//! recipe uses so a cylinder's length matches an extrude's depth. Positions are authored in that
//! frame: there is no reflection into another coordinate system.
//!
//! Angles go through [`regia_core::math::det::sin_cos`]. The rounded box samples each face on a
//! uniform grid (the published rounded-box construction of clamp-to-the-inner-box, then offset by
//! the radius). A power-of-`t` edge bias is not used: it is a platform-dependent transcendental,
//! and uniform samples stay bit-stable. The tube's spine is uniform Catmull-Rom (Catmull and Rom,
//! 1974) with a parallel-transport frame.

use regia_core::math::det;

use crate::build::{cross, dot, normalize, sub};
use crate::extrude::rounded_rect;
use crate::{CpuMesh, GenError, MAX_SEGMENTS, check_vertices, finite, weld_key};

/// A right-handed frame. `right`, `up` and `forward` are the local X, Y and Z axes.
///
/// Callers that fill the fields by hand pass unit orthogonal axes. [`Frame::identity`],
/// [`Frame::xyz`] and [`Frame::facing`] do that.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frame {
    /// World position of the local origin, metres.
    pub origin: [f32; 3],
    /// Local +X.
    pub right: [f32; 3],
    /// Local +Y.
    pub up: [f32; 3],
    /// Local +Z.
    pub forward: [f32; 3],
}

impl Frame {
    /// Engine axes: +X right, +Y up, −Z forward.
    pub fn identity() -> Self {
        Self {
            origin: [0.0; 3],
            right: [1.0, 0.0, 0.0],
            up: [0.0, 1.0, 0.0],
            forward: [0.0, 0.0, -1.0],
        }
    }

    /// Geometric axes: local +Z lies along world +Z. Mesh recipes use this frame.
    pub fn xyz() -> Self {
        Self {
            origin: [0.0; 3],
            right: [1.0, 0.0, 0.0],
            up: [0.0, 1.0, 0.0],
            forward: [0.0, 0.0, 1.0],
        }
    }

    /// A frame at `origin` looking along `forward`, with up toward world +Y (world +X when
    /// `forward` is nearly vertical).
    ///
    /// # Errors
    ///
    /// [`GenError::InvalidInput`] when an input is non-finite or `forward` is zero.
    pub fn facing(origin: [f32; 3], forward: [f32; 3]) -> Result<Self, GenError> {
        finite("frame origin", &origin)?;
        finite("forward", &forward)?;
        let forward = normalize(forward);
        if forward == [0.0; 3] {
            return Err(GenError::InvalidInput("forward is zero".into()));
        }
        let hint = if forward[1].abs() < 0.9 {
            [0.0, 1.0, 0.0]
        } else {
            [1.0, 0.0, 0.0]
        };
        let right = normalize(cross(hint, forward));
        let up = cross(forward, right);
        Ok(Self {
            origin,
            right,
            up,
            forward,
        })
    }

    /// Local point `(x, y, z)` in world space. `x` runs along [`right`](Self::right).
    pub fn at(self, x: f32, y: f32, z: f32) -> [f32; 3] {
        add(
            add(add(self.origin, mul(self.right, x)), mul(self.up, y)),
            mul(self.forward, z),
        )
    }

    /// Local direction `(x, y, z)` in world space, without translation or normalizing.
    pub fn dir(self, x: f32, y: f32, z: f32) -> [f32; 3] {
        add(add(mul(self.right, x), mul(self.up, y)), mul(self.forward, z))
    }
}

/// Options for [`cylinder`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CylinderOptions {
    /// Radius of the straight wall, metres. Must be `> 0`.
    pub radius: f32,
    /// Length along [`Frame::forward`], from the frame origin, metres. Must be `> 0`.
    pub length: f32,
    /// Facets around the axis, `3..=MAX_SEGMENTS`.
    pub sides: u32,
    /// `0` keeps a smooth wall. In `(0, 1)`, odd facets use `radius * (1 - knurl)` and the
    /// wall is flat-shaded. `1` is rejected (the odd radius would be zero).
    pub knurl: f32,
    /// Radial and axial inset of a 45° ring at the forward end. `0` leaves the end square.
    /// A value above `min(length / 2, radius / 2)` is clamped to that.
    pub chamfer: f32,
    /// Close the forward end (the end at `length`).
    pub cap: bool,
    /// Close the end on the frame origin. Off unless the caller asks: a one-cap cylinder is
    /// the usual part, and a closed solid sets both.
    pub bottom_cap: bool,
    /// UV repeats along the circumference and the length. Finite, any sign.
    pub tile: f32,
}

impl Default for CylinderOptions {
    /// Radius `0.5`, length `1`, 16 sides, no knurl, no chamfer, a forward cap, no bottom cap,
    /// tile `1`.
    fn default() -> Self {
        Self {
            radius: 0.5,
            length: 1.0,
            sides: 16,
            knurl: 0.0,
            chamfer: 0.0,
            cap: true,
            bottom_cap: false,
            tile: 1.0,
        }
    }
}

/// Options for [`annulus`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AnnulusOptions {
    /// Inner radius, metres. `0` is a disk. Must be `>= 0`.
    pub inner: f32,
    /// Outer radius, metres. Must be `> inner`.
    pub outer: f32,
    /// Facets around the ring, `3..=MAX_SEGMENTS`.
    pub sides: u32,
    /// UV repeats across the circumference. Finite, any sign.
    pub tile: f32,
}

/// Options for [`tube`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TubeOptions {
    /// Section radius, metres. Must be `> 0`.
    pub radius: f32,
    /// Facets around the section, `3..=MAX_SEGMENTS`.
    pub sides: u32,
    /// Samples on each Catmull-Rom span, `1..=MAX_SEGMENTS`. The ends of a two-point span are
    /// the input points; samples between them stay on a straight segment but are not spaced
    /// linearly (the uniform Catmull-Rom chord with duplicated end controls).
    pub steps_per_segment: u32,
    /// UV repeats around the section and along the span count. Finite, any sign.
    pub tile: f32,
}

impl Default for TubeOptions {
    /// Radius `0.5`, 12 sides, 4 steps per span, tile `1`.
    fn default() -> Self {
        Self {
            radius: 0.5,
            sides: 12,
            steps_per_segment: 4,
            tile: 1.0,
        }
    }
}

/// Options for [`sweep_rounded_rect`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SweepRect {
    /// Half extents of the section, metres, `(across, out)`. Both `> 0`.
    pub half: [f32; 2],
    /// Corner radius of the section, in `[0, min(half)]`.
    pub corner: f32,
    /// Slides the section along the orthogonalized `out` axis, metres.
    pub shift: f32,
    /// World axis the section's up follows. Re-orthogonalized against each tangent, so a path
    /// parallel to `out` is rejected.
    pub out: [f32; 3],
    /// Facets on each corner when `corner > 0` (`1..=MAX_SEGMENTS`). Ignored when `corner` is 0.
    pub corner_segments: u32,
    /// Samples on each straight span of the path, `1..=MAX_SEGMENTS`.
    pub steps: u32,
    /// UV repeats around the section and along the path. Finite, any sign.
    pub tile: f32,
}

/// One station of [`sweep_section`]: the section origin and the two axes of its plane.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RailFrame {
    /// World position of the section origin, metres.
    pub origin: [f32; 3],
    /// World direction of profile `x`. Orthogonalized against [`out`](Self::out) and normalized.
    pub across: [f32; 3],
    /// World direction of profile `y`. Normalized.
    pub out: [f32; 3],
}

/// A box of half extents `half`, centered on the frame origin, with corners rounded by `radius`.
///
/// `segments` is the number of quads along each edge of a face, spaced uniformly. The surface
/// stays inside the sharp box. Face normals point outward; a positive `mesh_stats` volume follows
/// when the frame is right-handed.
///
/// # Errors
///
/// [`GenError::InvalidInput`] for a non-finite value, a non-positive half extent, a radius
/// outside `[0, min(half)]`, or `segments` outside `1..=MAX_SEGMENTS`.
/// [`GenError::TooLarge`] when the vertex count would pass `MAX_VERTICES`.
pub fn rounded_box(
    frame: &Frame,
    half: [f32; 3],
    radius: f32,
    segments: u32,
    tile: f32,
) -> Result<CpuMesh, GenError> {
    let _span = tracing::trace_span!("regia_gen::rounded_box").entered();
    check_frame(frame)?;
    finite("half extents", &half)?;
    finite("radius", &[radius])?;
    check_tile(tile)?;
    if half[0] <= 0.0 || half[1] <= 0.0 || half[2] <= 0.0 {
        return Err(GenError::InvalidInput("half extents must be > 0".into()));
    }
    if radius < 0.0 || radius > half[0].min(half[1]).min(half[2]) {
        return Err(GenError::InvalidInput(
            "corner radius must be in [0, min(half extents)]".into(),
        ));
    }
    check_count("segments", segments, 1)?;
    let side = (segments as usize).saturating_add(1);
    let count = side.saturating_mul(side).saturating_mul(6);
    check_vertices(count)?;
    let mut sink = Sink::default();
    for face in FACES {
        emit_face(&mut sink, frame, face, half, radius, segments, tile);
    }
    Ok(sink.finish())
}

/// A cylinder along `frame.forward`, base on the frame origin.
///
/// The straight wall runs to `length - chamfer`. The chamfer is a 45° ring whose radius is
/// `radius * (1 - chamfer / radius)` (and the same factor on a knurled facet). Odd knurl facets
/// are the shorter ones. Caps are fans; leave [`CylinderOptions::bottom_cap`] off for an open
/// base.
///
/// # Errors
///
/// [`GenError::InvalidInput`] for a non-finite value, a non-positive radius or length, a knurl
/// outside `[0, 1)`, a negative chamfer, or `sides` outside `3..=MAX_SEGMENTS`.
pub fn cylinder(frame: &Frame, options: &CylinderOptions) -> Result<CpuMesh, GenError> {
    let _span = tracing::trace_span!("regia_gen::cylinder").entered();
    let spec = cylinder_spec(frame, options)?;
    let mut sink = Sink::default();
    if spec.knurl == 0.0 {
        smooth_cylinder(&mut sink, frame, &spec);
    } else {
        flat_cylinder(&mut sink, frame, &spec);
    }
    Ok(sink.finish())
}

/// A flat ring in the frame's right/up plane, normal along forward. `inner == 0` fills a disk.
///
/// # Errors
///
/// [`GenError::InvalidInput`] for a non-finite value, a negative inner radius, an outer radius
/// that is not greater than the inner, or `sides` outside `3..=MAX_SEGMENTS`.
pub fn annulus(frame: &Frame, options: &AnnulusOptions) -> Result<CpuMesh, GenError> {
    let _span = tracing::trace_span!("regia_gen::annulus").entered();
    check_frame(frame)?;
    finite("annulus radii", &[options.inner, options.outer])?;
    check_tile(options.tile)?;
    if options.inner < 0.0 || options.outer <= options.inner {
        return Err(GenError::InvalidInput(
            "annulus radii must satisfy 0 <= inner < outer".into(),
        ));
    }
    check_count("sides", options.sides, 3)?;
    let mut sink = Sink::default();
    emit_annulus(&mut sink, frame, options);
    Ok(sink.finish())
}

/// A circular tube along a Catmull-Rom spine through `points` (at least two).
///
/// The section is carried by parallel transport. Ends are open. A straight two-point path stays
/// on the segment, so every vertex is `radius` from the axis; the samples between the ends are
/// not linearly spaced.
///
/// # Errors
///
/// [`GenError::InvalidInput`] for fewer than two points, a non-finite value, a zero-length span,
/// a non-positive radius, or a count outside its range.
/// [`GenError::TooLarge`] when the vertex count would pass `MAX_VERTICES`.
pub fn tube(points: &[[f32; 3]], options: &TubeOptions) -> Result<CpuMesh, GenError> {
    let _span = tracing::trace_span!("regia_gen::tube").entered();
    finite("tube radius", &[options.radius])?;
    check_tile(options.tile)?;
    if options.radius <= 0.0 {
        return Err(GenError::InvalidInput("tube radius must be > 0".into()));
    }
    check_count("sides", options.sides, 3)?;
    check_count("steps", options.steps_per_segment, 1)?;
    let centers = catmull_stations(points, options.steps_per_segment)?;
    let stations = centers.len();
    let count = stations.saturating_mul((options.sides as usize).saturating_add(1));
    check_vertices(count)?;
    let mut sink = Sink::default();
    emit_tube(&mut sink, &centers, options);
    Ok(sink.finish())
}

/// Sweeps a rounded rectangle along the straight spans of `points`.
///
/// `out` is orthogonalized against each span tangent and stays as aligned with the caller axis
/// as that allows (it is not parallel-transported). The section is the same rounded rectangle as
/// `rounded_rect`, centered, then shifted along `out` by [`SweepRect::shift`].
///
/// # Errors
///
/// [`GenError::InvalidInput`] for a bad section, fewer than two points, a zero-length span, a
/// non-finite value, or a path parallel to `out`.
/// [`GenError::TooLarge`] when the vertex count would pass `MAX_VERTICES`.
pub fn sweep_rounded_rect(points: &[[f32; 3]], options: &SweepRect) -> Result<CpuMesh, GenError> {
    let _span = tracing::trace_span!("regia_gen::sweep_rounded_rect").entered();
    finite("sweep half", &options.half)?;
    finite("sweep corner", &[options.corner, options.shift])?;
    finite("sweep out", &options.out)?;
    check_tile(options.tile)?;
    check_count("steps", options.steps, 1)?;
    if dot(options.out, options.out) == 0.0 {
        return Err(GenError::InvalidInput("sweep out is zero".into()));
    }
    let segments = if options.corner == 0.0 {
        1
    } else {
        options.corner_segments
    };
    let profile = rounded_rect(
        [options.half[0] * 2.0, options.half[1] * 2.0],
        options.corner,
        segments,
    )?;
    let stations = linear_stations(points, options.steps)?;
    emit_profile_sweep(&stations, &profile, options.shift, options.out, options.tile)
}

/// Sweeps a closed profile along explicit frames.
///
/// Profile `x` follows [`RailFrame::across`] and `y` follows [`RailFrame::out`]. The polygon is
/// counter-clockwise so flat per-edge normals point outward; a repeated closing point is dropped.
/// `arris_band` (metres) inserts a sample that far from each corner along an edge long enough to
/// hold two of them, so a corner band has its own vertices. `0` inserts none. The documented
/// machined width is `0.0015` (1.5 mm); pass that when you want it. It is not applied unless you
/// pass it.
///
/// `wear`, when set, is one weight in `0..=1` per station. Vertex color is `(1 - wear)` on RGB
/// and `1` on alpha, and `colors` stays empty when `wear` is `None`. Ends are open.
///
/// # Errors
///
/// [`GenError::InvalidInput`] for fewer than two stations, a profile under three points, a
/// non-finite value, a negative arris band, a wear weight outside `0..=1`, a wear slice whose
/// length is not the station count, or a frame with a zero or parallel axis.
/// [`GenError::TooLarge`] when the vertex count would pass `MAX_VERTICES`.
pub fn sweep_section(
    rail: &[RailFrame],
    profile: &[[f32; 2]],
    arris_band: f32,
    wear: Option<&[f32]>,
) -> Result<CpuMesh, GenError> {
    let _span = tracing::trace_span!("regia_gen::sweep_section").entered();
    if rail.len() < 2 {
        return Err(GenError::InvalidInput(
            "a section sweep needs at least two frames".into(),
        ));
    }
    finite("arris band", &[arris_band])?;
    if arris_band < 0.0 {
        return Err(GenError::InvalidInput("arris band must be >= 0".into()));
    }
    let ring = open_ring(profile)?;
    let ring = expand_arris(&ring, arris_band);
    if let Some(weights) = wear {
        if weights.len() != rail.len() {
            return Err(GenError::InvalidInput(format!(
                "wear has {} weights for {} frames",
                weights.len(),
                rail.len()
            )));
        }
        finite("wear", weights)?;
        if weights.iter().any(|w| !(0.0..=1.0).contains(w)) {
            return Err(GenError::InvalidInput("wear weights must be in 0..=1".into()));
        }
    }
    let edges = ring.len();
    let count = edges.saturating_mul(rail.len()).saturating_mul(4);
    check_vertices(count)?;
    let mut sink = Sink::default();
    emit_section(&mut sink, rail, &ring, wear)?;
    Ok(sink.finish())
}

fn emit_profile_sweep(
    stations: &[[f32; 3]],
    profile: &[[f32; 2]],
    shift: f32,
    out: [f32; 3],
    tile: f32,
) -> Result<CpuMesh, GenError> {
    let normals = profile_normals(profile);
    let count = stations.len().saturating_mul(profile.len().saturating_add(1));
    check_vertices(count)?;
    let mut sink = Sink::default();
    let mut previous: Option<(u32, [f32; 3], [f32; 3])> = None;
    for (index, station) in stations.iter().enumerate() {
        let tangent = station_tangent(stations, index)?;
        let (across, up) = section_axes(tangent, out)?;
        let v = index as f32 / (stations.len() - 1) as f32 * tile;
        let placed = Placement {
            origin: *station,
            across,
            up,
            shift,
            v,
            tile,
        };
        let first = push_profile_ring(&mut sink, placed, profile, &normals);
        if let Some((prev, prev_across, prev_up)) = previous {
            bridge_rings(
                &mut sink,
                prev,
                first,
                profile.len(),
                prev_across,
                prev_up,
                &normals,
            );
        }
        previous = Some((first, across, up));
    }
    Ok(sink.finish())
}

struct Placement {
    origin: [f32; 3],
    across: [f32; 3],
    up: [f32; 3],
    shift: f32,
    v: f32,
    tile: f32,
}

fn push_profile_ring(sink: &mut Sink, place: Placement, profile: &[[f32; 2]], normals: &[[f32; 2]]) -> u32 {
    let first = sink.len();
    let m = profile.len();
    for k in 0..=m {
        let i = if k == m { 0 } else { k };
        let p = profile[i];
        let n = normals[i];
        let pos = add(
            add(place.origin, mul(place.across, p[0])),
            mul(place.up, p[1] + place.shift),
        );
        let normal = normalize(add(mul(place.across, n[0]), mul(place.up, n[1])));
        let u = k as f32 / m as f32 * place.tile;
        sink.vertex(pos, normal, [u, place.v]);
    }
    first
}

fn bridge_rings(
    sink: &mut Sink,
    prev: u32,
    next: u32,
    profile_len: usize,
    across: [f32; 3],
    up: [f32; 3],
    normals: &[[f32; 2]],
) {
    for (k, n) in normals.iter().enumerate().take(profile_len) {
        let hint = normalize(add(mul(across, n[0]), mul(up, n[1])));
        let k = k as u32;
        let a0 = prev + k;
        let a1 = prev + k + 1;
        let b0 = next + k;
        let b1 = next + k + 1;
        sink.tri_out(a0, a1, b1, hint);
        sink.tri_out(a0, b1, b0, hint);
    }
}

fn emit_section(
    sink: &mut Sink,
    rail: &[RailFrame],
    ring: &[[f32; 2]],
    wear: Option<&[f32]>,
) -> Result<(), GenError> {
    let mut axes = Vec::with_capacity(rail.len());
    for frame in rail {
        axes.push(rail_axes(frame)?);
    }
    let edges = ring.len();
    for e in 0..edges {
        let a = ring[e];
        let b = ring[(e + 1) % edges];
        let dx = b[0] - a[0];
        let dy = b[1] - a[1];
        let outward = normalize2([dy, -dx]);
        for s in 0..rail.len() - 1 {
            let color = wear.map(|w| wear_color(w[s]));
            let color_b = wear.map(|w| wear_color(w[s + 1]));
            let (across, up) = axes[s];
            let (across_b, up_b) = axes[s + 1];
            let p00 = section_point(rail[s].origin, across, up, a);
            let p10 = section_point(rail[s].origin, across, up, b);
            let p11 = section_point(rail[s + 1].origin, across_b, up_b, b);
            let p01 = section_point(rail[s + 1].origin, across_b, up_b, a);
            let hint = normalize(add(mul(across, outward[0]), mul(up, outward[1])));
            let normal = face_normal4(p00, p10, p01, hint);
            sink.color = color;
            let i00 = sink.vertex(p00, normal, [0.0, s as f32]);
            let i10 = sink.vertex(p10, normal, [1.0, s as f32]);
            sink.color = color_b;
            let i11 = sink.vertex(p11, normal, [1.0, (s + 1) as f32]);
            let i01 = sink.vertex(p01, normal, [0.0, (s + 1) as f32]);
            sink.tri_out(i00, i10, i11, hint);
            sink.tri_out(i00, i11, i01, hint);
        }
    }
    Ok(())
}

fn section_point(origin: [f32; 3], across: [f32; 3], up: [f32; 3], p: [f32; 2]) -> [f32; 3] {
    add(add(origin, mul(across, p[0])), mul(up, p[1]))
}

fn wear_color(weight: f32) -> [f32; 4] {
    let shade = 1.0 - weight;
    [shade, shade, shade, 1.0]
}

fn rail_axes(frame: &RailFrame) -> Result<([f32; 3], [f32; 3]), GenError> {
    finite("rail origin", &frame.origin)?;
    finite("rail across", &frame.across)?;
    finite("rail out", &frame.out)?;
    let out = normalize(frame.out);
    if out == [0.0; 3] {
        return Err(GenError::InvalidInput("rail out is zero".into()));
    }
    let across = normalize(sub(frame.across, mul(out, dot(frame.across, out))));
    if across == [0.0; 3] {
        return Err(GenError::InvalidInput("rail across is parallel to out".into()));
    }
    Ok((across, out))
}

fn open_ring(profile: &[[f32; 2]]) -> Result<Vec<[f32; 2]>, GenError> {
    if profile.len() < 3 {
        return Err(GenError::InvalidInput(
            "a sweep profile needs at least 3 points".into(),
        ));
    }
    let mut ring = profile.to_vec();
    if ring.first() == ring.last() {
        ring.pop();
    }
    if ring.len() < 3 {
        return Err(GenError::InvalidInput(
            "a sweep profile needs at least 3 points".into(),
        ));
    }
    finite("profile", ring.as_flattened())?;
    Ok(ring)
}

fn expand_arris(ring: &[[f32; 2]], band: f32) -> Vec<[f32; 2]> {
    if band == 0.0 {
        return ring.to_vec();
    }
    let n = ring.len();
    let mut out = Vec::with_capacity(n * 3);
    for i in 0..n {
        let a = ring[i];
        let b = ring[(i + 1) % n];
        out.push(a);
        let dx = b[0] - a[0];
        let dy = b[1] - a[1];
        let len = (dx * dx + dy * dy).sqrt();
        if len > band * 2.0 {
            let t = band / len;
            out.push([a[0] + dx * t, a[1] + dy * t]);
            out.push([b[0] - dx * t, b[1] - dy * t]);
        }
    }
    out
}

fn emit_tube(sink: &mut Sink, centers: &[[f32; 3]], options: &TubeOptions) {
    let tangents = path_tangents(centers);
    let mut normal = perp_to(tangents[0]);
    let mut frames = Vec::with_capacity(centers.len());
    for tangent in &tangents {
        normal = transport(normal, *tangent);
        let binormal = normalize(cross(*tangent, normal));
        frames.push((normal, binormal));
    }
    let sides = options.sides;
    let mut ids = Vec::with_capacity(centers.len());
    for (index, center) in centers.iter().enumerate() {
        let (n, b) = frames[index];
        let v = index as f32 / (centers.len() - 1) as f32 * options.tile;
        let first = sink.len();
        for k in 0..=sides {
            let (s, c) = circle_cs(k, sides);
            let radial = add(mul(n, c), mul(b, s));
            let pos = add(*center, mul(radial, options.radius));
            let u = k as f32 / sides as f32 * options.tile;
            sink.vertex(pos, normalize(radial), [u, v]);
        }
        ids.push(first);
    }
    for i in 0..centers.len() - 1 {
        let (n, b) = frames[i];
        for k in 0..sides {
            let (s, c) = circle_cs(k, sides);
            let hint = normalize(add(mul(n, c), mul(b, s)));
            let a0 = ids[i] + k;
            let a1 = ids[i] + k + 1;
            let b0 = ids[i + 1] + k;
            let b1 = ids[i + 1] + k + 1;
            sink.tri_out(a0, a1, b1, hint);
            sink.tri_out(a0, b1, b0, hint);
        }
    }
}

fn emit_annulus(sink: &mut Sink, frame: &Frame, options: &AnnulusOptions) {
    let normal = map_dir(frame, [0.0, 0.0, 1.0]);
    let sides = options.sides;
    if options.inner == 0.0 {
        let center = sink.vertex(frame.at(0.0, 0.0, 0.0), normal, [0.5, 0.5]);
        let mut ring = Vec::with_capacity(sides as usize + 1);
        for k in 0..=sides {
            let (s, c) = circle_cs(k, sides);
            let pos = frame.at(options.outer * c, options.outer * s, 0.0);
            let u = 0.5 + 0.5 * c;
            let v = 0.5 + 0.5 * s;
            ring.push(sink.vertex(pos, normal, [u, v]));
        }
        for k in 0..sides {
            sink.tri_out(center, ring[k as usize], ring[k as usize + 1], normal);
        }
        return;
    }
    let mut inner = Vec::with_capacity(sides as usize + 1);
    let mut outer = Vec::with_capacity(sides as usize + 1);
    for k in 0..=sides {
        let (s, c) = circle_cs(k, sides);
        let u = k as f32 / sides as f32 * options.tile;
        inner.push(sink.vertex(
            frame.at(options.inner * c, options.inner * s, 0.0),
            normal,
            [u, 0.0],
        ));
        outer.push(sink.vertex(
            frame.at(options.outer * c, options.outer * s, 0.0),
            normal,
            [u, 1.0],
        ));
    }
    for k in 0..sides as usize {
        sink.tri_out(inner[k], outer[k], outer[k + 1], normal);
        sink.tri_out(inner[k], outer[k + 1], inner[k + 1], normal);
    }
}

struct CylSpec {
    radius: f32,
    length: f32,
    sides: u32,
    knurl: f32,
    chamfer: f32,
    cap: bool,
    bottom_cap: bool,
    tile: f32,
}

fn cylinder_spec(frame: &Frame, options: &CylinderOptions) -> Result<CylSpec, GenError> {
    check_frame(frame)?;
    finite(
        "cylinder",
        &[
            options.radius,
            options.length,
            options.knurl,
            options.chamfer,
            options.tile,
        ],
    )?;
    if options.radius <= 0.0 || options.length <= 0.0 {
        return Err(GenError::InvalidInput(
            "cylinder radius and length must be > 0".into(),
        ));
    }
    if !(0.0..1.0).contains(&options.knurl) {
        return Err(GenError::InvalidInput("knurl must be in [0, 1)".into()));
    }
    if options.chamfer < 0.0 {
        return Err(GenError::InvalidInput("chamfer must be >= 0".into()));
    }
    check_count("sides", options.sides, 3)?;
    let limit = (options.length * 0.5).min(options.radius * 0.5);
    let chamfer = options.chamfer.min(limit);
    Ok(CylSpec {
        radius: options.radius,
        length: options.length,
        sides: options.sides,
        knurl: options.knurl,
        chamfer,
        cap: options.cap,
        bottom_cap: options.bottom_cap,
        tile: options.tile,
    })
}

#[derive(Clone, Copy)]
enum RingShade {
    Radial,
    Chamfer,
}

struct Corner {
    pos: [f32; 3],
    normal: [f32; 3],
    uv: [f32; 2],
}

fn smooth_cylinder(sink: &mut Sink, frame: &Frame, spec: &CylSpec) {
    let shoulder = spec.length - spec.chamfer;
    let bottom = make_ring(frame, spec, 0.0, 1.0, RingShade::Radial);
    let top_wall = make_ring(frame, spec, shoulder, 1.0, RingShade::Radial);
    push_band(sink, &bottom, &top_wall);
    let lip = if spec.chamfer > 0.0 {
        let factor = 1.0 - spec.chamfer / spec.radius;
        let chamfer_a = make_ring(frame, spec, shoulder, 1.0, RingShade::Chamfer);
        let chamfer_b = make_ring(frame, spec, spec.length, factor, RingShade::Chamfer);
        push_band(sink, &chamfer_a, &chamfer_b);
        chamfer_b
    } else {
        top_wall
    };
    if spec.bottom_cap {
        push_cap(sink, frame, &bottom, 0.0, [0.0, 0.0, -1.0], false);
    }
    if spec.cap {
        push_cap(sink, frame, &lip, spec.length, [0.0, 0.0, 1.0], true);
    }
}

fn flat_cylinder(sink: &mut Sink, frame: &Frame, spec: &CylSpec) {
    let shoulder = spec.length - spec.chamfer;
    push_flat_band(
        sink,
        frame,
        spec,
        Band {
            z0: 0.0,
            z1: shoulder,
            factor0: 1.0,
            factor1: 1.0,
            chamfer: false,
        },
    );
    let factor = if spec.chamfer > 0.0 {
        let lip = 1.0 - spec.chamfer / spec.radius;
        push_flat_band(
            sink,
            frame,
            spec,
            Band {
                z0: shoulder,
                z1: spec.length,
                factor0: 1.0,
                factor1: lip,
                chamfer: true,
            },
        );
        lip
    } else {
        1.0
    };
    if spec.bottom_cap {
        let bottom = make_ring(frame, spec, 0.0, 1.0, RingShade::Radial);
        push_cap(sink, frame, &bottom, 0.0, [0.0, 0.0, -1.0], false);
    }
    if spec.cap {
        let lip = make_ring(frame, spec, spec.length, factor, RingShade::Radial);
        push_cap(sink, frame, &lip, spec.length, [0.0, 0.0, 1.0], true);
    }
}

fn make_ring(frame: &Frame, spec: &CylSpec, z: f32, factor: f32, shade: RingShade) -> Vec<Corner> {
    let mut ring = Vec::with_capacity(spec.sides as usize + 1);
    let v = z / spec.length * spec.tile;
    for k in 0..=spec.sides {
        let (s, c) = circle_cs(k, spec.sides);
        let radius = facet_radius(k, spec.sides, spec.radius, spec.knurl) * factor;
        let local_n = match shade {
            RingShade::Radial => [c, s, 0.0],
            RingShade::Chamfer => normalize([c, s, 1.0]),
        };
        ring.push(Corner {
            pos: frame.at(radius * c, radius * s, z),
            normal: map_dir(frame, local_n),
            uv: [k as f32 / spec.sides as f32 * spec.tile, v],
        });
    }
    ring
}

fn push_band(sink: &mut Sink, a: &[Corner], b: &[Corner]) {
    let ia = push_corners(sink, a);
    let ib = push_corners(sink, b);
    let sides = a.len() - 1;
    for (k, corner) in a.iter().enumerate().take(sides) {
        let hint = corner.normal;
        let k = k as u32;
        sink.tri_out(ia + k, ia + k + 1, ib + k + 1, hint);
        sink.tri_out(ia + k, ib + k + 1, ib + k, hint);
    }
}

fn push_corners(sink: &mut Sink, ring: &[Corner]) -> u32 {
    let first = sink.len();
    for corner in ring {
        sink.vertex(corner.pos, corner.normal, corner.uv);
    }
    first
}

fn push_cap(sink: &mut Sink, frame: &Frame, ring: &[Corner], z: f32, outward: [f32; 3], top: bool) {
    let normal = map_dir(frame, outward);
    let center = sink.vertex(frame.at(0.0, 0.0, z), normal, [0.5, 0.5]);
    let first = sink.len();
    for (k, corner) in ring.iter().enumerate() {
        let (s, c) = circle_cs(k as u32, (ring.len() - 1) as u32);
        sink.vertex(corner.pos, normal, [0.5 + 0.5 * c, 0.5 + 0.5 * s]);
    }
    let sides = ring.len() - 1;
    for k in 0..sides as u32 {
        if top {
            sink.tri_out(center, first + k, first + k + 1, normal);
        } else {
            sink.tri_out(center, first + k + 1, first + k, normal);
        }
    }
}

struct Band {
    z0: f32,
    z1: f32,
    factor0: f32,
    factor1: f32,
    chamfer: bool,
}

fn push_flat_band(sink: &mut Sink, frame: &Frame, spec: &CylSpec, band: Band) {
    let v0 = band.z0 / spec.length * spec.tile;
    let v1 = band.z1 / spec.length * spec.tile;
    for k in 0..spec.sides {
        let (s0, c0) = circle_cs(k, spec.sides);
        let (s1, c1) = circle_cs(k + 1, spec.sides);
        let r00 = facet_radius(k, spec.sides, spec.radius, spec.knurl) * band.factor0;
        let r10 = facet_radius(k + 1, spec.sides, spec.radius, spec.knurl) * band.factor0;
        let r01 = facet_radius(k, spec.sides, spec.radius, spec.knurl) * band.factor1;
        let r11 = facet_radius(k + 1, spec.sides, spec.radius, spec.knurl) * band.factor1;
        let p00 = frame.at(r00 * c0, r00 * s0, band.z0);
        let p10 = frame.at(r10 * c1, r10 * s1, band.z0);
        let p11 = frame.at(r11 * c1, r11 * s1, band.z1);
        let p01 = frame.at(r01 * c0, r01 * s0, band.z1);
        let local_hint = if band.chamfer {
            normalize([c0, s0, 1.0])
        } else {
            [c0, s0, 0.0]
        };
        let hint = map_dir(frame, local_hint);
        let normal = face_normal4(p00, p10, p01, hint);
        let u0 = k as f32 / spec.sides as f32 * spec.tile;
        let u1 = (k + 1) as f32 / spec.sides as f32 * spec.tile;
        let i00 = sink.vertex(p00, normal, [u0, v0]);
        let i10 = sink.vertex(p10, normal, [u1, v0]);
        let i11 = sink.vertex(p11, normal, [u1, v1]);
        let i01 = sink.vertex(p01, normal, [u0, v1]);
        sink.tri_out(i00, i10, i11, hint);
        sink.tri_out(i00, i11, i01, hint);
    }
}

fn facet_radius(k: u32, sides: u32, radius: f32, knurl: f32) -> f32 {
    let k = if k == sides { 0 } else { k };
    if knurl == 0.0 || k.is_multiple_of(2) {
        radius
    } else {
        radius * (1.0 - knurl)
    }
}

#[derive(Clone, Copy)]
struct Face {
    fixed: usize,
    sign: f32,
    u_axis: usize,
    v_axis: usize,
    flip: bool,
}

const FACES: [Face; 6] = [
    Face {
        fixed: 0,
        sign: 1.0,
        u_axis: 1,
        v_axis: 2,
        flip: false,
    },
    Face {
        fixed: 0,
        sign: -1.0,
        u_axis: 1,
        v_axis: 2,
        flip: true,
    },
    Face {
        fixed: 1,
        sign: 1.0,
        u_axis: 2,
        v_axis: 0,
        flip: false,
    },
    Face {
        fixed: 1,
        sign: -1.0,
        u_axis: 0,
        v_axis: 2,
        flip: false,
    },
    Face {
        fixed: 2,
        sign: 1.0,
        u_axis: 0,
        v_axis: 1,
        flip: false,
    },
    Face {
        fixed: 2,
        sign: -1.0,
        u_axis: 0,
        v_axis: 1,
        flip: true,
    },
];

fn emit_face(
    sink: &mut Sink,
    frame: &Frame,
    face: Face,
    half: [f32; 3],
    radius: f32,
    segments: u32,
    tile: f32,
) {
    let n = segments as usize;
    let mut ids = vec![0u32; (n + 1) * (n + 1)];
    let face_n = {
        let mut nrm = [0.0f32; 3];
        nrm[face.fixed] = face.sign;
        nrm
    };
    let hint = map_dir(frame, face_n);
    for j in 0..=n {
        for i in 0..=n {
            let local = face_point(face, half, i as u32, j as u32, segments);
            let (surf, nrm) = round_point(local, half, radius, face_n);
            let u = i as f32 / segments as f32 * tile;
            let v = j as f32 / segments as f32 * tile;
            ids[j * (n + 1) + i] =
                sink.vertex(frame.at(surf[0], surf[1], surf[2]), map_dir(frame, nrm), [u, v]);
        }
    }
    for j in 0..n {
        for i in 0..n {
            let i00 = ids[j * (n + 1) + i];
            let i10 = ids[j * (n + 1) + i + 1];
            let i11 = ids[(j + 1) * (n + 1) + i + 1];
            let i01 = ids[(j + 1) * (n + 1) + i];
            if face.flip {
                sink.tri_out(i00, i01, i11, hint);
                sink.tri_out(i00, i11, i10, hint);
            } else {
                sink.tri_out(i00, i10, i11, hint);
                sink.tri_out(i00, i11, i01, hint);
            }
        }
    }
}

fn face_point(face: Face, half: [f32; 3], i: u32, j: u32, segments: u32) -> [f32; 3] {
    let mut p = [0.0; 3];
    p[face.fixed] = face.sign * half[face.fixed];
    p[face.u_axis] = axis_coord(half[face.u_axis], i, segments);
    p[face.v_axis] = axis_coord(half[face.v_axis], j, segments);
    p
}

fn axis_coord(half: f32, index: u32, segments: u32) -> f32 {
    -half + 2.0 * half * (index as f32 / segments as f32)
}

fn round_point(p: [f32; 3], half: [f32; 3], radius: f32, face_n: [f32; 3]) -> ([f32; 3], [f32; 3]) {
    if radius == 0.0 {
        return (p, face_n);
    }
    let inner = [half[0] - radius, half[1] - radius, half[2] - radius];
    let c = [
        p[0].clamp(-inner[0], inner[0]),
        p[1].clamp(-inner[1], inner[1]),
        p[2].clamp(-inner[2], inner[2]),
    ];
    let n = normalize(sub(p, c));
    if n == [0.0; 3] {
        return (p, face_n);
    }
    (add(c, mul(n, radius)), n)
}

fn catmull_stations(points: &[[f32; 3]], steps: u32) -> Result<Vec<[f32; 3]>, GenError> {
    if points.len() < 2 {
        return Err(GenError::InvalidInput("a tube needs at least two points".into()));
    }
    finite("tube points", points.as_flattened())?;
    for pair in points.windows(2) {
        if pair[0] == pair[1] {
            return Err(GenError::InvalidInput("a tube span has zero length".into()));
        }
    }
    let mut centers = Vec::new();
    for i in 0..points.len() - 1 {
        let p0 = if i == 0 { points[0] } else { points[i - 1] };
        let p3 = if i + 2 < points.len() {
            points[i + 2]
        } else {
            points[points.len() - 1]
        };
        let j0 = if i == 0 { 0 } else { 1 };
        for j in j0..=steps {
            let t = j as f32 / steps as f32;
            centers.push(catmull(p0, points[i], points[i + 1], p3, t));
        }
    }
    Ok(centers)
}

fn catmull(p0: [f32; 3], p1: [f32; 3], p2: [f32; 3], p3: [f32; 3], t: f32) -> [f32; 3] {
    let t2 = t * t;
    let t3 = t2 * t;
    let mut out = [0.0; 3];
    for k in 0..3 {
        let a = p0[k];
        let b = p1[k];
        let c = p2[k];
        let d = p3[k];
        let term = 2.0 * b
            + (c - a) * t
            + (2.0 * a - 5.0 * b + 4.0 * c - d) * t2
            + (-a + 3.0 * b - 3.0 * c + d) * t3;
        out[k] = 0.5 * term;
    }
    out
}

fn linear_stations(points: &[[f32; 3]], steps: u32) -> Result<Vec<[f32; 3]>, GenError> {
    if points.len() < 2 {
        return Err(GenError::InvalidInput("a sweep needs at least two points".into()));
    }
    finite("sweep points", points.as_flattened())?;
    for pair in points.windows(2) {
        if pair[0] == pair[1] {
            return Err(GenError::InvalidInput("a sweep span has zero length".into()));
        }
    }
    let mut out = Vec::new();
    for pair in points.windows(2) {
        let j0 = if out.is_empty() { 0 } else { 1 };
        for j in j0..=steps {
            let t = j as f32 / steps as f32;
            out.push(lerp3(pair[0], pair[1], t));
        }
    }
    Ok(out)
}

fn station_tangent(stations: &[[f32; 3]], index: usize) -> Result<[f32; 3], GenError> {
    let d = if index + 1 < stations.len() {
        sub(stations[index + 1], stations[index])
    } else {
        sub(stations[index], stations[index - 1])
    };
    let t = normalize(d);
    if t == [0.0; 3] {
        Err(GenError::InvalidInput(
            "a sweep station has a zero tangent".into(),
        ))
    } else {
        Ok(t)
    }
}

fn section_axes(tangent: [f32; 3], out: [f32; 3]) -> Result<([f32; 3], [f32; 3]), GenError> {
    let up = normalize(sub(out, mul(tangent, dot(out, tangent))));
    if up == [0.0; 3] {
        return Err(GenError::InvalidInput("sweep out is parallel to the path".into()));
    }
    let across = normalize(cross(up, tangent));
    Ok((across, up))
}

fn profile_normals(profile: &[[f32; 2]]) -> Vec<[f32; 2]> {
    let n = profile.len();
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let prev = profile[(i + n - 1) % n];
        let p = profile[i];
        let next = profile[(i + 1) % n];
        let e0 = [p[0] - prev[0], p[1] - prev[1]];
        let e1 = [next[0] - p[0], next[1] - p[1]];
        let n0 = normalize2([e0[1], -e0[0]]);
        let n1 = normalize2([e1[1], -e1[0]]);
        let summed = [n0[0] + n1[0], n0[1] + n1[1]];
        let m = normalize2(summed);
        out.push(if m == [0.0, 0.0] { n0 } else { m });
    }
    out
}

fn path_tangents(centers: &[[f32; 3]]) -> Vec<[f32; 3]> {
    let mut out = Vec::with_capacity(centers.len());
    for i in 0..centers.len() {
        let d = if i + 1 < centers.len() {
            sub(centers[i + 1], centers[i])
        } else {
            sub(centers[i], centers[i - 1])
        };
        let mut t = normalize(d);
        if t == [0.0; 3] {
            t = if i == 0 { [0.0, 0.0, 1.0] } else { out[i - 1] };
        }
        out.push(t);
    }
    out
}

fn perp_to(tangent: [f32; 3]) -> [f32; 3] {
    let hint = if tangent[1].abs() < 0.9 {
        [0.0, 1.0, 0.0]
    } else {
        [1.0, 0.0, 0.0]
    };
    let n = normalize(cross(hint, tangent));
    if n == [0.0; 3] { [1.0, 0.0, 0.0] } else { n }
}

fn transport(prev: [f32; 3], tangent: [f32; 3]) -> [f32; 3] {
    let n = normalize(sub(prev, mul(tangent, dot(prev, tangent))));
    if n == [0.0; 3] { perp_to(tangent) } else { n }
}

fn circle_cs(k: u32, sides: u32) -> (f32, f32) {
    if k == 0 || k == sides {
        (0.0, 1.0)
    } else {
        det::sin_cos(std::f32::consts::TAU * (k as f32 / sides as f32))
    }
}

fn face_normal4(a: [f32; 3], b: [f32; 3], d: [f32; 3], hint: [f32; 3]) -> [f32; 3] {
    let mut n = normalize(cross(sub(b, a), sub(d, a)));
    if n == [0.0; 3] {
        return hint;
    }
    if dot(n, hint) < 0.0 {
        n = mul(n, -1.0);
    }
    n
}

fn map_dir(frame: &Frame, d: [f32; 3]) -> [f32; 3] {
    let w = frame.dir(d[0], d[1], d[2]);
    let n = normalize(w);
    if n == [0.0; 3] { w } else { n }
}

#[derive(Default)]
struct Sink {
    mesh: CpuMesh,
    color: Option<[f32; 4]>,
}

impl Sink {
    fn len(&self) -> u32 {
        self.mesh.positions.len() as u32
    }

    fn vertex(&mut self, p: [f32; 3], n: [f32; 3], uv: [f32; 2]) -> u32 {
        let id = self.len();
        self.mesh.positions.push(p);
        self.mesh.normals.push(n);
        self.mesh.uvs.push(uv);
        if let Some(color) = self.color {
            self.mesh.colors.push(color);
        }
        id
    }

    fn tri_out(&mut self, a: u32, b: u32, c: u32, outward: [f32; 3]) {
        let Some(pa) = self.mesh.positions.get(a as usize).copied() else {
            return;
        };
        let Some(pb) = self.mesh.positions.get(b as usize).copied() else {
            return;
        };
        let Some(pc) = self.mesh.positions.get(c as usize).copied() else {
            return;
        };
        if weld_key(pa) == weld_key(pb) || weld_key(pb) == weld_key(pc) || weld_key(pa) == weld_key(pc) {
            return;
        }
        let n = cross(sub(pb, pa), sub(pc, pa));
        if dot(n, n) == 0.0 {
            return;
        }
        if dot(n, outward) < 0.0 {
            self.mesh.indices.extend([a, c, b]);
        } else {
            self.mesh.indices.extend([a, b, c]);
        }
    }

    fn finish(self) -> CpuMesh {
        self.mesh
    }
}

fn check_frame(frame: &Frame) -> Result<(), GenError> {
    finite("frame origin", &frame.origin)?;
    finite("frame right", &frame.right)?;
    finite("frame up", &frame.up)?;
    finite("frame forward", &frame.forward)?;
    for (name, axis) in [
        ("right", frame.right),
        ("up", frame.up),
        ("forward", frame.forward),
    ] {
        if dot(axis, axis) == 0.0 {
            return Err(GenError::InvalidInput(format!("frame {name} is zero")));
        }
    }
    Ok(())
}

fn check_tile(tile: f32) -> Result<(), GenError> {
    finite("tile", &[tile])
}

fn check_count(what: &str, value: u32, min: u32) -> Result<(), GenError> {
    if value < min || value > MAX_SEGMENTS {
        Err(GenError::InvalidInput(format!(
            "{what} must be in {min}..={MAX_SEGMENTS}"
        )))
    } else {
        Ok(())
    }
}

fn mul(v: [f32; 3], s: f32) -> [f32; 3] {
    [v[0] * s, v[1] * s, v[2] * s]
}

fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

fn normalize2(v: [f32; 2]) -> [f32; 2] {
    let len = (v[0] * v[0] + v[1] * v[1]).sqrt();
    if len > 0.0 && len.is_finite() {
        [v[0] / len, v[1] / len]
    } else {
        [0.0; 2]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh_stats;

    #[test]
    fn rounded_box_normals_point_out_and_stay_in_bounds() {
        let half = [0.1f32, 0.05, 0.02];
        let mesh = rounded_box(&Frame::xyz(), half, 0.008, 6, 1.0).expect("box");
        assert_eq!(mesh.positions.len(), mesh.normals.len());
        for (p, n) in mesh.positions.iter().zip(&mesh.normals) {
            for k in 0..3 {
                assert!(p[k].abs() <= half[k] + 1.0e-4, "position {p:?} outside {half:?}");
            }
            let outward = p[0] * n[0] + p[1] * n[1] + p[2] * n[2];
            assert!(outward > 0.0, "normal {n:?} at {p:?} points inward");
        }
        let stats = mesh_stats(&mesh);
        assert!(
            stats.watertight,
            "boundary {} nonmanifold {} degenerate {}",
            stats.boundary_edges, stats.non_manifold_edges, stats.degenerate_triangles
        );
        assert!(stats.volume > 0.0, "volume {}", stats.volume);
        assert!(stats.volume < 8.0 * 0.1 * 0.05 * 0.02);
        let again = rounded_box(&Frame::xyz(), half, 0.008, 6, 1.0).expect("again");
        assert_eq!(mesh_stats(&mesh).hash, mesh_stats(&again).hash);
    }

    #[test]
    fn cylinder_chamfer_reduces_the_top_ring() {
        let mesh = cylinder(
            &Frame::xyz(),
            &CylinderOptions {
                radius: 1.0,
                length: 2.0,
                sides: 16,
                chamfer: 0.2,
                cap: true,
                ..CylinderOptions::default()
            },
        )
        .expect("chamfer");
        let lip: Vec<f32> = mesh
            .positions
            .iter()
            .filter(|p| p[2] > 1.99)
            .map(|p| (p[0] * p[0] + p[1] * p[1]).sqrt())
            .filter(|r| *r > 0.1)
            .collect();
        assert!(!lip.is_empty());
        assert!(lip.iter().all(|r| (r - 0.8).abs() < 1.0e-3), "{lip:?}");
        // The wall is two rings (base and shoulder), not a stack of intermediate samples.
        let radial = |p: &[f32; 3]| (p[0] * p[0] + p[1] * p[1]).sqrt();
        let on_wall = |z: f32| {
            mesh.positions
                .iter()
                .any(|p| (p[2] - z).abs() < 1.0e-3 && (radial(p) - 1.0).abs() < 1.0e-3)
        };
        assert!(on_wall(0.0), "the base ring keeps the full radius");
        assert!(on_wall(1.8), "the shoulder ring keeps the full radius");
    }

    #[test]
    fn cylinder_knurl_alternates_radii() {
        let mesh = cylinder(
            &Frame::xyz(),
            &CylinderOptions {
                radius: 1.0,
                length: 1.0,
                sides: 8,
                knurl: 0.3,
                cap: false,
                ..CylinderOptions::default()
            },
        )
        .expect("knurl");
        let radii: Vec<f32> = mesh
            .positions
            .iter()
            .map(|p| (p[0] * p[0] + p[1] * p[1]).sqrt())
            .collect();
        assert!(radii.iter().any(|r| (r - 1.0).abs() < 1.0e-3), "{radii:?}");
        assert!(radii.iter().any(|r| (r - 0.7).abs() < 1.0e-3), "{radii:?}");
        assert!(
            radii
                .iter()
                .all(|r| (r - 1.0).abs() < 1.0e-3 || (r - 0.7).abs() < 1.0e-3),
            "{radii:?}"
        );
    }

    #[test]
    fn cylinder_both_caps_are_watertight() {
        let mesh = cylinder(
            &Frame::xyz(),
            &CylinderOptions {
                radius: 0.5,
                length: 1.0,
                sides: 12,
                cap: true,
                bottom_cap: true,
                ..CylinderOptions::default()
            },
        )
        .expect("capped");
        let stats = mesh_stats(&mesh);
        assert!(
            stats.watertight,
            "boundary {} nonmanifold {}",
            stats.boundary_edges, stats.non_manifold_edges
        );
        assert!(stats.volume > 0.0, "{}", stats.volume);
    }

    #[test]
    fn identity_frame_runs_the_cylinder_along_engine_forward() {
        let mesh = cylinder(
            &Frame::identity(),
            &CylinderOptions {
                radius: 0.25,
                length: 2.0,
                sides: 8,
                cap: true,
                ..CylinderOptions::default()
            },
        )
        .expect("identity");
        assert!(
            mesh.positions
                .iter()
                .any(|p| { (p[2] + 2.0).abs() < 1.0e-4 && p[0].abs() < 1.0e-4 && p[1].abs() < 1.0e-4 })
        );
    }

    #[test]
    fn straight_tube_keeps_its_radius() {
        let mesh = tube(
            &[[0.0, 0.0, 0.0], [0.0, 0.0, 4.0]],
            &TubeOptions {
                radius: 0.25,
                sides: 12,
                steps_per_segment: 4,
                tile: 1.0,
            },
        )
        .expect("tube");
        for p in &mesh.positions {
            let radius = (p[0] * p[0] + p[1] * p[1]).sqrt();
            assert!((radius - 0.25).abs() < 1.0e-4, "{p:?} radius {radius}");
            assert!((-1.0e-4..=4.0 + 1.0e-4).contains(&p[2]), "{p:?}");
        }
    }

    #[test]
    fn sweep_along_a_straight_segment_is_a_rounded_tube() {
        let mesh = sweep_rounded_rect(
            &[[0.0, 0.0, 0.0], [0.0, 0.0, 2.0]],
            &SweepRect {
                half: [0.5, 0.5],
                corner: 0.5,
                shift: 0.0,
                out: [0.0, 1.0, 0.0],
                corner_segments: 4,
                steps: 2,
                tile: 1.0,
            },
        )
        .expect("sweep");
        assert!(!mesh.positions.is_empty());
        for p in &mesh.positions {
            let radius = (p[0] * p[0] + p[1] * p[1]).sqrt();
            assert!((radius - 0.5).abs() < 1.0e-4, "{p:?} radius {radius}");
            assert!((-1.0e-4..=2.0 + 1.0e-4).contains(&p[2]), "{p:?}");
        }
    }

    #[test]
    fn section_sweep_wear_colors_match_the_stations() {
        let rail = [
            RailFrame {
                origin: [0.0, 0.0, 0.0],
                across: [1.0, 0.0, 0.0],
                out: [0.0, 1.0, 0.0],
            },
            RailFrame {
                origin: [0.0, 0.0, 1.0],
                across: [1.0, 0.0, 0.0],
                out: [0.0, 1.0, 0.0],
            },
        ];
        let profile = [[-0.2, -0.2], [0.2, -0.2], [0.2, 0.2], [-0.2, 0.2]];
        let mesh = sweep_section(&rail, &profile, 0.0015, Some(&[0.0, 0.25])).expect("section");
        assert_eq!(mesh.colors.len(), mesh.positions.len());
        assert!(mesh.colors.iter().any(|c| (c[0] - 1.0).abs() < 1.0e-6));
        assert!(mesh.colors.iter().any(|c| (c[0] - 0.75).abs() < 1.0e-6));
        assert!(mesh.tangents.is_empty());
    }

    #[test]
    fn primitives_reject_non_finite_input() {
        let err = rounded_box(&Frame::xyz(), [1.0, f32::NAN, 1.0], 0.1, 2, 1.0).expect_err("nan");
        assert_eq!(err.code(), "gen_invalid_input");
        let err = cylinder(
            &Frame::xyz(),
            &CylinderOptions {
                knurl: 1.0,
                ..CylinderOptions::default()
            },
        )
        .expect_err("knurl");
        assert_eq!(err.code(), "gen_invalid_input");
        let err = tube(&[[0.0, 0.0, 0.0]], &TubeOptions::default()).expect_err("short");
        assert_eq!(err.code(), "gen_invalid_input");
    }
}
