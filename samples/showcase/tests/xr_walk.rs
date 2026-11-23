// SPDX-License-Identifier: MIT-0
//! The showcase walk in a headset (`--xr`, ADR-2457) on the XR simulator: no headset.
//!
//! The first tests need no GPU: the session, the rig at the scene camera, ground follow on the example
//! heightfield, no bob, the head as the only audio listener and a desktop camera that never reaches the
//! eyes. The last renders both eyes on the headless device (lavapipe on the CI hosts) and checks they
//! hold the scene with stereo parallax; without any adapter it prints a note and passes, like the other
//! render tests.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use regia::audio::AudioListener;
use regia::prelude::*;
use regia_core::math::{Quat, Vec2, Vec3};
use regia_platform::xr::{Eye, Pose};
use regia_render::{Camera, RenderDevice};
use regia_scene::{GlobalTransform, Transform};
use regia_xr::{
    ComfortPreset, XrActionValue, XrAppState, XrBackendKind, XrHead, XrRenderState, XrRig, XrState, XrWalk,
    drive_input, drive_pose,
};
use showcase::app::{Options, build_headless_app};
use showcase::scene::{SceneData, load_scene};
use showcase::xr::{ShowcaseXrPlugin, XrEars, XrWalkPlan, add_headless_xr, scene_ground};

fn example() -> Arc<SceneData> {
    Arc::new(load_scene(&Path::new(env!("CARGO_MANIFEST_DIR")).join("example_scene")).expect("example scene"))
}

/// A CPU-only app: transforms, the fixed step and the walk (no renderer).
fn cpu_app(scene: &Arc<SceneData>) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(regia_scene::ScenePlugin);
    enable_fixed_step(&mut app, 60.0);
    app.add_plugins(ShowcaseXrPlugin {
        scene: Arc::clone(scene),
        backend: XrBackendKind::Simulator,
        render: false,
        preset: ComfortPreset::Moderate,
    });
    app
}

fn one<C: Component>(app: &mut App) -> Entity {
    let mut q = app.world_mut().query_filtered::<Entity, With<C>>();
    let found: Vec<Entity> = q.iter(app.world()).collect();
    assert_eq!(found.len(), 1, "exactly one {}", std::any::type_name::<C>());
    found[0]
}

fn head_world(app: &mut App) -> Vec3 {
    let rig = one::<XrRig>(app);
    let head = one::<XrHead>(app);
    let r = *app.world().get::<Transform>(rig).expect("rig");
    let h = *app.world().get::<Transform>(head).expect("head");
    showcase::xr::head_world(&r, &h).0
}

#[test]
fn the_walk_starts_at_the_scene_camera_on_the_ground_and_walks_without_bob() {
    let scene = example();
    let ground = scene_ground(&scene);
    let mut app = cpu_app(&scene);
    app.update();
    assert_eq!(app.world().resource::<XrState>().app, XrAppState::Focused);
    assert!(app.world().resource::<XrWalk>().active);
    // A standing user, 1.7 m over the tracking floor, looking straight ahead.
    drive_pose(
        app.world_mut(),
        Some(Pose::new(Vec3::new(0.0, 1.7, 0.0), Quat::IDENTITY)),
        [None, None],
    )
    .expect("pose");
    app.update();
    let plan = *app.world().resource::<XrWalkPlan>();
    let start = head_world(&mut app);
    assert!(
        (Vec2::new(start.x, start.z) - Vec2::new(plan.position.x, plan.position.z)).length() < 1e-3,
        "the head stands over the scene camera: {start} vs {:?}",
        plan.position
    );
    let floor = ground.height(start.x, start.z).expect("on the map");
    assert!((start.y - floor - 1.7).abs() < 1e-3, "{start} over {floor}");

    drive_input(
        app.world_mut(),
        "move",
        "left",
        XrActionValue::Vec2(Vec2::new(0.0, 1.0)),
    )
    .expect("move");
    for _ in 0..120 {
        app.update();
        let head = head_world(&mut app);
        let floor = ground.height(head.x, head.z).expect("on the map");
        assert!(
            (head.y - floor - 1.7).abs() < 1e-3,
            "the eye stays 1.7 m over the terrain (no bob, no noise): {head} over {floor}"
        );
    }
    let end = head_world(&mut app);
    let walked = Vec2::new(end.x - start.x, end.z - start.z).length();
    let distance = app.world().resource::<XrWalk>().distance_m();
    assert!((walked - distance).abs() < 1e-3, "{walked} vs {distance}");
    assert!(walked > 2.0, "two seconds at a walking pace: {walked}");
    // Forward is the scene camera's heading.
    let heading = Quat::from_rotation_y(plan.yaw) * Vec3::NEG_Z;
    let dir = Vec2::new(end.x - start.x, end.z - start.z).normalize();
    assert!(
        dir.dot(Vec2::new(heading.x, heading.z)) > 0.999,
        "{dir} vs {heading}"
    );
}

#[test]
fn the_head_is_the_only_listener_and_the_desktop_camera_never_moves_the_eyes() {
    let scene = example();
    let mut app = cpu_app(&scene);
    // A camera that carries a listener, as a desktop scene would.
    let camera = app
        .world_mut()
        .spawn((
            Camera::default(),
            Transform::from_translation(Vec3::new(1.0, 50.0, 1.0)),
            GlobalTransform::default(),
            AudioListener::default(),
        ))
        .id();
    app.update();
    drive_pose(
        app.world_mut(),
        Some(Pose::new(Vec3::new(0.1, 1.6, -0.2), Quat::from_rotation_y(0.4))),
        [None, None],
    )
    .expect("pose");
    app.update();
    app.update();
    let listener = one::<AudioListener>(&mut app);
    assert_eq!(
        listener,
        one::<XrEars>(&mut app),
        "the head's ears are the listener"
    );
    assert!(app.world().get::<AudioListener>(camera).is_none());
    let listener_at = app
        .world()
        .get::<GlobalTransform>(listener)
        .expect("global")
        .0
        .translation;
    let head = head_world(&mut app);
    assert!(
        (Vec3::from(listener_at) - head).length() < 1e-3,
        "spatial audio is heard from the head pose: {listener_at:?} vs {head}"
    );

    // Move and shake the desktop camera the way the path player and the shock shake do.
    for i in 0..10 {
        let mut t = app.world_mut().get_mut::<Transform>(camera).expect("camera");
        t.translation += Vec3::new(0.3, 0.1 * (i as f32).sin(), 0.0);
        t.rotation = Quat::from_rotation_z(0.05 * (i as f32).cos());
        let before = head_world(&mut app);
        app.update();
        assert_eq!(head_world(&mut app), before, "the camera never reaches the head");
    }
}

#[test]
fn a_teleport_gives_the_listener_new_ears_so_doppler_hears_a_cut() {
    let scene = example();
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(regia_scene::ScenePlugin);
    enable_fixed_step(&mut app, 60.0);
    app.add_plugins(ShowcaseXrPlugin {
        scene: Arc::clone(&scene),
        backend: XrBackendKind::Simulator,
        render: false,
        preset: ComfortPreset::Comfortable,
    });
    app.update();
    let hand = Pose::new(
        Vec3::new(-0.2, 1.2, -0.3),
        Quat::from_rotation_x(25f32.to_radians()),
    );
    drive_pose(
        app.world_mut(),
        Some(Pose::new(Vec3::new(0.0, 1.7, 0.0), Quat::IDENTITY)),
        [Some(hand), None],
    )
    .expect("pose");
    for _ in 0..3 {
        app.update();
    }
    let before = one::<XrEars>(&mut app);
    let jumps = app.world().resource::<XrWalk>().jumps();
    drive_input(
        app.world_mut(),
        "move",
        "left",
        XrActionValue::Vec2(Vec2::new(0.0, 0.9)),
    )
    .expect("aim");
    app.update();
    app.update();
    drive_input(app.world_mut(), "move", "left", XrActionValue::Vec2(Vec2::ZERO)).expect("release");
    for _ in 0..3 {
        app.update();
    }
    let walk = app.world().resource::<XrWalk>();
    assert_eq!(walk.teleports(), 1, "the teleport happened");
    assert!(walk.jumps() > jumps);
    let after = one::<XrEars>(&mut app);
    assert_ne!(before, after, "new ears after the jump");
    assert!(app.world().get_entity(before).is_err(), "the old ears are gone");
    assert_eq!(
        one::<AudioListener>(&mut app),
        after,
        "still exactly one listener"
    );
    let at = app
        .world()
        .get::<GlobalTransform>(after)
        .expect("global")
        .0
        .translation;
    let head = head_world(&mut app);
    assert!((Vec3::from(at) - head).length() < 1e-3, "{at:?} vs {head}");
}

#[test]
fn without_a_runtime_the_desktop_keeps_its_camera_and_listener() {
    let scene = example();
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(regia_scene::ScenePlugin);
    enable_fixed_step(&mut app, 60.0);
    // The OpenXR binding is not built: XR stays off with xr_no_loader.
    app.add_plugins(ShowcaseXrPlugin {
        scene: Arc::clone(&scene),
        backend: XrBackendKind::OpenXr,
        render: false,
        preset: ComfortPreset::Moderate,
    });
    let start = Transform::from_translation(Vec3::new(3.0, 60.0, 4.0));
    let camera = app
        .world_mut()
        .spawn((
            Camera::default(),
            start,
            GlobalTransform::default(),
            AudioListener::default(),
        ))
        .id();
    for _ in 0..3 {
        app.update();
    }
    assert_eq!(app.world().resource::<XrState>().app, XrAppState::Off);
    assert!(
        app.world().get::<AudioListener>(camera).is_some(),
        "the camera keeps its listener"
    );
    assert_eq!(one::<AudioListener>(&mut app), camera, "the ears do not listen");
    assert_eq!(
        *app.world().get::<Transform>(camera).expect("camera"),
        start,
        "the camera is not moved"
    );
}

#[test]
fn the_desktop_camera_follows_the_head_and_holds_both_eyes() {
    let scene = example();
    let mut app = cpu_app(&scene);
    let camera = app
        .world_mut()
        .spawn((
            Camera {
                fov_y: 60f32.to_radians(),
                ..Camera::default()
            },
            Transform::IDENTITY,
            GlobalTransform::default(),
        ))
        .id();
    app.update();
    // The user turns around: the head faces away from where the scene camera looked.
    drive_pose(
        app.world_mut(),
        Some(Pose::new(
            Vec3::new(0.2, 1.7, 0.1),
            Quat::from_rotation_y(std::f32::consts::PI),
        )),
        [None, None],
    )
    .expect("pose");
    for _ in 0..3 {
        app.update();
    }
    let (rig_e, head_e) = (one::<XrRig>(&mut app), one::<XrHead>(&mut app));
    let rig = *app.world().get::<Transform>(rig_e).expect("rig");
    let head = *app.world().get::<Transform>(head_e).expect("head");
    let (position, rotation) = showcase::xr::head_world(&rig, &head);
    let cam = *app.world().get::<Transform>(camera).expect("camera");
    assert!(
        (cam.translation - position).length() < 1e-4,
        "{:?} vs {position}",
        cam.translation
    );
    assert!(
        cam.rotation.dot(rotation).abs() > 1.0 - 1e-5,
        "the camera faces where the head faces"
    );
    let views = regia_xr::last_views(app.world()).expect("views");
    let fov = app.world().get::<Camera>(camera).expect("camera").fov_y;
    let aspect = showcase::xr::XR_LOD_VIEWPORT.0 as f32 / showcase::xr::XR_LOD_VIEWPORT.1 as f32;
    let tracked = app
        .world()
        .resource::<regia_xr::XrSession>()
        .held_head()
        .orientation;
    assert!(
        (fov - showcase::xr::covering_fov_y(&views, tracked, aspect)).abs() < 1e-5,
        "the camera's field of view holds both eyes: {fov}"
    );
    assert!(fov > 60f32.to_radians(), "wider than the filming camera");

    // With the walk off the camera is the desktop's again.
    app.world_mut().resource_mut::<XrWalk>().active = false;
    app.world_mut()
        .get_mut::<Transform>(camera)
        .expect("camera")
        .translation = Vec3::new(9.0, 9.0, 9.0);
    app.update();
    assert_eq!(
        app.world().get::<Transform>(camera).expect("camera").translation,
        Vec3::new(9.0, 9.0, 9.0)
    );
}

#[test]
fn both_eyes_render_the_scene_with_parallax_at_the_walk_budget() {
    if let Err(err) = RenderDevice::headless() {
        eprintln!("skip showcase xr render test: {err}");
        return;
    }
    let scene = example();
    let mut app = build_headless_app(
        Arc::clone(&scene),
        Options {
            plain: true,
            ..Options::default()
        },
        None,
    )
    .expect("headless app");
    add_headless_xr(
        &mut app,
        &scene,
        XrBackendKind::Simulator,
        ComfortPreset::Moderate,
    );
    // Run until two stereo frames are drawn (the boot warm holds the first frames back); a bounded count,
    // never the wall clock.
    let mut drawn = 0;
    for _ in 0..600 {
        app.update();
        drawn = app.world().resource::<XrRenderState>().stats().frames;
        if drawn >= 2 {
            break;
        }
    }
    assert!(drawn >= 2, "stereo frames drawn: {drawn}");
    let stats = app.world().resource::<XrRenderState>().stats();
    assert_eq!(stats.scene_passes, 2, "one pass per eye");
    let profile = *app.world().resource::<regia_xr::XrRenderProfile>();
    let budget = regia_xr::walk_budget(showcase::xr::XR_REFRESH_HZ);
    assert_eq!(
        app.world().resource::<regia_xr::XrConfig>().render_scale,
        budget.render_scale
    );
    assert_eq!(
        (profile.extent.x, profile.extent.y),
        showcase::xr::XR_LOD_VIEWPORT,
        "the eyes are the recommended size at the budget's scale"
    );
    let targets = app
        .world()
        .get_resource::<regia_xr::SimEyeTargets>()
        .expect("eye targets");
    let left = targets.read_rgba8(Eye::Left).expect("left");
    let right = targets.read_rgba8(Eye::Right).expect("right");
    let distinct = |img: &[u8]| {
        let mut set = std::collections::HashSet::new();
        for px in img.as_chunks::<4>().0 {
            set.insert(*px);
        }
        set.len()
    };
    assert!(
        distinct(&left) > 64,
        "the left eye holds a scene: {}",
        distinct(&left)
    );
    assert!(distinct(&right) > 64, "the right eye holds a scene");
    assert_ne!(left, right, "the eyes differ (parallax)");
    // The scene camera (the view the forest streams and culls around) sits on the head.
    let head = head_world(&mut app);
    let mut cameras = app.world_mut().query_filtered::<&Transform, With<Camera>>();
    let cams: Vec<Vec3> = cameras.iter(app.world()).map(|t| t.translation).collect();
    assert_eq!(cams.len(), 1, "the showcase has one camera");
    assert!(
        (cams[0] - head).length() < 1e-3,
        "the scene camera follows the head: {:?} vs {head}",
        cams[0]
    );
}
