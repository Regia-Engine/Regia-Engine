// SPDX-License-Identifier: MPL-2.0
//! Per-vertex scalar overlay: skin-weight heatmaps, deformation heatmaps and damage brushes
//! (Tincal engine request F10).
//!
//! A [`ScalarOverlay`] component on a mesh entity colours the mesh by one `f32` per vertex through
//! a [`Ramp`], drawn *unlit over the lit frame* at [`ScalarOverlay::opacity`]. The material, the
//! lighting and the shared GPU mesh are untouched: the overlay is its own pass
//! (`scalar_overlay_pass`), recorded after the post stack next to the gizmos, so the
//! heatmap colours are exactly the ramp colours (no exposure, no tonemap, no bloom). Skinned
//! meshes follow their joint palette; every entity sharing one mesh `Arc` (instanced batches)
//! draws its own overlay or none.
//!
//! - **Values.** `values[i]` belongs to vertex `i` of the entity's mesh. A length that differs
//!   from the vertex count ([`ScalarOverlay::fit`]) draws nothing and is counted in
//!   [`ScalarOverlayStats::mismatched`]; it never panics and never reads out of range. NaN reads
//!   as `0.0` and infinities as ±1e30 (clamped by the range like any other value).
//! - **Range.** [`ScalarOverlay::range`] maps `value` to a ramp position with
//!   [`ramp_position`]: linear, clamped to 0–1. A reversed range flips the ramp; an empty range
//!   (`lo == hi`) is a step at that value.
//! - **Ramps.** A [`Ramp`] is data: sorted colour stops in display-referred sRGB. [`Ramp::viridis`]
//!   and [`Ramp::diverging`] ship built in; [`Ramp::from_stops`] takes any other.
//! - **Cost.** The values upload once per distinct `Arc` (an edit is a new `Arc`); steady frames
//!   create no GPU buffers, bind groups or pipelines. Meshes without an overlay cost nothing.
//! - **Brushes.** [`VertexAdjacency`] turns a pick hit (triangle, barycentric weights) into the
//!   vertices within a surface distance ([`VertexAdjacency::neighborhood`]) and [`apply_brush`]
//!   writes new values for them.
//!
//! Extraction: [`extract_scalar_overlays`] (a `Render` system after [`crate::extract_frame`])
//! fills [`ExtractedScalarOverlays`] with the object keys of the frame's extracted meshes. A host
//! that renders an [`crate::ExtractedFrame`] itself passes [`ExtractedScalarOverlays::entries`] to
//! [`crate::FrameRenderer::set_scalar_overlays`] every frame (an empty iterator clears them).
//! The ramp stops and the brush distance transform are Regia Engine-original (`docs/provenance.md`).

use std::collections::BinaryHeap;
use std::sync::Arc;

use regia_app::prelude::*;

use crate::LabVertex;
use crate::runtime::{ExtractedFrame, ExtractedMesh};

/// Entries in the ramp lookup table the shader samples (linear interpolation between them).
pub const RAMP_LUT_SIZE: usize = 64;

/// One colour stop of a [`Ramp`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RampStop {
    /// Position along the ramp, 0–1.
    pub at: f32,
    /// Display-referred sRGB colour, 0–1 per channel.
    pub color: [f32; 3],
}

/// Why [`Ramp::from_stops`] refused a stop list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RampError {
    /// A ramp needs at least two stops.
    TooFewStops,
    /// A position or a colour channel is NaN or infinite.
    NonFinite,
}

impl std::fmt::Display for RampError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooFewStops => f.write_str("a ramp needs at least two stops"),
            Self::NonFinite => f.write_str("ramp stops must be finite numbers"),
        }
    }
}

impl std::error::Error for RampError {}

/// Names [`Ramp::from_name`] accepts.
pub const RAMP_NAMES: [&str; 3] = ["viridis", "diverging", "grayscale"];

/// A colour ramp as data: stops sorted by position, linearly interpolated in sRGB.
#[derive(Debug, Clone, PartialEq)]
pub struct Ramp {
    stops: Arc<[RampStop]>,
}

/// The ten-colour viridis listing (matplotlib's perceptually uniform map, CC0).
const VIRIDIS: [[f32; 3]; 10] = [
    [0.267, 0.005, 0.329],
    [0.282, 0.157, 0.471],
    [0.243, 0.287, 0.537],
    [0.192, 0.408, 0.557],
    [0.149, 0.510, 0.557],
    [0.122, 0.620, 0.537],
    [0.208, 0.718, 0.475],
    [0.431, 0.808, 0.345],
    [0.710, 0.871, 0.169],
    [0.993, 0.906, 0.144],
];

/// Blue, white, red: zero (the midpoint) is neutral, the two signs read apart.
const DIVERGING: [[f32; 3]; 7] = [
    [0.082, 0.231, 0.569],
    [0.231, 0.451, 0.784],
    [0.553, 0.718, 0.914],
    [0.965, 0.965, 0.965],
    [0.953, 0.659, 0.545],
    [0.827, 0.325, 0.255],
    [0.616, 0.063, 0.110],
];

impl Ramp {
    /// Evenly spaced stops through `colors`.
    fn even(colors: &[[f32; 3]]) -> Self {
        let last = (colors.len() - 1) as f32;
        let stops = colors
            .iter()
            .enumerate()
            .map(|(i, &color)| RampStop {
                at: i as f32 / last,
                color,
            })
            .collect();
        Self { stops }
    }

    /// Viridis-like, dark purple to yellow: weights, magnitudes and error maps.
    #[must_use]
    pub fn viridis() -> Self {
        Self::even(&VIRIDIS)
    }

    /// Diverging blue–white–red: signed quantities, with zero at the middle of the range.
    #[must_use]
    pub fn diverging() -> Self {
        Self::even(&DIVERGING)
    }

    /// Black to white.
    #[must_use]
    pub fn grayscale() -> Self {
        Self::even(&[[0.0; 3], [1.0; 3]])
    }

    /// A built-in ramp by name ([`RAMP_NAMES`]).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "viridis" => Some(Self::viridis()),
            "diverging" => Some(Self::diverging()),
            "grayscale" => Some(Self::grayscale()),
            _ => None,
        }
    }

    /// A ramp through `stops`: sorted by position, positions and channels clamped to 0–1.
    ///
    /// # Errors
    ///
    /// Fewer than two stops, or a non-finite number.
    pub fn from_stops(stops: impl IntoIterator<Item = RampStop>) -> Result<Self, RampError> {
        let mut stops: Vec<RampStop> = stops.into_iter().collect();
        if stops.len() < 2 {
            return Err(RampError::TooFewStops);
        }
        if stops
            .iter()
            .any(|s| !s.at.is_finite() || s.color.iter().any(|c| !c.is_finite()))
        {
            return Err(RampError::NonFinite);
        }
        for stop in &mut stops {
            stop.at = stop.at.clamp(0.0, 1.0);
            stop.color = stop.color.map(|c| c.clamp(0.0, 1.0));
        }
        stops.sort_by(|a, b| a.at.total_cmp(&b.at));
        Ok(Self { stops: stops.into() })
    }

    /// The stops, sorted by position.
    #[must_use]
    pub fn stops(&self) -> &[RampStop] {
        &self.stops
    }

    /// The colour at `t` (0–1, clamped; NaN reads as 0).
    #[must_use]
    pub fn sample(&self, t: f32) -> [f32; 3] {
        let t = if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) };
        let stops = &self.stops;
        let next = stops.partition_point(|s| s.at <= t);
        if next == 0 {
            return stops[0].color;
        }
        if next >= stops.len() {
            return stops[stops.len() - 1].color;
        }
        let (a, b) = (stops[next - 1], stops[next]);
        let span = b.at - a.at;
        let k = if span > 0.0 { (t - a.at) / span } else { 1.0 };
        [0, 1, 2].map(|c| a.color[c] + (b.color[c] - a.color[c]) * k)
    }

    /// The table the shader samples: [`RAMP_LUT_SIZE`] evenly spaced colours (w unused).
    #[must_use]
    pub fn lut(&self) -> [[f32; 4]; RAMP_LUT_SIZE] {
        std::array::from_fn(|i| {
            let [r, g, b] = self.sample(i as f32 / (RAMP_LUT_SIZE - 1) as f32);
            [r, g, b, 1.0]
        })
    }
}

impl Default for Ramp {
    fn default() -> Self {
        Self::viridis()
    }
}

/// The ramp position of `value` in `range` (0–1). Linear and clamped; a reversed range flips the
/// ramp; an empty range is a step at its value; NaN reads as `0.0` before mapping. The fragment
/// shader computes the same function.
#[must_use]
pub fn ramp_position(value: f32, range: (f32, f32)) -> f32 {
    let value = sanitize(value);
    let (lo, hi) = range;
    if hi == lo {
        return if value >= hi { 1.0 } else { 0.0 };
    }
    let t = (value - lo) / (hi - lo);
    if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) }
}

/// The value the GPU gets: NaN reads as 0, infinities as ±1e30.
#[must_use]
pub(crate) fn sanitize(value: f32) -> f32 {
    if value.is_nan() {
        0.0
    } else {
        value.clamp(-1.0e30, 1.0e30)
    }
}

/// How a [`ScalarOverlay`]'s values line up with a mesh's vertices ([`ScalarOverlay::fit`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayFit {
    /// One value per vertex: the overlay draws.
    Exact,
    /// Fewer values than vertices: nothing draws.
    Short {
        /// Values supplied.
        values: usize,
        /// Vertices the mesh has.
        vertices: usize,
    },
    /// More values than vertices: nothing draws.
    Long {
        /// Values supplied.
        values: usize,
        /// Vertices the mesh has.
        vertices: usize,
    },
}

/// Colours a mesh entity's vertices by a scalar each, unlit over the lit frame (see the module
/// docs). Applies to its own entity only.
#[derive(Component, Debug, Clone, PartialEq)]
pub struct ScalarOverlay {
    /// One value per vertex of the entity's mesh, in vertex order. Editing is a new `Arc`.
    pub values: Arc<[f32]>,
    /// Colours along the range.
    pub ramp: Ramp,
    /// Values mapped to the start and the end of the ramp (clamped outside).
    pub range: (f32, f32),
    /// Blend over the lit frame, 0–1 (NaN reads as 0).
    pub opacity: f32,
}

impl ScalarOverlay {
    /// `values` through viridis over 0–1, opaque.
    #[must_use]
    pub fn new(values: impl Into<Arc<[f32]>>) -> Self {
        Self {
            values: values.into(),
            ramp: Ramp::viridis(),
            range: (0.0, 1.0),
            opacity: 1.0,
        }
    }

    /// With `ramp`.
    #[must_use]
    pub fn with_ramp(mut self, ramp: Ramp) -> Self {
        self.ramp = ramp;
        self
    }

    /// With `range`.
    #[must_use]
    pub fn with_range(mut self, lo: f32, hi: f32) -> Self {
        self.range = (lo, hi);
        self
    }

    /// With `opacity`.
    #[must_use]
    pub fn with_opacity(mut self, opacity: f32) -> Self {
        self.opacity = opacity;
        self
    }

    /// The opacity the renderer uses: 0–1, NaN as 0.
    #[must_use]
    pub fn effective_opacity(&self) -> f32 {
        if self.opacity.is_nan() {
            0.0
        } else {
            self.opacity.clamp(0.0, 1.0)
        }
    }

    /// The colour `value` shows at full opacity.
    #[must_use]
    pub fn color_of(&self, value: f32) -> [f32; 3] {
        self.ramp.sample(ramp_position(value, self.range))
    }

    /// How the values line up with a mesh of `vertex_count` vertices.
    #[must_use]
    pub fn fit(&self, vertex_count: usize) -> OverlayFit {
        let values = self.values.len();
        match values.cmp(&vertex_count) {
            std::cmp::Ordering::Equal => OverlayFit::Exact,
            std::cmp::Ordering::Less => OverlayFit::Short {
                values,
                vertices: vertex_count,
            },
            std::cmp::Ordering::Greater => OverlayFit::Long {
                values,
                vertices: vertex_count,
            },
        }
    }
}

/// What the overlay pass did last frame ([`crate::FrameRenderer::scalar_overlay_stats`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScalarOverlayStats {
    /// Overlays submitted this frame whose mesh was drawn (possibly culled by the GPU).
    pub drawn: usize,
    /// Overlays skipped because their values did not match the vertex count.
    pub mismatched: usize,
    /// Value buffers uploaded in total (once per distinct values `Arc` per entity).
    pub value_uploads: u64,
    /// Mesh geometry copies uploaded in total (once per mesh key).
    pub geometry_uploads: u64,
}

/// This frame's overlays by object key ([`crate::FrameMesh::object_key`]), filled by
/// [`extract_scalar_overlays`].
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct ExtractedScalarOverlays {
    /// `(object key, overlay)` of every extracted mesh whose entity carries one.
    pub entries: Vec<(u64, ScalarOverlay)>,
}

impl ExtractedScalarOverlays {
    /// The overlays of `meshes`, each looked up with `find` (entity → overlay).
    pub fn collect(meshes: &[ExtractedMesh], find: impl Fn(Entity) -> Option<ScalarOverlay>) -> Self {
        let entries = meshes
            .iter()
            .filter_map(|mesh| find(mesh.entity).map(|o| (object_key(mesh), o)))
            .collect();
        Self { entries }
    }

    /// The overlays of `meshes` in `world`.
    #[must_use]
    pub fn from_world(world: &World, meshes: &[ExtractedMesh]) -> Self {
        // No `ScalarOverlay` was ever inserted: nothing to look up.
        if world.component_id::<ScalarOverlay>().is_none() {
            return Self::default();
        }
        Self::collect(meshes, |entity| world.get::<ScalarOverlay>(entity).cloned())
    }
}

/// The object key [`crate::ExtractedFrame::frame_scene`] gives an extracted mesh.
fn object_key(mesh: &ExtractedMesh) -> u64 {
    mesh.entity.to_bits() ^ mesh.object_key_xor
}

/// `Render`: fills [`ExtractedScalarOverlays`] for the frame [`crate::extract_frame`] just
/// extracted.
pub fn extract_scalar_overlays(
    extracted: Res<ExtractedFrame>,
    overlays: Query<&ScalarOverlay>,
    mut out: ResMut<ExtractedScalarOverlays>,
) {
    if overlays.is_empty() {
        if !out.entries.is_empty() {
            out.entries.clear();
        }
        return;
    }
    let _span = tracing::info_span!("extract_scalar_overlays", meshes = extracted.meshes.len()).entered();
    *out = ExtractedScalarOverlays::collect(&extracted.meshes, |entity| overlays.get(entity).ok().cloned());
}

// ---------------------------------------------------------------------------
// Brushes
// ---------------------------------------------------------------------------

/// Where a brush lands: a triangle of the mesh and the barycentric weights inside it. The field
/// names match the editor's `PickHit` (`indices`, `barycentric`), so a pick converts field by field.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BrushSeed {
    /// The triangle's three vertex indices.
    pub indices: [u32; 3],
    /// Weights of those vertices at the hit (sum 1).
    pub barycentric: [f32; 3],
}

/// Vertex-to-vertex edges of a triangle list, for brush neighborhoods. Build it once per mesh and
/// reuse it for every stroke.
#[derive(Debug, Clone, Default)]
pub struct VertexAdjacency {
    /// `offsets[v]..offsets[v + 1]` indexes `neighbors`.
    offsets: Vec<u32>,
    neighbors: Vec<u32>,
}

impl VertexAdjacency {
    /// The edges of `indices` (triangles) over `vertex_count` vertices. Triangles with an index
    /// out of range are skipped.
    #[must_use]
    pub fn new(indices: &[u32], vertex_count: usize) -> Self {
        let mut edges: Vec<(u32, u32)> = Vec::with_capacity(indices.len() * 2);
        for tri in indices.as_chunks::<3>().0 {
            if tri.iter().any(|&i| i as usize >= vertex_count) {
                continue;
            }
            for (a, b) in [(tri[0], tri[1]), (tri[1], tri[2]), (tri[2], tri[0])] {
                if a != b {
                    edges.push((a, b));
                    edges.push((b, a));
                }
            }
        }
        edges.sort_unstable();
        edges.dedup();
        let mut offsets = vec![0u32; vertex_count + 1];
        for &(a, _) in &edges {
            offsets[a as usize + 1] += 1;
        }
        for i in 0..vertex_count {
            offsets[i + 1] += offsets[i];
        }
        let neighbors = edges.into_iter().map(|(_, b)| b).collect();
        Self { offsets, neighbors }
    }

    /// Vertices the adjacency was built over.
    #[must_use]
    pub fn vertex_count(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    fn neighbors_of(&self, vertex: u32) -> &[u32] {
        let v = vertex as usize;
        match (self.offsets.get(v), self.offsets.get(v + 1)) {
            (Some(&a), Some(&b)) => &self.neighbors[a as usize..b as usize],
            _ => &[],
        }
    }

    /// Every vertex within `radius` of the seed point along the mesh surface (shortest path over
    /// edges, object-space metres), with its distance, nearest first. The seed point is the
    /// barycentric combination of the seed triangle's corners. Empty when the seed is out of
    /// range, the radius is not positive, or `vertices` is not the mesh the adjacency was built
    /// for. Flood fill follows edges, so a thin gap between two surfaces does not bleed across.
    #[must_use]
    pub fn neighborhood(&self, vertices: &[LabVertex], seed: &BrushSeed, radius: f32) -> Vec<(u32, f32)> {
        if radius.is_nan() || radius <= 0.0 || vertices.len() != self.vertex_count() {
            return Vec::new();
        }
        let position = |v: u32| vertices.get(v as usize).map(|v| v.position);
        let corners = seed.indices.map(position);
        let [Some(a), Some(b), Some(c)] = corners else {
            return Vec::new();
        };
        let w = seed.barycentric;
        let point = [0, 1, 2].map(|k| a[k] * w[0] + b[k] * w[1] + c[k] * w[2]);
        let mut best = vec![f32::INFINITY; vertices.len()];
        let mut heap = BinaryHeap::new();
        for (&vertex, corner) in seed.indices.iter().zip([a, b, c]) {
            let d = distance(point, corner);
            if d <= radius && d < best[vertex as usize] {
                best[vertex as usize] = d;
                heap.push(HeapItem { distance: d, vertex });
            }
        }
        while let Some(HeapItem { distance: d, vertex }) = heap.pop() {
            if d > best[vertex as usize] {
                continue;
            }
            let here = vertices[vertex as usize].position;
            for &next in self.neighbors_of(vertex) {
                let nd = d + distance(here, vertices[next as usize].position);
                if nd <= radius && nd < best[next as usize] {
                    best[next as usize] = nd;
                    heap.push(HeapItem {
                        distance: nd,
                        vertex: next,
                    });
                }
            }
        }
        let mut found: Vec<(u32, f32)> = best
            .iter()
            .enumerate()
            .filter(|(_, d)| d.is_finite())
            .map(|(v, &d)| (v as u32, d))
            .collect();
        found.sort_by(|x, y| x.1.total_cmp(&y.1).then(x.0.cmp(&y.0)));
        found
    }
}

fn distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

/// Min-heap entry (`BinaryHeap` is a max-heap).
struct HeapItem {
    distance: f32,
    vertex: u32,
}

impl PartialEq for HeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}
impl Eq for HeapItem {}
impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other
            .distance
            .total_cmp(&self.distance)
            .then(other.vertex.cmp(&self.vertex))
    }
}

/// How a brush weight falls off from the stroke centre to its radius.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BrushFalloff {
    /// Full strength inside the radius.
    Hard,
    /// Linear to zero at the radius.
    Linear,
    /// Smoothstep to zero at the radius.
    #[default]
    Smooth,
}

impl BrushFalloff {
    /// The weight 0–1 at `distance` of a brush of `radius`.
    #[must_use]
    pub fn weight(self, distance: f32, radius: f32) -> f32 {
        if radius.is_nan() || radius <= 0.0 || distance > radius || distance.is_nan() {
            return 0.0;
        }
        let x = 1.0 - (distance / radius).clamp(0.0, 1.0);
        match self {
            Self::Hard => 1.0,
            Self::Linear => x,
            Self::Smooth => x * x * (3.0 - 2.0 * x),
        }
    }
}

/// What a stroke does to a vertex of weight `w` and `strength`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BrushMode {
    /// `value += strength * w` (accumulate: damage).
    #[default]
    Add,
    /// Blend toward `strength` by `w` (paint a level).
    Set,
    /// `value = max(value, strength * w)` (never lowers).
    Max,
}

/// `values` with a stroke applied to the vertices of `neighborhood`
/// ([`VertexAdjacency::neighborhood`]): the new `Arc` to put in [`ScalarOverlay::values`]. Vertices
/// outside the stroke keep their value; neighborhood entries out of range are ignored.
#[must_use]
pub fn apply_brush(
    values: &[f32],
    neighborhood: &[(u32, f32)],
    radius: f32,
    falloff: BrushFalloff,
    mode: BrushMode,
    strength: f32,
) -> Arc<[f32]> {
    let mut out = values.to_vec();
    for &(vertex, d) in neighborhood {
        let Some(value) = out.get_mut(vertex as usize) else {
            continue;
        };
        let w = falloff.weight(d, radius);
        match mode {
            BrushMode::Add => *value += strength * w,
            BrushMode::Set => *value += (strength - *value) * w,
            BrushMode::Max => *value = value.max(strength * w),
        }
    }
    out.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vertex(x: f32, y: f32) -> LabVertex {
        LabVertex::from_pos_normal_uv([x, y, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0])
    }

    /// A strip of quads along X: vertices `(i, 0)` and `(i, 1)` for `i in 0..n`.
    fn strip(n: u32) -> (Vec<LabVertex>, Vec<u32>) {
        let mut vertices = Vec::new();
        for i in 0..n {
            vertices.push(vertex(i as f32, 0.0));
            vertices.push(vertex(i as f32, 1.0));
        }
        let mut indices = Vec::new();
        for i in 0..n - 1 {
            let (a, b, c, d) = (2 * i, 2 * i + 1, 2 * i + 2, 2 * i + 3);
            indices.extend([a, c, b, b, c, d]);
        }
        (vertices, indices)
    }

    #[test]
    fn range_clamps_and_reverses() {
        assert_eq!(ramp_position(-5.0, (0.0, 1.0)), 0.0);
        assert_eq!(ramp_position(0.25, (0.0, 1.0)), 0.25);
        assert_eq!(ramp_position(7.0, (0.0, 1.0)), 1.0);
        assert_eq!(ramp_position(0.0, (-1.0, 1.0)), 0.5);
        assert_eq!(
            ramp_position(0.0, (1.0, 0.0)),
            1.0,
            "a reversed range flips the ramp"
        );
        assert!((ramp_position(0.9, (1.0, 0.0)) - 0.1).abs() < 1e-6);
        assert_eq!(
            ramp_position(0.5, (0.5, 0.5)),
            1.0,
            "an empty range steps at its value"
        );
        assert_eq!(ramp_position(0.4, (0.5, 0.5)), 0.0);
        assert_eq!(ramp_position(f32::NAN, (0.0, 1.0)), 0.0);
        assert_eq!(ramp_position(f32::INFINITY, (0.0, 1.0)), 1.0);
        assert_eq!(ramp_position(f32::NEG_INFINITY, (0.0, 1.0)), 0.0);
        assert_eq!(
            ramp_position(0.5, (f32::NAN, 1.0)),
            0.0,
            "a NaN range draws the low end"
        );
    }

    #[test]
    fn built_in_ramps_cover_their_ends_and_clamp() {
        let viridis = Ramp::viridis();
        assert_eq!(viridis.sample(0.0), VIRIDIS[0]);
        assert_eq!(viridis.sample(1.0), VIRIDIS[9]);
        assert_eq!(viridis.sample(-3.0), VIRIDIS[0]);
        assert_eq!(viridis.sample(9.0), VIRIDIS[9]);
        assert_eq!(viridis.sample(f32::NAN), VIRIDIS[0]);
        let mid = viridis.sample(0.5);
        assert!(
            (mid[0] - (VIRIDIS[4][0] + VIRIDIS[5][0]) / 2.0).abs() < 1e-5,
            "{mid:?}"
        );
        let diverging = Ramp::diverging();
        assert_eq!(diverging.sample(0.5), DIVERGING[3], "the midpoint is neutral");
        let (low, high) = (diverging.sample(0.0), diverging.sample(1.0));
        assert!(low[2] > low[0] && high[0] > high[2], "blue below, red above");
        assert_eq!(Ramp::from_name("viridis"), Some(viridis));
        assert_eq!(Ramp::from_name("nope"), None);
        for name in RAMP_NAMES {
            assert!(Ramp::from_name(name).is_some(), "{name}");
        }
        assert_eq!(Ramp::default().lut()[RAMP_LUT_SIZE - 1][..3], VIRIDIS[9]);
    }

    #[test]
    fn custom_ramps_are_sorted_clamped_and_validated() {
        let ramp = Ramp::from_stops([
            RampStop {
                at: 2.0,
                color: [0.0, 0.0, 2.0],
            },
            RampStop {
                at: -1.0,
                color: [1.0, 0.0, 0.0],
            },
        ])
        .expect("two stops");
        assert_eq!(ramp.stops()[0].at, 0.0);
        assert_eq!(ramp.stops()[1].at, 1.0);
        assert_eq!(ramp.stops()[1].color, [0.0, 0.0, 1.0]);
        assert_eq!(ramp.sample(0.5), [0.5, 0.0, 0.5]);
        assert_eq!(
            Ramp::from_stops([RampStop {
                at: 0.0,
                color: [0.0; 3]
            }]),
            Err(RampError::TooFewStops)
        );
        assert_eq!(
            Ramp::from_stops([
                RampStop {
                    at: 0.0,
                    color: [f32::NAN, 0.0, 0.0]
                },
                RampStop {
                    at: 1.0,
                    color: [0.0; 3]
                },
            ]),
            Err(RampError::NonFinite)
        );
    }

    #[test]
    fn length_mismatch_is_reported_not_fatal() {
        let overlay = ScalarOverlay::new(vec![0.0; 4]);
        assert_eq!(overlay.fit(4), OverlayFit::Exact);
        assert_eq!(
            overlay.fit(6),
            OverlayFit::Short {
                values: 4,
                vertices: 6
            }
        );
        assert_eq!(
            overlay.fit(2),
            OverlayFit::Long {
                values: 4,
                vertices: 2
            }
        );
        assert_eq!(ScalarOverlay::new(Vec::new()).fit(0), OverlayFit::Exact);
        assert_eq!(
            ScalarOverlay::new(vec![1.0])
                .with_opacity(f32::NAN)
                .effective_opacity(),
            0.0
        );
        assert_eq!(
            ScalarOverlay::new(vec![1.0])
                .with_opacity(3.0)
                .effective_opacity(),
            1.0
        );
    }

    #[test]
    fn neighborhood_follows_the_surface_within_the_radius() {
        let (vertices, indices) = strip(6);
        let adjacency = VertexAdjacency::new(&indices, vertices.len());
        assert_eq!(adjacency.vertex_count(), 12);
        // Seed exactly on vertex 4 (x = 2, y = 0) of the first triangle that has it.
        let seed = BrushSeed {
            indices: [4, 6, 5],
            barycentric: [1.0, 0.0, 0.0],
        };
        let hood = adjacency.neighborhood(&vertices, &seed, 1.05);
        let ids: Vec<u32> = hood.iter().map(|h| h.0).collect();
        assert_eq!(hood[0], (4, 0.0));
        for expected in [4, 5, 2, 6] {
            assert!(ids.contains(&expected), "{expected} in {ids:?}");
        }
        assert!(!ids.contains(&0) && !ids.contains(&8), "{ids:?}");
        assert!(hood.iter().all(|h| h.1 <= 1.05));
        assert!(hood.windows(2).all(|w| w[0].1 <= w[1].1), "nearest first");
        assert!(!ids.contains(&3), "the diagonal corner is sqrt 2 away");
        // A wider brush reaches it (vertex 3 shares a triangle edge with vertex 4); vertex 7 is two edges away.
        let wide = adjacency.neighborhood(&vertices, &seed, 1.5);
        assert!(wide.iter().any(|h| h.0 == 3));
        assert!(!wide.iter().any(|h| h.0 == 7));
        // Degenerate input yields nothing and never panics.
        assert!(adjacency.neighborhood(&vertices, &seed, 0.0).is_empty());
        assert!(adjacency.neighborhood(&vertices, &seed, f32::NAN).is_empty());
        let bad = BrushSeed {
            indices: [4, 6, 99],
            ..seed
        };
        assert!(adjacency.neighborhood(&vertices, &bad, 1.0).is_empty());
        assert!(adjacency.neighborhood(&vertices[..4], &seed, 1.0).is_empty());
    }

    #[test]
    fn neighborhood_does_not_cross_a_gap_between_surfaces() {
        // Two triangles 0.1 apart with no shared edge: a brush on one leaves the other alone.
        let mut vertices: Vec<LabVertex> = [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0)]
            .iter()
            .map(|&(x, y)| vertex(x, y))
            .collect();
        vertices.extend([(0.0, 0.1), (1.0, 0.1), (0.0, 1.1)].iter().map(|&(x, y)| {
            let mut v = vertex(x, y);
            v.position[2] = 0.1;
            v
        }));
        let indices = [0, 1, 2, 3, 4, 5];
        let adjacency = VertexAdjacency::new(&indices, 6);
        let seed = BrushSeed {
            indices: [0, 1, 2],
            barycentric: [1.0, 0.0, 0.0],
        };
        let ids: Vec<u32> = adjacency
            .neighborhood(&vertices, &seed, 5.0)
            .iter()
            .map(|h| h.0)
            .collect();
        assert_eq!(ids, [0, 1, 2]);
    }

    fn assert_close(got: &[f32], want: &[f32]) {
        assert_eq!(got.len(), want.len());
        for (g, w) in got.iter().zip(want) {
            assert!((g - w).abs() < 1e-5, "{got:?} vs {want:?}");
        }
    }

    #[test]
    fn brush_modes_and_falloff() {
        assert_eq!(BrushFalloff::Hard.weight(0.9, 1.0), 1.0);
        assert_eq!(BrushFalloff::Hard.weight(1.1, 1.0), 0.0);
        assert_eq!(BrushFalloff::Linear.weight(0.25, 1.0), 0.75);
        assert_eq!(BrushFalloff::Smooth.weight(0.0, 1.0), 1.0);
        assert_eq!(BrushFalloff::Smooth.weight(1.0, 1.0), 0.0);
        assert_eq!(BrushFalloff::Linear.weight(0.0, 0.0), 0.0);
        let hood = [(0, 0.0), (1, 0.5), (9, 0.0)];
        let values = [0.2, 0.2, 0.2];
        let added = apply_brush(&values, &hood, 1.0, BrushFalloff::Linear, BrushMode::Add, 1.0);
        assert_close(&added, &[1.2, 0.7, 0.2]);
        let set = apply_brush(&values, &hood, 1.0, BrushFalloff::Hard, BrushMode::Set, 0.8);
        assert_close(&set, &[0.8, 0.8, 0.2]);
        let max = apply_brush(
            &[0.9, 0.0, 0.0],
            &hood,
            1.0,
            BrushFalloff::Linear,
            BrushMode::Max,
            0.5,
        );
        assert_close(&max, &[0.9, 0.25, 0.0]);
        assert_eq!(&values, &[0.2, 0.2, 0.2], "the input is untouched");
    }
}
