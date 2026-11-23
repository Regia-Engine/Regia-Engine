// SPDX-License-Identifier: MPL-2.0
//! Visibility-buffer clusters for a lightmap entry's unwrap (ADR-2107, `regia bake-lightmaps
//! --meshlets`).
//!
//! A receiver draws its entry's unwrap, where a chart seam splits a vertex, so clusters cooked from
//! the source mesh do not fit it: their counts differ, and their coarse levels were free to
//! collapse across a seam, where one triangle would read two charts of the atlas. These clusters
//! are built over the unwrapped vertices. A collapse joins vertices that share an edge, and two
//! charts share no vertex there, so a triangle of any level keeps all three corners in one chart
//! ([`cluster_triangles_across_charts`] checks that).

use regia_asset::lightmap::MeshUnwrap;
use regia_render::MeshletMesh;

use crate::meshlet::{
    MeshletAttributes, MeshletBuildError, MeshletSettings, build_meshlet_mesh_with_attributes,
};

/// Object-space attributes of the source mesh an unwrap was laid out from: the ones the runtime
/// draws. The clusters' bounds and errors must be in the space the instance's transform applies to,
/// so these are not the world-space positions the unwrap's charts were measured in.
#[derive(Clone, Copy, Debug)]
pub struct SourceAttributes<'a> {
    /// Vertex positions.
    pub positions: &'a [[f32; 3]],
    /// Vertex normals; not as long as `positions` means none (the simplifier then keeps position
    /// only).
    pub normals: &'a [[f32; 3]],
    /// Texture coordinates; not as long as `positions` means none.
    pub uvs: &'a [[f32; 2]],
}

/// Builds the cluster hierarchy of `unwrap` over `source`'s vertices, copied through its `xref`, as
/// `regia_render::lightmap::receiver_mesh` copies them for the draw.
///
/// # Errors
///
/// An `xref` past `source` ([`MeshletBuildError::IndexOutOfRange`]), what the builder refuses, or
/// a cluster triangle across two charts ([`MeshletBuildError::Invalid`]): the entry then keeps the
/// bake and draws in the batches, where it cannot read the wrong part of the atlas.
pub fn meshlets_for_unwrap(
    source: SourceAttributes<'_>,
    unwrap: &MeshUnwrap,
) -> Result<MeshletMesh, MeshletBuildError> {
    let vertices = source.positions.len();
    let out_of_range = |&index: &u32| MeshletBuildError::IndexOutOfRange { index, vertices };
    let positions = unwrap
        .xref
        .iter()
        .map(|x| {
            source
                .positions
                .get(*x as usize)
                .copied()
                .ok_or_else(|| out_of_range(x))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let copied = source.normals.len() == vertices && source.uvs.len() == vertices;
    let normals: Vec<[f32; 3]> = if copied {
        through_xref(&unwrap.xref, source.normals)
    } else {
        Vec::new()
    };
    let uvs: Vec<[f32; 2]> = if copied {
        through_xref(&unwrap.xref, source.uvs)
    } else {
        Vec::new()
    };
    let attributes = copied.then_some(MeshletAttributes {
        normals: &normals,
        uvs: &uvs,
    });
    let mesh = build_meshlet_mesh_with_attributes(
        &positions,
        attributes,
        &unwrap.indices,
        &MeshletSettings::default(),
    )?;
    let charts = chart_of_each_vertex(unwrap.xref.len(), &unwrap.indices);
    match cluster_triangles_across_charts(&mesh, &charts) {
        0 => Ok(mesh),
        n => Err(MeshletBuildError::Invalid(format!(
            "{n} cluster triangles cross a chart seam"
        ))),
    }
}

/// `values` copied through `xref`, whose entries the caller has checked against their length.
fn through_xref<T: Copy>(xref: &[u32], values: &[T]) -> Vec<T> {
    xref.iter().map(|&x| values[x as usize]).collect()
}

/// The chart of each unwrapped vertex: its connected set over `indices` (a seam split leaves two
/// charts with no vertex in common). Ids are the smallest vertex of the set.
#[must_use]
pub fn chart_of_each_vertex(vertices: usize, indices: &[u32]) -> Vec<u32> {
    fn find(parent: &mut [u32], mut v: u32) -> u32 {
        while parent[v as usize] != v {
            let next = parent[parent[v as usize] as usize];
            parent[v as usize] = next;
            v = next;
        }
        v
    }
    let mut parent: Vec<u32> = (0..vertices as u32).collect();
    for t in indices.as_chunks::<3>().0 {
        let mut root = find(&mut parent, t[0]);
        for &corner in &t[1..] {
            let other = find(&mut parent, corner);
            parent[root.max(other) as usize] = root.min(other);
            root = root.min(other);
        }
    }
    (0..vertices as u32).map(|v| find(&mut parent, v)).collect()
}

/// Triangles of `mesh`, over every level, whose corners are in more than one of `charts`
/// ([`chart_of_each_vertex`]). Each would interpolate UV2 across the atlas between two charts.
#[must_use]
pub fn cluster_triangles_across_charts(mesh: &MeshletMesh, charts: &[u32]) -> usize {
    (0..mesh.meshlets.len())
        .flat_map(|cluster| mesh.cluster_indices(cluster))
        .filter(|t| {
            let chart = t.map(|v| charts.get(v as usize));
            chart[0] != chart[1] || chart[1] != chart[2]
        })
        .count()
}

#[cfg(test)]
mod tests {
    use regia_asset::lightmap::LightmapLayout;

    use super::*;
    use crate::lightmap_uv::{LightmapUvSettings, UnwrapInput, build_atlas_layout, build_lightmap_layout};

    /// Positions, normals, texture coordinates and indices.
    type Sphere = (Vec<[f32; 3]>, Vec<[f32; 3]>, Vec<[f32; 2]>, Vec<u32>);

    /// A bumpy UV sphere: curved all over, so the unwrap cuts it into many charts.
    fn bumpy_sphere(rings: u32, segments: u32) -> Sphere {
        let (mut p, mut n, mut t) = (Vec::new(), Vec::new(), Vec::new());
        for r in 0..=rings {
            let v = r as f32 / rings as f32;
            let polar = v * std::f32::consts::PI;
            for s in 0..=segments {
                let u = s as f32 / segments as f32;
                let az = u * std::f32::consts::TAU;
                let bump = 1.0 + 0.05 * (5.0 * az).sin() * (7.0 * polar).sin();
                let dir = [polar.sin() * az.cos(), polar.cos(), polar.sin() * az.sin()];
                p.push(dir.map(|c| c * bump));
                n.push(dir);
                t.push([u, v]);
            }
        }
        let mut i = Vec::new();
        for r in 0..rings {
            for s in 0..segments {
                let a = r * (segments + 1) + s;
                let b = a + segments + 1;
                i.extend_from_slice(&[a, b, a + 1, a + 1, b, b + 1]);
            }
        }
        (p, n, t, i)
    }

    fn layouts(p: &[[f32; 3]], i: &[u32]) -> [(&'static str, LightmapLayout); 2] {
        let input = [UnwrapInput {
            name: "sphere",
            positions: p,
            indices: i,
            density_scale: 1.0,
        }];
        let settings = LightmapUvSettings {
            texels_per_metre: 32.0,
            ..LightmapUvSettings::default()
        };
        [
            (
                "charts",
                build_lightmap_layout(&input, &settings).expect("charts layout").0,
            ),
            (
                "atlas",
                build_atlas_layout(&input, &settings, 1).expect("atlas layout").0,
            ),
        ]
    }

    /// Every level of the clusters of a curved mesh's unwrap keeps each triangle inside one chart,
    /// on both generators, and has coarse levels at all.
    #[test]
    fn every_cluster_triangle_of_a_curved_receiver_stays_in_one_chart() {
        let (p, n, t, i) = bumpy_sphere(48, 96);
        for (name, layout) in layouts(&p, &i) {
            let unwrap = &layout.unwraps[0];
            let charts = chart_of_each_vertex(unwrap.xref.len(), &unwrap.indices);
            let mut distinct = charts.clone();
            distinct.sort_unstable();
            distinct.dedup();
            assert!(
                distinct.len() > 1,
                "{name}: a curved mesh has more than one chart"
            );
            let mesh = meshlets_for_unwrap(
                SourceAttributes {
                    positions: &p,
                    normals: &n,
                    uvs: &t,
                },
                unwrap,
            )
            .expect("clusters");
            mesh.validate().expect("valid");
            assert_eq!(mesh.source_vertex_count as usize, unwrap.xref.len());
            assert_eq!(mesh.source_index_count as usize, unwrap.indices.len());
            assert!(mesh.level_count() >= 2, "{name}: {} levels", mesh.level_count());
            assert_eq!(cluster_triangles_across_charts(&mesh, &charts), 0, "{name}");
        }
    }

    /// Connected sets join all three corners of a triangle whatever order their roots come in.
    #[test]
    fn charts_are_the_connected_sets_of_the_triangle_list() {
        // Roots 5, 3 and 1 in one triangle: joining the first two must not drop the second.
        let charts = chart_of_each_vertex(7, &[1, 0, 1, 3, 2, 3, 5, 4, 5, 5, 3, 1]);
        assert_eq!(charts, [0, 0, 0, 0, 0, 0, 6], "{charts:?}");
    }

    /// The check above can fail: one corner moved to another chart is counted.
    #[test]
    fn a_triangle_across_two_charts_is_counted() {
        let (p, n, t, i) = bumpy_sphere(24, 48);
        let (_, layout) = layouts(&p, &i).into_iter().next().expect("a layout");
        let unwrap = &layout.unwraps[0];
        let charts = chart_of_each_vertex(unwrap.xref.len(), &unwrap.indices);
        let mut mesh = meshlets_for_unwrap(
            SourceAttributes {
                positions: &p,
                normals: &n,
                uvs: &t,
            },
            unwrap,
        )
        .expect("clusters");
        assert_eq!(cluster_triangles_across_charts(&mesh, &charts), 0);
        let first = mesh.cluster_indices(0)[0];
        let other = (0..charts.len())
            .find(|&v| charts[v] != charts[first[0] as usize])
            .expect("a second chart") as u32;
        let local = mesh.meshlets[0].vertex_offset as usize;
        let corner = (0..mesh.meshlets[0].vertex_count as usize)
            .find(|&k| mesh.vertices[local + k] == first[1])
            .expect("the corner");
        mesh.vertices[local + corner] = other;
        assert!(cluster_triangles_across_charts(&mesh, &charts) >= 1);
    }

    /// A seam vertex is drawn from the source vertex it copies, the same input builds the same
    /// bytes, missing attributes still build, and a bad `xref` is refused.
    #[test]
    fn clusters_copy_through_xref_and_are_deterministic() {
        let (p, n, t, i) = bumpy_sphere(24, 48);
        let (_, layout) = layouts(&p, &i).into_iter().next().expect("a layout");
        let unwrap = &layout.unwraps[0];
        assert!(unwrap.xref.len() > p.len(), "the seams split vertices");
        let source = SourceAttributes {
            positions: &p,
            normals: &n,
            uvs: &t,
        };
        let a = meshlets_for_unwrap(source, unwrap).expect("clusters");
        let b = meshlets_for_unwrap(source, unwrap).expect("clusters");
        assert_eq!(a.to_bytes(), b.to_bytes());
        let bounds = a.bounds;
        let radius = bounds[3];
        assert!(
            radius > 0.9 && radius < 1.2,
            "object space, as the source: {bounds:?}"
        );

        let bare = SourceAttributes {
            normals: &[],
            uvs: &[],
            ..source
        };
        meshlets_for_unwrap(bare, unwrap).expect("position only");

        let mut broken = unwrap.clone();
        broken.xref[0] = p.len() as u32;
        assert!(matches!(
            meshlets_for_unwrap(source, &broken),
            Err(MeshletBuildError::IndexOutOfRange { .. })
        ));
    }
}
