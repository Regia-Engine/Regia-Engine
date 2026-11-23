// SPDX-License-Identifier: MPL-2.0
//! Cluster hierarchy cooker for the visibility-buffer path (`docs/specs/27` § 4.2 stage 3).
//!
//! Level 0 splits the mesh into clusters (`meshopt::build_meshlets`, 64 vertices / 124
//! triangles). Each further level partitions the current clusters into groups of about
//! [`MeshletSettings::group_size`] that share vertices (`meshopt::partition_clusters`), merges
//! each group, simplifies it to half its triangles with the group border locked (so neighbouring
//! groups keep matching edges), and splits the result into new clusters. A group's clusters get
//! the group's sphere and accumulated error as their `parent`; the new clusters get the same pair
//! as their own `lod`. That is the batched multi-triangulation DAG of Cignoni et al. (IEEE
//! Visualization 2005), built the way meshoptimizer's public `clusterlod` example builds it
//! (MIT, <https://github.com/zeux/meshoptimizer>). Groups that do not simplify stay roots.
//!
//! The output indexes the caller's vertex buffer: coarse clusters reuse source vertices.

use meshopt::{
    PositionDataAdapter, RadiusDataAdapter, SimplifyOptions, VertexDataAdapter, build_meshlets,
    compute_cluster_bounds, compute_sphere_bounds, generate_position_remap,
    partition_clusters_with_positions, simplify, simplify_with_attributes_and_locks, typed_to_bytes,
};
use regia_render::visbuffer::{
    MAX_MESHLET_TRIANGLES, MAX_MESHLET_VERTICES, Meshlet, MeshletMesh, ROOT_ERROR, sphere_of_points,
};
use thiserror::Error;

/// Cooker knobs. The defaults are what the runtime path is tuned for.
#[derive(Clone, Debug)]
pub struct MeshletSettings {
    /// Clusters per simplification group (meshoptimizer's example uses 8).
    pub group_size: usize,
    /// A group whose simplification keeps more than this fraction of its triangles stops.
    pub min_reduction: f32,
    /// Normal-cone weight passed to the cluster builder (0 = ignore cones).
    pub cone_weight: f32,
    /// Most simplification levels.
    pub max_levels: u32,
    /// Weight of UV deviation, as a multiple of the mesh's own world length per UV unit
    /// (√(Σ world area / Σ UV area)): 1 makes a UV shift cost what the same shift of the surface
    /// would. Used only with [`MeshletAttributes`].
    pub uv_weight: f32,
    /// Weight of normal deviation (unit vectors), as a fraction of the mesh's bounding diameter.
    /// Used only with [`MeshletAttributes`].
    pub normal_weight: f32,
}

/// Vertex attributes the simplifier keeps faithful, parallel to the positions. Without them a
/// flat textured surface has zero geometric error at every level and its coarse clusters drift
/// the texture.
#[derive(Clone, Copy, Debug)]
pub struct MeshletAttributes<'a> {
    /// Unit normals.
    pub normals: &'a [[f32; 3]],
    /// Texture coordinates.
    pub uvs: &'a [[f32; 2]],
}

impl Default for MeshletSettings {
    fn default() -> Self {
        Self {
            group_size: 8,
            min_reduction: 0.85,
            cone_weight: 0.25,
            max_levels: 24,
            uv_weight: 1.0,
            normal_weight: 0.05,
        }
    }
}

/// The source mesh cannot be clustered.
#[derive(Debug, Error, PartialEq)]
pub enum MeshletBuildError {
    /// No triangles.
    #[error("mesh has no triangles")]
    Empty,
    /// The index count is not a multiple of three.
    #[error("index count {0} is not a multiple of 3")]
    NotTriangles(usize),
    /// An index points past the vertex buffer.
    #[error("index {index} >= vertex count {vertices}")]
    IndexOutOfRange {
        /// Offending index.
        index: u32,
        /// Vertex count.
        vertices: usize,
    },
    /// Attribute arrays are not as long as the positions.
    #[error("attribute arrays do not match the {0} positions")]
    AttributeCount(usize),
    /// The cooked hierarchy failed its own validation (a cooker bug).
    #[error("built hierarchy is invalid: {0}")]
    Invalid(String),
}

struct Cluster {
    /// Source-vertex triangle list.
    indices: Vec<u32>,
    /// Cluster-local vertex list (source indices) and packed local triangles.
    local_vertices: Vec<u32>,
    local_triangles: Vec<u32>,
    bounds: [f32; 4],
    cone: [f32; 4],
    lod: [f32; 4],
    lod_error: f32,
    parent: Option<([f32; 4], f32)>,
    level: u32,
}

fn split(
    indices: &[u32],
    adapter: &VertexDataAdapter<'_>,
    settings: &MeshletSettings,
    lod: [f32; 4],
    lod_error: f32,
    level: u32,
) -> Vec<Cluster> {
    let meshlets = build_meshlets(
        indices,
        adapter,
        MAX_MESHLET_VERTICES,
        MAX_MESHLET_TRIANGLES,
        settings.cone_weight,
    );
    meshlets
        .iter()
        .filter(|m| !m.triangles.is_empty())
        .map(|m| {
            let local_triangles: Vec<u32> = m
                .triangles
                .as_chunks::<3>()
                .0
                .iter()
                .map(|t| u32::from(t[0]) | u32::from(t[1]) << 8 | u32::from(t[2]) << 16)
                .collect();
            let indices: Vec<u32> = m.triangles.iter().map(|&l| m.vertices[l as usize]).collect();
            let b = compute_cluster_bounds(&indices, adapter);
            // Level 0: the culling sphere is also the (zero-error) LOD sphere.
            let bounds = [b.center[0], b.center[1], b.center[2], b.radius];
            Cluster {
                indices,
                local_vertices: m.vertices.to_vec(),
                local_triangles,
                bounds,
                cone: [b.cone_axis[0], b.cone_axis[1], b.cone_axis[2], b.cone_cutoff],
                lod: if level == 0 { bounds } else { lod },
                lod_error,
                parent: None,
                level,
            }
        })
        .collect()
}

/// Sphere enclosing the `lod` spheres of `members` (the group's simplification sphere).
fn group_sphere(clusters: &[Cluster], members: &[usize]) -> [f32; 4] {
    let spheres: Vec<[f32; 4]> = members.iter().map(|&c| clusters[c].lod).collect();
    let bytes = typed_to_bytes(&spheres);
    let s = compute_sphere_bounds(
        PositionDataAdapter {
            data: bytes,
            position_count: spheres.len(),
            position_stride: 16,
            position_offset: 0,
        },
        Some(RadiusDataAdapter {
            data: bytes,
            radius_count: spheres.len(),
            radius_stride: 16,
            radius_offset: 12,
        }),
    );
    // meshopt's sphere is approximate; grow it so it encloses every member exactly, which
    // keeps the parent test monotonic (a parent sphere never lies inside a child's).
    let mut radius = s.radius;
    for m in &spheres {
        let d = ((m[0] - s.center[0]).powi(2) + (m[1] - s.center[1]).powi(2) + (m[2] - s.center[2]).powi(2))
            .sqrt();
        radius = radius.max(d + m[3]);
    }
    [s.center[0], s.center[1], s.center[2], radius]
}

/// Builds the cluster hierarchy of a triangle mesh.
///
/// # Errors
///
/// An empty or malformed index buffer; [`MeshletBuildError::Invalid`] if the result fails
/// [`MeshletMesh::validate`].
#[tracing::instrument(skip_all, fields(triangles = indices.len() / 3))]
pub fn build_meshlet_mesh(
    positions: &[[f32; 3]],
    indices: &[u32],
    settings: &MeshletSettings,
) -> Result<MeshletMesh, MeshletBuildError> {
    build_meshlet_mesh_with_attributes(positions, None, indices, settings)
}

/// World length per UV unit of a mesh: √(Σ world area / Σ UV area); 1 when the UVs are degenerate.
pub(crate) fn world_per_uv(positions: &[[f32; 3]], uvs: &[[f32; 2]], indices: &[u32]) -> f32 {
    let (mut world, mut uv) = (0.0f64, 0.0f64);
    for t in indices.as_chunks::<3>().0 {
        let [a, b, c] = t.map(|i| i as usize);
        let e1 = [
            positions[b][0] - positions[a][0],
            positions[b][1] - positions[a][1],
            positions[b][2] - positions[a][2],
        ];
        let e2 = [
            positions[c][0] - positions[a][0],
            positions[c][1] - positions[a][1],
            positions[c][2] - positions[a][2],
        ];
        let x = [
            e1[1] * e2[2] - e1[2] * e2[1],
            e1[2] * e2[0] - e1[0] * e2[2],
            e1[0] * e2[1] - e1[1] * e2[0],
        ];
        world += 0.5 * f64::from((x[0] * x[0] + x[1] * x[1] + x[2] * x[2]).sqrt());
        let (u1, u2) = (
            [uvs[b][0] - uvs[a][0], uvs[b][1] - uvs[a][1]],
            [uvs[c][0] - uvs[a][0], uvs[c][1] - uvs[a][1]],
        );
        uv += 0.5 * f64::from((u1[0] * u2[1] - u1[1] * u2[0]).abs());
    }
    if uv <= 1e-12 || world <= 0.0 {
        return 1.0;
    }
    (world / uv).sqrt() as f32
}

/// [`build_meshlet_mesh`] with normals and UVs in the simplification error (meshoptimizer's
/// attribute-aware quadrics), so coarse clusters keep shading and texture placement.
///
/// # Errors
///
/// As [`build_meshlet_mesh`]; [`MeshletBuildError::AttributeCount`] when an attribute array is
/// not as long as `positions`.
#[tracing::instrument(skip_all, fields(triangles = indices.len() / 3))]
pub fn build_meshlet_mesh_with_attributes(
    positions: &[[f32; 3]],
    attributes: Option<MeshletAttributes<'_>>,
    indices: &[u32],
    settings: &MeshletSettings,
) -> Result<MeshletMesh, MeshletBuildError> {
    if indices.is_empty() {
        return Err(MeshletBuildError::Empty);
    }
    if !indices.len().is_multiple_of(3) {
        return Err(MeshletBuildError::NotTriangles(indices.len()));
    }
    if let Some(&index) = indices.iter().find(|&&i| i as usize >= positions.len()) {
        return Err(MeshletBuildError::IndexOutOfRange {
            index,
            vertices: positions.len(),
        });
    }
    let adapter = VertexDataAdapter::new(typed_to_bytes(positions), 12, 0)
        .map_err(|e| MeshletBuildError::Invalid(e.to_string()))?;
    let remap = generate_position_remap(&adapter);
    // Attribute stream (normal xyz, uv) and its weights in world units, built once.
    let mut packed = Vec::new();
    let mut weights = [0.0f32; 5];
    if let Some(attr) = attributes {
        if attr.normals.len() != positions.len() || attr.uvs.len() != positions.len() {
            return Err(MeshletBuildError::AttributeCount(positions.len()));
        }
        let diameter = 2.0 * sphere_of_points(positions.iter().copied())[3];
        let nw = settings.normal_weight * diameter;
        let uw = settings.uv_weight * world_per_uv(positions, attr.uvs, indices);
        weights = [nw, nw, nw, uw, uw];
        packed.reserve(positions.len() * 5);
        for (n, t) in attr.normals.iter().zip(attr.uvs) {
            packed.extend_from_slice(&[n[0], n[1], n[2], t[0], t[1]]);
        }
    }
    let locks = if packed.is_empty() {
        Vec::new()
    } else {
        vec![false; positions.len()]
    };

    let mut clusters = split(indices, &adapter, settings, [0.0; 4], 0.0, 0);
    let mut current: Vec<usize> = (0..clusters.len()).collect();
    for level in 1..=settings.max_levels {
        if current.len() <= 1 {
            break;
        }
        // Groups of clusters that share (position-welded) vertices.
        let mut cluster_vertices = Vec::new();
        let mut counts = Vec::with_capacity(current.len());
        for &c in &current {
            let mut unique: Vec<u32> = clusters[c].indices.iter().map(|&v| remap[v as usize]).collect();
            unique.sort_unstable();
            unique.dedup();
            counts.push(unique.len() as u32);
            cluster_vertices.extend(unique);
        }
        let mut partition = vec![0u32; current.len()];
        let group_count = partition_clusters_with_positions(
            &mut partition,
            &cluster_vertices,
            &counts,
            &adapter,
            settings.group_size,
        );
        let mut groups: Vec<Vec<usize>> = vec![Vec::new(); group_count];
        for (slot, &g) in partition.iter().enumerate() {
            if let Some(group) = groups.get_mut(g as usize) {
                group.push(current[slot]);
            }
        }

        let mut next = Vec::new();
        let mut simplified_any = false;
        for members in groups.into_iter().filter(|g| !g.is_empty()) {
            let merged: Vec<u32> = members
                .iter()
                .flat_map(|&c| clusters[c].indices.iter().copied())
                .collect();
            let target = (merged.len() / 3 / 2) * 3;
            let mut error = 0.0f32;
            let options =
                SimplifyOptions::LockBorder | SimplifyOptions::Sparse | SimplifyOptions::ErrorAbsolute;
            let simplified = if packed.is_empty() {
                simplify(&merged, &adapter, target, f32::MAX, options, Some(&mut error))
            } else {
                simplify_with_attributes_and_locks(
                    &merged,
                    &adapter,
                    &packed,
                    &weights,
                    5 * 4,
                    &locks,
                    target,
                    f32::MAX,
                    options,
                    Some(&mut error),
                )
            };
            if simplified.is_empty() || simplified.len() as f32 > merged.len() as f32 * settings.min_reduction
            {
                // Stuck: keep the clusters for the next level, where other neighbours may help.
                next.extend(members);
                continue;
            }
            simplified_any = true;
            let sphere = group_sphere(&clusters, &members);
            let child_error = members
                .iter()
                .map(|&c| clusters[c].lod_error)
                .fold(0.0f32, f32::max);
            let group_error = child_error + error;
            for &c in &members {
                clusters[c].parent = Some((sphere, group_error));
            }
            let start = clusters.len();
            clusters.extend(split(&simplified, &adapter, settings, sphere, group_error, level));
            next.extend(start..clusters.len());
        }
        if !simplified_any {
            break;
        }
        current = next;
    }

    let mut mesh = MeshletMesh {
        source_vertex_count: positions.len() as u32,
        source_index_count: indices.len() as u32,
        bounds: sphere_of_points(positions.iter().copied()),
        ..MeshletMesh::default()
    };
    for c in &clusters {
        let (parent, parent_error) = c.parent.unwrap_or((c.lod, ROOT_ERROR));
        mesh.meshlets.push(Meshlet {
            bounds: c.bounds,
            cone: c.cone,
            lod: c.lod,
            parent,
            lod_error: c.lod_error,
            parent_error,
            vertex_offset: mesh.vertices.len() as u32,
            triangle_offset: mesh.triangles.len() as u32,
            vertex_count: c.local_vertices.len() as u32,
            triangle_count: c.local_triangles.len() as u32,
            level: c.level,
            pad: 0,
        });
        mesh.vertices.extend_from_slice(&c.local_vertices);
        mesh.triangles.extend_from_slice(&c.local_triangles);
    }
    mesh.validate()
        .map_err(|e| MeshletBuildError::Invalid(e.to_string()))?;
    Ok(mesh)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::visbuffer::LodView;

    /// A bumpy UV sphere (closed, with a UV seam), `rings` × `segments` quads.
    fn bumpy_sphere(rings: u32, segments: u32) -> (Vec<[f32; 3]>, Vec<u32>) {
        let mut p = Vec::new();
        for r in 0..=rings {
            let v = r as f32 / rings as f32 * std::f32::consts::PI;
            for s in 0..=segments {
                let u = s as f32 / segments as f32 * std::f32::consts::TAU;
                let bump = 1.0 + 0.05 * (5.0 * u).sin() * (7.0 * v).sin();
                p.push([bump * v.sin() * u.cos(), bump * v.cos(), bump * v.sin() * u.sin()]);
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
        (p, i)
    }

    fn area(p: &[[f32; 3]], t: [u32; 3]) -> f32 {
        let a = p[t[0] as usize];
        let b = p[t[1] as usize];
        let c = p[t[2] as usize];
        let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let v = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
        let x = [
            u[1] * v[2] - u[2] * v[1],
            u[2] * v[0] - u[0] * v[2],
            u[0] * v[1] - u[1] * v[0],
        ];
        0.5 * (x[0] * x[0] + x[1] * x[1] + x[2] * x[2]).sqrt()
    }

    #[test]
    fn rejects_malformed_input() {
        let s = MeshletSettings::default();
        assert_eq!(build_meshlet_mesh(&[], &[], &s), Err(MeshletBuildError::Empty));
        assert_eq!(
            build_meshlet_mesh(&[[0.0; 3]; 3], &[0, 1], &s),
            Err(MeshletBuildError::NotTriangles(2))
        );
        assert!(matches!(
            build_meshlet_mesh(&[[0.0; 3]; 3], &[0, 1, 7], &s),
            Err(MeshletBuildError::IndexOutOfRange { index: 7, .. })
        ));
    }

    #[test]
    fn builds_a_dag_whose_levels_shrink_to_few_roots() {
        let (p, i) = bumpy_sphere(96, 192);
        let mesh = build_meshlet_mesh(&p, &i, &MeshletSettings::default()).expect("build");
        mesh.validate().expect("valid");
        assert_eq!(mesh.base_triangle_count(), i.len() / 3);
        assert!(mesh.level_count() >= 4, "levels {}", mesh.level_count());
        let roots = mesh
            .meshlets
            .iter()
            .filter(|m| m.parent_error >= ROOT_ERROR)
            .count();
        let level0 = mesh.meshlets.iter().filter(|m| m.level == 0).count();
        assert!(roots * 8 < level0, "roots {roots} of {level0} level-0 clusters");
        // Errors grow monotonically up the DAG and parent spheres enclose own spheres.
        for m in &mesh.meshlets {
            assert!(m.parent_error >= m.lod_error);
            if m.parent_error < ROOT_ERROR {
                let d = ((m.parent[0] - m.lod[0]).powi(2)
                    + (m.parent[1] - m.lod[1]).powi(2)
                    + (m.parent[2] - m.lod[2]).powi(2))
                .sqrt();
                assert!(d + m.lod[3] <= m.parent[3] * 1.0001 + 1e-5);
            }
        }
    }

    /// An open bumpy heightfield grid, `n` × `n` quads, no duplicated positions.
    fn bumpy_grid(n: u32) -> (Vec<[f32; 3]>, Vec<u32>) {
        let mut p = Vec::new();
        for z in 0..=n {
            for x in 0..=n {
                let (fx, fz) = (x as f32 * 0.1, z as f32 * 0.1);
                p.push([
                    fx,
                    0.3 * (fx * 0.7).sin() * (fz * 0.5).cos() + 0.05 * (fx * 3.1 + fz * 2.3).sin(),
                    fz,
                ]);
            }
        }
        let mut i = Vec::new();
        for z in 0..n {
            for x in 0..n {
                let a = z * (n + 1) + x;
                i.extend_from_slice(&[a, a + n + 1, a + 1, a + 1, a + n + 1, a + n + 2]);
            }
        }
        (p, i)
    }

    fn edge_counts(tris: &[[u32; 3]]) -> std::collections::HashMap<(u32, u32), u32> {
        let mut edges = std::collections::HashMap::new();
        for t in tris {
            for k in 0..3 {
                let (a, b) = (t[k], t[(k + 1) % 3]);
                *edges.entry((a.min(b), a.max(b))).or_insert(0) += 1;
            }
        }
        edges
    }

    #[test]
    fn every_cut_is_watertight() {
        let (p, i) = bumpy_grid(160);
        let mesh = build_meshlet_mesh(&p, &i, &MeshletSettings::default()).expect("build");
        let source: Vec<[u32; 3]> = i.as_chunks::<3>().0.to_vec();
        let mut border: Vec<(u32, u32)> = edge_counts(&source)
            .into_iter()
            .filter(|(_, n)| *n == 1)
            .map(|(e, _)| e)
            .collect();
        border.sort_unstable();
        let mut last_tris = usize::MAX;
        let mut coarse_seen = false;
        for distance in [1.0f32, 4.0, 16.0, 64.0, 256.0, 4096.0] {
            let view = LodView {
                eye: [8.0, distance, 8.0],
                pixels_per_unit: 1000.0,
                threshold_px: 1.0,
                near: 0.1,
            };
            let cut: Vec<usize> = mesh.lod_cut(&view).collect();
            coarse_seen |= cut.iter().any(|&c| mesh.meshlets[c].level > 0);
            let tris: Vec<[u32; 3]> = cut.iter().flat_map(|&c| mesh.cluster_indices(c)).collect();
            let edges = edge_counts(&tris);
            // Overlapping levels give an edge three or more uses; a hole leaves an inner edge
            // with one use. Only the mesh's own border may be single.
            assert!(
                edges.values().all(|&n| n <= 2),
                "distance {distance}: overlapping clusters"
            );
            let mut open: Vec<(u32, u32)> = edges
                .into_iter()
                .filter(|(_, n)| *n == 1)
                .map(|(e, _)| e)
                .collect();
            open.sort_unstable();
            assert_eq!(open, border, "distance {distance}: the cut has a crack");
            assert!(tris.len() <= last_tris, "farther views never draw more triangles");
            last_tris = tris.len();
        }
        assert!(coarse_seen, "far views select simplified clusters");
        assert!(
            last_tris * 8 < source.len(),
            "a far view draws a small fraction: {last_tris}"
        );
    }

    #[test]
    fn closed_mesh_cuts_keep_the_area() {
        let (p, i) = bumpy_sphere(64, 128);
        let mesh = build_meshlet_mesh(&p, &i, &MeshletSettings::default()).expect("build");
        let full: f32 = i.as_chunks::<3>().0.iter().map(|t| area(&p, *t)).sum();
        for (distance, tolerance) in [(2.0f32, 0.02f32), (30.0, 0.05), (500.0, 0.3)] {
            let view = LodView {
                eye: [0.0, 0.0, distance],
                pixels_per_unit: 1000.0,
                threshold_px: 1.0,
                near: 0.1,
            };
            let covered: f32 = mesh
                .lod_cut(&view)
                .flat_map(|c| mesh.cluster_indices(c))
                .map(|t| area(&p, t))
                .sum();
            // Overlapping levels would double the area.
            assert!(
                covered / full > 1.0 - tolerance && covered / full < 1.02,
                "distance {distance}: area {covered} vs {full}"
            );
        }
    }

    #[test]
    fn uv_distortion_counts_as_error_with_attributes() {
        // A flat grid whose UVs are warped: zero geometric error at every level, so without
        // attributes every coarse cluster claims error 0 and the texture would drift.
        let (mut p, i) = bumpy_grid(64);
        for v in &mut p {
            v[1] = 0.0;
        }
        let uvs: Vec<[f32; 2]> = p
            .iter()
            .map(|v| [v[0] + 0.3 * (v[2] * 2.0).sin(), v[2]])
            .collect();
        let normals = vec![[0.0, 1.0, 0.0]; p.len()];
        let settings = MeshletSettings::default();
        let plain = build_meshlet_mesh(&p, &i, &settings).expect("plain");
        let faithful = build_meshlet_mesh_with_attributes(
            &p,
            Some(MeshletAttributes {
                normals: &normals,
                uvs: &uvs,
            }),
            &i,
            &settings,
        )
        .expect("attributes");
        let max_error = |m: &MeshletMesh| m.meshlets.iter().map(|c| c.lod_error).fold(0.0f32, f32::max);
        // The warp spans 0.3 UV units over a 6.4 m grid; attribute-aware errors must see it.
        assert!(
            max_error(&faithful) > 10.0 * max_error(&plain),
            "UV drift is an error: {} with attributes vs {} without",
            max_error(&faithful),
            max_error(&plain)
        );
        assert!(matches!(
            build_meshlet_mesh_with_attributes(
                &p,
                Some(MeshletAttributes {
                    normals: &normals[1..],
                    uvs: &uvs
                }),
                &i,
                &settings
            ),
            Err(MeshletBuildError::AttributeCount(_))
        ));
    }

    #[test]
    fn cooked_blob_round_trips() {
        let (p, i) = bumpy_sphere(16, 32);
        let mesh = build_meshlet_mesh(&p, &i, &MeshletSettings::default()).expect("build");
        assert_eq!(MeshletMesh::from_bytes(&mesh.to_bytes()), Ok(mesh));
    }

    /// BLAKE3 of the cooked bytes of [`the_rock_cook_is_the_same_on_every_host`]'s rock. Measured
    /// on x86_64 (the build server, glibc) and required unchanged on aarch64 (Apple M5) and every other
    /// cook host (ADR-7878). Never re-pin to make one host pass: a different hash on one host means
    /// its cook depends on the host's ISA or compiler flags (`.cargo/config.toml` `[env]`).
    const ROCK_COOK_BLAKE3: &str = "0a7d272308c98472d4442d82e92fea74d9f2a62afdd3ce5d0ef74bf517cabf18";

    /// The visibility-buffer close tests' rock (a 96 × 48 UV sphere with normals and UVs, built
    /// with the deterministic `sin`/`cos`, so the input bits are the same everywhere) cooks to the
    /// same bytes on every host: meshoptimizer's C++ simplifier, clusterizer and partitioner are
    /// built without floating-point contraction, so aarch64 does not fuse `a * b + c` where
    /// x86_64 rounds twice.
    #[test]
    fn the_rock_cook_is_the_same_on_every_host() {
        let (vertices, indices) = regia_render::uv_sphere_deterministic(1.0, 96, 48);
        let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
        let normals: Vec<[f32; 3]> = vertices.iter().map(|v| v.normal).collect();
        let uvs: Vec<[f32; 2]> = vertices.iter().map(|v| v.uv).collect();
        let mesh = build_meshlet_mesh_with_attributes(
            &positions,
            Some(MeshletAttributes {
                normals: &normals,
                uvs: &uvs,
            }),
            &indices,
            &MeshletSettings::default(),
        )
        .expect("cook");
        let bytes = mesh.to_bytes();
        let hex = blake3::hash(&bytes).to_hex();
        eprintln!(
            "rock cook {}: {} bytes, {} levels, blake3 {hex}",
            std::env::consts::ARCH,
            bytes.len(),
            mesh.level_count()
        );
        assert_eq!(
            hex.as_str(),
            ROCK_COOK_BLAKE3,
            "the rock cooks to other bytes on this host ({}, {} bytes)",
            std::env::consts::ARCH,
            bytes.len()
        );
    }

    /// The workspace builds the cook's C++ without floating-point contraction and with astcenc's
    /// invariance on, for every cook and runtime target (ADR-7878): `cc` reads these per-target
    /// variables from `.cargo/config.toml`'s `[env]`. Without them the two pinned cook hashes
    /// above differ between x86_64 and aarch64 hosts.
    #[test]
    fn the_workspace_builds_the_cook_cpp_without_contraction() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../.cargo/config.toml");
        let config = std::fs::read_to_string(path).expect("workspace .cargo/config.toml");
        let gnu = "\"-ffp-contract=off -UASTCENC_NO_INVARIANCE\"";
        for target in [
            "x86_64_unknown_linux_gnu",
            "aarch64_unknown_linux_gnu",
            "aarch64_apple_darwin",
            "x86_64_apple_darwin",
            "aarch64_linux_android",
            "x86_64_linux_android",
            "x86_64_pc_windows_gnu",
            "wasm32_unknown_unknown",
        ] {
            let line = format!("CXXFLAGS_{target} = {gnu}");
            assert!(
                config.lines().any(|l| l.trim() == line),
                "`{line}` missing from the [env] table of {path}"
            );
        }
        let msvc = "CXXFLAGS_x86_64_pc_windows_msvc = \"/UASTCENC_NO_INVARIANCE\"";
        assert!(
            config.lines().any(|l| l.trim() == msvc),
            "`{msvc}` missing from {path}"
        );
        assert!(
            config.lines().any(|l| l.trim() == "[env]"),
            "no [env] table in {path}"
        );
    }
}
