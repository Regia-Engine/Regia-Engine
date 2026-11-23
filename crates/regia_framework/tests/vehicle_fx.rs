// SPDX-License-Identifier: MPL-2.0
//! `VehicleFxPlugin` on a real ray-cast vehicle (headless, fixed steps, no GPU): a car that rolls
//! on gravel lays ruts into the decal pool and raises dust; on asphalt a straight run makes neither
//! a skid nor smoke; the surface follows the ground entity's override and physics material; the
//! stamp cap holds; switching the feature off clears the marks.
#![cfg(feature = "client")]

use std::time::Duration;

use regia_app::prelude::*;
use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::{Quat, Vec3};
use regia_framework::vehicle_fx::{VehicleFxPlugin, VehicleFxState, WheelSurfaceOverride};
use regia_physics::vehicle::{VehicleConfig, VehicleInput, VehiclePlugin, spawn_vehicle};
use regia_physics::{BodyKind, Collider, ColliderShape, PhysicsMaterialRef, PhysicsPlugin, RigidBody};
use regia_scene::Transform;
use regia_vfx::decal::DecalPool;
use regia_vfx::scene::{VfxLibrary, VfxScene};
use regia_vfx::vehicle_fx::WheelSurface;

fn world(surface_override: Option<WheelSurface>, material: Option<&str>) -> (App, Entity) {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed)
        .add_plugins((PhysicsPlugin, VehiclePlugin, VehicleFxPlugin));
    app.insert_resource(DecalPool::new(256));
    app.insert_resource(VfxScene::new(VfxLibrary::new()));
    let mut ground = app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
        RigidBody {
            kind: BodyKind::Static,
            ..RigidBody::default()
        },
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(300.0, 0.5, 300.0),
            },
            ..Collider::default()
        },
    ));
    if let Some(s) = surface_override {
        ground.insert(WheelSurfaceOverride(s));
    }
    if let Some(m) = material {
        ground.insert(PhysicsMaterialRef(m.to_string()));
    }
    let car = spawn_vehicle(
        app.world_mut(),
        VehicleConfig::default(),
        Vec3::new(0.0, 0.8, 0.0),
        Quat::IDENTITY,
    )
    .expect("spawn");
    (app, car)
}

fn drive(app: &mut App, car: Entity, throttle: f32, steer: f32, frames: usize) {
    app.world_mut()
        .get_mut::<VehicleInput>(car)
        .expect("input")
        .throttle = throttle;
    app.world_mut().get_mut::<VehicleInput>(car).expect("input").steer = steer;
    for _ in 0..frames {
        app.update();
    }
}

#[test]
fn a_car_on_gravel_lays_ruts_and_raises_dust() {
    let (mut app, car) = world(Some(WheelSurface::Gravel), None);
    drive(&mut app, car, 0.0, 0.0, 60);
    drive(&mut app, car, 1.0, 0.0, 240);
    let state = app.world().resource::<VehicleFxState>();
    let wheels = state.wheels();
    assert_eq!(wheels.len(), 4);
    assert!(
        wheels.iter().all(|w| w.surface == WheelSurface::Gravel),
        "{wheels:?}"
    );
    assert!(wheels.iter().any(|w| w.output.dust > 0.0), "{wheels:?}");
    assert!(state.logic().stats().emitted > 20, "{:?}", state.logic().stats());
    let pool = app.world().resource::<DecalPool>();
    assert!(pool.len() > 20, "{} decals", pool.len());
    let scene = app.world().resource::<VfxScene>();
    assert!(scene.instances().count() >= 1, "a dust instance follows a wheel");
}

#[test]
fn a_straight_asphalt_run_makes_no_skid_and_no_smoke() {
    let (mut app, car) = world(None, Some("asphalt_dry"));
    drive(&mut app, car, 0.0, 0.0, 60);
    drive(&mut app, car, 0.6, 0.0, 240);
    let state = app.world().resource::<VehicleFxState>();
    let wheels = state.wheels();
    assert!(
        wheels.iter().all(|w| w.surface == WheelSurface::Asphalt),
        "{wheels:?}"
    );
    assert!(wheels.iter().all(|w| w.output.smoke == 0.0), "{wheels:?}");
    assert_eq!(state.logic().stats().emitted, 0);
    assert_eq!(app.world().resource::<DecalPool>().len(), 0);
}

#[test]
fn the_stamp_cap_holds_and_off_clears_the_marks() {
    let (mut app, car) = world(Some(WheelSurface::Dirt), None);
    app.world_mut().resource_mut::<VehicleFxState>().set_config({
        let mut c = regia_vfx::vehicle_fx::VehicleFxConfig::default();
        c.limits.max_stamps = 16;
        c
    });
    drive(&mut app, car, 0.0, 0.0, 60);
    for _ in 0..8 {
        drive(&mut app, car, 1.0, 0.0, 60);
        let live = app.world().resource::<VehicleFxState>().logic().stats().live;
        assert!(live <= 16, "live {live}");
        assert!(app.world().resource::<DecalPool>().len() <= 16);
    }
    let stats = app.world().resource::<VehicleFxState>().logic().stats();
    assert!(stats.retired_by_cap > 0, "{stats:?}");
    app.world_mut().resource_mut::<VehicleFxState>().enabled = false;
    drive(&mut app, car, 1.0, 0.0, 2);
    assert_eq!(app.world().resource::<VehicleFxState>().logic().stats().live, 0);
    assert_eq!(app.world().resource::<DecalPool>().len(), 0);
}

#[test]
fn the_material_name_and_the_terrain_probe_pick_the_surface() {
    let (mut app, car) = world(None, Some("Muddy_Track"));
    drive(&mut app, car, 0.0, 0.0, 90);
    let surfaces: Vec<_> = app
        .world()
        .resource::<VehicleFxState>()
        .wheels()
        .iter()
        .map(|w| w.surface)
        .collect();
    assert_eq!(surfaces, vec![WheelSurface::Mud; 4]);

    let (mut app, car) = world(None, None);
    app.world_mut().resource_mut::<VehicleFxState>().terrain_probe =
        Some(std::sync::Arc::new(|_| Some(WheelSurface::Sand)));
    drive(&mut app, car, 0.0, 0.0, 90);
    let surfaces: Vec<_> = app
        .world()
        .resource::<VehicleFxState>()
        .wheels()
        .iter()
        .map(|w| w.surface)
        .collect();
    assert_eq!(surfaces, vec![WheelSurface::Sand; 4]);
}

#[test]
fn two_runs_are_identical() {
    let go = || {
        let (mut app, car) = world(Some(WheelSurface::Dirt), None);
        drive(&mut app, car, 0.0, 0.0, 60);
        drive(&mut app, car, 1.0, 0.3, 240);
        let s = app.world().resource::<VehicleFxState>();
        (s.logic().state_digest(), s.logic().stats())
    };
    assert_eq!(go(), go());
}
