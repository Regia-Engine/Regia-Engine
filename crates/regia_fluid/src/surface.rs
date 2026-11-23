// SPDX-License-Identifier: MPL-2.0
//! The liquid surface: a particle level set (Zhu and Bridson 2005, § 3: the distance to the
//! kernel-weighted mean particle position minus the weighted mean radius) sampled on a lattice finer
//! than the solver grid, and a triangle mesh of its zero set from a marching-cubes cell walk
//! (Lorensen and Cline 1987). Each cube is split into six tetrahedra around its main diagonal (Doi
//! and Koide 1991), which gives a watertight surface with no ambiguous cases and no 256-case table.

use std::collections::HashMap;

use glam::Vec3;

#[cfg(feature = "draw")]
use crate::grid::GridDims;
use crate::sim::FluidSim;
#[cfg(feature = "draw")]
use crate::sim::{Bins, bin_positions};

/// A triangle mesh.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SurfaceMesh {
    /// Vertex positions (world).
    pub positions: Vec<[f32; 3]>,
    /// Unit vertex normals (pointing out of the liquid).
    pub normals: Vec<[f32; 3]>,
    /// Triangle list.
    pub indices: Vec<u32>,
}

impl SurfaceMesh {
    /// Triangles.
    #[must_use]
    pub fn triangle_count(&self) -> usize {
        self.indices.len() / 3
    }

    /// The enclosed volume by the divergence theorem (closed meshes only).
    #[must_use]
    pub fn enclosed_volume(&self) -> f32 {
        let mut v = 0.0f64;
        for t in self.indices.as_chunks::<3>().0 {
            let [a, b, c] = t.map(|i| Vec3::from(self.positions[i as usize]).as_dvec3());
            v += a.dot(b.cross(c)) / 6.0;
        }
        v as f32
    }
}

/// Most triangles one mesh may have.
pub const MAX_SURFACE_TRIANGLES: usize = 2_000_000;

/// Lattice samples per solver cell edge (1..=4).
pub const MAX_SURFACE_RESOLUTION: u32 = 4;

/// Cube corners (x, y, z offsets) in the numbering of `TETRAHEDRA`.
const CORNERS: [[usize; 3]; 8] = [
    [0, 0, 0],
    [1, 0, 0],
    [1, 1, 0],
    [0, 1, 0],
    [0, 0, 1],
    [1, 0, 1],
    [1, 1, 1],
    [0, 1, 1],
];

/// Scratch for the level set and the surface mesh: the sample buffer, the narrow-band mask and the
/// edge map stay allocated across builds.
#[derive(Debug, Clone, Default)]
pub(crate) struct SurfaceScratch {
    /// Node samples, `far` outside the narrow band.
    phi: Vec<f32>,
    /// Solver cells that hold a particle.
    occupied: Vec<bool>,
    /// Solver cells within Chebyshev distance 1 of an occupied cell.
    band: Vec<bool>,
    /// Marching-cubes edge vertices. Never iterated: vertex order is the cell walk.
    edges: HashMap<(u32, u32), u32>,
    /// Mesh built in place. The public methods clone it out.
    mesh: SurfaceMesh,
    /// Nodes per axis of the last build.
    n: [usize; 3],
    /// Node spacing of the last build.
    h: f32,
}

/// Six tetrahedra sharing the diagonal 0-6.
const TETRAHEDRA: [[usize; 4]; 6] = [
    [0, 5, 1, 6],
    [0, 1, 2, 6],
    [0, 2, 3, 6],
    [0, 3, 7, 6],
    [0, 7, 4, 6],
    [0, 4, 5, 6],
];

impl FluidSim {
    /// The particle level set on a lattice of `resolution` samples per cell edge: values at
    /// `(n * resolution + 1)` nodes per axis (x fastest), negative inside the liquid.
    #[must_use]
    pub fn level_set(&self, resolution: u32) -> (Vec<f32>, [usize; 3], f32) {
        let _span = tracing::info_span!(target: "regia_fluid", "fluid_level_set").entered();
        let mut scratch = std::mem::take(&mut *self.surface_scratch.lock());
        let (visits, allocated) = self.fill_level_set(&mut scratch, resolution);
        let out = (scratch.phi.clone(), scratch.n, scratch.h);
        let cap = scratch.phi.capacity();
        *self.surface_scratch.lock() = scratch;
        self.note_level_set(visits, cap, allocated);
        out
    }

    /// Writes the narrow-band level set into `scratch`. Returns `(nodes walked, sample buffer grew)`.
    fn fill_level_set(&self, scratch: &mut SurfaceScratch, resolution: u32) -> (u64, bool) {
        let res = resolution.clamp(1, MAX_SURFACE_RESOLUTION) as usize;
        let d = *self.dims();
        let n = [d.nx * res + 1, d.ny * res + 1, d.nz * res + 1];
        let h = d.dx / res as f32;
        let support = d.dx;
        let radius = d.dx * 0.35;
        let far = support;
        let need = n[0] * n[1] * n[2];
        let allocated = scratch.phi.capacity() < need;
        if scratch.phi.len() != need {
            scratch.phi.resize(need, far);
        }
        scratch.phi.fill(far);
        scratch.n = n;
        scratch.h = h;
        let pos = self.positions();
        if pos.is_empty() {
            return (0, allocated);
        }
        let cells = d.cells();
        if scratch.occupied.len() != cells {
            scratch.occupied.resize(cells, false);
        }
        scratch.occupied.fill(false);
        if scratch.band.len() != cells {
            scratch.band.resize(cells, false);
        }
        scratch.band.fill(false);
        let held = self.hold_bins();
        for &p in held.bins.items.iter() {
            let (i, j, k) = self.cell_coords_clamped(pos[p as usize]);
            scratch.occupied[d.cell(i, j, k)] = true;
        }
        let mut seeds = Vec::new();
        for c in 0..cells {
            if scratch.occupied[c] {
                let i = c % d.nx;
                let j = (c / d.nx) % d.ny;
                let k = c / (d.nx * d.ny);
                seeds.push((i, j, k));
            }
        }
        for (i, j, k) in seeds {
            for kk in k.saturating_sub(1)..=(k + 1).min(d.nz - 1) {
                for jj in j.saturating_sub(1)..=(j + 1).min(d.ny - 1) {
                    for ii in i.saturating_sub(1)..=(i + 1).min(d.nx - 1) {
                        scratch.band[d.cell(ii, jj, kk)] = true;
                    }
                }
            }
        }
        let mut visited = 0u64;
        for k in 0..n[2] {
            for j in 0..n[1] {
                for i in 0..n[0] {
                    let ci = (i / res).min(d.nx - 1);
                    let cj = (j / res).min(d.ny - 1);
                    let ck = (k / res).min(d.nz - 1);
                    if !scratch.band[d.cell(ci, cj, ck)] {
                        continue;
                    }
                    visited += 1;
                    let x = d.origin + Vec3::new(i as f32, j as f32, k as f32) * h;
                    let mut wsum = 0.0f32;
                    let mut xsum = Vec3::ZERO;
                    for kk in ck.saturating_sub(1)..=(ck + 1).min(d.nz - 1) {
                        for jj in cj.saturating_sub(1)..=(cj + 1).min(d.ny - 1) {
                            for ii in ci.saturating_sub(1)..=(ci + 1).min(d.nx - 1) {
                                for &p in held.bins.cell(d.cell(ii, jj, kk)) {
                                    let s2 = (pos[p as usize] - x).length_squared() / (support * support);
                                    if s2 < 1.0 {
                                        let t = 1.0 - s2;
                                        let w = t * t * t;
                                        wsum += w;
                                        xsum += pos[p as usize] * w;
                                    }
                                }
                            }
                        }
                    }
                    if wsum > 0.0 {
                        let mean = xsum / wsum;
                        scratch.phi[i + n[0] * (j + n[1] * k)] = ((x - mean).length() - radius).min(far);
                    }
                }
            }
        }
        self.restore_bins(held);
        // The closed domain is outside: the boundary nodes are air, so the mesh closes on the walls.
        for k in 0..n[2] {
            for j in 0..n[1] {
                for i in 0..n[0] {
                    if i == 0 || j == 0 || k == 0 || i == n[0] - 1 || j == n[1] - 1 || k == n[2] - 1 {
                        let v = &mut scratch.phi[i + n[0] * (j + n[1] * k)];
                        *v = v.max(h * 0.5);
                    }
                }
            }
        }
        (visited, allocated)
    }

    /// The liquid surface mesh at `resolution` samples per cell edge (see the module docs).
    #[must_use]
    pub fn surface_mesh(&self, resolution: u32) -> SurfaceMesh {
        let (phi, n, h) = self.level_set(resolution);
        let _span = tracing::info_span!(target: "regia_fluid", "fluid_marching_cubes").entered();
        let mut scratch = std::mem::take(&mut *self.surface_scratch.lock());
        polygonize_into(
            &phi,
            n,
            h,
            self.dims().origin,
            &mut scratch.edges,
            &mut scratch.mesh,
        );
        let mesh = scratch.mesh.clone();
        *self.surface_scratch.lock() = scratch;
        mesh
    }
}

/// [`FluidSim::level_set`] of the liquid particles `pos` on the grid `dims` (cache playback
/// draws a recorded frame's liquid through this).
#[cfg(feature = "draw")]
pub(crate) fn level_set_of(dims: &GridDims, pos: &[Vec3], resolution: u32) -> (Vec<f32>, [usize; 3], f32) {
    let _span = tracing::info_span!(target: "regia_fluid", "fluid_level_set").entered();
    let res = resolution.clamp(1, MAX_SURFACE_RESOLUTION) as usize;
    let d = *dims;
    let n = [d.nx * res + 1, d.ny * res + 1, d.nz * res + 1];
    let h = d.dx / res as f32;
    // Kernel support one cell; particle radius a little over the rest half-spacing.
    let support = d.dx;
    let radius = d.dx * 0.35;
    let far = support;
    let mut phi = vec![far; n[0] * n[1] * n[2]];
    let bins: Bins = bin_positions(dims, pos);
    if pos.is_empty() {
        return (phi, n, h);
    }
    for k in 0..n[2] {
        for j in 0..n[1] {
            for i in 0..n[0] {
                let x = d.origin + Vec3::new(i as f32, j as f32, k as f32) * h;
                let ci = (i / res).min(d.nx - 1);
                let cj = (j / res).min(d.ny - 1);
                let ck = (k / res).min(d.nz - 1);
                let mut wsum = 0.0f32;
                let mut xsum = Vec3::ZERO;
                for kk in ck.saturating_sub(1)..=(ck + 1).min(d.nz - 1) {
                    for jj in cj.saturating_sub(1)..=(cj + 1).min(d.ny - 1) {
                        for ii in ci.saturating_sub(1)..=(ci + 1).min(d.nx - 1) {
                            for &p in bins.cell(d.cell(ii, jj, kk)) {
                                let s2 = (pos[p as usize] - x).length_squared() / (support * support);
                                if s2 < 1.0 {
                                    let t = 1.0 - s2;
                                    let w = t * t * t;
                                    wsum += w;
                                    xsum += pos[p as usize] * w;
                                }
                            }
                        }
                    }
                }
                if wsum > 0.0 {
                    let mean = xsum / wsum;
                    phi[i + n[0] * (j + n[1] * k)] = ((x - mean).length() - radius).min(far);
                }
            }
        }
    }
    // The closed domain is outside: the boundary nodes are air, so the mesh closes on the walls.
    for k in 0..n[2] {
        for j in 0..n[1] {
            for i in 0..n[0] {
                if i == 0 || j == 0 || k == 0 || i == n[0] - 1 || j == n[1] - 1 || k == n[2] - 1 {
                    let v = &mut phi[i + n[0] * (j + n[1] * k)];
                    *v = v.max(h * 0.5);
                }
            }
        }
    }
    (phi, n, h)
}

/// [`FluidSim::surface_mesh`] of the liquid particles `pos` on the grid `dims`.
#[cfg(feature = "draw")]
pub(crate) fn surface_mesh_of(dims: &GridDims, pos: &[Vec3], resolution: u32) -> SurfaceMesh {
    let (phi, n, h) = level_set_of(dims, pos, resolution);
    let _span = tracing::info_span!(target: "regia_fluid", "fluid_marching_cubes").entered();
    polygonize(&phi, n, h, dims.origin)
}

/// Triangulates the zero set of `phi` (node values on an `n` lattice with spacing `h` from `origin`).
#[must_use]
pub fn polygonize(phi: &[f32], n: [usize; 3], h: f32, origin: Vec3) -> SurfaceMesh {
    let mut edges = HashMap::new();
    let mut mesh = SurfaceMesh::default();
    polygonize_into(phi, n, h, origin, &mut edges, &mut mesh);
    mesh
}

/// The same triangulation as [`polygonize`], writing into reused buffers. `edges` is cleared and
/// never iterated, so its hasher does not affect the mesh.
fn polygonize_into(
    phi: &[f32],
    n: [usize; 3],
    h: f32,
    origin: Vec3,
    edges: &mut HashMap<(u32, u32), u32>,
    mesh: &mut SurfaceMesh,
) {
    edges.clear();
    mesh.positions.clear();
    mesh.normals.clear();
    mesh.indices.clear();
    if n.iter().any(|&v| v < 2) || phi.len() < n[0] * n[1] * n[2] {
        return;
    }
    let node = |i: usize, j: usize, k: usize| i + n[0] * (j + n[1] * k);
    let at = |id: usize| -> Vec3 {
        let i = id % n[0];
        let j = (id / n[0]) % n[1];
        let k = id / (n[0] * n[1]);
        origin + Vec3::new(i as f32, j as f32, k as f32) * h
    };
    let vertex = |mesh: &mut SurfaceMesh, edges: &mut HashMap<(u32, u32), u32>, a: usize, b: usize| -> u32 {
        let key = (a.min(b) as u32, a.max(b) as u32);
        if let Some(&v) = edges.get(&key) {
            return v;
        }
        let (pa, pb) = (phi[a], phi[b]);
        let t = if (pa - pb).abs() > 1e-12 {
            (pa / (pa - pb)).clamp(0.0, 1.0)
        } else {
            0.5
        };
        let p = at(a) + (at(b) - at(a)) * t;
        let v = mesh.positions.len() as u32;
        mesh.positions.push(p.to_array());
        mesh.normals.push([0.0; 3]);
        edges.insert(key, v);
        v
    };
    'cells: for k in 0..n[2] - 1 {
        for j in 0..n[1] - 1 {
            for i in 0..n[0] - 1 {
                let ids: [usize; 8] = CORNERS.map(|c| node(i + c[0], j + c[1], k + c[2]));
                let inside = ids.map(|id| phi[id] < 0.0);
                if inside.iter().all(|v| *v) || inside.iter().all(|v| !*v) {
                    continue;
                }
                for tet in TETRAHEDRA {
                    let v = tet.map(|c| ids[c]);
                    let mut ins = [0usize; 4];
                    let mut outs = [0usize; 4];
                    let mut n_in = 0usize;
                    let mut n_out = 0usize;
                    for &id in &v {
                        if phi[id] < 0.0 {
                            ins[n_in] = id;
                            n_in += 1;
                        } else {
                            outs[n_out] = id;
                            n_out += 1;
                        }
                    }
                    let mut tris = [[0u32; 3]; 2];
                    let n_tri = match (n_in, n_out) {
                        (1, 3) => {
                            tris[0] = [
                                vertex(&mut *mesh, &mut *edges, ins[0], outs[0]),
                                vertex(&mut *mesh, &mut *edges, ins[0], outs[1]),
                                vertex(&mut *mesh, &mut *edges, ins[0], outs[2]),
                            ];
                            1
                        }
                        (3, 1) => {
                            tris[0] = [
                                vertex(&mut *mesh, &mut *edges, outs[0], ins[0]),
                                vertex(&mut *mesh, &mut *edges, outs[0], ins[1]),
                                vertex(&mut *mesh, &mut *edges, outs[0], ins[2]),
                            ];
                            1
                        }
                        (2, 2) => {
                            let a = vertex(&mut *mesh, &mut *edges, ins[0], outs[0]);
                            let b = vertex(&mut *mesh, &mut *edges, ins[0], outs[1]);
                            let c = vertex(&mut *mesh, &mut *edges, ins[1], outs[1]);
                            let d = vertex(&mut *mesh, &mut *edges, ins[1], outs[0]);
                            tris[0] = [a, b, c];
                            tris[1] = [a, c, d];
                            2
                        }
                        _ => 0,
                    };
                    if n_tri == 0 {
                        continue;
                    }
                    // Orient each triangle out of the liquid: toward the outside corners.
                    let centroid =
                        |ids: &[usize]| ids.iter().fold(Vec3::ZERO, |s, id| s + at(*id)) / ids.len() as f32;
                    let outward = centroid(&outs[..n_out]) - centroid(&ins[..n_in]);
                    for mut t in tris.into_iter().take(n_tri) {
                        let [a, b, c] = t.map(|x| Vec3::from(mesh.positions[x as usize]));
                        let normal = (b - a).cross(c - a);
                        if normal.dot(outward) < 0.0 {
                            t.swap(1, 2);
                        }
                        if normal.length_squared() <= 0.0 {
                            continue;
                        }
                        mesh.indices.extend_from_slice(&t);
                        if mesh.indices.len() / 3 >= MAX_SURFACE_TRIANGLES {
                            break 'cells;
                        }
                    }
                }
            }
        }
    }
    // Area-weighted vertex normals. The accumulator is the one allocation this walk still makes;
    // it tracks the vertex count, which the reused mesh already sized.
    let mut acc = vec![Vec3::ZERO; mesh.positions.len()];
    for t in mesh.indices.as_chunks::<3>().0 {
        let [a, b, c] = t.map(|x| Vec3::from(mesh.positions[x as usize]));
        let nrm = (b - a).cross(c - a);
        for &x in t {
            acc[x as usize] += nrm;
        }
    }
    for (out, a) in mesh.normals.iter_mut().zip(acc) {
        *out = a.normalize_or_zero().to_array();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sphere's level set polygonizes to a closed mesh with about the sphere's volume.
    #[test]
    fn sphere_volume_is_close() {
        let n = [33, 33, 33];
        let h = 1.0 / 32.0;
        let center = Vec3::splat(0.5);
        let mut phi = vec![0.0; n[0] * n[1] * n[2]];
        for k in 0..n[2] {
            for j in 0..n[1] {
                for i in 0..n[0] {
                    let p = Vec3::new(i as f32, j as f32, k as f32) * h;
                    phi[i + n[0] * (j + n[1] * k)] = (p - center).length() - 0.3;
                }
            }
        }
        let mesh = polygonize(&phi, n, h, Vec3::ZERO);
        let exact = 4.0 / 3.0 * std::f32::consts::PI * 0.3f32.powi(3);
        let v = mesh.enclosed_volume();
        assert!((v - exact).abs() / exact < 0.03, "{v} vs {exact}");
        assert!(mesh.triangle_count() > 100);
        assert!(
            mesh.normals
                .iter()
                .all(|n| (Vec3::from(*n).length() - 1.0).abs() < 1e-3)
        );
    }

    /// One particle in a corner cell: the kernel only reaches the nodes of that cell and its
    /// Chebyshev-1 neighbours. A second build reuses the sample buffer.
    #[test]
    fn level_set_visits_only_the_narrow_band() {
        use crate::sim::{FluidParams, FluidSim};

        let mut params = FluidParams {
            bounds_min: Vec3::ZERO,
            bounds_max: Vec3::splat(0.8),
            cell_size: 0.1,
            gravity: Vec3::ZERO,
            reseed: false,
            separation: 0.0,
            particle_cap: 65_536,
            ..FluidParams::default()
        };
        params.whitewater.enabled = false;
        let mut sim = FluidSim::new(params).expect("sim");
        sim.set_particles(vec![Vec3::new(0.05, 0.05, 0.05)], vec![Vec3::ZERO]);
        let (phi, n, _) = sim.level_set(1);
        let full = (n[0] * n[1] * n[2]) as u64;
        let visits = sim.level_set_node_visits();
        // Cell 0 and the three +1 neighbours: 2 nodes per axis at resolution 1.
        let band = 2usize * 2 * 2;
        eprintln!("level set visits {visits} of {full} nodes, phi len {}", phi.len());
        assert!(
            visits <= band as u64,
            "walked {visits} nodes, the narrow band is {band} (full lattice {full})"
        );
        let allocs = sim.level_set_allocs();
        let cap = sim.level_set_phi_capacity();
        assert!(cap >= phi.len());
        let _ = sim.level_set(1);
        assert_eq!(
            sim.level_set_allocs(),
            allocs,
            "the second level set allocated again"
        );
        assert_eq!(sim.level_set_phi_capacity(), cap);
    }
}
