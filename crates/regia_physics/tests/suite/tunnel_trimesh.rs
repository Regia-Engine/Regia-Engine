// SPDX-License-Identifier: MPL-2.0
//! Mesh-tunnel trimesh: a ray through the tunnel mouth misses the walls (open path).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::Vec3;
use regia_physics::{Collider, ColliderShape, PhysicsPlugin, PhysicsQuery, QueryFilter, RigidBody};
use regia_scene::Transform;
use regia_terrain::{CollisionProvenance, HoleEllipse, MeshTunnel, TunnelDetail};

fn physics_app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed).add_plugins(PhysicsPlugin);
    app
}

fn box_tunnel_collision() -> (Vec<[f32; 3]>, Vec<u32>) {
    let (w, h, l) = (4.0f32, 3.0f32, 20.0f32);
    let p = [
        [-w, 0.0, -l],
        [w, 0.0, -l],
        [w, 0.0, l],
        [-w, 0.0, l],
        [-w, h, -l],
        [w, h, -l],
        [w, h, l],
        [-w, h, l],
    ];
    let mut indices = Vec::new();
    indices.extend_from_slice(&[0, 3, 1, 1, 3, 2]);
    indices.extend_from_slice(&[4, 5, 7, 5, 6, 7]);
    indices.extend_from_slice(&[0, 4, 7, 0, 7, 3]);
    indices.extend_from_slice(&[1, 2, 6, 1, 6, 5]);
    (p.to_vec(), indices)
}

struct Probe {
    mouth_clear: bool,
    wall_x: Option<f32>,
}

#[test]
fn ray_through_tunnel_mouth_is_clear() {
    let (pos, idx) = box_tunnel_collision();
    let tunnel = MeshTunnel::from_collision(
        &pos,
        &idx,
        HoleEllipse {
            center: [0.0, -20.0],
            radius: [5.0, 4.0],
            rotation_radians: 0.0,
        },
        TunnelDetail {
            normal_displace: 0.0,
            noise_amplitude: 0.0,
            ..TunnelDetail::default()
        },
        CollisionProvenance {
            source_asset: "collision_tunnel_test".into(),
            triangle_count: (idx.len() / 3) as u32,
            note: "physics e2e".into(),
        },
    )
    .expect("tunnel");

    let shape = ColliderShape::trimesh_from_triangles(&tunnel.collision_vertices, &tunnel.collision_indices)
        .expect("trimesh");

    let slot: Arc<Mutex<Option<Probe>>> = Arc::new(Mutex::new(None));
    let mut app = physics_app();
    app.world_mut().spawn((
        Transform::IDENTITY,
        RigidBody::static_body(),
        Collider {
            shape,
            friction: 0.8,
            ..Collider::default()
        },
    ));
    let slot2 = slot.clone();
    app.add_systems(regia_app::FixedUpdate, move |queries: PhysicsQuery| {
        let mouth_clear = queries
            .raycast(Vec3::new(0.0, 1.5, -25.0), Vec3::Z, 50.0, QueryFilter::new())
            .is_none();
        let wall_x = queries
            .raycast(Vec3::new(0.0, 1.5, 0.0), -Vec3::X, 20.0, QueryFilter::new())
            .map(|h| h.point.x);
        *slot2.lock().expect("slot") = Some(Probe { mouth_clear, wall_x });
    });
    app.update();
    app.update();

    let probe = slot.lock().expect("slot").take().expect("system ran");
    assert!(
        probe.mouth_clear,
        "ray through the open tunnel mouth must not hit walls"
    );
    let wall_x = probe.wall_x.expect("ray toward the left wall must hit");
    assert!((wall_x + 4.0).abs() < 0.2, "left wall at x ≈ -4, got {wall_x}");
}

#[test]
fn trimesh_from_triangles_rejects_bad_indices() {
    assert!(ColliderShape::trimesh_from_triangles(&[[0.0; 3]], &[0, 1]).is_none());
    assert!(ColliderShape::trimesh_from_triangles(&[], &[0, 1, 2]).is_none());
}

/// Open mouth at −Z, cap at +Z so a long ray down the bore hits the dead end.
fn closed_box_tunnel() -> (Vec<[f32; 3]>, Vec<u32>) {
    let (positions, mut indices) = box_tunnel_collision();
    // Far cap, normal toward −Z (incoming ray travels +Z). Vertices 3, 7, 2, 6.
    indices.extend_from_slice(&[3, 7, 2, 2, 7, 6]);
    (positions, indices)
}

struct HoleProbe {
    mouth_clear: bool,
    beside_hit: bool,
    cap_z: Option<f32>,
}

#[test]
fn ray_through_hole_misses_terrain_and_hits_tunnel_cap() {
    // Parry's heightfield stores rows along Z and columns along X, column-major
    // (`Array2` index = iz + ix * nrows). `shape_to_shared_shape` passes `nx` as
    // the row count, so `nx` is the Z sample count here.
    let nx = 17usize;
    let nz = 17usize;
    let scale = Vec3::new(40.0, 1.0, 40.0);
    let mouth = HoleEllipse {
        center: [0.0, 0.0],
        radius: [6.0, 22.0],
        rotation_radians: 0.0,
    };
    let mask = regia_terrain::HolesMask::new(vec![mouth.clone()]).expect("mask");
    let mut heights = vec![0.0f32; nx * nz];
    for iz in 0..nx {
        for ix in 0..nz {
            let z = (-0.5 + iz as f32 / (nx - 1) as f32) * scale.z;
            let x = (-0.5 + ix as f32 / (nz - 1) as f32) * scale.x;
            let hillside = (z + scale.z * 0.5) / scale.z * 30.0;
            let h = if mask.contains(x, z) { -1.0 } else { hillside };
            heights[iz + ix * nx] = h;
        }
    }
    let heightfield = ColliderShape::HeightField {
        nx,
        nz,
        heights,
        scale,
    };

    let (pos, idx) = closed_box_tunnel();
    let tunnel = MeshTunnel::from_collision(
        &pos,
        &idx,
        mouth,
        TunnelDetail {
            normal_displace: 0.0,
            noise_amplitude: 0.0,
            ..TunnelDetail::default()
        },
        CollisionProvenance {
            source_asset: "collision_tunnel_capped".into(),
            triangle_count: (idx.len() / 3) as u32,
            note: "heightfield hole + capped trimesh".into(),
        },
    )
    .expect("tunnel");
    let trimesh =
        ColliderShape::trimesh_from_triangles(&tunnel.collision_vertices, &tunnel.collision_indices)
            .expect("trimesh");

    let slot: Arc<Mutex<Option<HoleProbe>>> = Arc::new(Mutex::new(None));
    let mut app = physics_app();
    app.world_mut().spawn((
        Transform::IDENTITY,
        RigidBody::static_body(),
        Collider {
            shape: heightfield,
            friction: 0.8,
            ..Collider::default()
        },
    ));
    app.world_mut().spawn((
        Transform::IDENTITY,
        RigidBody::static_body(),
        Collider {
            shape: trimesh,
            friction: 0.8,
            ..Collider::default()
        },
    ));
    let slot2 = slot.clone();
    app.add_systems(regia_app::FixedUpdate, move |queries: PhysicsQuery| {
        let mouth_clear = queries
            .raycast(Vec3::new(0.0, 1.5, -30.0), Vec3::Z, 16.0, QueryFilter::new())
            .is_none();
        let beside_hit = queries
            .raycast(Vec3::new(10.0, 1.5, -30.0), Vec3::Z, 40.0, QueryFilter::new())
            .is_some();
        let cap_z = queries
            .raycast(Vec3::new(0.0, 1.5, -30.0), Vec3::Z, 80.0, QueryFilter::new())
            .map(|hit| hit.point.z);
        *slot2.lock().expect("slot") = Some(HoleProbe {
            mouth_clear,
            beside_hit,
            cap_z,
        });
    });
    app.update();
    app.update();

    let probe = slot.lock().expect("slot").take().expect("system ran");
    assert!(
        probe.mouth_clear,
        "short ray into the punched mouth must miss terrain and the cap"
    );
    assert!(probe.beside_hit, "ray into solid ground beside the hole must hit");
    let cap_z = probe.cap_z.expect("long ray must hit the far cap");
    assert!((cap_z - 20.0).abs() < 0.5, "far cap at z ≈ 20, got {cap_z}");
}
