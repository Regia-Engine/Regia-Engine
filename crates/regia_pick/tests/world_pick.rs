// SPDX-License-Identifier: MPL-2.0
//! `world_pick` over a synthetic stack of floors: the right layer, layer filters, a terrain hole
//! the ray passes through, hierarchy-inherited layers, skinned meshes and the BVH agreeing with
//! the brute-force cast. No GPU and no game maps.

use std::sync::Arc;

use regia_core::glam::{Affine3A, Quat};
use regia_core::math::{Mat4, Ray, Vec3};
use regia_ecs::prelude::*;
use regia_pick::{
    FaceCull, LayerSet, MapLayer, PickFilter, PickSurface, bvh_for, map_layer_of, raycast_mesh,
    raycast_mesh_bvh, raycast_mesh_entity, raycast_meshes, raycast_terrain, world_pick, world_pick_layers,
};
use regia_render::{
    CpuMesh, JointMatrix, LabMaterial, LabVertex, MeshMorphTargets, MeshRenderer, MorphMode, MorphTargets,
    SkinInfluences8, SkinJoints, SkinningMethod, SkinningOptions, box_mesh, skin_vertex_reference,
};
use regia_scene::GlobalTransform;
use regia_scene::components::MorphWeights;
use regia_terrain::{CdlodConfig, HoleShape, HolesMask, MinMaxPyramid, Terrain, TerrainLayers, TerrainMap};

fn flat_terrain(world: &mut World, holes: Option<HolesMask>) -> Entity {
    let n = 129;
    let map = TerrainMap::from_heights(n, n, vec![0.0; (n * n) as usize], 256.0, 1.0, 0.0).expect("map");
    let minmax = MinMaxPyramid::build(&map, 4).expect("pyramid");
    world
        .spawn(Terrain {
            map,
            cdlod: CdlodConfig::default(),
            root_level: 4,
            minmax,
            layers: TerrainLayers::default(),
            holes,
            water: None,
        })
        .id()
}

/// A flat slab (top face at `top`) 40 m across.
fn floor(world: &mut World, top: f32, layer: Option<u8>) -> Entity {
    let (vertices, indices) = box_mesh([20.0, 0.1, 20.0]);
    let mesh = Arc::new(CpuMesh { vertices, indices });
    let mut entity = world.spawn((
        MeshRenderer::new(mesh, Arc::new(LabMaterial::default())),
        GlobalTransform(Affine3A::from_translation(Vec3::new(0.0, top - 0.1, 0.0))),
    ));
    if let Some(layer) = layer {
        entity.insert(MapLayer(layer));
    }
    entity.id()
}

fn down(x: f32, z: f32) -> Ray {
    Ray::new(Vec3::new(x, 50.0, z), Vec3::NEG_Y)
}

#[test]
fn stacked_floors_return_the_right_layer() {
    let mut world = World::new();
    let ground = flat_terrain(&mut world, None);
    let deck1 = floor(&mut world, 5.0, Some(1));
    let deck2 = floor(&mut world, 10.0, Some(2));

    let top = world_pick(&world, down(0.0, 0.0), &PickFilter::default()).expect("hit");
    assert_eq!(
        (top.layer, top.entity, top.surface),
        (MapLayer(2), deck2, PickSurface::Mesh)
    );
    assert!((top.point.y - 10.0).abs() < 1e-3 && (top.distance - 40.0).abs() < 1e-3);
    assert!(top.normal.dot(Vec3::Y) > 0.99);

    let mid = world_pick(&world, down(0.0, 0.0), &PickFilter::on_layers(&[0, 1])).expect("hit");
    assert_eq!((mid.layer, mid.entity), (MapLayer(1), deck1));

    let low = world_pick(&world, down(0.0, 0.0), &PickFilter::on_layers(&[0])).expect("hit");
    assert_eq!(
        (low.layer, low.entity, low.surface),
        (MapLayer(0), ground, PickSurface::Terrain)
    );
    assert!(low.point.y.abs() < 1e-3 && low.normal.dot(Vec3::Y) > 0.99 && low.triangle.is_none());

    // Off the decks (they are 40 m across): terrain.
    let beside = world_pick(&world, down(60.0, 0.0), &PickFilter::default()).expect("hit");
    assert_eq!(beside.layer, MapLayer(0));

    // Nothing on an unused layer, and the distance cap applies.
    assert!(world_pick(&world, down(0.0, 0.0), &PickFilter::on_layers(&[7])).is_none());
    let capped = PickFilter {
        max_distance: 30.0,
        ..PickFilter::default()
    };
    assert!(world_pick(&world, down(0.0, 0.0), &capped).is_none());

    // Every floor the ray crosses, nearest first.
    let all = world_pick_layers(&world, down(0.0, 0.0), &PickFilter::default());
    assert_eq!(all.iter().map(|p| p.layer.0).collect::<Vec<_>>(), vec![2, 1, 0]);
    assert!(all.windows(2).all(|w| w[0].distance < w[1].distance));

    // An upward ray from below hits the underside of the nearest deck, back faces included.
    let up = Ray::new(Vec3::new(0.0, -5.0, 0.0), Vec3::Y);
    assert_eq!(
        world_pick(&world, up, &PickFilter::default()).map(|p| p.layer),
        Some(MapLayer(1))
    );
    // Seen from above the deck's top is a front face, so culling back faces keeps the hit.
    let culled = PickFilter {
        cull: FaceCull::Back,
        ..PickFilter::default()
    };
    assert_eq!(
        world_pick(&world, down(0.0, 0.0), &culled).map(|p| p.layer),
        Some(MapLayer(2))
    );
}

#[test]
fn the_ray_passes_through_a_terrain_hole() {
    let mut world = World::new();
    let mask = HolesMask::from_shapes(vec![HoleShape::Rect {
        center: [0.0, 0.0],
        half_extents: [10.0, 10.0],
        rotation_radians: 0.0,
    }])
    .expect("mask");
    let ground = flat_terrain(&mut world, Some(mask));
    // The tunnel floor below the mouth, layer 1.
    let tunnel = floor(&mut world, -6.0, Some(1));

    let through = world_pick(&world, down(0.0, 0.0), &PickFilter::default()).expect("hit");
    assert_eq!((through.layer, through.entity), (MapLayer(1), tunnel));
    assert!((through.point.y + 6.0).abs() < 1e-3);

    // Layer 0 only: the hole leaves nothing to hit.
    assert!(world_pick(&world, down(0.0, 0.0), &PickFilter::on_layers(&[0])).is_none());

    // Outside the mouth the terrain is solid and hides the tunnel floor.
    let outside = world_pick(&world, down(15.0, 0.0), &PickFilter::default()).expect("hit");
    assert_eq!((outside.layer, outside.entity), (MapLayer(0), ground));

    // A slanted ray that enters the mouth and would cross the surface inside the hole passes too;
    // one that crosses the surface outside the hole hits the terrain.
    let into = Ray::new(Vec3::new(-8.0, 10.0, 0.0), Vec3::new(0.2, -1.0, 0.0));
    assert_eq!(
        world_pick(&world, into, &PickFilter::default()).map(|p| p.layer),
        Some(MapLayer(1))
    );
    let short = Ray::new(Vec3::new(-14.0, 10.0, 0.0), Vec3::new(0.2, -1.0, 0.0));
    assert_eq!(
        world_pick(&world, short, &PickFilter::default()).map(|p| p.layer),
        Some(MapLayer(0))
    );
}

#[test]
fn an_unlinked_holes_handle_is_ignored() {
    use regia_terrain::TerrainHoles;
    let mut world = World::new();
    flat_terrain(&mut world, None);
    // No linked draw: the handle is ignored and the terrain is solid.
    world.insert_resource(TerrainHoles::default());
    assert_eq!(
        world_pick(&world, down(0.0, 0.0), &PickFilter::default()).map(|p| p.layer),
        Some(MapLayer(0))
    );
}

#[test]
fn layers_follow_the_hierarchy_and_default_to_zero() {
    let mut world = World::new();
    let parent = world.spawn(MapLayer(3)).id();
    let (vertices, indices) = box_mesh([1.0; 3]);
    let mesh = Arc::new(CpuMesh { vertices, indices });
    let child = world
        .spawn((
            MeshRenderer::new(mesh, Arc::new(LabMaterial::default())),
            GlobalTransform(Affine3A::IDENTITY),
            ChildOf(parent),
        ))
        .id();
    let stray = world.spawn_empty().id();
    assert_eq!(map_layer_of(&world, child), MapLayer(3));
    assert_eq!(map_layer_of(&world, stray), MapLayer(0));
    let hit = world_pick(&world, down(0.0, 0.0), &PickFilter::on_layers(&[3])).expect("hit");
    assert_eq!((hit.entity, hit.layer), (child, MapLayer(3)));
    assert!(world_pick(&world, down(0.0, 0.0), &PickFilter::on_layers(&[0])).is_none());
}

#[test]
fn hidden_and_skipped_entities_are_ignored() {
    use regia_scene::prelude::Visibility;
    let mut world = World::new();
    let deck = floor(&mut world, 5.0, Some(1));
    let under = floor(&mut world, 0.0, Some(0));
    world.entity_mut(deck).insert(Visibility::Hidden);
    assert_eq!(
        world_pick(&world, down(0.0, 0.0), &PickFilter::default()).map(|p| p.entity),
        Some(under)
    );
    let with_hidden = PickFilter {
        include_hidden: true,
        ..PickFilter::default()
    };
    assert_eq!(
        world_pick(&world, down(0.0, 0.0), &with_hidden).map(|p| p.entity),
        Some(deck)
    );
    let skipping = PickFilter {
        include_hidden: true,
        skip: vec![deck],
        ..PickFilter::default()
    };
    assert_eq!(
        world_pick(&world, down(0.0, 0.0), &skipping).map(|p| p.entity),
        Some(under)
    );
}

#[test]
fn skinned_meshes_pick_where_the_palette_puts_them() {
    let mut world = World::new();
    let (vertices, indices) = box_mesh([0.5; 3]);
    let mut mesh = CpuMesh { vertices, indices };
    for v in &mut mesh.vertices {
        v.joints = [0, 1, 0, 0];
        v.weights = [0.5, 0.5, 0.0, 0.0];
    }
    let bone = world
        .spawn((
            MeshRenderer::new(Arc::new(mesh), Arc::new(LabMaterial::default())),
            SkinJoints {
                palette: vec![
                    JointMatrix::translation(4.0, 0.0, 0.0),
                    JointMatrix::translation(2.0, 0.0, 0.0),
                ],
            },
            GlobalTransform(Affine3A::from_translation(Vec3::Y)),
            MapLayer(4),
        ))
        .id();
    // The skinned box sits at x = 3, y = 1: a ray at x = 3 hits it, one at the bind pose does not.
    let hit = world_pick(
        &world,
        Ray::new(Vec3::new(3.0, 1.0, 5.0), Vec3::NEG_Z),
        &PickFilter::default(),
    )
    .expect("hit");
    assert_eq!((hit.entity, hit.layer), (bone, MapLayer(4)));
    assert!((hit.point - Vec3::new(3.0, 1.0, 0.5)).length() < 1e-4);
    assert!(hit.normal.dot(Vec3::Z) > 0.99);
    assert!(
        world_pick(
            &world,
            Ray::new(Vec3::new(0.0, 1.0, 5.0), Vec3::NEG_Z),
            &PickFilter::default()
        )
        .is_none()
    );
}

#[test]
fn a_rebase_keeps_the_world_pick_in_local_coordinates() {
    let mut world = World::new();
    flat_terrain(&mut world, None);
    let offset = Vec3::new(2048.0, 0.0, 0.0);
    world.insert_resource(regia_scene::streaming::OriginOffset(offset.as_dvec3().to_array()));
    // The schema's world-looking origin is local here, so it samples the heightfield at world x = 2048 and misses.
    assert!(
        world_pick(
            &world,
            Ray::new(Vec3::ZERO + Vec3::Y * 50.0, Vec3::NEG_Y),
            &PickFilter::default()
        )
        .is_none()
    );
    let local = Vec3::new(-2048.0, 50.0, 0.0);
    let hit = world_pick(&world, Ray::new(local, Vec3::NEG_Y), &PickFilter::default()).expect("local ray");
    assert_eq!(hit.surface, PickSurface::Terrain);
    assert!((hit.point.x - local.x).abs() < 1.0, "{:?}", hit.point);
    assert!(hit.point.y.abs() < 1e-2, "{:?}", hit.point);

    // A mesh's GlobalTransform is already local, so the same local origin hits it and the point stays local.
    let deck = floor(&mut world, 5.0, Some(1));
    let mesh = world_pick(
        &world,
        Ray::new(Vec3::new(0.0, 50.0, 0.0), Vec3::NEG_Y),
        &PickFilter::default(),
    )
    .expect("mesh");
    assert_eq!(mesh.entity, deck);
    assert!(mesh.point.x.abs() < 1e-3, "{:?}", mesh.point);
}

#[test]
fn an_upward_terrain_ray_misses_for_either_cull() {
    let mut world = World::new();
    flat_terrain(&mut world, None);
    let up = Ray::new(Vec3::new(0.0, -10.0, 0.0), Vec3::Y);
    let down = Ray::new(Vec3::new(0.0, 50.0, 0.0), Vec3::NEG_Y);
    for cull in [FaceCull::None, FaceCull::Back] {
        let filter = PickFilter {
            cull,
            ..PickFilter::default()
        };
        assert!(world_pick(&world, up, &filter).is_none(), "{cull:?}");
        let hit = world_pick(&world, down, &filter).expect("downward");
        assert_eq!(hit.surface, PickSurface::Terrain, "{cull:?}");
    }
}

#[test]
fn the_bvh_matches_the_brute_force_cast() {
    // A bumpy 40x40 grid (3200 triangles), posed under a rotation and a mirror.
    let n = 41_usize;
    let mut vertices = Vec::new();
    let template = box_mesh([0.5; 3]).0[0];
    for z in 0..n {
        for x in 0..n {
            let mut v = template;
            let h = ((x * 7 + z * 13) % 11) as f32 * 0.07;
            v.position = [x as f32 - 20.0, h, z as f32 - 20.0];
            vertices.push(v);
        }
    }
    let mut indices = Vec::new();
    for z in 0..n - 1 {
        for x in 0..n - 1 {
            let i = (z * n + x) as u32;
            let (a, b, c, d) = (i, i + 1, i + n as u32, i + n as u32 + 1);
            indices.extend_from_slice(&[a, c, b, b, c, d]);
        }
    }
    let mesh = Arc::new(CpuMesh { vertices, indices });
    let bvh = bvh_for(&mesh).expect("bvh");
    for model in [
        Mat4::IDENTITY,
        Mat4::from_rotation_z(0.4) * Mat4::from_translation(Vec3::new(1.0, 2.0, -3.0)),
        Mat4::from_scale(Vec3::new(-1.0, 1.0, 1.0)),
    ] {
        for cull in [FaceCull::None, FaceCull::Back] {
            for i in 0..200 {
                let f = i as f32;
                let origin = Vec3::new(
                    (f * 0.37).sin() * 15.0,
                    8.0 + (f * 0.11).cos(),
                    (f * 0.53).cos() * 15.0,
                );
                let target = Vec3::new((f * 0.91).sin() * 12.0, 0.0, (f * 0.29).cos() * 12.0);
                let direction = (target - origin).normalize();
                let brute = raycast_mesh(&mesh, &mesh.indices, model, &[], origin, direction, cull);
                let fast = raycast_mesh_bvh(&mesh, &bvh, model, origin, direction, cull);
                assert_eq!(
                    brute.map(|h| h.triangle),
                    fast.map(|h| h.triangle),
                    "ray {i} {cull:?}"
                );
                if let (Some(a), Some(b)) = (brute, fast) {
                    assert!((a.distance - b.distance).abs() < 1e-5 && (a.normal - b.normal).length() < 1e-5);
                }
            }
        }
    }
    // Small meshes skip the tree.
    let tiny = Arc::new(CpuMesh {
        vertices: mesh.vertices.clone(),
        indices: mesh.indices[..30].to_vec(),
    });
    assert!(bvh_for(&tiny).is_none());
    assert_eq!(LayerSet::only(&[1, 200]).layers(), vec![1, 200]);
}

#[test]
fn pick_results_follow_scene_similarity_and_direction_rescaling() {
    use regia_core::glam::Quat;

    // Triangle names are vertex-index triples. The rest are deterministic, spatially separated
    // decoys, large enough to exercise the BVH without introducing a coplanar tie.
    let vertex =
        |position| regia_render::LabVertex::from_pos_normal_uv(position, [0.0, 0.0, 1.0], [0.0, 0.0]);
    let mut vertices = vec![
        vertex([0.0, 0.0, 0.0]),
        vertex([3.0, 0.0, 0.0]),
        vertex([0.0, 2.0, 0.0]),
    ];
    let mut indices = vec![0, 1, 2];
    for n in 0..63_u32 {
        let base = vertices.len() as u32;
        let x = 10.0 + n as f32 * 4.0;
        let z = -2.0 - n as f32;
        vertices.extend([
            vertex([x, 0.0, z]),
            vertex([x + 1.0, 0.0, z]),
            vertex([x, 1.0, z]),
        ]);
        indices.extend([base, base + 1, base + 2]);
    }
    let mesh = Arc::new(CpuMesh { vertices, indices });
    let bvh = bvh_for(&mesh).expect("fixture crosses the BVH threshold");
    let local_origin = Vec3::new(0.4, 0.6, 5.0);
    let local_direction = Vec3::NEG_Z;
    let expected_indices = [0, 1, 2];

    let transforms = [
        (Vec3::ZERO, Quat::IDENTITY, 1.0),
        (
            Vec3::new(7.0, -3.0, 11.0),
            Quat::from_rotation_y(0.73) * Quat::from_rotation_x(-0.31),
            1.0,
        ),
        (Vec3::new(-4.0, 8.0, 2.0), Quat::from_rotation_z(0.41), 2.5),
    ];
    for (translation, rotation, scale) in transforms {
        let model = Mat4::from_scale_rotation_translation(Vec3::splat(scale), rotation, translation);
        let origin = model.transform_point3(local_origin);
        let unit_direction = rotation * local_direction;
        let direction = unit_direction * 1.0e-8;
        let brute = raycast_mesh(
            &mesh,
            &mesh.indices,
            model,
            &[],
            origin,
            direction,
            FaceCull::None,
        )
        .expect("brute force hit");
        let accelerated =
            raycast_mesh_bvh(&mesh, &bvh, model, origin, direction * 19.0, FaceCull::None).expect("BVH hit");
        assert_eq!(brute.indices, expected_indices);
        assert_eq!(accelerated.indices, expected_indices);
        assert_eq!(brute.indices, accelerated.indices);
        assert!((brute.position - model.transform_point3(Vec3::new(0.4, 0.6, 0.0))).length() < 2e-4);
        assert!((brute.normal - rotation * Vec3::Z).length() < 2e-5);
        assert!((brute.distance - 5.0 * scale).abs() < 2e-4);
        assert!((accelerated.position - brute.position).length() < 2e-5);
        assert!((accelerated.normal - brute.normal).length() < 2e-5);
        assert!((accelerated.distance - brute.distance).abs() < 2e-5);
    }

    // A fixed one-joint palette moves the asymmetric triangle before the same scene-space
    // similarity transform. This checks the skinned branch's world/object conversion too.
    let mut skinned_vertices = vec![
        vertex([0.0, 0.0, 0.0]),
        vertex([3.0, 0.0, 0.0]),
        vertex([0.0, 2.0, 0.0]),
    ];
    for v in &mut skinned_vertices {
        v.joints = [0, 0, 0, 0];
        v.weights = [1.0, 0.0, 0.0, 0.0];
    }
    let skinned = CpuMesh {
        vertices: skinned_vertices,
        indices: vec![0, 1, 2],
    };
    let palette = [JointMatrix::translation(2.0, 1.0, 0.0)];
    for (translation, rotation, scale) in transforms {
        let model = Mat4::from_scale_rotation_translation(Vec3::splat(scale), rotation, translation);
        let local_point = Vec3::new(2.4, 1.6, 0.0);
        let origin = model.transform_point3(local_point + Vec3::new(0.0, 0.0, 5.0));
        let direction = rotation * Vec3::NEG_Z * 1.0e-8;
        let hit = raycast_mesh(
            &skinned,
            &skinned.indices,
            model,
            &palette,
            origin,
            direction,
            FaceCull::None,
        )
        .expect("skinned hit");
        assert_eq!(hit.indices, [0, 1, 2]);
        assert!((hit.position - model.transform_point3(local_point)).length() < 2e-4);
        assert!((hit.normal - rotation * Vec3::Z).length() < 2e-5);
        assert!((hit.distance - 5.0 * scale).abs() < 2e-4);
    }

    // Storage order is not triangle identity; the named vertex triple must survive permutation.
    let mut permuted = CpuMesh {
        vertices: mesh.vertices.clone(),
        indices: mesh.indices.clone(),
    };
    permuted.indices.rotate_left(3);
    let permuted_bvh = bvh_for(&Arc::new(CpuMesh {
        vertices: permuted.vertices.clone(),
        indices: permuted.indices.clone(),
    }))
    .expect("permuted BVH");
    let hit = raycast_mesh_bvh(
        &permuted,
        &permuted_bvh,
        Mat4::IDENTITY,
        local_origin,
        local_direction,
        FaceCull::None,
    )
    .expect("permuted hit");
    assert_eq!(hit.indices, expected_indices);
    assert_eq!(hit.triangle, 63);

    let mut world = World::new();
    let entity = world
        .spawn((
            MeshRenderer::new(mesh.clone(), Arc::new(LabMaterial::default())),
            GlobalTransform(Affine3A::from_mat4(Mat4::from_scale_rotation_translation(
                Vec3::splat(2.5),
                Quat::from_rotation_z(0.41),
                Vec3::new(-4.0, 8.0, 2.0),
            ))),
            MapLayer(9),
        ))
        .id();
    let rotation = Quat::from_rotation_z(0.41);
    let origin = Mat4::from_scale_rotation_translation(Vec3::splat(2.5), rotation, Vec3::new(-4.0, 8.0, 2.0))
        .transform_point3(local_origin);
    let ray = Ray::new(origin, rotation * local_direction * 1.0e-8);
    let picked = world_pick(&world, ray, &PickFilter::on_layers(&[9])).expect("filtered hit");
    assert_eq!((picked.entity, picked.layer), (entity, MapLayer(9)));
    assert_eq!(picked.surface, PickSurface::Mesh);
    assert_eq!(picked.triangle, Some(0));
    assert!(
        (picked.point
            - Mat4::from_scale_rotation_translation(Vec3::splat(2.5), rotation, Vec3::new(-4.0, 8.0, 2.0),)
                .transform_point3(Vec3::new(0.4, 0.6, 0.0)))
        .length()
            < 2e-4
    );
    assert!((picked.normal - rotation * Vec3::Z).length() < 2e-5);
    assert!((picked.distance - 12.5).abs() < 2e-4);
    assert!(world_pick(&world, ray, &PickFilter::on_layers(&[8])).is_none());
}

#[test]
fn coplanar_triangle_ties_keep_the_lowest_storage_index() {
    let vertex =
        |position| regia_render::LabVertex::from_pos_normal_uv(position, [0.0, 0.0, 1.0], [0.0, 0.0]);
    let mut vertices = vec![
        vertex([0.0, 0.0, 0.0]),
        vertex([3.0, 0.0, 0.0]),
        vertex([0.0, 2.0, 0.0]),
        vertex([0.0, 0.0, 0.0]),
        vertex([3.0, 0.0, 0.0]),
        vertex([0.0, 2.0, 0.0]),
    ];
    let mut indices = vec![0, 1, 2, 3, 4, 5];
    for n in 0..62_u32 {
        let base = vertices.len() as u32;
        let x = 10.0 + n as f32 * 4.0;
        let z = -2.0 - n as f32;
        vertices.extend([
            vertex([x, 0.0, z]),
            vertex([x + 1.0, 0.0, z]),
            vertex([x, 1.0, z]),
        ]);
        indices.extend([base, base + 1, base + 2]);
    }
    let mesh = Arc::new(CpuMesh { vertices, indices });
    let bvh = bvh_for(&mesh).expect("fixture crosses the BVH threshold");
    let origin = Vec3::new(0.4, 0.6, 5.0);
    let brute = raycast_mesh(
        &mesh,
        &mesh.indices,
        Mat4::IDENTITY,
        &[],
        origin,
        Vec3::NEG_Z,
        FaceCull::None,
    )
    .expect("brute force hit");
    let accelerated =
        raycast_mesh_bvh(&mesh, &bvh, Mat4::IDENTITY, origin, Vec3::NEG_Z, FaceCull::None).expect("BVH hit");
    assert_eq!((brute.triangle, brute.indices), (0, [0, 1, 2]));
    assert_eq!((accelerated.triangle, accelerated.indices), (0, [0, 1, 2]));
}

#[test]
fn meshes_hidden_by_an_ancestor_are_ignored_and_coplanar_ties_go_to_the_lower_entity() {
    use regia_scene::prelude::InheritedVisibility;
    let mut world = World::new();
    let under = floor(&mut world, 0.0, Some(0));
    let deck = floor(&mut world, 5.0, Some(1));
    // What transform propagation records for an `Inherited` child of a `Hidden` parent.
    world.entity_mut(deck).insert(InheritedVisibility(false));
    assert_eq!(
        world_pick(&world, down(0.0, 0.0), &PickFilter::default()).map(|p| p.entity),
        Some(under)
    );
    let with_hidden = PickFilter {
        include_hidden: true,
        ..PickFilter::default()
    };
    assert_eq!(
        world_pick(&world, down(0.0, 0.0), &with_hidden).map(|p| p.entity),
        Some(deck)
    );

    // Two slabs on the same plane: the lower entity wins, whatever order the world visits them.
    let a = floor(&mut world, 9.0, Some(2));
    let b = floor(&mut world, 9.0, Some(2));
    let any_layer = world_pick(&world, down(0.0, 0.0), &PickFilter::on_layers(&[2])).expect("hit");
    assert_eq!(any_layer.entity, a.min(b));
}

#[test]
fn a_far_origin_ray_terminates_instead_of_stalling_the_march() {
    let mut world = World::new();
    flat_terrain(&mut world, None);
    // At 2e7 m the f32 resolution is 2 m: a 5 cm step cannot advance the parameter.
    let ray = Ray::new(Vec3::new(2.0e7, 100.0, 0.0), Vec3::NEG_X);
    assert!(world_pick(&world, ray, &PickFilter::default()).is_none());
}

#[test]
fn hidden_terrain_is_not_picked_unless_included() {
    use regia_scene::prelude::Visibility;
    let mut world = World::new();
    let ground = flat_terrain(&mut world, None);
    world.entity_mut(ground).insert(Visibility::Hidden);
    assert!(world_pick(&world, down(0.0, 0.0), &PickFilter::default()).is_none());
}

#[test]
fn a_crossing_inside_the_map_is_hit_even_when_the_next_march_step_leaves_it() {
    let mut world = World::new();
    let ground = flat_terrain(&mut world, None);
    // Crosses y = 0 at x = 127.8 (the edge is 128): the step after the crossing is outside.
    let ray = Ray::new(Vec3::new(119.8, 6.0, 0.0), Vec3::new(0.8, -0.6, 0.0));
    let hit = world_pick(&world, ray, &PickFilter::default()).expect("hit near the edge");
    assert_eq!((hit.entity, hit.surface), (ground, PickSurface::Terrain));
    assert!((hit.point.x - 127.8).abs() < 1e-2 && hit.point.y.abs() < 1e-2);
    // A crossing past the edge is still a miss.
    let outside = Ray::new(Vec3::new(121.0, 6.0, 0.0), Vec3::new(0.8, -0.6, 0.0));
    assert!(world_pick(&world, outside, &PickFilter::default()).is_none());
}

#[test]
fn a_fine_heightfield_is_marched_at_half_its_sample_spacing() {
    // 17 x 17 samples over 0.32 m (2 cm spacing) with a 1 m spike at the centre sample.
    let n = 17_u32;
    let mut heights = vec![0.0_f32; (n * n) as usize];
    heights[((n / 2) * n + n / 2) as usize] = 1.0;
    let map = TerrainMap::from_heights(n, n, heights, 0.32, 1.0, 0.0).expect("map");
    let minmax = MinMaxPyramid::build(&map, 2).expect("pyramid");
    let mut world = World::new();
    world.spawn(Terrain {
        map,
        cdlod: CdlodConfig::default(),
        root_level: 2,
        minmax,
        layers: TerrainLayers::default(),
        holes: None,
        water: None,
    });
    // Horizontal at y = 0.4 along the spike's row: a 1 cm step lands inside the 2.4 cm-wide cone.
    let hit = raycast_terrain(&world, Vec3::new(-0.08, 0.4, 0.0), Vec3::X, 1.0);
    assert!(hit.is_some(), "the spike is stepped over");
}

#[test]
fn hidden_terrain_does_not_block_unless_hidden_entities_are_included() {
    use regia_scene::prelude::Visibility;
    let mut world = World::new();
    let ground = flat_terrain(&mut world, None);
    let deck = floor(&mut world, -5.0, Some(0));
    world.entity_mut(ground).insert(Visibility::Hidden);
    assert_eq!(
        world_pick(&world, down(0.0, 0.0), &PickFilter::default()).map(|p| p.entity),
        Some(deck)
    );
    let with_hidden = PickFilter {
        include_hidden: true,
        ..PickFilter::default()
    };
    assert_eq!(
        world_pick(&world, down(0.0, 0.0), &with_hidden).map(|p| p.entity),
        Some(ground)
    );
}

#[test]
fn a_mesh_bvh_used_with_another_mesh_misses_instead_of_panicking() {
    let (vertices, indices) = box_mesh([1.0; 3]);
    let mesh = CpuMesh { vertices, indices };
    let bvh = regia_pick::MeshBvh::build(&mesh).expect("bvh");
    assert!(bvh.triangle_indices(0).is_some());
    assert!(bvh.triangle_indices(u32::MAX).is_none());
    let empty = CpuMesh {
        vertices: Vec::new(),
        indices: Vec::new(),
    };
    let hit = raycast_mesh_bvh(
        &empty,
        &bvh,
        Mat4::IDENTITY,
        Vec3::new(0.0, 5.0, 0.0),
        Vec3::NEG_Y,
        FaceCull::None,
    );
    assert!(hit.is_none());
}

#[test]
fn a_very_short_direction_casts_like_a_unit_one() {
    let mut world = World::new();
    let deck = floor(&mut world, 5.0, None);
    let origin = Vec3::new(0.0, 10.0, 0.0);
    let short = raycast_mesh_entity(&world, deck, origin, Vec3::new(0.0, -1e-13, 0.0), FaceCull::None);
    let unit = raycast_mesh_entity(&world, deck, origin, Vec3::NEG_Y, FaceCull::None);
    assert!(unit.is_some());
    assert_eq!(short.map(|h| h.triangle), unit.map(|h| h.triangle));
}

#[test]
fn a_terrain_crossing_outside_the_map_square_is_not_a_hit() {
    let mut world = World::new();
    flat_terrain(&mut world, None);
    // Starts outside the 256 m square and meets the clamped pad plane at x = 130.
    let ray = Ray::new(Vec3::new(129.0, 1.0, 0.0), Vec3::new(1.0, -1.0, 0.0));
    assert!(world_pick(&world, ray, &PickFilter::default()).is_none());
}

#[test]
fn an_in_place_mesh_edit_rebuilds_the_cached_bvh() {
    let (vertices, indices) = box_mesh([1.0, 1.0, 1.0]);
    let mut mesh = Arc::new(CpuMesh { vertices, indices });
    // Enough triangles for a BVH: repeat the box's index list.
    {
        let m = Arc::get_mut(&mut mesh).expect("unique");
        let base = m.indices.clone();
        while m.indices.len() / 3 < 80 {
            m.indices.extend_from_slice(&base);
        }
    }
    assert!(bvh_for(&mesh).is_some());
    // Shrink the vertex list so the old tree would index out of range, then query again. The
    // cache's weak handle blocks `Arc::get_mut`, so the edit takes the mesh out of its `Arc`.
    assert!(Arc::get_mut(&mut mesh).is_none());
    let mut m = Arc::into_inner(mesh).expect("only strong handle");
    m.vertices.truncate(3);
    m.indices = [0, 1, 2].repeat(80);
    let mesh = Arc::new(m);
    let bvh = bvh_for(&mesh).expect("rebuilt for the edited mesh");
    // Would index past the three remaining vertices if the old tree were served.
    let _ = raycast_mesh_bvh(
        &mesh,
        &bvh,
        Mat4::IDENTITY,
        Vec3::new(0.0, 10.0, 0.0),
        Vec3::NEG_Y,
        FaceCull::None,
    );
}

#[test]
fn terrain_is_layer_zero_whatever_it_is_tagged_with() {
    let mut world = World::new();
    let parent = world.spawn(MapLayer(1)).id();
    let ground = flat_terrain(&mut world, None);
    world.entity_mut(ground).insert((MapLayer(2), ChildOf(parent)));
    let hit = world_pick(&world, down(0.0, 0.0), &PickFilter::on_layers(&[0])).expect("hit");
    assert_eq!(
        (hit.entity, hit.layer, hit.surface),
        (ground, MapLayer(0), PickSurface::Terrain)
    );
    assert!(world_pick(&world, down(0.0, 0.0), &PickFilter::on_layers(&[1, 2])).is_none());
    // The layer lookup agrees with the cast: a host building a filter from it queries layer 0.
    assert_eq!(map_layer_of(&world, ground), MapLayer(0));
}

/// A flat terrain with `n` x `n` zero samples over `world_width` metres.
fn tiny_terrain(world: &mut World, n: u32, world_width: f32) {
    let map =
        TerrainMap::from_heights(n, n, vec![0.0; (n * n) as usize], world_width, 1.0, 0.0).expect("map");
    let minmax = MinMaxPyramid::build(&map, 0).expect("pyramid");
    world.spawn(Terrain {
        map,
        cdlod: CdlodConfig::default(),
        root_level: 0,
        minmax,
        layers: TerrainLayers::default(),
        holes: None,
        water: None,
    });
}

#[test]
fn a_millimeter_map_far_from_the_origin_terminates() {
    // Spacing 2 mm gives a 1 mm march step, which is below one f32 ulp at 40 km: the march must
    // still advance (a stalled `t` would never return).
    let mut world = World::new();
    tiny_terrain(&mut world, 2, 0.002);
    let skim = Ray::new(Vec3::new(40000.0, 0.0005, 0.0), Vec3::NEG_X);
    assert!(raycast_terrain(&world, skim.origin, skim.direction, f32::INFINITY).is_none());
}

#[test]
fn a_nan_distance_cap_hits_nothing_even_with_a_terrain() {
    let mut world = World::new();
    tiny_terrain(&mut world, 2, 4.0);
    let from = Vec3::new(0.0, 5.0, 0.0);
    assert!(raycast_terrain(&world, from, Vec3::NEG_Y, 100.0).is_some());
    assert!(raycast_terrain(&world, from, Vec3::NEG_Y, f32::NAN).is_none());
}

#[test]
fn a_joint_index_past_the_palette_clamps_to_the_last_joint() {
    let mut world = World::new();
    let (vertices, indices) = box_mesh([0.5; 3]);
    let mut mesh = CpuMesh { vertices, indices };
    for v in &mut mesh.vertices {
        v.joints = [7, 0, 0, 0];
        v.weights = [1.0, 0.0, 0.0, 0.0];
    }
    world.spawn((
        MeshRenderer::new(Arc::new(mesh), Arc::new(LabMaterial::default())),
        SkinJoints {
            palette: vec![
                JointMatrix::translation(4.0, 0.0, 0.0),
                JointMatrix::translation(2.0, 0.0, 0.0),
            ],
        },
        GlobalTransform(Affine3A::IDENTITY),
    ));
    let at = |x: f32| {
        world_pick(
            &world,
            Ray::new(Vec3::new(x, 0.0, 5.0), Vec3::NEG_Z),
            &PickFilter::default(),
        )
    };
    assert!(at(2.0).is_some(), "joint 7 clamps to the last joint (x = 2)");
    assert!(at(0.0).is_none(), "not the identity pose");
}

#[test]
fn parked_members_are_not_picked_even_with_hidden_entities_included() {
    use regia_scene::layers::LayerParked;
    use regia_scene::prelude::Visibility;

    let mut world = World::new();
    let upper = floor(&mut world, 10.0, Some(1));
    let lower = floor(&mut world, 0.0, Some(2));
    world.entity_mut(upper).insert(LayerParked {
        restore: Visibility::Inherited,
        inherited: false,
    });
    let filter = PickFilter {
        include_hidden: true,
        ..PickFilter::default()
    };
    let hit = world_pick(&world, down(0.0, 0.0), &filter).expect("the floor below");
    assert_eq!(hit.entity, lower);
}

/// Coplanar floors where the entity id order and the arrival order disagree: `first` is spawned
/// before `second` but has the larger entity bits (a reused index at generation 1).
fn reused_index_pair(world: &mut World, layers: (Option<u8>, Option<u8>)) -> (Entity, Entity) {
    let dead = floor(world, 5.0, None);
    world.despawn(dead);
    let first = floor(world, 5.0, layers.0);
    let second = floor(world, 5.0, layers.1);
    assert!(
        first.to_bits() > second.to_bits(),
        "the setup needs the reused id to sort last"
    );
    (first, second)
}

/// ADR-2273: equal distances go to the surface that arrived first, not to the lower entity id.
#[test]
fn coplanar_ties_follow_arrival_order_not_entity_bits() {
    let mut world = World::new();
    assert_eq!(regia_pick::track_pick_order(&mut world), (true, true));
    let (first, _second) = reused_index_pair(&mut world, (None, None));
    let hit = world_pick(&world, down(0.0, 0.0), &PickFilter::default()).expect("hit");
    assert_eq!(hit.entity, first);
    let cast = regia_pick::raycast_meshes(
        &world,
        Vec3::new(0.0, 50.0, 0.0),
        Vec3::NEG_Y,
        FaceCull::None,
        &|_| true,
    )
    .expect("hit");
    assert_eq!(cast.0, first);
}

/// ADR-2273: `world_pick_layers` breaks equal distances like `world_pick`, not by layer id.
#[test]
fn layer_picks_break_ties_like_world_pick() {
    let mut world = World::new();
    assert_eq!(regia_pick::track_pick_order(&mut world), (true, true));
    let (first, second) = reused_index_pair(&mut world, (Some(3), Some(1)));
    let one = world_pick(&world, down(0.0, 0.0), &PickFilter::default()).expect("hit");
    let layers = world_pick_layers(&world, down(0.0, 0.0), &PickFilter::default());
    assert_eq!(one.entity, first);
    assert_eq!(
        layers.iter().map(|p| p.entity).collect::<Vec<_>>(),
        vec![first, second]
    );
}

/// ADR-2273: two coplanar terrains resolve to the earlier arrival whatever the storage order,
/// also after the first-stored one is despawned.
#[test]
fn coplanar_terrains_resolve_by_arrival_order() {
    let mut world = World::new();
    assert_eq!(regia_pick::track_pick_order(&mut world), (true, true));
    let a = flat_terrain(&mut world, None);
    let b = flat_terrain(&mut world, None);
    let c = flat_terrain(&mut world, None);
    let origin = Vec3::new(0.0, 50.0, 0.0);
    let hit = raycast_terrain(&world, origin, Vec3::NEG_Y, 100.0).expect("hit");
    assert_eq!(hit.entity, a);
    world.despawn(a);
    let hit = raycast_terrain(&world, origin, Vec3::NEG_Y, 100.0).expect("hit");
    assert_eq!(hit.entity, b);
    world.despawn(b);
    let hit = raycast_terrain(&world, origin, Vec3::NEG_Y, 100.0).expect("hit");
    assert_eq!(hit.entity, c);
}

/// A mesh without a `GlobalTransform` is not drawn (the extract needs one), so it is not pickable.
#[test]
fn a_mesh_without_a_global_transform_is_not_picked() {
    let mut world = World::new();
    let deck = floor(&mut world, 5.0, None);
    let origin = Vec3::new(0.0, 10.0, 0.0);
    assert!(raycast_mesh_entity(&world, deck, origin, Vec3::NEG_Y, FaceCull::None).is_some());
    world.entity_mut(deck).remove::<GlobalTransform>();
    assert!(raycast_mesh_entity(&world, deck, origin, Vec3::NEG_Y, FaceCull::None).is_none());
}

/// `raycast_meshes` keeps the nearest accepted hit, honours the callback, and on an exact tie
/// keeps the earliest-arrived mesh (ADR-2273), not the entity ordering.
#[test]
fn raycast_meshes_takes_the_nearest_accepted_mesh() {
    let mut world = World::new();
    assert_eq!(regia_pick::track_pick_order(&mut world), (true, true));
    let low = floor(&mut world, 5.0, None);
    let high = floor(&mut world, 10.0, None);
    let ray = down(0.0, 0.0);
    let (entity, hit) =
        raycast_meshes(&world, ray.origin, ray.direction, FaceCull::None, &|_| true).expect("hit");
    assert_eq!(entity, high);
    assert!((hit.distance - 40.0).abs() < 1e-3);
    // Rejecting the nearest mesh leaves the one below it.
    let (entity, _) =
        raycast_meshes(&world, ray.origin, ray.direction, FaceCull::None, &|e| e != high).expect("hit");
    assert_eq!(entity, low);
    assert!(raycast_meshes(&world, ray.origin, ray.direction, FaceCull::None, &|_| false).is_none());
    // Two coincident slabs tie on distance: the earlier arrival wins.
    let twin = floor(&mut world, 10.0, None);
    assert_ne!(high, twin, "the coincident slab is a second entity");
    let (entity, _) =
        raycast_meshes(&world, ray.origin, ray.direction, FaceCull::None, &|e| e != low).expect("hit");
    assert_eq!(
        entity, high,
        "the tie goes to the earlier arrival, not the entity ordering"
    );
}

/// A triangle in the plane `y = 0` covering `(0.25, _, 0.25)`.
fn flat_triangle() -> CpuMesh {
    let vertex = |position: [f32; 3]| LabVertex::from_pos_normal_uv(position, [0.0, 1.0, 0.0], [0.0, 0.0]);
    CpuMesh {
        vertices: vec![
            vertex([0.0, 0.0, 0.0]),
            vertex([1.0, 0.0, 0.0]),
            vertex([0.0, 0.0, 1.0]),
        ],
        indices: vec![0, 1, 2],
    }
}

fn copy_mesh(mesh: &CpuMesh) -> CpuMesh {
    CpuMesh {
        vertices: mesh.vertices.clone(),
        indices: mesh.indices.clone(),
    }
}

fn morph_delta(name: &str, delta: [f32; 3]) -> regia_asset::CpuMorphTarget {
    regia_asset::CpuMorphTarget {
        name: name.into(),
        vertex_indices: vec![0, 1, 2],
        position_deltas: vec![delta; 3],
        normal_deltas: Vec::new(),
        tangent_deltas: Vec::new(),
    }
}

fn targets_of(mesh: &CpuMesh, targets: &[regia_asset::CpuMorphTarget]) -> Arc<MorphTargets> {
    Arc::new(MorphTargets::new(&mesh.vertices, targets).expect("morph targets"))
}

fn down_at(x: f32, y: f32, z: f32) -> Ray {
    Ray::new(Vec3::new(x, y, z), Vec3::NEG_Y)
}

/// Weight 1 lifts the triangle by 10 m. The ray through the drawn triangle hits there; the ray
/// through the bind-pose triangle misses. Skinning runs on the deformed vertices. A sideways
/// delta is not rejected by the bind-pose bounds.
#[test]
fn morphed_mesh_is_picked_where_it_is_drawn() {
    let mut world = World::new();
    let mesh = flat_triangle();
    let targets = targets_of(&mesh, &[morph_delta("lift", [0.0, 10.0, 0.0])]);
    world.spawn((
        MeshRenderer::new(Arc::new(mesh), Arc::new(LabMaterial::default())),
        GlobalTransform::default(),
        MeshMorphTargets::new(Arc::clone(&targets)),
        MorphWeights(vec![1.0]),
    ));

    let lifted =
        world_pick(&world, down_at(0.25, 20.0, 0.25), &PickFilter::default()).expect("drawn surface");
    assert!(
        (lifted.point.y - 10.0).abs() < 1e-4,
        "hit the bind pose at {}",
        lifted.point.y
    );
    assert!((lifted.distance - 10.0).abs() < 1e-4);
    assert!(
        world_pick(&world, down_at(0.25, 5.0, 0.25), &PickFilter::default()).is_none(),
        "the bind-pose triangle is not drawn"
    );

    // Targets without weights, and weights without targets, stay on the bind pose.
    let mut bare = World::new();
    let mesh = flat_triangle();
    bare.spawn((
        MeshRenderer::new(Arc::new(copy_mesh(&mesh)), Arc::new(LabMaterial::default())),
        GlobalTransform::default(),
        MeshMorphTargets::new(targets_of(&mesh, &[morph_delta("lift", [0.0, 10.0, 0.0])])),
    ));
    let bind = world_pick(&bare, down_at(0.25, 5.0, 0.25), &PickFilter::default()).expect("bind");
    assert!(bind.point.y.abs() < 1e-4);
    assert!(
        world_pick(
            &bare,
            down_at(0.25, 20.0, 0.25),
            &PickFilter {
                max_distance: 15.0,
                ..PickFilter::default()
            }
        )
        .is_none()
    );

    let mut weights_only = World::new();
    weights_only.spawn((
        MeshRenderer::new(Arc::new(flat_triangle()), Arc::new(LabMaterial::default())),
        GlobalTransform::default(),
        MorphWeights(vec![1.0]),
    ));
    let bind = world_pick(&weights_only, down_at(0.25, 5.0, 0.25), &PickFilter::default()).expect("bind");
    assert!(bind.point.y.abs() < 1e-4);

    // The joint translates the already lifted triangle, so the drawn surface is at y = 12.
    let mut skinned = World::new();
    let mut mesh = flat_triangle();
    for vertex in &mut mesh.vertices {
        vertex.joints = [0, 0, 0, 0];
        vertex.weights = [1.0, 0.0, 0.0, 0.0];
    }
    skinned.spawn((
        MeshRenderer::new(Arc::new(copy_mesh(&mesh)), Arc::new(LabMaterial::default())),
        GlobalTransform::default(),
        MeshMorphTargets::new(targets_of(&mesh, &[morph_delta("lift", [0.0, 10.0, 0.0])])),
        MorphWeights(vec![1.0]),
        SkinJoints {
            palette: vec![JointMatrix::translation(0.0, 2.0, 0.0)],
        },
    ));
    let both = world_pick(&skinned, down_at(0.25, 20.0, 0.25), &PickFilter::default())
        .expect("morphed then skinned");
    assert!(
        (both.point.y - 12.0).abs() < 1e-4,
        "morph then skin, got {}",
        both.point.y
    );
    assert!(world_pick(&skinned, down_at(0.25, 5.0, 0.25), &PickFilter::default()).is_none());

    // Moved out of the bind-pose box: the ray still hits the drawn triangle.
    let mut aside = World::new();
    let mesh = flat_triangle();
    aside.spawn((
        MeshRenderer::new(Arc::new(copy_mesh(&mesh)), Arc::new(LabMaterial::default())),
        GlobalTransform::default(),
        MeshMorphTargets::new(targets_of(&mesh, &[morph_delta("slide", [10.0, 0.0, 0.0])])),
        MorphWeights(vec![1.0]),
    ));
    let slid = world_pick(&aside, down_at(10.25, 5.0, 0.25), &PickFilter::default()).expect("slid");
    assert!((slid.point.x - 10.25).abs() < 1e-3 && slid.point.y.abs() < 1e-4);
    assert!(world_pick(&aside, down_at(0.25, 5.0, 0.25), &PickFilter::default()).is_none());
}

/// Game mode keeps the sixteen largest weights. A seventeenth, smaller weight must not move the
/// surface; creator mode keeps it.
#[test]
fn morph_weights_follow_the_renderers_active_subset() {
    let mesh = flat_triangle();
    let mut deltas = vec![morph_delta("big", [0.0, 10.0, 0.0])];
    for index in 1..16 {
        deltas.push(morph_delta(&format!("pad{index}"), [0.0, 0.0, 0.0]));
    }
    deltas.push(morph_delta("small", [0.0, 100.0, 0.0]));
    let targets = targets_of(&mesh, &deltas);
    let mut weights = vec![0.01; 17];
    weights[0] = 1.0;
    weights[16] = 0.001;

    let spawn = |mode: MorphMode| {
        let mut world = World::new();
        world.spawn((
            MeshRenderer::new(Arc::new(copy_mesh(&mesh)), Arc::new(LabMaterial::default())),
            GlobalTransform::default(),
            MeshMorphTargets {
                targets: Arc::clone(&targets),
                mode,
            },
            MorphWeights(weights.clone()),
        ));
        world
    };

    let game = world_pick(
        &spawn(MorphMode::Game),
        down_at(0.25, 20.0, 0.25),
        &PickFilter::default(),
    )
    .expect("game");
    assert!(
        (game.point.y - 10.0).abs() < 1e-3,
        "game mode kept the dropped weight, y = {}",
        game.point.y
    );
    let creator = world_pick(
        &spawn(MorphMode::Creator),
        down_at(0.25, 20.0, 0.25),
        &PickFilter::default(),
    )
    .expect("creator");
    assert!(
        (creator.point.y - 10.1).abs() < 1e-3,
        "creator mode dropped a weight, y = {}",
        creator.point.y
    );
}

/// A patch of a tube twisted 160° between two joints. Linear blend pinches it inside a quarter
/// of the radius; dual quaternions keep the radius. Positions are the CPU skinning reference.
fn twisted_patch() -> (CpuMesh, Vec<JointMatrix>) {
    let radius = 0.12;
    let at = |angle: f32, y: f32| [radius * angle.cos(), y, radius * angle.sin()];
    let mut mesh = CpuMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv(at(-0.25, 0.06), [1.0, 0.0, 0.0], [0.0, 0.0]),
            LabVertex::from_pos_normal_uv(at(-0.25, -0.06), [1.0, 0.0, 0.0], [0.0, 0.0]),
            LabVertex::from_pos_normal_uv(at(0.25, 0.0), [1.0, 0.0, 0.0], [0.0, 0.0]),
        ],
        indices: vec![0, 1, 2],
    };
    for vertex in &mut mesh.vertices {
        vertex.joints = [0, 1, 0, 0];
        vertex.weights = [0.5, 0.5, 0.0, 0.0];
    }
    let joints = vec![
        JointMatrix::identity(),
        JointMatrix::from_mat4(Mat4::from_quat(Quat::from_rotation_y(160f32.to_radians()))),
    ];
    (mesh, joints)
}

fn posed_positions(
    mesh: &CpuMesh,
    joints: &[JointMatrix],
    table: Option<&[SkinInfluences8]>,
    method: SkinningMethod,
) -> Vec<Vec3> {
    mesh.vertices
        .iter()
        .enumerate()
        .map(|(index, vertex)| {
            let influences: Vec<(u32, f32)> = match table {
                Some(rows) => (0..8)
                    .map(|k| (rows[index].joints[k], rows[index].weights[k]))
                    .collect(),
                None => (0..4).map(|k| (vertex.joints[k], vertex.weights[k])).collect(),
            };
            let tangent = [vertex.tangent[0], vertex.tangent[1], vertex.tangent[2]];
            let (position, _, _) = skin_vertex_reference(
                (vertex.position, vertex.normal, tangent),
                &influences,
                joints,
                method,
            );
            Vec3::from_array(position)
        })
        .collect()
}

fn centroid(points: &[Vec3]) -> Vec3 {
    points.iter().copied().fold(Vec3::ZERO, |sum, point| sum + point) / points.len() as f32
}

fn xz_radius(point: Vec3) -> f32 {
    point.x.hypot(point.z)
}

/// Ray from `outward` metres further off the axis than `points`' centroid, aimed at that centroid.
fn ray_at_pose(points: &[Vec3], outward: f32) -> Ray {
    let center = centroid(points);
    let radial = Vec3::new(center.x, 0.0, center.z);
    let outward_dir = if radial.length() > 1e-5 {
        radial.normalize()
    } else {
        Vec3::X
    };
    let origin = center + outward_dir * outward;
    Ray::new(origin, center - origin)
}

#[test]
fn dual_quaternion_skin_is_picked_on_the_drawn_ring() {
    let (mesh, joints) = twisted_patch();
    let linear = posed_positions(&mesh, &joints, None, SkinningMethod::Linear);
    let dual = posed_positions(&mesh, &joints, None, SkinningMethod::DualQuaternion);
    let linear_radius = linear.iter().copied().map(xz_radius).fold(0.0, f32::max);
    let dual_radius = dual.iter().copied().map(xz_radius).fold(f32::MAX, f32::min);
    assert!(
        linear_radius < 0.25 * 0.12,
        "linear blend did not pinch ({linear_radius})"
    );
    assert!(
        dual_radius > 0.99 * 0.12,
        "dual quaternions lost the radius ({dual_radius})"
    );

    let spawn = |options: Option<SkinningOptions>, on_parent: bool| {
        let mut world = World::new();
        let renderer = (
            MeshRenderer::new(Arc::new(copy_mesh(&mesh)), Arc::new(LabMaterial::default())),
            GlobalTransform::default(),
            SkinJoints {
                palette: joints.clone(),
            },
        );
        if on_parent {
            let parent = world.spawn(options.expect("parent options")).id();
            world.spawn((renderer, ChildOf(parent)));
        } else if let Some(options) = options {
            world.spawn((renderer, options));
        } else {
            world.spawn(renderer);
        }
        world
    };

    // The round ring is about 0.12 m off the axis. The cap stops before the pinched ring.
    let round = ray_at_pose(&dual, 0.5);
    let round_cap = PickFilter {
        max_distance: 0.55,
        ..PickFilter::default()
    };
    let drawn = world_pick(
        &spawn(Some(SkinningOptions::dual_quaternion()), false),
        round,
        &round_cap,
    )
    .expect("dual quaternion ring");
    assert!(
        (drawn.point - centroid(&dual)).length() < 1e-3,
        "hit {:?} not the drawn centroid {:?}",
        drawn.point,
        centroid(&dual)
    );
    assert!(
        world_pick(&spawn(None, false), round, &round_cap).is_none(),
        "linear blend is not at the round ring"
    );
    // The same method inherited from the parent, with no options on the mesh.
    let inherited = world_pick(
        &spawn(Some(SkinningOptions::dual_quaternion()), true),
        round,
        &round_cap,
    )
    .expect("inherited dual quaternion");
    assert!((inherited.point - centroid(&dual)).length() < 1e-3);

    // Inside the round ring, a ray through the pinched pose hits linear blend only.
    let pinched = ray_at_pose(&linear, 0.02);
    let pinch_cap = PickFilter {
        max_distance: 0.03,
        ..PickFilter::default()
    };
    let collapsed = world_pick(&spawn(None, false), pinched, &pinch_cap).expect("linear pinch");
    assert!((collapsed.point - centroid(&linear)).length() < 1e-3);
    assert!(
        world_pick(
            &spawn(Some(SkinningOptions::dual_quaternion()), false),
            pinched,
            &pinch_cap
        )
        .is_none(),
        "dual quaternions are not on the pinched ring"
    );
}

/// Eight influences whose last four carry the pose. The first four weights stay put, so the two
/// poses differ by a metre. A table on a parent, or one of the wrong length, is not read.
#[test]
fn eight_influence_skin_is_picked_not_the_first_four() {
    let mut mesh = flat_triangle();
    for vertex in &mut mesh.vertices {
        vertex.joints = [0, 1, 2, 3];
        vertex.weights = [1.0, 0.0, 0.0, 0.0];
    }
    let row = SkinInfluences8 {
        joints: [0, 1, 2, 3, 4, 0, 0, 0],
        weights: [0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0],
    };
    let table: Arc<[SkinInfluences8]> = Arc::from(vec![row; mesh.vertices.len()]);
    let palette = vec![
        JointMatrix::identity(),
        JointMatrix::identity(),
        JointMatrix::identity(),
        JointMatrix::identity(),
        JointMatrix::translation(0.0, 1.0, 0.0),
    ];
    let four = posed_positions(&mesh, &palette, None, SkinningMethod::Linear);
    let eight = posed_positions(&mesh, &palette, Some(&table), SkinningMethod::Linear);
    let apart = four
        .iter()
        .zip(&eight)
        .map(|(a, b)| (*a - *b).length())
        .fold(0.0, f32::max);
    assert!(apart > 1e-3, "eight influences match the first four ({apart})");

    let spawn = |options: SkinningOptions| {
        let mut world = World::new();
        world.spawn((
            MeshRenderer::new(Arc::new(copy_mesh(&mesh)), Arc::new(LabMaterial::default())),
            GlobalTransform::default(),
            SkinJoints {
                palette: palette.clone(),
            },
            options,
        ));
        world
    };

    let with_eight = world_pick(
        &spawn(SkinningOptions {
            influences8: Some(Arc::clone(&table)),
            ..SkinningOptions::default()
        }),
        down_at(0.25, 5.0, 0.25),
        &PickFilter::default(),
    )
    .expect("eight");
    assert!(
        (with_eight.point.y - 1.0).abs() < 1e-4,
        "picked the four-weight pose at {}",
        with_eight.point.y
    );
    let with_four = world_pick(
        &spawn(SkinningOptions::default()),
        down_at(0.25, 5.0, 0.25),
        &PickFilter::default(),
    )
    .expect("four");
    assert!(with_four.point.y.abs() < 1e-4);
    assert!((with_eight.point - with_four.point).length() > 1e-3);

    // The ray through the four-weight triangle starts below the eight-influence surface.
    assert!(
        world_pick(
            &spawn(SkinningOptions {
                influences8: Some(Arc::clone(&table)),
                ..SkinningOptions::default()
            }),
            down_at(0.25, 0.5, 0.25),
            &PickFilter::default(),
        )
        .is_none()
    );

    // A table of the wrong length is ignored, as the compute path ignores it.
    let short: Arc<[SkinInfluences8]> = Arc::from(vec![row]);
    let mismatched = world_pick(
        &spawn(SkinningOptions {
            influences8: Some(short),
            ..SkinningOptions::default()
        }),
        down_at(0.25, 5.0, 0.25),
        &PickFilter::default(),
    )
    .expect("fallback");
    assert!(mismatched.point.y.abs() < 1e-4);

    // The table is not inherited. A parent that carries it leaves the child on four weights.
    let mut world = World::new();
    let parent = world
        .spawn(SkinningOptions {
            influences8: Some(table),
            ..SkinningOptions::default()
        })
        .id();
    world.spawn((
        MeshRenderer::new(Arc::new(mesh), Arc::new(LabMaterial::default())),
        GlobalTransform::default(),
        SkinJoints { palette },
        ChildOf(parent),
    ));
    let inherited = world_pick(&world, down_at(0.25, 5.0, 0.25), &PickFilter::default()).expect("child");
    assert!(
        inherited.point.y.abs() < 1e-4,
        "inherited an influence table, y = {}",
        inherited.point.y
    );
}
