// SPDX-License-Identifier: MPL-2.0
//! Mesh tunnels through a heightfield: hole mask + imported tunnel mesh + collision
//! triangles (`docs/specs/26-terrain-vfx-highend.md` § 2).
//!
//! The heightfield keeps a [`HolesMask`] so CDLOD drops the entrance triangles; the
//! tunnel interior is an authored (or collision-derived) mesh with a matching trimesh
//! collider built by the physics layer from [`MeshTunnel::collision_trimesh`].

use regia_asset::{CpuMesh, MeshValidationError};
use serde::{Deserialize, Serialize};

use crate::holes::{HoleEllipse, HolesError, HolesMask};
use crate::tunnel::{CollisionProvenance, TunnelDetail, TunnelError, tunnel_mesh_from_collision};

/// A tunnel punched through a heightfield: entrance hole + render mesh + collision.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MeshTunnel {
    /// Elliptical hole(s) on the heightfield surface (tunnel mouth).
    pub holes: HolesMask,
    /// Render mesh (detail-displaced from collision, or an imported mesh).
    pub render_mesh: CpuMesh,
    /// Collision triangle vertices (local space, metres).
    pub collision_vertices: Vec<[f32; 3]>,
    /// Collision triangle indices (triples).
    pub collision_indices: Vec<u32>,
    /// Provenance link to the collision source asset.
    pub provenance: CollisionProvenance,
}

/// Errors building a [`MeshTunnel`].
#[derive(Clone, Debug, thiserror::Error)]
pub enum MeshTunnelError {
    /// Hole mask is invalid.
    #[error(transparent)]
    Holes(#[from] HolesError),
    /// Tunnel mesh is invalid.
    #[error(transparent)]
    Tunnel(#[from] TunnelError),
    /// Imported render mesh failed validation.
    #[error(transparent)]
    Mesh(#[from] MeshValidationError),
    /// Collision index count is not a multiple of 3.
    #[error("collision indices must be a non-empty multiple of 3, got {0}")]
    BadCollisionIndices(usize),
}

impl MeshTunnel {
    /// Builds a tunnel from collision triangles plus a mouth hole on the heightfield.
    ///
    /// # Errors
    ///
    /// See [`MeshTunnelError`].
    pub fn from_collision(
        positions: &[[f32; 3]],
        indices: &[u32],
        mouth: HoleEllipse,
        detail: TunnelDetail,
        provenance: CollisionProvenance,
    ) -> Result<Self, MeshTunnelError> {
        if indices.is_empty() || !indices.len().is_multiple_of(3) {
            return Err(MeshTunnelError::BadCollisionIndices(indices.len()));
        }
        let holes = HolesMask::new(vec![mouth])?;
        let render_mesh = tunnel_mesh_from_collision(positions, indices, detail)?;
        Ok(Self {
            holes,
            render_mesh,
            collision_vertices: positions.to_vec(),
            collision_indices: indices.to_vec(),
            provenance,
        })
    }

    /// Builds a tunnel from an already-authored render mesh (imported glTF) plus
    /// matching collision triangles and a mouth hole.
    ///
    /// # Errors
    ///
    /// See [`MeshTunnelError`].
    pub fn from_imported_mesh(
        render_mesh: CpuMesh,
        collision_vertices: Vec<[f32; 3]>,
        collision_indices: Vec<u32>,
        mouth: HoleEllipse,
        provenance: CollisionProvenance,
    ) -> Result<Self, MeshTunnelError> {
        if collision_indices.is_empty() || !collision_indices.len().is_multiple_of(3) {
            return Err(MeshTunnelError::BadCollisionIndices(collision_indices.len()));
        }
        render_mesh.validate()?;
        let holes = HolesMask::new(vec![mouth])?;
        Ok(Self {
            holes,
            render_mesh,
            collision_vertices,
            collision_indices,
            provenance,
        })
    }

    /// Triangle list for a static/kinematic trimesh collider (`regia_physics::ColliderShape::TriMesh`).
    #[must_use]
    pub fn collision_trimesh(&self) -> (Vec<[f32; 3]>, Vec<[u32; 3]>) {
        let verts = self.collision_vertices.clone();
        // A deserialized tunnel is not re-validated: drop triangles that index past the vertices.
        let count = verts.len();
        let tris: Vec<[u32; 3]> = self
            .collision_indices
            .as_chunks::<3>()
            .0
            .iter()
            .filter(|tri| tri.iter().all(|&i| (i as usize) < count))
            .copied()
            .collect();
        (verts, tris)
    }

    /// World-XZ centre of the primary mouth hole (for ray tests through the entrance).
    #[must_use]
    pub fn mouth_center_xz(&self) -> [f32; 2] {
        self.holes.holes.first().map(|e| e.center).unwrap_or([0.0, 0.0])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn box_tunnel() -> (Vec<[f32; 3]>, Vec<u32>) {
        let (w, h, l) = (4.0f32, 3.0f32, 20.0f32);
        let p = [
            [-w, 0.0, -l],
            [w, 0.0, -l],
            [w, 0.0, l],
            [-w, 0.0, l],
            [-w, h, -l],
            [w, h, -l],
            [w, h, l],
            [-w, h, l],
        ];
        let mut indices = Vec::new();
        indices.extend_from_slice(&[0, 3, 1, 1, 3, 2]);
        indices.extend_from_slice(&[4, 5, 7, 5, 6, 7]);
        indices.extend_from_slice(&[0, 4, 7, 0, 7, 3]);
        indices.extend_from_slice(&[1, 2, 6, 1, 6, 5]);
        (p.to_vec(), indices)
    }

    #[test]
    fn mesh_tunnel_has_hole_and_trimesh() {
        let (pos, idx) = box_tunnel();
        let tunnel = MeshTunnel::from_collision(
            &pos,
            &idx,
            HoleEllipse {
                center: [0.0, -20.0],
                radius: [5.0, 4.0],
                rotation_radians: 0.0,
            },
            TunnelDetail {
                normal_displace: 0.0,
                noise_amplitude: 0.0,
                ..TunnelDetail::default()
            },
            CollisionProvenance {
                source_asset: "collision_tunnel_test".into(),
                triangle_count: (idx.len() / 3) as u32,
                note: "unit test".into(),
            },
        )
        .expect("tunnel");
        assert!(!tunnel.holes.is_empty());
        assert!(tunnel.holes.contains(0.0, -20.0));
        let (verts, tris) = tunnel.collision_trimesh();
        assert_eq!(verts.len(), 8);
        assert_eq!(tris.len(), idx.len() / 3);
        assert_eq!(tunnel.mouth_center_xz(), [0.0, -20.0]);
    }

    #[test]
    fn trimesh_drops_out_of_range_triangles() {
        let (pos, idx) = box_tunnel();
        let mut tunnel = MeshTunnel::from_collision(
            &pos,
            &idx,
            HoleEllipse {
                center: [0.0, -20.0],
                radius: [5.0, 4.0],
                rotation_radians: 0.0,
            },
            TunnelDetail::default(),
            CollisionProvenance {
                source_asset: "collision_tunnel_test".into(),
                triangle_count: (idx.len() / 3) as u32,
                note: "unit test".into(),
            },
        )
        .expect("tunnel");
        tunnel.collision_indices.extend_from_slice(&[0, 1, 99]);
        let (_, tris) = tunnel.collision_trimesh();
        assert_eq!(tris.len(), idx.len() / 3);
    }
}
