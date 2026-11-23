// SPDX-License-Identifier: MPL-2.0
//! Generated meshes as glTF binary files: what `procgen.detach` writes so a road ribbon or a bent
//! spline piece becomes an ordinary mesh asset (spec 62 § 7.3).

use regia_procgen::GeneratedMesh;

use crate::gltf_write::{GlbMesh, GlbNode, GlbPrimitive, GlbScene, GlbWriteError};

/// The mesh as a GLB (world-space vertices in one node with an identity transform).
///
/// # Errors
///
/// [`GlbWriteError`] when a bounded float accessor (the positions) holds a non-finite component.
pub fn generated_mesh_glb(name: &str, mesh: &GeneratedMesh) -> Result<Vec<u8>, GlbWriteError> {
    let scene = GlbScene {
        generator: "regia procgen.detach".to_string(),
        nodes: vec![GlbNode {
            name: name.to_string(),
            mesh: Some(0),
            ..GlbNode::default()
        }],
        roots: vec![0],
        meshes: vec![GlbMesh {
            name: name.to_string(),
            primitives: vec![GlbPrimitive {
                positions: mesh.positions.clone(),
                normals: mesh.normals.clone(),
                uvs: vec![mesh.uvs.clone()],
                colors: mesh.colors.clone(),
                indices: mesh.indices.clone(),
                ..GlbPrimitive::default()
            }],
            ..GlbMesh::default()
        }],
        ..GlbScene::default()
    };
    scene.to_glb()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit finding 1 (asset_pipeline3 correctness): a generated mesh with a non-finite
    /// position used to get `null` bounds from serde_json 1.0.151 — an invalid glTF, not a
    /// process abort; the export now fails.
    #[test]
    fn a_non_finite_position_fails_the_export_instead_of_panicking() {
        let mesh = GeneratedMesh {
            positions: vec![[f32::NAN, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]],
            ..GeneratedMesh::default()
        };
        assert!(generated_mesh_glb("bad", &mesh).is_err());
    }

    #[test]
    fn a_generated_mesh_writes_a_glb_the_gltf_crate_reads_back() {
        let mesh = GeneratedMesh {
            material: "road".to_string(),
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]],
            normals: vec![[0.0, 1.0, 0.0]; 3],
            uvs: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
            indices: vec![0, 2, 1],
            ..GeneratedMesh::default()
        };
        let bytes = generated_mesh_glb("ribbon", &mesh).expect("writes");
        let glb = gltf::Gltf::from_slice(&bytes).expect("parses");
        let m = glb.document.meshes().next().expect("mesh");
        assert_eq!(m.name(), Some("ribbon"));
        let prim = m.primitives().next().expect("primitive");
        assert_eq!(prim.indices().expect("indices").count(), 3);
        assert_eq!(
            prim.get(&gltf::Semantic::Positions).expect("positions").count(),
            3
        );
    }
}
