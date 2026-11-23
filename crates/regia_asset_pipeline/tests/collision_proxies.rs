// SPDX-License-Identifier: MPL-2.0
//! Authored `collision_<kind>_*` glTF nodes cook into collider shapes (ADR-2341): each proxy kind
//! fits the expected shape, the proxies stay out of the drawn model, the cook is deterministic, and
//! a prop spawned with `AuthoredCollision` from a real cook answers a raycast with its proxy, not
//! its render mesh.
//!
//! The expectations are elementary geometry of the fixtures built here.

use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_asset::collision_set::CollisionAxis;
use regia_asset::{
    AssetId, AssetServer, AssetServerSettings, COLLISION_SET_SUB_PATH, COLLISION_SET_TYPE_PATH,
    CollisionProxyKind, CookedCollisionGeometry, CookedCollisionSet, CookedModel,
};
use regia_asset_pipeline::{
    CookOptions, Forward, TextureCookQuality, cook_project, host_platform, import_gltf_glb_bytes,
};
use regia_core::math::Vec3;
use regia_physics::{
    AuthoredCollision, AuthoredCollisionStatus, Collider, ColliderShape, PhysicsPlugin, PhysicsWorld,
    QueryFilter, RigidBody, WorldQueries,
};
use regia_scene::Transform;

#[path = "support/collision_glb.rs"]
mod collision_glb;
use collision_glb::{Node, cuboid, glb, octahedron};

/// The render mesh every fixture draws: a thin pole 2 m tall.
fn pole() -> Node {
    Node {
        name: "Crate",
        translation: [0.0; 3],
        mesh: Some(cuboid([-0.05, 0.0, -0.05], [0.05, 2.0, 0.05])),
    }
}

/// A prop with one proxy of every kind (and one name with no kind).
fn every_kind_glb() -> Vec<u8> {
    let mut cube = cuboid([0.0; 3], [1.0; 3]);
    cube.0.push([0.5, 0.5, 0.5]); // interior: not a hull point
    glb(&[
        pole(),
        Node {
            name: "collision_box_lid",
            translation: [0.0; 3],
            mesh: Some(cuboid([-0.5, 0.0, -0.5], [0.5, 1.0, 0.5])),
        },
        Node {
            name: "collision_sphere_ball",
            translation: [2.0, 0.0, 0.0],
            mesh: Some(octahedron(0.25)),
        },
        Node {
            name: "collision_capsule_post",
            translation: [0.0, 0.0, 3.0],
            mesh: Some(cuboid([-0.1, -1.0, -0.1], [0.1, 1.0, 0.1])),
        },
        Node {
            name: "collision_convex_body",
            translation: [-2.0, 0.0, 0.0],
            mesh: Some(cube),
        },
        Node {
            name: "collision_mesh_floor",
            translation: [0.0, -1.0, 0.0],
            mesh: Some(cuboid([-3.0, -0.1, -3.0], [3.0, 0.0, 3.0])),
        },
        Node {
            name: "collision_hull",
            translation: [0.0; 3],
            mesh: Some(cuboid([0.0; 3], [0.5; 3])),
        },
    ])
}

fn parent() -> AssetId {
    AssetId::sub_asset(AssetId::default(), "collision-proxies-test")
}

fn import(bytes: &[u8]) -> regia_asset_pipeline::GltfImportResult {
    import_gltf_glb_bytes(bytes, parent(), Forward::Keep, "linux", TextureCookQuality::Low).expect("import")
}

fn close(a: [f32; 3], b: [f32; 3]) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-5)
}

#[test]
fn every_proxy_kind_cooks_to_its_shape() {
    let imported = import(&every_kind_glb());
    let set = imported.collision.as_ref().expect("a collision set");
    let names: Vec<&str> = set.shapes.iter().map(|s| s.node.as_str()).collect();
    assert_eq!(
        names,
        [
            "collision_box_lid",
            "collision_sphere_ball",
            "collision_capsule_post",
            "collision_convex_body",
            "collision_mesh_floor",
            "collision_hull"
        ]
    );
    let [lid, ball, post, body, floor, hull] = &set.shapes[..] else {
        panic!("six shapes")
    };

    let CookedCollisionGeometry::Box(b) = &lid.geometry else {
        panic!("{:?}", lid.geometry)
    };
    assert!(close(b.half_extents, [0.5, 0.5, 0.5]), "{:?}", b.half_extents);
    assert!(close(lid.translation, [0.0, 0.5, 0.0]), "{:?}", lid.translation);

    assert_eq!(
        ball.geometry,
        CookedCollisionGeometry::Sphere(regia_asset::collision_set::CollisionSphere { radius: 0.25 })
    );
    assert!(close(ball.translation, [2.0, 0.0, 0.0]));

    let CookedCollisionGeometry::Capsule(c) = &post.geometry else {
        panic!("{:?}", post.geometry)
    };
    let corner = (0.1f32 * 0.1 * 2.0).sqrt();
    assert_eq!(c.axis, CollisionAxis::Y);
    assert!(
        (c.radius - corner).abs() < 1e-5 && (c.half_height - (1.0 - corner)).abs() < 1e-5,
        "{c:?}"
    );
    assert!(close(post.translation, [0.0, 0.0, 3.0]));

    let CookedCollisionGeometry::Convex(hull_points) = &body.geometry else {
        panic!("{:?}", body.geometry)
    };
    assert_eq!(hull_points.points.len(), 8, "the interior point is dropped");
    assert!(hull_points.points.iter().all(|p| (-2.0..=-1.0).contains(&p[0])));

    let CookedCollisionGeometry::TriMesh(m) = &floor.geometry else {
        panic!("{:?}", floor.geometry)
    };
    assert_eq!((m.vertices.len(), m.indices.len()), (8, 12));
    assert!(
        m.vertices
            .iter()
            .all(|v| (-1.1 - 1e-6..=-1.0 + 1e-6).contains(&v[1]))
    );

    // No kind in the name: the old meaning, a hand-made hull piece.
    assert_eq!(hull.geometry.kind(), CollisionProxyKind::Convex);
    for kind in CollisionProxyKind::ALL {
        let want = if kind == CollisionProxyKind::Convex { 2 } else { 1 };
        assert_eq!(set.count(kind), want, "{kind:?}");
    }
}

#[test]
fn the_set_is_a_sub_asset_and_proxies_stay_out_of_the_model() {
    let imported = import(&every_kind_glb());
    let collision: Vec<_> = imported
        .sub_assets
        .iter()
        .filter(|s| s.type_path == COLLISION_SET_TYPE_PATH)
        .collect();
    assert_eq!(collision.len(), 1);
    assert_eq!(collision[0].path, COLLISION_SET_SUB_PATH);
    assert_eq!(
        collision[0].id,
        AssetId::sub_asset(parent(), COLLISION_SET_SUB_PATH)
    );
    let decoded = CookedCollisionSet::from_postcard(&collision[0].payload).expect("decode");
    assert_eq!(Some(&decoded), imported.collision.as_ref());
    assert!(
        imported
            .meta_entries
            .iter()
            .any(|e| e.path == COLLISION_SET_SUB_PATH && e.type_path == COLLISION_SET_TYPE_PATH)
    );
    // DO-6: the drawn model holds the render mesh only, and no cooked mesh is a proxy.
    let model = imported
        .sub_assets
        .iter()
        .find(|s| s.type_path == "regia_asset::CookedModel")
        .expect("model");
    let model = CookedModel::from_postcard(&model.payload).expect("model decodes");
    let nodes: Vec<&str> = model.parts.iter().map(|p| p.node.as_str()).collect();
    assert_eq!(nodes, ["Crate"]);
    assert!(imported.sub_assets.iter().all(|s| !s.path.contains("collision_")));
}

#[test]
fn the_cook_is_deterministic() {
    let bytes = every_kind_glb();
    let a = import(&bytes);
    let b = import(&bytes);
    let payload = |r: &regia_asset_pipeline::GltfImportResult| {
        r.sub_assets
            .iter()
            .find(|s| s.type_path == COLLISION_SET_TYPE_PATH)
            .map(|s| s.payload.clone())
    };
    assert_eq!(payload(&a), payload(&b));
    assert!(payload(&a).is_some());
}

#[test]
fn an_asset_without_proxies_has_no_collision_sub_asset() {
    let imported = import(&glb(&[pole()]));
    assert!(imported.collision.is_none());
    assert!(
        imported
            .sub_assets
            .iter()
            .all(|s| s.type_path != COLLISION_SET_TYPE_PATH)
    );
    assert!(
        imported
            .meta_entries
            .iter()
            .all(|e| e.path != COLLISION_SET_SUB_PATH)
    );
}

/// A static prop spawned with `AuthoredCollision` from a real cook: the ray finds the proxy box
/// (top at 1 m, 1 m wide), not the 2 m render pole; a prop without proxies gets no collider.
#[test]
fn a_raycast_against_a_spawned_prop_hits_the_proxy_not_the_render_mesh() {
    let root = regia_test_utils::unique_temp_dir("collision-proxies-e2e");
    let props = root.join("assets/props");
    std::fs::create_dir_all(&props).expect("dirs");
    let with_proxy = glb(&[
        pole(),
        Node {
            name: "collision_box_crate",
            translation: [0.0; 3],
            mesh: Some(cuboid([-0.5, 0.0, -0.5], [0.5, 1.0, 0.5])),
        },
    ]);
    std::fs::write(props.join("crate.glb"), &with_proxy).expect("write");
    std::fs::write(props.join("plain.glb"), glb(&[pole()])).expect("write");
    let report = cook_project(CookOptions::for_host(root.clone())).expect("cook");
    assert!(report.errors.is_empty(), "{:?}", report.errors);

    let server = AssetServer::open(AssetServerSettings {
        project_root: root.clone(),
        platform: host_platform().to_string(),
        content_dir: None,
    })
    .expect("asset server");
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed)
        .insert_resource(server)
        .add_plugins(PhysicsPlugin);
    let prop = app
        .world_mut()
        .spawn((
            Transform::IDENTITY,
            RigidBody::static_body(),
            AuthoredCollision::new("assets/props/crate.glb"),
        ))
        .id();
    let plain = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(10.0, 0.0, 0.0)),
            RigidBody::static_body(),
            AuthoredCollision::new("assets/props/plain.glb"),
        ))
        .id();
    for _ in 0..6 {
        app.update();
    }

    assert_eq!(
        app.world().get::<AuthoredCollisionStatus>(prop),
        Some(&AuthoredCollisionStatus::Attached {
            shapes: 1,
            trimesh_as_hull: 0
        })
    );
    let collider = app.world().get::<Collider>(prop).expect("collider");
    assert!(
        matches!(collider.shape, ColliderShape::Box { .. }),
        "{:?}",
        collider.shape
    );
    assert_eq!(
        app.world().get::<AuthoredCollisionStatus>(plain),
        Some(&AuthoredCollisionStatus::NoProxies)
    );
    assert!(app.world().get::<Collider>(plain).is_none());

    let queries = WorldQueries {
        world: app.world().resource::<PhysicsWorld>(),
        layers: None,
    };
    // Straight down over the pole: the pole's top (2 m) is not a collider; the proxy's (1 m) is.
    let hit = queries
        .raycast(Vec3::new(0.0, 10.0, 0.0), Vec3::NEG_Y, 20.0, QueryFilter::new())
        .expect("the ray hits the proxy");
    assert_eq!(hit.entity, prop);
    assert!((hit.distance - 9.0).abs() < 1e-3, "{}", hit.distance);
    // Beside the pole, inside the proxy's footprint: still the proxy.
    let beside = queries
        .raycast(Vec3::new(0.3, 10.0, -0.3), Vec3::NEG_Y, 20.0, QueryFilter::new())
        .expect("the proxy is 1 m wide");
    assert!((beside.distance - 9.0).abs() < 1e-3, "{}", beside.distance);
    // Past the proxy's footprint: nothing (the prop without proxies has no collider either).
    assert!(
        queries
            .raycast(Vec3::new(0.7, 10.0, 0.0), Vec3::NEG_Y, 20.0, QueryFilter::new())
            .is_none()
    );
    assert!(
        queries
            .raycast(Vec3::new(10.0, 10.0, 0.0), Vec3::NEG_Y, 20.0, QueryFilter::new())
            .is_none()
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Mutated GLBs with every proxy kind go through the whole import (parse, proxy fit, set encode)
/// without a panic: each returns a result or an error.
#[test]
fn importing_mutated_proxy_glbs_never_panics() {
    let fixture = every_kind_glb();
    regia_test_utils::fuzz::fuzz_loader(&fixture, 0xC011, 600, |bytes| {
        let parent = AssetId::sub_asset(AssetId::default(), "collision-fuzz");
        import_gltf_glb_bytes(bytes, parent, Forward::Convert, "linux", TextureCookQuality::Low).map(
            |imported| {
                imported.collision.map(|set| {
                    set.validate().expect("a cooked set always validates");
                    set.shapes.len()
                })
            },
        )
    });
}
