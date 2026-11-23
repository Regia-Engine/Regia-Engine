// SPDX-License-Identifier: MPL-2.0
//! Walk mode (ADR-2457) on the simulator backend: no GPU, no headset, fixed steps.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::glam::{Quat, Vec2, Vec3};
use regia_input::InputInjection;
use regia_platform::xr::Pose;
use regia_scene::Transform;
use regia_xr::sim::ScriptState;
use regia_xr::{
    ComfortPreset, Locomotion, WalkStart, XrActionValue, XrConfig, XrController, XrGround, XrHead, XrPlugin,
    XrRig, XrSession, XrState, XrWalk, XrWalkPlugin, calibrate_height, drive_input, drive_pose,
    drive_session_state, walk_budget, walk_start,
};

/// A gentle slope: the ground rises 0.1 m per meter toward -Z.
fn slope(_x: f32, z: f32) -> Option<f32> {
    Some(-0.1 * z)
}

fn walk_app() -> App {
    let mut app = App::new();
    enable_fixed_step(&mut app, 60.0);
    app.init_resource::<InputInjection>();
    app.add_plugins(XrPlugin::new(XrConfig {
        backend: regia_xr::XrBackendKind::Simulator,
        ..XrConfig::default()
    }));
    app.add_plugins(XrWalkPlugin::default());
    app.insert_resource(XrGround(Arc::new(slope)));
    app
}

struct Rig {
    rig: Entity,
    head: Entity,
}

fn spawn_rig(app: &mut App) -> Rig {
    let rig = app.world_mut().spawn((XrRig, Transform::IDENTITY)).id();
    let head = app.world_mut().spawn((XrHead, Transform::IDENTITY)).id();
    for hand in [regia_platform::xr::Hand::Left, regia_platform::xr::Hand::Right] {
        app.world_mut()
            .spawn((XrController { hand }, Transform::IDENTITY));
    }
    Rig { rig, head }
}

fn head_world(app: &App, r: &Rig) -> Vec3 {
    let rig = app.world().get::<Transform>(r.rig).expect("rig");
    let head = app.world().get::<Transform>(r.head).expect("head");
    rig.translation + rig.rotation * head.translation
}

fn rig_t(app: &App, r: &Rig) -> Transform {
    *app.world().get::<Transform>(r.rig).expect("rig")
}

fn focus(app: &mut App, head: Pose, left: Option<Pose>) {
    drive_session_state(app.world_mut(), ScriptState::Focused).expect("focused");
    drive_pose(app.world_mut(), Some(head), [left, None]).expect("pose");
    app.update();
}

fn stick(app: &mut App, v: Vec2) {
    drive_input(app.world_mut(), "move", "left", XrActionValue::Vec2(v)).expect("move");
}

fn walk(app: &App) -> &XrWalk {
    app.world().resource::<XrWalk>()
}

fn session(app: &App) -> &XrSession {
    app.world().resource::<XrSession>()
}

#[test]
fn an_inactive_walk_never_moves_the_rig() {
    let mut app = walk_app();
    let r = spawn_rig(&mut app);
    focus(
        &mut app,
        Pose::new(Vec3::new(0.0, 1.7, 0.0), Quat::IDENTITY),
        None,
    );
    stick(&mut app, Vec2::new(0.0, 1.0));
    for _ in 0..20 {
        app.update();
    }
    assert_eq!(rig_t(&app, &r).translation, Vec3::ZERO);
    assert_eq!(walk(&app).steps(), 0);
}

#[test]
fn smooth_locomotion_walks_the_head_heading_at_pace_and_follows_the_ground_without_bob() {
    let mut app = walk_app();
    let r = spawn_rig(&mut app);
    // The head looks 90 degrees left (down -X) and stands 1.7 m over the tracking floor.
    let yaw = std::f32::consts::FRAC_PI_2;
    focus(
        &mut app,
        Pose::new(Vec3::new(0.0, 1.7, 0.0), Quat::from_rotation_y(yaw)),
        None,
    );
    walk_start(
        app.world_mut(),
        WalkStart {
            position: Some(Vec3::new(5.0, 0.0, 5.0)),
            ..WalkStart::default()
        },
    )
    .expect("start");
    app.update();
    let start = head_world(&app, &r);
    assert!(
        (start.x - 5.0).abs() < 1e-5 && (start.z - 5.0).abs() < 1e-5,
        "{start}"
    );
    assert!(
        (start.y - (slope(5.0, 5.0).unwrap() + 1.7)).abs() < 1e-4,
        "on the ground: {start}"
    );
    assert_eq!(session(&app).vignette(), 0.0, "placing the rig is not motion");

    // Push the stick forward: the move follows the head heading, not the rig.
    stick(&mut app, Vec2::new(0.0, 1.0));
    let steps_before = walk(&app).steps();
    for _ in 0..30 {
        app.update();
        let head = head_world(&app, &r);
        let ground = slope(head.x, head.z).unwrap();
        assert!(
            (head.y - ground - 1.7).abs() < 1e-4,
            "the eye stays exactly 1.7 m over the ground (no bob): {head}"
        );
    }
    let steps = (walk(&app).steps() - steps_before) as f32;
    assert!(steps > 0.0);
    let end = head_world(&app, &r);
    let moved = end - start;
    let expected = 1.4 * steps / 60.0;
    assert!(
        (moved.x + expected).abs() < 1e-3,
        "walked down -X: {moved}, {expected}"
    );
    assert!(moved.z.abs() < 1e-4, "no sideways drift: {moved}");
    assert!((walk(&app).distance_m() - expected).abs() < 1e-3);
    assert!(
        session(&app).vignette() > 0.0,
        "smooth motion raises the vignette"
    );

    // Release: the rig stops and the vignette eases back to zero.
    stick(&mut app, Vec2::ZERO);
    for _ in 0..200 {
        app.update();
    }
    let rest = head_world(&app, &r);
    for _ in 0..10 {
        app.update();
    }
    assert_eq!(head_world(&app, &r), rest, "stopped");
    assert_eq!(session(&app).vignette(), 0.0);
}

#[test]
fn room_scale_head_motion_moves_no_rig_and_raises_no_vignette() {
    let mut app = walk_app();
    let r = spawn_rig(&mut app);
    focus(
        &mut app,
        Pose::new(Vec3::new(0.0, 1.7, 0.0), Quat::IDENTITY),
        None,
    );
    walk_start(app.world_mut(), WalkStart::default()).expect("start");
    app.update();
    let rig_xz = |app: &App| {
        let t = rig_t(app, &r).translation;
        Vec2::new(t.x, t.z)
    };
    let before = rig_xz(&app);
    for i in 0..30 {
        let x = i as f32 * 0.02;
        drive_pose(
            app.world_mut(),
            Some(Pose::new(Vec3::new(x, 1.7, 0.0), Quat::from_rotation_y(x))),
            [None, None],
        )
        .expect("pose");
        app.update();
    }
    assert_eq!(
        rig_xz(&app),
        before,
        "the user walking in the room does not move the rig"
    );
    assert_eq!(session(&app).vignette(), 0.0);
}

#[test]
fn a_teleport_aims_an_arc_and_jumps_on_release_without_a_vignette_flash() {
    let mut app = walk_app();
    app.insert_resource(XrGround::flat(2.0));
    let r = spawn_rig(&mut app);
    // The left hand points forward and 30 degrees up.
    let hand = Pose::new(
        Vec3::new(-0.2, 1.2, -0.3),
        Quat::from_rotation_x(30f32.to_radians()),
    );
    focus(
        &mut app,
        Pose::new(Vec3::new(0.0, 1.7, 0.0), Quat::IDENTITY),
        Some(hand),
    );
    walk_start(
        app.world_mut(),
        WalkStart {
            position: Some(Vec3::ZERO),
            preset: Some(ComfortPreset::Comfortable),
            ..WalkStart::default()
        },
    )
    .expect("start");
    assert_eq!(walk(&app).settings.locomotion, Locomotion::Teleport);
    app.update();
    for _ in 0..20 {
        app.update();
    }
    let start = head_world(&app, &r);
    assert!((start.y - 3.7).abs() < 1e-4, "{start}");

    stick(&mut app, Vec2::new(0.0, 0.9));
    for _ in 0..3 {
        app.update();
    }
    assert!(walk(&app).aiming());
    let target = walk(&app).aim_target().expect("the arc lands");
    assert_eq!(target.y, 2.0);
    assert!(
        target.z < -3.0 && target.z > -8.5,
        "ahead, within range: {target}"
    );
    assert_eq!(head_world(&app, &r), start, "aiming does not move");

    stick(&mut app, Vec2::ZERO);
    app.update();
    assert_eq!(walk(&app).teleports(), 1);
    assert!(!walk(&app).aiming());
    let end = head_world(&app, &r);
    assert!(
        (end.x - target.x).abs() < 1e-4 && (end.z - target.z).abs() < 1e-4,
        "{end} vs {target}"
    );
    assert!((end.y - 3.7).abs() < 1e-4);
    assert_eq!(session(&app).vignette(), 0.0, "a jump never flashes the vignette");
}

#[test]
fn height_calibration_lets_a_seated_user_walk_at_standing_height() {
    let mut app = walk_app();
    let r = spawn_rig(&mut app);
    // Seated: the head is 1.2 m over the tracking floor.
    focus(
        &mut app,
        Pose::new(Vec3::new(0.0, 1.2, 0.0), Quat::IDENTITY),
        None,
    );
    walk_start(app.world_mut(), WalkStart::default()).expect("start");
    let lift = calibrate_height(app.world_mut(), None).expect("calibrate");
    assert!((lift - 0.45).abs() < 1e-5, "{lift}");
    app.update();
    let head = head_world(&app, &r);
    assert!(
        (head.y - slope(head.x, head.z).unwrap() - 1.65).abs() < 1e-4,
        "{head}"
    );
    let lift = calibrate_height(app.world_mut(), Some(1.8)).expect("calibrate");
    assert!((lift - 0.6).abs() < 1e-5);
    let err = calibrate_height(app.world_mut(), Some(9.0)).expect_err("range");
    assert_eq!(err.code(), "xr_invalid_value");
}

#[test]
fn calibration_needs_a_running_session() {
    let mut app = walk_app();
    spawn_rig(&mut app);
    let err = calibrate_height(app.world_mut(), None).expect_err("idle");
    assert_eq!(err.code(), "xr_not_running");
}

#[test]
fn the_start_applies_the_preset_and_the_budget() {
    let mut app = walk_app();
    spawn_rig(&mut app);
    focus(
        &mut app,
        Pose::new(Vec3::new(0.0, 1.7, 0.0), Quat::IDENTITY),
        None,
    );
    walk_start(
        app.world_mut(),
        WalkStart {
            preset: Some(ComfortPreset::Free),
            budget: true,
            ..WalkStart::default()
        },
    )
    .expect("start");
    let hz = app.world().resource::<XrState>().refresh_hz;
    let budget = walk_budget(hz);
    let config = app.world().resource::<XrConfig>();
    assert_eq!(config.render_scale, budget.render_scale);
    assert_eq!(config.foveation, budget.foveation);
    assert_eq!(config.comfort.snap_turn_deg, 0.0, "free turns smoothly");
    assert_eq!(config.comfort.vignette_strength, 0.0);
    assert_eq!(walk(&app).budget, Some(budget));
    assert_eq!(walk(&app).preset, Some(ComfortPreset::Free));
}

#[test]
fn a_start_without_a_rig_is_refused_by_name() {
    let mut app = walk_app();
    let err = walk_start(app.world_mut(), WalkStart::default()).expect_err("no rig");
    assert_eq!(err.code(), "xr_invalid_value");
    assert!(err.to_string().contains("rig"), "{err}");
}

/// Runs a fixed scripted walk and returns the rig's pose bits after each step.
fn scripted_walk() -> Vec<[u32; 7]> {
    let mut app = walk_app();
    let r = spawn_rig(&mut app);
    focus(
        &mut app,
        Pose::new(Vec3::new(0.1, 1.6, 0.2), Quat::from_rotation_y(0.3)),
        None,
    );
    walk_start(app.world_mut(), WalkStart::default()).expect("start");
    let mut out = Vec::new();
    for i in 0..40 {
        let a = i as f32 * 0.2;
        stick(&mut app, Vec2::new(a.sin() * 0.8, a.cos()));
        if i == 20 {
            drive_input(
                app.world_mut(),
                "turn",
                "right",
                XrActionValue::Vec2(Vec2::new(1.0, 0.0)),
            )
            .expect("turn");
        }
        app.update();
        let t = rig_t(&app, &r);
        out.push([
            t.translation.x.to_bits(),
            t.translation.y.to_bits(),
            t.translation.z.to_bits(),
            t.rotation.x.to_bits(),
            t.rotation.y.to_bits(),
            t.rotation.z.to_bits(),
            t.rotation.w.to_bits(),
        ]);
    }
    out
}

#[test]
fn the_same_script_walks_the_same_path_bit_for_bit() {
    let a = scripted_walk();
    let b = scripted_walk();
    assert_eq!(a, b);
    assert_ne!(a.first(), a.last(), "the script moved the rig");
}

#[test]
fn the_teleport_marker_lies_on_the_target_while_aiming_and_goes_on_release() {
    let mut app = walk_app();
    app.init_resource::<regia_xr::XrPanels>();
    app.insert_resource(XrGround::flat(0.0));
    spawn_rig(&mut app);
    let hand = Pose::new(
        Vec3::new(-0.2, 1.2, -0.3),
        Quat::from_rotation_x(20f32.to_radians()),
    );
    focus(
        &mut app,
        Pose::new(Vec3::new(0.0, 1.7, 0.0), Quat::IDENTITY),
        Some(hand),
    );
    walk_start(
        app.world_mut(),
        WalkStart {
            preset: Some(ComfortPreset::Comfortable),
            ..WalkStart::default()
        },
    )
    .expect("start");
    app.update();
    let marker = |app: &App| {
        app.world()
            .resource::<regia_xr::XrPanels>()
            .get(regia_xr::walk::TELEPORT_MARKER_ID)
            .cloned()
    };
    assert!(marker(&app).is_none(), "no marker before aiming");
    stick(&mut app, Vec2::new(0.0, 0.9));
    app.update();
    app.update();
    let target = walk(&app).aim_target().expect("target");
    let panel = marker(&app).expect("a marker while aiming");
    assert_eq!(panel.space, regia_platform::xr::LayerSpace::World);
    assert!((panel.pose.position - (target + Vec3::new(0.0, 0.02, 0.0))).length() < 1e-5);
    let normal = panel.pose.orientation * Vec3::Z;
    assert!(
        (normal - Vec3::Y).length() < 1e-5,
        "it lies flat, facing up: {normal}"
    );
    stick(&mut app, Vec2::ZERO);
    app.update();
    assert_eq!(walk(&app).teleports(), 1);
    assert!(marker(&app).is_none(), "gone after the jump");
}
