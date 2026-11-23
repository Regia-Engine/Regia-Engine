// SPDX-License-Identifier: MPL-2.0
//! Mesh cooking with meshoptimizer (`docs/specs/12-assets.md` § 7.2).

use meshopt::{
    SimplifyOptions, VertexDataAdapter, optimize_overdraw_in_place, optimize_vertex_cache, simplify_decoder,
    simplify_scale_decoder, simplify_with_attributes_and_locks_decoder, typed_to_bytes,
};
use regia_asset::{
    COOKED_MESH_FORMAT_VERSION, CookedIndexWidth, CookedMesh, CookedMeshLod, CookedSubmesh, CpuMesh,
    CpuMorphTarget, CpuSkin, LOD_METRIC_ERROR, MeshValidationError, pack_colors_unorm16,
    thresholds_from_errors,
};
use tracing::instrument;

/// LOD generation settings from `.meta` importer settings.
#[derive(Clone, Debug)]
pub struct MeshLodSettings {
    /// Number of LOD levels including LOD0 (max 4).
    pub count: u32,
    /// Target triangle ratios per LOD (decreasing; a level not coarser than the previous one is skipped); defaults generated if empty.
    pub ratios: Vec<f32>,
}

/// Apply meshoptimizer vertex cache, overdraw, and fetch passes to a [`CpuMesh`].
///
/// Reference: meshoptimizer library (MIT), https://github.com/zeux/meshoptimizer
#[instrument(skip_all, fields(vertices = mesh.positions.len(), indices = mesh.indices.len()))]
pub fn cook_mesh(mesh: CpuMesh) -> CpuMesh {
    if mesh.positions.is_empty() || mesh.indices.len() < 3 {
        return mesh;
    }

    let vertex_count = mesh.positions.len();
    let mut indices = optimize_vertex_cache(&mesh.indices, vertex_count);
    if let Ok(adapter) = VertexDataAdapter::new(typed_to_bytes(&mesh.positions), 12, 0) {
        optimize_overdraw_in_place(&mut indices, &adapter, 1.05);
    }

    let (old_to_new, unique) = vertex_fetch_remap(&mut indices, vertex_count);
    let positions = scatter(&mesh.positions, &old_to_new, unique);
    let normals = scatter(&mesh.normals, &old_to_new, unique);
    let uvs = scatter(&mesh.uvs, &old_to_new, unique);
    let tangents = scatter(&mesh.tangents, &old_to_new, unique);

    let cooked = CpuMesh {
        positions,
        normals,
        uvs,
        tangents,
        indices,
        skin: mesh.skin.as_ref().map(|skin| CpuSkin {
            joint_names: skin.joint_names.clone(),
            joints: scatter(&skin.joints, &old_to_new, unique),
            weights: scatter(&skin.weights, &old_to_new, unique),
            joints1: scatter(&skin.joints1, &old_to_new, unique),
            weights1: scatter(&skin.weights1, &old_to_new, unique),
            eight_influences: skin.eight_influences,
        }),
        colors: scatter(&mesh.colors, &old_to_new, unique),
        morph_targets: mesh
            .morph_targets
            .iter()
            .map(|target| remap_morph_target(target, &old_to_new))
            .collect(),
    };
    if let Err(e) = cooked.validate() {
        tracing::warn!(target: "regia_asset_pipeline", "mesh cook validation failed, using input: {e}");
        mesh
    } else {
        cooked
    }
}

/// Vertex-fetch reorder: renumbers vertices in order of first use by `indices` (rewritten in
/// place) and returns the old → new table at full source length (`None` for vertices no index
/// references) with the unique count. Same order as meshoptimizer's `optimizeVertexFetchRemap`
/// (MIT); the `meshopt` 0.6 wrapper cuts its table to the unique count, so its
/// `remap_index_buffer`/`remap_vertex_buffer` read past the end when the mesh has unused vertices.
fn vertex_fetch_remap(indices: &mut [u32], vertex_count: usize) -> (Vec<Option<u32>>, usize) {
    let mut old_to_new: Vec<Option<u32>> = vec![None; vertex_count];
    let mut unique = 0u32;
    for index in indices.iter_mut() {
        let Some(slot) = old_to_new.get_mut(*index as usize) else {
            continue;
        };
        let new = *slot.get_or_insert_with(|| {
            unique += 1;
            unique - 1
        });
        *index = new;
    }
    (old_to_new, unique as usize)
}

/// Moves per-vertex values to their new slots (`old_to_new`); an empty input stays empty.
fn scatter<T: Copy + Default>(values: &[T], old_to_new: &[Option<u32>], unique: usize) -> Vec<T> {
    if values.is_empty() {
        return Vec::new();
    }
    let mut out = vec![T::default(); unique];
    for (old, value) in values.iter().enumerate() {
        if let Some(Some(new)) = old_to_new.get(old)
            && let Some(slot) = out.get_mut(*new as usize)
        {
            *slot = *value;
        }
    }
    out
}

/// Renumbers a sparse morph target's vertices, dropping vertices the cook removed and keeping
/// the index list strictly increasing.
fn remap_morph_target(target: &CpuMorphTarget, old_to_new: &[Option<u32>]) -> CpuMorphTarget {
    let mut entries: Vec<(u32, usize)> = target
        .vertex_indices
        .iter()
        .enumerate()
        .filter_map(|(i, &old)| {
            old_to_new
                .get(old as usize)
                .copied()
                .flatten()
                .map(|new| (new, i))
        })
        .collect();
    entries.sort_unstable_by_key(|(new, _)| *new);
    let pick = |deltas: &[[f32; 3]]| -> Vec<[f32; 3]> {
        if deltas.is_empty() {
            Vec::new()
        } else {
            entries.iter().map(|(_, i)| deltas[*i]).collect()
        }
    };
    CpuMorphTarget {
        name: target.name.clone(),
        vertex_indices: entries.iter().map(|(new, _)| *new).collect(),
        position_deltas: pick(&target.position_deltas),
        normal_deltas: pick(&target.normal_deltas),
        tangent_deltas: pick(&target.tangent_deltas),
    }
}

/// Pack a cooked CPU mesh into [`CookedMesh`] with optional meshopt LOD chain.
///
/// The skin, `COLOR_0` and morph targets of `mesh` are written as the v2 blocks
/// ([`COOKED_MESH_FORMAT_VERSION`]); a mesh without them packs exactly as v1 did, plus empty
/// blocks. `skin_influences` (v3) is 8 when the skin opted in (`CpuSkin::eight_influences`),
/// else 4.
#[instrument(skip(mesh, lod_settings))]
pub fn pack_cooked_mesh(
    mesh: &CpuMesh,
    lod_settings: Option<&MeshLodSettings>,
) -> Result<CookedMesh, MeshValidationError> {
    mesh.validate()?;
    let (aabb_min, aabb_max, sphere) = compute_bounds(mesh);
    let mut lod_indices: Vec<Vec<u32>> = Vec::new();
    lod_indices.push(mesh.indices.clone());
    // Absolute error of each coarser LOD (metres). LOD i's screen size is the size at which
    // LOD i+1's error is one pixel; the triangle ratio only chooses the target count.
    let mut errors_abs: Vec<f32> = Vec::new();
    let diameter = sphere[3].max(0.0) * 2.0;
    if let Some(lod) = lod_settings {
        let ratios = lod_ratios(lod);
        if ratios.first().is_some_and(|r| *r < 1.0) {
            tracing::warn!(
                first = ratios[0],
                "generate_lods: ratios[0] is LOD0, which is never simplified; the entry is ignored (start the list with 1.0)"
            );
        }
        let scale = if mesh.positions.is_empty() {
            0.0
        } else {
            simplify_scale_decoder(&mesh.positions)
        };
        // The same weights as the cluster cooker (`meshlet::MeshletSettings`): a normal error of
        // 1 counts as `normal_weight` of the bounding diameter, a UV error of 1 as `uv_weight`
        // times the mesh's world length per UV unit. A valid mesh always has both streams;
        // a length mismatch falls back to positions.
        let attributes = if mesh.normals.len() == mesh.positions.len()
            && mesh.uvs.len() == mesh.positions.len()
            && !mesh.positions.is_empty()
        {
            let weights = crate::meshlet::MeshletSettings::default();
            let normal_weight = weights.normal_weight * diameter;
            let uv_weight =
                weights.uv_weight * crate::meshlet::world_per_uv(&mesh.positions, &mesh.uvs, &mesh.indices);
            let mut packed = Vec::with_capacity(mesh.positions.len() * 5);
            for (normal, uv) in mesh.normals.iter().zip(&mesh.uvs) {
                packed.extend_from_slice(&[normal[0], normal[1], normal[2], uv[0], uv[1]]);
            }
            Some((
                packed,
                [normal_weight, normal_weight, normal_weight, uv_weight, uv_weight],
            ))
        } else {
            None
        };
        let locks = vec![false; mesh.positions.len()];
        let mut prev = mesh.indices.clone();
        for (i, ratio) in ratios.iter().enumerate().skip(1) {
            if i >= 4 {
                break;
            }
            let target = ((mesh.indices.len() as f32) * ratio).max(3.0) as usize;
            let mut relative_error = 0.0f32;
            let simplified = if let Some((packed, weights)) = &attributes {
                simplify_with_attributes_and_locks_decoder(
                    &prev,
                    &mesh.positions,
                    packed,
                    weights,
                    5 * 4,
                    &locks,
                    target,
                    0.01,
                    SimplifyOptions::LockBorder,
                    Some(&mut relative_error),
                )
            } else {
                simplify_decoder(
                    &prev,
                    &mesh.positions,
                    target,
                    0.01,
                    SimplifyOptions::LockBorder,
                    Some(&mut relative_error),
                )
            };
            if simplified.len() < prev.len() {
                // A degenerate extent has no measurable error: the coarse LOD is used almost always.
                let abs = if scale.is_finite() && scale > 0.0 && relative_error.is_finite() {
                    relative_error * scale
                } else {
                    0.0
                };
                prev = simplified;
                lod_indices.push(prev.clone());
                errors_abs.push(abs);
            }
        }
    }
    let screens = if errors_abs.is_empty() {
        vec![0.0]
    } else {
        thresholds_from_errors(&errors_abs, diameter)
    };

    let index_width = if mesh.positions.len() <= 65535 {
        CookedIndexWidth::U16
    } else {
        CookedIndexWidth::U32
    };

    let mut indices_bytes = Vec::new();
    let mut lods = Vec::new();
    let mut submeshes = Vec::new();
    for (idx_list, screen) in lod_indices.iter().zip(&screens) {
        let offset = indices_bytes.len() as u32;
        encode_indices(&mut indices_bytes, idx_list, index_width);
        let len = indices_bytes.len() as u32 - offset;
        lods.push(CookedMeshLod {
            screen_relative_size: *screen,
            index_byte_offset: offset,
            index_byte_length: len,
        });
    }
    if let Some(first) = lods.first() {
        submeshes.push(CookedSubmesh {
            index_byte_offset: first.index_byte_offset,
            index_byte_length: first.index_byte_length,
            material_slot: 0,
        });
    }

    let (stream_positions, stream_normals_tangents, stream_uv0) = pack_vertex_streams(mesh)?;
    let vertex_count = mesh.positions.len();
    let skin = mesh.skin.as_ref().map(|s| s.pack(vertex_count)).transpose()?;
    let morph_targets = mesh
        .morph_targets
        .iter()
        .map(|t| t.pack(vertex_count))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(CookedMesh {
        aabb_min,
        aabb_max,
        bounding_sphere: sphere,
        stream_positions,
        stream_normals_tangents,
        stream_uv0,
        indices: indices_bytes,
        index_width,
        vertex_count: mesh.positions.len() as u32,
        lods,
        submeshes,
        lod_metric: LOD_METRIC_ERROR,
        format_version: COOKED_MESH_FORMAT_VERSION,
        skin,
        stream_color0: pack_colors_unorm16(&mesh.colors),
        morph_targets,
        skin_influences: if mesh.skin.as_ref().is_some_and(|s| s.eight_influences) {
            regia_asset::EIGHT_SKIN_INFLUENCES
        } else {
            regia_asset::DEFAULT_SKIN_INFLUENCES
        },
    })
}

fn lod_ratios(lod: &MeshLodSettings) -> Vec<f32> {
    if !lod.ratios.is_empty() {
        return lod.ratios.clone();
    }
    let count = lod.count.clamp(1, 4) as usize;
    (0..count).map(|i| 1.0 / (1u32 << i) as f32).collect()
}

fn compute_bounds(mesh: &CpuMesh) -> ([f32; 3], [f32; 3], [f32; 4]) {
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for p in &mesh.positions {
        for i in 0..3 {
            min[i] = min[i].min(p[i]);
            max[i] = max[i].max(p[i]);
        }
    }
    let center = [
        (min[0] + max[0]) * 0.5,
        (min[1] + max[1]) * 0.5,
        (min[2] + max[2]) * 0.5,
    ];
    let mut radius = 0.0f32;
    for p in &mesh.positions {
        let d = ((p[0] - center[0]).powi(2) + (p[1] - center[1]).powi(2) + (p[2] - center[2]).powi(2)).sqrt();
        radius = radius.max(d);
    }
    (min, max, [center[0], center[1], center[2], radius])
}

type PackedVertexStreams = (Vec<u8>, Vec<u8>, Vec<u8>);

fn pack_vertex_streams(mesh: &CpuMesh) -> Result<PackedVertexStreams, MeshValidationError> {
    let vc = mesh.positions.len();
    let mut stream_positions = Vec::with_capacity(vc * 12);
    let mut stream_nt = Vec::with_capacity(vc * 8);
    let mut stream_uv = Vec::with_capacity(vc * 4);
    for i in 0..vc {
        let p = mesh.positions[i];
        for c in p {
            stream_positions.extend_from_slice(&c.to_le_bytes());
        }
        let n = mesh.normals.get(i).copied().unwrap_or([0.0, 1.0, 0.0]);
        let (ex, ey) = regia_asset::encode_octahedral_normal(n);
        let tangent_sign = mesh
            .tangents
            .get(i)
            .map(|t| if t[3] >= 0.0 { 1i16 } else { -1i16 })
            .unwrap_or(1);
        stream_nt.extend_from_slice(&ex.to_le_bytes());
        stream_nt.extend_from_slice(&ey.to_le_bytes());
        stream_nt.extend_from_slice(&0i16.to_le_bytes());
        stream_nt.extend_from_slice(&tangent_sign.to_le_bytes());
        let uv = mesh.uvs.get(i).copied().unwrap_or([0.0, 0.0]);
        stream_uv.extend_from_slice(&f32_to_f16(uv[0]).to_le_bytes());
        stream_uv.extend_from_slice(&f32_to_f16(uv[1]).to_le_bytes());
    }
    Ok((stream_positions, stream_nt, stream_uv))
}

fn encode_indices(out: &mut Vec<u8>, indices: &[u32], width: CookedIndexWidth) {
    match width {
        CookedIndexWidth::U16 => {
            for &idx in indices {
                out.extend_from_slice(&(idx as u16).to_le_bytes());
            }
        }
        CookedIndexWidth::U32 => {
            for idx in indices {
                out.extend_from_slice(&idx.to_le_bytes());
            }
        }
    }
}

fn f32_to_f16(v: f32) -> u16 {
    let bits = v.to_bits();
    let sign = (bits >> 16) & 0x8000;
    let val = (bits & 0x7fff_ffff) as i32;
    if val >= 0x477fe000 {
        return (sign | 0x7c00) as u16;
    }
    if val < 0x38800000 {
        return sign as u16;
    }
    let exp = ((val >> 23) & 0xff) - 127 + 15;
    let mant = (val >> 13) as u32 & 0x3ff;
    (sign | ((exp as u32) << 10) | mant) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::{lod_at, projected_screen_size};

    fn wavy_grid(n: u32, extent: f32) -> CpuMesh {
        let mut positions = Vec::new();
        let mut normals = Vec::new();
        let mut uvs = Vec::new();
        let mut indices = Vec::new();
        for y in 0..=n {
            for x in 0..=n {
                let fx = x as f32 / n as f32;
                let fy = y as f32 / n as f32;
                let h = (fx * 6.0).sin() * (fy * 5.0).cos() * extent * 0.15;
                positions.push([fx * extent, h, fy * extent]);
                normals.push([0.0, 1.0, 0.0]);
                uvs.push([fx, fy]);
            }
        }
        let row = n + 1;
        for y in 0..n {
            for x in 0..n {
                let i = y * row + x;
                indices.extend_from_slice(&[i, i + row, i + 1, i + 1, i + row, i + row + 1]);
            }
        }
        CpuMesh {
            positions,
            normals,
            uvs,
            tangents: Vec::new(),
            indices,
            ..Default::default()
        }
    }

    /// A strip of `vertex_count` vertices with two triangles that reach its last vertex.
    fn strip(vertex_count: u32) -> CpuMesh {
        let n = vertex_count as usize;
        CpuMesh {
            positions: (0..n).map(|i| [i as f32 * 0.001, (i % 2) as f32, 0.0]).collect(),
            normals: vec![[0.0, 0.0, 1.0]; n],
            uvs: vec![[0.0, 0.0]; n],
            tangents: Vec::new(),
            indices: vec![0, 1, 2, vertex_count - 3, vertex_count - 2, vertex_count - 1],
            ..Default::default()
        }
    }

    /// Spec 12 § 7.2: indices are `u16` when the mesh has at most 65535 vertices, else `u32`.
    #[test]
    fn index_width_is_u16_up_to_65535_vertices_and_u32_above() {
        let small = pack_cooked_mesh(&strip(65_535), None).expect("cook");
        assert_eq!(small.index_width, CookedIndexWidth::U16);
        assert_eq!(small.indices.len(), 6 * 2);
        let large = pack_cooked_mesh(&strip(65_536), None).expect("cook");
        assert_eq!(large.index_width, CookedIndexWidth::U32);
        assert_eq!(large.indices.len(), 6 * 4);
        // The last index survives the narrow encoding exactly.
        let last = u16::from_le_bytes([small.indices[10], small.indices[11]]);
        assert_eq!(last, 65_534);
    }

    /// Spec 12 § 7.2: at most 4 LODs (LOD0 included), whatever the settings ask for.
    #[test]
    fn a_mesh_cooks_at_most_four_lods() {
        let mesh = wavy_grid(48, 2.0);
        let by_ratios = MeshLodSettings {
            count: 8,
            ratios: vec![1.0, 0.6, 0.35, 0.2, 0.1, 0.05, 0.02],
        };
        let cooked = pack_cooked_mesh(&mesh, Some(&by_ratios)).expect("cook");
        assert!(cooked.lods.len() <= 4, "{} LODs", cooked.lods.len());
        assert!(cooked.lods.len() >= 2, "the grid simplifies");
        let by_count = MeshLodSettings {
            count: 9,
            ratios: vec![],
        };
        assert_eq!(lod_ratios(&by_count).len(), 4);
        let cooked = pack_cooked_mesh(&mesh, Some(&by_count)).expect("cook");
        assert!(cooked.lods.len() <= 4, "{} LODs", cooked.lods.len());
        // Every LOD is an index range of the one shared vertex buffer (index-only LODs).
        let width = match cooked.index_width {
            CookedIndexWidth::U16 => 2,
            CookedIndexWidth::U32 => 4,
        };
        let vertex_count = cooked.vertex_count as usize;
        for lod in &cooked.lods {
            let start = lod.index_byte_offset as usize;
            let end = start + lod.index_byte_length as usize;
            assert!(end <= cooked.indices.len());
            for chunk in cooked.indices[start..end].chunks(width) {
                let index = if width == 2 {
                    usize::from(u16::from_le_bytes([chunk[0], chunk[1]]))
                } else {
                    u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as usize
                };
                assert!(index < vertex_count);
            }
        }
    }

    /// Skin, colour and morph values derived from the source vertex number, so a cooked vertex
    /// can be checked against its source after meshoptimizer reorders the vertices.
    fn tag_vertex(source: usize, joint_count: usize) -> ([u16; 4], [u16; 4], [f32; 4]) {
        let j = |k: usize| ((source * 7 + k * 13) % joint_count) as u16;
        let w0 = (1000 + (source * 37) % 30000) as u16;
        let w1 = (500 + (source * 11) % 20000) as u16;
        let weights = [w0, w1, 65535 - w0 - w1 - 7, 7];
        let c = (source % 256) as f32 / 255.0;
        ([j(0), j(1), j(2), j(3)], weights, [c, 1.0 - c, 0.5, 1.0])
    }

    #[test]
    fn cook_keeps_skin_colour_and_morph_data_on_their_vertices() {
        for joint_count in [67usize, 82, 256, 300] {
            let mut mesh = wavy_grid(12, 2.0);
            // One vertex no triangle uses: the cook drops it and every v2 array with it.
            mesh.positions.push([9.0, 9.0, 9.0]);
            mesh.normals.push([0.0, 1.0, 0.0]);
            mesh.uvs.push([0.0, 0.0]);
            let n = mesh.positions.len();
            let tags: Vec<_> = (0..n).map(|i| tag_vertex(i, joint_count)).collect();
            mesh.skin = Some(CpuSkin {
                joint_names: (0..joint_count).map(|i| format!("joint_{i}")).collect(),
                joints: tags.iter().map(|t| t.0).collect(),
                weights: tags.iter().map(|t| t.1).collect(),
                joints1: Vec::new(),
                weights1: Vec::new(),
                eight_influences: false,
            });
            mesh.colors = tags.iter().map(|t| t.2).collect();
            let morph_sources: Vec<u32> = (0..n as u32).filter(|i| i.is_multiple_of(5)).collect();
            let delta = |i: u32| [i as f32 * 0.25, -(i as f32) * 0.125, 1.0];
            mesh.morph_targets = vec![CpuMorphTarget {
                name: "bulge".into(),
                vertex_indices: morph_sources.clone(),
                position_deltas: morph_sources.iter().map(|&i| delta(i)).collect(),
                normal_deltas: Vec::new(),
                tangent_deltas: Vec::new(),
            }];
            let source_by_position: std::collections::HashMap<[u32; 3], usize> = mesh
                .positions
                .iter()
                .enumerate()
                .map(|(i, p)| (p.map(f32::to_bits), i))
                .collect();

            let cooked = cook_mesh(mesh);
            assert_eq!(cooked.positions.len(), n - 1, "the unused vertex is dropped");
            let packed = pack_cooked_mesh(&cooked, None).expect("pack");
            let loaded = CookedMesh::from_postcard(&packed.to_postcard().expect("encode"))
                .expect("decode")
                .to_cpu_mesh()
                .expect("unpack");
            let skin = loaded.skin.as_ref().expect("skin survives");
            assert_eq!(skin.joint_names.len(), joint_count);
            let mut morphed = Vec::new();
            for (v, position) in loaded.positions.iter().enumerate() {
                let source = source_by_position[&position.map(f32::to_bits)];
                let (joints, weights, color) = tags[source];
                assert_eq!(skin.joints[v], joints, "{joint_count} joints, vertex {v}");
                assert_eq!(skin.weights[v], weights, "weights bit-exact");
                for (got, want) in loaded.colors[v].iter().zip(color) {
                    assert!((got - want).abs() <= 0.5 / 65535.0, "colour");
                }
                if source.is_multiple_of(5) {
                    morphed.push((v as u32, delta(source as u32)));
                }
            }
            let target = &loaded.morph_targets[0];
            assert_eq!(target.name, "bulge");
            assert_eq!(
                target.vertex_indices,
                morphed.iter().map(|m| m.0).collect::<Vec<_>>()
            );
            for (got, (_, want)) in target.position_deltas.iter().zip(&morphed) {
                for axis in 0..3 {
                    assert!(
                        (got[axis] - want[axis]).abs() <= want[axis].abs() / 1024.0,
                        "delta"
                    );
                }
            }
        }
    }

    #[test]
    fn vertex_fetch_remap_matches_meshopt_order_with_a_full_length_table() {
        // Every vertex used: same order and index buffer as meshoptimizer's own remap.
        let mesh = wavy_grid(6, 1.0);
        let n = mesh.positions.len();
        let indices = optimize_vertex_cache(&mesh.indices, n);
        let theirs = meshopt::optimize_vertex_fetch_remap(&indices, n);
        assert_eq!(theirs.len(), n);
        let mut ours_indices = indices.clone();
        let (table, unique) = vertex_fetch_remap(&mut ours_indices, n);
        assert_eq!(unique, n);
        assert_eq!(table, theirs.iter().map(|&v| Some(v)).collect::<Vec<_>>());
        assert_eq!(
            ours_indices,
            indices.iter().map(|&i| theirs[i as usize]).collect::<Vec<_>>()
        );

        // Unused vertices at the front: old ids run past the unique count, the table keeps the
        // full source length, and unused vertices map to `None`.
        let mut indices = vec![7u32, 5, 6, 6, 5, 4];
        let (table, unique) = vertex_fetch_remap(&mut indices, 8);
        assert_eq!(unique, 4);
        assert_eq!(table.len(), 8);
        assert_eq!(indices, vec![0, 1, 2, 2, 1, 3]);
        assert_eq!(&table[..4], &[None; 4]);
        assert_eq!(&table[4..], &[Some(3), Some(1), Some(2), Some(0)]);
    }

    #[test]
    fn cook_keeps_attributes_when_most_vertices_are_unused() {
        let mut mesh = wavy_grid(4, 1.0);
        let used = mesh.positions.len() as u32;
        // 50 orphan vertices ahead of the used ones: every source id is past the unique count.
        let pad = 50u32;
        let orphan = |i: u32| [100.0 + i as f32, 0.0, 0.0];
        mesh.positions.splice(0..0, (0..pad).map(orphan));
        mesh.normals.splice(0..0, (0..pad).map(|_| [1.0, 0.0, 0.0]));
        mesh.uvs.splice(0..0, (0..pad).map(|_| [0.5, 0.5]));
        for index in &mut mesh.indices {
            *index += pad;
        }
        let source = mesh.clone();
        let cooked = cook_mesh(mesh);
        assert_eq!(cooked.positions.len(), used as usize);
        let by_position: std::collections::HashMap<[u32; 3], usize> = source
            .positions
            .iter()
            .enumerate()
            .map(|(i, p)| (p.map(f32::to_bits), i))
            .collect();
        for (v, p) in cooked.positions.iter().enumerate() {
            let s = by_position[&p.map(f32::to_bits)];
            assert!(s >= pad as usize, "no orphan survives");
            assert_eq!(cooked.uvs[v], source.uvs[s], "uv moves with its position");
            assert_eq!(cooked.normals[v], source.normals[s]);
        }
        // Same triangles and winding, renumbered (corners rotated to a canonical start).
        let corners = |m: &CpuMesh| -> std::collections::BTreeSet<[[u32; 3]; 3]> {
            m.indices
                .chunks(3)
                .map(|t| {
                    let mut tri: [[u32; 3]; 3] =
                        std::array::from_fn(|k| m.positions[t[k] as usize].map(f32::to_bits));
                    let first = (0..3).min_by_key(|&k| tri[k]).unwrap_or(0);
                    tri.rotate_left(first);
                    tri
                })
                .collect()
        };
        assert_eq!(corners(&cooked), corners(&source));
    }

    #[test]
    fn rigid_mesh_packs_the_v1_streams_unchanged() {
        let mesh = cook_mesh(wavy_grid(8, 1.0));
        let packed = pack_cooked_mesh(&mesh, None).expect("pack");
        assert!(packed.skin.is_none() && packed.stream_color0.is_empty() && packed.morph_targets.is_empty());
        // The current payload is the v1 payload plus the version, three empty v2 blocks and the
        // v3 `skin_influences` byte (4 by default, ADR-1622).
        let v1 = packed.to_postcard_v1().expect("v1");
        let v3 = packed.to_postcard().expect("v3");
        assert_eq!(&v3[..v1.len()], v1.as_slice());
        assert_eq!(
            &v3[v1.len()..],
            &[
                COOKED_MESH_FORMAT_VERSION as u8,
                0,
                0,
                0,
                regia_asset::DEFAULT_SKIN_INFLUENCES
            ]
        );
    }

    #[test]
    fn generated_lod_screen_size_comes_from_the_error_not_the_ratio() {
        let mesh = wavy_grid(32, 2.0);
        let settings = MeshLodSettings {
            count: 4,
            ratios: vec![],
        };
        let cooked = pack_cooked_mesh(&mesh, Some(&settings)).expect("cook");
        assert!(cooked.lods.len() >= 2, "the grid must simplify");
        assert_eq!(cooked.lod_metric, LOD_METRIC_ERROR);
        assert_eq!(cooked.lods.last().expect("last").screen_relative_size, 0.0);
        let ratios = lod_ratios(&settings);
        for (lod, ratio) in cooked.lods.iter().zip(&ratios) {
            assert!(lod.screen_relative_size.is_finite());
            assert!(
                (lod.screen_relative_size - ratio).abs() > 1.0e-3,
                "LOD screen {:.4} must not be the triangle ratio {ratio}",
                lod.screen_relative_size
            );
        }
        for pair in cooked.lods.windows(2) {
            assert!(pair[0].screen_relative_size > pair[1].screen_relative_size);
        }
        let bytes = cooked.to_postcard().expect("postcard");
        let loaded = CookedMesh::from_postcard(&bytes).expect("round trip");
        assert_eq!(loaded.lod_metric, cooked.lod_metric);
        assert_eq!(
            loaded.lods[0].screen_relative_size,
            cooked.lods[0].screen_relative_size
        );
    }

    /// Lookdev-like distances (road is 16 m, shots use fov_y ≈ 0.9) for a 2 m prop.
    #[test]
    fn lookdev_distances_drop_triangles() {
        let mesh = wavy_grid(24, 2.0);
        let lod0_tris = mesh.indices.len() / 3;
        let settings = MeshLodSettings {
            count: 4,
            ratios: vec![],
        };
        let cooked = pack_cooked_mesh(&mesh, Some(&settings)).expect("cook");
        let radius = cooked.bounding_sphere[3];
        let fov = 0.9f32;
        let thresholds: Vec<f32> = cooked.lods.iter().map(|l| l.screen_relative_size).collect();
        let width = match cooked.index_width {
            CookedIndexWidth::U16 => 2,
            CookedIndexWidth::U32 => 4,
        };
        let mut report = String::new();
        let mut far_tris = lod0_tris;
        let mut close_tris = lod0_tris;
        for (name, dist) in [
            ("close 8 m", 8.0),
            ("mid 40 m", 40.0),
            ("far 120 m", 120.0),
            ("vista 400 m", 400.0),
        ] {
            let size = projected_screen_size(radius, dist, fov);
            let lod = lod_at(&thresholds, size);
            let tris = cooked.lods[lod].index_byte_length as usize / width / 3;
            let shadow = lod_at(&thresholds, size * 0.5);
            let color_draws = 1;
            let shadow_draws = 1;
            report.push_str(&format!(
                "{name}: lod {lod} (shadow {shadow}), {tris} tris ({:.0}% of LOD0), color draws {color_draws}, shadow draws {shadow_draws}\n",
                100.0 * tris as f32 / lod0_tris as f32
            ));
            if dist == 8.0 {
                close_tris = tris;
            }
            if dist == 400.0 {
                far_tris = tris;
            }
        }
        eprintln!(
            "LOD savings, 2 m prop, fov 0.9, 1080p, LOD0 {lod0_tris} tris, {} levels:\n{report}",
            cooked.lods.len()
        );
        assert!(
            far_tris < close_tris,
            "vista must draw fewer triangles than 8 m:\n{report}"
        );
        assert!(far_tris < lod0_tris);
    }

    /// A flat grid whose UVs are constant inside 3-vertex blocks and jump on the block edges.
    /// Positions-only simplification sees no error and hits the triangle target. Collapses
    /// inside a block are free, so a coarse LOD is stored. A collapse across a jump stays
    /// above the 0.01 relative error (normals + UV0, the cluster cooker's weights, borders
    /// locked), so that LOD keeps more indices than the positions-only result.
    ///
    /// Shared vertices: the jump is not a geometric border, and LockBorder does not pin it.
    /// A one-direction wave leaves the other axis free to collapse, and both paths then hit
    /// the same target.
    #[test]
    fn generated_lods_keep_an_interior_uv_warp() {
        let n = 12u32;
        let block = 3u32;
        let mut positions = Vec::new();
        let mut normals = Vec::new();
        let mut uvs = Vec::new();
        let mut indices = Vec::new();
        for y in 0..=n {
            for x in 0..=n {
                let fx = x as f32 / n as f32;
                let fy = y as f32 / n as f32;
                positions.push([fx, 0.0, fy]);
                normals.push([0.0, 1.0, 0.0]);
                let bx = x / block;
                let by = y / block;
                uvs.push([((bx + by) & 1) as f32, (by & 1) as f32]);
            }
        }
        let row = n + 1;
        for y in 0..n {
            for x in 0..n {
                let i = y * row + x;
                indices.extend_from_slice(&[i, i + row, i + 1, i + 1, i + row, i + row + 1]);
            }
        }
        let mesh = CpuMesh {
            positions,
            normals,
            uvs,
            indices,
            ..CpuMesh::default()
        };
        let settings = MeshLodSettings {
            count: 2,
            ratios: vec![1.0, 0.25],
        };
        let cooked = pack_cooked_mesh(&mesh, Some(&settings)).expect("cook");
        assert!(cooked.lods.len() >= 2, "a coarse LOD is stored");
        let width = match cooked.index_width {
            CookedIndexWidth::U16 => 2,
            CookedIndexWidth::U32 => 4,
        };
        let attribute_indices = cooked.lods[1].index_byte_length as usize / width;
        let target = ((mesh.indices.len() as f32) * 0.25).max(3.0) as usize;
        let mut relative_error = 0.0f32;
        let positions_only = simplify_decoder(
            &mesh.indices,
            &mesh.positions,
            target,
            0.01,
            SimplifyOptions::LockBorder,
            Some(&mut relative_error),
        );
        assert!(
            attribute_indices > positions_only.len(),
            "UV warp stays in the coarse LOD: {attribute_indices} indices vs {} positions-only",
            positions_only.len()
        );
    }
}
