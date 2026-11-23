// SPDX-License-Identifier: MPL-2.0
//! MikkTSpace tangent generation.
//!
//! Calls [`bevy_mikktspace`], a safe reimplementation of Morten S. Mikkelsen's public
//! mikktspace algorithm (master's thesis, 2008). glTF `TANGENT` attributes are preferred
//! when present; this fills the gap when they are missing.

use bevy_mikktspace::{Geometry, generate_tangents};

struct CornerMesh<'a> {
    positions: &'a [[f32; 3]],
    normals: &'a [[f32; 3]],
    uvs: &'a [[f32; 2]],
    indices: &'a [u32],
    tangents: &'a mut [[f32; 4]],
}

impl Geometry for CornerMesh<'_> {
    fn num_faces(&self) -> usize {
        self.indices.len() / 3
    }

    fn num_vertices_of_face(&self, _face: usize) -> usize {
        3
    }

    fn position(&self, face: usize, vert: usize) -> [f32; 3] {
        self.positions[self.indices[face * 3 + vert] as usize]
    }

    fn normal(&self, face: usize, vert: usize) -> [f32; 3] {
        self.normals[self.indices[face * 3 + vert] as usize]
    }

    fn tex_coord(&self, face: usize, vert: usize) -> [f32; 2] {
        // glTF's V runs down the image, but its normal textures put +Y up the image (glTF 2.0
        // § 3.9.3, OpenGL convention). MikkTSpace's bitangent follows +V, so V is flipped here
        // (as Blender's importer does) so that `cross(n, t) · w` points up the image.
        let [u, v] = self.uvs[self.indices[face * 3 + vert] as usize];
        [u, 1.0 - v]
    }

    fn set_tangent(
        &mut self,
        tangent_space: Option<bevy_mikktspace::TangentSpace>,
        face: usize,
        vert: usize,
    ) {
        let index = self.indices[face * 3 + vert] as usize;
        if let Some(space) = tangent_space {
            self.tangents[index] = space.tangent_encoded();
        }
    }
}

thread_local! {
    /// MikkTSpace passes [`generate_vertex_tangents`] ran on this thread (ADR-10080).
    static TANGENT_PASSES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// How many MikkTSpace passes [`generate_vertex_tangents`] has run on the calling thread since
/// it started. Per thread, so a check around a synchronous call (`CpuMesh::from_cooked`, a
/// counted mesh upload's `prepare`) is not disturbed by other threads: a GPU-ready mesh that
/// stores its tangents builds its render vertices without a pass (ADR-10080).
#[must_use]
pub fn tangent_passes_on_this_thread() -> u64 {
    TANGENT_PASSES.with(std::cell::Cell::get)
}

/// Writes a tangent xyz and bitangent sign per vertex.
///
/// Each call that reaches the MikkTSpace pass counts in [`tangent_passes_on_this_thread`].
///
/// Returns false when the mesh is not a triangle list, an index is out of range, or generation fails. On failure
/// `tangents` is left as the caller initialised it.
pub fn generate_vertex_tangents(
    positions: &[[f32; 3]],
    normals: &[[f32; 3]],
    uvs: &[[f32; 2]],
    indices: &[u32],
    tangents: &mut [[f32; 4]],
) -> bool {
    if !indices.len().is_multiple_of(3)
        || positions.len() != normals.len()
        || positions.len() != uvs.len()
        || positions.len() != tangents.len()
    {
        return false;
    }
    if indices.iter().any(|&index| index as usize >= positions.len()) {
        return false;
    }
    TANGENT_PASSES.with(|n| n.set(n.get() + 1));
    let mut mesh = CornerMesh {
        positions,
        normals,
        uvs,
        indices,
        tangents,
    };
    generate_tangents(&mut mesh).is_ok()
}

/// Writes smooth per-vertex normals from triangle geometry (facet normals
/// accumulated per vertex, then normalised).
///
/// Used when a glTF mesh carries no `NORMAL` attribute (e.g. the Khronos Fox):
/// a zero normal would zero every `N·L` term and leave the mesh unlit.
///
/// Returns false when the mesh is not a triangle list or an index is out of range; `normals` is then left
/// as the caller initialised it.
pub fn generate_vertex_normals(positions: &[[f32; 3]], indices: &[u32], normals: &mut [[f32; 3]]) -> bool {
    if !indices.len().is_multiple_of(3) || positions.len() != normals.len() {
        return false;
    }
    if indices.iter().any(|&index| index as usize >= positions.len()) {
        return false;
    }
    let mut acc = vec![[0.0f32; 3]; positions.len()];
    for tri in indices.chunks(3) {
        let a = positions[tri[0] as usize];
        let b = positions[tri[1] as usize];
        let c = positions[tri[2] as usize];
        let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let ac = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
        // Cross product; degenerate triangles contribute nothing.
        let face = [
            ab[1] * ac[2] - ab[2] * ac[1],
            ab[2] * ac[0] - ab[0] * ac[2],
            ab[0] * ac[1] - ab[1] * ac[0],
        ];
        for index in tri {
            let slot = &mut acc[*index as usize];
            slot[0] += face[0];
            slot[1] += face[1];
            slot[2] += face[2];
        }
    }
    for (normal, sum) in normals.iter_mut().zip(acc) {
        let len = (sum[0] * sum[0] + sum[1] * sum[1] + sum[2] * sum[2]).sqrt();
        if len > 1.0e-12 {
            normal[0] = sum[0] / len;
            normal[1] = sum[1] / len;
            normal[2] = sum[2] / len;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    #[test]
    fn out_of_range_indices_return_false() {
        let positions = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let normals = [[0.0, 0.0, 1.0]; 3];
        let uvs = [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]];
        let mut tangents = [[0.0; 4]; 3];
        assert!(!generate_vertex_tangents(
            &positions,
            &normals,
            &uvs,
            &[0, 1, 3],
            &mut tangents
        ));
        let mut out = [[0.0; 3]; 3];
        assert!(!generate_vertex_normals(&positions, &[0, 1, 3], &mut out));
    }

    use super::*;

    #[test]
    fn quad_tangent_follows_u() {
        let positions = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 1.0, 0.0], [0.0, 1.0, 0.0]];
        let normals = [[0.0, 0.0, 1.0]; 4];
        let uvs = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
        let indices = [0, 1, 2, 0, 2, 3];
        let mut tangents = [[0.0; 4]; 4];
        assert!(generate_vertex_tangents(
            &positions,
            &normals,
            &uvs,
            &indices,
            &mut tangents
        ));
        for tangent in tangents {
            assert!(tangent[0] > 0.9, "tangent should point along +U, got {tangent:?}");
            assert!(tangent[3].abs() > 0.5);
        }
    }

    #[test]
    fn bitangent_points_up_the_image_as_gltf_normal_textures_expect() {
        // glTF UVs: V = 0 at the top of the image. Here V grows with +Y, so "up the image"
        // (the normal texture's +Y, glTF 2.0 § 3.9.3) is world −Y. A Cycles render of the same
        // glTF (dome normal map) is the reference this was checked against.
        let positions = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 1.0, 0.0], [0.0, 1.0, 0.0]];
        let normals = [[0.0, 0.0, 1.0]; 4];
        let indices = [0, 1, 2, 0, 2, 3];
        for (flip_u, want_t) in [(false, 1.0), (true, -1.0)] {
            let u = |x: f32| if flip_u { 1.0 - x } else { x };
            let uvs = [[u(0.0), 0.0], [u(1.0), 0.0], [u(1.0), 1.0], [u(0.0), 1.0]];
            let mut tangents = [[0.0; 4]; 4];
            assert!(generate_vertex_tangents(
                &positions,
                &normals,
                &uvs,
                &indices,
                &mut tangents
            ));
            for t in tangents {
                assert!((t[0] - want_t).abs() < 1e-4, "{t:?}");
                // bitangent = cross(n, t) · w with n = +Z: (−t.y, t.x, 0) · w.
                let b_y = t[0] * t[3];
                assert!(b_y < -0.99, "bitangent must point up the image (−Y here): {t:?}");
            }
        }
    }

    #[test]
    fn generated_normals_follow_the_winding() {
        // CCW quad in the XY plane facing +Z.
        let positions = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 1.0, 0.0], [0.0, 1.0, 0.0]];
        let indices = [0, 1, 2, 0, 2, 3];
        let mut normals = [[0.0; 3]; 4];
        assert!(generate_vertex_normals(&positions, &indices, &mut normals));
        for normal in normals {
            assert!((normal[2] - 1.0).abs() < 1e-5, "expected +Z, got {normal:?}");
        }
        // Reversed winding faces -Z.
        let indices = [0, 2, 1, 0, 3, 2];
        let mut normals = [[0.0; 3]; 4];
        assert!(generate_vertex_normals(&positions, &indices, &mut normals));
        for normal in normals {
            assert!((normal[2] + 1.0).abs() < 1e-5, "expected -Z, got {normal:?}");
        }
    }

    #[test]
    fn normal_generation_rejects_bad_counts() {
        let positions = [[0.0; 3]; 3];
        let indices = [0, 1];
        let mut normals = [[0.0; 3]; 3];
        assert!(!generate_vertex_normals(&positions, &indices, &mut normals));
    }

    #[test]
    fn out_of_range_index_is_rejected_not_a_panic() {
        let positions = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let normals = [[0.0, 0.0, 1.0]; 3];
        let uvs = [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]];
        let indices = [0, 1, 3];
        let mut tangents = [[7.0; 4]; 3];
        assert!(!generate_vertex_tangents(
            &positions,
            &normals,
            &uvs,
            &indices,
            &mut tangents
        ));
        assert_eq!(tangents, [[7.0; 4]; 3], "tangents stay as initialised on failure");
        let mut out = [[5.0; 3]; 3];
        assert!(!generate_vertex_normals(&positions, &indices, &mut out));
        assert_eq!(out, [[5.0; 3]; 3]);
    }

    #[test]
    fn mismatched_attribute_lengths_and_partial_triangles_are_rejected() {
        let positions = [[0.0; 3]; 3];
        let normals = [[0.0, 0.0, 1.0]; 2];
        let uvs = [[0.0; 2]; 3];
        let mut tangents = [[0.0; 4]; 3];
        assert!(!generate_vertex_tangents(
            &positions,
            &normals,
            &uvs,
            &[0, 1, 2],
            &mut tangents
        ));
        let normals = [[0.0, 0.0, 1.0]; 3];
        assert!(!generate_vertex_tangents(
            &positions,
            &normals,
            &uvs,
            &[0, 1],
            &mut tangents
        ));
        let mut out = [[0.0; 3]; 2];
        assert!(!generate_vertex_normals(&positions, &[0, 1, 2], &mut out));
    }

    #[test]
    fn generated_normals_of_a_flat_triangle_are_unit_and_degenerate_ones_stay() {
        let positions = [[0.0, 0.0, 0.0], [2.0, 0.0, 0.0], [0.0, 2.0, 0.0], [5.0, 5.0, 5.0]];
        let mut normals = [[9.0, 9.0, 9.0]; 4];
        assert!(generate_vertex_normals(&positions, &[0, 1, 2], &mut normals));
        for n in &normals[..3] {
            assert!((n[2] - 1.0).abs() < 1.0e-6 && n[0].abs() < 1.0e-6 && n[1].abs() < 1.0e-6);
        }
        assert_eq!(
            normals[3],
            [9.0, 9.0, 9.0],
            "an unreferenced vertex keeps its initial normal"
        );
    }
}
