// SPDX-License-Identifier: MPL-2.0
//! Procedural mesh helpers (spec 23 § 6, ADR-7240): extrude with bevel, lathe, boolean-lite plane
//! clipping and planar/box UV projection, all producing the engine's [`CpuMesh`].
//!
//! Every operation is deterministic to the bit on every platform: the code uses only `+ - * /`
//! and `sqrt` (which IEEE 754 defines exactly), the engine's platform-independent
//! [`regia_core::math::det::sin_cos`] for angles, fixed iteration orders, and no hash-map
//! iteration on output paths. [`mesh_hash`] pins the bytes in the tests.
//!
//! ```
//! use regia_gen::{Bevel, ExtrudeOptions, Profile, extrude, mesh_stats};
//!
//! // A 2 m × 1 m slab, 0.5 m thick, with a two-step bevel on its cap edges.
//! let profile = Profile::new(vec![[0.0, 0.0], [2.0, 0.0], [2.0, 1.0], [0.0, 1.0]]);
//! let options = ExtrudeOptions { depth: 0.5, bevel: Some(Bevel { width: 0.05, segments: 2 }) };
//! let mesh = extrude(&profile, &options)?;
//! let stats = mesh_stats(&mesh);
//! assert!(stats.watertight);
//! # Ok::<(), regia_gen::GenError>(())
//! ```
//!
//! The texture and heightfield half (ADR-7280): seeded value, gradient and cellular noise with fBm
//! or ridged octaves, tileable on request ([`noise_image`]); image masks ([`threshold`],
//! [`levels`], [`blend`], [`blur`]) over the R32F [`FloatImage`]; edge and cavity masks baked from
//! a mesh's dihedral angles into UV space ([`curvature_masks`]); an edge-wear and grime recipe
//! ([`wear_masks`]); and [`Heightfield`] tools (noise fill, terrace, flatten-to-pad, clamp,
//! normalize, thermal erosion, slope and normal maps, export to the terrain heightmap form and to
//! a mesh grid). Integer hashing and exact IEEE operations only, so the bytes match on every
//! platform too ([`image_hash`], [`Heightfield::hash`]).
//!
//! Rounded boxes, chamfered and knurled cylinders, annuli, tubes and sweeps ([`rounded_box`],
//! [`cylinder`], [`annulus`], [`tube`], [`sweep_rounded_rect`], [`sweep_section`]) and a
//! marching-squares contour helper ([`contours`], ADR-8044). Same determinism rules.
//!
//! Not built: a true boolean of two overlapping solids (only convex intersection and disjoint
//! merge are here).

mod build;
mod clip;
mod extrude;
mod lathe;
mod recipe;
mod triangulate;
mod uv;
// Texture and heightfield half (ADR-7280).
mod curvature;
mod heightfield;
mod heightfield_recipe;
mod image;
mod masks;
mod noise;
mod texture_recipe;
mod wear;
// Rounded primitives, sweeps and marching-squares contours (ADR-8044).
mod contour;
mod meshkit;

use std::collections::HashMap;

pub use build::{flat_normals, merge, scale, translate};
pub use clip::{Plane, clip, intersect_convex};
pub use extrude::{Bevel, ExtrudeOptions, Profile, extrude, rounded_rect};
pub use lathe::{LatheOptions, lathe};
pub use recipe::{Op, Recipe, RecipeLimits, Shape, build_recipe};
pub use regia_asset::CpuMesh;
pub use triangulate::triangulate;
pub use uv::{Axis, uv_box, uv_planar};
// Texture and heightfield half (ADR-7280).
pub use curvature::{CurvatureMasks, CurvatureOptions, MAX_DILATE, curvature_masks};
pub use heightfield::{
    Erosion, ErosionReport, Heightfield, MAX_EROSION_RATE, MAX_HEIGHTFIELD_EDGE, Pad, TerrainExport,
};
pub use heightfield_recipe::{
    HeightOp, HeightfieldLimits, HeightfieldRecipe, build_heightfield, heightfield_work,
};
pub use image::{
    FloatImage, ImageStats, MAX_IMAGE_PIXELS, image_hash, pack_rgba8, quantize_u8, quantize_u16, r16_hash,
    rgba8_hash,
};
pub use masks::{
    BlendMode, Levels, MAX_BLUR_PASSES, MAX_BLUR_RADIUS, blend, blur, invert, levels, threshold,
};
pub use noise::{Fractal, MAX_NOISE_CELLS, MAX_OCTAVES, NoiseKind, NoiseParams, NoiseSampler, noise_image};
pub use texture_recipe::{
    Layer, MaskOp, Source, TextureFormat, TextureLimits, TextureOutput, TextureRecipe, build_texture,
    texture_work,
};
pub use wear::{WearMasks, WearOptions, wear_masks};
// Rounded primitives, sweeps and marching-squares contours (ADR-8044).
pub use contour::{
    Contour, ContourOptions, MAX_CONTOUR_EDGE, MAX_CONTOUR_LEVELS, MAX_SMOOTH_PASSES, ScalarGrid, contours,
};
pub use meshkit::{
    AnnulusOptions, CylinderOptions, Frame, RailFrame, SweepRect, TubeOptions, annulus, cylinder,
    rounded_box, sweep_rounded_rect, sweep_section, tube,
};

/// The most points a profile may have, all rings together (holes merge into one ring, and ear
/// clipping is quadratic in the ring size).
pub const MAX_RING_POINTS: usize = 4096;

/// The most segments a lathe or bevel may ask for.
pub const MAX_SEGMENTS: u32 = 1024;

/// The most vertices one operation may produce.
pub const MAX_VERTICES: usize = 4_000_000;

/// Errors of the mesh helpers.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum GenError {
    /// An input is out of its documented range (a non-finite value, too few points, a zero
    /// plane normal, a zero-area ring).
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// The bevel inset turns an edge of the profile around: the width is larger than the
    /// profile's narrowest feature allows.
    #[error("bevel width {width} is too wide for the profile (edge {edge} of ring {ring} flips)")]
    BevelTooWide {
        /// The requested bevel width.
        width: f32,
        /// The ring (0 = outer, 1.. = holes in order).
        ring: usize,
        /// The edge index within the ring.
        edge: usize,
    },
    /// A clip's cut edges do not close into loops (the input mesh is open along the plane).
    #[error("the cut along the clip plane does not close into loops ({open_edges} open cut edges)")]
    CapNotClosed {
        /// Cut edges left over after chaining loops.
        open_edges: usize,
    },
    /// The request is larger than a documented limit.
    #[error("{what} is {value}, above the limit of {limit}")]
    TooLarge {
        /// What was counted.
        what: &'static str,
        /// The requested amount.
        value: usize,
        /// The limit.
        limit: usize,
    },
}

impl GenError {
    /// The stable error code the agent protocol reports (`gen.mesh`, `gen.contour`, `book/agents/gen-mesh.md`):
    /// `gen_invalid_input`, `gen_bevel_too_wide`, `gen_cap_not_closed` or `gen_too_large`.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidInput(_) => "gen_invalid_input",
            Self::BevelTooWide { .. } => "gen_bevel_too_wide",
            Self::CapNotClosed { .. } => "gen_cap_not_closed",
            Self::TooLarge { .. } => "gen_too_large",
        }
    }
}

/// Facts about a mesh: counts, closedness, bounds, UV range, volume and a byte hash.
#[derive(Clone, Debug, PartialEq)]
pub struct MeshStats {
    /// Vertex count.
    pub vertices: usize,
    /// Triangle count.
    pub triangles: usize,
    /// Every edge, after welding equal positions, is used exactly once in each direction and no
    /// triangle is degenerate: a closed, consistently oriented 2-manifold surface.
    pub watertight: bool,
    /// Directed edges with no opposite edge (holes in the surface).
    pub boundary_edges: usize,
    /// Directed edges used more than once (non-manifold or inconsistently wound).
    pub non_manifold_edges: usize,
    /// Triangles whose welded corners repeat (zero area by topology).
    pub degenerate_triangles: usize,
    /// Distinct positions (equal bits weld; `-0.0` welds with `0.0`).
    pub welded_positions: usize,
    /// Position bounds `(min, max)`, `None` for an empty mesh.
    pub bounds: Option<([f32; 3], [f32; 3])>,
    /// UV bounds `(min, max)`, `None` when the mesh has no UVs.
    pub uv_bounds: Option<([f32; 2], [f32; 2])>,
    /// Signed enclosed volume by the divergence theorem (positive when triangles face outward).
    /// Meaningful for watertight meshes only.
    pub volume: f64,
    /// [`mesh_hash`] of the mesh.
    pub hash: String,
}

/// The weld key of a position: its bits, with `-0.0` folded into `0.0`.
pub(crate) fn weld_key(p: [f32; 3]) -> [u32; 3] {
    p.map(|c| (c + 0.0).to_bits())
}

/// Counts, closedness, bounds, UV range, volume and hash of `mesh` ([`MeshStats`]).
pub fn mesh_stats(mesh: &CpuMesh) -> MeshStats {
    let mut ids: HashMap<[u32; 3], u32> = HashMap::new();
    let welded: Vec<u32> = mesh
        .positions
        .iter()
        .map(|p| {
            let next = ids.len() as u32;
            *ids.entry(weld_key(*p)).or_insert(next)
        })
        .collect();
    let mut edges: HashMap<(u32, u32), u32> = HashMap::new();
    let mut degenerate = 0;
    let mut volume = 0.0f64;
    for t in mesh.indices.as_chunks::<3>().0 {
        let w = [t[0], t[1], t[2]].map(|i| welded.get(i as usize).copied().unwrap_or(u32::MAX));
        if w[0] == w[1] || w[1] == w[2] || w[0] == w[2] || w.contains(&u32::MAX) {
            degenerate += 1;
            continue;
        }
        for k in 0..3 {
            *edges.entry((w[k], w[(k + 1) % 3])).or_insert(0) += 1;
        }
        let [a, b, c] = [t[0], t[1], t[2]].map(|i| mesh.positions[i as usize].map(f64::from));
        // a · (b × c) / 6
        volume += (a[0] * (b[1] * c[2] - b[2] * c[1])
            + a[1] * (b[2] * c[0] - b[0] * c[2])
            + a[2] * (b[0] * c[1] - b[1] * c[0]))
            / 6.0;
    }
    let mut boundary = 0;
    let mut non_manifold = 0;
    for (&(a, b), &n) in &edges {
        if n > 1 {
            non_manifold += 1;
        }
        if !edges.contains_key(&(b, a)) {
            boundary += 1;
        }
    }
    let bounds = (!mesh.positions.is_empty()).then(|| {
        let mut lo = [f32::INFINITY; 3];
        let mut hi = [f32::NEG_INFINITY; 3];
        for p in &mesh.positions {
            lo = std::array::from_fn(|k| lo[k].min(p[k]));
            hi = std::array::from_fn(|k| hi[k].max(p[k]));
        }
        (lo, hi)
    });
    let uv_bounds = (!mesh.uvs.is_empty()).then(|| {
        let mut lo = [f32::INFINITY; 2];
        let mut hi = [f32::NEG_INFINITY; 2];
        for p in &mesh.uvs {
            lo = std::array::from_fn(|k| lo[k].min(p[k]));
            hi = std::array::from_fn(|k| hi[k].max(p[k]));
        }
        (lo, hi)
    });
    MeshStats {
        vertices: mesh.positions.len(),
        triangles: mesh.indices.len() / 3,
        watertight: !mesh.indices.is_empty() && boundary == 0 && non_manifold == 0 && degenerate == 0,
        boundary_edges: boundary,
        non_manifold_edges: non_manifold,
        degenerate_triangles: degenerate,
        welded_positions: ids.len(),
        bounds,
        uv_bounds,
        volume,
        hash: mesh_hash(mesh),
    }
}

/// A BLAKE3 hash (hex) of the mesh's positions, normals, UVs and indices, by bits. Two meshes
/// with the same hash are byte-identical in those arrays; tests pin it across platforms.
pub fn mesh_hash(mesh: &CpuMesh) -> String {
    let mut h = blake3::Hasher::new();
    h.update(b"regia_gen.mesh.v1");
    for len in [
        mesh.positions.len(),
        mesh.normals.len(),
        mesh.uvs.len(),
        mesh.indices.len(),
    ] {
        h.update(&(len as u64).to_le_bytes());
    }
    for v in mesh.positions.iter().chain(&mesh.normals).flatten() {
        h.update(&v.to_bits().to_le_bytes());
    }
    for v in mesh.uvs.iter().flatten() {
        h.update(&v.to_bits().to_le_bytes());
    }
    for i in &mesh.indices {
        h.update(&i.to_le_bytes());
    }
    h.finalize().to_hex().to_string()
}

pub(crate) fn finite(what: &str, values: &[f32]) -> Result<(), GenError> {
    if values.iter().all(|v| v.is_finite()) {
        Ok(())
    } else {
        Err(GenError::InvalidInput(format!("{what} must be finite")))
    }
}

pub(crate) fn check_vertices(n: usize) -> Result<(), GenError> {
    if n > MAX_VERTICES {
        Err(GenError::TooLarge {
            what: "vertex count",
            value: n,
            limit: MAX_VERTICES,
        })
    } else {
        Ok(())
    }
}
