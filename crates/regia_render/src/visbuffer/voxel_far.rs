// SPDX-License-Identifier: MPL-2.0
//! Voxel far proxies: a sparse-brick voxel stand-in for a clustered mesh past its last cluster
//! LOD (ADR-2445, `book/agents/voxel-far.md`).
//!
//! A cluster DAG ([`super::MeshletMesh`]) stops simplifying where the simplifier cannot remove
//! more triangles without breaking the error bound: foliage, branches and other thin, disjoint
//! geometry keep thousands of root triangles at any distance. A [`VoxelFarProxy`] is cooked from
//! the same mesh (`regia_asset_pipeline::voxel_far_cook`, opt-in per asset with `voxel_far` in the
//! glTF `.meta`): a cubic grid over the mesh, stored as 4×4×4 bricks with one occupancy mask
//! each and one prefiltered sample (mean normal, base color, opacity, a representative texture
//! coordinate) per occupied voxel, with a mip chain of coarser grids down to one voxel.
//!
//! **No swap pop.** [`VoxelFarProxy::extend_meshlets`] turns the voxel levels into coarser
//! levels of the mesh's own cluster DAG: every voxel level becomes clusters of cube faces (the
//! exposed faces of its occupied voxels, each voxel shaded with its prefiltered sample), the DAG's
//! root clusters get the first voxel level as their parent, and each voxel level the next one.
//! The visibility buffer's cut test ([`super::LodView::selects`]) then picks the voxel level for
//! a view exactly where its projected error (the level's stored error times [`VoxelFarSettings::error_scale`]) is within the cluster LOD
//! threshold, the same rule that moves between triangle levels, so the hand-off is sub-pixel and
//! there is no impostor cross-fade. Shadow jobs take the cut with their texel size, so far
//! casters shadow as voxels too; the voxel faces are ordinary opaque triangles, so depth, the
//! material resolve, SSAO, fog and the GI inputs see them like any other surface
//! ([`VoxelFarProxy::mesh_sdf`] gives a level as a signed-distance volume for probe rays).
//!
//! Levels whose face count is not below the root cut's triangle count are skipped
//! ([`VoxelFarSettings::max_cost_ratio`]), so a proxy only ever lowers the far cost.
//!
//! **Sub-voxel boxes** (format v2, ADR-2655). Each occupied voxel also stores the box its surface
//! fills inside it ([`VoxelFarProxy::extents`], 1/15-voxel steps rounded outward), and a level
//! draws those boxes instead of whole cubes. A box that is the bounding box of the surface in
//! its voxel touches the surface on every face, which bounds how far any point of it lies from
//! the surface ([`voxel_extent_error`]): at most 1.5 voxel edges for a full box against the
//! cube's diagonal (1.73), much less for a thin or flat one. The cook stores the largest bound of
//! a level as its [`VoxelLevel::error`], so the cut takes the voxel levels nearer at the same
//! silhouette error.
//!
//! Versioning (`specs/12` § 4a): the blob carries [`VOXEL_FAR_FORMAT_VERSION`]. Version 1 blobs
//! (whole cubes, no boxes) still load, every voxel full and the level error the cube diagonal,
//! so they draw as before; [`migrate_cooked_voxel_far_file`] rewrites them (and v2 blobs) as v3
//! for `regia upgrade-project`, and a recook (the cook's cache key hashes the format and cooker
//! versions) gives the tighter boxes. Version 3 (ADR-3108) appends one `f32` per level, the box
//! error the cook measured by sampling distances from the drawn boxes to the surface
//! ([`VoxelFarProxy::sampled_errors`]); the analytic [`VoxelLevel::error`] stays beside it.
//! Version 4 (ADR-3227) appends the cook's estimate of each level's screen coverage at its switch
//! distance as a share of the alpha-tested source's ([`VoxelFarProxy::coverage`]): a count (0 or
//! the level count) and one `f32` per level.

use thiserror::Error;

use super::meshlet::{
    MAX_MESHLET_TRIANGLES, MAX_MESHLET_VERTICES, Meshlet, MeshletMesh, ROOT_ERROR, sphere_of_points,
};
use crate::LabVertex;

/// Cooked format version ([`VoxelFarProxy::to_bytes`]): 2 adds the per-voxel sub-voxel boxes
/// (ADR-2655), 3 a per-level sampled box error after them ([`VoxelFarProxy::sampled_errors`],
/// ADR-3108), 4 the per-level coverage estimate after those ([`VoxelFarProxy::coverage`],
/// ADR-3227). Loaders read 1 to 4 and reject any other value.
pub const VOXEL_FAR_FORMAT_VERSION: u32 = 4;
/// Largest [`VoxelFarProxy::coverage`] value a proxy may store: a level covering more than this
/// many times its source's view is a blob, and the cook clamps it here.
pub const MAX_VOXEL_COVERAGE: f32 = 64.0;
/// Steps of a sub-voxel box bound along a voxel edge (a bound is 0 to 15 fifteenths of a voxel).
pub const VOXEL_EXTENT_STEPS: u32 = 15;
/// The packed box of a voxel filled edge to edge ([`pack_voxel_extent`] of 0 and 15 on every
/// axis): every voxel of a version 1 proxy.
pub const VOXEL_EXTENT_FULL: u32 = 0x00f0_f0f0;
/// Bit 24 of a packed box: the cook could not bound the voxel's surface (a sliver given to its
/// centroid's voxel), so the box is the whole voxel ([`VOXEL_EXTENT_FULL`]) with no face known to
/// touch the surface, and its error is the cube diagonal.
pub const VOXEL_EXTENT_INEXACT: u32 = 1 << 24;

/// How a voxel level draws its voxels ([`VoxelFarProxy::surface_drawn`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VoxelShape {
    /// Every voxel as its whole cube: the fewest faces (neighbouring cubes share theirs).
    Cubes,
    /// Every voxel as its sub-voxel box: the smallest error, but boxes that do not touch their
    /// neighbours draw all their faces.
    Boxes,
    /// A voxel whose cube is within this object-space error of its surface
    /// ([`voxel_cube_error`]) draws its cube, any other its box.
    Mixed(f32),
}
/// Type path of cooked voxel far sub-assets (`voxels/<node>/primitive_<i>`, one per glTF mesh
/// primitive whose `.meta` settings have a `voxel_far` block).
pub const VOXEL_FAR_TYPE_PATH: &str = "regia_render::VoxelFarProxy";
/// Voxels along one edge of a brick (a brick is 4×4×4 voxels, one `u64` occupancy mask).
pub const VOXEL_BRICK_EDGE: u32 = 4;
/// Largest level-0 resolution (voxels along the grid's edge) a proxy may have.
pub const MAX_VOXEL_FAR_RESOLUTION: u32 = 256;

const MAGIC: [u8; 4] = *b"FVOX";

/// The sub-asset path of the voxel proxy cooked for the mesh sub-asset `mesh_sub_path`
/// (`mesh/<node>/primitive_<i>` → `voxels/<node>/primitive_<i>`). `None` for other paths.
#[must_use]
pub fn voxel_far_sub_path(mesh_sub_path: &str) -> Option<String> {
    mesh_sub_path
        .strip_prefix("mesh/")
        .map(|rest| format!("voxels/{rest}"))
}

/// Packs a sub-voxel box: per axis `k`, the low bound `min[k]` in bits `8k..8k+4` and the high
/// bound `max[k]` in bits `8k+4..8k+8`, in [`VOXEL_EXTENT_STEPS`] steps of the voxel edge from the
/// voxel's low corner (values above 15 are clamped). Bits 24-31 stay zero.
#[must_use]
pub fn pack_voxel_extent(min: [u32; 3], max: [u32; 3]) -> u32 {
    (0..3).fold(0u32, |acc, k| {
        let lo = min[k].min(VOXEL_EXTENT_STEPS);
        let hi = max[k].min(VOXEL_EXTENT_STEPS);
        acc | (lo << (8 * k)) | (hi << (8 * k + 4))
    })
}

/// Inverse of [`pack_voxel_extent`]: `(min, max)` in steps.
#[must_use]
pub fn unpack_voxel_extent(packed: u32) -> ([u32; 3], [u32; 3]) {
    (
        std::array::from_fn(|k| (packed >> (8 * k)) & 0xf),
        std::array::from_fn(|k| (packed >> (8 * k + 4)) & 0xf),
    )
}

/// How far any point of the sub-voxel box `packed` can lie from the surface whose bounding box
/// (inside the voxel, rounded outward to steps) it is, for voxels of edge `voxel_size`.
///
/// Take the axis `k` and the face of the true bounding box nearest a box point `p`: the surface
/// touches that face, so some surface point `q` lies on it. Along `k`, `p` is at most half the
/// box's size from it (or the size itself when the box is one step or less, where the outward
/// rounding can be the larger term); across `k`, `p` and `q` are both inside the box. So
/// `|p - q|² ≤ a_k² + Σ_{j≠k} d_j²` for the box sizes `d`, and the bound is the smallest of the
/// three, plus 1e-4 voxel for the clamp to the voxel. A full box gives 1.5 voxel edges, a flat
/// full-width one (zero size on an axis) about 1.12, a thin stick about 0.51. An
/// [`VOXEL_EXTENT_INEXACT`] box gives the cube diagonal.
#[must_use]
pub fn voxel_extent_error(packed: u32, voxel_size: f32) -> f32 {
    if packed & VOXEL_EXTENT_INEXACT != 0 {
        return voxel_size * 3f32.sqrt();
    }
    let (lo, hi) = unpack_voxel_extent(packed);
    let step = voxel_size / VOXEL_EXTENT_STEPS as f32;
    let d: [f32; 3] = std::array::from_fn(|k| hi[k].saturating_sub(lo[k]) as f32 * step);
    let steps: [u32; 3] = std::array::from_fn(|k| hi[k].saturating_sub(lo[k]));
    let best = (0..3)
        .map(|k| {
            let along = if steps[k] >= 2 { d[k] * 0.5 } else { d[k] };
            let across: f32 = (0..3).filter(|&j| j != k).map(|j| d[j] * d[j]).sum();
            (along * along + across).sqrt()
        })
        .fold(f32::MAX, f32::min);
    best + voxel_size * 1e-4
}

/// How far any point of a voxel's whole cube can lie from its surface, when `packed` is the
/// surface's bounding box in it (rounded outward to steps), for voxels of edge `voxel_size`.
///
/// The argument of [`voxel_extent_error`] with the cube in place of the box: the nearer of the
/// box's two faces across axis `k` is at most `max(d_k / 2, slack_k)` from a cube point along
/// `k` (`slack_k` the larger gap between the box and the cube on `k`, plus one step for the
/// rounding; at most a voxel edge), and the surface point on that face at most a voxel edge away
/// on each other axis. A box that spans the voxel on an axis gives 1.5 voxel edges, against the
/// cube diagonal (1.73) that holds without the box; a box in a corner gives up to the diagonal.
#[must_use]
pub fn voxel_cube_error(packed: u32, voxel_size: f32) -> f32 {
    let diagonal = voxel_size * 3f32.sqrt();
    if packed & VOXEL_EXTENT_INEXACT != 0 {
        return diagonal;
    }
    let (lo, hi) = unpack_voxel_extent(packed);
    let step = voxel_size / VOXEL_EXTENT_STEPS as f32;
    let best = (0..3)
        .map(|k| {
            let size = hi[k].saturating_sub(lo[k]) as f32 * step;
            let slack = (lo[k].max(VOXEL_EXTENT_STEPS - hi[k].min(VOXEL_EXTENT_STEPS)) + 1) as f32 * step;
            let along = (size * 0.5).max(slack).min(voxel_size);
            (along * along + 2.0 * voxel_size * voxel_size).sqrt()
        })
        .fold(f32::MAX, f32::min);
    (best + voxel_size * 1e-4).min(diagonal)
}

/// One level of the voxel mip chain (20 bytes in the blob).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct VoxelLevel {
    /// Voxels along the grid's edge (level 0's resolution halved per level, at least 1).
    pub resolution: u32,
    /// Edge length of one voxel, object units.
    pub voxel_size: f32,
    /// Object-space error of drawing this level: the farthest any drawn point lies from the
    /// surface. Version 1: a voxel diagonal (`voxel_size` × √3), the farthest a conservatively
    /// filled cube reaches past its surface. Version 2: the largest [`voxel_extent_error`] of the
    /// level's boxes (the diagonal for a voxel the cook could not bound). Never below the boxes'
    /// bound ([`VoxelFarProxy::validate`]).
    pub error: f32,
    /// First brick of this level in [`VoxelFarProxy::bricks`].
    pub brick_offset: u32,
    /// Bricks of this level.
    pub brick_count: u32,
}

/// A 4×4×4 block of voxels with at least one occupied (16 bytes in the blob).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct VoxelBrick {
    /// Brick coordinate (grid voxel / 4): x in bits 0-9, y in 10-19, z in 20-29.
    pub coord: u32,
    /// First sample of this brick in [`VoxelFarProxy::voxels`]; samples follow in mask bit order.
    pub voxel_offset: u32,
    /// Occupancy bits 0-31 (bit `x + 4y + 16z` of the brick-local voxel).
    pub mask_lo: u32,
    /// Occupancy bits 32-63.
    pub mask_hi: u32,
}

impl VoxelBrick {
    /// The brick's coordinate in brick units.
    #[must_use]
    pub fn brick_xyz(&self) -> [u32; 3] {
        [
            self.coord & 0x3ff,
            (self.coord >> 10) & 0x3ff,
            (self.coord >> 20) & 0x3ff,
        ]
    }

    /// The 64-bit occupancy mask.
    #[must_use]
    pub fn mask(&self) -> u64 {
        u64::from(self.mask_lo) | (u64::from(self.mask_hi) << 32)
    }

    /// Packs a brick coordinate (each axis below 1024).
    #[must_use]
    pub fn pack_coord(xyz: [u32; 3]) -> u32 {
        (xyz[0] & 0x3ff) | ((xyz[1] & 0x3ff) << 10) | ((xyz[2] & 0x3ff) << 20)
    }
}

/// The prefiltered surface inside one occupied voxel (16 bytes in the blob).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct VoxelSample {
    /// Mean surface normal, octahedral-encoded: x in bits 0-15, y in 16-31 (unorm16).
    pub normal: u32,
    /// Linear base color (rgb8, bytes 0-2) and opacity (byte 3: the share of view rays the
    /// voxel's surface stops, see `regia_asset_pipeline::voxel_far_cook`; 1 for a closed or
    /// planar surface, lower for scattered leaves).
    pub color: u32,
    /// Texture coordinate of the surface point nearest the voxel's centre (the material's
    /// textures are sampled there).
    pub uv: [f32; 2],
}

impl VoxelSample {
    /// Packs a sample (the normal need not be unit length; a zero normal encodes +Y).
    #[must_use]
    pub fn new(normal: [f32; 3], color: [f32; 3], opacity: f32, uv: [f32; 2]) -> Self {
        let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u32;
        Self {
            normal: oct_encode(normal),
            color: byte(color[0]) | (byte(color[1]) << 8) | (byte(color[2]) << 16) | (byte(opacity) << 24),
            uv,
        }
    }

    /// The unit normal.
    #[must_use]
    pub fn unit_normal(&self) -> [f32; 3] {
        oct_decode(self.normal)
    }

    /// The linear base color.
    #[must_use]
    pub fn rgb(&self) -> [f32; 3] {
        let c = |shift: u32| ((self.color >> shift) & 0xff) as f32 / 255.0;
        [c(0), c(8), c(16)]
    }

    /// Opacity, 0 to 1.
    #[must_use]
    pub fn opacity(&self) -> f32 {
        (self.color >> 24) as f32 / 255.0
    }
}

/// A fixed threshold in [0, 1) per voxel and level for [`VoxelFarSettings::opacity_dropout`]: an
/// integer hash of the coordinate (the `lowbias32` finalizer from C. Wellons' public-domain "hash
/// prospector", 2018), so the kept set is the same on every machine and every frame.
#[must_use]
pub fn voxel_dither(xyz: [u32; 3], level: u32) -> f32 {
    let mut h = xyz[0].wrapping_mul(0x8da6_b343)
        ^ xyz[1].wrapping_mul(0xd816_3841)
        ^ xyz[2].wrapping_mul(0xcb1a_b31f)
        ^ level.wrapping_mul(0x1656_67b1);
    h ^= h >> 16;
    h = h.wrapping_mul(0x7feb_352d);
    h ^= h >> 15;
    h = h.wrapping_mul(0x846c_a68b);
    h ^= h >> 16;
    (h >> 8) as f32 / (1u32 << 24) as f32
}

/// Octahedral encoding of a direction into two unorm16 (Cigolle et al., "A Survey of Efficient
/// Representations for Independent Unit Vectors", JCGT 3(2), 2014, § 3.1).
#[must_use]
pub fn oct_encode(n: [f32; 3]) -> u32 {
    let l1 = n[0].abs() + n[1].abs() + n[2].abs();
    if !l1.is_finite() || l1 <= 1e-20 {
        return oct_encode([0.0, 1.0, 0.0]);
    }
    let (mut x, mut y) = (n[0] / l1, n[1] / l1);
    if n[2] < 0.0 {
        let (ox, oy) = (x, y);
        x = (1.0 - oy.abs()) * if ox >= 0.0 { 1.0 } else { -1.0 };
        y = (1.0 - ox.abs()) * if oy >= 0.0 { 1.0 } else { -1.0 };
    }
    let q = |v: f32| ((v.clamp(-1.0, 1.0) * 0.5 + 0.5) * 65535.0 + 0.5) as u32;
    q(x) | (q(y) << 16)
}

/// Inverse of [`oct_encode`] (unit length).
#[must_use]
pub fn oct_decode(packed: u32) -> [f32; 3] {
    let f = |v: u32| (v as f32 / 65535.0) * 2.0 - 1.0;
    let (x, y) = (f(packed & 0xffff), f(packed >> 16));
    let z = 1.0 - x.abs() - y.abs();
    let t = (-z).max(0.0);
    let x = x + if x >= 0.0 { -t } else { t };
    let y = y + if y >= 0.0 { -t } else { t };
    let len = (x * x + y * y + z * z).sqrt().max(1e-20);
    [x / len, y / len, z / len]
}

/// A cooked voxel blob or proxy is malformed.
#[derive(Debug, Error, PartialEq)]
pub enum VoxelFarFormatError {
    /// The blob does not start with `FVOX`.
    #[error("not a voxel far blob (bad magic)")]
    BadMagic,
    /// The blob was cooked by another format version.
    #[error("voxel far format version {found}, expected {VOXEL_FAR_FORMAT_VERSION}")]
    Version {
        /// Version in the blob.
        found: u32,
    },
    /// The blob ends before its declared arrays.
    #[error("voxel far blob truncated")]
    Truncated,
    /// The cooked file's wrapper is unreadable.
    #[error("cooked voxel far file: {0}")]
    Cooked(String),
    /// The cooked file holds another asset type.
    #[error("cooked file is not a voxel far sub-asset")]
    WrongType,
    /// A level, brick or count is out of range.
    #[error("voxel far proxy: {0}")]
    Invalid(&'static str),
}

/// Why a cooked voxel proxy did not load ([`VoxelFarProxy::load`]).
#[derive(Debug, Error, PartialEq)]
pub enum VoxelFarLoadError {
    /// No mounted pak or cooked cache holds it (not cooked, or no `voxel_far` in the `.meta`).
    #[error("voxel far proxy not cooked: {0}")]
    NotCooked(String),
    /// The payload is unreadable, of another version or invalid.
    #[error(transparent)]
    Format(#[from] VoxelFarFormatError),
}

impl VoxelFarLoadError {
    /// A stable code for the agent protocol: `voxel_far_not_cooked`, `voxel_far_unreadable`,
    /// `voxel_far_old_version` (`regia cook` rebuilds it) or `voxel_far_invalid`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotCooked(_) => "voxel_far_not_cooked",
            Self::Format(VoxelFarFormatError::Version { .. }) => "voxel_far_old_version",
            Self::Format(
                VoxelFarFormatError::BadMagic
                | VoxelFarFormatError::Cooked(_)
                | VoxelFarFormatError::WrongType,
            ) => "voxel_far_unreadable",
            Self::Format(VoxelFarFormatError::Truncated | VoxelFarFormatError::Invalid(_)) => {
                "voxel_far_invalid"
            }
        }
    }
}

/// A sparse-brick voxel stand-in for a mesh, with a mip chain (finest level first).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VoxelFarProxy {
    /// Object-space minimum corner of the grid.
    pub origin: [f32; 3],
    /// Edge length of the cubic grid, object units.
    pub extent: f32,
    /// Object-space bounding sphere of the source mesh.
    pub bounds: [f32; 4],
    /// Source triangles the proxy was cooked from.
    pub source_triangles: u32,
    /// Levels, finest first.
    pub levels: Vec<VoxelLevel>,
    /// Bricks of every level.
    pub bricks: Vec<VoxelBrick>,
    /// Samples of every occupied voxel.
    pub voxels: Vec<VoxelSample>,
    /// Sub-voxel box of every occupied voxel ([`pack_voxel_extent`]), parallel to
    /// [`Self::voxels`]; empty when every voxel is full ([`VOXEL_EXTENT_FULL`], a version 1
    /// proxy), which is also how [`Self::from_bytes`] returns an all-full proxy.
    pub extents: Vec<u32>,
    /// Per level, how far the drawn boxes lie from the surface as the cook measured it (a
    /// distance sample over every box face plus the sample spacing's margin, ADR-3108): at most
    /// [`VoxelLevel::error`] (the analytic bound) and often far below it for flat boxes. Empty
    /// when the cook measured nothing (format 1 and 2) or nothing tighter, which is how
    /// [`Self::from_bytes`] returns it; see [`Self::level_box_error`].
    pub sampled_errors: Vec<f32>,
    /// Per level, the cook's estimate of the level's screen coverage at its switch distance as a
    /// share of the alpha-tested source's (ADR-3227): 1 draws as much of the view as the near
    /// clusters, below 1 the far canopy thins, above 1 it is a blob. In `0..=`
    /// [`MAX_VOXEL_COVERAGE`]; empty when the cook measured nothing (formats 1 to 3).
    pub coverage: Vec<f32>,
}

/// Knobs of [`VoxelFarProxy::extend_meshlets`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VoxelFarSettings {
    /// A voxel level is used only while its triangles are fewer than this times the root cut's
    /// (the DAG's root clusters). 1 keeps every level that is cheaper than the roots.
    pub max_cost_ratio: f32,
    /// Multiplies the error the cut tests. `1` leaves the stored level error unchanged (a v2
    /// box bound, or a v1 diagonal). Below 1 the voxels take over nearer, at a visible error;
    /// above 1 later.
    pub error_scale: f32,
    /// Keep a voxel only when its opacity is above a fixed per-voxel threshold
    /// ([`voxel_dither`]), so a level of partly transparent voxels (a canopy of leaves) lets the
    /// same share of view rays through as the leaves do, instead of drawing as a solid shell.
    /// Opaque voxels (closed or planar surfaces) are always kept. On by default.
    pub opacity_dropout: bool,
}

impl Default for VoxelFarSettings {
    fn default() -> Self {
        Self {
            max_cost_ratio: 1.0,
            error_scale: 1.0,
            opacity_dropout: true,
        }
    }
}

/// One voxel level as [`VoxelFarProxy::extend_meshlets`] put it into the DAG.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VoxelFarLevelUse {
    /// Index into [`VoxelFarProxy::levels`].
    pub level: usize,
    /// The DAG level its clusters carry.
    pub dag_level: u32,
    /// The error the cut tests (after [`VoxelFarSettings::error_scale`], at least the roots').
    pub error: f32,
    /// Clusters of the level.
    pub clusters: u32,
    /// Triangles of the level.
    pub triangles: u32,
    /// Vertices the level appended.
    pub vertices: u32,
    /// How the level draws its voxels (the cheapest shape whose error keeps the switch nearest).
    pub shape: VoxelShape,
}

/// A mesh's cluster DAG with the voxel levels on top ([`VoxelFarProxy::extend_meshlets`]).
#[derive(Clone, Debug, Default)]
pub struct VoxelFarMesh {
    /// The mesh's vertices followed by the voxel faces' vertices: draw the mesh with these and
    /// its own indices (the voxel triangles live only in the clusters, so the forward path,
    /// which draws the indices, never draws them).
    pub vertices: Vec<LabVertex>,
    /// The extended DAG ([`MeshletMesh::source_vertex_count`] counts the appended vertices,
    /// [`MeshletMesh::source_index_count`] is the mesh's own).
    pub meshlets: MeshletMesh,
    /// The voxel levels used, finest first (empty: no level was cheaper than the roots).
    pub levels: Vec<VoxelFarLevelUse>,
    /// Triangles of the root cut the levels were compared with.
    pub root_triangles: u32,
    /// Vertices of the mesh itself (the start of the appended voxel vertices in
    /// [`Self::vertices`]).
    pub base_vertices: u32,
}

impl VoxelFarMesh {
    /// The extended DAG for a mesh that keeps its own vertex buffer and mesh key (an ECS mesh
    /// entity, `regia_framework::meshlet_bridge`): the voxel vertices travel in
    /// [`MeshletMesh::appended_vertices`], so the draw supplies only the mesh's vertices and the
    /// visibility buffer checks them against [`MeshletMesh::base_vertex_count`]. Instance sets,
    /// which pass their vertices, use [`Self::vertices`] and [`Self::meshlets`] instead.
    #[must_use]
    pub fn bridged(&self) -> MeshletMesh {
        let mut mesh = self.meshlets.clone();
        let base = (self.base_vertices as usize).min(self.vertices.len());
        mesh.appended_vertices = super::meshlet::AppendedVertices(self.vertices[base..].to_vec());
        mesh
    }

    /// Triangles the voxel levels add (every used level).
    #[must_use]
    pub fn voxel_triangles(&self) -> u64 {
        self.levels.iter().map(|l| u64::from(l.triangles)).sum()
    }

    /// Clusters the voxel levels add.
    #[must_use]
    pub fn voxel_clusters(&self) -> u64 {
        self.levels.iter().map(|l| u64::from(l.clusters)).sum()
    }
}

/// The exposed cube faces of one voxel level as a triangle mesh.
#[derive(Clone, Debug, Default)]
pub struct VoxelSurface {
    /// Corner vertices (up to eight per voxel, shared by its faces, shaded with its sample).
    pub vertices: Vec<LabVertex>,
    /// Triangle indices, counter-clockwise seen from outside.
    pub indices: Vec<u32>,
    /// Vertex range and triangle range of each voxel with at least one exposed face, in brick
    /// order: `(first vertex, vertices, first index, indices)`.
    pub voxel_ranges: Vec<[u32; 4]>,
}

/// Corners of a unit voxel: bit 0 = +x, bit 1 = +y, bit 2 = +z.
const FACES: [([i32; 3], [usize; 4]); 6] = [
    ([1, 0, 0], [5, 1, 3, 7]),
    ([-1, 0, 0], [0, 4, 6, 2]),
    ([0, 1, 0], [6, 7, 3, 2]),
    ([0, -1, 0], [0, 1, 5, 4]),
    ([0, 0, 1], [4, 5, 7, 6]),
    ([0, 0, -1], [1, 0, 2, 3]),
];

fn read_u32(bytes: &[u8], at: &mut usize) -> Result<u32, VoxelFarFormatError> {
    let slice = bytes.get(*at..*at + 4).ok_or(VoxelFarFormatError::Truncated)?;
    *at += 4;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_f32(bytes: &[u8], at: &mut usize) -> Result<f32, VoxelFarFormatError> {
    read_u32(bytes, at).map(f32::from_bits)
}

fn read_array<T: bytemuck::Pod>(
    bytes: &[u8],
    at: &mut usize,
    count: usize,
) -> Result<Vec<T>, VoxelFarFormatError> {
    let size = count
        .checked_mul(std::mem::size_of::<T>())
        .ok_or(VoxelFarFormatError::Truncated)?;
    let end = at.checked_add(size).ok_or(VoxelFarFormatError::Truncated)?;
    let slice = bytes.get(*at..end).ok_or(VoxelFarFormatError::Truncated)?;
    *at = end;
    Ok(slice
        .chunks_exact(std::mem::size_of::<T>())
        .map(bytemuck::pod_read_unaligned)
        .collect())
}

fn sphere_union(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    let d = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let dist = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
    if dist + b[3] <= a[3] {
        return a;
    }
    if dist + a[3] <= b[3] {
        return b;
    }
    let radius = (dist + a[3] + b[3]) * 0.5;
    let t = if dist > 1e-20 { (radius - a[3]) / dist } else { 0.0 };
    [a[0] + d[0] * t, a[1] + d[1] * t, a[2] + d[2] * t, radius]
}

fn tangent_for(n: [f32; 3]) -> [f32; 4] {
    // Any unit vector orthogonal to the normal (a voxel face has no texture direction).
    let a = if n[0].abs() < 0.9 {
        [1.0, 0.0, 0.0]
    } else {
        [0.0, 1.0, 0.0]
    };
    let d = a[0] * n[0] + a[1] * n[1] + a[2] * n[2];
    let t = [a[0] - n[0] * d, a[1] - n[1] * d, a[2] - n[2] * d];
    let len = (t[0] * t[0] + t[1] * t[1] + t[2] * t[2]).sqrt().max(1e-20);
    [t[0] / len, t[1] / len, t[2] / len, 1.0]
}

impl VoxelFarProxy {
    /// Occupied voxels of `level` (0 for a level that does not exist).
    #[must_use]
    pub fn voxel_count(&self, level: usize) -> usize {
        self.level_bricks(level)
            .iter()
            .map(|b| b.mask().count_ones() as usize)
            .sum()
    }

    /// Bricks of `level` (empty for a level that does not exist).
    #[must_use]
    pub fn level_bricks(&self, level: usize) -> &[VoxelBrick] {
        self.levels
            .get(level)
            .and_then(|l| {
                self.bricks
                    .get(l.brick_offset as usize..(l.brick_offset as usize + l.brick_count as usize))
            })
            .unwrap_or(&[])
    }

    /// Every occupied voxel of `level` in brick order: grid coordinate and sample.
    pub fn level_voxels(&self, level: usize) -> impl Iterator<Item = ([u32; 3], VoxelSample)> + '_ {
        self.level_voxel_slots(level)
            .map(|(xyz, slot)| (xyz, self.voxels[slot]))
    }

    /// Every occupied voxel of `level` in brick order with its sub-voxel box.
    pub fn level_voxel_extents(
        &self,
        level: usize,
    ) -> impl Iterator<Item = ([u32; 3], VoxelSample, u32)> + '_ {
        self.level_voxel_slots(level)
            .map(|(xyz, slot)| (xyz, self.voxels[slot], self.extent(slot)))
    }

    /// The sub-voxel box of sample `slot` ([`VOXEL_EXTENT_FULL`] when [`Self::extents`] is
    /// empty or shorter).
    #[must_use]
    pub fn extent(&self, slot: usize) -> u32 {
        self.extents.get(slot).copied().unwrap_or(VOXEL_EXTENT_FULL)
    }

    /// The error of drawing `level` as its boxes ([`VoxelShape::Boxes`]): the cook's sampled box
    /// error where it has one ([`Self::sampled_errors`]), else [`VoxelLevel::error`]; 0 for a
    /// level that does not exist.
    #[must_use]
    pub fn level_box_error(&self, level: usize) -> f32 {
        let Some(info) = self.levels.get(level) else {
            return 0.0;
        };
        self.sampled_errors
            .get(level)
            .map_or(info.error, |&sampled| sampled.min(info.error))
    }

    /// The largest [`voxel_extent_error`] of `level`'s boxes (0 for a level that does not exist):
    /// the error bound the boxes give, which [`VoxelLevel::error`] may not undercut.
    #[must_use]
    pub fn level_extent_error(&self, level: usize) -> f32 {
        let Some(info) = self.levels.get(level) else {
            return 0.0;
        };
        self.level_voxel_slots(level)
            .map(|(_, slot)| voxel_extent_error(self.extent(slot), info.voxel_size))
            .fold(0.0f32, f32::max)
    }

    /// The largest [`voxel_cube_error`] of `level`'s voxels: the error of drawing them all as
    /// whole cubes ([`VoxelShape::Cubes`]); [`VoxelLevel::error`] when the proxy has no boxes
    /// (a v1 proxy, or one whose boxes are all whole).
    #[must_use]
    pub fn level_cube_error(&self, level: usize) -> f32 {
        let Some(info) = self.levels.get(level) else {
            return 0.0;
        };
        if self.extents.is_empty() {
            return info.error;
        }
        self.level_voxel_slots(level)
            .map(|(_, slot)| voxel_cube_error(self.extent(slot), info.voxel_size))
            .fold(info.error, f32::max)
    }

    /// Grid coordinate and sample index of every occupied voxel of `level` (indices past
    /// [`Self::voxels`] are skipped).
    fn level_voxel_slots(&self, level: usize) -> impl Iterator<Item = ([u32; 3], usize)> + '_ {
        let samples = self.voxels.len();
        self.level_bricks(level).iter().flat_map(move |brick| {
            let base = brick.brick_xyz().map(|v| v * VOXEL_BRICK_EDGE);
            let mask = brick.mask();
            let mut slot = brick.voxel_offset as usize;
            (0..64u32).filter_map(move |bit| {
                if mask & (1u64 << bit) == 0 {
                    return None;
                }
                let here = slot;
                slot += 1;
                (here < samples).then_some((
                    [
                        base[0] + (bit & 3),
                        base[1] + ((bit >> 2) & 3),
                        base[2] + (bit >> 4),
                    ],
                    here,
                ))
            })
        })
    }

    /// Bytes of the cooked blob ([`Self::to_bytes`]).
    #[must_use]
    pub fn byte_size(&self) -> usize {
        HEADER_BYTES
            + self.levels.len() * std::mem::size_of::<VoxelLevel>()
            + self.bricks.len() * std::mem::size_of::<VoxelBrick>()
            + self.voxels.len() * (std::mem::size_of::<VoxelSample>() + 4)
            + self.levels.len() * 4
            + 4
            + self.coverage.len() * 4
    }

    /// Checks every level, brick and sample range.
    ///
    /// # Errors
    ///
    /// [`VoxelFarFormatError::Invalid`] naming what is wrong.
    pub fn validate(&self) -> Result<(), VoxelFarFormatError> {
        let invalid = |reason| Err(VoxelFarFormatError::Invalid(reason));
        if !self.extent.is_finite() || self.extent <= 0.0 || self.origin.iter().any(|v| !v.is_finite()) {
            return invalid("grid extent or origin not finite and positive");
        }
        let mut expected_resolution = None;
        let mut brick_end = 0u64;
        let mut voxel_end = 0u64;
        for (level_index, level) in self.levels.iter().enumerate() {
            if level.resolution == 0 || level.resolution > MAX_VOXEL_FAR_RESOLUTION {
                return invalid("level resolution out of range");
            }
            if let Some(expected) = expected_resolution
                && level.resolution != expected
            {
                return invalid("level resolution is not half the previous level's");
            }
            expected_resolution = Some((level.resolution / 2).max(1));
            if level.error.is_nan()
                || level.error < 0.0
                || level.voxel_size.is_nan()
                || level.voxel_size <= 0.0
            {
                return invalid("level error or voxel size not positive");
            }
            if u64::from(level.brick_offset) != brick_end {
                return invalid("levels' bricks are not contiguous");
            }
            brick_end += u64::from(level.brick_count);
            if brick_end > self.bricks.len() as u64 {
                return invalid("level bricks past the brick array");
            }
            let bricks_per_edge = level.resolution.div_ceil(VOXEL_BRICK_EDGE);
            for brick in &self.bricks[level.brick_offset as usize..brick_end as usize] {
                if brick.brick_xyz().iter().any(|&v| v >= bricks_per_edge) {
                    return invalid("brick outside its level's grid");
                }
                if brick.mask() == 0 {
                    return invalid("empty brick");
                }
                if u64::from(brick.voxel_offset) != voxel_end {
                    return invalid("bricks' samples are not contiguous");
                }
                voxel_end += u64::from(brick.mask().count_ones());
            }
            let edge = level.resolution;
            for (xyz, _) in self.level_voxels(level_index) {
                if xyz.iter().any(|&v| v >= edge) {
                    return invalid("voxel outside its level's grid");
                }
            }
        }
        if brick_end != self.bricks.len() as u64 || voxel_end != self.voxels.len() as u64 {
            return invalid("bricks or samples not owned by a level");
        }
        if !self.extents.is_empty() && self.extents.len() != self.voxels.len() {
            return invalid("extents not parallel to the samples");
        }
        for &packed in &self.extents {
            let (lo, hi) = unpack_voxel_extent(packed);
            if packed >> 25 != 0 || (0..3).any(|k| lo[k] > hi[k]) {
                return invalid("sub-voxel box reversed or reserved bits set");
            }
            if packed & VOXEL_EXTENT_INEXACT != 0 && packed != VOXEL_EXTENT_FULL | VOXEL_EXTENT_INEXACT {
                return invalid("an unbounded voxel's box is not the whole voxel");
            }
        }
        for (index, level) in self.levels.iter().enumerate() {
            // A level may claim a larger error than its boxes give, never a smaller one.
            if level.error < self.level_extent_error(index) * (1.0 - 1e-4) {
                return invalid("level error below its boxes' bound");
            }
        }
        if !self.sampled_errors.is_empty() {
            if self.sampled_errors.len() != self.levels.len() {
                return invalid("sampled errors not one per level");
            }
            // A sampled error tightens the level's, it never loosens it.
            for (sampled, level) in self.sampled_errors.iter().zip(&self.levels) {
                if !sampled.is_finite() || *sampled < 0.0 || *sampled > level.error {
                    return invalid("sampled box error outside 0..=the level's error");
                }
            }
        }
        if !self.coverage.is_empty() {
            if self.coverage.len() != self.levels.len() {
                return invalid("coverage not one per level");
            }
            if self
                .coverage
                .iter()
                .any(|c| !c.is_finite() || *c < 0.0 || *c > MAX_VOXEL_COVERAGE)
            {
                return invalid("coverage outside 0..=MAX_VOXEL_COVERAGE");
            }
        }
        Ok(())
    }

    /// Serializes to the cooked blob: `FVOX`, version, header, then levels, bricks and samples
    /// (little-endian).
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.byte_size());
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&VOXEL_FAR_FORMAT_VERSION.to_le_bytes());
        for v in self.origin {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&self.extent.to_le_bytes());
        for v in self.bounds {
            out.extend_from_slice(&v.to_le_bytes());
        }
        for v in [
            self.source_triangles,
            self.levels.len() as u32,
            self.bricks.len() as u32,
            self.voxels.len() as u32,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(bytemuck::cast_slice(&self.levels));
        out.extend_from_slice(bytemuck::cast_slice(&self.bricks));
        out.extend_from_slice(bytemuck::cast_slice(&self.voxels));
        for slot in 0..self.voxels.len() {
            out.extend_from_slice(&self.extent(slot).to_le_bytes());
        }
        for index in 0..self.levels.len() {
            out.extend_from_slice(&self.level_box_error(index).to_le_bytes());
        }
        out.extend_from_slice(&(self.coverage.len() as u32).to_le_bytes());
        for c in &self.coverage {
            out.extend_from_slice(&c.to_le_bytes());
        }
        out
    }

    /// Parses and validates a cooked blob of version 4, 3, 2 or 1 (a v1 blob has no boxes: every
    /// voxel is full and [`Self::extents`] empty, so it draws as v1 did; v1 and v2 blobs have no
    /// sampled box errors, and v1 to v3 blobs no coverage estimate).
    ///
    /// # Errors
    ///
    /// Bad magic, another version, truncation, or an invalid proxy.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, VoxelFarFormatError> {
        if bytes.get(0..4) != Some(&MAGIC[..]) {
            return Err(VoxelFarFormatError::BadMagic);
        }
        let mut at = 4;
        let version = read_u32(bytes, &mut at)?;
        if !(1..=VOXEL_FAR_FORMAT_VERSION).contains(&version) {
            return Err(VoxelFarFormatError::Version { found: version });
        }
        let origin = [
            read_f32(bytes, &mut at)?,
            read_f32(bytes, &mut at)?,
            read_f32(bytes, &mut at)?,
        ];
        let extent = read_f32(bytes, &mut at)?;
        let bounds = [
            read_f32(bytes, &mut at)?,
            read_f32(bytes, &mut at)?,
            read_f32(bytes, &mut at)?,
            read_f32(bytes, &mut at)?,
        ];
        let source_triangles = read_u32(bytes, &mut at)?;
        let level_count = read_u32(bytes, &mut at)? as usize;
        let brick_count = read_u32(bytes, &mut at)? as usize;
        let voxel_count = read_u32(bytes, &mut at)? as usize;
        if level_count > 32 {
            return Err(VoxelFarFormatError::Invalid("more than 32 levels"));
        }
        let mut proxy = Self {
            origin,
            extent,
            bounds,
            source_triangles,
            levels: read_array(bytes, &mut at, level_count)?,
            bricks: read_array(bytes, &mut at, brick_count)?,
            voxels: read_array(bytes, &mut at, voxel_count)?,
            extents: Vec::new(),
            sampled_errors: Vec::new(),
            coverage: Vec::new(),
        };
        if version >= 2 {
            let extents: Vec<u32> = read_array(bytes, &mut at, voxel_count)?;
            // All-full boxes are stored as none, so a v1-shaped proxy round trips equal.
            if extents.iter().any(|&e| e != VOXEL_EXTENT_FULL) {
                proxy.extents = extents;
            }
        }
        if version >= 3 {
            let sampled: Vec<f32> = read_array(bytes, &mut at, level_count)?;
            // A sampled error equal to the level's tightens nothing: stored as none, so a v2-shaped
            // proxy round trips equal.
            if sampled
                .iter()
                .zip(&proxy.levels)
                .any(|(s, level)| s.to_bits() != level.error.to_bits())
            {
                proxy.sampled_errors = sampled;
            }
        }
        if version >= 4 {
            let count = read_u32(bytes, &mut at)? as usize;
            if count != 0 && count != level_count {
                return Err(VoxelFarFormatError::Invalid(
                    "coverage count is neither 0 nor the level count",
                ));
            }
            proxy.coverage = read_array(bytes, &mut at, count)?;
        }
        if at != bytes.len() {
            return Err(VoxelFarFormatError::Invalid("bytes past the declared arrays"));
        }
        proxy.validate()?;
        Ok(proxy)
    }

    /// Reads a cooked voxel sub-asset as `regia cook` writes it: the cooked header with a
    /// [`VOXEL_FAR_TYPE_PATH`] type tag, then the `.fvox` blob. Links no cooker.
    ///
    /// # Errors
    ///
    /// A bad wrapper, another asset type, or any [`Self::from_bytes`] error.
    pub fn from_cooked_file(bytes: &[u8]) -> Result<Self, VoxelFarFormatError> {
        let (header, payload) =
            regia_asset::decode_cooked_file(bytes).map_err(|e| VoxelFarFormatError::Cooked(e.to_string()))?;
        if header.type_tag != regia_core::id::TypePathHash::of(VOXEL_FAR_TYPE_PATH) {
            return Err(VoxelFarFormatError::WrongType);
        }
        Self::from_bytes(payload)
    }

    /// Loads the cooked voxel sub-asset `id` through `server` (the latest mounted pak that holds
    /// it, else the cooked cache), marked as decoded outside the server so a hot reload bumps
    /// [`regia_asset::AssetServer::generation`].
    ///
    /// # Errors
    ///
    /// [`VoxelFarLoadError::NotCooked`] when nothing holds it, else the
    /// [`Self::from_cooked_file`] error.
    pub fn load(
        server: &mut regia_asset::AssetServer,
        id: regia_asset::AssetId,
    ) -> Result<Self, VoxelFarLoadError> {
        let _span = tracing::info_span!("asset_load", kind = "voxel_far").entered();
        server.track_external(id);
        let (bytes, _) = server
            .read_cooked(id)
            .map_err(|e| VoxelFarLoadError::NotCooked(e.to_string()))?;
        Ok(Self::from_cooked_file(&bytes)?)
    }

    /// The exposed faces of `level`'s occupied voxels, each voxel drawn as its sub-voxel box
    /// ([`Self::extents`]; the whole cube when full) with its corners shaded with its
    /// prefiltered sample. A face is left out when the neighbouring voxel's box covers it (both
    /// boxes reach the shared side and the neighbour's spans this face's rectangle), and when it
    /// has no area (a flat box draws its two broad faces only). With every box full this is the
    /// exposed cube faces of the occupied voxels. Every occupied voxel is kept;
    /// [`Self::surface_with_dropout`] drops partly transparent ones.
    #[must_use]
    pub fn surface(&self, level: usize) -> VoxelSurface {
        self.surface_with_dropout(level, false)
    }

    /// Whether `sample` at `xyz` of `level` is drawn: always without `dropout`, else when its
    /// opacity is above [`voxel_dither`].
    #[must_use]
    pub fn keeps(level: usize, xyz: [u32; 3], sample: &VoxelSample, dropout: bool) -> bool {
        !dropout || sample.opacity() > voxel_dither(xyz, level as u32)
    }

    /// [`Self::surface`] over the voxels [`Self::keeps`] keeps: with `dropout`, a voxel of
    /// opacity `a` is kept for a fixed `a` share of the voxels, and the faces it leaves open are
    /// exposed, so view rays and shadow rays pass a dropped voxel and meet the ones behind it.
    #[must_use]
    pub fn surface_with_dropout(&self, level: usize, dropout: bool) -> VoxelSurface {
        self.surface_drawn(level, dropout, VoxelShape::Boxes)
    }

    /// The box `shape` draws for a voxel with the packed box `extent` at `level`.
    fn drawn_extent(&self, level: usize, extent: u32, shape: VoxelShape) -> u32 {
        let extent = extent & !VOXEL_EXTENT_INEXACT;
        match shape {
            VoxelShape::Cubes => VOXEL_EXTENT_FULL,
            VoxelShape::Boxes => extent,
            VoxelShape::Mixed(error) => {
                let size = self.levels.get(level).map_or(0.0, |l| l.voxel_size);
                if voxel_cube_error(extent, size) <= error {
                    VOXEL_EXTENT_FULL
                } else {
                    extent
                }
            }
        }
    }

    /// [`Self::surface_with_dropout`] with each voxel drawn as `shape` says. The error of the
    /// result is [`Self::level_box_error`] for boxes, [`Self::level_cube_error`] for cubes and the
    /// larger of `error` and the level's error for [`VoxelShape::Mixed`].
    #[must_use]
    pub fn surface_drawn(&self, level: usize, dropout: bool, shape: VoxelShape) -> VoxelSurface {
        let mut out = VoxelSurface::default();
        let Some(info) = self.levels.get(level) else {
            return out;
        };
        let edge = info.resolution as usize;
        let mut occupied = vec![0u64; (edge * edge * edge).div_ceil(64)];
        let index = |x: usize, y: usize, z: usize| x + edge * (y + edge * z);
        // Boxes of the kept voxels that are not full (a full box is the default).
        let mut boxes: std::collections::HashMap<usize, u32> = std::collections::HashMap::new();
        for (xyz, sample, extent) in self.level_voxel_extents(level) {
            if !Self::keeps(level, xyz, &sample, dropout) {
                continue;
            }
            let extent = self.drawn_extent(level, extent, shape);
            let i = index(xyz[0] as usize, xyz[1] as usize, xyz[2] as usize);
            occupied[i / 64] |= 1u64 << (i % 64);
            if extent != VOXEL_EXTENT_FULL {
                boxes.insert(i, extent);
            }
        }
        // The neighbour's box, `None` when the neighbour is empty, dropped or off the grid.
        let neighbour = |x: i64, y: i64, z: i64| {
            if x < 0 || y < 0 || z < 0 || x >= edge as i64 || y >= edge as i64 || z >= edge as i64 {
                return None;
            }
            let i = index(x as usize, y as usize, z as usize);
            (occupied[i / 64] & (1u64 << (i % 64)) != 0)
                .then(|| boxes.get(&i).copied().unwrap_or(VOXEL_EXTENT_FULL))
        };
        let size = info.voxel_size;
        let steps = VOXEL_EXTENT_STEPS;
        for (xyz, sample, extent) in self.level_voxel_extents(level) {
            if !Self::keeps(level, xyz, &sample, dropout) {
                continue;
            }
            let (lo, hi) = unpack_voxel_extent(self.drawn_extent(level, extent, shape));
            let (x, y, z) = (i64::from(xyz[0]), i64::from(xyz[1]), i64::from(xyz[2]));
            let exposed: Vec<usize> = FACES
                .iter()
                .enumerate()
                .filter(|(_, (d, _))| {
                    let k = d.iter().position(|&c| c != 0).unwrap_or(0);
                    let positive = d[k] > 0;
                    // No area: a box of zero size across this face.
                    if (0..3).any(|j| j != k && hi[j] == lo[j]) {
                        return false;
                    }
                    let Some(other) =
                        neighbour(x + i64::from(d[0]), y + i64::from(d[1]), z + i64::from(d[2]))
                    else {
                        return true;
                    };
                    let (olo, ohi) = unpack_voxel_extent(other);
                    let reaches = if positive {
                        hi[k] == steps && olo[k] == 0
                    } else {
                        lo[k] == 0 && ohi[k] == steps
                    };
                    let spans = (0..3).all(|j| j == k || (olo[j] <= lo[j] && ohi[j] >= hi[j]));
                    !(reaches && spans)
                })
                .map(|(f, _)| f)
                .collect();
            if exposed.is_empty() {
                continue;
            }
            let first_vertex = out.vertices.len() as u32;
            let first_index = out.indices.len() as u32;
            let normal = sample.unit_normal();
            let rgb = sample.rgb();
            let mut corner_slot = [u32::MAX; 8];
            for &f in &exposed {
                let corners = FACES[f].1;
                for &c in &corners {
                    if corner_slot[c] == u32::MAX {
                        corner_slot[c] = out.vertices.len() as u32;
                        let p: [f32; 3] = std::array::from_fn(|k| {
                            let q = if (c >> k) & 1 == 1 { hi[k] } else { lo[k] };
                            self.origin[k] + (xyz[k] as f32 + q as f32 / steps as f32) * size
                        });
                        out.vertices.push(LabVertex {
                            position: p,
                            normal,
                            uv: sample.uv,
                            tangent: tangent_for(normal),
                            color: [rgb[0], rgb[1], rgb[2], 1.0],
                            joints: [0; 4],
                            weights: [0.0; 4],
                        });
                    }
                }
                let [a, b, c, d] = corners.map(|c| corner_slot[c]);
                out.indices.extend_from_slice(&[a, b, c, a, c, d]);
            }
            out.voxel_ranges.push([
                first_vertex,
                out.vertices.len() as u32 - first_vertex,
                first_index,
                out.indices.len() as u32 - first_index,
            ]);
        }
        out
    }

    /// Triangles [`Self::surface`] gives for `level`.
    #[must_use]
    pub fn surface_triangles(&self, level: usize) -> usize {
        self.surface(level).indices.len() / 3
    }

    /// `level` as a signed-distance volume for probe rays ([`crate::MeshSdf`], `resolution`
    /// samples per axis) with the level's area-weighted mean base color: the GI input of a mesh
    /// whose near geometry is not resident. `None` for a level that does not exist or is empty.
    #[must_use]
    pub fn mesh_sdf(&self, level: usize, resolution: u32) -> Option<crate::MeshSdf> {
        let surface = self.surface(level);
        if surface.indices.is_empty() {
            return None;
        }
        let mut albedo = [0.0f32; 3];
        let mut weight = 0.0f32;
        for (_, sample) in self.level_voxels(level) {
            let w = sample.opacity().max(1.0 / 255.0);
            let c = sample.rgb();
            for k in 0..3 {
                albedo[k] += c[k] * w;
            }
            weight += w;
        }
        let albedo = albedo.map(|c| c / weight.max(1e-20));
        let positions: Vec<[f32; 3]> = surface.vertices.iter().map(|v| v.position).collect();
        Some(crate::MeshSdf::from_triangles(
            &positions,
            &surface.indices,
            resolution,
            albedo,
        ))
    }

    /// Distance from a sphere's centre at which a level with object-space `error` projects to
    /// `threshold_px` pixels, for `pixels_per_unit` = viewport height / (2 tan(fov_y / 2)) (the
    /// inverse of [`super::LodView::projected_error`]).
    #[must_use]
    pub fn switch_distance(error: f32, sphere_radius: f32, pixels_per_unit: f32, threshold_px: f32) -> f32 {
        error * pixels_per_unit / threshold_px.max(1e-6) + sphere_radius
    }

    /// The mesh's cluster DAG with the voxel levels on top: the voxel levels' faces become
    /// clusters (at most [`MAX_MESHLET_VERTICES`] vertices and [`MAX_MESHLET_TRIANGLES`]
    /// triangles, whole voxels in brick order), the root clusters (and, through the cut rule,
    /// their subtrees) get the first used level as their parent, and each used level the next.
    /// Every voxel cluster and every root shares one sphere, the union of the mesh's bounds and
    /// the roots' spheres, so the switch is all-or-nothing per instance and the cut never shows a
    /// hole or a double surface.
    ///
    /// `vertices` are the mesh's own (the ones `meshlets` index); the result appends the voxel
    /// vertices to them.
    ///
    /// # Errors
    ///
    /// The DAG does not index `vertices`, or the result would not validate.
    pub fn extend_meshlets(
        &self,
        meshlets: &MeshletMesh,
        vertices: &[LabVertex],
        settings: &VoxelFarSettings,
    ) -> Result<VoxelFarMesh, VoxelFarFormatError> {
        let _span = tracing::info_span!("voxel_far_extend", levels = self.levels.len()).entered();
        if meshlets.source_vertex_count as usize != vertices.len() || meshlets.meshlets.is_empty() {
            return Err(VoxelFarFormatError::Invalid(
                "meshlets do not index these vertices",
            ));
        }
        if !meshlets.appended_vertices.0.is_empty() {
            return Err(VoxelFarFormatError::Invalid("meshlets already append vertices"));
        }
        let mut out = VoxelFarMesh {
            vertices: vertices.to_vec(),
            meshlets: meshlets.clone(),
            levels: Vec::new(),
            root_triangles: 0,
            base_vertices: vertices.len() as u32,
        };
        let roots: Vec<usize> = meshlets
            .meshlets
            .iter()
            .enumerate()
            .filter(|(_, m)| m.parent_error >= ROOT_ERROR)
            .map(|(i, _)| i)
            .collect();
        let mut sphere = meshlets.bounds;
        let mut root_error = 0.0f32;
        for &r in &roots {
            let m = &meshlets.meshlets[r];
            sphere = sphere_union(sphere, m.lod);
            root_error = root_error.max(m.lod_error);
            out.root_triangles += m.triangle_count;
        }
        sphere = sphere_union(sphere, self.bounds);
        let base_levels = meshlets.level_count();
        // Pick the levels: cheaper than the roots, errors strictly increasing. Each level tries
        // its shapes from the smallest error up (boxes, cubes where they are within 1.5 and 1.6
        // voxel edges, all cubes) and takes the first one cheaper than the roots.
        let mut picked: Vec<(usize, f32, VoxelSurface, VoxelShape)> = Vec::new();
        let mut last_error = root_error;
        let scale = settings.error_scale.max(0.0);
        for (index, level) in self.levels.iter().enumerate() {
            let cube_error = self.level_cube_error(index);
            let mut shapes: Vec<(f32, VoxelShape)> = if self.extents.is_empty() {
                vec![(level.error, VoxelShape::Cubes)]
            } else {
                let mixed = |edges: f32| {
                    let e = (level.voxel_size * edges).max(self.level_box_error(index));
                    (e, VoxelShape::Mixed(e))
                };
                vec![
                    (self.level_box_error(index), VoxelShape::Boxes),
                    mixed(1.5 * (1.0 + 2e-4)),
                    mixed(1.6),
                    (cube_error, VoxelShape::Cubes),
                ]
            };
            shapes.retain(|(e, _)| *e <= cube_error * (1.0 + 1e-6));
            shapes.dedup_by(|a, b| (a.0 - b.0).abs() <= f32::EPSILON * b.0.abs());
            for (raw, shape) in shapes {
                let error = raw * scale;
                if error.is_nan() || error <= last_error {
                    continue;
                }
                let surface = self.surface_drawn(index, settings.opacity_dropout, shape);
                let triangles = surface.indices.len() / 3;
                if triangles == 0 || triangles as f32 >= settings.max_cost_ratio * out.root_triangles as f32 {
                    continue;
                }
                last_error = error;
                picked.push((index, error, surface, shape));
                break;
            }
        }
        if picked.is_empty() {
            return Ok(out);
        }
        for &r in &roots {
            let m = &mut out.meshlets.meshlets[r];
            m.parent = sphere;
            m.parent_error = picked[0].1;
        }
        for (n, (index, error, surface, shape)) in picked.iter().enumerate() {
            let parent_error = picked.get(n + 1).map_or(ROOT_ERROR, |p| p.1);
            let dag_level = base_levels + n as u32;
            let vertex_base = out.vertices.len() as u32;
            out.vertices.extend_from_slice(&surface.vertices);
            let first_cluster = out.meshlets.meshlets.len();
            let mut cluster_vertices: Vec<u32> = Vec::new();
            let mut cluster_triangles: Vec<u32> = Vec::new();
            let flush =
                |mesh: &mut MeshletMesh, verts: &mut Vec<u32>, tris: &mut Vec<u32>, all: &[LabVertex]| {
                    if tris.is_empty() {
                        return;
                    }
                    let bounds = sphere_of_points(verts.iter().map(|&v| all[v as usize].position));
                    mesh.meshlets.push(Meshlet {
                        bounds,
                        cone: [0.0, 0.0, 1.0, 1.0],
                        lod: sphere,
                        parent: sphere,
                        lod_error: *error,
                        parent_error,
                        vertex_offset: mesh.vertices.len() as u32,
                        triangle_offset: mesh.triangles.len() as u32,
                        vertex_count: verts.len() as u32,
                        triangle_count: tris.len() as u32,
                        level: dag_level,
                        pad: 0,
                    });
                    mesh.vertices.append(verts);
                    mesh.triangles.append(tris);
                };
            for range in &surface.voxel_ranges {
                let [first_vertex, vertex_count, first_index, index_count] = *range;
                if cluster_vertices.len() + vertex_count as usize > MAX_MESHLET_VERTICES
                    || cluster_triangles.len() + index_count as usize / 3 > MAX_MESHLET_TRIANGLES
                {
                    flush(
                        &mut out.meshlets,
                        &mut cluster_vertices,
                        &mut cluster_triangles,
                        &out.vertices,
                    );
                }
                let local_base = cluster_vertices.len() as u32;
                cluster_vertices.extend((0..vertex_count).map(|v| vertex_base + first_vertex + v));
                let idx = &surface.indices[first_index as usize..(first_index + index_count) as usize];
                for tri in idx.as_chunks::<3>().0 {
                    let local = tri.map(|v| v - first_vertex + local_base);
                    cluster_triangles.push(local[0] | (local[1] << 8) | (local[2] << 16));
                }
            }
            flush(
                &mut out.meshlets,
                &mut cluster_vertices,
                &mut cluster_triangles,
                &out.vertices,
            );
            out.levels.push(VoxelFarLevelUse {
                level: *index,
                dag_level,
                error: *error,
                clusters: (out.meshlets.meshlets.len() - first_cluster) as u32,
                triangles: (surface.indices.len() / 3) as u32,
                vertices: surface.vertices.len() as u32,
                shape: *shape,
            });
        }
        out.meshlets.source_vertex_count = out.vertices.len() as u32;
        out.meshlets
            .validate()
            .map_err(|_| VoxelFarFormatError::Invalid("extended DAG does not validate"))?;
        Ok(out)
    }
}

/// A description of the `.fvox` layout for `cargo xtask check-formats` (the format has no serde
/// shape, so the layout description is what is fingerprinted, registered as `voxel-far`).
#[must_use]
pub fn voxel_far_schema_fingerprint() -> String {
    format!(
        "FVOX;v{VOXEL_FAR_FORMAT_VERSION};f32x3 origin;f32 extent;f32x4 bounds;u32 source_triangles;\
         u32 level_count(<=32);u32 brick_count;u32 voxel_count;\
         level:[u32 resolution(1..={MAX_VOXEL_FAR_RESOLUTION});f32 voxel_size;f32 error;u32 brick_offset;u32 brick_count];\
         brick:[u32 coord(10:10:10);u32 voxel_offset;u32 mask_lo;u32 mask_hi];\
         voxel:[u32 normal(oct16:16);u32 color(rgb8,opacity8);f32x2 uv];\
         extent:[u32 box(x lo4 hi4,y lo4 hi4,z lo4 hi4,inexact1,reserved7;steps 15)];\
         sampled_error:[f32 per level,0<=sampled<=level.error];\
         u32 coverage_count(0|level_count);coverage:[f32 per level,0<=coverage<={MAX_VOXEL_COVERAGE}];\
         level.error>=max extent bound"
    )
}

/// The `from` versions of the voxel far format's migration steps: v1 (whole cubes) loads with
/// every box full and the cube-diagonal error, v2 with no sampled box error and v3 with no
/// coverage estimate, so all three migrate to v4 unchanged in look
/// ([`migrate_cooked_voxel_far_file`]); a recook measures the coverage.
#[must_use]
pub fn voxel_far_migration_sources() -> Vec<u32> {
    vec![1, 2, 3]
}

/// Rewrites an older cooked voxel far file (`FCKD` wrapper) at [`VOXEL_FAR_FORMAT_VERSION`],
/// the cooked header kept: `Ok(None)` when the file is another cooked type, not a cooked file,
/// or already current (for `regia upgrade-project`). A v1 proxy keeps its cubes and error; a
/// recook gives it tight boxes.
///
/// # Errors
/// A voxel far file that does not parse.
pub fn migrate_cooked_voxel_far_file(bytes: &[u8]) -> Result<Option<Vec<u8>>, String> {
    match cooked_voxel_far_file_version(bytes)? {
        Some(version) if version < VOXEL_FAR_FORMAT_VERSION => {}
        _ => return Ok(None),
    }
    let (mut header, payload) = regia_asset::decode_cooked_file(bytes).map_err(|e| e.to_string())?;
    let proxy = VoxelFarProxy::from_bytes(payload).map_err(|e| e.to_string())?;
    let out = proxy.to_bytes();
    header.payload_len = out.len() as u64;
    regia_asset::encode_cooked_file(&header, &out)
        .map(Some)
        .map_err(|e| e.to_string())
}

/// The payload version of a cooked voxel far file (`FCKD` wrapper), `Ok(None)` when the file is
/// another cooked type or not a cooked file (for `regia upgrade-project`).
///
/// # Errors
/// A voxel far file whose payload is shorter than its header or has a bad magic.
pub fn cooked_voxel_far_file_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    let Ok((header, payload)) = regia_asset::decode_cooked_file(bytes) else {
        return Ok(None);
    };
    if header.type_tag != regia_core::id::TypePathHash::of(VOXEL_FAR_TYPE_PATH) {
        return Ok(None);
    }
    match payload {
        [m0, m1, m2, m3, v0, v1, v2, v3, ..] if [*m0, *m1, *m2, *m3] == MAGIC => {
            Ok(Some(u32::from_le_bytes([*v0, *v1, *v2, *v3])))
        }
        _ => Err("cooked voxel far proxy has a bad magic".to_string()),
    }
}

/// Bytes of the blob header: magic, version, origin, extent, bounds, four counts.
const HEADER_BYTES: usize = 4 + 4 + 12 + 4 + 16 + 16;

#[cfg(test)]
mod tests;
