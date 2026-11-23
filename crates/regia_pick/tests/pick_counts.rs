// SPDX-License-Identifier: MPL-2.0
#![cfg(debug_assertions)]
//! Operation-count regressions for the CPU pick path. The thread-local counters
//! ([`regia_pick::stats`]) pin the audit findings on the current morph/dual-quaternion design:
//! an off-ray skinned mesh must not be skinned, the caches sweep once per query, a nearer hit
//! skips the far BVH, the terrain march walks the min/max pyramid, the world walk visits mesh
//! archetypes (not every entity), and a rigid cast inverts the model once.
//!
//! The counters only count when `debug_assertions` is on ([`regia_pick::stats`]), so under
//! `cargo test --release` every counter reads 0 and these assertions cannot hold; the file
//! compiles to nothing there.

use std::sync::Arc;

use regia_core::glam::Affine3A;
use regia_core::math::{Ray, Vec3};
use regia_ecs::prelude::*;
use regia_pick::{FaceCull, PickFilter, bvh_for, raycast_mesh_entity, world_pick};
use regia_render::{CpuMesh, JointMatrix, LabMaterial, LabVertex, MeshRenderer, SkinJoints, box_mesh};
use regia_scene::GlobalTransform;
use regia_terrain::{CdlodConfig, MinMaxPyramid, Terrain, TerrainLayers, TerrainMap};

/// The counting tests read this thread's counters; they never run picks on other threads.
fn counts() -> regia_pick::PickCounts {
    regia_pick::pick_counts()
}

/// A rigid slab (12 triangles, so it casts brute force).
fn slab(world: &mut World, center: Vec3) -> Entity {
    let (vertices, indices) = box_mesh([1.0; 3]);
    world
        .spawn((
            MeshRenderer::new(
                Arc::new(CpuMesh { vertices, indices }),
                Arc::new(LabMaterial::default()),
            ),
            GlobalTransform(Affine3A::from_translation(center)),
        ))
        .id()
}

/// A 128-triangle grid in the z = 0 plane, 8 m across: big enough for the BVH.
fn grid_mesh() -> CpuMesh {
    let n = 9;
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    for z in 0..n {
        for x in 0..n {
            vertices.push(LabVertex::from_pos_normal_uv(
                [x as f32, z as f32, 0.0],
                [0.0, 0.0, 1.0],
                [0.0, 0.0],
            ));
        }
    }
    for z in 0..n - 1 {
        for x in 0..n - 1 {
            let a = (z * n + x) as u32;
            let b = a + 1;
            let c = a + n as u32;
            let d = c + 1;
            indices.extend([a, b, d, a, d, c]);
        }
    }
    CpuMesh { vertices, indices }
}

/// A flat 129x129 terrain, height 0 except a 16 m block, 10 m tall, just -x/-z of the center
/// (samples 56..64, world -16..-2).
fn blocked_terrain(world: &mut World) -> Entity {
    let n = 129usize;
    let mut heights = vec![0.0_f32; n * n];
    // A 16x16 m block just -x/-z of the center (samples 56..64, world -16..-2 on this 256 m
    // map; samples are 2 m apart).
    for z in 56..64 {
        for x in 56..64 {
            heights[z * n + x] = 10.0;
        }
    }
    let map = TerrainMap::from_heights(n as u32, n as u32, heights, 256.0, 1.0, 0.0).expect("map");
    let minmax = MinMaxPyramid::build(&map, 4).expect("pyramid");
    world
        .spawn(Terrain {
            map,
            cdlod: CdlodConfig::default(),
            root_level: 4,
            minmax,
            layers: TerrainLayers::default(),
            holes: None,
            water: None,
        })
        .id()
}

#[test]
fn an_off_ray_skinned_mesh_is_not_skinned_and_the_buffer_is_reused() {
    let mut world = World::new();
    let (vertices, indices) = box_mesh([0.5; 3]);
    let mut mesh = CpuMesh { vertices, indices };
    for v in &mut mesh.vertices {
        v.joints = [0, 0, 0, 0];
        v.weights = [1.0, 0.0, 0.0, 0.0];
    }
    world.spawn((
        MeshRenderer::new(Arc::new(mesh), Arc::new(LabMaterial::default())),
        SkinJoints {
            palette: vec![JointMatrix::translation(3.0, 0.0, 0.0)],
        },
        GlobalTransform(Affine3A::IDENTITY),
    ));

    let _ = regia_pick::reset_pick_counts();
    // A ray straight down at x = 0: the posed box is at x = 3 and the ray misses it.
    assert!(
        world_pick(
            &world,
            Ray::new(Vec3::new(0.0, 5.0, 0.0), Vec3::NEG_Y),
            &PickFilter::default()
        )
        .is_none()
    );
    let miss = counts();
    assert_eq!(
        miss.skin_evaluations, 0,
        "an off-ray skinned mesh is rejected before any skinning: {miss:?}"
    );

    // On-ray casts skin; the scratch buffer does not grow on the repeat cast.
    let _ = regia_pick::reset_pick_counts();
    let hit = world_pick(
        &world,
        Ray::new(Vec3::new(3.0, 5.0, 0.0), Vec3::NEG_Y),
        &PickFilter::default(),
    )
    .expect("hit");
    assert!((hit.point.y - 0.5).abs() < 1e-3, "{:?}", hit.point);
    let first = counts();
    assert!(
        first.skin_evaluations > 0,
        "an on-ray cast still skins the mesh: {first:?}"
    );
    let _ = regia_pick::reset_pick_counts();
    let again = world_pick(
        &world,
        Ray::new(Vec3::new(3.0, 5.0, 0.0), Vec3::NEG_Y),
        &PickFilter::default(),
    )
    .expect("hit again");
    assert_eq!(again.entity, hit.entity);
    let second = counts();
    assert_eq!(
        second.skin_buffer_grows, 0,
        "the repeat cast reuses the thread-local scratch buffer: {second:?}"
    );
}

#[test]
fn a_query_sweeps_the_pick_caches_once() {
    let mut world = World::new();
    let (vertices, indices) = box_mesh([1.0; 3]);
    let mesh = Arc::new(CpuMesh { vertices, indices });
    for index in 0..24 {
        world.spawn((
            MeshRenderer::new(Arc::clone(&mesh), Arc::new(LabMaterial::default())),
            GlobalTransform(Affine3A::from_translation(Vec3::new(index as f32, 0.0, 20.0))),
        ));
    }
    let _ = regia_pick::reset_pick_counts();
    let pick = world_pick(
        &world,
        Ray::new(Vec3::new(0.0, 0.5, 30.0), Vec3::NEG_Z),
        &PickFilter::default(),
    )
    .expect("hit");
    assert_eq!(pick.point.z, 21.0, "{:?}", pick.point);
    let after = counts();
    assert!(
        after.cache_sweeps <= 2,
        "one query sweeps the bounds and BVH caches at most once each: {after:?}"
    );
}

#[test]
fn a_nearer_mesh_skips_the_far_bvh() {
    let mut world = World::new();
    // The far mesh is spawned first: without a nearest-first walk and a running distance cap
    // its BVH is walked in full before the near triangle is even cast.
    let grid = Arc::new(grid_mesh());
    let far = world
        .spawn((
            MeshRenderer::new(Arc::clone(&grid), Arc::new(LabMaterial::default())),
            GlobalTransform(Affine3A::from_translation(Vec3::new(-4.0, -4.0, 5.0))),
        ))
        .id();
    let mut tri = CpuMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv([0.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0]),
            LabVertex::from_pos_normal_uv([2.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0]),
            LabVertex::from_pos_normal_uv([0.0, 2.0, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0]),
        ],
        indices: vec![0, 1, 2],
    };
    for v in &mut tri.vertices {
        v.joints = [0, 0, 0, 0];
        v.weights = [1.0, 0.0, 0.0, 0.0];
    }
    // The ray starts at z = 10, so the near triangle sits at z = 8 (distance 2) and the far
    // grid at z = 5 (distance 5).
    let near = world
        .spawn((
            MeshRenderer::new(Arc::new(tri), Arc::new(LabMaterial::default())),
            GlobalTransform(Affine3A::from_translation(Vec3::new(0.0, 0.0, 8.0))),
        ))
        .id();
    let down = Ray::new(Vec3::new(0.25, 0.25, 10.0), Vec3::NEG_Z);

    let _ = regia_pick::reset_pick_counts();
    let hit = world_pick(&world, down, &PickFilter::default()).expect("near hit");
    assert_eq!(hit.entity, near);
    let with_near = counts();
    assert_eq!(
        with_near.bvh_nodes, 0,
        "a nearer hit rules out the far mesh's BVH before any node is visited: {with_near:?}"
    );

    world.despawn(near);
    let _ = regia_pick::reset_pick_counts();
    let far_hit = world_pick(&world, down, &PickFilter::default()).expect("far hit");
    assert_eq!(far_hit.entity, far);
    assert!(
        counts().bvh_nodes > 0,
        "without the near mesh the far BVH is walked and still hits"
    );
}

#[test]
fn a_ray_over_flat_ground_skips_the_heightfield_with_the_pyramid() {
    let mut world = World::new();
    blocked_terrain(&mut world);
    // Parallel to the ground at y = 5, far from the block: the ray stays inside the map's
    // height range, so the old march sampled the whole span at half spacing.
    let ray = Ray::new(Vec3::new(-120.0, 5.0, -64.0), Vec3::X);
    let _ = regia_pick::reset_pick_counts();
    assert!(
        world_pick(&world, ray, &PickFilter::default()).is_none(),
        "y = 5 clears the flat ground and passes beside the block"
    );
    let miss = counts();
    assert!(
        miss.terrain_nodes >= 1 && miss.terrain_nodes <= 80,
        "the march walks the min/max pyramid: {miss:?}"
    );
    assert!(
        miss.terrain_samples < 32,
        "a ray that crosses no leaf samples almost nothing: {miss:?}"
    );

    // Onto the block the ray still hits at the raised height (the block spans samples 56..64,
    // world -16..0 on this 256 m map).
    let down = Ray::new(Vec3::new(-8.0, 30.0, -8.0), Vec3::NEG_Y);
    let hit = world_pick(&world, down, &PickFilter::default()).expect("block hit");
    assert!(
        hit.point.y > 5.0 && hit.point.y < 20.0,
        "the block is hit at its height: {:?}",
        hit.point
    );
}

#[test]
fn world_pick_visits_meshes_not_the_whole_entity_list() {
    let mut world = World::new();
    for _ in 0..2000 {
        world.spawn_empty();
    }
    let floor = slab(&mut world, Vec3::new(0.0, 0.0, 0.0));
    let _ = regia_pick::reset_pick_counts();
    let hit = world_pick(
        &world,
        Ray::new(Vec3::new(0.0, 5.0, 0.0), Vec3::NEG_Y),
        &PickFilter::default(),
    )
    .expect("hit");
    assert_eq!(hit.entity, floor);
    let after = counts();
    assert!(
        after.entity_visits <= 16,
        "the walk visits mesh and terrain archetypes, not 2000 empty entities: {after:?}"
    );
}

#[test]
fn a_rigid_cast_inverts_the_model_once() {
    let mut world = World::new();
    let floor = slab(&mut world, Vec3::new(0.0, 0.0, 0.0));
    let _ = regia_pick::reset_pick_counts();
    let hit = raycast_mesh_entity(
        &world,
        floor,
        Vec3::new(0.0, 5.0, 0.0),
        Vec3::NEG_Y,
        FaceCull::None,
    )
    .expect("hit");
    assert!((hit.distance - 4.0).abs() < 1e-3, "{}", hit.distance);
    let after = counts();
    assert_eq!(
        after.matrix_inverses, 1,
        "one rigid cast inverts the model once (bounds, cast and normals share it): {after:?}"
    );
}

/// The BVH cache and the entity cast agree on the grid mesh (the near/far test's far mesh).
#[test]
fn the_grid_mesh_casts_through_its_bvh() {
    let mut world = World::new();
    let grid = Arc::new(grid_mesh());
    assert!(bvh_for(&grid).is_some(), "128 triangles build a BVH");
    let entity = world
        .spawn((
            MeshRenderer::new(Arc::clone(&grid), Arc::new(LabMaterial::default())),
            GlobalTransform(Affine3A::from_translation(Vec3::new(-4.0, -4.0, 5.0))),
        ))
        .id();
    let hit = world_pick(
        &world,
        Ray::new(Vec3::new(0.25, 0.25, 10.0), Vec3::NEG_Z),
        &PickFilter::default(),
    )
    .expect("hit");
    assert_eq!(hit.entity, entity);
    assert!((hit.distance - 5.0).abs() < 1e-3);
}
