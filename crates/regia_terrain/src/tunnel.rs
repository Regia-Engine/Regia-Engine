// SPDX-License-Identifier: MPL-2.0
//! Tunnel render meshes built from the game's collision triangles (spec 26 § 2).
//!
//! The render mesh never replaces the collision geometry: it starts from exactly those
//! triangles, computes area-weighted vertex normals (as the cooked heightfield mesh does) and
//! displaces vertices along their normals by a deterministic value-noise detail so interiors
//! are not perfectly smooth. A [`CollisionProvenance`] record ties the output to its source
//! (glTF import in M4, or a direct hand-off from the simulation's collision mesh) so a CI check
//! can compare the two — the engine side of the spec's "provenance link to the collision
//! source".

use regia_asset::CpuMesh;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::noise;

/// Errors returned for invalid tunnel inputs.
#[derive(Clone, Debug, Error)]
pub enum TunnelError {
    /// The index buffer must be a non-empty multiple of three.
    #[error("tunnel collision indices must be a non-empty multiple of 3, got {0}")]
    BadIndexCount(usize),
    /// An index points outside the vertex array.
    #[error("tunnel collision index {index} out of range (vertex count {vertex_count})")]
    IndexOutOfRange {
        /// The offending index.
        index: u32,
        /// Number of vertices.
        vertex_count: usize,
    },
    /// A vertex coordinate is not finite.
    #[error("tunnel collision vertex {index} is not finite: {value:?}")]
    NonFiniteVertex {
        /// Index of the offending vertex.
        index: usize,
        /// The offending position.
        value: [f32; 3],
    },
    /// Detail parameters must be finite.
    #[error("tunnel detail parameters must be finite")]
    BadDetail,
    /// Displacement pushed a vertex out of the finite range, or the built mesh failed validation.
    #[error("tunnel mesh is invalid after displacement: {0}")]
    InvalidMesh(String),
}

/// How much render detail to add on top of the collision triangles.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct TunnelDetail {
    /// Base displacement along vertex normals, metres (walls slightly offset from collision).
    pub normal_displace: f32,
    /// Noise displacement amplitude, metres.
    pub noise_amplitude: f32,
    /// Noise spatial frequency (1/metres).
    pub noise_scale: f32,
    /// Deterministic noise seed.
    pub seed: u32,
}

impl Default for TunnelDetail {
    fn default() -> Self {
        Self {
            normal_displace: 0.05,
            noise_amplitude: 0.12,
            noise_scale: 0.9,
            seed: 7,
        }
    }
}

/// Options that do not change [`TunnelDetail`] (struct literals list all four of its fields).
///
/// The default leaves the displacement bit-identical to a call without options.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TunnelMeshOptions {
    /// Boundary-loop vertices (edges used by one triangle) keep zero displacement.
    pub pin_boundary: bool,
}

/// Vertices incident to an edge used by exactly one triangle.
fn boundary_vertices(indices: &[u32], vertex_count: usize) -> Vec<bool> {
    let mut count = std::collections::BTreeMap::<(u32, u32), u32>::new();
    for &[a, b, c] in indices.as_chunks::<3>().0 {
        for (u, v) in [(a, b), (b, c), (c, a)] {
            let key = if u < v { (u, v) } else { (v, u) };
            *count.entry(key).or_insert(0) += 1;
        }
    }
    let mut pinned = vec![false; vertex_count];
    for ((u, v), n) in count {
        if n == 1 {
            if let Some(slot) = pinned.get_mut(u as usize) {
                *slot = true;
            }
            if let Some(slot) = pinned.get_mut(v as usize) {
                *slot = true;
            }
        }
    }
    pinned
}

/// Provenance link from a tunnel render mesh to its collision source (spec 26 § 2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CollisionProvenance {
    /// Asset path of the collision source (e.g. the glTF `collision_tunnel_north` node).
    pub source_asset: String,
    /// Triangle count of the collision source, for CI comparison against the render mesh's
    /// un-displaced triangle count.
    pub triangle_count: u32,
    /// Free-form note (generator, export settings).
    pub note: String,
}

/// Builds a tunnel render mesh from collision triangles plus detail displacement.
///
/// The output keeps the input's triangle count and index buffer; positions move along the
/// area-weighted vertex normals by `normal_displace + noise * noise_amplitude`; normals are
/// recomputed after displacement; UVs are a dominant-axis (box) projection with a fixed 4 m
/// tile so wall texture density is uniform.
///
/// # Errors
///
/// See [`TunnelError`].
pub fn tunnel_mesh_from_collision(
    positions: &[[f32; 3]],
    indices: &[u32],
    detail: TunnelDetail,
) -> Result<CpuMesh, TunnelError> {
    tunnel_mesh_from_collision_with(positions, indices, detail, TunnelMeshOptions::default())
}

/// [`tunnel_mesh_from_collision`] with [`TunnelMeshOptions`].
///
/// `pin_boundary: false` (the default) runs the same displacement as
/// [`tunnel_mesh_from_collision`]. `pin_boundary: true` leaves vertices of edges used by exactly
/// one triangle where the collision mesh put them, so a seam zipper can stitch that loop.
///
/// # Errors
///
/// See [`TunnelError`].
pub fn tunnel_mesh_from_collision_with(
    positions: &[[f32; 3]],
    indices: &[u32],
    detail: TunnelDetail,
    options: TunnelMeshOptions,
) -> Result<CpuMesh, TunnelError> {
    if indices.is_empty() || !indices.len().is_multiple_of(3) {
        return Err(TunnelError::BadIndexCount(indices.len()));
    }
    for (i, p) in positions.iter().enumerate() {
        if !p[0].is_finite() || !p[1].is_finite() || !p[2].is_finite() {
            return Err(TunnelError::NonFiniteVertex { index: i, value: *p });
        }
    }
    if !detail.normal_displace.is_finite()
        || !detail.noise_amplitude.is_finite()
        || !detail.noise_scale.is_finite()
    {
        return Err(TunnelError::BadDetail);
    }
    for &index in indices {
        if index as usize >= positions.len() {
            return Err(TunnelError::IndexOutOfRange {
                index,
                vertex_count: positions.len(),
            });
        }
    }

    // Area-weighted vertex normals: each triangle adds its unnormalised cross product
    // (length = twice the area) to its three vertices.
    let mut normals = vec![[0.0f32; 3]; positions.len()];
    for &[a, b, c] in indices.as_chunks::<3>().0 {
        let (a, b, c) = (a as usize, b as usize, c as usize);
        let pa = positions[a];
        let pb = positions[b];
        let pc = positions[c];
        let u = [pb[0] - pa[0], pb[1] - pa[1], pb[2] - pa[2]];
        let v = [pc[0] - pa[0], pc[1] - pa[1], pc[2] - pa[2]];
        let n = [
            u[1] * v[2] - u[2] * v[1],
            u[2] * v[0] - u[0] * v[2],
            u[0] * v[1] - u[1] * v[0],
        ];
        for i in [a, b, c] {
            normals[i][0] += n[0];
            normals[i][1] += n[1];
            normals[i][2] += n[2];
        }
    }
    for normal in &mut normals {
        let len = (normal[0] * normal[0] + normal[1] * normal[1] + normal[2] * normal[2]).sqrt();
        if len.is_finite() && len > 1e-9 {
            *normal = [normal[0] / len, normal[1] / len, normal[2] / len];
        } else {
            *normal = [0.0, 1.0, 0.0];
        }
    }

    let pinned = if options.pin_boundary {
        boundary_vertices(indices, positions.len())
    } else {
        Vec::new()
    };

    // Displace along normals with deterministic noise detail.
    let mut out_positions = Vec::with_capacity(positions.len());
    let mut uvs = Vec::with_capacity(positions.len());
    const TILE: f32 = 4.0;
    for (p, n) in positions.iter().zip(&normals) {
        // Box projection: drop the axis the vertex normal points along, so a floor tiles in
        // x/z and walls tile in their own plane however long the bore is.
        let ax = n[0].abs();
        let ay = n[1].abs();
        let az = n[2].abs();
        let uv = if ay >= ax && ay >= az {
            [p[0] / TILE, p[2] / TILE]
        } else if ax >= az {
            [p[2] / TILE, p[1] / TILE]
        } else {
            [p[0] / TILE, p[1] / TILE]
        };
        uvs.push(uv);
        out_positions.push(*p);
    }
    for (i, (p, n)) in out_positions.iter_mut().zip(&normals).enumerate() {
        if options.pin_boundary && pinned.get(i).copied().unwrap_or(false) {
            continue;
        }
        let noise = noise::value_noise3(
            p[0] * detail.noise_scale,
            p[1] * detail.noise_scale,
            p[2] * detail.noise_scale,
            detail.seed,
        ) - 0.5;
        let d = detail.normal_displace + noise * detail.noise_amplitude;
        p[0] += n[0] * d;
        p[1] += n[1] * d;
        p[2] += n[2] * d;
    }

    if let Some(index) = out_positions
        .iter()
        .position(|p| !p.iter().all(|v| v.is_finite()))
    {
        return Err(TunnelError::InvalidMesh(format!(
            "vertex {index} is not finite after displacement"
        )));
    }

    // Recompute normals after displacement.
    let mut out_normals = vec![[0.0f32; 3]; positions.len()];
    for &[a, b, c] in indices.as_chunks::<3>().0 {
        let (a, b, c) = (a as usize, b as usize, c as usize);
        let pa = out_positions[a];
        let pb = out_positions[b];
        let pc = out_positions[c];
        let u = [pb[0] - pa[0], pb[1] - pa[1], pb[2] - pa[2]];
        let v = [pc[0] - pa[0], pc[1] - pa[1], pc[2] - pa[2]];
        let n = [
            u[1] * v[2] - u[2] * v[1],
            u[2] * v[0] - u[0] * v[2],
            u[0] * v[1] - u[1] * v[0],
        ];
        for i in [a, b, c] {
            out_normals[i][0] += n[0];
            out_normals[i][1] += n[1];
            out_normals[i][2] += n[2];
        }
    }
    for normal in &mut out_normals {
        let len = (normal[0] * normal[0] + normal[1] * normal[1] + normal[2] * normal[2]).sqrt();
        if len.is_finite() && len > 1e-9 {
            *normal = [normal[0] / len, normal[1] / len, normal[2] / len];
        } else {
            *normal = [0.0, 1.0, 0.0];
        }
    }

    let mesh = CpuMesh {
        positions: out_positions,
        normals: out_normals,
        uvs,
        indices: indices.to_vec(),
        tangents: Vec::new(),
        ..Default::default()
    };
    mesh.validate()
        .map_err(|e| TunnelError::InvalidMesh(e.to_string()))?;
    Ok(mesh)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A simple box tunnel: floor + two walls + ceiling, wound CCW seen from inside.
    fn box_tunnel() -> (Vec<[f32; 3]>, Vec<u32>) {
        let (w, h, l) = (4.0f32, 3.0f32, 20.0f32);
        let p = [
            [-w, 0.0, -l],
            [w, 0.0, -l],
            [w, 0.0, l],
            [-w, 0.0, l], // floor 0-3
            [-w, h, -l],
            [w, h, -l],
            [w, h, l],
            [-w, h, l], // ceiling 4-7
        ];
        let mut indices = Vec::new();
        // Floor (normal +y): CCW seen from above.
        indices.extend_from_slice(&[0, 3, 1, 1, 3, 2]);
        // Ceiling (normal -y): CCW seen from below.
        indices.extend_from_slice(&[4, 5, 7, 5, 6, 7]);
        // Left wall x = -w (normal +x).
        indices.extend_from_slice(&[0, 4, 7, 0, 7, 3]);
        // Right wall x = +w (normal -x).
        indices.extend_from_slice(&[1, 2, 6, 1, 6, 5]);
        (p.to_vec(), indices)
    }

    #[test]
    fn builds_from_collision_and_keeps_triangles() {
        let (positions, indices) = box_tunnel();
        let mesh = tunnel_mesh_from_collision(&positions, &indices, TunnelDetail::default()).expect("mesh");
        mesh.validate().expect("mesh invariants");
        assert_eq!(mesh.indices, indices);
        assert_eq!(mesh.positions.len(), positions.len());
        for n in &mesh.normals {
            let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            assert!((len - 1.0).abs() < 1e-4, "normal {n:?} len {len}");
        }
    }

    #[test]
    fn floor_normal_points_up_and_displacement_stays_bounded() {
        // An isolated floor quad: every normal points up.
        let floor = (
            vec![
                [-4.0, 0.0, -10.0],
                [4.0, 0.0, -10.0],
                [4.0, 0.0, 10.0],
                [-4.0, 0.0, 10.0],
            ],
            vec![0u32, 3, 1, 1, 3, 2],
        );
        let mesh = tunnel_mesh_from_collision(&floor.0, &floor.1, TunnelDetail::default()).expect("mesh");
        assert!(
            mesh.normals.iter().all(|n| n[1] > 0.99),
            "floor normals {:?}",
            mesh.normals
        );

        // Box tunnel: displacement bounded by |normal_displace| + |noise| * amplitude.
        let (positions, indices) = box_tunnel();
        let detail = TunnelDetail {
            normal_displace: 0.05,
            noise_amplitude: 0.2,
            noise_scale: 0.5,
            seed: 3,
        };
        let mesh = tunnel_mesh_from_collision(&positions, &indices, detail).expect("mesh");
        for (p, q) in mesh.positions.iter().zip(&positions) {
            let d = ((p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2) + (p[2] - q[2]).powi(2)).sqrt();
            assert!(d <= 0.05 + 0.5 * 0.2 + 1e-4, "displacement {d}");
        }
        // With the noise amplitude zeroed every vertex moves by exactly `normal_displace`
        // along its unit normal, so a mesh that ignores the detail cannot pass.
        let flat = TunnelDetail {
            noise_amplitude: 0.0,
            ..detail
        };
        let mesh = tunnel_mesh_from_collision(&positions, &indices, flat).expect("mesh");
        for (p, q) in mesh.positions.iter().zip(&positions) {
            let d = ((p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2) + (p[2] - q[2]).powi(2)).sqrt();
            assert!((d - 0.05).abs() < 1e-3, "displacement {d}");
        }
    }

    #[test]
    fn uvs_follow_the_vertex_normal_not_the_position() {
        // A long floor: |z| dominates every position, but the normal is +y, so the UV must
        // be x/4, z/4 and the V coordinate must change along the bore.
        let positions = vec![
            [-4.0, 0.0, -20.0],
            [4.0, 0.0, -20.0],
            [4.0, 0.0, 20.0],
            [-4.0, 0.0, 20.0],
        ];
        let indices = vec![0u32, 3, 1, 1, 3, 2];
        let mesh = tunnel_mesh_from_collision(&positions, &indices, TunnelDetail::default()).expect("mesh");
        assert_eq!(mesh.uvs[0], [-1.0, -5.0]);
        assert_eq!(mesh.uvs[2], [1.0, 5.0]);
    }

    #[test]
    fn deterministic_for_the_same_detail() {
        let (positions, indices) = box_tunnel();
        let a = tunnel_mesh_from_collision(&positions, &indices, TunnelDetail::default()).expect("a");
        let b = tunnel_mesh_from_collision(&positions, &indices, TunnelDetail::default()).expect("b");
        assert_eq!(a, b);
        let c = tunnel_mesh_from_collision(
            &positions,
            &indices,
            TunnelDetail {
                seed: 99,
                ..Default::default()
            },
        )
        .expect("c");
        assert_ne!(a.positions, c.positions, "seed changes the detail");
    }

    #[test]
    fn rejects_bad_inputs() {
        let (positions, indices) = box_tunnel();
        assert!(matches!(
            tunnel_mesh_from_collision(&positions, &indices[..5], TunnelDetail::default()),
            Err(TunnelError::BadIndexCount(5))
        ));
        assert!(matches!(
            tunnel_mesh_from_collision(&positions, &[0, 1, 99], TunnelDetail::default()),
            Err(TunnelError::IndexOutOfRange { index: 99, .. })
        ));
        let nan_pos = vec![[0.0, f32::NAN, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        assert!(matches!(
            tunnel_mesh_from_collision(&nan_pos, &[0, 1, 2], TunnelDetail::default()),
            Err(TunnelError::NonFiniteVertex { index: 0, .. })
        ));
        assert!(matches!(
            tunnel_mesh_from_collision(
                &positions,
                &indices,
                TunnelDetail {
                    noise_scale: f32::NAN,
                    ..Default::default()
                }
            ),
            Err(TunnelError::BadDetail)
        ));
    }

    #[test]
    fn displacement_overflow_is_an_error_not_a_panic() {
        let p = [3.0e38f32; 3];
        let result = tunnel_mesh_from_collision(
            &[p, p, p],
            &[0, 1, 2],
            TunnelDetail {
                normal_displace: 3.0e38,
                ..Default::default()
            },
        );
        assert!(matches!(result, Err(TunnelError::InvalidMesh(_))));
    }

    #[test]
    fn provenance_round_trips() {
        let provenance = CollisionProvenance {
            source_asset: "tunnels/collision_north.gltf".into(),
            triangle_count: 8,
            note: "generator v3".into(),
        };
        let text = ron::ser::to_string(&provenance).expect("serialize");
        let back: CollisionProvenance = ron::from_str(&text).expect("parse");
        assert_eq!(provenance, back);
    }

    /// Default options match `tunnel_mesh_from_collision`. Pinning leaves boundary vertices on
    /// the collision positions and still moves an interior vertex.
    #[test]
    fn pin_boundary_keeps_the_loop_and_the_default_is_unchanged() {
        let (positions, indices) = box_tunnel();
        let plain = tunnel_mesh_from_collision(&positions, &indices, TunnelDetail::default()).expect("plain");
        let via = tunnel_mesh_from_collision_with(
            &positions,
            &indices,
            TunnelDetail::default(),
            TunnelMeshOptions::default(),
        )
        .expect("via");
        assert_eq!(plain.positions, via.positions);
        let pinned = tunnel_mesh_from_collision_with(
            &positions,
            &indices,
            TunnelDetail::default(),
            TunnelMeshOptions { pin_boundary: true },
        )
        .expect("pinned");
        // The open ends of the box (z = ±l) are the boundary; every vertex of this box sits on
        // an open edge, so the whole loop stays.
        for (src, out) in positions.iter().zip(&pinned.positions) {
            assert_eq!(src.map(f32::to_bits), out.map(f32::to_bits));
        }
        // A ring of triangles around a centre: the centre's edges are all shared, the ring is
        // the boundary.
        let mut positions = vec![[0.0, 0.0, 0.0]];
        let mut indices = Vec::new();
        let n = 6u32;
        for i in 0..n {
            let a = (i as f32) / n as f32 * std::f32::consts::TAU;
            positions.push([a.cos() * 2.0, 0.0, a.sin() * 2.0]);
        }
        for i in 0..n {
            let b = 1 + i;
            let c = 1 + (i + 1) % n;
            indices.extend_from_slice(&[0, b, c]);
        }
        let pinned = tunnel_mesh_from_collision_with(
            &positions,
            &indices,
            TunnelDetail::default(),
            TunnelMeshOptions { pin_boundary: true },
        )
        .expect("fan");
        assert_eq!(
            positions[1].map(f32::to_bits),
            pinned.positions[1].map(f32::to_bits),
            "a ring vertex stays"
        );
        assert_ne!(
            positions[0].map(f32::to_bits),
            pinned.positions[0].map(f32::to_bits),
            "the interior vertex still displaces"
        );
    }
}
