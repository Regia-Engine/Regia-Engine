// SPDX-License-Identifier: MPL-2.0
//! Cooked voxel far proxies (`.fvox`, ADR-2445): a sparse-brick voxel stand-in per mesh primitive
//! for the distances past its last cluster LOD (`regia_render::visbuffer::voxel_far`).
//!
//! With `voxel_far: Some((...))` in a glTF's `.meta` settings, `regia cook` writes one
//! [`regia_render::visbuffer::VOXEL_FAR_TYPE_PATH`] sub-asset per cooked mesh primitive
//! (`voxels/<node>/primitive_<i>`), built over the cooked mesh as the runtime decodes it
//! ([`regia_render::visbuffer::cooked_mesh_lod0`]), so it shares the object space of the
//! primitive's meshlets.
//!
//! **Voxelization** is conservative surface voxelization: a voxel is occupied when a triangle
//! overlaps its box, tested on the 13 separating axes of Akenine-Möller ("Fast 3D Triangle-Box
//! Overlap Testing", JGT 6(1), 2001). Each triangle's area is shared evenly by the voxels it
//! overlaps, and each voxel prefilters what it holds (after Heitz and Neyret, "Representing
//! Appearance and Pre-filtering Subpixel Data in Sparse Voxel Octrees", HPG 2012): the
//! area-weighted mean normal and vertex color, an opacity (the share of view rays its surface stops:
//! a turbid-medium leaf attenuation blended towards 1 by how planar the surface is, `opacity`), and
//! the texture coordinate of the surface point nearest its centre.
//! Coarser levels merge 2×2×2 children with the same sums, the sparse-voxel mip chain of Crassin
//! et al. ("GigaVoxels", I3D 2009) and Laine and Karras ("Efficient Sparse Voxel Octrees",
//! I3D 2010). Occupied voxels are stored in 4×4×4 bricks with a 64-bit mask; bricks are sorted
//! by Morton order so the runtime's face clusters are spatially compact.
//!
//! **Sub-voxel boxes** (format v2, ADR-2655): each triangle is clipped to every voxel it overlaps
//! (Sutherland and Hodgman, "Reentrant Polygon Clipping", CACM 17(1), 1974) and the voxel keeps
//! the bounding box of the clipped pieces. A coarser voxel's box is the union of its children's
//! (exact, in object units), quantized outward to fifteenths of the voxel at each level. A level's
//! error is the largest [`regia_render::visbuffer::voxel_extent_error`] of its boxes: the box of a
//! surface touches it on every face, which bounds how far a drawn point can be from it.
//!
//! Versioning (`specs/12` § 4a): the payload carries
//! [`regia_render::visbuffer::VOXEL_FAR_FORMAT_VERSION`]; proxies are derived data, so the
//! migration is a recook: the cache key hashes the cooked mesh bytes, the settings, the format
//! version and [`VOXEL_FAR_COOK_VERSION`].

use std::collections::BTreeMap;
use std::path::Path;

use regia_asset::{
    AssetId, COOKED_FORMAT_VERSION, CookedHeader, CookedMesh, ManifestEntry, SubAssetEntry, cooked_file_name,
    encode_cooked_file,
};
use regia_render::visbuffer::{
    MAX_VOXEL_COVERAGE, MAX_VOXEL_FAR_RESOLUTION, VOXEL_BRICK_EDGE, VOXEL_EXTENT_FULL, VOXEL_EXTENT_INEXACT,
    VOXEL_EXTENT_STEPS, VOXEL_FAR_TYPE_PATH, VoxelBrick, VoxelFarProxy, VoxelLevel, VoxelSample,
    pack_voxel_extent, sphere_of_points, unpack_voxel_extent, voxel_extent_error, voxel_far_sub_path,
};
use serde::{Deserialize, Serialize};

use crate::gltf_extensions::{GltfExtensionTable, TextureTransform};

/// Bumped whenever the voxel cooker's output changes for the same input.
pub const VOXEL_FAR_COOK_VERSION: u32 = 4;

const MESH_TYPE_PATH: &str = "regia_asset::CookedMesh";
const MODEL_TYPE_PATH: &str = "regia_asset::CookedModel";
const MATERIAL_TYPE_PATH: &str = "regia_asset::CookedStandardMaterial";
const TEXTURE_TYPE_PATH: &str = "regia_asset::CookedTexture";
/// A unit cube's diagonal: the farthest a conservatively filled voxel reaches past the surface
/// that touches it, so the error bound of a voxel drawn full (one with no exact box).
const DIAGONAL: f32 = 1.732_050_8;

/// The `.meta` `voxel_far` block.
#[derive(Clone, Copy, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct VoxelFarCookSettings {
    /// Voxels along the longest axis at level 0, rounded up to a power of two (1 to 256;
    /// default 32). Coarser levels halve it down to one voxel.
    pub resolution: u32,
}

impl Default for VoxelFarCookSettings {
    fn default() -> Self {
        Self { resolution: 32 }
    }
}

impl VoxelFarCookSettings {
    /// The level-0 resolution the cooker uses: a power of two in 1..=256.
    #[must_use]
    pub fn effective_resolution(&self) -> u32 {
        self.resolution
            .clamp(1, MAX_VOXEL_FAR_RESOLUTION)
            .next_power_of_two()
    }
}

/// Why a primitive's voxel proxy was not cooked.
#[derive(Debug, thiserror::Error)]
pub enum VoxelFarCookError {
    /// The cooked mesh payload does not decode.
    #[error("{path}: cooked mesh: {reason}")]
    Mesh {
        /// Sub-asset path.
        path: String,
        /// Decoder message.
        reason: String,
    },
    /// The mesh has no finite triangles to voxelize.
    #[error("{path}: no triangles to voxelize")]
    Empty {
        /// Sub-asset path.
        path: String,
    },
    /// Writing the cooked file failed.
    #[error("writing voxel proxy: {0}")]
    Io(#[from] std::io::Error),
    /// Encoding the cooked wrapper failed.
    #[error("cooked voxel wrapper: {0}")]
    Cooked(String),
}

/// What [`cook_voxel_far_sub_assets`] produced.
#[derive(Debug, Default)]
pub struct VoxelFarCookOutput {
    /// Manifest rows (`<source>#voxels/...`).
    pub manifest: Vec<ManifestEntry>,
    /// `.meta` sub-asset rows.
    pub meta: Vec<SubAssetEntry>,
    /// Primitives whose proxies were rebuilt (the rest were current in the cache).
    pub built: usize,
}

/// The surface data the voxelizer reads; `normals`, `uvs` and `colors` may be empty (or any
/// length other than `positions`'s), in which case geometric normals, zero UVs and white are
/// used.
#[derive(Clone, Copy, Debug)]
pub struct VoxelizeInput<'a> {
    /// Object-space positions.
    pub positions: &'a [[f32; 3]],
    /// Vertex normals.
    pub normals: &'a [[f32; 3]],
    /// Texture coordinates.
    pub uvs: &'a [[f32; 2]],
    /// Linear vertex colors.
    pub colors: &'a [[f32; 4]],
    /// Triangle list.
    pub indices: &'a [u32],
}

#[derive(Clone, Copy)]
struct Accum {
    /// Covered surface area (the alpha test's share of `full`; all of it for opaque geometry).
    area: f32,
    /// Geometric surface area, covered or not (ADR-3108): `area / full` is the alpha coverage
    /// of the surface in the voxel.
    full: f32,
    normal: [f32; 3],
    color: [f32; 3],
    /// The surface point nearest the voxel's centre seen so far, its squared distance and UV.
    point: [f32; 3],
    best_d2: f32,
    uv: [f32; 2],
    /// Object-space bounding box of the surface pieces inside the voxel (empty: `lo > hi`).
    lo: V3,
    hi: V3,
    /// False when a piece could not be bounded (a sliver given to its centroid's voxel): the
    /// voxel is drawn full and its error is the cube diagonal.
    exact: bool,
    /// Covered area projected along each axis (`area` times the normal's absolute component,
    /// ADR-3227): what the covered surface shows in the three axis views.
    proj: V3,
    /// Holds alpha-tested geometry with part of it cut away (ADR-3227): its opacity is fitted to
    /// the drawn box ([`box_fitted_opacity`]).
    masked: bool,
}

impl Default for Accum {
    fn default() -> Self {
        Self {
            area: 0.0,
            full: 0.0,
            normal: [0.0; 3],
            color: [0.0; 3],
            point: [0.0; 3],
            best_d2: f32::MAX,
            uv: [0.0; 2],
            lo: [f32::MAX; 3],
            hi: [f32::MIN; 3],
            exact: true,
            proj: [0.0; 3],
            masked: false,
        }
    }
}

impl Accum {
    /// Keeps `point` (with `uv`) when it is nearer `center` than the current one (ties keep the
    /// first, so the result follows the deterministic input order).
    fn offer(&mut self, center: V3, point: V3, uv: [f32; 2]) {
        let d = sub(point, center);
        let d2 = dot(d, d);
        if d2 < self.best_d2 {
            self.best_d2 = d2;
            self.point = point;
            // A non-finite UV would be stored on the sample and sampled at runtime.
            self.uv = if uv.iter().all(|c| c.is_finite()) {
                uv
            } else {
                [0.0, 0.0]
            };
        }
    }

    /// Adds a child voxel's sums; the representative point is re-chosen against the parent's
    /// `center`.
    fn merge(&mut self, other: &Accum, center: V3) {
        self.area += other.area;
        self.full += other.full;
        for k in 0..3 {
            self.normal[k] += other.normal[k];
            self.color[k] += other.color[k];
        }
        self.offer(center, other.point, other.uv);
        self.grow(other.lo, other.hi);
        self.exact &= other.exact;
        for k in 0..3 {
            self.proj[k] += other.proj[k];
        }
        self.masked |= other.masked;
    }

    /// Grows the surface box by `lo..hi`.
    fn grow(&mut self, lo: V3, hi: V3) {
        for k in 0..3 {
            self.lo[k] = self.lo[k].min(lo[k]);
            self.hi[k] = self.hi[k].max(hi[k]);
        }
    }
}

type V3 = [f32; 3];
/// A voxel a triangle overlaps and the bounding box of the triangle's piece inside it (`None`:
/// the sliver fallback, not bounded).
type Hit = ([u32; 3], Option<(V3, V3)>);

fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn dot(a: V3, b: V3) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn cross(a: V3, b: V3) -> V3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

/// True when the triangle `v` overlaps the axis-aligned box at `center` with half size `half`
/// (Akenine-Möller 2001: the box's three axes, the triangle's normal, and the nine edge × axis
/// cross products).
#[must_use]
pub fn triangle_overlaps_box(center: V3, half: V3, v: [V3; 3]) -> bool {
    let p = v.map(|x| sub(x, center));
    let edges = [sub(p[1], p[0]), sub(p[2], p[1]), sub(p[0], p[2])];
    let axes = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    let separated = |axis: V3| {
        let r = half[0] * axis[0].abs() + half[1] * axis[1].abs() + half[2] * axis[2].abs();
        let d = [dot(p[0], axis), dot(p[1], axis), dot(p[2], axis)];
        let lo = d[0].min(d[1]).min(d[2]);
        let hi = d[0].max(d[1]).max(d[2]);
        lo > r || hi < -r
    };
    for e in edges {
        for a in axes {
            let axis = cross(e, a);
            if dot(axis, axis) > 1e-30 && separated(axis) {
                return false;
            }
        }
    }
    for a in axes {
        if separated(a) {
            return false;
        }
    }
    let n = cross(edges[0], edges[1]);
    !(dot(n, n) > 1e-30 && separated(n))
}

/// The bounding box of triangle `v` clipped to the box `lo..hi` (Sutherland-Hodgman against its
/// six planes), `None` when nothing of it is inside.
#[must_use]
pub fn clipped_triangle_bounds(v: [V3; 3], lo: V3, hi: V3) -> Option<(V3, V3)> {
    let mut poly: Vec<V3> = v.to_vec();
    let mut next: Vec<V3> = Vec::with_capacity(9);
    for k in 0..3 {
        for (plane, above) in [(lo[k], true), (hi[k], false)] {
            next.clear();
            let side = |p: V3| if above { p[k] - plane } else { plane - p[k] };
            let n = poly.len();
            for (i, &a) in poly.iter().enumerate() {
                let b = poly[(i + 1) % n];
                let (da, db) = (side(a), side(b));
                if da >= 0.0 {
                    next.push(a);
                }
                if (da >= 0.0) != (db >= 0.0) {
                    let t = da / (da - db);
                    let mut p: V3 = std::array::from_fn(|j| a[j] + (b[j] - a[j]) * t);
                    p[k] = plane;
                    next.push(p);
                }
            }
            std::mem::swap(&mut poly, &mut next);
            if poly.is_empty() {
                return None;
            }
        }
    }
    let lo: V3 = std::array::from_fn(|k| poly.iter().map(|p| p[k]).fold(f32::MAX, f32::min));
    let hi: V3 = std::array::from_fn(|k| poly.iter().map(|p| p[k]).fold(f32::MIN, f32::max));
    Some((lo, hi))
}

/// The packed sub-voxel box of `a` in the voxel whose low corner is `corner` (edge `size`): the
/// surface box clamped to the voxel and rounded outward to [`VOXEL_EXTENT_STEPS`]; a box with
/// no size on two or more axes gets one step on them (a line or point draws no face). `None` when
/// the voxel has no exact box (drawn full).
fn quantized_extent(a: &Accum, corner: V3, size: f32) -> Option<u32> {
    if !a.exact || (0..3).any(|k| a.lo[k] > a.hi[k]) {
        return None;
    }
    let steps = VOXEL_EXTENT_STEPS as f32;
    let mut lo = [0u32; 3];
    let mut hi = [0u32; 3];
    for k in 0..3 {
        let f = |v: f32| ((v - corner[k]) / size * steps).clamp(0.0, steps);
        lo[k] = f(a.lo[k]).floor() as u32;
        hi[k] = (f(a.hi[k]).ceil() as u32).max(lo[k]);
    }
    if (0..3).filter(|&k| hi[k] == lo[k]).count() >= 2 {
        for k in 0..3 {
            if hi[k] == lo[k] {
                if hi[k] < VOXEL_EXTENT_STEPS {
                    hi[k] += 1;
                } else {
                    lo[k] -= 1;
                }
            }
        }
    }
    Some(pack_voxel_extent(lo, hi))
}

/// Barycentric coordinates of the point of triangle `v` nearest `q` (Ericson, *Real-Time
/// Collision Detection*, 2005, § 5.1.5).
fn closest_barycentric(q: V3, v: [V3; 3]) -> V3 {
    let (a, b, c) = (v[0], v[1], v[2]);
    let ab = sub(b, a);
    let ac = sub(c, a);
    let ap = sub(q, a);
    let d1 = dot(ab, ap);
    let d2 = dot(ac, ap);
    if d1 <= 0.0 && d2 <= 0.0 {
        return [1.0, 0.0, 0.0];
    }
    let bp = sub(q, b);
    let d3 = dot(ab, bp);
    let d4 = dot(ac, bp);
    if d3 >= 0.0 && d4 <= d3 {
        return [0.0, 1.0, 0.0];
    }
    let vc = d1 * d4 - d3 * d2;
    if vc <= 0.0 && d1 >= 0.0 && d3 <= 0.0 {
        let t = d1 / (d1 - d3);
        return [1.0 - t, t, 0.0];
    }
    let cp = sub(q, c);
    let d5 = dot(ab, cp);
    let d6 = dot(ac, cp);
    if d6 >= 0.0 && d5 <= d6 {
        return [0.0, 0.0, 1.0];
    }
    let vb = d5 * d2 - d1 * d6;
    if vb <= 0.0 && d2 >= 0.0 && d6 <= 0.0 {
        let t = d2 / (d2 - d6);
        return [1.0 - t, 0.0, t];
    }
    let va = d3 * d6 - d5 * d4;
    if va <= 0.0 && (d4 - d3) >= 0.0 && (d5 - d6) >= 0.0 {
        let t = (d4 - d3) / ((d4 - d3) + (d5 - d6));
        return [0.0, 1.0 - t, t];
    }
    let denom = 1.0 / (va + vb + vc);
    let (bv, cv) = (vb * denom, vc * denom);
    [1.0 - bv - cv, bv, cv]
}

/// Sub-triangles per edge at most when an alpha-tested triangle is split (ADR-3108).
const MAX_SUBDIVISIONS: u32 = 64;
/// Alpha-test samples per triangle edge at most (sub-triangles times samples per sub-triangle).
const MAX_SAMPLES_PER_EDGE: u32 = 512;

/// The alpha test of a masked material (glTF `alphaMode: MASK`), as the masked raster applies it
/// (ADR-3009): a point is covered when the base color texture's alpha at its texture coordinate
/// (through the texture's `KHR_texture_transform`, bilinear, repeating, level 0) times `factor`
/// times the vertex alpha is at least `cutoff`. The coverage reads the base color alpha only: a
/// vertex alpha that carries occlusion (`vertex_ao`) stays a shading term (ADR-3227).
#[derive(Clone, Copy, Debug)]
pub struct VoxelCoverage<'a> {
    /// Texture width in texels.
    pub width: u32,
    /// Texture height in texels.
    pub height: u32,
    /// Row-major alpha, `width * height` bytes; any other length reads as alpha 1 everywhere (a
    /// material with no base color texture).
    pub alpha: &'a [u8],
    /// The base color factor's alpha.
    pub factor: f32,
    /// The material's alpha cutoff.
    pub cutoff: f32,
    /// Multiply by the vertex color's alpha (off when the alpha carries vertex occlusion, the
    /// importer's `vertex_ao` setting).
    pub vertex_alpha: bool,
    /// The base color texture's `KHR_texture_transform` (ADR-3227): applied to a mesh texture
    /// coordinate before the alpha lookup. [`TextureTransform::IDENTITY`] when there is none.
    pub uv_transform: TextureTransform,
}

impl VoxelCoverage<'_> {
    /// The texture coordinate the alpha lookup reads for the mesh texture coordinate `uv`
    /// ([`Self::uv_transform`] applied).
    #[must_use]
    pub fn texture_uv(&self, uv: [f32; 2]) -> [f32; 2] {
        if self.uv_transform == TextureTransform::IDENTITY {
            uv
        } else {
            self.uv_transform.apply(uv)
        }
    }

    /// The texture's alpha at the mesh texture coordinate `uv`: transformed by
    /// [`Self::texture_uv`], then bilinear between texel centres, repeating at the edges.
    #[must_use]
    pub fn alpha_at(&self, uv: [f32; 2]) -> f32 {
        let (w, h) = (self.width as usize, self.height as usize);
        if w == 0 || h == 0 || self.alpha.len() != w * h {
            return 1.0;
        }
        let uv = self.texture_uv(uv);
        let x = uv[0] * w as f32 - 0.5;
        let y = uv[1] * h as f32 - 0.5;
        let (x0, y0) = (x.floor(), y.floor());
        let (fx, fy) = (x - x0, y - y0);
        let wrap = |i: f32, n: usize| (i as i64).rem_euclid(n as i64) as usize;
        let texel = |xi: f32, yi: f32| f32::from(self.alpha[wrap(yi, h) * w + wrap(xi, w)]) / 255.0;
        let top = texel(x0, y0) * (1.0 - fx) + texel(x0 + 1.0, y0) * fx;
        let bottom = texel(x0, y0 + 1.0) * (1.0 - fx) + texel(x0 + 1.0, y0 + 1.0) * fx;
        top * (1.0 - fy) + bottom * fy
    }

    /// Whether the alpha test keeps a point at `uv` with vertex alpha `vertex_alpha`.
    #[must_use]
    pub fn passes(&self, uv: [f32; 2], vertex_alpha: f32) -> bool {
        let vertex = if self.vertex_alpha { vertex_alpha } else { 1.0 };
        self.alpha_at(uv) * self.factor * vertex >= self.cutoff
    }
}

/// The level-0 grid of a voxelization.
struct Grid {
    origin: V3,
    size: f32,
    resolution: u32,
}

impl Grid {
    fn cell(&self, v: f32, k: usize) -> u32 {
        (((v - self.origin[k]) / self.size).floor().max(0.0) as u32).min(self.resolution - 1)
    }

    fn center(&self, xyz: [u32; 3]) -> V3 {
        std::array::from_fn(|k| self.origin[k] + (xyz[k] as f32 + 0.5) * self.size)
    }

    /// The voxel box of `xyz`, inflated a hair so faces exactly on a voxel boundary land on both
    /// sides.
    fn inflated_box(&self, xyz: [u32; 3]) -> (V3, V3) {
        let center = self.center(xyz);
        let half = self.size * 0.5 * 1.0001;
        (center.map(|c| c - half), center.map(|c| c + half))
    }

    /// The voxels triangle `v` overlaps, each with the bounds of its piece inside; a sub-voxel
    /// sliver missed by rounding gets the voxel of its centroid, unbounded.
    fn overlaps(&self, hits: &mut Vec<Hit>, v: [V3; 3]) {
        let tlo: [u32; 3] = std::array::from_fn(|k| self.cell(v[0][k].min(v[1][k]).min(v[2][k]), k));
        let thi: [u32; 3] = std::array::from_fn(|k| self.cell(v[0][k].max(v[1][k]).max(v[2][k]), k));
        let half = [self.size * 0.5; 3];
        hits.clear();
        for z in tlo[2]..=thi[2] {
            for y in tlo[1]..=thi[1] {
                for x in tlo[0]..=thi[0] {
                    let center = self.center([x, y, z]);
                    let inflated = half.map(|h| h * 1.0001);
                    if triangle_overlaps_box(center, inflated, v) {
                        let lo: V3 = std::array::from_fn(|k| center[k] - inflated[k]);
                        let hi: V3 = std::array::from_fn(|k| center[k] + inflated[k]);
                        hits.push(([x, y, z], clipped_triangle_bounds(v, lo, hi)));
                    }
                }
            }
        }
        if hits.is_empty() {
            let c: V3 = std::array::from_fn(|k| (v[0][k] + v[1][k] + v[2][k]) / 3.0);
            hits.push((std::array::from_fn(|k| self.cell(c[k], k)), None));
        }
    }
}

/// The surface pieces each voxel holds (whole triangles, or the covered sub-triangles of an
/// alpha-tested one), for the sampled box error (ADR-3108).
#[derive(Default)]
struct Sources {
    pieces: Vec<[V3; 3]>,
    per_voxel: BTreeMap<[u32; 3], Vec<u32>>,
}

impl Sources {
    fn piece(&mut self, v: [V3; 3]) -> u32 {
        self.pieces.push(v);
        self.pieces.len() as u32 - 1
    }

    fn hold(&mut self, xyz: [u32; 3], piece: u32) {
        let list = self.per_voxel.entry(xyz).or_default();
        if list.last() != Some(&piece) {
            list.push(piece);
        }
    }

    /// The pieces of each parent voxel: its children's, sorted, once each.
    fn parents(&self) -> BTreeMap<[u32; 3], Vec<u32>> {
        let mut out: BTreeMap<[u32; 3], Vec<u32>> = BTreeMap::new();
        for (xyz, list) in &self.per_voxel {
            out.entry(xyz.map(|v| v / 2)).or_default().extend_from_slice(list);
        }
        for list in out.values_mut() {
            list.sort_unstable();
            list.dedup();
        }
        out
    }
}

/// What a triangle deposits besides its area: unit shading normal, mean vertex color and the
/// corners' texture coordinates (`None`: no UVs).
struct Surface {
    unit: V3,
    color: V3,
    uv3: Option<[[f32; 2]; 3]>,
}

/// Shares an opaque (or fully covered) triangle's area evenly among the voxels it overlaps.
fn deposit(
    level0: &mut BTreeMap<[u32; 3], Accum>,
    (hits, sources): (&mut Vec<Hit>, &mut Sources),
    grid: &Grid,
    v: [V3; 3],
    area: f32,
    surface: &Surface,
) {
    grid.overlaps(hits, v);
    let share = area / hits.len() as f32;
    let source = sources.piece(v);
    for &(xyz, piece) in hits.iter() {
        sources.hold(xyz, source);
        let center = grid.center(xyz);
        let entry = level0.entry(xyz).or_default();
        match piece {
            Some((lo, hi)) => entry.grow(lo, hi),
            None => entry.exact = false,
        }
        entry.area += share;
        entry.full += share;
        for k in 0..3 {
            entry.normal[k] += surface.unit[k] * share;
            entry.color[k] += surface.color[k] * share;
            entry.proj[k] += surface.unit[k].abs() * share;
        }
        let b = closest_barycentric(center, v);
        let point = std::array::from_fn(|k| v[0][k] * b[0] + v[1][k] * b[1] + v[2][k] * b[2]);
        let uv = match surface.uv3 {
            Some(uv) => [
                uv[0][0] * b[0] + uv[1][0] * b[1] + uv[2][0] * b[2],
                uv[0][1] * b[0] + uv[1][1] * b[1] + uv[2][1] * b[2],
            ],
            None => [0.0, 0.0],
        };
        entry.offer(center, point, uv);
    }
}

/// One alpha-test sample of a split triangle.
#[derive(Clone, Copy)]
struct Sample {
    point: V3,
    uv: [f32; 2],
    covered: bool,
}

/// Deposits one sub-triangle of an alpha-tested triangle: each sample stands for `sample_area`
/// in the voxel it lies in. A voxel with a covered sample gets the covered area, the geometric
/// area, the sub-triangle's piece box and a covered sample as its representative point; a voxel
/// with none only records its geometric area in `uncovered`.
fn deposit_masked(
    level0: &mut BTreeMap<[u32; 3], Accum>,
    (uncovered, sources): (&mut Vec<([u32; 3], f32)>, &mut Sources),
    grid: &Grid,
    corners: [V3; 3],
    samples: &[Sample],
    sample_area: f32,
    surface: &Surface,
) {
    let voxel_of = |p: V3| -> [u32; 3] { std::array::from_fn(|k| grid.cell(p[k], k)) };
    let mut source = None;
    let mut counts: BTreeMap<[u32; 3], (u32, u32)> = BTreeMap::new();
    for sample in samples {
        let count = counts.entry(voxel_of(sample.point)).or_default();
        count.1 += 1;
        count.0 += u32::from(sample.covered);
    }
    for (&xyz, &(covered, total)) in &counts {
        let full = sample_area * total as f32;
        if covered == 0 {
            uncovered.push((xyz, full));
            continue;
        }
        let area = sample_area * covered as f32;
        let piece = *source.get_or_insert_with(|| sources.piece(corners));
        sources.hold(xyz, piece);
        let center = grid.center(xyz);
        let (lo, hi) = grid.inflated_box(xyz);
        let entry = level0.entry(xyz).or_default();
        match clipped_triangle_bounds(corners, lo, hi) {
            Some((plo, phi)) => entry.grow(plo, phi),
            None => {
                for sample in samples.iter().filter(|s| s.covered && voxel_of(s.point) == xyz) {
                    entry.grow(sample.point, sample.point);
                }
            }
        }
        entry.area += area;
        entry.full += full;
        entry.masked = true;
        for k in 0..3 {
            entry.normal[k] += surface.unit[k] * area;
            entry.color[k] += surface.color[k] * area;
            entry.proj[k] += surface.unit[k].abs() * area;
        }
        for sample in samples.iter().filter(|s| s.covered && voxel_of(s.point) == xyz) {
            entry.offer(center, sample.point, sample.uv);
        }
    }
}

/// The alpha-test samples of one triangle: split into `n`² sub-triangles of at most half a voxel
/// edge, each sampled at the centroids of its own `m`² sub-division (stratified, so each sample
/// stands for an equal area), `m` set by the texels the sub-triangle spans.
#[derive(Default)]
struct MicroScratch {
    subs: Vec<[V3; 3]>,
    offsets: Vec<u32>,
    samples: Vec<Sample>,
    covered: u32,
    total: u32,
}

/// Barycentric `(a, b)` corners of the `n`² triangles of a regular subdivision, upward then
/// downward per cell, rows in order.
fn lattice(n: u32) -> impl Iterator<Item = [[f32; 2]; 3]> {
    let f = n as f32;
    (0..n).flat_map(move |j| {
        (0..n - j).flat_map(move |i| {
            let (a, b) = (i as f32, j as f32);
            let up = [[a / f, b / f], [(a + 1.0) / f, b / f], [a / f, (b + 1.0) / f]];
            let down = (i + j + 2 <= n).then_some([
                [(a + 1.0) / f, b / f],
                [a / f, (b + 1.0) / f],
                [(a + 1.0) / f, (b + 1.0) / f],
            ]);
            std::iter::once(up).chain(down)
        })
    })
}

impl MicroScratch {
    fn samples_of(&self, sub: usize) -> &[Sample] {
        &self.samples[self.offsets[sub] as usize..self.offsets[sub + 1] as usize]
    }

    fn sample(&mut self, mask: &VoxelCoverage<'_>, v: [V3; 3], uv: [[f32; 2]; 3], va: [f32; 3], size: f32) {
        self.subs.clear();
        self.offsets.clear();
        self.samples.clear();
        self.covered = 0;
        self.total = 0;
        let len = |a: V3, b: V3| dot(sub(a, b), sub(a, b)).sqrt();
        let longest = len(v[0], v[1]).max(len(v[1], v[2])).max(len(v[2], v[0]));
        let n = ((longest / (size * 0.5)).ceil() as u32).clamp(1, MAX_SUBDIVISIONS);
        let (w, h) = (mask.width.max(1) as f32, mask.height.max(1) as f32);
        let texels = |a: [f32; 2], b: [f32; 2]| ((a[0] - b[0]) * w).hypot((a[1] - b[1]) * h);
        // Texels spanned in texture space (the transform is affine: its corners' span).
        let tuv = uv.map(|t| mask.texture_uv(t));
        let span = texels(tuv[0], tuv[1])
            .max(texels(tuv[1], tuv[2]))
            .max(texels(tuv[2], tuv[0]));
        let span = if span.is_finite() { span } else { 0.0 };
        let m = ((span / n as f32).ceil() as u32)
            .clamp(2, 16)
            .min((MAX_SAMPLES_PER_EDGE / n).max(1));
        let at = |ab: [f32; 2]| -> (V3, [f32; 2], f32) {
            let (a, b) = (ab[0], ab[1]);
            let p = std::array::from_fn(|k| v[0][k] + a * (v[1][k] - v[0][k]) + b * (v[2][k] - v[0][k]));
            let t = std::array::from_fn(|k| uv[0][k] + a * (uv[1][k] - uv[0][k]) + b * (uv[2][k] - uv[0][k]));
            (p, t, va[0] + a * (va[1] - va[0]) + b * (va[2] - va[0]))
        };
        self.offsets.push(0);
        for q in lattice(n) {
            self.subs.push(q.map(|ab| at(ab).0));
            for local in lattice(m) {
                let x = (local[0][0] + local[1][0] + local[2][0]) / 3.0;
                let y = (local[0][1] + local[1][1] + local[2][1]) / 3.0;
                let ab = std::array::from_fn(|k| q[0][k] + x * (q[1][k] - q[0][k]) + y * (q[2][k] - q[0][k]));
                let (point, t, alpha) = at(ab);
                let covered = mask.passes(t, alpha);
                self.samples.push(Sample {
                    point,
                    uv: t,
                    covered,
                });
                self.covered += u32::from(covered);
                self.total += 1;
            }
            self.offsets.push(self.samples.len() as u32);
        }
    }
}

/// Interleaves the low 10 bits of x, y and z (Morton order).
fn morton(xyz: [u32; 3]) -> u32 {
    let spread = |mut v: u32| {
        v &= 0x3ff;
        v = (v | (v << 16)) & 0x0300_00ff;
        v = (v | (v << 8)) & 0x0300_f00f;
        v = (v | (v << 4)) & 0x030c_30c3;
        (v | (v << 2)) & 0x0924_9249
    };
    spread(xyz[0]) | (spread(xyz[1]) << 1) | (spread(xyz[2]) << 2)
}

/// Voxelizes a triangle mesh into a proxy with `settings`' level-0 resolution and its mip chain.
///
/// # Errors
///
/// No finite, non-degenerate triangle (the message names `path`).
pub fn voxelize(
    path: &str,
    input: &VoxelizeInput<'_>,
    settings: &VoxelFarCookSettings,
) -> Result<VoxelFarProxy, VoxelFarCookError> {
    voxelize_with_coverage(path, input, None, settings)
}

/// [`voxelize`] for alpha-tested geometry (ADR-3108): with `coverage`, each triangle is split into
/// sub-triangles of at most half a level-0 voxel, each sub-triangle's covered share is measured
/// with the material's alpha test at stratified points, and only covered geometry makes voxels
/// (its area weighted by that share; the representative texture coordinate is a covered point).
/// The cut-away area still counts in the voxels the covered geometry occupies, so each voxel's
/// opacity follows the coverage the near level shows (the fill rule in `opacity`). A triangle that
/// is covered at every point cooks exactly as [`voxelize`] cooks it; one that is covered nowhere
/// makes no voxel. `None` is [`voxelize`].
///
/// # Errors
///
/// No finite, non-degenerate triangle, or nothing covered (the message names `path`).
pub fn voxelize_with_coverage(
    path: &str,
    input: &VoxelizeInput<'_>,
    coverage: Option<&VoxelCoverage<'_>>,
    settings: &VoxelFarCookSettings,
) -> Result<VoxelFarProxy, VoxelFarCookError> {
    let _span = tracing::info_span!(
        "voxel_far_voxelize",
        triangles = input.indices.len() / 3,
        masked = coverage.is_some()
    )
    .entered();
    let positions = input.positions;
    let tris: Vec<[u32; 3]> = input
        .indices
        .as_chunks::<3>()
        .0
        .iter()
        .copied()
        .filter(|t| {
            t.iter().all(|&i| {
                positions
                    .get(i as usize)
                    .is_some_and(|p| p.iter().all(|c| c.is_finite()))
            })
        })
        .collect();
    let empty = || VoxelFarCookError::Empty {
        path: path.to_string(),
    };
    if tris.is_empty() {
        return Err(empty());
    }
    let mut lo = [f32::MAX; 3];
    let mut hi = [f32::MIN; 3];
    for t in &tris {
        for &i in t {
            let p = positions[i as usize];
            for k in 0..3 {
                lo[k] = lo[k].min(p[k]);
                hi[k] = hi[k].max(p[k]);
            }
        }
    }
    let longest = (0..3).map(|k| hi[k] - lo[k]).fold(0.0f32, f32::max);
    let resolution = settings.effective_resolution();
    // A small margin keeps faces on the AABB inside the grid; a flat mesh still gets a voxel.
    let extent = (longest * (1.0 + 1e-3)).max(1e-4);
    let origin: V3 = std::array::from_fn(|k| (lo[k] + hi[k]) * 0.5 - extent * 0.5);
    let size = extent / resolution as f32;
    let has_normals = input.normals.len() == positions.len();
    let has_uvs = input.uvs.len() == positions.len();
    let has_colors = input.colors.len() == positions.len();

    let grid = Grid {
        origin,
        size,
        resolution,
    };
    let mut level0: BTreeMap<[u32; 3], Accum> = BTreeMap::new();
    let mut hits: Vec<Hit> = Vec::new();
    // Uncovered pieces of alpha-tested triangles: their area counts towards the voxels the covered
    // geometry occupies (after every triangle, so the order of triangles does not matter), and
    // never makes a voxel of its own.
    let mut uncovered: Vec<([u32; 3], f32)> = Vec::new();
    let mut micro = MicroScratch::default();
    let mut sources = Sources::default();
    for t in &tris {
        let v = t.map(|i| positions[i as usize]);
        let n = cross(sub(v[1], v[0]), sub(v[2], v[0]));
        let area = 0.5 * dot(n, n).sqrt();
        if area.is_nan() || area <= 0.0 {
            continue;
        }
        let shading = if has_normals {
            let s = t.iter().fold([0.0f32; 3], |acc, &i| {
                let m = input.normals[i as usize];
                [acc[0] + m[0], acc[1] + m[1], acc[2] + m[2]]
            });
            if dot(s, s) > 1e-20 { s } else { n }
        } else {
            n
        };
        let shading_len = dot(shading, shading).sqrt().max(1e-20);
        let unit = shading.map(|c| c / shading_len);
        let color = if has_colors {
            let c = t.iter().fold([0.0f32; 3], |acc, &i| {
                let m = input.colors[i as usize];
                [acc[0] + m[0], acc[1] + m[1], acc[2] + m[2]]
            });
            c.map(|x| (x / 3.0).clamp(0.0, 1.0))
        } else {
            [1.0; 3]
        };
        let uv3 = has_uvs.then(|| t.map(|i| input.uvs[i as usize]));
        let surface = Surface { unit, color, uv3 };
        let Some(mask) = coverage else {
            deposit(&mut level0, (&mut hits, &mut sources), &grid, v, area, &surface);
            continue;
        };
        let vertex_alpha = if mask.vertex_alpha && has_colors {
            t.map(|i| input.colors[i as usize][3])
        } else {
            [1.0; 3]
        };
        micro.sample(mask, v, uv3.unwrap_or([[0.0; 2]; 3]), vertex_alpha, size);
        if micro.covered == micro.total {
            // Covered at every sample: exactly the opaque cook of this triangle.
            deposit(&mut level0, (&mut hits, &mut sources), &grid, v, area, &surface);
            continue;
        }
        let sample_area = area / micro.total as f32;
        for (k, &corners) in micro.subs.iter().enumerate() {
            deposit_masked(
                &mut level0,
                (&mut uncovered, &mut sources),
                &grid,
                corners,
                micro.samples_of(k),
                sample_area,
                &surface,
            );
        }
    }
    for (xyz, share) in uncovered {
        if let Some(entry) = level0.get_mut(&xyz) {
            entry.full += share;
        }
    }
    if level0.is_empty() {
        return Err(empty());
    }

    let mut proxy = VoxelFarProxy {
        origin,
        extent,
        bounds: sphere_of_points(tris.iter().flat_map(|t| t.map(|i| positions[i as usize]))),
        source_triangles: tris.len() as u32,
        ..VoxelFarProxy::default()
    };
    let mut level = level0;
    let mut level_resolution = resolution;
    loop {
        let voxel_size = extent / level_resolution as f32;
        push_level(
            &mut proxy,
            &level,
            (&sources, level_resolution),
            voxel_size,
            origin,
        );
        if level_resolution == 1 {
            break;
        }
        let mut parent: BTreeMap<[u32; 3], Accum> = BTreeMap::new();
        let parent_size = voxel_size * 2.0;
        for (xyz, accum) in &level {
            let p = xyz.map(|v| v / 2);
            let center = std::array::from_fn(|k| origin[k] + (p[k] as f32 + 0.5) * parent_size);
            parent.entry(p).or_default().merge(accum, center);
        }
        sources.per_voxel = sources.parents();
        level = parent;
        level_resolution /= 2;
    }
    // All-full boxes are stored as none (the canonical form `from_bytes` returns).
    if proxy.extents.iter().all(|&e| e == VOXEL_EXTENT_FULL) {
        proxy.extents.clear();
    }
    // Sampled errors that tighten nothing are stored as none (the canonical form).
    if proxy
        .sampled_errors
        .iter()
        .zip(&proxy.levels)
        .all(|(s, level)| s.to_bits() == level.error.to_bits())
    {
        proxy.sampled_errors.clear();
    }
    proxy.coverage = level_coverage(input, coverage, &proxy);
    Ok(proxy)
}

/// Raster pixels of the coverage estimate per level-0 voxel edge (ADR-3227).
const COVERAGE_PIXELS_PER_VOXEL: u32 = 4;
/// Largest raster edge of the coverage estimate.
const MAX_COVERAGE_PIXELS: u32 = 512;

/// An `n` × `n` coverage raster over two axes of the proxy's grid cube, one bit per pixel.
struct CoverageRaster {
    n: usize,
    bits: Vec<u64>,
}

impl CoverageRaster {
    fn new(n: usize) -> Self {
        Self {
            n,
            bits: vec![0; (n * n).div_ceil(64)],
        }
    }

    fn set(&mut self, x: usize, y: usize) {
        let i = y * self.n + x;
        self.bits[i / 64] |= 1 << (i % 64);
    }

    fn count(&self) -> u64 {
        self.bits.iter().map(|b| u64::from(b.count_ones())).sum()
    }

    /// Pixels whose centres lie in `[a, b)` (raster units) along one axis.
    fn span(&self, a: f32, b: f32) -> std::ops::Range<usize> {
        let first = (a - 0.5).ceil().max(0.0) as usize;
        let end = ((b - 0.5).ceil().max(0.0) as usize).min(self.n);
        first..end.max(first)
    }
}

/// The screen coverage of each voxel level at its switch distance, as a share of the source's
/// (ADR-3227): `1` draws as much of the view as the alpha-tested source, below `1` the far
/// canopy thins, above it the level is a blob. Empty when the source covers nothing.
///
/// Both sides are measured as the converged (temporally resolved) image sees them: the covered
/// area of three orthographic views along the grid axes, on a raster of
/// `COVERAGE_PIXELS_PER_VOXEL` pixels per level-0 voxel (at most `MAX_COVERAGE_PIXELS`). Past the
/// switch distance a pixel is larger than a level's error, so what a pixel shows is the share of
/// it the covered surface fills, and the sum over the view is the covered area, whatever the
/// pixel size. The source is every triangle, a pixel covered where the hit passes `coverage`'s
/// alpha test (always without one); a level is every voxel the default opacity dropout draws
/// ([`VoxelFarProxy::keeps`]) as its sub-voxel box, the shape with the level's smallest error,
/// alpha-tested at its representative texture coordinate as the masked raster tests its faces.
#[must_use]
pub fn level_coverage(
    input: &VoxelizeInput<'_>,
    coverage: Option<&VoxelCoverage<'_>>,
    proxy: &VoxelFarProxy,
) -> Vec<f32> {
    let Some(level0) = proxy.levels.first() else {
        return Vec::new();
    };
    let _span = tracing::info_span!("voxel_far_level_coverage", levels = proxy.levels.len()).entered();
    let n = (level0.resolution * COVERAGE_PIXELS_PER_VOXEL).clamp(1, MAX_COVERAGE_PIXELS) as usize;
    let scale = n as f32 / proxy.extent.max(1e-20);
    let has_uvs = input.uvs.len() == input.positions.len();
    let has_colors = input.colors.len() == input.positions.len();
    let mut source_total = 0u64;
    let mut level_totals = vec![0u64; proxy.levels.len()];
    for axis in 0..3 {
        let (ax, ay) = ((axis + 1) % 3, (axis + 2) % 3);
        let to_px = |p: [f32; 3]| {
            [
                (p[ax] - proxy.origin[ax]) * scale,
                (p[ay] - proxy.origin[ay]) * scale,
            ]
        };
        let mut source = CoverageRaster::new(n);
        let position = |i: u32| input.positions.get(i as usize).copied();
        for t in input.indices.as_chunks::<3>().0 {
            let (Some(a), Some(b), Some(c)) = (position(t[0]), position(t[1]), position(t[2])) else {
                continue;
            };
            let v = [a, b, c];
            if v.iter().flatten().any(|c| !c.is_finite()) {
                continue;
            }
            let p = [to_px(v[0]), to_px(v[1]), to_px(v[2])];
            let area = (p[1][0] - p[0][0]) * (p[2][1] - p[0][1]) - (p[2][0] - p[0][0]) * (p[1][1] - p[0][1]);
            if area.abs() < 1e-12 {
                continue;
            }
            let lo = |k: usize| p.iter().map(|q| q[k]).fold(f32::MAX, f32::min);
            let hi = |k: usize| p.iter().map(|q| q[k]).fold(f32::MIN, f32::max);
            let (xs, ys) = (source.span(lo(0), hi(0) + 1.0), source.span(lo(1), hi(1) + 1.0));
            for y in ys {
                for x in xs.clone() {
                    let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                    let w = |a: [f32; 2], b: [f32; 2]| {
                        ((b[0] - a[0]) * (py - a[1]) - (px - a[0]) * (b[1] - a[1])) / area
                    };
                    let bary = [w(p[1], p[2]), w(p[2], p[0]), w(p[0], p[1])];
                    if bary.iter().any(|&b| b < 0.0) {
                        continue;
                    }
                    let covered = coverage.is_none_or(|mask| {
                        let uv: [f32; 2] = if has_uvs {
                            std::array::from_fn(|k| {
                                (0..3).map(|c| input.uvs[t[c] as usize][k] * bary[c]).sum::<f32>()
                            })
                        } else {
                            [0.0; 2]
                        };
                        let alpha = if has_colors {
                            (0..3)
                                .map(|c| input.colors[t[c] as usize][3] * bary[c])
                                .sum::<f32>()
                        } else {
                            1.0
                        };
                        mask.passes(uv, alpha)
                    });
                    if covered {
                        source.set(x, y);
                    }
                }
            }
        }
        source_total += source.count();
        for (index, level) in proxy.levels.iter().enumerate() {
            let mut drawn = CoverageRaster::new(n);
            let size = level.voxel_size;
            let steps = VOXEL_EXTENT_STEPS as f32;
            for (xyz, sample, extent) in proxy.level_voxel_extents(index) {
                if !VoxelFarProxy::keeps(index, xyz, &sample, true) {
                    continue;
                }
                if coverage.is_some_and(|mask| !mask.passes(sample.uv, 1.0)) {
                    continue;
                }
                let (lo, hi) = unpack_voxel_extent(extent & !VOXEL_EXTENT_INEXACT);
                let edge = |k: usize, q: u32| (xyz[k] as f32 + q as f32 / steps) * size * scale;
                let xs = drawn.span(edge(ax, lo[ax]), edge(ax, hi[ax]));
                for y in drawn.span(edge(ay, lo[ay]), edge(ay, hi[ay])) {
                    for x in xs.clone() {
                        drawn.set(x, y);
                    }
                }
            }
            level_totals[index] += drawn.count();
        }
    }
    if source_total == 0 {
        return Vec::new();
    }
    level_totals
        .iter()
        .map(|&drawn| (drawn as f64 / source_total as f64) as f32)
        .map(|c| c.min(MAX_VOXEL_COVERAGE))
        .collect()
}

/// The share of view rays a voxel's surface stops. Scattered small surfaces (leaves) attenuate as
/// a turbid medium, `1 - exp(-G L)` with the leaf area ratio `L` = area / face area and `G` = 0.5
/// for leaves of uniformly random orientation (Beer-Lambert in plant canopies: Monsi and Saeki
/// 1953; the projection function G: J. Ross, *The Radiation Regime and Architecture of Plant
/// Stands*, 1981). A closed or planar surface stops every ray that crosses it; the normal
/// consistency `c` = |sum of area-weighted normals| / area (1 for a plane, near 0 for random
/// leaves) blends the two: `c + (1 - c)(1 - exp(-G L))`.
///
/// Alpha-tested geometry (ADR-3108) stops only the rays its covered area stops: the turbid term
/// counts the covered area, and the planar term is scaled by the alpha coverage `f` = covered
/// area / geometric area of the surface in the voxel, `c f + (1 - c)(1 - exp(-G L))`. With
/// nothing cut away `f` is 1 and this is the opaque rule, bit for bit.
fn opacity(a: &Accum, face_area: f32) -> f32 {
    const G: f32 = 0.5;
    let area = a.area.max(1e-30);
    let consistency = (dot(a.normal, a.normal).sqrt() / area).clamp(0.0, 1.0);
    let leaves = 1.0 - (-G * a.area / face_area.max(1e-30)).exp();
    let covered = if a.full <= a.area {
        1.0
    } else {
        (a.area / a.full).clamp(0.0, 1.0)
    };
    (consistency * covered + (1.0 - consistency) * leaves).clamp(0.0, 1.0)
}

/// The opacity of a voxel holding alpha-tested geometry (ADR-3227), fitted to the box the runtime
/// draws: the dropout keeps a voxel with this probability and draws its whole sub-voxel box, so
/// the expected drawn area in the three axis views, `opacity × Σ A_k`, equals what the covered
/// surface shows there, `Σ A_k g(P_k / A_k)`. `A_k` is the drawn box's face area across axis `k`
/// (`packed`, the whole voxel when it has none), `P_k` the covered area projected along `k`, and
/// `g(x) = c min(1, x) + (1 - c)(1 - exp(-x))` the share of the face the surface fills: a planar
/// surface (normal consistency `c` = 1) fills its projection, scattered leaves overlap as a
/// turbid medium (the projected area is already the projection function's `G L`). Where every
/// face has no area (a point), [`opacity`]'s rule. With nothing cut away the voxel keeps
/// [`opacity`]'s rule, so opaque meshes cook as before.
fn box_fitted_opacity(a: &Accum, packed: Option<u32>, voxel_size: f32) -> f32 {
    let steps = VOXEL_EXTENT_STEPS as f32;
    let size: V3 = match packed {
        Some(p) => {
            let (lo, hi) = unpack_voxel_extent(p);
            std::array::from_fn(|k| (hi[k] - lo[k]) as f32 / steps * voxel_size)
        }
        None => [voxel_size; 3],
    };
    let area = a.area.max(1e-30);
    let c = (dot(a.normal, a.normal).sqrt() / area).clamp(0.0, 1.0);
    let (mut drawn, mut shown) = (0.0f32, 0.0f32);
    for k in 0..3 {
        let face = size[(k + 1) % 3] * size[(k + 2) % 3];
        if face <= 1e-30 {
            continue;
        }
        let x = a.proj[k] / face;
        drawn += face;
        shown += face * (c * x.min(1.0) + (1.0 - c) * (1.0 - (-x).exp()));
    }
    if drawn <= 1e-30 {
        return opacity(a, voxel_size * voxel_size);
    }
    (shown / drawn).clamp(0.0, 1.0)
}

fn push_level(
    proxy: &mut VoxelFarProxy,
    level: &BTreeMap<[u32; 3], Accum>,
    (sources, resolution): (&Sources, u32),
    voxel_size: f32,
    origin: V3,
) {
    // (analytic bound, voxel, corner, packed box) of every voxel, for the sampled error.
    let mut bounds: Vec<(f32, [u32; 3], V3, u32)> = Vec::with_capacity(level.len());
    // Bricks in Morton order of the brick coordinate; voxels in mask-bit order inside a brick.
    let mut bricks: BTreeMap<u32, ([u32; 3], BTreeMap<u32, Accum>)> = BTreeMap::new();
    for (xyz, accum) in level {
        let b = xyz.map(|v| v / VOXEL_BRICK_EDGE);
        let l = xyz.map(|v| v % VOXEL_BRICK_EDGE);
        let bit = l[0] + 4 * l[1] + 16 * l[2];
        bricks
            .entry(morton(b))
            .or_insert_with(|| (b, BTreeMap::new()))
            .1
            .insert(bit, *accum);
    }
    let face_area = voxel_size * voxel_size;
    let brick_offset = proxy.bricks.len() as u32;
    let mut error = 0.0f32;
    for (b, voxels) in bricks.values() {
        let mut mask = 0u64;
        let voxel_offset = proxy.voxels.len() as u32;
        for (&bit, a) in voxels {
            mask |= 1u64 << bit;
            let local = [bit & 3, (bit >> 2) & 3, bit >> 4];
            let xyz: [u32; 3] = std::array::from_fn(|k| b[k] * VOXEL_BRICK_EDGE + local[k]);
            let corner: V3 = std::array::from_fn(|k| origin[k] + xyz[k] as f32 * voxel_size);
            let packed = quantized_extent(a, corner, voxel_size);
            let bound = match packed {
                Some(p) => voxel_extent_error(p, voxel_size),
                None => voxel_size * DIAGONAL,
            };
            error = error.max(bound);
            bounds.push((bound, xyz, corner, packed.unwrap_or(VOXEL_EXTENT_FULL)));
            proxy
                .extents
                .push(packed.unwrap_or(VOXEL_EXTENT_FULL | VOXEL_EXTENT_INEXACT));
            let w = a.area.max(1e-30);
            let alpha = if a.masked && a.area < a.full {
                box_fitted_opacity(a, packed, voxel_size)
            } else {
                opacity(a, face_area)
            };
            proxy
                .voxels
                .push(VoxelSample::new(a.normal, a.color.map(|c| c / w), alpha, a.uv));
        }
        proxy.bricks.push(VoxelBrick {
            coord: VoxelBrick::pack_coord(*b),
            voxel_offset,
            mask_lo: mask as u32,
            mask_hi: (mask >> 32) as u32,
        });
    }
    proxy.levels.push(VoxelLevel {
        resolution,
        voxel_size,
        error,
        brick_offset,
        brick_count: proxy.bricks.len() as u32 - brick_offset,
    });
    // The sampled box error (ADR-3108): voxels in order of their analytic bound, largest first;
    // each one's error is the smaller of its bound and its sampled distance, and the scan stops
    // once no remaining bound can raise the level's.
    bounds.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut sampled = 0.0f32;
    for &(bound, xyz, corner, packed) in &bounds {
        if bound <= sampled {
            break;
        }
        let pieces = sources
            .per_voxel
            .get(&xyz)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .map(|&i| sources.pieces[i as usize]);
        let distance = sampled_box_error(packed, corner, voxel_size, pieces, sampled);
        sampled = sampled.max(distance.min(bound));
    }
    proxy.sampled_errors.push(sampled.min(error));
}

/// Box face samples per voxel edge for the sampled box error: the spacing's margin is at most
/// `sqrt(2) / 2 / SAMPLES_PER_EDGE` of a voxel edge.
const SAMPLES_PER_EDGE: f32 = 8.0;

/// How far the drawn box `packed` (in the voxel at `corner`, edge `size`) lies from the surface
/// `pieces`, measured: the distance from each sample of a grid on each of its six faces to the
/// nearest piece, plus the grid's half-diagonal (the distance to a set is 1-Lipschitz, so no
/// point of a face cell is farther than its nearest sample plus that). The largest such value is
/// an upper bound for every point of the box's surface. `floor` lets a sample stop early once a
/// piece is nearer than what is already known; the result is then at most `floor`.
fn sampled_box_error(
    packed: u32,
    corner: V3,
    size: f32,
    pieces: impl Iterator<Item = [V3; 3]> + Clone,
    floor: f32,
) -> f32 {
    let (lo, hi) = regia_render::visbuffer::unpack_voxel_extent(packed);
    let step = size / VOXEL_EXTENT_STEPS as f32;
    let blo: V3 = std::array::from_fn(|k| corner[k] + lo[k] as f32 * step);
    let bhi: V3 = std::array::from_fn(|k| corner[k] + hi[k] as f32 * step);
    let spacing = size / SAMPLES_PER_EDGE;
    let mut worst = 0.0f32;
    for k in 0..3 {
        let (a, b) = ((k + 1) % 3, (k + 2) % 3);
        let (da, db) = (bhi[a] - blo[a], bhi[b] - blo[b]);
        let na = ((da / spacing).ceil() as u32).max(1);
        let nb = ((db / spacing).ceil() as u32).max(1);
        let margin = 0.5 * (da / na as f32).hypot(db / nb as f32);
        for side in [blo[k], bhi[k]] {
            for i in 0..=na {
                for j in 0..=nb {
                    let mut p = [0.0f32; 3];
                    p[k] = side;
                    p[a] = blo[a] + da * i as f32 / na as f32;
                    p[b] = blo[b] + db * j as f32 / nb as f32;
                    let enough = (floor.max(worst) - margin).max(0.0);
                    let mut nearest = f32::MAX;
                    for v in pieces.clone() {
                        let w = closest_barycentric(p, v);
                        let q: V3 = std::array::from_fn(|c| v[0][c] * w[0] + v[1][c] * w[1] + v[2][c] * w[2]);
                        let d = sub(p, q);
                        nearest = nearest.min(dot(d, d).sqrt());
                        if nearest <= enough {
                            break;
                        }
                    }
                    worst = worst.max(nearest + margin);
                }
            }
        }
    }
    worst + size * 1e-4
}

/// Voxelizes a cooked mesh payload (postcard [`CookedMesh`]) as the cook does.
///
/// # Errors
///
/// The payload does not decode or has no triangles.
pub fn voxel_far_for_cooked_mesh(
    path: &str,
    payload: &[u8],
    settings: &VoxelFarCookSettings,
) -> Result<VoxelFarProxy, VoxelFarCookError> {
    voxel_far_for_cooked_mesh_with_coverage(path, payload, None, settings)
}

/// [`voxel_far_for_cooked_mesh`] with the alpha test of the mesh's masked material
/// ([`voxelize_with_coverage`], ADR-3108).
///
/// # Errors
///
/// The payload does not decode, has no triangles, or nothing of it is covered.
pub fn voxel_far_for_cooked_mesh_with_coverage(
    path: &str,
    payload: &[u8],
    coverage: Option<&VoxelCoverage<'_>>,
    settings: &VoxelFarCookSettings,
) -> Result<VoxelFarProxy, VoxelFarCookError> {
    let bad = |reason: String| VoxelFarCookError::Mesh {
        path: path.to_string(),
        reason,
    };
    let mesh = CookedMesh::from_postcard(payload).map_err(|e| bad(e.to_string()))?;
    let cpu = regia_render::visbuffer::cooked_mesh_lod0(&mesh).map_err(bad)?;
    voxelize_with_coverage(
        path,
        &VoxelizeInput {
            positions: &cpu.positions,
            normals: &cpu.normals,
            uvs: &cpu.uvs,
            colors: &cpu.colors,
            indices: &cpu.indices,
        },
        coverage,
        settings,
    )
}

/// A masked material's alpha test as the cook found it beside the meshes (ADR-3108): the decoded
/// base color alpha (empty without a texture or when it does not decode) and the bytes the cache
/// key hashes.
struct MaskedMaterial {
    width: u32,
    height: u32,
    alpha: Vec<u8>,
    factor: f32,
    cutoff: f32,
    vertex_alpha: bool,
    uv_transform: TextureTransform,
    key: [u8; 32],
}

impl MaskedMaterial {
    fn coverage(&self) -> VoxelCoverage<'_> {
        VoxelCoverage {
            width: self.width,
            height: self.height,
            alpha: &self.alpha,
            factor: self.factor,
            cutoff: self.cutoff,
            vertex_alpha: self.vertex_alpha,
            uv_transform: self.uv_transform,
        }
    }
}

/// What the voxel far cook needs to know about a glTF source beyond its cooked sub-assets
/// (ADR-3227): the cooked material carries neither, so the coverage test would otherwise read a
/// `vertex_ao` alpha as opacity and the base color texture without its transform.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VoxelFarSource {
    /// The importer's `vertex_ao` setting: `COLOR_0` alpha is an occlusion factor, so the coverage
    /// test leaves it out (it stays a shading term).
    pub vertex_ao: bool,
    /// The base color texture's `KHR_texture_transform` by material sub-asset id (materials
    /// without one are absent).
    pub base_color_transforms: BTreeMap<AssetId, TextureTransform>,
}

impl VoxelFarSource {
    /// The source facts of an imported glTF: its `vertex_ao` importer setting, its material
    /// sub-asset ids in document order ([`crate::GltfImportResult::material_ids`]) and its
    /// extension table ([`crate::GltfImportResult::extensions`]).
    #[must_use]
    pub fn from_gltf(vertex_ao: bool, material_ids: &[AssetId], extensions: &GltfExtensionTable) -> Self {
        let mut base_color_transforms = BTreeMap::new();
        for material in &extensions.materials {
            let Some(transform) = material.textures.get("base_color").and_then(|t| t.transform) else {
                continue;
            };
            let Some(&id) = material_ids.get(material.index) else {
                continue;
            };
            if transform.tex_coord.is_some_and(|set| set != 0) {
                tracing::warn!(
                    material = material.index,
                    "voxel far cook: a base color transform selects another UV set; the coverage test reads UV 0"
                );
            }
            if transform != TextureTransform::IDENTITY {
                base_color_transforms.insert(id, transform);
            }
        }
        Self {
            vertex_ao,
            base_color_transforms,
        }
    }
}

/// The masked material each cooked mesh primitive draws with, found through the glTF's
/// `CookedModel` sub-asset (mesh to material), the `CookedStandardMaterial` (alpha mode, cutoff,
/// base color factor and texture) and the `CookedTexture` (decoded to RGBA8 as a device without
/// the block format would sample it), with the source's `vertex_ao` setting and base color
/// transform ([`VoxelFarSource`]). Opaque and blended materials are left out: they cook as before.
fn masked_materials(
    subs: &[(&str, AssetId, &str, &[u8])],
    source: &VoxelFarSource,
) -> BTreeMap<AssetId, std::sync::Arc<MaskedMaterial>> {
    let payload_of = |id: AssetId, ty: &str| {
        subs.iter()
            .find(|(_, sub_id, type_path, _)| *sub_id == id && *type_path == ty)
            .map(|(_, _, _, payload)| *payload)
    };
    let mut out = BTreeMap::new();
    let mut by_material: BTreeMap<AssetId, Option<std::sync::Arc<MaskedMaterial>>> = BTreeMap::new();
    for (_, _, type_path, payload) in subs {
        if *type_path != MODEL_TYPE_PATH {
            continue;
        }
        let Ok(model) = regia_asset::CookedModel::from_postcard(payload) else {
            tracing::warn!("voxel far cook: the model sub-asset does not decode; masked meshes cook opaque");
            continue;
        };
        for part in &model.parts {
            let Some(material_id) = part.material else {
                continue;
            };
            let masked = by_material
                .entry(material_id)
                .or_insert_with(|| {
                    let bytes = payload_of(material_id, MATERIAL_TYPE_PATH)?;
                    let material = regia_asset::CookedStandardMaterial::from_postcard(bytes).ok()?;
                    let authored = &material.authored;
                    if authored.render_state.alpha_mode != regia_asset::AlphaMode::Mask {
                        return None;
                    }
                    let uv_transform = source
                        .base_color_transforms
                        .get(&material_id)
                        .copied()
                        .unwrap_or(TextureTransform::IDENTITY);
                    let mut hasher = blake3::Hasher::new();
                    hasher.update(b"mask");
                    hasher.update(bytes);
                    // The source facts the cooked material does not carry (ADR-3227); hashed only
                    // when they change the test, so other masked meshes keep their keys.
                    if source.vertex_ao {
                        hasher.update(b"vertex_ao");
                    }
                    if uv_transform != TextureTransform::IDENTITY {
                        hasher.update(b"uv_transform");
                        for v in [
                            uv_transform.offset[0],
                            uv_transform.offset[1],
                            uv_transform.rotation,
                            uv_transform.scale[0],
                            uv_transform.scale[1],
                        ] {
                            hasher.update(&v.to_le_bytes());
                        }
                    }
                    let texture = match &authored.base.base_color_map {
                        Some(regia_asset::TextureSlot::Asset(id)) => payload_of(*id, TEXTURE_TYPE_PATH),
                        _ => None,
                    };
                    let (width, height, alpha) = texture
                        .and_then(|bytes| {
                            hasher.update(bytes);
                            let cooked = regia_asset::CookedTexture::from_postcard(bytes).ok()?;
                            let decoded = regia_render::CompressedTexture::from_cooked(&cooked)
                                .ok()?
                                .decode_rgba8()?;
                            let alpha = decoded.rgba.as_chunks::<4>().0.iter().map(|px| px[3]).collect();
                            Some((decoded.width, decoded.height, alpha))
                        })
                        .unwrap_or_else(|| {
                            if texture.is_some() {
                                tracing::warn!(
                                    "voxel far cook: a masked material's base color texture does not decode; \
                                     its coverage uses the factor alone"
                                );
                            }
                            (0, 0, Vec::new())
                        });
                    Some(std::sync::Arc::new(MaskedMaterial {
                        width,
                        height,
                        alpha,
                        factor: authored.base.base_color[3],
                        cutoff: authored.render_state.alpha_cutoff,
                        vertex_alpha: !source.vertex_ao,
                        uv_transform,
                        key: *hasher.finalize().as_bytes(),
                    }))
                })
                .clone();
            if let Some(masked) = masked {
                out.entry(part.mesh).or_insert(masked);
            }
        }
    }
    out
}

/// The cache key: the mesh bytes, settings, versions and, for a masked material, its material and
/// texture bytes (so an edited mask recooks; opaque meshes keep the key they always had).
fn cache_key(payload: &[u8], settings: &VoxelFarCookSettings, masked: Option<&MaskedMaterial>) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"fvox");
    hasher.update(&regia_render::visbuffer::VOXEL_FAR_FORMAT_VERSION.to_le_bytes());
    hasher.update(&VOXEL_FAR_COOK_VERSION.to_le_bytes());
    hasher.update(&settings.effective_resolution().to_le_bytes());
    hasher.update(payload);
    if let Some(masked) = masked {
        hasher.update(&masked.key);
    }
    *hasher.finalize().as_bytes()
}

/// Writes a voxel far sub-asset for every cooked mesh primitive in `sub_assets` (path, id, type
/// path, payload) into `cache_dir`. A primitive whose cached proxy was built from the same mesh
/// bytes and settings by the same cooker is not rebuilt (a `<cooked file>.key` beside it holds
/// the key). Files are written to a temporary name and renamed. A glTF cook passes what it knows
/// of the source with [`cook_voxel_far_sub_assets_from`]; this is that with
/// [`VoxelFarSource::default`].
///
/// # Errors
///
/// A mesh fails to decode or has no triangles, or a write fails.
pub fn cook_voxel_far_sub_assets<'a>(
    cache_dir: &Path,
    rel_path: &str,
    importer_version: u32,
    parent: AssetId,
    settings: &VoxelFarCookSettings,
    sub_assets: impl Iterator<Item = (&'a str, AssetId, &'a str, &'a [u8])>,
) -> Result<VoxelFarCookOutput, VoxelFarCookError> {
    cook_voxel_far_sub_assets_from(
        cache_dir,
        rel_path,
        importer_version,
        parent,
        settings,
        &VoxelFarSource::default(),
        sub_assets,
    )
}

/// [`cook_voxel_far_sub_assets`] with the source facts the cooked sub-assets do not carry
/// ([`VoxelFarSource`], ADR-3227): a masked material's coverage test leaves a `vertex_ao` alpha
/// out and reads the base color texture through its `KHR_texture_transform`.
///
/// # Errors
///
/// A mesh fails to decode or has no triangles, or a write fails.
pub fn cook_voxel_far_sub_assets_from<'a>(
    cache_dir: &Path,
    rel_path: &str,
    importer_version: u32,
    parent: AssetId,
    settings: &VoxelFarCookSettings,
    source: &VoxelFarSource,
    sub_assets: impl Iterator<Item = (&'a str, AssetId, &'a str, &'a [u8])>,
) -> Result<VoxelFarCookOutput, VoxelFarCookError> {
    let mut out = VoxelFarCookOutput::default();
    let sub_assets: Vec<(&str, AssetId, &str, &[u8])> = sub_assets.collect();
    let masked = masked_materials(&sub_assets, source);
    for &(sub_path, mesh_id, type_path, payload) in &sub_assets {
        if type_path != MESH_TYPE_PATH {
            continue;
        }
        let mask = masked.get(&mesh_id).map(std::sync::Arc::as_ref);
        let Some(path) = voxel_far_sub_path(sub_path) else {
            continue;
        };
        let id = AssetId::sub_asset(parent, &path);
        let file = cache_dir.join(cooked_file_name(id));
        let key_file = cache_dir.join(format!("{}.key", cooked_file_name(id)));
        let key = cache_key(payload, settings, mask);
        let current = file.is_file() && std::fs::read(&key_file).is_ok_and(|k| k == key);
        if !current {
            let _span = tracing::info_span!("cook_voxel_far", path = %path).entered();
            let coverage = mask.map(MaskedMaterial::coverage);
            let proxy = match voxel_far_for_cooked_mesh_with_coverage(
                &path,
                payload,
                coverage.as_ref(),
                settings,
            ) {
                Ok(proxy) => proxy,
                // A masked primitive its alpha test cuts away everywhere (a base color factor
                // under the cutoff, a helper exported fully transparent) draws nothing, so it
                // has no far proxy, as the stream cook leaves a node without one (ADR-5571).
                // The rest of the asset still cooks; a primitive with no triangles at all still
                // fails, as before. A proxy cooked before the mask cut it away is removed, so
                // the runtime does not load it by id.
                Err(VoxelFarCookError::Empty { .. })
                    if coverage.is_some()
                        && voxel_far_for_cooked_mesh_with_coverage(&path, payload, None, settings)
                            .is_ok() =>
                {
                    tracing::warn!(
                        path = %path,
                        "voxel far cook: the masked material cuts the primitive away everywhere; it has no voxel proxy"
                    );
                    remove_if_present(&file)?;
                    remove_if_present(&key_file)?;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let bytes = proxy.to_bytes();
            let header = CookedHeader {
                format_version: COOKED_FORMAT_VERSION,
                type_tag: regia_core::id::TypePathHash::of(VOXEL_FAR_TYPE_PATH),
                asset_id: id,
                importer_version,
                dependencies: vec![mesh_id],
                payload_len: bytes.len() as u64,
            };
            let cooked =
                encode_cooked_file(&header, &bytes).map_err(|e| VoxelFarCookError::Cooked(e.to_string()))?;
            write_atomic(&file, &cooked)?;
            write_atomic(&key_file, &key)?;
            out.built += 1;
        }
        out.manifest.push(ManifestEntry {
            path: format!("{rel_path}#{path}"),
            id,
            type_path: VOXEL_FAR_TYPE_PATH.to_string(),
        });
        out.meta.push(SubAssetEntry {
            path,
            id,
            type_path: VOXEL_FAR_TYPE_PATH.to_string(),
        });
    }
    Ok(out)
}

/// Removes `path`; a file that is not there is not an error.
fn remove_if_present(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp{}", std::process::id()));
    let tmp = std::path::PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod coverage_tests;
