// SPDX-License-Identifier: MPL-2.0
//! Camera driver acceptance (`docs/specs/35-camera-framework.md` § 12): parity of the four
//! legacy-mode presets with the `CameraRig` update on a 600-frame scripted path (items 11), the
//! driver disabling the legacy update on its own entity (item 12), layer events and the shake
//! marker. Poses are compared as numbers; no golden image is involved.

use std::time::Duration;

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_framework::camera::asset::CameraAsset;
use regia_framework::camera::director::CameraHooks;
use regia_framework::camera::driver::{CameraAssets, CameraDriver, CameraLayerEvent};
use regia_framework::camera::presets;
use regia_framework::camera::runtime::{LayerEventKind, OverlayOptions};
use regia_framework::camera::{
    CameraMode, CameraPlugin, CameraRig, CameraRuntime, ChaseCamera, CinematicCamera, FirstPerson, Free,
    FreeLookInput, Overhead, ThirdPerson,
};
use regia_scene::Transform;

const STEP: Duration = Duration::from_millis(16);
const FRAMES: usize = 600;
const POSITION_TOLERANCE: f32 = 1.0e-4;
const ROTATION_TOLERANCE: f32 = 1.0e-4;

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, CameraPlugin, regia_physics::PhysicsPlugin))
        .insert_resource(TimeUpdateStrategy::ManualDuration(STEP));
    app
}

fn register(app: &mut App, asset: CameraAsset) -> String {
    let name = asset.name.clone();
    let status = app
        .world_mut()
        .resource_mut::<CameraAssets>()
        .insert(asset, None, &CameraHooks::default());
    assert_ne!(
        status,
        regia_framework::camera::asset::CameraBuildStatus::Errors,
        "the asset under test is valid"
    );
    name
}

fn bind(app: &mut App, camera: Entity) {
    app.world_mut()
        .resource_scope(|world, assets: Mut<CameraAssets>| {
            world
                .get_mut::<CameraDriver>(camera)
                .expect("driver")
                .bind(&assets);
        });
}

fn set_slot(app: &mut App, camera: Entity, slot: &str, yaw: f32, pitch: f32) {
    let mut driver = app.world_mut().get_mut::<CameraDriver>(camera).expect("driver");
    let core = driver.core_mut().expect("bound driver");
    assert!(core.inputs.set_angles(slot, yaw, pitch));
}

fn spawn_wall(app: &mut App, centre: Vec3, half_extents: Vec3) {
    app.world_mut().spawn((
        Transform {
            translation: centre,
            ..Transform::default()
        },
        regia_physics::RigidBody {
            kind: regia_physics::BodyKind::Static,
            ..Default::default()
        },
        regia_physics::Collider {
            shape: regia_physics::ColliderShape::Box { half_extents },
            ..Default::default()
        },
    ));
}

/// The scripted target pose and look angles of frame `i`.
fn script(i: usize) -> (Vec3, Quat, f32, f32) {
    let t = i as f32;
    let position = Vec3::new(3.0 * (t * 0.02).sin(), 0.5 * (t * 0.013).sin(), -0.05 * t);
    let rotation = Quat::from_rotation_y(t * 0.01);
    let yaw = 0.6 * (t * 0.017).sin() + t * 0.003;
    let pitch = 0.3 * (t * 0.011).sin();
    (position, rotation, yaw, pitch)
}

/// The angle between two rotations, precise for tiny angles (`angle_between` bottoms out near
/// 4e-4 rad in f32).
fn angle_between(a: Quat, b: Quat) -> f32 {
    let delta = a * b.inverse();
    2.0 * Vec3::new(delta.x, delta.y, delta.z).length().min(1.0).asin()
}

fn assert_same_pose(frame: usize, legacy: &Transform, driven: &Transform) {
    let moved = (legacy.translation - driven.translation).length();
    let turned = angle_between(legacy.rotation, driven.rotation);
    assert!(
        moved <= POSITION_TOLERANCE,
        "frame {frame}: position differs by {moved} ({:?} vs {:?})",
        legacy.translation,
        driven.translation
    );
    assert!(
        turned <= ROTATION_TOLERANCE,
        "frame {frame}: rotation differs by {turned} rad (legacy {:?} vs driven {:?})",
        legacy.rotation,
        driven.rotation
    );
}

/// Runs a legacy camera and a preset driver side by side over the scripted path.
fn parity(mode: CameraMode, asset: CameraAsset, with_wall: bool) {
    let mut app = app();
    let target = app.world_mut().spawn(Transform::default()).id();
    if with_wall {
        spawn_wall(&mut app, Vec3::new(0.0, 2.0, 3.0), Vec3::new(4.0, 4.0, 0.25));
    }
    // A few frames for the colliders to reach the physics world.
    for _ in 0..6 {
        app.update();
    }
    let start = Transform {
        translation: Vec3::new(1.0, 2.0, 8.0),
        rotation: Quat::from_rotation_y(0.4),
        ..Transform::default()
    };
    let legacy = app
        .world_mut()
        .spawn((
            start,
            CameraRig {
                mode,
                ..CameraRig::default()
            },
            ChaseCamera { target },
            // The legacy update inserts its runtime state through `Commands`, so its first frame
            // runs without smoothing state; start it with the state a steady-state frame has.
            CameraRuntime::default(),
        ))
        .id();
    let name = register(&mut app, asset);
    let driven = app
        .world_mut()
        .spawn((start, CameraDriver::new(name).with_target(target)))
        .id();
    bind(&mut app, driven);

    for i in 0..FRAMES {
        let (position, rotation, yaw, pitch) = script(i);
        {
            let mut t = app.world_mut().get_mut::<Transform>(target).expect("target");
            t.translation = position;
            t.rotation = rotation;
        }
        {
            let mut rig = app.world_mut().get_mut::<CameraRig>(legacy).expect("rig");
            rig.yaw = yaw;
            rig.pitch = pitch;
        }
        set_slot(&mut app, driven, "look", yaw, pitch);
        app.update();
        let a = *app.world().get::<Transform>(legacy).expect("legacy transform");
        let b = *app.world().get::<Transform>(driven).expect("driven transform");
        assert_same_pose(i, &a, &b);
    }
}

#[test]
fn third_person_preset_matches_the_legacy_rig_with_lag_lean_and_boom_collision() {
    // Short lags and a stronger lean make the comparison sensitive to the integration order.
    let mode = ThirdPerson {
        socket_offset: Vec3::new(0.0, 1.5, 0.0),
        target_offset: Vec3::new(0.0, 0.8, 0.0),
        lag_speed: 6.0,
        rotation_lag: 9.0,
        ..ThirdPerson::default()
    };
    parity(
        CameraMode::ThirdPerson(mode),
        presets::third_person_from(&mode),
        true,
    );
}

#[test]
fn third_person_preset_matches_the_default_parameters() {
    parity(
        CameraMode::ThirdPerson(ThirdPerson::default()),
        presets::third_person(),
        true,
    );
}

#[test]
fn overhead_preset_matches_the_legacy_rig() {
    let mode = Overhead {
        pitch: -1.2,
        ..Overhead::default()
    };
    parity(CameraMode::Overhead(mode), presets::overhead_from(&mode), false);
    let steep = Overhead {
        pitch: -1.5,
        ..Overhead::default()
    };
    parity(CameraMode::Overhead(steep), presets::overhead_from(&steep), false);
}

/// Exactly straight down the legacy look-at is degenerate and drops the rig's yaw (the satellite
/// stays north-up whatever the yaw says); the preset keeps the yaw, as the legacy docs describe
/// ("the rig's yaw spins the satellite's orientation"). ADR-1637.
#[test]
fn overhead_preset_straight_down_keeps_the_rig_yaw() {
    let mut app = app();
    let target = app.world_mut().spawn(Transform::default()).id();
    let name = register(&mut app, presets::overhead());
    let camera = app
        .world_mut()
        .spawn((Transform::default(), CameraDriver::new(name).with_target(target)))
        .id();
    bind(&mut app, camera);
    set_slot(&mut app, camera, "look", 0.7, -0.3);
    app.update();
    let (yaw, pitch, _) = app
        .world()
        .get::<Transform>(camera)
        .expect("transform")
        .rotation
        .to_euler(regia_core::math::EulerRot::YXZ);
    assert!((yaw - 0.7).abs() < 1.0e-4, "yaw kept: {yaw}");
    assert!(
        (pitch + std::f32::consts::FRAC_PI_2).abs() < 1.0e-4,
        "pitch fixed straight down: {pitch}"
    );
}

#[test]
fn first_person_preset_matches_the_legacy_rig() {
    parity(
        CameraMode::FirstPerson(FirstPerson::default()),
        presets::first_person(),
        false,
    );
}

#[test]
fn free_preset_matches_the_legacy_rig() {
    let mut app = app();
    let start = Transform {
        translation: Vec3::new(0.0, 3.0, 0.0),
        ..Transform::default()
    };
    let mode = Free::default();
    let legacy = app
        .world_mut()
        .spawn((
            start,
            CameraRig {
                mode: CameraMode::Free(mode),
                ..CameraRig::default()
            },
            CameraRuntime::default(),
        ))
        .id();
    let name = register(&mut app, presets::free_from(&mode));
    let driven = app.world_mut().spawn((start, CameraDriver::new(name))).id();
    bind(&mut app, driven);
    let (mut yaw, mut pitch) = (0.0f32, 0.0f32);
    for i in 0..FRAMES {
        let t = i as f32;
        let look = regia_core::math::Vec2::new(0.01 * (t * 0.05).sin(), 0.004 * (t * 0.03).cos());
        let axes = regia_core::math::Vec2::new((t * 0.07).sin(), 0.5 + 0.5 * (t * 0.02).cos());
        let rise = if i % 90 < 20 { 1.0 } else { 0.0 };
        let sprint = i % 200 < 60;
        *app.world_mut().resource_mut::<FreeLookInput>() = FreeLookInput {
            look,
            move_axes: axes,
            rise,
            sprint,
        };
        // The legacy rig turns by the look delta; the driver's slot carries the resulting angles.
        yaw = regia_framework::camera::nodes::wrap_pi(yaw - look.x);
        pitch = (pitch + look.y).clamp(-std::f32::consts::FRAC_PI_2, std::f32::consts::FRAC_PI_2);
        set_slot(&mut app, driven, "look", yaw, pitch);
        {
            let mut driver = app.world_mut().get_mut::<CameraDriver>(driven).expect("driver");
            let core = driver.core_mut().expect("bound");
            core.set_input("move", [axes.x, axes.y]).expect("move slot");
            core.set_var("rise", regia_framework::camera::asset::VarValue::Float(rise))
                .expect("rise");
            core.set_var("sprint", regia_framework::camera::asset::VarValue::Bool(sprint))
                .expect("sprint");
        }
        app.update();
        let a = *app.world().get::<Transform>(legacy).expect("legacy");
        let b = *app.world().get::<Transform>(driven).expect("driven");
        assert_same_pose(i, &a, &b);
    }
}

#[test]
fn a_driver_disables_the_legacy_update_on_its_entity_only() {
    let mut app = app();
    let target = app.world_mut().spawn(Transform::default()).id();
    let start = Transform {
        translation: Vec3::new(5.0, 6.0, 7.0),
        ..Transform::default()
    };
    let rig = CameraRig {
        mode: CameraMode::ThirdPerson(ThirdPerson {
            lag_speed: 1.0e6,
            rotation_lag: 1.0e6,
            ..ThirdPerson::default()
        }),
        ..CameraRig::default()
    };
    let with_driver = app
        .world_mut()
        // The asset name matches nothing, so the driver never binds and never writes.
        .spawn((start, rig, ChaseCamera { target }, CameraDriver::new("missing")))
        .id();
    let plain = app.world_mut().spawn((start, rig, ChaseCamera { target })).id();
    for _ in 0..3 {
        app.update();
    }
    assert_eq!(
        app.world()
            .get::<Transform>(with_driver)
            .expect("driven")
            .translation,
        start.translation,
        "the legacy rig did not move the camera that has a driver"
    );
    assert!(
        app.world()
            .get::<Transform>(plain)
            .expect("plain")
            .translation
            .abs_diff_eq(Vec3::new(0.0, 0.0, 5.0), 1.0e-3),
        "the legacy rig still moves the other camera"
    );
}

#[derive(Resource, Default)]
struct Seen(Vec<(LayerEventKind, String)>);

fn collect(mut reader: MessageReader<CameraLayerEvent>, mut seen: ResMut<Seen>) {
    for event in reader.read() {
        seen.0.push((event.kind, event.rig.clone()));
    }
}

#[test]
fn layer_events_arrive_as_messages_and_the_camera_feels_shakes() {
    let mut app = app();
    app.init_resource::<Seen>().add_systems(Last, collect);
    let asset = CameraAsset::from_ron(
        r#"(format_version: 1, name: "ev",
            rigs: [
                (name: "main", root: [Fov(degrees: Literal(60.0))]),
                (name: "cam", root: [Fov(degrees: Literal(30.0))]),
            ],
            director: Fixed("main"))"#,
    )
    .expect("asset parses");
    let name = register(&mut app, asset);
    let camera = app
        .world_mut()
        .spawn((Transform::default(), CameraDriver::new(name)))
        .id();
    app.update();
    {
        let mut driver = app.world_mut().get_mut::<CameraDriver>(camera).expect("driver");
        let core = driver.core_mut().expect("bound after the first frame");
        core.push_overlay("cam", OverlayOptions::default()).expect("push");
    }
    app.update();
    app.update();
    let seen = &app.world().resource::<Seen>().0;
    assert_eq!(seen.first(), Some(&(LayerEventKind::Entered, "cam".to_owned())));
    assert!(seen.iter().any(|(kind, _)| *kind == LayerEventKind::Cut));
    assert!(
        app.world().get::<CinematicCamera>(camera).is_some(),
        "a driven camera feels the director's shakes"
    );
}

#[test]
fn an_asset_with_errors_never_loads_and_the_previous_good_version_keeps_running() {
    let mut app = app();
    let good = CameraAsset::from_ron(
        r#"(format_version: 1, name: "g", rigs: [(name: "a", root: [Fov(degrees: Literal(45.0))])], director: Fixed("a"))"#,
    )
    .expect("parses");
    let name = register(&mut app, good.clone());
    let camera = app
        .world_mut()
        .spawn((Transform::default(), CameraDriver::new(name.clone())))
        .id();
    app.update();
    let mut bad = good;
    bad.director = regia_framework::camera::asset::DirectorDef::Fixed("ghost".into());
    let status = app
        .world_mut()
        .resource_mut::<CameraAssets>()
        .insert(bad, None, &CameraHooks::default());
    assert_eq!(status, regia_framework::camera::asset::CameraBuildStatus::Errors);
    app.update();
    let assets = app.world().resource::<CameraAssets>();
    assert!(
        matches!(
            assets.active(&name).expect("active").director,
            regia_framework::camera::asset::DirectorDef::Fixed(ref rig) if rig == "a"
        ),
        "the last good version is still the one drivers run"
    );
    assert!(
        app.world()
            .get::<CameraDriver>(camera)
            .expect("driver")
            .core()
            .is_some()
    );
}

/// Spec 35 § 8: a driver evaluates only while `enabled`; it starts as soon as it is enabled and
/// its asset is valid (lane spec-15-35, ADR-5778).
#[test]
fn a_disabled_driver_leaves_the_camera_alone_until_it_is_enabled() {
    let mut app = app();
    let asset = CameraAsset::from_ron(
        r#"(format_version: 1, name: "fixed_point",
            rigs: [(name: "a", root: [Pivot(target: Point((1.0, 2.0, 3.0)))])],
            director: Fixed("a"))"#,
    )
    .expect("parses");
    let name = register(&mut app, asset);
    let start = Transform {
        translation: Vec3::new(-4.0, 0.5, 9.0),
        ..Transform::default()
    };
    let mut driver = CameraDriver::new(name);
    driver.enabled = false;
    let camera = app.world_mut().spawn((start, driver)).id();
    for _ in 0..3 {
        app.update();
    }
    let at = |app: &App| app.world().get::<Transform>(camera).expect("camera").translation;
    assert_eq!(at(&app), start.translation, "a disabled driver writes nothing");
    assert!(
        app.world()
            .get::<CameraDriver>(camera)
            .expect("driver")
            .core()
            .is_none(),
        "a disabled driver is not even bound"
    );
    app.world_mut()
        .get_mut::<CameraDriver>(camera)
        .expect("driver")
        .enabled = true;
    app.update();
    assert!(
        at(&app).abs_diff_eq(Vec3::new(1.0, 2.0, 3.0), 1.0e-5),
        "enabled: evaluated on the next frame: {:?}",
        at(&app)
    );
    app.world_mut()
        .get_mut::<CameraDriver>(camera)
        .expect("driver")
        .enabled = false;
    app.world_mut()
        .get_mut::<Transform>(camera)
        .expect("camera")
        .translation = start.translation;
    app.update();
    assert_eq!(
        at(&app),
        start.translation,
        "disabled again: the camera stays where it is put"
    );
}
