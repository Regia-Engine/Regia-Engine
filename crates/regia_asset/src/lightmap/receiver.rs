// SPDX-License-Identifier: MPL-2.0
//! Receiver geometry for the forward pass (ADR-1556, the ADR-860 step 2 plan): a lightmapped
//! mesh is drawn from its entry's unwrap, because a chart seam splits a source vertex into
//! several unwrapped vertices with different UV2. [`Lightmap::check_receiver`] proves the mesh is
//! the one that was baked (same vertex count, same triangles in the same order), then
//! [`Lightmap::remap_attribute`] copies any per-vertex attribute (position, normal, UV0, tangent,
//! colour) through `xref`, and [`Lightmap::receiver_uv2`] gives the matching atlas UV2. The
//! entry's own indices ([`super::MeshUnwrap::indices`]) index the remapped vertices.

use super::{Lightmap, LightmapError};

impl Lightmap {
    /// Checks that a source mesh with `source_vertices` vertices and the triangle list
    /// `source_indices` is the mesh entry `entry` was baked for: the vertex count matches and
    /// every unwrapped triangle copies the source triangle at the same place.
    ///
    /// # Errors
    ///
    /// [`LightmapError::Layout`] naming the entry: no such entry, a different vertex or index
    /// count, or the first triangle that differs (the mesh changed since the bake; re-bake it).
    pub fn check_receiver(
        &self,
        entry: usize,
        source_vertices: usize,
        source_indices: &[u32],
    ) -> Result<(), LightmapError> {
        let (Some(e), Some(u)) = (self.entries.get(entry), self.unwraps.get(entry)) else {
            return Err(LightmapError::Layout(format!(
                "no entry {entry} (the lightmap has {})",
                self.entries.len()
            )));
        };
        if e.source_vertices as usize != source_vertices {
            return Err(LightmapError::Layout(format!(
                "entry `{}` was baked for {} vertices, the mesh has {source_vertices} (re-bake it)",
                e.name, e.source_vertices
            )));
        }
        if u.indices.len() != source_indices.len() {
            return Err(LightmapError::Layout(format!(
                "entry `{}` was baked for {} indices, the mesh has {} (re-bake it)",
                e.name,
                u.indices.len(),
                source_indices.len()
            )));
        }
        for (k, (&i, &s)) in u.indices.iter().zip(source_indices).enumerate() {
            if u.xref.get(i as usize) != Some(&s) {
                return Err(LightmapError::Layout(format!(
                    "entry `{}`: triangle {} differs from the baked mesh (re-bake it)",
                    e.name,
                    k / 3
                )));
            }
        }
        Ok(())
    }

    /// Copies a per-vertex attribute of the source mesh to entry `entry`'s unwrapped vertices
    /// (`out[v] = attribute[xref[v]]`). Call [`Self::check_receiver`] first; a missing entry or an
    /// `xref` past `attribute` gives `None`.
    #[must_use]
    pub fn remap_attribute<T: Copy>(&self, entry: usize, attribute: &[T]) -> Option<Vec<T>> {
        self.unwraps
            .get(entry)?
            .xref
            .iter()
            .map(|&x| attribute.get(x as usize).copied())
            .collect()
    }

    /// Atlas UV2 (`0..1` over the whole atlas) of entry `entry`'s unwrapped vertices: the local
    /// UV2 through the entry's `scale_offset`, what the forward pass's UV2 stream holds when it
    /// does not apply `scale_offset` itself.
    #[must_use]
    pub fn receiver_uv2(&self, entry: usize) -> Option<Vec<[f32; 2]>> {
        let e = self.entries.get(entry)?;
        let s = e.scale_offset;
        Some(
            self.unwraps
                .get(entry)?
                .uv
                .iter()
                .map(|uv| [uv[0] * s[0] + s[2], uv[1] * s[1] + s[3]])
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Coverage, LightmapBakeInfo, LightmapEntry, LightmapLayout, MeshUnwrap};
    use super::*;

    /// A quad (4 source vertices, 2 triangles) unwrapped with one seam: source vertex 2 is split
    /// into unwrapped vertices 2 and 4, placed in the right half of a 4×2 atlas.
    fn seamed() -> Lightmap {
        Lightmap::from_irradiance(
            LightmapLayout {
                width: 4,
                height: 2,
                density: 1.0,
                padding: 1,
                entries: vec![LightmapEntry {
                    name: "quad".into(),
                    source_vertices: 4,
                    rect: [2, 0, 2, 2],
                    scale_offset: [0.5, 1.0, 0.5, 0.0],
                    vertex_count: 0,
                    index_count: 0,
                    first_vertex: 0,
                    first_index: 0,
                    charts: 2,
                    area_m2: 1.0,
                    texels_per_metre: 1.0,
                    key: None,
                    seams: None,
                    page: 0,
                }],
                unwraps: vec![MeshUnwrap {
                    xref: vec![0, 1, 2, 3, 2],
                    uv: vec![[0.0, 0.0], [0.5, 0.0], [0.5, 1.0], [0.0, 1.0], [1.0, 1.0]],
                    indices: vec![0, 1, 2, 0, 4, 3],
                }],
            },
            &[[1.0; 3]; 8],
            vec![Coverage::BAKED; 8],
            LightmapBakeInfo::default(),
        )
        .expect("valid")
    }

    #[test]
    fn the_baked_mesh_passes_and_a_changed_mesh_is_named() {
        let map = seamed();
        let source = [0u32, 1, 2, 0, 2, 3];
        map.check_receiver(0, 4, &source).expect("the baked mesh");
        let msg = |r: Result<(), LightmapError>| r.expect_err("refused").to_string();
        assert!(msg(map.check_receiver(0, 5, &source)).contains("4 vertices"));
        assert!(msg(map.check_receiver(0, 4, &source[..3])).contains("indices"));
        assert!(msg(map.check_receiver(0, 4, &[0, 1, 2, 0, 3, 2])).contains("triangle 1"));
        assert!(msg(map.check_receiver(1, 4, &source)).contains("no entry 1"));
        assert_eq!(
            map.check_receiver(0, 5, &source).expect_err("x").code(),
            "lightmap_layout"
        );
    }

    #[test]
    fn attributes_follow_xref_and_uv2_lands_in_the_entry_rectangle() {
        let map = seamed();
        let positions = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 1.0, 0.0], [0.0, 1.0, 0.0]];
        let remapped = map.remap_attribute(0, &positions).expect("remapped");
        assert_eq!(remapped.len(), 5);
        assert_eq!(
            remapped[4], positions[2],
            "the seam copy keeps the source position"
        );
        // Drawing the unwrap's indices over the remapped positions gives the source triangles.
        let tris: Vec<[f32; 3]> = map.unwraps[0]
            .indices
            .iter()
            .map(|&i| remapped[i as usize])
            .collect();
        let want: Vec<[f32; 3]> = [0usize, 1, 2, 0, 2, 3].iter().map(|&i| positions[i]).collect();
        assert_eq!(tris, want);
        let uv2 = map.receiver_uv2(0).expect("uv2");
        assert_eq!(uv2[0], [0.5, 0.0]);
        assert_eq!(uv2[4], [1.0, 1.0]);
        assert!(
            uv2.iter()
                .all(|u| (0.5..=1.0).contains(&u[0]) && (0.0..=1.0).contains(&u[1]))
        );
        assert_eq!(
            map.remap_attribute(0, &positions[..2]),
            None,
            "an xref past the attribute"
        );
        assert_eq!(map.remap_attribute::<u8>(3, &[]), None);
        assert_eq!(map.receiver_uv2(3), None);
    }
}
