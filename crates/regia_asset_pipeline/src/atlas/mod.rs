// SPDX-License-Identifier: MIT
// Portions ported from xatlas (https://github.com/jpcy/xatlas, commit f700c77), source/xatlas/xatlas.cpp
// and xatlas.h (the public option set and defaults, the chart / vertex remap output shape).
// Copyright (c) 2018-2020 Jonathan Young. xatlas is derived from thekla_atlas
// (https://github.com/Thekla/thekla_atlas), Copyright (c) 2013 Thekla, Inc and Copyright NVIDIA
// Corporation 2006 -- Ignacio Castano. MIT licence (see docs/ported-sources.toml):
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software
// and associated documentation files (the "Software"), to deal in the Software without restriction,
// including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all copies or substantial
// portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT
// LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
// NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
// WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
// SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

//! UV atlas generation for lightmap UVs (UV2): a Rust port of the xatlas chart, parametrization and
//! packing pipeline (ADR-1483, `book/agents/atlas.md`). Editor and pipeline side only; no runtime
//! crate may depend on it.
//!
//! [`generate`] runs three stages on one triangle mesh:
//!
//! 1. **Charts** (`segment`). Faces are welded by position into an edge-adjacent surface. Regions
//!    of connected coplanar faces whose every outer edge bends by 90° or more become planar charts
//!    as they are. The rest is clustered: seeds are planted, and charts grow one face (or coplanar
//!    region) at a time, cheapest first, by a weighted cost of normal deviation, roundness,
//!    straightness, normal seams and texture seams ([`AtlasOptions`] weights, `max_cost`); seeds
//!    then move to the most central, best-fitting face and growth restarts (`max_iterations`).
//! 2. **Parametrization** (`param`). A chart is projected onto its plane; if that projection has no
//!    flipped triangle, no self-crossing boundary and little stretch it is kept (orthogonal),
//!    otherwise a least squares conformal map (LSCM, Lévy et al. 2002) is solved with a Jacobi
//!    preconditioned conjugate gradient. A chart that still folds is split into smaller pieces that
//!    project cleanly (piecewise).
//! 3. **Packing** (`pack`). Every chart is scaled to a common texel density, turned to the axis of
//!    its minimum-area bounding box, rasterized conservatively into a bit image, grown by the
//!    bilinear footprint and dilated by the padding, then placed into a square bitmap of
//!    `resolution` texels where it does not touch any placed chart (optional 90° turns). With
//!    `texels_per_unit == 0` the largest density that fits is searched.
//!
//! The result is deterministic: no stage depends on thread timing or hash order, and the parallel
//! parts (parametrization, rasterization) compute each chart with the same sequential code whatever
//! [`AtlasOptions::threads`] is. Output vertices are split at chart seams; [`AtlasVertex::xref`]
//! names the source vertex.

mod mesh;
mod pack;
mod param;
mod segment;
#[cfg(test)]
mod tests;

use std::fmt;

/// Largest accepted [`AtlasOptions::resolution`].
pub const MAX_ATLAS_RESOLUTION: u32 = 16_384;
/// Largest accepted [`AtlasOptions::padding`].
pub const MAX_ATLAS_PADDING: u32 = 64;
/// Largest accepted [`AtlasOptions::min_chart_texels`].
pub const MAX_ATLAS_MIN_CHART_TEXELS: u32 = 64;

/// One triangle mesh to unwrap. Positions are in any unit; [`AtlasOptions::texels_per_unit`] is per
/// that unit.
#[derive(Clone, Copy, Debug)]
pub struct AtlasMesh<'a> {
    /// Vertex positions.
    pub positions: &'a [[f32; 3]],
    /// Triangle list (three indices per face).
    pub indices: &'a [u32],
    /// Optional vertex normals (same length as `positions`): a hint for normal seams. Without them
    /// a seam is where neighbouring faces' normals differ.
    pub normals: Option<&'a [[f32; 3]]>,
    /// Optional source texture coordinates (same length as `positions`): a hint for texture seams.
    pub uvs: Option<&'a [[f32; 2]]>,
}

impl<'a> AtlasMesh<'a> {
    /// A mesh from positions and indices only.
    #[must_use]
    pub fn new(positions: &'a [[f32; 3]], indices: &'a [u32]) -> Self {
        Self {
            positions,
            indices,
            normals: None,
            uvs: None,
        }
    }
}

/// Chart, parametrization and packing settings. The chart weights and defaults are xatlas's.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AtlasOptions {
    /// Weight of the angle between a face and its chart's average normal.
    pub normal_deviation_weight: f32,
    /// Weight of the chart's roundness (boundary length² over area) getting worse.
    pub roundness_weight: f32,
    /// Weight of the straightness metric (rewards faces that close gaps in the boundary).
    pub straightness_weight: f32,
    /// Weight of crossing a normal seam. Above 1000 normal seams are never crossed.
    pub normal_seam_weight: f32,
    /// Weight of crossing a texture seam (only with [`AtlasMesh::uvs`]).
    pub texture_seam_weight: f32,
    /// A face whose weighted cost exceeds this does not join a chart. Lower values give more charts.
    pub max_cost: f32,
    /// Seed relocation and regrowth rounds (0 keeps the greedy first charts).
    pub max_iterations: u32,
    /// Largest chart surface area (0 = no limit).
    pub max_chart_area: f32,
    /// Largest chart boundary length (0 = no limit).
    pub max_boundary_length: f32,
    /// Side of the square atlas, texels.
    pub resolution: u32,
    /// Texels per unit of surface. 0 searches the largest density that fits `resolution`; a fixed
    /// value that does not fit is reduced ([`AtlasWarning::DensityReduced`]).
    pub texels_per_unit: f32,
    /// Gutter texels added around every chart's footprint; two charts' footprints end up at least
    /// `2 * padding + 1` texels apart (8-neighbour distance).
    pub padding: u32,
    /// Smallest side of a chart, texels: a smaller chart is stretched to it, so a tiny prop still
    /// has more than one texel to hold its light. 1 leaves every chart at its density.
    pub min_chart_texels: u32,
    /// Grow each footprint by the texels a bilinear lookup inside the chart can touch.
    pub bilinear: bool,
    /// Allow 90° turns while packing.
    pub rotate_charts: bool,
    /// Turn every chart to the axis of its minimum-area bounding box before packing.
    pub rotate_charts_to_axis: bool,
    /// Try every position for every chart (slow, tighter). Otherwise a fixed-seed random search.
    pub brute_force: bool,
    /// Worker threads for parametrization and rasterization (0 = available parallelism). The
    /// result is identical for every value.
    pub threads: usize,
}

impl Default for AtlasOptions {
    fn default() -> Self {
        Self {
            normal_deviation_weight: 2.0,
            roundness_weight: 0.01,
            straightness_weight: 6.0,
            normal_seam_weight: 4.0,
            texture_seam_weight: 0.5,
            max_cost: 2.0,
            max_iterations: 1,
            max_chart_area: 0.0,
            max_boundary_length: 0.0,
            resolution: 1024,
            texels_per_unit: 0.0,
            padding: 2,
            min_chart_texels: 1,
            bilinear: true,
            rotate_charts: true,
            rotate_charts_to_axis: true,
            brute_force: false,
            threads: 0,
        }
    }
}

/// How a chart was flattened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChartKind {
    /// Coplanar faces, projected onto their plane.
    Planar,
    /// Projected onto the chart's average plane (little stretch, no fold).
    Ortho,
    /// Least squares conformal map.
    Lscm,
    /// A piece of a chart that neither projection nor LSCM could flatten without a fold.
    Piecewise,
}

impl ChartKind {
    /// Lowercase protocol name (`planar`, `ortho`, `lscm`, `piecewise`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Planar => "planar",
            Self::Ortho => "ortho",
            Self::Lscm => "lscm",
            Self::Piecewise => "piecewise",
        }
    }
}

/// One output vertex.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AtlasVertex {
    /// Atlas coordinate in `[0, 1]²` (x right, y down, over [`AtlasResult::width`] × `height`).
    pub uv: [f32; 2],
    /// Index of the source vertex this vertex came from.
    pub xref: u32,
    /// Chart index, `None` for the corners of skipped (degenerate) faces, whose `uv` is `[0, 0]`.
    pub chart: Option<u32>,
}

/// One packed chart.
#[derive(Clone, Debug, PartialEq)]
pub struct AtlasChart {
    /// Source face indices in the chart, in chart order.
    pub faces: Vec<u32>,
    /// How the chart was flattened.
    pub kind: ChartKind,
    /// Placed with a 90° turn.
    pub rotated: bool,
    /// Placed footprint `[x, y, width, height]` in texels, padding included.
    pub rect: [u32; 4],
}

/// A non-fatal problem; the atlas is still produced.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AtlasWarning {
    /// A face with zero area, a repeated (or co-located) corner or a non-finite normal was skipped.
    DegenerateTriangle {
        /// Source face index.
        face: u32,
    },
    /// The fixed [`AtlasOptions::texels_per_unit`] did not fit and was reduced.
    DensityReduced {
        /// The requested density.
        requested: f32,
        /// The density used.
        used: f32,
    },
}

impl AtlasWarning {
    /// Protocol code (`atlas_degenerate_triangle`, `atlas_density_reduced`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::DegenerateTriangle { .. } => "atlas_degenerate_triangle",
            Self::DensityReduced { .. } => "atlas_density_reduced",
        }
    }
}

impl fmt::Display for AtlasWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DegenerateTriangle { face } => write!(f, "face {face} is degenerate and was skipped"),
            Self::DensityReduced { requested, used } => write!(
                f,
                "texels_per_unit {requested} does not fit the atlas; used {used}"
            ),
        }
    }
}

/// Why no atlas was produced.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum AtlasError {
    /// The index count is not a multiple of three.
    #[error("index count {count} is not a multiple of 3")]
    IndexCount {
        /// The index count.
        count: usize,
    },
    /// An index names a vertex past the end of `positions`.
    #[error("face {face} uses vertex {index}, but there are {vertex_count} vertices")]
    IndexOutOfRange {
        /// Source face index.
        face: u32,
        /// The bad index.
        index: u32,
        /// Number of positions.
        vertex_count: usize,
    },
    /// A position has a NaN or infinite component.
    #[error("position {vertex} is not finite")]
    NonFinitePosition {
        /// Vertex index.
        vertex: u32,
    },
    /// `normals` or `uvs` is not as long as `positions`.
    #[error("{attribute} has {actual} entries, positions has {expected}")]
    AttributeLength {
        /// `normals` or `uvs`.
        attribute: &'static str,
        /// Number of positions.
        expected: usize,
        /// Number of entries given.
        actual: usize,
    },
    /// An option is out of range.
    #[error("option {option}: {reason}")]
    InvalidOption {
        /// The option's field name.
        option: &'static str,
        /// What is wrong with it.
        reason: String,
    },
    /// Every face is degenerate (or there are none).
    #[error("the mesh has no non-degenerate triangle")]
    NoTriangles,
    /// The charts do not fit the atlas even at the smallest density tried.
    #[error("the charts do not fit a {resolution}x{resolution} atlas")]
    DoesNotFit {
        /// [`AtlasOptions::resolution`].
        resolution: u32,
    },
}

impl AtlasError {
    /// Protocol code: `atlas_index_count`, `atlas_index_out_of_range`, `atlas_non_finite`,
    /// `atlas_attribute_length`, `atlas_invalid_option`, `atlas_no_triangles`, `atlas_does_not_fit`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::IndexCount { .. } => "atlas_index_count",
            Self::IndexOutOfRange { .. } => "atlas_index_out_of_range",
            Self::NonFinitePosition { .. } => "atlas_non_finite",
            Self::AttributeLength { .. } => "atlas_attribute_length",
            Self::InvalidOption { .. } => "atlas_invalid_option",
            Self::NoTriangles => "atlas_no_triangles",
            Self::DoesNotFit { .. } => "atlas_does_not_fit",
        }
    }
}

/// The unwrapped, packed mesh.
#[derive(Clone, Debug, PartialEq)]
pub struct AtlasResult {
    /// Atlas width, texels.
    pub width: u32,
    /// Atlas height, texels.
    pub height: u32,
    /// Output vertices (source vertices split at chart seams).
    pub vertices: Vec<AtlasVertex>,
    /// Output triangle list, one triangle per source face, in source order (skipped faces included,
    /// on chart-less vertices).
    pub indices: Vec<u32>,
    /// The packed charts.
    pub charts: Vec<AtlasChart>,
    /// The texel density used.
    pub texels_per_unit: f32,
    /// Share of the atlas covered by chart triangles (conservative footprint, no padding).
    pub utilization: f32,
    /// Packing passes run (density search steps).
    pub pack_passes: u32,
    /// Non-fatal problems.
    pub warnings: Vec<AtlasWarning>,
}

impl AtlasResult {
    /// Number of charts of `kind`.
    #[must_use]
    pub fn chart_count(&self, kind: ChartKind) -> usize {
        self.charts.iter().filter(|c| c.kind == kind).count()
    }
}

fn check_options(options: &AtlasOptions) -> Result<(), AtlasError> {
    let bad = |option: &'static str, reason: &str| {
        Err(AtlasError::InvalidOption {
            option,
            reason: reason.to_string(),
        })
    };
    if options.resolution < 8 || options.resolution > MAX_ATLAS_RESOLUTION {
        return bad("resolution", "must be in 8..=16384");
    }
    if options.padding > MAX_ATLAS_PADDING || options.padding * 2 + 2 >= options.resolution {
        return bad("padding", "must be at most 64 and leave room inside the atlas");
    }
    if options.min_chart_texels == 0 || options.min_chart_texels > MAX_ATLAS_MIN_CHART_TEXELS {
        return bad("min_chart_texels", "must be in 1..=64");
    }
    if !options.texels_per_unit.is_finite() || options.texels_per_unit < 0.0 {
        return bad("texels_per_unit", "must be finite and >= 0");
    }
    let weights = [
        ("normal_deviation_weight", options.normal_deviation_weight),
        ("roundness_weight", options.roundness_weight),
        ("straightness_weight", options.straightness_weight),
        ("normal_seam_weight", options.normal_seam_weight),
        ("texture_seam_weight", options.texture_seam_weight),
        ("max_chart_area", options.max_chart_area),
        ("max_boundary_length", options.max_boundary_length),
    ];
    for (name, value) in weights {
        if !value.is_finite() || value < 0.0 {
            return bad(name, "must be finite and >= 0");
        }
    }
    if !options.max_cost.is_finite() || options.max_cost <= 0.0 {
        return bad("max_cost", "must be finite and > 0");
    }
    if options.max_iterations > 64 {
        return bad("max_iterations", "must be at most 64");
    }
    Ok(())
}

/// Worker count for `threads` (0 = available parallelism), at least 1.
pub(crate) fn worker_count(threads: usize) -> usize {
    if threads == 0 {
        std::thread::available_parallelism().map_or(1, usize::from)
    } else {
        threads
    }
    .clamp(1, 256)
}

/// Runs `f(i)` for every `i < count` on up to `threads` workers and returns the results in index
/// order. Each result depends only on its index, so the output is the same for any worker count.
pub(crate) fn parallel_map<T: Send, F: Fn(usize) -> T + Sync>(count: usize, threads: usize, f: F) -> Vec<T> {
    let workers = threads.min(count).max(1);
    if workers == 1 {
        return (0..count).map(f).collect();
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let mut parts: Vec<Vec<(usize, T)>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut out = Vec::new();
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if i >= count {
                            break;
                        }
                        out.push((i, f(i)));
                    }
                    out
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| match h.join() {
                Ok(part) => part,
                Err(panic) => std::panic::resume_unwind(panic),
            })
            .collect()
    });
    let mut slots: Vec<Option<T>> = (0..count).map(|_| None).collect();
    for part in &mut parts {
        for (i, value) in part.drain(..) {
            slots[i] = Some(value);
        }
    }
    slots.into_iter().flatten().collect()
}

/// Unwraps and packs `mesh` (see the module docs).
///
/// # Errors
/// [`AtlasError`] when the input is malformed, an option is out of range, no face is usable or the
/// charts do not fit.
pub fn generate(mesh: &AtlasMesh<'_>, options: &AtlasOptions) -> Result<AtlasResult, AtlasError> {
    let _span = tracing::info_span!(
        "atlas.generate",
        vertices = mesh.positions.len(),
        faces = mesh.indices.len() / 3
    )
    .entered();
    let flattened = flatten(mesh, options)?;
    let threads = worker_count(options.threads);
    let mut warnings = flattened.warnings.clone();
    let packed = pack::pack(&flattened.flat, options, threads, &mut warnings)?;
    Ok(build_result(
        mesh,
        &flattened.prepared,
        &flattened.flat,
        packed,
        warnings,
        options,
    ))
}

/// A mesh charted and flattened ([`flatten`]): stages 1 and 2 of [`generate`], kept so several
/// densities or resolutions can be packed without charting again (`regia bake-lightmaps`' atlas
/// fit, ADR-2024).
pub struct FlattenedMesh<'a> {
    mesh: AtlasMesh<'a>,
    options: AtlasOptions,
    prepared: mesh::Prepared,
    flat: Vec<param::FlatChart>,
    warnings: Vec<AtlasWarning>,
}

impl FlattenedMesh<'_> {
    /// The charts' total surface area, in the unit of the positions squared.
    #[must_use]
    pub fn surface_area(&self) -> f64 {
        self.flat.iter().map(|c| c.surface_area).sum()
    }

    /// Packs at exactly `texels_per_unit` into a square of `resolution` texels. `None` when the
    /// charts do not fit: unlike [`generate`] the density is never lowered, so the caller's density
    /// rule stays in charge.
    ///
    /// # Errors
    ///
    /// [`AtlasError::InvalidOption`] for a resolution, density or padding out of range.
    pub fn pack_exact(
        &self,
        resolution: u32,
        texels_per_unit: f32,
        padding: u32,
    ) -> Result<Option<AtlasResult>, AtlasError> {
        let options = AtlasOptions {
            resolution,
            texels_per_unit,
            padding,
            ..self.options
        };
        check_options(&options)?;
        let threads = worker_count(options.threads);
        let Some(packed) = pack::pack_exact(&self.flat, &options, threads) else {
            return Ok(None);
        };
        Ok(Some(build_result(
            &self.mesh,
            &self.prepared,
            &self.flat,
            packed,
            self.warnings.clone(),
            &options,
        )))
    }
}

/// Charts and flattens `mesh`: stages 1 and 2 of [`generate`], with no packing.
///
/// # Errors
///
/// [`AtlasError`] when the input is malformed, an option is out of range or no face is usable.
pub fn flatten<'a>(mesh: &AtlasMesh<'a>, options: &AtlasOptions) -> Result<FlattenedMesh<'a>, AtlasError> {
    check_options(options)?;
    let threads = worker_count(options.threads);
    let mut warnings = Vec::new();
    let prepared = mesh::Prepared::new(mesh, &mut warnings)?;
    let charts = segment::compute_charts(&prepared, options);
    let flat = param::parameterize_all(&prepared, &charts, threads);
    Ok(FlattenedMesh {
        mesh: *mesh,
        options: *options,
        prepared,
        flat,
        warnings,
    })
}

/// Splits vertices at chart seams and writes the output buffers.
fn build_result(
    mesh: &AtlasMesh<'_>,
    prepared: &mesh::Prepared,
    flat: &[param::FlatChart],
    packed: pack::Packed,
    warnings: Vec<AtlasWarning>,
    options: &AtlasOptions,
) -> AtlasResult {
    let face_count = mesh.indices.len() / 3;
    let mut indices = vec![0u32; face_count * 3];
    let mut vertices = Vec::new();
    let mut charts = Vec::with_capacity(flat.len());
    let inv = 1.0 / f64::from(options.resolution);
    // Output vertex of a source vertex in the current chart (reset per chart via `touched`).
    let mut local_out = vec![u32::MAX; mesh.positions.len()];
    let mut touched = Vec::new();
    for (c, chart) in flat.iter().enumerate() {
        let placed = &packed.charts[c];
        for &v in &touched {
            local_out[v as usize] = u32::MAX;
        }
        touched.clear();
        let mut faces = Vec::with_capacity(chart.faces.len());
        for (local_face, &face) in chart.faces.iter().enumerate() {
            let source_face = prepared.face_source[face as usize] as usize;
            faces.push(source_face as u32);
            for corner in 0..3 {
                let source_vertex = prepared.faces[face as usize][corner];
                let mut out = local_out[source_vertex as usize];
                if out == u32::MAX {
                    let local = chart.local_faces[local_face][corner] as usize;
                    let uv = placed.uvs[local];
                    out = vertices.len() as u32;
                    vertices.push(AtlasVertex {
                        uv: [
                            (uv[0] * inv).clamp(0.0, 1.0) as f32,
                            (uv[1] * inv).clamp(0.0, 1.0) as f32,
                        ],
                        xref: source_vertex,
                        chart: Some(c as u32),
                    });
                    local_out[source_vertex as usize] = out;
                    touched.push(source_vertex);
                }
                indices[source_face * 3 + corner] = out;
            }
        }
        charts.push(AtlasChart {
            faces,
            kind: chart.kind,
            rotated: placed.rotated,
            rect: placed.rect,
        });
    }
    // Skipped faces: one chart-less vertex per source vertex.
    for &v in &touched {
        local_out[v as usize] = u32::MAX;
    }
    for &face in &prepared.skipped {
        for corner in 0..3 {
            let source_vertex = mesh.indices[face as usize * 3 + corner];
            let mut out = local_out[source_vertex as usize];
            if out == u32::MAX {
                out = vertices.len() as u32;
                vertices.push(AtlasVertex {
                    uv: [0.0, 0.0],
                    xref: source_vertex,
                    chart: None,
                });
                local_out[source_vertex as usize] = out;
            }
            indices[face as usize * 3 + corner] = out;
        }
    }
    AtlasResult {
        width: options.resolution,
        height: options.resolution,
        vertices,
        indices,
        charts,
        texels_per_unit: packed.texels_per_unit,
        utilization: packed.utilization,
        pack_passes: packed.passes,
        warnings,
    }
}
