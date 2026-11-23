// SPDX-License-Identifier: MPL-2.0
//! `XrPlugin` on the simulator backend (spec 58 § 8, "Sim" items): no GPU, no headset, fixed steps.

use regia_app::prelude::*;
use regia_core::glam::{Quat, Vec2, Vec3};
use regia_input::InputInjection;
use regia_platform::xr::{Fov, NullConfig, Pose, SessionState, View, XrRuntime};
use regia_scene::Transform;
use regia_xr::sim::ScriptState;
use regia_xr::{
    AppliedAt, OptionValue, TrackingSpace, XrActionValue, XrAppState, XrConfig, XrController, XrHead,
    XrOriginChanged, XrPlugin, XrRig, XrRuntimeLost, XrSession, XrSimScript, XrState, XrStateChanged,
    drive_input, drive_pose, drive_session_state, eye_matrices, eye_projection, option_value, recenter,
    set_option,
};

fn app_with(config: XrConfig) -> App {
    let mut app = App::new();
    enable_fixed_step(&mut app, 60.0);
    app.init_resource::<InputInjection>();
    app.add_plugins(XrPlugin::new(config));
    app
}

fn sim_app() -> App {
    app_with(XrConfig {
        backend: regia_xr::XrBackendKind::Simulator,
        retry_frames: 5,
        ..XrConfig::default()
    })
}

fn app_state(app: &App) -> XrAppState {
    app.world().resource::<XrState>().app
}

fn drive(app: &mut App, s: ScriptState) {
    drive_session_state(app.world_mut(), s).expect("drive");
}

fn state_changes(app: &mut App) -> Vec<(XrAppState, XrAppState)> {
    let messages = app.world_mut().resource_mut::<Messages<XrStateChanged>>();
    let mut cursor = messages.get_cursor();
    cursor.read(&messages).map(|m| (m.from, m.to)).collect()
}

#[test]
fn the_lifecycle_table_maps_every_row() {
    let mut app = sim_app();
    assert_eq!(app_state(&app), XrAppState::Idle);
    drive(&mut app, ScriptState::Ready);
    let changes = state_changes(&mut app);
    assert!(
        changes.contains(&(XrAppState::Idle, XrAppState::Starting)),
        "{changes:?}"
    );
    assert_eq!(app_state(&app), XrAppState::Running, "ready begins the session");
    drive(&mut app, ScriptState::Synchronized);
    assert_eq!(app_state(&app), XrAppState::Running);
    assert_eq!(
        app.world().resource::<XrState>().session,
        SessionState::Synchronized
    );
    drive(&mut app, ScriptState::Visible);
    assert_eq!(app_state(&app), XrAppState::Running);
    drive(&mut app, ScriptState::Focused);
    assert_eq!(app_state(&app), XrAppState::Focused);
    drive(&mut app, ScriptState::Stopping);
    assert_eq!(
        app_state(&app),
        XrAppState::Idle,
        "stopping ends the session and returns to idle"
    );
    let changes = state_changes(&mut app);
    assert!(
        changes.contains(&(XrAppState::Focused, XrAppState::Stopping)),
        "{changes:?}"
    );
    drive(&mut app, ScriptState::Exiting);
    assert_eq!(app_state(&app), XrAppState::Exiting);
    assert!(
        !app.world().contains_resource::<XrRuntime>(),
        "everything destroyed"
    );
    let exits = app.world().resource::<Messages<AppExit>>();
    assert!(
        exits
            .iter_current_update_messages()
            .any(|e| *e == AppExit::Success)
    );
}

#[test]
fn a_lost_runtime_returns_to_no_session_and_recreates_after_retry_frames() {
    let mut app = sim_app();
    drive(&mut app, ScriptState::Focused);
    drive(&mut app, ScriptState::LossPending);
    assert_eq!(app_state(&app), XrAppState::NoSession);
    assert!(!app.world().contains_resource::<XrRuntime>());
    let changes = state_changes(&mut app);
    assert!(
        changes.contains(&(XrAppState::Focused, XrAppState::LossPending)),
        "{changes:?}"
    );
    assert!(changes.contains(&(XrAppState::LossPending, XrAppState::NoSession)));
    assert_eq!(app.world().resource::<Messages<XrRuntimeLost>>().len(), 1);
    assert_eq!(
        app.world()
            .resource::<XrState>()
            .last_error
            .as_ref()
            .map(|e| e.0.as_str()),
        Some("xr_runtime_lost")
    );
    for step in 1..=4 {
        app.update();
        assert_eq!(app_state(&app), XrAppState::NoSession, "step {step}");
    }
    app.update();
    assert_eq!(
        app_state(&app),
        XrAppState::Idle,
        "re-created on the 5th fixed step"
    );
    assert_eq!(app.world().resource::<XrState>().sessions_created, 2);
    drive(&mut app, ScriptState::Focused);
    assert_eq!(app_state(&app), XrAppState::Focused);
}

#[test]
fn a_new_session_restarts_frame_order_bookkeeping() {
    let mut app = sim_app();
    drive(&mut app, ScriptState::Focused);
    for _ in 0..3 {
        app.update();
    }
    let frames = app
        .world()
        .resource::<XrRuntime>()
        .last_report()
        .map(|r| r.timing.frame_index);
    assert_eq!(frames, Some(2));
    drive(&mut app, ScriptState::Stopping);
    drive(&mut app, ScriptState::Focused);
    app.update();
    let frame = app
        .world()
        .resource::<XrRuntime>()
        .last_report()
        .map(|r| r.timing.frame_index);
    assert_eq!(frame, Some(0), "begin resets the frame counter");
    let extracted = app
        .world()
        .resource::<XrSession>()
        .extracted()
        .map(|f| f.frame_index);
    assert_eq!(extracted, Some(0), "the mailbox reader was reset too");
}

#[test]
fn input_is_gated_on_focus_and_published_to_regia_input() {
    let mut app = sim_app();
    drive(&mut app, ScriptState::Visible);
    drive_input(
        app.world_mut(),
        "move",
        "left",
        XrActionValue::Vec2(Vec2::new(0.0, 1.0)),
    )
    .expect("inject");
    app.update();
    let states = app.world().resource::<regia_xr::XrActionStates>();
    assert_eq!(
        states.get("move", "left"),
        Some((false, XrActionValue::Vec2(Vec2::ZERO)))
    );
    assert_eq!(
        app.world().resource::<InputInjection>().value_for("move", 0),
        None
    );
    drive(&mut app, ScriptState::Focused);
    app.update();
    let states = app.world().resource::<regia_xr::XrActionStates>();
    let a = states.get("move", "left");
    assert_eq!(a, Some((true, XrActionValue::Vec2(Vec2::new(0.0, 1.0)))));
    assert_eq!(states.get("move", "left"), a, "two reads between syncs agree");
    assert_eq!(
        app.world().resource::<InputInjection>().value_for("move", 0),
        Some(Vec3::new(0.0, 1.0, 0.0))
    );
    let e = drive_input(app.world_mut(), "warp", "", XrActionValue::Bool(true)).expect_err("unknown");
    assert_eq!(e.code(), "xr_action_unknown");
}

#[test]
fn a_paused_frame_loop_repeats_the_old_frame_and_never_stalls_the_sim() {
    let mut app = sim_app();
    drive(&mut app, ScriptState::Focused);
    app.update();
    let ticks_before = app.world().resource::<XrSession>().tick();
    let extracted = app.world().resource::<XrSession>().extracted();
    assert!(extracted.is_some());
    app.world_mut().resource_mut::<XrSession>().frame_paused = true;
    for i in 1..=4 {
        app.update();
        assert_eq!(app.world().resource::<XrState>().stats.stale_frames, i);
        assert_eq!(
            app.world().resource::<XrSession>().extracted(),
            extracted,
            "old frame repeated"
        );
    }
    assert_eq!(
        app.world().resource::<XrSession>().tick(),
        ticks_before + 4,
        "fixed steps unchanged"
    );
    app.world_mut().resource_mut::<XrSession>().frame_paused = false;
    app.update();
    assert_eq!(app.world().resource::<XrState>().stats.stale_frames, 4);
    assert_ne!(app.world().resource::<XrSession>().extracted(), extracted);
}

#[test]
fn a_synchronized_session_submits_but_renders_nothing() {
    let mut app = sim_app();
    drive(&mut app, ScriptState::Synchronized);
    app.update();
    let report = app
        .world()
        .resource::<XrRuntime>()
        .last_report()
        .copied()
        .expect("frame ran");
    assert!(!report.rendered);
    assert_eq!(app.world().resource::<XrState>().stats.frames, 1);
}

fn spawn_rig(app: &mut App) -> (Entity, Entity, Entity) {
    let rig = app
        .world_mut()
        .spawn((XrRig, Transform::from_translation(Vec3::new(2.0, 0.0, 3.0))))
        .id();
    let head = app.world_mut().spawn((XrHead, Transform::IDENTITY)).id();
    let right = app
        .world_mut()
        .spawn((
            XrController {
                hand: regia_platform::xr::Hand::Right,
            },
            Transform::IDENTITY,
        ))
        .id();
    (rig, head, right)
}

fn head_world(app: &App, rig: Entity, head: Entity) -> Vec3 {
    let r = app.world().get::<Transform>(rig).expect("rig");
    let h = app.world().get::<Transform>(head).expect("head");
    r.translation + r.rotation * h.translation
}

#[test]
fn snap_turn_rotates_the_rig_about_the_head_once_per_deflection() {
    let mut app = sim_app();
    let (rig, head, right) = spawn_rig(&mut app);
    drive(&mut app, ScriptState::Focused);
    drive_pose(
        app.world_mut(),
        Some(Pose::new(Vec3::new(0.3, 1.7, -0.4), Quat::IDENTITY)),
        [None, Some(Pose::new(Vec3::new(0.2, 1.2, -0.3), Quat::IDENTITY))],
    )
    .expect("pose");
    app.update();
    let before = head_world(&app, rig, head);
    assert!((before - Vec3::new(2.3, 1.7, 2.6)).length() < 1e-5, "{before}");
    let ctrl = app
        .world()
        .get::<Transform>(right)
        .expect("controller")
        .translation;
    assert!((ctrl - Vec3::new(0.2, 1.2, -0.3)).length() < 1e-5);
    drive_input(
        app.world_mut(),
        "turn",
        "right",
        XrActionValue::Vec2(Vec2::new(0.9, 0.0)),
    )
    .expect("turn");
    for _ in 0..10 {
        app.update();
    }
    assert_eq!(
        app.world().resource::<XrSession>().snaps(),
        1,
        "held stick snaps once"
    );
    let rot = app.world().get::<Transform>(rig).expect("rig").rotation;
    let yaw = regia_xr::space::yaw_of(rot);
    assert!((yaw + 30f32.to_radians()).abs() < 1e-5, "{yaw}");
    let after = head_world(&app, rig, head);
    assert!(
        (after - before).length() < 1e-5,
        "head did not translate: {before} -> {after}"
    );
    assert!(
        app.world().resource::<XrSession>().vignette() > 0.0,
        "rig rotation drives the vignette"
    );
    drive_input(app.world_mut(), "turn", "right", XrActionValue::Vec2(Vec2::ZERO)).expect("release");
    for _ in 0..12 {
        app.update();
    }
    drive_input(
        app.world_mut(),
        "turn",
        "right",
        XrActionValue::Vec2(Vec2::new(-0.9, 0.0)),
    )
    .expect("left");
    app.update();
    assert_eq!(app.world().resource::<XrSession>().snaps(), 2, "re-armed");
}

#[test]
fn head_motion_alone_never_raises_the_vignette() {
    let mut app = sim_app();
    spawn_rig(&mut app);
    drive(&mut app, ScriptState::Focused);
    for i in 0..30 {
        let x = i as f32 * 0.1;
        drive_pose(
            app.world_mut(),
            Some(Pose::new(Vec3::new(x, 1.7, 0.0), Quat::from_rotation_y(x))),
            [None, None],
        )
        .expect("pose");
        app.update();
    }
    assert_eq!(app.world().resource::<XrSession>().vignette(), 0.0);
}

#[test]
fn non_finite_poses_are_rejected_and_counted() {
    let mut app = sim_app();
    drive(&mut app, ScriptState::Focused);
    let good = Pose::new(Vec3::new(0.0, 1.6, 0.0), Quat::IDENTITY);
    drive_pose(app.world_mut(), Some(good), [None, None]).expect("pose");
    app.update();
    let nan = Pose {
        position: Vec3::new(f32::NAN, 1.0, 0.0),
        orientation: Quat::IDENTITY,
    };
    drive_pose(app.world_mut(), Some(nan), [Some(nan), None]).expect("answered");
    app.update();
    assert_eq!(app.world().resource::<XrState>().stats.bad_poses, 2);
    assert_eq!(
        app.world().resource::<XrSession>().held_head().position,
        good.position,
        "previous kept"
    );
}

#[test]
fn tracking_space_changes_fall_back_and_recenter_aligns_the_head() {
    let mut app = sim_app();
    drive(&mut app, ScriptState::Focused);
    assert_eq!(
        app.world().resource::<XrState>().actual_tracking_space,
        TrackingSpace::Stage
    );
    let applied = set_option(
        app.world_mut(),
        "tracking_space",
        &OptionValue::Text("local".into()),
    )
    .expect("set");
    assert_eq!(applied, AppliedAt::Now);
    assert_eq!(
        app.world().resource::<XrState>().actual_tracking_space,
        TrackingSpace::Local
    );
    assert_eq!(app.world().resource::<Messages<XrOriginChanged>>().len(), 1);
    drive_pose(
        app.world_mut(),
        Some(Pose::new(Vec3::new(0.5, 1.6, -0.2), Quat::from_rotation_y(1.0))),
        [None, None],
    )
    .expect("pose");
    app.update();
    let offset = recenter(app.world_mut()).expect("recenter");
    let session = app.world().resource::<XrSession>();
    let centered = offset.compose(&session.held_head());
    assert!(centered.position.x.abs() < 1e-5 && centered.position.z.abs() < 1e-5);
    assert!(regia_xr::space::yaw_of(centered.orientation).abs() < 1e-5);
    assert_eq!(app.world().resource::<Messages<XrOriginChanged>>().len(), 2);
}

#[test]
fn options_validate_keys_types_and_ranges() {
    let mut app = sim_app();
    let w = app.world_mut();
    assert_eq!(
        set_option(w, "warp", &OptionValue::Bool(true))
            .expect_err("key")
            .code(),
        "xr_unknown_option"
    );
    let e = set_option(w, "render_scale", &OptionValue::Number(3.0)).expect_err("range");
    assert_eq!(e.code(), "xr_invalid_value");
    assert!(e.to_string().contains("0.5..=2"), "{e}");
    assert_eq!(
        set_option(w, "render_scale", &OptionValue::Text("big".into()))
            .expect_err("type")
            .code(),
        "xr_invalid_value"
    );
    assert_eq!(
        set_option(w, "render_scale", &OptionValue::Number(1.5)),
        Ok(AppliedAt::NextFrame)
    );
    assert_eq!(w.resource::<XrState>().render_scale, 1.5);
    let live_hz = w.resource::<XrState>().refresh_hz;
    assert_eq!(
        set_option(w, "refresh_hz", &OptionValue::Number(120.0)),
        Ok(AppliedAt::NextSession)
    );
    assert_eq!(
        option_value(w, "refresh_hz"),
        Ok(OptionValue::Number(120.0)),
        "the pending preferred rate, which is not xr.get"
    );
    assert_eq!(
        w.resource::<XrState>().refresh_hz,
        live_hz,
        "xr.get stays on the live session rate until the session is recreated"
    );
    assert_eq!(
        set_option(w, "enabled", &OptionValue::Bool(false)),
        Ok(AppliedAt::Now)
    );
    assert_eq!(w.resource::<XrState>().app, XrAppState::Off);
    assert!(!w.contains_resource::<XrRuntime>());
    assert_eq!(
        set_option(w, "enabled", &OptionValue::Bool(true)),
        Ok(AppliedAt::Now)
    );
    assert_eq!(w.resource::<XrState>().app, XrAppState::Idle);
    assert!(
        (w.resource::<XrState>().refresh_hz - 120.0).abs() < 1e-3,
        "applied at the new session"
    );
}

#[test]
fn without_a_runtime_the_game_keeps_running_with_the_reason() {
    let mut app = app_with(XrConfig::default());
    app.update();
    let st = app.world().resource::<XrState>();
    assert_eq!(st.app, XrAppState::Off);
    assert_eq!(st.last_error.as_ref().map(|e| e.0.as_str()), Some("xr_no_loader"));
    assert!(
        app.world().resource::<Messages<AppExit>>().is_empty(),
        "not required: no exit"
    );
    let required = app_with(XrConfig {
        required: true,
        ..XrConfig::default()
    });
    assert!(
        !required.world().resource::<Messages<AppExit>>().is_empty(),
        "required: exits"
    );
}

#[test]
fn seated_uses_local_space_and_the_eye_height_offset() {
    let comfort = regia_xr::ComfortSettings {
        seated: true,
        ..regia_xr::ComfortSettings::default()
    };
    let app = app_with(XrConfig {
        backend: regia_xr::XrBackendKind::Simulator,
        comfort,
        ..XrConfig::default()
    });
    let state = app.world().resource::<XrState>();
    assert_eq!(state.tracking_space, TrackingSpace::Stage);
    assert_eq!(state.actual_tracking_space, TrackingSpace::Local);
    let offset = app.world().resource::<XrSession>().offset;
    assert!(
        (offset.position.y - 1.65).abs() < 1e-5,
        "seated lifts the rig by the eye height: {offset:?}"
    );
    assert_eq!(
        app.world().resource::<Messages<XrOriginChanged>>().len(),
        0,
        "opening does not send an origin change"
    );
}

#[test]
fn the_real_backend_refuses_simulator_only_drives() {
    let mut app = sim_app();
    struct Real(regia_platform::xr::NullXr);
    impl regia_platform::xr::XrBackend for Real {
        fn name(&self) -> &'static str {
            "openxr"
        }
        fn state(&self) -> SessionState {
            self.0.state()
        }
        fn poll_events(&mut self) -> Vec<regia_platform::xr::XrEvent> {
            self.0.poll_events()
        }
        fn begin_session(&mut self) -> Result<(), regia_platform::xr::XrError> {
            self.0.begin_session()
        }
        fn request_exit(&mut self) -> Result<(), regia_platform::xr::XrError> {
            self.0.request_exit()
        }
        fn end_session(&mut self) -> Result<(), regia_platform::xr::XrError> {
            self.0.end_session()
        }
        fn wait_frame(&mut self) -> Result<regia_platform::xr::FrameTiming, regia_platform::xr::XrError> {
            self.0.wait_frame()
        }
        fn begin_frame(&mut self) -> Result<(), regia_platform::xr::XrError> {
            self.0.begin_frame()
        }
        fn locate_views(&mut self, t: i64) -> Result<[View; 2], regia_platform::xr::XrError> {
            self.0.locate_views(t)
        }
        fn head_sample(&self) -> regia_platform::xr::PoseSample {
            self.0.head_sample()
        }
        fn sync_actions(&mut self) -> Result<(), regia_platform::xr::XrError> {
            self.0.sync_actions()
        }
        fn controller(&self, hand: regia_platform::xr::Hand) -> regia_platform::xr::ControllerState {
            self.0.controller(hand)
        }
        fn apply_haptic(
            &mut self,
            h: regia_platform::xr::Hand,
            a: f32,
            d: i64,
        ) -> Result<(), regia_platform::xr::XrError> {
            self.0.apply_haptic(h, a, d)
        }
        fn end_frame(
            &mut self,
            s: &regia_platform::xr::FrameSubmit,
        ) -> Result<(), regia_platform::xr::XrError> {
            self.0.end_frame(s)
        }
    }
    app.insert_resource(XrRuntime::new(Box::new(Real(regia_platform::xr::NullXr::new(
        NullConfig::default(),
    )))));
    let w = app.world_mut();
    assert_eq!(
        drive_session_state(w, ScriptState::Ready)
            .expect_err("real")
            .code(),
        "xr_simulator_only"
    );
    assert_eq!(
        drive_pose(w, None, [None, None]).expect_err("real").code(),
        "xr_simulator_only"
    );
    assert_eq!(
        drive_input(w, "menu", "left", XrActionValue::Bool(true))
            .expect_err("real")
            .code(),
        "xr_simulator_only"
    );
}

#[test]
fn eye_projection_matches_the_engine_camera_and_feeds_the_latched_views() {
    // The renderer builds `perspective(fov_y, aspect, near, far)` (directx, standard Z) with the
    // stand-in far plane for an infinite camera; a symmetric XR eye must equal it.
    use regia_core::glam::camera::rh::proj::directx::perspective;
    let half = 0.8_f32;
    let near = 0.05;
    let xr = eye_projection(&Fov::symmetric(half, half), near).expect("projection");
    let far = regia_render::view_math::INFINITE_FAR_STANDIN;
    let engine = perspective(2.0 * half, 1.0, near, far);
    let (a, b) = (xr.to_cols_array(), engine.to_cols_array());
    for i in 0..16 {
        assert!(
            (a[i] - b[i]).abs() <= 1e-6 * (1.0 + b[i].abs()),
            "element {i}: {} vs {}",
            a[i],
            b[i]
        );
    }
    let mut app = sim_app();
    drive(&mut app, ScriptState::Focused);
    drive_pose(
        app.world_mut(),
        Some(Pose::new(Vec3::new(0.0, 1.7, 0.0), Quat::from_rotation_y(0.3))),
        [None, None],
    )
    .expect("pose");
    app.update();
    let mats = eye_matrices(app.world()).expect("a frame ran").expect("valid");
    let views = app
        .world()
        .resource::<XrRuntime>()
        .last_report()
        .expect("report")
        .views;
    for (m, v) in mats.iter().zip(&views) {
        let expected = eye_projection(&v.fov, 0.05).expect("p") * v.view_matrix();
        assert_eq!(
            m.to_cols_array(),
            expected.to_cols_array(),
            "exactly the latched view"
        );
    }
}

#[test]
fn a_script_drives_the_session_poses_and_inputs_deterministically() {
    let script = XrSimScript::from_ron(
        r#"(format_version: 1, ticks: [
            (tick: 0, head: (position: (0.0, 1.6, 0.0)), state: Some(Focused)),
            (tick: 4, head: (position: (0.4, 1.6, 0.0)), right: Some((position: (0.2, 1.1, -0.3))),
             inputs: [("select", "right", Float(0.5))]),
        ])"#,
    )
    .expect("script");
    let run = || {
        let mut app = App::new();
        enable_fixed_step(&mut app, 60.0);
        app.add_plugins(XrPlugin {
            script: Some(script.clone()),
            ..XrPlugin::simulator()
        });
        let (rig, head, _) = spawn_rig(&mut app);
        let mut trace = Vec::new();
        for _ in 0..6 {
            app.update();
            trace.push(head_world(&app, rig, head).to_array());
        }
        let st = app
            .world()
            .resource::<regia_xr::XrActionStates>()
            .get("select", "right");
        (app_state(&app), trace, st)
    };
    let (state, first, select) = run();
    assert_eq!(state, XrAppState::Focused);
    assert_eq!(select, Some((true, XrActionValue::Float(0.5))));
    assert!(
        (first[2][0] - (2.0 + 0.2)).abs() < 1e-5,
        "tick 2 interpolates: {:?}",
        first[2]
    );
    let (_, second, _) = run();
    assert_eq!(first, second, "same script, same transforms, bit for bit");
}
