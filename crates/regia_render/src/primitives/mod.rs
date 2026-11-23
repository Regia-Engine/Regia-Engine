// SPDX-License-Identifier: MPL-2.0
//! Procedural meshes: the render lab's sphere/box helpers, the greybox primitive meshers, the
//! immediate 3D primitives in [`models`] and the allocation-free 2D shape fast paths in
//! [`triangulation`].
//!
//! The parametric shapes ([`Cuboid`], [`Sphere`], [`Capsule`], [`Cylinder`], [`Cone`],
//! [`ConicalFrustum`], [`Plane`], [`Torus`], [`Tetrahedron`], [`Extrusion`]) are ported from
//! Bevy's `bevy_mesh` primitive meshers (MIT OR Apache-2.0, used under MIT; each file carries the
//! notice) and adapted to Regia Engine: +Y up, counter-clockwise front faces, [`LabVertex`] output with
//! MikkTSpace tangents, parameter validation instead of panics, and a smooth cone tip.
//! [`Primitive`] is the scene component that references one of them by parameters.

pub mod models;
pub mod triangulation;

use crate::lab_frame::LabVertex;
use crate::runtime::CpuMesh;

mod capsule;
mod cone;
mod conical_frustum;
mod cuboid;
mod cylinder;
mod extrusion;
mod plane;
mod shape;
mod sphere;
mod tetrahedron;
mod torus;

pub use capsule::{Capsule, CapsuleUvProfile};
pub use cone::{Cone, ConeAnchor};
pub use conical_frustum::ConicalFrustum;
pub use cuboid::Cuboid;
pub use cylinder::{Cylinder, CylinderAnchor};
pub use extrusion::{Extrusion, Profile};
pub use plane::Plane;
pub use shape::{
    PRIMITIVE_FORMAT_VERSION, Primitive, PrimitiveCache, PrimitiveError, PrimitiveMeshMarker, PrimitiveShape,
    primitive_migration_sources, primitive_schema_fingerprint, sync_primitive_meshes,
};
pub use sphere::Sphere;
pub use tetrahedron::Tetrahedron;
pub use torus::Torus;

/// Builds a UV sphere centered at the origin.
///
/// The positions come from the platform `sin`/`cos`, which may differ in the last bit between
/// hosts; [`uv_sphere_deterministic`] is the same mesh with the same bits on every host.
#[must_use]
pub fn uv_sphere(radius: f32, segments: u32, rings: u32) -> (Vec<LabVertex>, Vec<u32>) {
    uv_sphere_with(radius, segments, rings, |a| (a.sin(), a.cos()))
}

/// [`uv_sphere`] with `regia_core::math::det::sin_cos` in place of the platform `sin`/`cos`:
/// the same topology, UVs and indices, and positions and normals that are bit-identical on
/// every host and architecture (ADR-7878). For test fixtures and cooks whose bytes are pinned.
#[must_use]
pub fn uv_sphere_deterministic(radius: f32, segments: u32, rings: u32) -> (Vec<LabVertex>, Vec<u32>) {
    uv_sphere_with(radius, segments, rings, regia_core::math::det::sin_cos)
}

fn uv_sphere_with(
    radius: f32,
    segments: u32,
    rings: u32,
    sin_cos: impl Fn(f32) -> (f32, f32),
) -> (Vec<LabVertex>, Vec<u32>) {
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    for ring in 0..=rings {
        let v = ring as f32 / rings as f32;
        let phi = v * std::f32::consts::PI;
        let (phi_sin, phi_cos) = sin_cos(phi);
        for seg in 0..=segments {
            let u = seg as f32 / segments as f32;
            let theta = u * std::f32::consts::TAU;
            let (theta_sin, theta_cos) = sin_cos(theta);
            let x = radius * phi_sin * theta_cos;
            let y = radius * phi_cos;
            let z = radius * phi_sin * theta_sin;
            let nx = if radius > 0.0 { x / radius } else { 0.0 };
            let ny = if radius > 0.0 { y / radius } else { 1.0 };
            let nz = if radius > 0.0 { z / radius } else { 0.0 };
            vertices.push(LabVertex {
                position: [x, y, z],
                normal: [nx, ny, nz],
                uv: [u, v],
                tangent: [1.0, 0.0, 0.0, 1.0],
                color: [1.0; 4],
                joints: [0; 4],
                weights: [0.0; 4],
            });
        }
    }
    let stride = segments + 1;
    for ring in 0..rings {
        for seg in 0..segments {
            let i0 = ring * stride + seg;
            let i1 = i0 + 1;
            let i2 = i0 + stride;
            let i3 = i2 + 1;
            // Counter-clockwise seen from outside the sphere (front faces survive
            // back-face culling); the previous [i0, i2, i1, …] order rendered the
            // inside, so camera-facing fragments carried away-facing normals and
            // the sun lit only silhouette edges.
            indices.extend_from_slice(&[i0, i1, i2, i1, i3, i2]);
        }
    }
    (vertices, indices)
}

/// An axis-aligned box centred at the origin, outward normals, six faces.
#[must_use]
pub fn box_mesh(half: [f32; 3]) -> (Vec<LabVertex>, Vec<u32>) {
    let (hx, hy, hz) = (half[0].abs(), half[1].abs(), half[2].abs());
    // Each face is four corners, CCW when viewed from outside.
    let faces: [[([f32; 3], [f32; 2]); 4]; 6] = [
        // +X
        [
            ([hx, -hy, hz], [0.0, 1.0]),
            ([hx, -hy, -hz], [1.0, 1.0]),
            ([hx, hy, -hz], [1.0, 0.0]),
            ([hx, hy, hz], [0.0, 0.0]),
        ],
        // -X
        [
            ([-hx, -hy, -hz], [0.0, 1.0]),
            ([-hx, -hy, hz], [1.0, 1.0]),
            ([-hx, hy, hz], [1.0, 0.0]),
            ([-hx, hy, -hz], [0.0, 0.0]),
        ],
        // +Y
        [
            ([-hx, hy, hz], [0.0, 1.0]),
            ([hx, hy, hz], [1.0, 1.0]),
            ([hx, hy, -hz], [1.0, 0.0]),
            ([-hx, hy, -hz], [0.0, 0.0]),
        ],
        // -Y
        [
            ([-hx, -hy, -hz], [0.0, 1.0]),
            ([hx, -hy, -hz], [1.0, 1.0]),
            ([hx, -hy, hz], [1.0, 0.0]),
            ([-hx, -hy, hz], [0.0, 0.0]),
        ],
        // +Z
        [
            ([-hx, -hy, hz], [0.0, 1.0]),
            ([hx, -hy, hz], [1.0, 1.0]),
            ([hx, hy, hz], [1.0, 0.0]),
            ([-hx, hy, hz], [0.0, 0.0]),
        ],
        // -Z
        [
            ([hx, -hy, -hz], [0.0, 1.0]),
            ([-hx, -hy, -hz], [1.0, 1.0]),
            ([-hx, hy, -hz], [1.0, 0.0]),
            ([hx, hy, -hz], [0.0, 0.0]),
        ],
    ];
    let normals = [
        [1.0, 0.0, 0.0],
        [-1.0, 0.0, 0.0],
        [0.0, 1.0, 0.0],
        [0.0, -1.0, 0.0],
        [0.0, 0.0, 1.0],
        [0.0, 0.0, -1.0],
    ];
    let mut vertices = Vec::with_capacity(24);
    let mut indices = Vec::with_capacity(36);
    for (face, normal) in faces.into_iter().zip(normals) {
        let base = vertices.len() as u32;
        for (position, uv) in face {
            vertices.push(LabVertex {
                position,
                normal,
                uv,
                tangent: [1.0, 0.0, 0.0, 1.0],
                color: [1.0; 4],
                joints: [0; 4],
                weights: [0.0; 4],
            });
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    (vertices, indices)
}

/// Translates mesh vertices by `offset`.
#[must_use]
pub fn translate_mesh(vertices: &[LabVertex], offset: [f32; 3]) -> Vec<LabVertex> {
    vertices
        .iter()
        .map(|v| LabVertex {
            position: [
                v.position[0] + offset[0],
                v.position[1] + offset[1],
                v.position[2] + offset[2],
            ],
            ..*v
        })
        .collect()
}

/// Positions, normals, UVs and triangle-list indices of a mesh under construction.
///
/// The meshers fill these the way Bevy's builders fill mesh attributes; [`MeshParts::finish`]
/// turns them into Regia Engine vertices with MikkTSpace tangents.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct MeshParts {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub uvs: Vec<[f32; 2]>,
    pub indices: Vec<u32>,
}

impl MeshParts {
    /// Empty parts with room for `vertices` vertices and `indices` indices.
    pub fn with_capacity(vertices: usize, indices: usize) -> Self {
        Self {
            positions: Vec::with_capacity(vertices),
            normals: Vec::with_capacity(vertices),
            uvs: Vec::with_capacity(vertices),
            indices: Vec::with_capacity(indices),
        }
    }

    /// Appends one vertex and returns its index.
    pub fn push(&mut self, position: [f32; 3], normal: [f32; 3], uv: [f32; 2]) -> u32 {
        let index = self.positions.len() as u32;
        self.positions.push(position);
        self.normals.push(normal);
        self.uvs.push(uv);
        index
    }

    /// Builds Regia Engine vertices: white colour, no skinning, MikkTSpace tangents (a tangent
    /// perpendicular to the normal where MikkTSpace has nothing to say, e.g. degenerate UVs).
    pub fn finish(self) -> CpuMesh {
        let mut tangents: Vec<[f32; 4]> = self.normals.iter().map(|n| fallback_tangent(*n)).collect();
        // A failed generation leaves the fallback tangents in place; every mesher emits triangle
        // lists with matching attribute lengths, so this only trips on degenerate input.
        let _ = crate::tangents::generate_vertex_tangents(
            &self.positions,
            &self.normals,
            &self.uvs,
            &self.indices,
            &mut tangents,
        );
        let vertices = self
            .positions
            .iter()
            .zip(&self.normals)
            .zip(&self.uvs)
            .zip(&tangents)
            .map(|(((position, normal), uv), tangent)| LabVertex {
                position: *position,
                normal: *normal,
                uv: *uv,
                tangent: *tangent,
                color: [1.0; 4],
                joints: [0; 4],
                weights: [0.0; 4],
            })
            .collect();
        CpuMesh {
            vertices,
            indices: self.indices,
        }
    }
}

/// A unit tangent perpendicular to `normal` (bitangent sign +1).
fn fallback_tangent(normal: [f32; 3]) -> [f32; 4] {
    let n = regia_core::math::Vec3::from_array(normal);
    let helper = if n.x.abs() < 0.9 {
        regia_core::math::Vec3::X
    } else {
        regia_core::math::Vec3::Y
    };
    let t = (helper - n * n.dot(helper)).normalize_or(regia_core::math::Vec3::X);
    [t.x, t.y, t.z, 1.0]
}

/// Mesh checks shared by the meshers' unit tests: welded-edge manifoldness, unit normals,
/// UV ranges and winding against the vertex normals.
#[cfg(test)]
pub(crate) mod check {
    use std::collections::HashMap;

    use regia_core::math::Vec3;

    use crate::runtime::CpuMesh;

    /// Quantised position key (0.1 mm grid) so seam duplicates weld.
    fn key(p: [f32; 3]) -> [i64; 3] {
        p.map(|c| (c * 10_000.0).round() as i64)
    }

    /// Undirected welded edges → how many triangles use each, and the directed count.
    fn edge_uses(mesh: &CpuMesh) -> HashMap<([i64; 3], [i64; 3]), (u32, i32)> {
        let mut edges: HashMap<([i64; 3], [i64; 3]), (u32, i32)> = HashMap::new();
        for tri in mesh.indices.chunks(3) {
            let k = [0, 1, 2].map(|i| key(mesh.vertices[tri[i] as usize].position));
            if k[0] == k[1] || k[1] == k[2] || k[0] == k[2] {
                continue; // degenerate after welding (pole fans)
            }
            for (a, b) in [(k[0], k[1]), (k[1], k[2]), (k[2], k[0])] {
                let (lo, hi, dir) = if a < b { (a, b, 1) } else { (b, a, -1) };
                let entry = edges.entry((lo, hi)).or_insert((0, 0));
                entry.0 += 1;
                entry.1 += dir;
            }
        }
        edges
    }

    /// Every welded edge is shared by exactly two triangles with opposite directions: a closed,
    /// consistently wound surface.
    pub fn assert_watertight(mesh: &CpuMesh, what: &str) {
        let edges = edge_uses(mesh);
        assert!(!edges.is_empty(), "{what}: no edges");
        for ((a, b), (uses, dir)) in &edges {
            assert!(
                *uses == 2 && *dir == 0,
                "{what}: edge {a:?}-{b:?} used {uses} times (direction sum {dir})"
            );
        }
    }

    /// Number of welded edges used by exactly one triangle (the open boundary).
    pub fn boundary_edges(mesh: &CpuMesh) -> usize {
        edge_uses(mesh).values().filter(|(uses, _)| *uses == 1).count()
    }

    /// Normals are unit length, tangents unit length and perpendicular to the normal, UVs in
    /// `[0, 1]`, indices in range, and every non-degenerate triangle faces the way its vertex
    /// normals point (counter-clockwise front faces).
    pub fn assert_well_formed(mesh: &CpuMesh, what: &str) {
        assert!(
            mesh.indices.len().is_multiple_of(3),
            "{what}: not a triangle list"
        );
        for (i, v) in mesh.vertices.iter().enumerate() {
            let n = Vec3::from_array(v.normal);
            assert!(
                (n.length() - 1.0).abs() < 1.0e-4,
                "{what}: normal {i} length {}",
                n.length()
            );
            let t = Vec3::new(v.tangent[0], v.tangent[1], v.tangent[2]);
            assert!(
                (t.length() - 1.0).abs() < 1.0e-3,
                "{what}: tangent {i} length {}",
                t.length()
            );
            assert!(t.dot(n).abs() < 1.0e-2, "{what}: tangent {i} not perpendicular");
            assert!(
                v.tangent[3].abs() == 1.0,
                "{what}: bitangent sign {}",
                v.tangent[3]
            );
            for c in v.uv {
                assert!((-1.0e-5..=1.0 + 1.0e-5).contains(&c), "{what}: uv {i} {:?}", v.uv);
            }
            assert!(v.position.iter().all(|c| c.is_finite()), "{what}: position {i}");
        }
        for tri in mesh.indices.chunks(3) {
            assert!(
                tri.iter().all(|&i| (i as usize) < mesh.vertices.len()),
                "{what}: index range"
            );
            let p = [0, 1, 2].map(|k| Vec3::from_array(mesh.vertices[tri[k] as usize].position));
            let face = (p[1] - p[0]).cross(p[2] - p[0]);
            if face.length() < 1.0e-9 {
                continue;
            }
            let n: Vec3 = tri
                .iter()
                .map(|&i| Vec3::from_array(mesh.vertices[i as usize].normal))
                .sum();
            assert!(
                face.dot(n) > 0.0,
                "{what}: triangle {tri:?} wound against its normals"
            );
        }
    }

    /// Triangle count.
    pub fn triangles(mesh: &CpuMesh) -> usize {
        mesh.indices.len() / 3
    }
}

#[cfg(test)]
mod sphere_bits {
    use super::{uv_sphere, uv_sphere_deterministic};

    /// FNV-1a over the little-endian bits (the hash the macOS pass of ADR-7711 printed).
    fn fnv(bytes: impl IntoIterator<Item = u8>) -> u64 {
        bytes.into_iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        })
    }

    /// The deterministic sphere is the platform sphere's mesh with every position and normal from
    /// `det::sin_cos` (ADR-7878): same UVs and indices, positions equal to the kernel expression,
    /// and the 96 × 48 rock's position bits pinned. The pin is the hash the Apple M5's platform
    /// sphere printed in ADR-7711 (`5f83ac53efa9eee1`); x86_64 glibc's `sinf`/`cosf` give
    /// `aeaec308db7b202a` for the same sphere, which is why the platform sphere is not used for
    /// cooks whose bytes are compared across hosts.
    #[test]
    fn the_deterministic_sphere_has_the_same_bits_on_every_host() {
        let (det, det_indices) = uv_sphere_deterministic(1.0, 96, 48);
        let (platform, indices) = uv_sphere(1.0, 96, 48);
        assert_eq!(det_indices, indices);
        assert_eq!(det.len(), platform.len());
        for (d, p) in det.iter().zip(&platform) {
            assert_eq!(d.uv, p.uv);
        }
        for (i, d) in det.iter().enumerate() {
            let (ring, seg) = (i / 97, i % 97);
            let phi = (ring as f32 / 48.0) * std::f32::consts::PI;
            let theta = (seg as f32 / 96.0) * std::f32::consts::TAU;
            let (ps, pc) = regia_core::math::det::sin_cos(phi);
            let (ts, tc) = regia_core::math::det::sin_cos(theta);
            let want = [ps * tc, pc, ps * ts];
            assert_eq!(d.position.map(f32::to_bits), want.map(f32::to_bits), "vertex {i}");
        }
        let pos = fnv(det
            .iter()
            .flat_map(|v| v.position.iter().flat_map(|c| c.to_bits().to_le_bytes())));
        assert_eq!(pos, 0x5f83_ac53_efa9_eee1, "position bits moved: {pos:016x}");
    }
}
