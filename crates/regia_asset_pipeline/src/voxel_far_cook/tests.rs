// SPDX-License-Identifier: MPL-2.0
//! CPU reference checks of the voxel cooker and of the cooked proxy on a real cluster DAG.

use super::*;
use crate::meshlet::{MeshletAttributes, MeshletSettings, build_meshlet_mesh_with_attributes};
use regia_render::LabVertex;
use regia_render::visbuffer::{LodView, VoxelFarSettings};

fn uv_sphere(rings: u32, segments: u32) -> regia_asset::CpuMesh {
    let mut positions = Vec::new();
    let mut normals = Vec::new();
    let mut uvs = Vec::new();
    for r in 0..=rings {
        let phi = std::f32::consts::PI * r as f32 / rings as f32;
        for s in 0..=segments {
            let theta = std::f32::consts::TAU * s as f32 / segments as f32;
            let n = [phi.sin() * theta.cos(), phi.cos(), phi.sin() * theta.sin()];
            positions.push(n);
            normals.push(n);
            uvs.push([s as f32 / segments as f32, r as f32 / rings as f32]);
        }
    }
    let mut indices = Vec::new();
    for r in 0..rings {
        for s in 0..segments {
            let a = r * (segments + 1) + s;
            let b = a + segments + 1;
            indices.extend_from_slice(&[a, b, a + 1, a + 1, b, b + 1]);
        }
    }
    regia_asset::CpuMesh {
        positions,
        normals,
        uvs,
        indices,
        ..Default::default()
    }
}

/// A foliage-like canopy: `count` small disjoint leaf quads scattered in a unit ball (a fixed
/// LCG, so every run is the same), green vertex color. Disjoint quads are what the simplifier
/// cannot reduce: the cluster DAG's roots keep most of the leaves.
fn canopy(count: u32) -> regia_asset::CpuMesh {
    let mut state = 0x2545_f491u32;
    let mut next = || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (state >> 8) as f32 / (1u32 << 24) as f32
    };
    let mut mesh = regia_asset::CpuMesh::default();
    for _ in 0..count {
        let (mut c, mut l2) = ([0.0f32; 3], 2.0f32);
        while l2 > 1.0 {
            c = [next() * 2.0 - 1.0, next() * 2.0 - 1.0, next() * 2.0 - 1.0];
            l2 = c[0] * c[0] + c[1] * c[1] + c[2] * c[2];
        }
        let len = l2.sqrt().max(1e-3);
        let n = c.map(|v| v / len);
        // Two tangents of the leaf plane.
        let a = if n[0].abs() < 0.9 {
            [1.0, 0.0, 0.0]
        } else {
            [0.0, 1.0, 0.0]
        };
        let t = cross(n, a);
        let tl = dot(t, t).sqrt();
        let t = t.map(|v| v / tl * 0.04);
        let b = cross(n, t);
        let base = mesh.positions.len() as u32;
        for (i, (u, v)) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)]
            .into_iter()
            .enumerate()
        {
            mesh.positions
                .push(std::array::from_fn(|k| c[k] * 0.9 + t[k] * u + b[k] * v));
            mesh.normals.push(n);
            mesh.uvs.push([(i & 1) as f32, (i >> 1) as f32]);
            mesh.colors.push([0.2, 0.5, 0.1, 1.0]);
        }
        mesh.indices
            .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    mesh
}

fn input(mesh: &regia_asset::CpuMesh) -> VoxelizeInput<'_> {
    VoxelizeInput {
        positions: &mesh.positions,
        normals: &mesh.normals,
        uvs: &mesh.uvs,
        colors: &mesh.colors,
        indices: &mesh.indices,
    }
}

fn lab_vertices(mesh: &regia_asset::CpuMesh) -> Vec<LabVertex> {
    mesh.positions
        .iter()
        .zip(&mesh.normals)
        .zip(&mesh.uvs)
        .map(|((p, n), uv)| LabVertex::from_pos_normal_uv(*p, *n, *uv))
        .collect()
}

#[test]
fn triangle_box_overlap_separates_on_every_axis_kind() {
    let half = [0.5; 3];
    // Inside, touching, and clearly apart along a box axis.
    let inside = [[0.0, 0.0, 0.0], [0.1, 0.0, 0.0], [0.0, 0.1, 0.0]];
    assert!(triangle_overlaps_box([0.0; 3], half, inside));
    let apart = [[2.0, 0.0, 0.0], [2.1, 0.0, 0.0], [2.0, 0.1, 0.0]];
    assert!(!triangle_overlaps_box([0.0; 3], half, apart));
    // A big triangle whose plane misses the box (separated by the triangle normal).
    let above = [[-5.0, -5.0, 0.8], [5.0, -5.0, 0.8], [0.0, 5.0, 0.8]];
    assert!(!triangle_overlaps_box([0.0; 3], half, above));
    // A triangle passing diagonally near a corner but outside it (separated by an edge cross
    // product only: its AABB and plane both overlap the box).
    let diagonal = [[1.2, 0.0, -1.0], [0.0, 1.2, -1.0], [0.0, 1.2, 1.0]];
    assert!(!triangle_overlaps_box([0.0; 3], half, diagonal));
    // A big triangle that slices through the box.
    let slice = [[-5.0, -5.0, 0.1], [5.0, -5.0, 0.1], [0.0, 5.0, 0.1]];
    assert!(triangle_overlaps_box([0.0; 3], half, slice));
}

/// A non-finite texture coordinate is user data. It must not be stored on a voxel sample: the
/// sample's UV is what the far proxy samples the material with.
#[test]
fn audfix_non_finite_uv_does_not_enter_the_proxy() {
    let positions = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
    let uvs = [[0.2, 0.3], [f32::NAN, 0.4], [0.6, 0.7]];
    let indices = [0u32, 1, 2];
    let mesh = VoxelizeInput {
        positions: &positions,
        normals: &[],
        uvs: &uvs,
        colors: &[],
        indices: &indices,
    };
    let proxy = voxelize("nan-uv", &mesh, &VoxelFarCookSettings { resolution: 4 }).expect("voxelize");
    assert!(!proxy.voxels.is_empty(), "the triangle still makes voxels");
    assert!(
        proxy
            .voxels
            .iter()
            .all(|s| s.uv[0].is_finite() && s.uv[1].is_finite()),
        "a NaN UV was stored: {:?}",
        proxy.voxels.iter().map(|s| s.uv).collect::<Vec<_>>()
    );
}

#[test]
fn a_sphere_voxelizes_to_a_conservative_shell_with_a_mip_chain() {
    let mesh = uv_sphere(24, 48);
    let settings = VoxelFarCookSettings { resolution: 16 };
    let proxy = voxelize("sphere", &input(&mesh), &settings).expect("voxelize");
    proxy.validate().expect("valid");
    assert_eq!(proxy.levels.len(), 5, "16, 8, 4, 2, 1");
    assert_eq!(
        proxy.levels.iter().map(|l| l.resolution).collect::<Vec<_>>(),
        [16, 8, 4, 2, 1]
    );
    assert_eq!(proxy.voxel_count(4), 1);
    // Conservative: every vertex lies in an occupied voxel.
    let size = proxy.levels[0].voxel_size;
    let occupied: std::collections::HashSet<[u32; 3]> = proxy.level_voxels(0).map(|(xyz, _)| xyz).collect();
    for p in &mesh.positions {
        let xyz: [u32; 3] =
            std::array::from_fn(|k| (((p[k] - proxy.origin[k]) / size).floor() as u32).min(15));
        assert!(occupied.contains(&xyz), "vertex {p:?} not in an occupied voxel");
    }
    // A shell, not a solid: the centre voxel is empty, and the mean normals point outward.
    assert!(!occupied.contains(&[8, 8, 8]) && !occupied.contains(&[7, 7, 7]));
    for (xyz, sample) in proxy.level_voxels(0) {
        let c: [f32; 3] = std::array::from_fn(|k| proxy.origin[k] + (xyz[k] as f32 + 0.5) * size);
        let n = sample.unit_normal();
        let len = dot(c, c).sqrt();
        assert!(dot(n, c) / len > 0.7, "voxel {xyz:?} normal {n:?}");
        assert!(sample.opacity() > 0.0);
        assert_eq!(sample.rgb(), [1.0, 1.0, 1.0], "no vertex colors: white");
    }
    // Same input, same bytes.
    let again = voxelize("sphere", &input(&mesh), &settings).expect("voxelize");
    assert_eq!(proxy.to_bytes(), again.to_bytes());
}

#[test]
fn a_flat_quad_is_opaque_with_its_texture_coordinates() {
    // A 1×1 quad in z = 0 at resolution 4: each voxel it crosses is opaque (a plane).
    let mesh = regia_asset::CpuMesh {
        positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 1.0, 0.0], [0.0, 1.0, 0.0]],
        normals: vec![[0.0, 0.0, 1.0]; 4],
        uvs: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
        colors: vec![[1.0, 0.0, 0.0, 1.0]; 4],
        indices: vec![0, 1, 2, 0, 2, 3],
        ..Default::default()
    };
    let proxy = voxelize("quad", &input(&mesh), &VoxelFarCookSettings { resolution: 4 }).expect("voxelize");
    let voxels: Vec<_> = proxy.level_voxels(0).collect();
    assert!(voxels.len() >= 16, "{} voxels", voxels.len());
    let mut interior = 0;
    for (xyz, s) in &voxels {
        assert!((s.unit_normal()[2] - 1.0).abs() < 1e-3);
        assert!((s.rgb()[0] - 1.0).abs() < 0.01 && s.rgb()[1] < 0.01);
        let uv = s.uv;
        assert!((0.0..=1.0).contains(&uv[0]) && (0.0..=1.0).contains(&uv[1]));
        if (1..3).contains(&xyz[0]) && (1..3).contains(&xyz[1]) {
            interior += 1;
            // Interior voxels: a plane stops every ray that crosses it.
            assert!(s.opacity() > 0.99, "opacity {}", s.opacity());
            let expect = [(xyz[0] as f32 + 0.5) / 4.0, (xyz[1] as f32 + 0.5) / 4.0];
            assert!(
                (uv[0] - expect[0]).abs() < 0.02 && (uv[1] - expect[1]).abs() < 0.02,
                "{uv:?} vs {expect:?}"
            );
        }
    }
    assert!(interior >= 4);
}

#[test]
fn nothing_to_voxelize_is_an_error() {
    let mesh = regia_asset::CpuMesh {
        positions: vec![[f32::NAN, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
        indices: vec![0, 1, 2, 0, 1, 9],
        ..Default::default()
    };
    assert!(matches!(
        voxelize("bad", &input(&mesh), &VoxelFarCookSettings::default()),
        Err(VoxelFarCookError::Empty { .. })
    ));
}

fn cooked_payload(mesh: regia_asset::CpuMesh) -> Vec<u8> {
    let cooked = crate::mesh_cook::pack_cooked_mesh(&crate::mesh_cook::cook_mesh(mesh), None).expect("pack");
    cooked.to_postcard().expect("postcard")
}

#[test]
fn cooked_proxies_round_trip_and_reuse_the_cache() {
    let dir = regia_test_utils::unique_temp_dir("regia-voxel-far-cook");
    let payload = cooked_payload(uv_sphere(12, 24));
    let parent = AssetId::new_random();
    let mesh_id = AssetId::sub_asset(parent, "mesh/Tree/primitive_0");
    let subs = [(
        "mesh/Tree/primitive_0",
        mesh_id,
        MESH_TYPE_PATH,
        payload.as_slice(),
    )];
    let settings = VoxelFarCookSettings { resolution: 16 };
    let first = cook_voxel_far_sub_assets(
        &dir,
        "assets/tree.gltf",
        1,
        parent,
        &settings,
        subs.iter().copied(),
    )
    .expect("cook");
    assert_eq!(first.built, 1);
    assert_eq!(first.manifest[0].path, "assets/tree.gltf#voxels/Tree/primitive_0");
    assert_eq!(first.manifest[0].type_path, VOXEL_FAR_TYPE_PATH);
    let again = cook_voxel_far_sub_assets(
        &dir,
        "assets/tree.gltf",
        1,
        parent,
        &settings,
        subs.iter().copied(),
    )
    .expect("recook");
    assert_eq!(again.built, 0, "same mesh and settings: cached");
    let finer = VoxelFarCookSettings { resolution: 32 };
    let changed =
        cook_voxel_far_sub_assets(&dir, "assets/tree.gltf", 1, parent, &finer, subs.iter().copied())
            .expect("recook");
    assert_eq!(changed.built, 1, "a new resolution rebuilds");
    let bytes = std::fs::read(dir.join(cooked_file_name(first.manifest[0].id))).expect("read");
    let proxy = VoxelFarProxy::from_cooked_file(&bytes).expect("runtime load, no cooker");
    assert_eq!(proxy.levels[0].resolution, 32);
    // Other sub-asset types are skipped.
    let none = cook_voxel_far_sub_assets(
        &dir,
        "assets/tree.gltf",
        1,
        parent,
        &settings,
        [(
            "material/Leaf",
            mesh_id,
            "regia_asset::CookedMaterial",
            payload.as_slice(),
        )]
        .into_iter(),
    )
    .expect("cook");
    assert!(none.manifest.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_canopy_dag_hands_off_to_cheaper_voxels_with_one_surface_at_every_distance() {
    // The case the proxy exists for: leaves the simplifier cannot merge.
    let mesh = canopy(3000);
    let dag = build_meshlet_mesh_with_attributes(
        &mesh.positions,
        Some(MeshletAttributes {
            normals: &mesh.normals,
            uvs: &mesh.uvs,
        }),
        &mesh.indices,
        &MeshletSettings::default(),
    )
    .expect("dag");
    let proxy =
        voxelize("canopy", &input(&mesh), &VoxelFarCookSettings { resolution: 32 }).expect("voxelize");
    let vertices = lab_vertices(&mesh);
    let far = proxy
        .extend_meshlets(&dag, &vertices, &VoxelFarSettings::default())
        .expect("extend");
    assert!(!far.levels.is_empty());
    let first = far.levels[0];
    assert!(first.triangles < far.root_triangles);
    // The leaves keep green: the voxel corners carry the prefiltered vertex color.
    let appended = &far.vertices[vertices.len()..];
    assert!(
        appended
            .iter()
            .all(|v| (v.color[1] - 0.5).abs() < 0.01 && v.color[2] < 0.2)
    );

    let ppu = 1440.0 / (2.0 * (0.5f32).tan());
    let base_levels = first.dag_level;
    let mut switched_at = None;
    let mut distance = 1.0f32;
    while distance < 20_000.0 {
        let view = LodView {
            eye: [0.0, 0.3, distance],
            pixels_per_unit: ppu,
            threshold_px: 1.0,
            near: 0.1,
        };
        let cut: Vec<usize> = far.meshlets.lod_cut(&view).collect();
        let voxel_levels: std::collections::BTreeSet<u32> = cut
            .iter()
            .map(|&i| far.meshlets.meshlets[i].level)
            .filter(|&l| l >= base_levels)
            .collect();
        let triangle_clusters = cut
            .iter()
            .filter(|&&i| far.meshlets.meshlets[i].level < base_levels)
            .count();
        // The triangle part of the cut is exactly the DAG's own cut while no voxel level draws.
        let own: Vec<usize> = dag.lod_cut(&view).collect();
        if voxel_levels.is_empty() {
            assert_eq!(triangle_clusters, own.len(), "at {distance}");
        } else {
            assert_eq!(
                triangle_clusters, 0,
                "at {distance}: voxels and triangles together"
            );
            assert_eq!(voxel_levels.len(), 1, "at {distance}: two voxel levels");
            switched_at.get_or_insert(distance);
        }
        distance *= 1.02;
    }
    let switched_at = switched_at.expect("the voxels take over");
    let radius = far.meshlets.meshlets[cut_root(&far.meshlets, base_levels)].parent[3];
    let predicted = VoxelFarProxy::switch_distance(first.error, radius, ppu, 1.0);
    assert!(
        (switched_at / predicted - 1.0).abs() < 0.03,
        "switched at {switched_at}, predicted {predicted}"
    );

    // Perf note (printed; `--nocapture`): cost and memory per asset.
    let lab_bytes = std::mem::size_of::<LabVertex>();
    let cluster_bytes = std::mem::size_of::<regia_render::visbuffer::Meshlet>();
    let voxel_clusters: u32 = far.levels.iter().map(|l| l.clusters).sum();
    let voxel_vertices: u32 = far.levels.iter().map(|l| l.vertices).sum();
    let gpu_words = u64::from(voxel_vertices) * 16
        + (far.meshlets.vertices.len() - dag.vertices.len()) as u64
        + (far.meshlets.triangles.len() - dag.triangles.len()) as u64;
    println!(
        "voxel_far perf: source {} tris, DAG {} levels, root cut {} tris; blob {} B ({} voxels, {} bricks); \
         extension {} levels, {} clusters, {} vertices ({} B as LabVertex, {} B on the GPU arena incl. lists, \
         {} B cluster records); switch at {:.0} m for a {:.2} m sphere at 1440p",
        mesh.indices.len() / 3,
        dag.level_count(),
        far.root_triangles,
        proxy.byte_size(),
        proxy.voxels.len(),
        proxy.bricks.len(),
        far.levels.len(),
        voxel_clusters,
        voxel_vertices,
        voxel_vertices as usize * lab_bytes,
        gpu_words * 4,
        voxel_clusters as usize * cluster_bytes,
        predicted,
        radius,
    );
    for l in &far.levels {
        println!(
            "voxel_far level {} (dag {}): {} tris, {} clusters, error {:.4}, switch {:.0} m",
            l.level,
            l.dag_level,
            l.triangles,
            l.clusters,
            l.error,
            VoxelFarProxy::switch_distance(l.error, radius, ppu, 1.0)
        );
    }
}

fn cut_root(mesh: &regia_render::visbuffer::MeshletMesh, base_levels: u32) -> usize {
    mesh.meshlets
        .iter()
        .position(|m| m.level >= base_levels)
        .unwrap_or(0)
}

#[test]
fn scattered_leaves_are_partly_transparent_and_a_closed_surface_is_opaque() {
    // A sphere shell: every voxel's surface is nearly planar, so it stops every ray.
    let sphere = voxelize(
        "sphere",
        &input(&uv_sphere(24, 48)),
        &VoxelFarCookSettings { resolution: 16 },
    )
    .expect("voxelize");
    let mean = |p: &VoxelFarProxy, level: usize| {
        let (sum, n) = p
            .level_voxels(level)
            .fold((0.0f32, 0usize), |(s, n), (_, v)| (s + v.opacity(), n + 1));
        sum / n.max(1) as f32
    };
    assert!(
        mean(&sphere, 0) > 0.9,
        "sphere shell opacity {}",
        mean(&sphere, 0)
    );
    // Randomly turned leaves: one leaf fragment per voxel at level 0 is planar (opaque); coarser
    // voxels hold leaves of many orientations and let part of the view rays through.
    let leaves = voxelize(
        "leaves",
        &input(&random_leaves(4000)),
        &VoxelFarCookSettings { resolution: 32 },
    )
    .expect("voxelize");
    let means: Vec<f32> = (0..leaves.levels.len()).map(|l| mean(&leaves, l)).collect();
    println!("canopy opacity by level: {means:?}");
    assert!(
        means.iter().any(|&m| m < 0.85),
        "some level is partly transparent: {means:?}"
    );
}

/// `count` leaf quads (0.08 m) at random points of a 2 m cube, each turned to a random normal.
fn random_leaves(count: u32) -> regia_asset::CpuMesh {
    let mut state = 0x9e37_79b9u32;
    let mut next = || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
    };
    let mut mesh = regia_asset::CpuMesh::default();
    for _ in 0..count {
        let c = [next(), next(), next()];
        let n = [next(), next(), next()];
        let l = dot(n, n).sqrt().max(1e-3);
        let n = n.map(|v| v / l);
        let a = if n[0].abs() < 0.9 {
            [1.0, 0.0, 0.0]
        } else {
            [0.0, 1.0, 0.0]
        };
        let t = cross(n, a);
        let tl = dot(t, t).sqrt();
        let t = t.map(|v| v / tl * 0.04);
        let b = cross(n, t);
        let base = mesh.positions.len() as u32;
        for (u, v) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            mesh.positions
                .push(std::array::from_fn(|k| c[k] + t[k] * u + b[k] * v));
            mesh.normals.push(n);
        }
        mesh.indices
            .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    mesh
}

/// `proxy` as format v1 drew it: every voxel a whole cube, each level's error the cube diagonal.
fn as_whole_cubes(proxy: &VoxelFarProxy) -> VoxelFarProxy {
    let mut cubes = proxy.clone();
    cubes.extents.clear();
    cubes.sampled_errors.clear();
    for level in &mut cubes.levels {
        level.error = level.voxel_size * DIAGONAL;
    }
    cubes
}

/// Format v2 (ADR-2655): every surface point lies inside a box of every level, and a level's
/// error is its boxes' bound: never above the cube diagonal v1 used, below it everywhere here.
#[test]
fn sub_voxel_boxes_cover_the_surface_and_tighten_the_error() {
    let quad = regia_asset::CpuMesh {
        positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 1.0, 0.0], [0.0, 1.0, 0.0]],
        normals: vec![[0.0, 0.0, 1.0]; 4],
        indices: vec![0, 1, 2, 0, 2, 3],
        ..Default::default()
    };
    for (name, mesh, resolution) in [
        ("sphere", uv_sphere(24, 48), 16),
        ("canopy", canopy(400), 32),
        ("quad", quad, 4),
    ] {
        let proxy = voxelize(name, &input(&mesh), &VoxelFarCookSettings { resolution }).expect("voxelize");
        proxy.validate().expect("valid");
        assert_eq!(proxy.extents.len(), proxy.voxels.len(), "{name}: boxes stored");
        // Surface samples: vertices, edge midpoints and centroids.
        let mut samples: Vec<[f32; 3]> = Vec::new();
        for t in mesh.indices.as_chunks::<3>().0 {
            let v = t.map(|i| mesh.positions[i as usize]);
            samples.extend_from_slice(&v);
            for (a, b) in [(0, 1), (1, 2), (2, 0)] {
                samples.push(std::array::from_fn(|k| (v[a][k] + v[b][k]) * 0.5));
            }
            samples.push(std::array::from_fn(|k| (v[0][k] + v[1][k] + v[2][k]) / 3.0));
        }
        for (level, info) in proxy.levels.iter().enumerate() {
            let size = info.voxel_size;
            assert!(info.error <= size * DIAGONAL * (1.0 + 1e-6), "{name} {level}");
            assert!(info.error >= proxy.level_extent_error(level) * (1.0 - 1e-4));
            assert!(
                info.error < size * DIAGONAL * 0.9,
                "{name} level {level}: error {} vs diagonal {}",
                info.error,
                size * DIAGONAL
            );
            let boxes: std::collections::HashMap<[u32; 3], ([f32; 3], [f32; 3])> = proxy
                .level_voxel_extents(level)
                .map(|(xyz, _, packed)| {
                    let (lo, hi) = regia_render::visbuffer::unpack_voxel_extent(packed);
                    let at = |k: usize, q: u32| proxy.origin[k] + (xyz[k] as f32 + q as f32 / 15.0) * size;
                    (
                        xyz,
                        (
                            std::array::from_fn(|k| at(k, lo[k])),
                            std::array::from_fn(|k| at(k, hi[k])),
                        ),
                    )
                })
                .collect();
            let eps = size * 1e-3;
            for p in &samples {
                let cell = |k: usize, d: f32| ((p[k] + d - proxy.origin[k]) / size).floor().max(0.0) as u32;
                let inside = (0..8).any(|corner: u32| {
                    let xyz: [u32; 3] =
                        std::array::from_fn(|k| cell(k, if (corner >> k) & 1 == 1 { eps } else { -eps }));
                    boxes
                        .get(&xyz)
                        .is_some_and(|(lo, hi)| (0..3).all(|k| p[k] >= lo[k] - eps && p[k] <= hi[k] + eps))
                });
                assert!(inside, "{name} level {level}: {p:?} outside every box");
            }
        }
        println!(
            "voxel_far boxes {name}: level errors / cube diagonal {:?}",
            proxy
                .levels
                .iter()
                .map(|l| format!("{:.3}", l.error / (l.voxel_size * DIAGONAL)))
                .collect::<Vec<_>>()
        );
    }
}

/// The counted margin on the canopy (the GPU differential in `regia_render`'s
/// `visbuffer_voxel_far` checks the silhouette): the first voxel level the cut takes switches
/// nearer with the boxes than as whole cubes, by at least the full-box bound's 13 %.
#[test]
fn boxes_move_the_canopy_switch_nearer() {
    let mesh = canopy(3000);
    let dag = build_meshlet_mesh_with_attributes(
        &mesh.positions,
        Some(MeshletAttributes {
            normals: &mesh.normals,
            uvs: &mesh.uvs,
        }),
        &mesh.indices,
        &MeshletSettings::default(),
    )
    .expect("dag");
    let boxes =
        voxelize("canopy", &input(&mesh), &VoxelFarCookSettings { resolution: 32 }).expect("voxelize");
    let cubes = as_whole_cubes(&boxes);
    cubes.validate().expect("v1-shaped proxy is valid");
    let vertices = lab_vertices(&mesh);
    let settings = VoxelFarSettings::default();
    let tight = boxes.extend_meshlets(&dag, &vertices, &settings).expect("extend");
    let loose = cubes.extend_meshlets(&dag, &vertices, &settings).expect("extend");
    let (t, l) = (tight.levels[0], loose.levels[0]);
    let radius = dag.bounds[3];
    let ppu = 1440.0 / (2.0 * (0.5f32).tan());
    let (near, far) = (
        VoxelFarProxy::switch_distance(t.error, radius, ppu, 1.0),
        VoxelFarProxy::switch_distance(l.error, radius, ppu, 1.0),
    );
    println!(
        "voxel_far switch: boxes level {} ({:?}, {} tris, error {:.4}) at {near:.1} m, cubes level {} ({} tris, \
         error {:.4}) at {far:.1} m: {:.1} m ({:.1} %) nearer",
        t.level,
        t.shape,
        t.triangles,
        t.error,
        l.level,
        l.triangles,
        l.error,
        far - near,
        (1.0 - near / far) * 100.0
    );
    assert!(t.level <= l.level, "no coarser level takes over");
    assert!(near < far * 0.95, "{near} vs {far}");
}
