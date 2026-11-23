// SPDX-License-Identifier: MPL-2.0
//! CPU reference checks of the voxel far proxy: the blob, the surface, and the DAG hand-off.

use std::collections::BTreeMap;

use super::*;
use crate::visbuffer::LodView;

/// A solid voxel ball of radius `resolution / 2 - 1` voxels in a grid over [-1, 1]³, with
/// outward normals, and its mip chain (a parent is occupied when any child is).
fn ball(resolution: u32) -> VoxelFarProxy {
    ball_with(resolution, 1.0)
}

/// [`ball`] with every voxel at `opacity`.
fn ball_with(resolution: u32, opacity: f32) -> VoxelFarProxy {
    let extent = 2.0f32;
    let mut proxy = VoxelFarProxy {
        origin: [-1.0; 3],
        extent,
        bounds: [0.0, 0.0, 0.0, 1.0],
        source_triangles: 1000,
        ..VoxelFarProxy::default()
    };
    let r = resolution as f32 * 0.5;
    let mut level: BTreeMap<[u32; 3], [f32; 3]> = BTreeMap::new();
    for z in 0..resolution {
        for y in 0..resolution {
            for x in 0..resolution {
                let c = [x as f32 + 0.5 - r, y as f32 + 0.5 - r, z as f32 + 0.5 - r];
                if (c[0] * c[0] + c[1] * c[1] + c[2] * c[2]).sqrt() <= (r - 1.0).max(0.5) {
                    level.insert([x, y, z], c);
                }
            }
        }
    }
    let mut res = resolution;
    loop {
        let size = extent / res as f32;
        let mut bricks: BTreeMap<[u32; 3], BTreeMap<u32, [f32; 3]>> = BTreeMap::new();
        for (xyz, n) in &level {
            let b = xyz.map(|v| v / 4);
            let l = xyz.map(|v| v % 4);
            bricks
                .entry(b)
                .or_default()
                .insert(l[0] + 4 * l[1] + 16 * l[2], *n);
        }
        let brick_offset = proxy.bricks.len() as u32;
        for (b, voxels) in &bricks {
            let mut mask = 0u64;
            let voxel_offset = proxy.voxels.len() as u32;
            for (&bit, n) in voxels {
                mask |= 1 << bit;
                proxy
                    .voxels
                    .push(VoxelSample::new(*n, [0.4, 0.6, 0.2], opacity, [0.25, 0.75]));
            }
            proxy.bricks.push(VoxelBrick {
                coord: VoxelBrick::pack_coord(*b),
                voxel_offset,
                mask_lo: mask as u32,
                mask_hi: (mask >> 32) as u32,
            });
        }
        proxy.levels.push(VoxelLevel {
            resolution: res,
            voxel_size: size,
            error: size * 1.732_050_8,
            brick_offset,
            brick_count: proxy.bricks.len() as u32 - brick_offset,
        });
        if res == 1 {
            break;
        }
        let mut parent: BTreeMap<[u32; 3], [f32; 3]> = BTreeMap::new();
        for (xyz, n) in &level {
            let e = parent.entry(xyz.map(|v| v / 2)).or_insert([0.0; 3]);
            for k in 0..3 {
                e[k] += n[k];
            }
        }
        level = parent;
        res /= 2;
    }
    proxy
}

/// A unit sphere and its single-level cluster DAG (every cluster a root at error 0).
fn sphere_dag() -> (Vec<LabVertex>, Vec<u32>, MeshletMesh) {
    let (vertices, indices) = crate::uv_sphere(1.0, 64, 32);
    let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
    let dag = MeshletMesh::single_level(&positions, &indices);
    (vertices, indices, dag)
}

#[test]
fn blob_round_trips_and_rejects_damage() {
    let proxy = ball(16);
    proxy.validate().expect("valid");
    let bytes = proxy.to_bytes();
    assert_eq!(bytes.len(), proxy.byte_size());
    assert_eq!(VoxelFarProxy::from_bytes(&bytes).expect("parse"), proxy);
    assert_eq!(
        VoxelFarProxy::from_bytes(b"FMLT\x01\0\0\0"),
        Err(VoxelFarFormatError::BadMagic)
    );
    let mut old = bytes.clone();
    old[4..8].copy_from_slice(&7u32.to_le_bytes());
    assert_eq!(
        VoxelFarProxy::from_bytes(&old),
        Err(VoxelFarFormatError::Version { found: 7 })
    );
    assert_eq!(
        VoxelFarProxy::from_bytes(&bytes[..bytes.len() - 3]),
        Err(VoxelFarFormatError::Truncated)
    );
    // A brick outside its level's grid is refused, and the error maps to its agent code.
    let mut bad = proxy.clone();
    bad.bricks[0].coord = VoxelBrick::pack_coord([9, 0, 0]);
    let err = VoxelFarProxy::from_bytes(&bad.to_bytes()).expect_err("invalid");
    assert!(matches!(err, VoxelFarFormatError::Invalid(_)));
    assert_eq!(VoxelFarLoadError::Format(err).code(), "voxel_far_invalid");
    assert_eq!(
        VoxelFarLoadError::Format(VoxelFarFormatError::Version { found: 0 }).code(),
        "voxel_far_old_version"
    );
    // A cooked file of another type is refused.
    let header = regia_asset::CookedHeader {
        format_version: regia_asset::COOKED_FORMAT_VERSION,
        type_tag: regia_core::id::TypePathHash::of(crate::visbuffer::MESHLET_TYPE_PATH),
        asset_id: regia_asset::AssetId::new_random(),
        importer_version: 1,
        dependencies: Vec::new(),
        payload_len: bytes.len() as u64,
    };
    let wrong = regia_asset::encode_cooked_file(&header, &bytes).expect("encode");
    assert_eq!(
        VoxelFarProxy::from_cooked_file(&wrong),
        Err(VoxelFarFormatError::WrongType)
    );
}

#[test]
fn octahedral_normals_round_trip() {
    let mut worst = 1.0f32;
    for i in 0..500 {
        let t = i as f32 * 0.37;
        let n = [
            t.sin() * (t * 0.7).cos(),
            (t * 1.3).cos(),
            t.cos() * (t * 0.7).sin() - 0.2,
        ];
        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
        let n = n.map(|c| c / len);
        let d = oct_decode(oct_encode(n));
        worst = worst.min(n[0] * d[0] + n[1] * d[1] + n[2] * d[2]);
    }
    assert!(worst > 0.9999, "worst cosine {worst}");
    // Degenerate input decodes to a unit vector.
    let d = oct_decode(oct_encode([0.0; 3]));
    assert!(((d[0] * d[0] + d[1] * d[1] + d[2] * d[2]) - 1.0).abs() < 1e-5);
}

#[test]
fn surface_keeps_exposed_faces_wound_outward() {
    let proxy = ball(16);
    for level in 0..proxy.levels.len() {
        let surface = proxy.surface(level);
        assert_eq!(surface.indices.len() % 3, 0);
        let size = proxy.levels[level].voxel_size;
        let mut volume = 0.0f64;
        for range in &surface.voxel_ranges {
            let [_, nv, fi, ni] = *range;
            assert!(nv <= 8 && ni <= 36, "a voxel has at most 8 corners and 6 faces");
            for tri in surface.indices[fi as usize..(fi + ni) as usize]
                .as_chunks::<3>()
                .0
            {
                let [a, b, c] = tri.map(|i| surface.vertices[i as usize].position);
                let n = [
                    (b[1] - a[1]) * (c[2] - a[2]) - (b[2] - a[2]) * (c[1] - a[1]),
                    (b[2] - a[2]) * (c[0] - a[0]) - (b[0] - a[0]) * (c[2] - a[2]),
                    (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]),
                ];
                let area = 0.5 * (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
                assert!((area - 0.5 * size * size).abs() < 1e-4 * size * size.max(1.0));
                // Divergence theorem: a closed, outward-wound surface's signed tetrahedra
                // (against the origin) sum to the enclosed volume.
                let [a, b, c] = [a, b, c].map(|p| p.map(f64::from));
                let det = a[0] * (b[1] * c[2] - b[2] * c[1]) - a[1] * (b[0] * c[2] - b[2] * c[0])
                    + a[2] * (b[0] * c[1] - b[1] * c[0]);
                volume += det / 6.0;
            }
        }
        // The exposed faces of a voxel set bound exactly its voxels: closed, wound outward, no
        // interior faces left over.
        let expect = proxy.voxel_count(level) as f64 * f64::from(size).powi(3);
        assert!(
            (volume - expect).abs() < 1e-3 * expect.max(1e-6),
            "level {level}: enclosed volume {volume} vs {expect}"
        );
    }
    // A full 1-voxel level is one cube: 6 faces, 8 corners.
    let last = proxy.levels.len() - 1;
    let cube = proxy.surface(last);
    assert_eq!((cube.indices.len(), cube.vertices.len()), (36, 8));
    // A solid ball has no interior faces: its face count is that of the shell, far below
    // six per voxel.
    let s0 = proxy.surface(0);
    assert!(s0.indices.len() / 3 < proxy.voxel_count(0) * 12 / 2);
}

#[test]
fn surface_corners_carry_the_prefiltered_sample() {
    let proxy = ball(8);
    let surface = proxy.surface(0);
    for v in &surface.vertices {
        let radial = v.position;
        let len = (radial[0] * radial[0] + radial[1] * radial[1] + radial[2] * radial[2]).sqrt();
        let along = (v.normal[0] * radial[0] + v.normal[1] * radial[1] + v.normal[2] * radial[2]) / len;
        assert!(along > 0.3, "corner normal follows the ball outward ({along})");
        assert!((v.color[0] - 0.4).abs() < 0.01 && (v.color[1] - 0.6).abs() < 0.01);
        assert_eq!(v.uv, [0.25, 0.75]);
    }
}

/// Which representation a view's cut picks: the triangle clusters, one voxel level, or a mix.
fn cut_kind(mesh: &VoxelFarMesh, view: &LodView) -> (usize, Vec<u32>) {
    let base_levels = mesh.levels.first().map_or(u32::MAX, |l| l.dag_level);
    let mut triangle_clusters = 0usize;
    let mut voxel_levels = Vec::new();
    for i in mesh.meshlets.lod_cut(view) {
        let m = &mesh.meshlets.meshlets[i];
        if m.level >= base_levels {
            if !voxel_levels.contains(&m.level) {
                voxel_levels.push(m.level);
            }
        } else {
            triangle_clusters += 1;
        }
    }
    (triangle_clusters, voxel_levels)
}

#[test]
fn voxel_levels_take_over_past_the_last_cluster_lod_without_overlap() {
    let (vertices, _, dag) = sphere_dag();
    let proxy = ball(32);
    let mesh = proxy
        .extend_meshlets(&dag, &vertices, &VoxelFarSettings::default())
        .expect("extend");
    assert!(
        !mesh.levels.is_empty(),
        "some voxel level is cheaper than the roots"
    );
    assert_eq!(mesh.root_triangles as usize, dag.triangle_count());
    for level in &mesh.levels {
        assert!(level.triangles < mesh.root_triangles, "{level:?}");
    }
    for pair in mesh.levels.windows(2) {
        assert!(pair[1].error > pair[0].error && pair[1].triangles <= pair[0].triangles);
    }
    // The mesh's own vertices come first and unchanged; the clusters index the appended ones.
    let pos = |v: &[LabVertex]| v.iter().map(|v| (v.position, v.normal, v.uv)).collect::<Vec<_>>();
    assert_eq!(pos(&mesh.vertices[..vertices.len()]), pos(&vertices));
    assert_eq!(mesh.meshlets.source_vertex_count as usize, mesh.vertices.len());
    assert_eq!(mesh.meshlets.source_index_count, dag.source_index_count);
    mesh.meshlets.validate().expect("extended DAG validates");

    let ppu = 720.0 / (2.0 * (0.5f32).tan());
    let view_at = |d: f32| LodView {
        eye: [0.0, 0.0, d],
        pixels_per_unit: ppu,
        threshold_px: 1.0,
        near: 0.1,
    };
    let radius = mesh.meshlets.meshlets[0].parent[3];
    let first_switch = VoxelFarProxy::switch_distance(mesh.levels[0].error, radius, ppu, 1.0);
    let mut last_level = None;
    let mut distance = 2.0f32;
    while distance < first_switch * 64.0 {
        let (triangles, voxels) = cut_kind(&mesh, &view_at(distance));
        // Exactly one representation: every triangle cluster (the roots), or one voxel level.
        assert!(
            (triangles == dag.meshlets.len() && voxels.is_empty()) || (triangles == 0 && voxels.len() == 1),
            "at {distance}: {triangles} triangle clusters and voxel levels {voxels:?}"
        );
        // The level never gets finer as the view recedes.
        if let Some(&level) = voxels.first() {
            assert!(
                last_level.is_none_or(|l| level >= l),
                "level went finer at {distance}"
            );
            last_level = Some(level);
        }
        let expect_voxels = distance > first_switch * 1.001;
        let near_switch = (distance - first_switch).abs() < first_switch * 0.002;
        if !near_switch {
            assert_eq!(
                !voxels.is_empty(),
                expect_voxels,
                "switch at {first_switch}, view at {distance}"
            );
        }
        distance *= 1.05;
    }
    assert!(
        last_level.is_some_and(|l| l > mesh.levels[0].dag_level),
        "coarser levels follow"
    );
    // The switch distance is where a voxel diagonal covers the threshold.
    let s = proxy.levels[mesh.levels[0].level].voxel_size;
    assert!((mesh.levels[0].error - s * 1.732_050_8).abs() < 1e-6);
}

#[test]
fn shadow_views_take_the_voxel_levels_by_texel_size() {
    // A shadow job's cut uses its texels per metre at any distance (an orthographic cascade
    // projects an error by its texel density), so a coarse cascade takes the voxels.
    let (vertices, _, dag) = sphere_dag();
    let mesh = ball(32)
        .extend_meshlets(&dag, &vertices, &VoxelFarSettings::default())
        .expect("extend");
    let texel_view = |texels_per_metre: f32| LodView {
        eye: [0.0, 0.0, 1.0e6],
        pixels_per_unit: texels_per_metre * 1.0e6,
        threshold_px: 1.0,
        near: 0.1,
    };
    let (fine_tris, fine_vox) = cut_kind(&mesh, &texel_view(200.0));
    assert!(
        fine_tris > 0 && fine_vox.is_empty(),
        "a sharp cascade keeps the triangles"
    );
    let (coarse_tris, coarse_vox) = cut_kind(&mesh, &texel_view(2.0));
    assert!(
        coarse_tris == 0 && coarse_vox.len() == 1,
        "a coarse cascade casts voxels"
    );
}

#[test]
fn levels_no_cheaper_than_the_roots_are_skipped() {
    // A coarse root cut (a 12-triangle box) leaves no level cheaper than it but the 1-voxel cube.
    let (vertices, indices) = crate::box_mesh([1.0, 1.0, 1.0]);
    let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
    let dag = MeshletMesh::single_level(&positions, &indices);
    let mesh = ball(16)
        .extend_meshlets(&dag, &vertices, &VoxelFarSettings::default())
        .expect("extend");
    assert!(mesh.levels.is_empty(), "{:?}", mesh.levels);
    assert_eq!(mesh.meshlets, dag, "nothing cheaper: the DAG is unchanged");
    // A looser ratio admits levels again.
    let loose = ball(16)
        .extend_meshlets(
            &dag,
            &vertices,
            &VoxelFarSettings {
                max_cost_ratio: 1000.0,
                ..VoxelFarSettings::default()
            },
        )
        .expect("extend");
    assert!(!loose.levels.is_empty());
    // Mismatched vertices are refused.
    assert!(
        ball(8)
            .extend_meshlets(&dag, &vertices[..4], &VoxelFarSettings::default())
            .is_err()
    );
}

#[test]
fn error_scale_moves_the_switch() {
    let (vertices, _, dag) = sphere_dag();
    let proxy = ball(32);
    let base = proxy
        .extend_meshlets(&dag, &vertices, &VoxelFarSettings::default())
        .expect("extend");
    let nearer = proxy
        .extend_meshlets(
            &dag,
            &vertices,
            &VoxelFarSettings {
                error_scale: 0.5,
                ..VoxelFarSettings::default()
            },
        )
        .expect("extend");
    assert!(nearer.levels[0].error < base.levels[0].error);
}

#[test]
fn a_level_becomes_a_signed_distance_volume() {
    // ball(8): voxels of 0.25 within 3 voxels of the centre, so the shell's faces sit at 0.75.
    let proxy = ball(8);
    let sdf = proxy.mesh_sdf(0, 16).expect("sdf");
    let inside = sdf.sample([0.0, 0.0, 0.0]);
    // The volume's grid ends one voxel past the shell (0.75 + 1.5 / 16), so sample inside it.
    let outside = sdf.sample([0.8, 0.0, 0.0]);
    assert!(inside < -0.3, "inside distance {inside}");
    assert!(outside > 0.02, "outside distance {outside}");
    assert!(
        (sdf.albedo[1] - 0.6).abs() < 0.01,
        "the level's mean color: {:?}",
        sdf.albedo
    );
    assert!(proxy.mesh_sdf(99, 8).is_none());
}

#[test]
fn switch_distance_inverts_the_projected_error() {
    let view = LodView {
        eye: [0.0, 0.0, 0.0],
        pixels_per_unit: 1000.0,
        threshold_px: 1.0,
        near: 0.1,
    };
    let d = VoxelFarProxy::switch_distance(0.05, 2.0, 1000.0, 1.0);
    let at = view.projected_error([0.0, 0.0, -d, 2.0], 0.05);
    assert!((at - 1.0).abs() < 1e-4, "{at}");
}

#[test]
fn upgrade_project_reads_the_cooked_version() {
    let bytes = ball(4).to_bytes();
    let header = |type_path: &str| regia_asset::CookedHeader {
        format_version: regia_asset::COOKED_FORMAT_VERSION,
        type_tag: regia_core::id::TypePathHash::of(type_path),
        asset_id: regia_asset::AssetId::new_random(),
        importer_version: 1,
        dependencies: Vec::new(),
        payload_len: bytes.len() as u64,
    };
    let cooked = regia_asset::encode_cooked_file(&header(VOXEL_FAR_TYPE_PATH), &bytes).expect("encode");
    assert_eq!(
        cooked_voxel_far_file_version(&cooked),
        Ok(Some(VOXEL_FAR_FORMAT_VERSION))
    );
    let other = regia_asset::encode_cooked_file(&header("regia_asset::CookedMesh"), &bytes).expect("encode");
    assert_eq!(cooked_voxel_far_file_version(&other), Ok(None));
    assert_eq!(cooked_voxel_far_file_version(b"not cooked"), Ok(None));
    let junk = b"XXXX\x01\0\0\0";
    let mut bad_header = header(VOXEL_FAR_TYPE_PATH);
    bad_header.payload_len = junk.len() as u64;
    let bad = regia_asset::encode_cooked_file(&bad_header, junk).expect("encode");
    assert!(cooked_voxel_far_file_version(&bad).is_err());
    assert!(voxel_far_schema_fingerprint().starts_with("FVOX;v4;"));
    assert!(voxel_far_schema_fingerprint().contains("coverage"));
    assert_eq!(voxel_far_migration_sources(), [1, 2, 3]);
}

/// Signed volume of a triangle surface (divergence theorem).
fn enclosed_volume(surface: &VoxelSurface) -> f64 {
    surface
        .indices
        .as_chunks::<3>()
        .0
        .iter()
        .map(|tri| {
            let [a, b, c] = tri.map(|i| surface.vertices[i as usize].position.map(f64::from));
            (a[0] * (b[1] * c[2] - b[2] * c[1]) - a[1] * (b[0] * c[2] - b[2] * c[0])
                + a[2] * (b[0] * c[1] - b[1] * c[0]))
                / 6.0
        })
        .sum()
}

#[test]
fn opacity_dropout_keeps_opaque_voxels_and_thins_transparent_ones() {
    // Opaque voxels: dropout changes nothing.
    let solid = ball(16);
    assert_eq!(
        solid.surface_with_dropout(0, true).indices,
        solid.surface(0).indices
    );
    // Half-transparent voxels: about half are kept, the same ones every time, and the faces
    // they leave open make the surface of exactly the kept voxels (still closed).
    let half = ball_with(16, 0.5);
    let size = f64::from(half.levels[0].voxel_size);
    let total = half.voxel_count(0);
    let kept = half
        .level_voxels(0)
        .filter(|(xyz, v)| VoxelFarProxy::keeps(0, *xyz, v, true))
        .count();
    let share = kept as f64 / total as f64;
    assert!((0.4..0.6).contains(&share), "kept {kept} of {total}");
    let thinned = half.surface_with_dropout(0, true);
    assert_eq!(
        thinned.indices,
        half.surface_with_dropout(0, true).indices,
        "deterministic"
    );
    let volume = enclosed_volume(&thinned);
    let expect = kept as f64 * size.powi(3);
    assert!(
        (volume - expect).abs() < 1e-3 * expect,
        "volume {volume} vs {expect}"
    );
    // Transparent voxels are all dropped; the threshold is in [0, 1).
    assert!(ball_with(8, 0.0).surface_with_dropout(0, true).indices.is_empty());
    for i in 0..1000u32 {
        let t = voxel_dither([i, i * 7, i * 13], i % 5);
        assert!((0.0..1.0).contains(&t));
    }
    // extend_meshlets thins with the default settings and keeps everything without dropout.
    let (vertices, _, dag) = sphere_dag();
    let half32 = ball_with(32, 0.5);
    let thin = half32
        .extend_meshlets(&dag, &vertices, &VoxelFarSettings::default())
        .expect("extend");
    let full = half32
        .extend_meshlets(
            &dag,
            &vertices,
            &VoxelFarSettings {
                opacity_dropout: false,
                ..VoxelFarSettings::default()
            },
        )
        .expect("extend");
    assert!(full.vertices.len() > thin.vertices.len());
}

/// `proxy` as a version 3 blob: the current bytes with version 3 and no coverage block.
fn v3_bytes(proxy: &VoxelFarProxy) -> Vec<u8> {
    let mut bytes = proxy.to_bytes();
    bytes[4..8].copy_from_slice(&3u32.to_le_bytes());
    bytes.truncate(bytes.len() - 4 - proxy.coverage.len() * 4);
    bytes
}

/// `proxy` as a version 1 blob: the v3 bytes with version 1 and no extent or sampled-error
/// array.
fn v1_bytes(proxy: &VoxelFarProxy) -> Vec<u8> {
    let mut bytes = v3_bytes(proxy);
    bytes[4..8].copy_from_slice(&1u32.to_le_bytes());
    bytes.truncate(bytes.len() - proxy.voxels.len() * 4 - proxy.levels.len() * 4);
    bytes
}

/// `proxy` as a version 2 blob: the v3 bytes with version 2 and no sampled-error array.
fn v2_bytes(proxy: &VoxelFarProxy) -> Vec<u8> {
    let mut bytes = v3_bytes(proxy);
    bytes[4..8].copy_from_slice(&2u32.to_le_bytes());
    bytes.truncate(bytes.len() - proxy.levels.len() * 4);
    bytes
}

/// Version 3 blobs (no coverage estimate) still load with an empty [`VoxelFarProxy::coverage`]
/// and migrate to v4 unchanged; a v4 blob's coverage round trips, and a coverage that is not one
/// per level, not finite, negative or past [`MAX_VOXEL_COVERAGE`] is refused (ADR-3227).
#[test]
fn v3_blobs_load_and_migrate_to_v4_and_coverage_round_trips() {
    let proxy = ball(8);
    let old = v3_bytes(&proxy);
    let loaded = VoxelFarProxy::from_bytes(&old).expect("v3 loads");
    assert_eq!(loaded, proxy);
    assert!(loaded.coverage.is_empty());
    let file = cooked(&old);
    assert_eq!(cooked_voxel_far_file_version(&file), Ok(Some(3)));
    let migrated = migrate_cooked_voxel_far_file(&file)
        .expect("migrates")
        .expect("older version");
    assert_eq!(cooked_voxel_far_file_version(&migrated), Ok(Some(4)));
    assert_eq!(VoxelFarProxy::from_cooked_file(&migrated).expect("v4"), proxy);

    let mut measured = proxy.clone();
    measured.coverage = (0..measured.levels.len())
        .map(|i| 0.75 + i as f32 * 0.125)
        .collect();
    measured.validate().expect("a coverage per level is valid");
    let bytes = measured.to_bytes();
    assert_eq!(bytes.len(), measured.byte_size());
    let back = VoxelFarProxy::from_bytes(&bytes).expect("v4 round trip");
    assert_eq!(back, measured);
    assert_eq!(back.coverage, measured.coverage);
    for bad in [f32::NAN, -0.5, MAX_VOXEL_COVERAGE * 2.0] {
        let mut wrong = measured.clone();
        wrong.coverage[0] = bad;
        assert!(wrong.validate().is_err(), "coverage {bad} is refused");
    }
    let mut short = measured.clone();
    short.coverage.pop();
    assert!(short.validate().is_err());
    // A count that is neither 0 nor the level count is a format error, not a panic.
    let mut damaged = measured.to_bytes();
    let count_at = damaged.len() - 4 - measured.coverage.len() * 4;
    damaged[count_at..count_at + 4].copy_from_slice(&1u32.to_le_bytes());
    assert!(VoxelFarProxy::from_bytes(&damaged).is_err());
}

/// Version 2 blobs (boxes, no sampled error, ADR-2655) still load with their analytic errors and
/// migrate to v3 unchanged; a v3 blob's sampled errors round trip, tighten only the box shape, and
/// may never exceed the level's error (ADR-3108).
#[test]
fn v2_blobs_load_and_migrate_to_v3_and_sampled_errors_round_trip() {
    let mut proxy = ball(8);
    // Boxes on every voxel: the ball's samples as one flat slab each.
    proxy.extents = vec![pack_voxel_extent([0, 7, 0], [15, 8, 15]); proxy.voxels.len()];
    for index in 0..proxy.levels.len() {
        proxy.levels[index].error = proxy.level_extent_error(index);
    }
    proxy.validate().expect("valid v2-shaped proxy");
    let old = v2_bytes(&proxy);
    let loaded = VoxelFarProxy::from_bytes(&old).expect("v2 loads");
    assert_eq!(loaded, proxy);
    assert!(loaded.sampled_errors.is_empty());
    assert_eq!(loaded.level_box_error(0), proxy.levels[0].error);
    let file = cooked(&old);
    assert_eq!(cooked_voxel_far_file_version(&file), Ok(Some(2)));
    let migrated = migrate_cooked_voxel_far_file(&file)
        .expect("migrates")
        .expect("older version");
    assert_eq!(
        cooked_voxel_far_file_version(&migrated),
        Ok(Some(VOXEL_FAR_FORMAT_VERSION))
    );
    assert_eq!(
        VoxelFarProxy::from_cooked_file(&migrated).expect("current"),
        proxy
    );

    let mut sampled = proxy.clone();
    sampled.sampled_errors = sampled.levels.iter().map(|l| l.error * 0.25).collect();
    sampled.validate().expect("tighter sampled errors are valid");
    let back = VoxelFarProxy::from_bytes(&sampled.to_bytes()).expect("v3 round trip");
    assert_eq!(back, sampled);
    assert!((back.level_box_error(0) - proxy.levels[0].error * 0.25).abs() < 1e-6);
    // The analytic error and the cube error are untouched.
    assert_eq!(back.levels[0].error, proxy.levels[0].error);
    assert_eq!(back.level_cube_error(0), proxy.level_cube_error(0));
    // Sampled errors that loosen the level, are not finite, or are not one per level are refused.
    let mut loose = sampled.clone();
    loose.sampled_errors[0] = loose.levels[0].error * 1.5;
    assert!(loose.validate().is_err());
    let mut nan = sampled.clone();
    nan.sampled_errors[0] = f32::NAN;
    assert!(nan.validate().is_err());
    let mut short = sampled;
    short.sampled_errors.pop();
    assert!(short.validate().is_err());
}

fn cooked(payload: &[u8]) -> Vec<u8> {
    let header = regia_asset::CookedHeader {
        format_version: regia_asset::COOKED_FORMAT_VERSION,
        type_tag: regia_core::id::TypePathHash::of(VOXEL_FAR_TYPE_PATH),
        asset_id: regia_asset::AssetId::new_random(),
        importer_version: 1,
        dependencies: Vec::new(),
        payload_len: payload.len() as u64,
    };
    regia_asset::encode_cooked_file(&header, payload).expect("encode")
}

/// Version 1 blobs (whole cubes) still load, with every box full and their cube-diagonal error,
/// and `regia upgrade-project`'s migrator rewrites them as v2 with the same proxy.
#[test]
fn v1_blobs_load_full_and_migrate_to_v2() {
    let proxy = ball(8);
    let old = v1_bytes(&proxy);
    let loaded = VoxelFarProxy::from_bytes(&old).expect("v1 loads");
    assert_eq!(loaded, proxy);
    assert!(loaded.extents.is_empty(), "every box full");
    assert_eq!(
        loaded.surface(0).indices,
        proxy.surface(0).indices,
        "draws as v1 did"
    );
    let file = cooked(&old);
    assert_eq!(cooked_voxel_far_file_version(&file), Ok(Some(1)));
    let migrated = migrate_cooked_voxel_far_file(&file)
        .expect("migrates")
        .expect("older version");
    assert_eq!(
        cooked_voxel_far_file_version(&migrated),
        Ok(Some(VOXEL_FAR_FORMAT_VERSION))
    );
    assert_eq!(VoxelFarProxy::from_cooked_file(&migrated).expect("v2"), proxy);
    assert_eq!(
        migrate_cooked_voxel_far_file(&migrated),
        Ok(None),
        "already current"
    );
    assert_eq!(migrate_cooked_voxel_far_file(b"not cooked"), Ok(None));
    // A damaged v1 payload is an error, not a panic.
    assert!(migrate_cooked_voxel_far_file(&cooked(&old[..old.len() - 5])).is_err());
    // Bytes past the declared arrays are refused (v1 blob read as v2 is short; v2 with junk long).
    let mut long = proxy.to_bytes();
    long.extend_from_slice(&[0; 4]);
    assert!(VoxelFarProxy::from_bytes(&long).is_err());
}

#[test]
fn extent_bounds_are_honest_and_tighter_than_the_diagonal() {
    let s = 0.3f32;
    let close = |a: f32, b: f32| (a - b).abs() < 1e-4;
    assert_eq!(unpack_voxel_extent(VOXEL_EXTENT_FULL), ([0; 3], [15; 3]));
    assert_eq!(pack_voxel_extent([0; 3], [15; 3]), VOXEL_EXTENT_FULL);
    assert_eq!(
        unpack_voxel_extent(pack_voxel_extent([1, 2, 3], [4, 5, 99])),
        ([1, 2, 3], [4, 5, 15])
    );
    // A full box: 1.5 voxel edges, against the cube's diagonal 1.73.
    assert!(close(
        voxel_extent_error(VOXEL_EXTENT_FULL, s),
        1.5 * s + s * 1e-4
    ));
    assert!(voxel_extent_error(VOXEL_EXTENT_FULL, s) < s * 3f32.sqrt());
    // Flat (no size on z): half an edge along x plus a full edge across.
    let flat = pack_voxel_extent([0, 0, 7], [15, 15, 7]);
    assert!(close(
        voxel_extent_error(flat, s),
        (0.25f32 + 1.0).sqrt() * s + s * 1e-4
    ));
    // One step thick on z: the step itself is the term along z when it is the smallest.
    let thin = pack_voxel_extent([7, 0, 7], [8, 15, 8]);
    let expect = ((0.5f32).powi(2) + 2.0 / 225.0).sqrt() * s + s * 1e-4;
    assert!(
        close(voxel_extent_error(thin, s), expect),
        "{}",
        voxel_extent_error(thin, s)
    );
    // Brute force: every point of a box is within the bound of the surface that touches each of
    // its faces (here the box's two opposite edges, the worst surface the box rule allows).
    // Whole cubes: 1.5 edges when the box spans the voxel, the diagonal for a corner point or an
    // unbounded voxel.
    assert!(close(voxel_cube_error(VOXEL_EXTENT_FULL, s), 1.5 * s + s * 1e-4));
    assert!(close(
        voxel_cube_error(pack_voxel_extent([0; 3], [0; 3]), s),
        s * 3f32.sqrt()
    ));
    let inexact = VOXEL_EXTENT_FULL | VOXEL_EXTENT_INEXACT;
    assert!(close(voxel_cube_error(inexact, s), s * 3f32.sqrt()));
    assert!(close(voxel_extent_error(inexact, s), s * 3f32.sqrt()));
    for packed in [
        VOXEL_EXTENT_FULL,
        flat,
        thin,
        pack_voxel_extent([2, 3, 4], [9, 15, 6]),
        pack_voxel_extent([0, 0, 0], [3, 2, 4]),
        pack_voxel_extent([12, 1, 6], [15, 14, 9]),
    ] {
        let (lo, hi) = unpack_voxel_extent(packed);
        let at = |q: [f32; 3]| -> [f32; 3] {
            std::array::from_fn(|k| (lo[k] as f32 + (hi[k] - lo[k]) as f32 * q[k]) / 15.0 * s)
        };
        // Surface: the segment from the low corner along x, and the one from the high corner
        // along -x: together they touch all six faces of the box.
        let surface: Vec<[f32; 3]> = (0..=40)
            .flat_map(|i| {
                let t = i as f32 / 40.0;
                [at([t, 0.0, 0.0]), at([1.0 - t, 1.0, 1.0])]
            })
            .collect();
        let bound = voxel_extent_error(packed, s);
        let cube_bound = voxel_cube_error(packed, s);
        assert!(cube_bound >= bound - 1e-6 && cube_bound <= s * 3f32.sqrt() + 1e-6);
        for i in 0..=8 {
            for j in 0..=8 {
                for k in 0..=8 {
                    let t = [i as f32 / 8.0, j as f32 / 8.0, k as f32 / 8.0];
                    let nearest = |p: [f32; 3]| {
                        surface
                            .iter()
                            .map(|q| (0..3).map(|c| (p[c] - q[c]).powi(2)).sum::<f32>().sqrt())
                            .fold(f32::MAX, f32::min)
                    };
                    let d = nearest(at(t));
                    assert!(d <= bound + s * 0.02, "{packed:x}: {d} > {bound}");
                    // Every point of the whole cube is within the cube bound.
                    let d = nearest(t.map(|v| v * s));
                    assert!(d <= cube_bound + s * 0.02, "{packed:x} cube: {d} > {cube_bound}");
                }
            }
        }
    }
}

/// One flat box in the middle of a 4³ grid: it draws its two broad faces inside its voxel, and
/// validation keeps the level error at or above the boxes' bound.
#[test]
fn sub_voxel_boxes_draw_inside_their_voxels() {
    let mut proxy = ball(4);
    proxy.extents = vec![VOXEL_EXTENT_FULL; proxy.voxels.len()];
    // Level 0 of ball(4) is the 2×2×2 core; flatten every box to z steps 7..7.
    let first = proxy.level_bricks(0)[0];
    let count = first.mask().count_ones() as usize;
    for e in &mut proxy.extents[..count] {
        *e = pack_voxel_extent([0, 0, 7], [15, 15, 7]);
    }
    proxy
        .validate()
        .expect("valid: the cube-diagonal error is above the flat bound");
    let surface = proxy.surface(0);
    let size = proxy.levels[0].voxel_size;
    // Each flat box keeps only its ±z faces that are not covered: neighbours along x and y
    // reach the shared side but have no area there, so 2 faces per voxel.
    assert_eq!(surface.indices.len() / 6, count * 2, "two broad faces per voxel");
    for (range, (xyz, _)) in surface.voxel_ranges.iter().zip(proxy.level_voxels(0)) {
        let z = proxy.origin[2] + (xyz[2] as f32 + 7.0 / 15.0) * size;
        for v in &surface.vertices[range[0] as usize..(range[0] + range[1]) as usize] {
            assert!((v.position[2] - z).abs() < 1e-5, "on the box's plane");
        }
    }
    // Round trip with the boxes, and the tighter error is accepted.
    let mut tight = proxy.clone();
    tight.levels[0].error = proxy.level_extent_error(0);
    assert!(tight.levels[0].error < size * 1.2);
    assert_eq!(
        VoxelFarProxy::from_bytes(&tight.to_bytes()).expect("parse"),
        tight
    );
    // An error below the boxes' bound, a reversed box, reserved bits and a short array are not.
    let mut low = tight.clone();
    low.levels[0].error *= 0.9;
    assert!(matches!(low.validate(), Err(VoxelFarFormatError::Invalid(_))));
    let mut reversed = tight.clone();
    reversed.extents[0] = pack_voxel_extent([9, 0, 0], [3, 15, 15]);
    assert!(reversed.validate().is_err());
    let mut reserved = tight.clone();
    reserved.extents[0] |= 1 << 30;
    assert!(reserved.validate().is_err());
    let mut short = tight;
    short.extents.pop();
    assert!(short.validate().is_err());
}

/// The bridged DAG keeps the mesh's own vertices as its base range and carries the voxel
/// vertices itself, so a mesh entity's vertex buffer and mesh key do not change.
#[test]
fn the_bridged_dag_appends_the_voxel_vertices() {
    let (vertices, _, dag) = sphere_dag();
    let far = ball(32)
        .extend_meshlets(&dag, &vertices, &VoxelFarSettings::default())
        .expect("extend");
    assert!(!far.levels.is_empty());
    let bridged = far.bridged();
    bridged.validate().expect("valid");
    assert_eq!(bridged.base_vertex_count(), vertices.len());
    assert_eq!(
        bridged.appended_vertices.0.len(),
        far.vertices.len() - vertices.len()
    );
    assert_eq!(bridged.source_index_count, dag.source_index_count);
    assert_eq!(
        far.voxel_triangles(),
        far.levels.iter().map(|l| u64::from(l.triangles)).sum::<u64>()
    );
    // A DAG that already appends is not extended again.
    assert!(
        ball(32)
            .extend_meshlets(&bridged, &vertices, &VoxelFarSettings::default())
            .is_err()
    );
}

/// A level tries its shapes from the smallest error up and takes the first cheaper than the
/// roots: flat boxes where they are cheap enough, whole cubes where they are not.
#[test]
fn levels_draw_the_cheapest_shape_that_keeps_the_error_low() {
    let (vertices, _, dag) = sphere_dag();
    let mut proxy = ball(32);
    proxy.extents = vec![pack_voxel_extent([0, 0, 7], [15, 15, 7]); proxy.voxels.len()];
    let errors: Vec<f32> = (0..proxy.levels.len())
        .map(|l| proxy.level_extent_error(l))
        .collect();
    for (level, error) in proxy.levels.iter_mut().zip(errors) {
        level.error = error;
    }
    proxy.validate().expect("valid");
    let size = proxy.levels[0].voxel_size;
    // Cubes cost the fewest faces and draw as v1 did; flat boxes cost more.
    let cubes = proxy.surface_drawn(0, false, VoxelShape::Cubes);
    let boxes = proxy.surface_drawn(0, false, VoxelShape::Boxes);
    assert_eq!(cubes.indices, ball(32).surface(0).indices, "cubes draw as v1");
    assert!(boxes.indices.len() > cubes.indices.len());
    assert!(proxy.level_cube_error(0) > proxy.levels[0].error);
    let far = proxy
        .extend_meshlets(&dag, &vertices, &VoxelFarSettings::default())
        .expect("extend");
    assert!(!far.levels.is_empty());
    for used in &far.levels {
        let info = proxy.levels[used.level];
        assert!(used.error >= info.error * (1.0 - 1e-6));
        assert!(used.error <= proxy.level_cube_error(used.level) * (1.0 + 1e-6));
        if used.shape == VoxelShape::Boxes {
            assert!(
                (used.error - info.error).abs() < 1e-6,
                "boxes at the boxes' error"
            );
        }
    }
    // With a cost ratio that lets every surface in, the first level draws boxes at the flat
    // boxes' error (well under a cube's 1.5 edges).
    let generous = proxy
        .extend_meshlets(
            &dag,
            &vertices,
            &VoxelFarSettings {
                max_cost_ratio: 1e6,
                ..VoxelFarSettings::default()
            },
        )
        .expect("extend");
    assert_eq!(generous.levels[0].shape, VoxelShape::Boxes);
    assert!(generous.levels[0].error < 1.2 * size);
    // An unbounded voxel must be whole.
    let mut bad = proxy.clone();
    bad.extents[0] = pack_voxel_extent([0, 0, 7], [15, 15, 7]) | VOXEL_EXTENT_INEXACT;
    assert!(bad.validate().is_err());
}
