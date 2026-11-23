// SPDX-License-Identifier: MPL-2.0
//! Signed-distance bricks for the GPU probe tracer ([`crate::ddgi`]).
//!
//! Each rigid mesh is voxelised once, in object space, into a small brick of distances.
//! Voxels are anisotropic: the long axes get up to [`MAX_BRICK_DIM`] voxels and a thin
//! axis still gets at least [`MIN_INTERIOR_DIM`], so a 20 cm wall keeps a solid core and
//! probe rays cannot slip through it. Near the surface the distance is signed by the
//! closest triangle's geometric normal (positive outside); farther away it is unsigned,
//! which keeps open meshes (a terrain skirt, a single plane) from turning half of space
//! solid. Rays only need the sign at the surface: a hit is `d < ε`, its normal is the
//! gradient, and a hit whose normal faces along the ray is a back face (the probe is
//! inside the mesh).
//!
//! The grid puts a sample plane on each face of the mesh's bounds (ADR-5338): trilinear
//! interpolation reproduces an axis-aligned box exactly there, edges included. With the
//! bounds halfway between two samples, a coarse axis (a 16 m wall's 0.375 m voxels up its
//! height) rounded the plate's edges off by up to half a voxel, and two plates that touch
//! (a tunnel's wall under its roof, a wall on its floor) left a slit at the joint that the
//! probes saw the sunlit outside through.
//!
//! ## Provenance
//! Sphere tracing a distance field: Hart, "Sphere Tracing", The Visual Computer 1996.
//! Closest point on a triangle: Ericson, *Real-Time Collision Detection* (2005) § 5.1.5, in f64
//! (ADR-5338).
//! Sign from the closest triangle near the surface: a one-triangle stand-in for
//! Bærentzen and Aanæs, "Signed Distance Computation Using the Angle Weighted
//! Pseudonormal", IEEE TVCG 2005. Triangle binning into a uniform grid for the
//! closest-point search: Ericson § 7.1.

use regia_core::glam::{DVec3, Vec3};

/// Most voxels along one brick axis across the mesh's bounds (the brick stores one sample
/// more, on the far face, plus a border sample on each side).
pub const MAX_BRICK_DIM: u32 = 40;
/// Fewest voxels across the bounds along any axis.
pub const MIN_INTERIOR_DIM: u32 = 4;
/// Largest triangle count baked. Bigger meshes get no brick (no probe proxy).
pub const MAX_BAKE_TRIANGLES: usize = 400_000;

/// A baked brick: object-space distances on an axis-aligned voxel grid.
#[derive(Clone, Debug, PartialEq)]
pub struct SdfBrick {
    /// Object-space centre of voxel `(0,0,0)`.
    pub origin: [f32; 3],
    /// Voxel edge per axis, object units.
    pub voxel: [f32; 3],
    /// Voxels per axis (border included).
    pub dims: [u32; 3],
    /// Distances, x fastest, then y, then z (object units).
    pub values: Vec<f32>,
}

impl SdfBrick {
    /// Trilinear sample (the GPU's `brick_sample`). Outside the grid the border repeats.
    #[cfg(test)]
    #[must_use]
    pub fn sample(&self, p: [f32; 3]) -> f32 {
        let mut i = [0u32; 3];
        let mut f = [0f32; 3];
        for a in 0..3 {
            let max = (self.dims[a].max(1) - 1) as f32;
            let g = ((p[a] - self.origin[a]) / self.voxel[a].max(1e-9)).clamp(0.0, max);
            let fl = g.floor().min((max - 1.0).max(0.0));
            i[a] = fl as u32;
            f[a] = g - fl;
        }
        let at = |x: u32, y: u32, z: u32| -> f32 {
            let x = x.min(self.dims[0] - 1);
            let y = y.min(self.dims[1] - 1);
            let z = z.min(self.dims[2] - 1);
            self.values[(x + self.dims[0] * (y + self.dims[1] * z)) as usize]
        };
        let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
        let (x, y, z) = (i[0], i[1], i[2]);
        let c00 = lerp(at(x, y, z), at(x + 1, y, z), f[0]);
        let c10 = lerp(at(x, y + 1, z), at(x + 1, y + 1, z), f[0]);
        let c01 = lerp(at(x, y, z + 1), at(x + 1, y, z + 1), f[0]);
        let c11 = lerp(at(x, y + 1, z + 1), at(x + 1, y + 1, z + 1), f[0]);
        lerp(lerp(c00, c10, f[1]), lerp(c01, c11, f[1]), f[2])
    }
}

struct Tri {
    a: Vec3,
    b: Vec3,
    c: Vec3,
    n: Vec3,
}

/// Uniform grid of triangle indices over the mesh bounds (closest-point search).
struct TriGrid {
    min: Vec3,
    cell: Vec3,
    dims: [i32; 3],
    cells: Vec<Vec<u32>>,
}

impl TriGrid {
    fn new(tris: &[Tri], min: Vec3, max: Vec3) -> Self {
        let extent = (max - min).max(Vec3::splat(1e-4));
        // About 4 triangles per occupied cell on average, at most 32 per axis.
        let target = ((tris.len() as f32 / 4.0).cbrt().ceil() as i32).clamp(1, 32);
        let longest = extent.max_element();
        let dims: [i32; 3] =
            std::array::from_fn(|a| ((extent[a] / longest * target as f32).ceil() as i32).clamp(1, 32));
        let cell = Vec3::new(
            extent.x / dims[0] as f32,
            extent.y / dims[1] as f32,
            extent.z / dims[2] as f32,
        );
        let mut cells = vec![Vec::new(); (dims[0] * dims[1] * dims[2]) as usize];
        for (index, t) in tris.iter().enumerate() {
            let lo = t.a.min(t.b).min(t.c);
            let hi = t.a.max(t.b).max(t.c);
            let c0: [i32; 3] =
                std::array::from_fn(|a| (((lo[a] - min[a]) / cell[a]).floor() as i32).clamp(0, dims[a] - 1));
            let c1: [i32; 3] =
                std::array::from_fn(|a| (((hi[a] - min[a]) / cell[a]).floor() as i32).clamp(0, dims[a] - 1));
            for z in c0[2]..=c1[2] {
                for y in c0[1]..=c1[1] {
                    for x in c0[0]..=c1[0] {
                        cells[(x + dims[0] * (y + dims[1] * z)) as usize].push(index as u32);
                    }
                }
            }
        }
        Self {
            min,
            cell,
            dims,
            cells,
        }
    }

    /// Closest triangle to `p`: (distance, signed-by-normal distance). Shells of cells
    /// grow until the best distance found is closer than the next shell can be.
    ///
    /// Several triangles can be equally close: every triangle that meets at an edge or a vertex
    /// nearest `p`. Their normals can disagree about the side `p` is on (above a box's top edge,
    /// the end face's normal is perpendicular to the offset and its sign is rounding noise), so the
    /// sign comes from the tied triangle the offset is most perpendicular to, the face `p` lies
    /// straight over (ADR-5338; a reduced form of Bærentzen and Aanæs's angle-weighted pseudonormal).
    /// The grid's sample planes sit on the bounds' faces, so samples on an edge's planes are common.
    ///
    /// The search starts from `p`'s own cell, or from the grid's nearest cell when `p` lies outside the
    /// grid (ADR-5716). A brick's border samples lie up to a voxel outside the mesh's bounds, and the
    /// bounds of a flat mesh (a ground quad, a foliage card) have no extent along its normal: this
    /// grid then keeps a tenth of a millimetre there while the brick keeps a millimetre, so most of
    /// the brick's samples stand more cells from the grid than the rings below reach. Started from
    /// the sample's own cell the search visited no cell at all for them and returned +∞: a probe ray
    /// that entered the brick through such a sample took an infinite step and left it. On lavapipe a
    /// flat mesh was invisible to the probes from its front; on NVIDIA the non-finite distance made
    /// the march's least step carry a steep ray to the finite samples, so the same scene traced
    /// differently on the two.
    fn closest(&self, tris: &[Tri], p: Vec3) -> (f32, f32) {
        // Every unvisited cell is still at least `ring` cells from `p`: a cell farther along an axis
        // than the clamped start is farther from a `p` beyond the grid's edge on that side.
        let home: [i32; 3] = std::array::from_fn(|a| {
            (((p[a] - self.min[a]) / self.cell[a]).floor() as i32).clamp(0, self.dims[a] - 1)
        });
        let pd = p.as_dvec3();
        let mut best = f64::MAX;
        let mut facing = -1.0f64;
        let mut positive = true;
        let min_cell = f64::from(self.cell.min_element());
        let max_ring = self.dims.iter().copied().max().unwrap_or(1) + 2;
        for ring in 0..=max_ring {
            for z in home[2] - ring..=home[2] + ring {
                for y in home[1] - ring..=home[1] + ring {
                    for x in home[0] - ring..=home[0] + ring {
                        let on_shell = (x - home[0]).abs() == ring
                            || (y - home[1]).abs() == ring
                            || (z - home[2]).abs() == ring;
                        if !on_shell
                            || x < 0
                            || y < 0
                            || z < 0
                            || x >= self.dims[0]
                            || y >= self.dims[1]
                            || z >= self.dims[2]
                        {
                            continue;
                        }
                        for &ti in &self.cells[(x + self.dims[0] * (y + self.dims[1] * z)) as usize] {
                            let t = &tris[ti as usize];
                            let delta = offset_from_triangle(pd, t);
                            let d = delta.length();
                            let along = delta.dot(t.n.as_dvec3());
                            let cos = along.abs() / d.max(1e-30);
                            let tie = TIE_REL * best.min(1e30) + TIE_ABS;
                            if d < best - tie || (d <= best + tie && cos > facing) {
                                best = best.min(d);
                                facing = cos;
                                positive = along >= 0.0;
                            }
                        }
                    }
                }
            }
            // Every unvisited cell is at least `ring` cells away from `p`'s cell.
            if best <= f64::from(ring) * min_cell {
                break;
            }
        }
        let d = best as f32;
        (d, if positive { d } else { -d })
    }
}

/// Two triangles are equally close to a sample (they meet at the edge or vertex nearest it) when
/// their distances differ by less than this share of the distance plus [`TIE_ABS`].
const TIE_REL: f64 = 1e-6;
/// The absolute part of the tie tolerance, metres (object units).
const TIE_ABS: f64 = 1e-9;

/// `p` minus its closest point on `t` (Ericson § 5.1.5), in f64 (ADR-5338).
///
/// The region tests take differences of products of dot products, which grow as the fourth power of
/// the triangle's size: for a 240 m wall face (two triangles) they are about 3·10⁹, and in f32 they
/// cancelled to metres of error that changed from one voxel column to the next. A long corridor's
/// walls then baked a different wall at every position along them (2 m apart on `ddgi_temporal`'s
/// corridor), and the probe field differed with where the camera stood (ADR-5290 § 2). In f64 the
/// error is under a micrometre at that size.
fn offset_from_triangle(p: DVec3, t: &Tri) -> DVec3 {
    let (a, b, c) = (t.a.as_dvec3(), t.b.as_dvec3(), t.c.as_dvec3());
    let ab = b - a;
    let ac = c - a;
    let ap = p - a;
    let d1 = ab.dot(ap);
    let d2 = ac.dot(ap);
    if d1 <= 0.0 && d2 <= 0.0 {
        return ap;
    }
    let bp = p - b;
    let d3 = ab.dot(bp);
    let d4 = ac.dot(bp);
    if d3 >= 0.0 && d4 <= d3 {
        return bp;
    }
    let vc = d1 * d4 - d3 * d2;
    if vc <= 0.0 && d1 >= 0.0 && d3 <= 0.0 {
        let v = d1 / (d1 - d3);
        return ap - ab * v;
    }
    let cp = p - c;
    let d5 = ab.dot(cp);
    let d6 = ac.dot(cp);
    if d6 >= 0.0 && d5 <= d6 {
        return cp;
    }
    let vb = d5 * d2 - d1 * d6;
    if vb <= 0.0 && d2 >= 0.0 && d6 <= 0.0 {
        let w = d2 / (d2 - d6);
        return ap - ac * w;
    }
    let va = d3 * d6 - d5 * d4;
    if va <= 0.0 && (d4 - d3) >= 0.0 && (d5 - d6) >= 0.0 {
        let w = (d4 - d3) / ((d4 - d3) + (d5 - d6));
        return bp - (c - b) * w;
    }
    let denom = 1.0 / (va + vb + vc);
    ap - ab * (vb * denom) - ac * (vc * denom)
}

/// Voxelises a triangle mesh (object space), signed by its winding alone. `None` for an
/// empty or oversized mesh.
#[cfg(test)]
#[must_use]
pub fn bake_brick(positions: &[[f32; 3]], indices: &[u32]) -> Option<SdfBrick> {
    bake_brick_oriented(positions, &[], indices)
}

/// Triangles the baker keeps: non-degenerate, within [`MAX_BAKE_TRIANGLES`]. `None` when
/// there is nothing to voxelise. `normals` flip a winding that disagrees with the vertex
/// normals (empty: winding alone).
fn collect_tris(positions: &[[f32; 3]], normals: &[[f32; 3]], indices: &[u32]) -> Option<Vec<Tri>> {
    let mut tris = Vec::with_capacity(indices.len() / 3);
    for tri in indices.as_chunks::<3>().0 {
        let (Some(a), Some(b), Some(c)) = (
            positions.get(tri[0] as usize),
            positions.get(tri[1] as usize),
            positions.get(tri[2] as usize),
        ) else {
            continue;
        };
        let (a, b, c) = (Vec3::from_array(*a), Vec3::from_array(*b), Vec3::from_array(*c));
        // A NaN or infinite vertex poisons the geometric normal: the degenerate
        // length check below compares false against NaN, and a non-finite
        // normal bakes inverted voxels along the triangle's finite edge.
        if !(a.is_finite() && b.is_finite() && c.is_finite()) {
            continue;
        }
        let mut n = (b - a).cross(c - a);
        if !n.is_finite() || n.length_squared() < 1e-14 {
            continue;
        }
        if let (Some(na), Some(nb), Some(nc)) = (
            normals.get(tri[0] as usize),
            normals.get(tri[1] as usize),
            normals.get(tri[2] as usize),
        ) && n.dot(Vec3::from_array(*na) + Vec3::from_array(*nb) + Vec3::from_array(*nc)) < 0.0
        {
            n = -n;
        }
        tris.push(Tri {
            a,
            b,
            c,
            n: n.normalize(),
        });
    }
    if tris.is_empty() || tris.len() > MAX_BAKE_TRIANGLES {
        None
    } else {
        Some(tris)
    }
}

fn extent_of(tris: &[Tri]) -> (Vec3, Vec3) {
    let mut min = Vec3::splat(f32::MAX);
    let mut max = Vec3::splat(f32::MIN);
    for t in tris {
        min = min.min(t.a).min(t.b).min(t.c);
        max = max.max(t.a).max(t.b).max(t.c);
    }
    (min, max)
}

/// Voxel grid for a mesh bounds: anisotropic voxels, a sample plane on each face of the
/// bounds and one border sample beyond each (ADR-5338).
///
/// Samples run from `min − voxel` to `max + voxel`, so `min` and `max` are sample planes:
/// along each axis the trilinear field crosses zero exactly on the bounds' faces, whatever
/// the voxel size, and a box mesh is exact in its brick, edges and corners included. (With
/// the faces halfway between samples, as before, the interpolation between a sample inside a
/// thin plate and one outside its edge put the edge up to half a voxel inside the plate.)
fn brick_grid(min: Vec3, max: Vec3) -> ([f32; 3], [f32; 3], [u32; 3]) {
    let extent = (max - min).max(Vec3::splat(1e-3));
    let longest = extent.max_element();
    // Voxels across the bounds per axis: the long axis gets MAX_BRICK_DIM, the others in
    // proportion, but never fewer than MIN_INTERIOR_DIM (a thin wall keeps voxels inside it).
    let across: [u32; 3] = std::array::from_fn(|a| {
        ((extent[a] / longest * MAX_BRICK_DIM as f32).ceil() as u32).clamp(MIN_INTERIOR_DIM, MAX_BRICK_DIM)
    });
    let voxel: [f32; 3] = std::array::from_fn(|a| extent[a] / across[a] as f32);
    // `across + 1` samples from `min` to `max`, one border sample each side.
    let dims: [u32; 3] = std::array::from_fn(|a| across[a] + 3);
    let origin: [f32; 3] = std::array::from_fn(|a| min[a] - voxel[a]);
    (origin, voxel, dims)
}

/// How many distance values [`bake_brick_oriented`] would store. `None` for an empty or
/// oversized mesh. The pool uses this to decide whether a bake fits before it runs.
#[must_use]
pub(crate) fn brick_value_count(positions: &[[f32; 3]], indices: &[u32]) -> Option<u64> {
    let tris = collect_tris(positions, &[], indices)?;
    let (min, max) = extent_of(&tris);
    let (_, _, dims) = brick_grid(min, max);
    Some(u64::from(dims[0]) * u64::from(dims[1]) * u64::from(dims[2]))
}

/// Voxelises a triangle mesh (object space). `None` for an empty or oversized mesh.
/// `normals` are the mesh's vertex normals (empty: none). A triangle whose winding
/// disagrees with its vertex normals is signed by the normals, as the renderer shades it: a
/// mesh wound inside out (clockwise from outside) otherwise bakes inside out, the probes in
/// front of its faces read as buried and the faces shade black.
#[must_use]
pub fn bake_brick_oriented(
    positions: &[[f32; 3]],
    normals: &[[f32; 3]],
    indices: &[u32],
) -> Option<SdfBrick> {
    let tris = collect_tris(positions, normals, indices)?;
    let (min, max) = extent_of(&tris);
    let (origin, voxel, dims) = brick_grid(min, max);
    let grid = TriGrid::new(&tris, min, max);
    // Within this distance of the surface the closest triangle's normal signs the value.
    let band = 1.5 * voxel.iter().copied().fold(0.0f32, f32::max);
    let mut values = Vec::with_capacity((dims[0] * dims[1] * dims[2]) as usize);
    // Samples past the band that lie behind their closest triangle (`sign_enclosed`).
    let mut behind = Vec::new();
    for z in 0..dims[2] {
        for y in 0..dims[1] {
            for x in 0..dims[0] {
                let p = Vec3::new(
                    origin[0] + x as f32 * voxel[0],
                    origin[1] + y as f32 * voxel[1],
                    origin[2] + z as f32 * voxel[2],
                );
                let (unsigned, signed) = grid.closest(&tris, p);
                if unsigned > band && signed < 0.0 {
                    behind.push(values.len() as u32);
                }
                values.push(if unsigned <= band { signed } else { unsigned });
            }
        }
    }
    sign_enclosed(&mut values, dims, &behind);
    Some(SdfBrick {
        origin,
        voxel,
        dims,
        values,
    })
}

/// Gives the samples deep inside a closed solid their sign (ADR-5457). `behind` lists the samples
/// past the signed band whose closest triangle faces away from them; the baker stored their
/// unsigned distance, so an open mesh does not turn the half of space behind it solid. Inside a
/// closed solid thicker than two bands that left a positive core: a ray that started there marched
/// to the band's inner edge and met the field's zero crossing from its positive side, a front
/// face about a voxel inside the real wall. A probe in the core of a tower or a thick rock
/// therefore saw no back face, stayed active, and held the black of a closed cavity (no sun, no
/// sky), which every lookup that weighted it took.
///
/// A sample keeps its unsigned distance while it is connected to the brick's border through
/// positive samples (6-neighbors): the space behind an open mesh reaches the border around the
/// mesh's edge or through its opening. A sample the surface's negative band closes off from the
/// border is inside the solid and takes the sign of its closest triangle. A closed pocket of air
/// (a hollow mesh whose inner faces look inward) is not in `behind` and stays positive.
fn sign_enclosed(values: &mut [f32], dims: [u32; 3], behind: &[u32]) {
    if behind.is_empty() {
        return;
    }
    let [nx, ny, nz] = dims.map(|d| d as usize);
    let index = |x: usize, y: usize, z: usize| x + nx * (y + ny * z);
    let mut open = vec![false; values.len()];
    let mut stack = Vec::new();
    for z in 0..nz {
        for y in 0..ny {
            for x in 0..nx {
                let border = x == 0 || y == 0 || z == 0 || x + 1 == nx || y + 1 == ny || z + 1 == nz;
                let i = index(x, y, z);
                if border && values[i] > 0.0 {
                    open[i] = true;
                    stack.push((x, y, z));
                }
            }
        }
    }
    while let Some((x, y, z)) = stack.pop() {
        let mut visit = |x: usize, y: usize, z: usize| {
            let i = index(x, y, z);
            if !open[i] && values[i] > 0.0 {
                open[i] = true;
                stack.push((x, y, z));
            }
        };
        if x > 0 {
            visit(x - 1, y, z);
        }
        if x + 1 < nx {
            visit(x + 1, y, z);
        }
        if y > 0 {
            visit(x, y - 1, z);
        }
        if y + 1 < ny {
            visit(x, y + 1, z);
        }
        if z > 0 {
            visit(x, y, z - 1);
        }
        if z + 1 < nz {
            visit(x, y, z + 1);
        }
    }
    for &i in behind {
        let i = i as usize;
        if !open[i] {
            values[i] = -values[i];
        }
    }
}

/// Linear-light mean of an sRGB-encoded RGBA8 image (a material's average albedo).
#[must_use]
pub fn mean_linear_rgb(rgba: &[u8]) -> Option<[f32; 3]> {
    let lin = |c: u8| {
        let s = f32::from(c) / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    let mut sum = [0f64; 3];
    let mut n = 0u64;
    // Every 7th texel: plenty for a mean and cheap on 4k maps.
    for px in rgba.as_chunks::<4>().0.iter().step_by(7) {
        for c in 0..3 {
            sum[c] += f64::from(lin(px[c]));
        }
        n += 1;
    }
    (n > 0).then(|| sum.map(|s| (s / n as f64) as f32))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Axis-aligned box as 12 outward-wound triangles.
    pub(crate) fn box_mesh(min: [f32; 3], max: [f32; 3]) -> (Vec<[f32; 3]>, Vec<u32>) {
        let p = |x: usize, y: usize, z: usize| {
            [
                if x == 0 { min[0] } else { max[0] },
                if y == 0 { min[1] } else { max[1] },
                if z == 0 { min[2] } else { max[2] },
            ]
        };
        let verts: Vec<[f32; 3]> = (0..8).map(|i| p(i & 1, (i >> 1) & 1, (i >> 2) & 1)).collect();
        // Faces (outward): -x, +x, -y, +y, -z, +z.
        let quads = [
            [0, 4, 6, 2],
            [1, 3, 7, 5],
            [0, 1, 5, 4],
            [2, 6, 7, 3],
            [0, 2, 3, 1],
            [4, 5, 7, 6],
        ];
        let mut idx = Vec::new();
        for q in quads {
            idx.extend_from_slice(&[q[0], q[1], q[2], q[0], q[2], q[3]]);
        }
        (verts, idx)
    }

    #[test]
    fn a_triangle_with_a_non_finite_vertex_is_skipped() {
        // NaN vertex: the geometric normal is NaN, so the old degenerate check
        // (`length_squared() < 1e-14`) kept the triangle and baked inverted
        // (negative) voxels along its finite edge.
        let nan_tri = bake_brick(
            &[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.5, 1.0, f32::NAN]],
            &[0, 1, 2],
        );
        assert!(nan_tri.is_none());
        // The same for an infinite vertex.
        let inf_tri = bake_brick(
            &[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [f32::INFINITY, 1.0, 0.0]],
            &[0, 1, 2],
        );
        assert!(inf_tri.is_none());
        // A bad triangle next to good ones must not change their brick.
        let (v, i) = box_mesh([0.0, 0.0, 0.0], [1.0, 1.0, 1.0]);
        let mut v2 = v.clone();
        let mut i2 = i.clone();
        v2.extend_from_slice(&[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.5, 1.0, f32::NAN]]);
        i2.extend_from_slice(&[8, 9, 10]);
        let clean = bake_brick(&v, &i).expect("clean brick");
        let mixed = bake_brick(&v2, &i2).expect("mixed brick");
        assert_eq!(clean, mixed);
    }

    #[test]
    fn a_box_brick_is_negative_inside_positive_outside() {
        let (v, i) = box_mesh([-1.0, -0.5, -2.0], [1.0, 0.5, 2.0]);
        let brick = bake_brick(&v, &i).expect("brick");
        // Just inside a face the value is negative (signed band), and so is the core the band
        // closes off from the brick's border (ADR-5457: it held the unsigned distance, +0.5, which
        // a ray from outside never reaches but a probe inside the box starts in).
        assert!(
            brick.sample([0.9, 0.0, 0.0]) < -0.05,
            "{}",
            brick.sample([0.9, 0.0, 0.0])
        );
        assert!((brick.sample([0.0, 0.0, 0.0]) + 0.5).abs() < 0.1);
        // Outside within the border voxel: positive.
        let out = brick.sample([1.06, 0.0, 0.0]);
        assert!(out > 0.0 && out < 0.12, "{out}");
        // On the face the distance is near zero.
        assert!(brick.sample([1.0, 0.0, 0.0]).abs() < 0.12);
    }

    #[test]
    fn an_inside_out_box_with_outward_normals_bakes_the_right_way_round() {
        // Wound clockwise from outside (the planet goldens' tower), shaded by outward vertex
        // normals: the winding alone bakes it inside out.
        let (v, mut i) = box_mesh([-1.0, -0.5, -2.0], [1.0, 0.5, 2.0]);
        for tri in i.as_chunks_mut::<3>().0 {
            tri.swap(1, 2);
        }
        let normals: Vec<[f32; 3]> = v
            .iter()
            .map(|p| Vec3::from_array(*p).normalize().to_array())
            .collect();
        assert!(bake_brick(&v, &i).expect("brick").sample([1.06, 0.0, 0.0]) < 0.0);
        let brick = bake_brick_oriented(&v, &normals, &i).expect("brick");
        assert!(
            brick.sample([0.9, 0.0, 0.0]) < -0.05,
            "{}",
            brick.sample([0.9, 0.0, 0.0])
        );
        let out = brick.sample([1.06, 0.0, 0.0]);
        assert!(out > 0.0 && out < 0.12, "{out}");
    }

    #[test]
    fn a_thin_wall_keeps_a_solid_core() {
        // 8 m × 3 m × 0.2 m: the thin axis still has MIN_INTERIOR_DIM voxels.
        let (v, i) = box_mesh([-4.0, 0.0, -0.1], [4.0, 3.0, 0.1]);
        let brick = bake_brick(&v, &i).expect("brick");
        assert!(brick.dims[2] >= MIN_INTERIOR_DIM + 2);
        assert!(brick.voxel[2] <= 0.06);
        assert!(
            brick.sample([0.0, 1.5, 0.0]) < 0.0,
            "{}",
            brick.sample([0.0, 1.5, 0.0])
        );
        // A sphere-traced ray across the wall stops in it.
        let mut z = -0.5f32;
        let mut hit = false;
        for _ in 0..64 {
            let d = brick.sample([0.3, 1.2, z]);
            if d < 0.01 {
                hit = true;
                break;
            }
            z += d.max(0.005);
        }
        assert!(hit && z < 0.0, "ray crossed the wall (z = {z})");
    }

    /// ADR-5457: a ray that starts in the core of a closed solid is inside it all the way to the
    /// wall. The planet goldens' 12 × 28 × 12 m tower has 0.7 m voxels and a 1.05 m signed band;
    /// its core held unsigned (positive) distances, so the probes' march from the middle ran to
    /// the band's inner edge and stopped on a front face a voxel inside the wall: the probes in
    /// the tower saw no back face, stayed active and held the black of a closed cavity.
    #[test]
    fn a_closed_solids_core_is_inside_all_the_way_to_the_wall() {
        let (v, i) = box_mesh([-6.0, 0.0, -6.0], [6.0, 28.0, 6.0]);
        let brick = bake_brick(&v, &i).expect("brick");
        let band = 1.5 * brick.voxel.iter().copied().fold(0.0f32, f32::max);
        assert!((band - 1.05).abs() < 0.01, "{band}");
        // The core: far past the band from every face.
        let centre = brick.sample([0.0, 14.0, 0.0]);
        assert!((centre + 6.0).abs() < 0.2, "{centre}");
        // No sign change on the way out: the first zero crossing is the wall itself.
        for axis in [
            Vec3::X,
            Vec3::NEG_X,
            Vec3::Z,
            Vec3::NEG_Z,
            Vec3::new(0.6, 0.0, 0.8),
        ] {
            let mut t = 0.0f32;
            while t < 5.9 {
                let p = Vec3::new(0.0, 14.0, 0.0) + axis * t;
                let d = brick.sample(p.to_array());
                assert!(
                    d < 0.0,
                    "positive ({d}) at {p:?}, {t} m from the middle along {axis:?}"
                );
                t += 0.05;
            }
        }
        // The march of `sdf_trace` from the middle stops where it starts (inside), not on a shell.
        assert_eq!(
            march(&brick, Vec3::new(0.0, 14.0, 0.0), Vec3::X, 100.0),
            Some(0.0)
        );
        // From outside nothing changed: the wall is where the box is.
        let hit = march(&brick, Vec3::new(-20.0, 14.0, 0.0), Vec3::X, 100.0).expect("hits the wall");
        assert!((hit - 14.0).abs() < 0.4, "{hit}");
    }

    /// ADR-5457 keeps the reason the far field is unsigned: the space behind an open mesh is not
    /// solid. A box without its lid is open to the brick's border through the opening, so the
    /// space in it keeps its unsigned distance, as before.
    #[test]
    fn the_space_in_an_open_box_stays_unsigned() {
        let (v, i) = box_mesh([-6.0, 0.0, -6.0], [6.0, 28.0, 6.0]);
        // Drop the +y face (quad 3: indices 18..24).
        let open: Vec<u32> = i[..18].iter().chain(&i[24..]).copied().collect();
        let brick = bake_brick(&v, &open).expect("brick");
        let centre = brick.sample([0.0, 14.0, 0.0]);
        assert!((centre - 6.0).abs() < 0.2, "{centre}");
        let closed = bake_brick(&v, &i).expect("brick");
        assert!(closed.sample([0.0, 14.0, 0.0]) < 0.0);
    }

    /// A closed pocket of air keeps its positive distance (ADR-5457): a hollow mesh whose inner
    /// faces look into the hollow. Its samples lie in front of their closest triangle, so the
    /// enclosure rule does not touch them.
    #[test]
    fn a_closed_air_pocket_stays_positive() {
        let (mut v, mut i) = box_mesh([-8.0, 0.0, -8.0], [8.0, 16.0, 8.0]);
        // The hollow: an inner box wound inward (2 m walls).
        let (inner_v, inner_i) = box_mesh([-6.0, 2.0, -6.0], [6.0, 14.0, 6.0]);
        let base = v.len() as u32;
        v.extend(inner_v);
        for tri in inner_i.as_chunks::<3>().0 {
            i.extend([base + tri[0], base + tri[2], base + tri[1]]);
        }
        let brick = bake_brick(&v, &i).expect("brick");
        let band = 1.5 * brick.voxel.iter().copied().fold(0.0f32, f32::max);
        assert!(band < 1.0, "{band}");
        // The middle of the hollow: 6 m from its walls, positive.
        let air = brick.sample([0.0, 8.0, 0.0]);
        assert!((air - 6.0).abs() < 0.3, "{air}");
        // The middle of a wall: solid.
        assert!(brick.sample([7.0, 8.0, 0.0]) < 0.0);
    }

    #[test]
    fn grid_search_matches_brute_force() {
        let (v, i) = box_mesh([0.0, 0.0, 0.0], [3.0, 1.0, 2.0]);
        let brick = bake_brick(&v, &i).expect("brick");
        let mesh = crate::sdf::MeshSdf::from_triangles(&v, &i, 8, [1.0; 3]);
        // Compare against the existing brute-force mesh distance inside the brick.
        for p in [[1.5, 0.5, 1.0], [0.4, 0.5, 0.3], [2.9, 0.9, 1.9]] {
            let unsigned_far = brick.sample(p).abs();
            let reference = mesh.sample(p).abs();
            assert!(
                (unsigned_far - reference).abs() < 0.35,
                "{p:?}: {unsigned_far} vs {reference}"
            );
        }
    }

    #[test]
    fn value_count_matches_the_baked_brick() {
        // 4 m × 0.2 m × 0.2 m: the long axis is 40 voxels across, the thin ones stay at 4;
        // each axis stores one sample more than its voxels plus a border sample each side.
        let (v, i) = box_mesh([-2.0, -0.1, -0.1], [2.0, 0.1, 0.1]);
        let count = brick_value_count(&v, &i).expect("count");
        let brick = bake_brick(&v, &i).expect("brick");
        assert_eq!(count, brick.values.len() as u64);
        assert_eq!(count, 43 * 7 * 7);
    }

    /// The GPU's per-brick march (`ddgi_trace.wgsl` `sdf_trace`, identity transform, closest-hit
    /// mode): slab test against the brick's sample bounds, a hit at `d < ε` with ε half the
    /// finest voxel, a step of at least a quarter of it, 64 steps. The hit distance, if any.
    fn march(b: &SdfBrick, o: Vec3, d: Vec3, tmax: f32) -> Option<f32> {
        let lo = Vec3::from_array(b.origin);
        let hi = lo
            + Vec3::new(
                b.voxel[0] * (b.dims[0] - 1) as f32,
                b.voxel[1] * (b.dims[1] - 1) as f32,
                b.voxel[2] * (b.dims[2] - 1) as f32,
            );
        let inv = Vec3::from_array(d.to_array().map(|c| if c.abs() > 1e-12 { 1.0 / c } else { 1e12 }));
        let (a, c) = ((lo - o) * inv, (hi - o) * inv);
        let ta = a.min(c).max_element().max(0.0);
        let tb = a.max(c).min_element().min(tmax);
        let fine = b.voxel.iter().copied().fold(f32::MAX, f32::min);
        let (eps, min_step) = (0.5 * fine, 0.25 * fine);
        let mut t = ta;
        for _ in 0..64 {
            if t > tb {
                break;
            }
            let dist = b.sample((o + d * t).to_array());
            if dist < eps {
                return Some(t);
            }
            t += dist.max(min_step);
        }
        None
    }

    /// A flat quad of `half` metres in the plane `axis` = 0 (two triangles, its normal toward +`axis`).
    fn flat_quad(axis: usize, half: f32) -> (Vec<[f32; 3]>, Vec<u32>) {
        let (u, v) = ((axis + 2) % 3, (axis + 1) % 3);
        let corner = |a: f32, b: f32| {
            let mut p = [0.0f32; 3];
            p[u] = a;
            p[v] = b;
            p
        };
        (
            vec![
                corner(-half, -half),
                corner(-half, half),
                corner(half, half),
                corner(half, -half),
            ],
            vec![0, 1, 2, 0, 2, 3],
        )
    }

    /// ADR-5716: a mesh without thickness (a studio ground quad, a foliage card) bakes its plane's
    /// distance in every sample: positive before it, negative behind it, finite everywhere. Until then
    /// the samples more than three closest-point cells from the plane (0.4 mm of the brick's 1.25 mm)
    /// held +∞.
    #[test]
    fn a_flat_quad_bakes_a_finite_distance_in_every_sample() {
        for (axis, half) in [(1usize, 80.0f32), (2, 4.0), (0, 0.5)] {
            let (v, i) = flat_quad(axis, half);
            let brick = bake_brick(&v, &i).expect("brick");
            assert!(
                brick.values.iter().all(|d| d.is_finite()),
                "axis {axis}: {} of {} values are not finite",
                brick.values.iter().filter(|d| !d.is_finite()).count(),
                brick.values.len()
            );
            // The brick's samples reach a voxel behind the plane and five before it.
            for k in 0..brick.dims[axis] {
                // Off the quad's diagonal, over one triangle's inside.
                let mut p = [0.3 * half; 3];
                p[(axis + 1) % 3] = -0.2 * half;
                p[axis] = brick.origin[axis] + k as f32 * brick.voxel[axis];
                let d = brick.sample(p);
                assert!(
                    (d - p[axis]).abs() <= 1e-6 + 1e-3 * p[axis].abs(),
                    "axis {axis}, plane {k}: {d} at {} from the quad",
                    p[axis]
                );
            }
        }
    }

    /// ADR-5716: a ray from before a flat quad stops on it, steep or shallow, as a ray toward a slab
    /// does. Until then the march's first sample was +∞ and the ray left the brick with one step.
    #[test]
    fn a_ray_toward_a_flat_quad_stops_on_it() {
        let (v, i) = flat_quad(1, 80.0);
        let brick = bake_brick(&v, &i).expect("brick");
        for (height, slope) in [
            (1.0f32, 1.0f32),
            (1.0, 0.5),
            (0.12, 0.3),
            (2.0, 0.15),
            (1.0, 0.08),
        ] {
            let o = Vec3::new(2.0, height, -1.0);
            let d = Vec3::new((1.0 - slope * slope).sqrt(), -slope, 0.0);
            let hit = march(&brick, o, d, 200.0);
            let want = height / slope;
            assert!(
                hit.is_some_and(|t| (t - want).abs() < 2e-3 * want.max(1.0)),
                "from {height} m at slope {slope}: {hit:?}, the plane is {want} m along the ray"
            );
        }
        // From behind, the ray starts in the quad's negative side: a back face at the brick's face.
        let hit = march(&brick, Vec3::new(2.0, -1.0, -1.0), Vec3::Y, 200.0);
        assert!(hit.is_some_and(|t| (t - 1.0).abs() < 2e-3), "{hit:?}");
    }

    /// The tunnel of `regia_pt`'s `gi_acceptance` (ADR-4948 finding G): one 16 × 3 × 0.2 m side
    /// wall (+z), the 16 × 0.2 × 3.4 m roof resting on it and the 80 × 0.5 × 80 m ground under it,
    /// each its own brick, as the probes trace them.
    fn tunnel_bricks() -> Vec<SdfBrick> {
        [
            ([-8.0, 0.0, 1.5], [8.0, 3.0, 1.7]),
            ([-8.0, 3.0, -1.7], [8.0, 3.2, 1.7]),
            ([-40.0, -0.5, -40.0], [40.0, 0.0, 40.0]),
        ]
        .into_iter()
        .map(|(lo, hi)| {
            let (v, i) = box_mesh(lo, hi);
            bake_brick(&v, &i).expect("brick")
        })
        .collect()
    }

    /// A long wall is the same all along its length (ADR-5338): `ddgi_temporal`'s corridor walls (240 m × 4 m ×
    /// 0.3 m, each face two triangles 240 m long) bake every voxel column away from the ends to within a
    /// millimetre of the column at the wall's middle, so the probes see the same wall wherever the camera stands.
    #[test]
    fn a_long_wall_bakes_the_same_all_along_it() {
        for half in [120.0f32, 1200.0] {
            let (v, i) = box_mesh([-half, 0.0, 3.0], [half, 4.0, 3.3]);
            let b = bake_brick(&v, &i).expect("brick");
            let [nx, ny, nz] = b.dims.map(|d| d as usize);
            let at = |x: usize, y: usize, z: usize| b.values[x + nx * (y + ny * z)];
            let mid = nx / 2;
            let mut worst = (0.0f32, 0usize);
            for x in 3..nx - 3 {
                for y in 0..ny {
                    for z in 0..nz {
                        let d = (at(x, y, z) - at(mid, y, z)).abs();
                        if d > worst.0 {
                            worst = (d, x);
                        }
                    }
                }
            }
            assert!(
                worst.0 < 1e-3,
                "{} m wall: column {} differs from the middle by {} m",
                2.0 * half,
                worst.1,
                worst.0
            );
        }
    }

    /// ADR-5338: a plate's edges are where the plate is, however coarse its voxels along the
    /// plate. The wall's 3 m height has 0.375 m voxels; 2 cm inside its top and bottom faces,
    /// across its whole 20 cm thickness, the brick is solid, and 2 cm outside it is empty.
    #[test]
    fn a_plate_brick_keeps_its_edges_where_the_plate_is() {
        let wall = &tunnel_bricks()[0];
        assert!(
            wall.voxel[1] > 0.3,
            "the wall is coarse along its height: {:?}",
            wall.voxel
        );
        for k in 0..=8 {
            let z = 1.51 + 0.0225 * k as f32;
            for x in [-7.9, -3.3, 0.3, 5.1] {
                for (inside, outside) in [(2.98, 3.02), (0.02, -0.02)] {
                    let d_in = wall.sample([x, inside, z]);
                    let d_out = wall.sample([x, outside, z]);
                    assert!(d_in < 0.0, "({x}, {inside}, {z}) inside the wall reads {d_in}");
                    assert!(
                        d_out > 0.0,
                        "({x}, {outside}, {z}) outside the wall reads {d_out}; brick {:?} {:?} {:?}; corners {:?}",
                        wall.origin,
                        wall.voxel,
                        wall.dims,
                        [(-8.0f32, 3.0f32), (-7.6, 3.0), (-8.0, 3.375), (-7.6, 3.375)]
                            .map(|(cx, cy)| (wall.sample([cx, cy, 1.5]), wall.sample([cx, cy, 1.55])))
                    );
                }
            }
        }
    }

    /// ADR-4948 finding G, ADR-5338: the tunnel's roof rests on its wall and the wall stands on the
    /// ground, three bricks that touch. A probe ray from the sunlit outside toward the joints (any
    /// height across them, rising or falling up to 40°) stops in one of the bricks before it is
    /// through the wall: none slips through a slit between the bricks into the tunnel.
    #[test]
    fn touching_plate_bricks_leave_no_slit_at_their_joints() {
        let bricks = tunnel_bricks();
        let mut leaks = Vec::new();
        for joint in [3.0f32, 0.0] {
            for h in 0..=40 {
                let y = joint - 0.1 + 0.005 * h as f32;
                for deg in (-40..=40).step_by(5) {
                    let a = (deg as f32).to_radians();
                    let d = Vec3::new(0.0, a.sin(), -a.cos());
                    // Start 0.6 m outside the wall's outer face, aimed at (y, z = 1.6).
                    let o = Vec3::new(0.3, y, 1.6) - d * (0.5 / a.cos());
                    // Where the ray is through the wall: its inner face less one probe band.
                    let through = (o.z - 1.45) / a.cos();
                    let hit = bricks
                        .iter()
                        .filter_map(|b| march(b, o, d, through))
                        .fold(f32::MAX, f32::min);
                    if hit >= through {
                        leaks.push((joint, y, deg));
                    }
                }
            }
        }
        assert!(
            leaks.is_empty(),
            "{} rays slip through the joints: {:?}",
            leaks.len(),
            &leaks[..leaks.len().min(12)]
        );
    }

    #[test]
    fn mean_albedo_is_linear() {
        let grey = [188u8, 188, 188, 255].repeat(64);
        let m = mean_linear_rgb(&grey).expect("mean");
        assert!((m[0] - 0.5).abs() < 0.01, "{m:?}");
    }
}
