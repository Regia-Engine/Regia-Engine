// SPDX-License-Identifier: MPL-2.0
//! `XrPlugin` state machine and options on the simulator backend: session loss and recovery,
//! retry, exit, disable/enable, held input after a session ends, option ranges and effect timing,
//! scripts that end or fail mid-run, recentering, seated mode and record/replay. No GPU, no
//! headset, fixed steps only.

use regia_app::prelude::*;
use regia_core::glam::{Quat, Vec2, Vec3};
use regia_input::InputInjection;
use regia_platform::xr::{Hand, Pose, SimEvent, XrError, XrRuntime};
use regia_scene::Transform;
use regia_xr::sim::{ScriptPose, ScriptState, ScriptValue, SimTick};
use regia_xr::space::yaw_of;
use regia_xr::{
    ActionPoses, AppliedAt, DominantHand, OPTION_KEYS, OptionValue, StereoMode, TrackingSpace,
    XrActionStates, XrActionValue, XrAppState, XrBackendKind, XrConfig, XrController, XrFocusChanged, XrHead,
    XrOriginChanged, XrPlugin, XrRenderProfile, XrRig, XrRuntimeLost, XrSession, XrSimScript, XrState,
    XrStateChanged, drive_input, drive_pose, drive_session_state, eye_matrices, last_views, option_value,
    recenter, set_option,
};

fn sim_config(retry_frames: u32) -> XrConfig {
    XrConfig {
        backend: XrBackendKind::Simulator,
        retry_frames,
        ..XrConfig::default()
    }
}

fn app_with(config: XrConfig, script: Option<XrSimScript>) -> App {
    let mut app = App::new();
    enable_fixed_step(&mut app, 60.0);
    app.init_resource::<InputInjection>();
    app.add_plugins(XrPlugin {
        script,
        ..XrPlugin::new(config)
    });
    app
}

fn sim_app(retry_frames: u32) -> App {
    app_with(sim_config(retry_frames), None)
}

fn app_state(app: &App) -> XrAppState {
    app.world().resource::<XrState>().app
}

fn drive(app: &mut App, state: ScriptState) {
    drive_session_state(app.world_mut(), state).expect("drive");
}

fn messages<M: Message + Clone>(app: &App) -> Vec<M> {
    app.world()
        .resource::<Messages<M>>()
        .iter_current_update_messages()
        .cloned()
        .collect()
}

fn injected(app: &App, action: &str) -> Option<Vec3> {
    app.world().resource::<InputInjection>().value_for(action, 0)
}

fn spawn_rig(app: &mut App, at: Vec3) -> (Entity, Entity, Entity) {
    let rig = app
        .world_mut()
        .spawn((XrRig, Transform::from_translation(at)))
        .id();
    let head = app.world_mut().spawn((XrHead, Transform::IDENTITY)).id();
    let hand = app
        .world_mut()
        .spawn((XrController { hand: Hand::Right }, Transform::IDENTITY))
        .id();
    (rig, head, hand)
}

fn key(tick: u64, x: f32) -> SimTick {
    SimTick {
        tick,
        head: ScriptPose {
            position: (x, 1.6, 0.0),
            orientation: (0.0, 0.0, 0.0, 1.0),
        },
        left: None,
        right: None,
        inputs: Vec::new(),
        state: None,
    }
}

fn script(ticks: Vec<SimTick>) -> XrSimScript {
    XrSimScript {
        ticks,
        ..XrSimScript::new()
    }
}

// ---------------------------------------------------------------------------------------------
// Start-up
// ---------------------------------------------------------------------------------------------

/// Catches an invalid config panicking later: XR stays off with the reason, no session resource
/// exists, the frame loop is a no-op, and the option readers refuse instead of panicking on the
/// missing session resources.
#[test]
fn an_invalid_config_leaves_xr_off_and_the_option_functions_refuse() {
    let mut app = app_with(
        XrConfig {
            backend: XrBackendKind::Simulator,
            render_scale: 9.0,
            ..XrConfig::default()
        },
        None,
    );
    app.update();
    app.update();
    let st = app.world().resource::<XrState>();
    assert_eq!(st.app, XrAppState::Off);
    assert_eq!(
        st.last_error.as_ref().map(|e| e.0.as_str()),
        Some("xr_invalid_value")
    );
    assert_eq!(st.sessions_created, 0);
    assert!(!app.world().contains_resource::<XrRuntime>());
    assert!(!app.world().contains_resource::<XrSession>());
    let w = app.world_mut();
    assert_eq!(
        set_option(w, "snap_turn_deg", &OptionValue::Number(10.0)).map_err(|e| e.code()),
        Err("xr_unsupported")
    );
    assert_eq!(
        option_value(w, "tracking_space"),
        Ok(OptionValue::Text("stage".into()))
    );
    for key in ["runtime_json", "stereo_mode", "hidden_area", "depth_layer"] {
        assert_eq!(
            option_value(w, key).map_err(|e| e.code()),
            Err("xr_unsupported"),
            "{key} needs resources an invalid config never creates"
        );
    }
    assert!(matches!(recenter(w), Err(XrError::NotRunning { .. })));
}

/// Catches `option_value("refresh_hz")` reading `XrState` through `resource` (a panic when a
/// world holds the config without the state): it must refuse like the neighbouring keys, and a
/// world with the state must report its refresh rate.
#[test]
fn refresh_hz_option_refuses_without_state_and_reads_it_otherwise() {
    let mut world = World::new();
    world.insert_resource(XrConfig::default());
    assert_eq!(
        option_value(&world, "refresh_hz").map_err(|e| e.code()),
        Err("xr_unsupported"),
        "no XrState: refuse, do not panic"
    );
    world.insert_resource(XrState::default());
    assert_eq!(
        option_value(&world, "refresh_hz"),
        Ok(OptionValue::Number(0.0)),
        "no preference: the state's refresh rate"
    );
    world.resource_mut::<XrConfig>().preferred_refresh_hz = Some(90.0);
    assert_eq!(
        option_value(&world, "refresh_hz"),
        Ok(OptionValue::Number(90.0)),
        "a preference wins over the state"
    );
}

/// Catches the `Off` backend creating a session or an error, and enabling it later reporting the
/// wrong reason.
#[test]
fn backend_off_creates_nothing_and_enabling_it_reports_no_loader() {
    let mut app = app_with(
        XrConfig {
            backend: XrBackendKind::Off,
            required: true,
            ..XrConfig::default()
        },
        None,
    );
    app.update();
    let st = app.world().resource::<XrState>();
    assert_eq!((st.app, st.sessions_created), (XrAppState::Off, 0));
    assert!(st.last_error.is_none(), "off is a choice, not a failure");
    assert!(!app.world().contains_resource::<XrRuntime>());
    assert!(
        app.world().resource::<Messages<AppExit>>().is_empty(),
        "required does not matter when XR is off"
    );
    let w = app.world_mut();
    assert_eq!(option_value(w, "enabled"), Ok(OptionValue::Bool(false)));
    let e = set_option(w, "enabled", &OptionValue::Bool(true)).expect_err("no loader in this build");
    assert_eq!(e.code(), "xr_no_loader");
    assert_eq!(w.resource::<XrState>().app, XrAppState::Off);
    assert_eq!(
        w.resource::<XrState>().last_error.as_ref().map(|e| e.0.as_str()),
        Some("xr_no_loader")
    );
}

/// Catches `required` not turning into an error exit for the OpenXR choice, and a simulator
/// session wrongly exiting.
#[test]
fn required_exits_with_an_error_only_when_no_session_can_be_made() {
    let exits = |config: XrConfig| -> Vec<AppExit> { messages(&app_with(config, None)) };
    let openxr = exits(XrConfig {
        backend: XrBackendKind::OpenXr,
        required: true,
        ..XrConfig::default()
    });
    assert_eq!(openxr, vec![AppExit::Error(std::num::NonZeroU8::MIN)]);
    assert!(
        exits(XrConfig {
            backend: XrBackendKind::OpenXr,
            ..XrConfig::default()
        })
        .is_empty()
    );
    assert!(
        exits(XrConfig {
            required: true,
            ..sim_config(5)
        })
        .is_empty()
    );
}

/// Catches the script's view parameters and the preferred refresh not reaching the backend: the
/// eyes sit half the script's IPD either side of the head, and the reported display period
/// follows the preferred rate over the script's.
#[test]
fn the_script_and_preferred_refresh_shape_the_simulated_headset() {
    let mut s = script(vec![key(0, 0.0)]);
    s.ipd_m = 0.07;
    s.fov_deg = 90.0;
    s.refresh_hz = 60.0;
    let mut app = app_with(sim_config(5), Some(s.clone()));
    assert_eq!(app.world().resource::<XrState>().refresh_hz, 60.0);
    drive(&mut app, ScriptState::Focused);
    app.update();
    let views = last_views(app.world()).expect("a frame ran");
    assert!(
        (views[0].pose.position.x + 0.035).abs() < 1e-6,
        "{:?}",
        views[0].pose.position
    );
    assert!((views[1].pose.position.x - 0.035).abs() < 1e-6);
    assert!(
        (views[0].pose.position.y - 1.6).abs() < 1e-6,
        "the scripted head height"
    );
    let half = std::f32::consts::FRAC_PI_4;
    assert!((views[0].fov.right - half).abs() < 1e-6 && (views[0].fov.left + half).abs() < 1e-6);
    let period = app
        .world()
        .resource::<XrSession>()
        .extracted()
        .expect("a frame")
        .period_ns;
    assert_eq!(period, 16_666_667, "60 Hz");

    let mut app = app_with(
        XrConfig {
            preferred_refresh_hz: Some(72.0),
            ..sim_config(5)
        },
        Some(s),
    );
    assert_eq!(
        app.world().resource::<XrState>().refresh_hz,
        72.0,
        "preferred wins over the script"
    );
    drive(&mut app, ScriptState::Focused);
    app.update();
    assert_eq!(
        app.world()
            .resource::<XrSession>()
            .extracted()
            .expect("frame")
            .period_ns,
        13_888_889
    );
}

/// Catches a mask-change event or a new session not bumping the counter the eyes re-upload on.
#[test]
fn the_visibility_mask_generation_counts_changes_and_new_sessions() {
    let mut app = sim_app(2);
    let gen_of = |app: &App| app.world().resource::<XrSession>().mask_generation();
    assert_eq!(gen_of(&app), 0);
    for expected in [1, 2] {
        app.world_mut()
            .resource_mut::<XrRuntime>()
            .backend_mut()
            .simulate(SimEvent::LensMask {
                radius: 0.4 + 0.1 * expected as f32,
            })
            .expect("lens");
        app.update();
        assert_eq!(gen_of(&app), expected);
    }
    drive(&mut app, ScriptState::LossPending);
    for _ in 0..3 {
        app.update();
    }
    assert_eq!(app_state(&app), XrAppState::Idle, "re-created");
    assert_eq!(gen_of(&app), 3, "a new session loads the mask again");
}

// ---------------------------------------------------------------------------------------------
// Loss, retry, exit, disable
// ---------------------------------------------------------------------------------------------

/// Catches a loss before any frame ran (Idle) skipping the state or the message: the app goes
/// Idle -> LossPending -> NoSession with exactly one runtime-lost message.
#[test]
fn losing_an_idle_session_reports_both_transitions() {
    let mut app = sim_app(5);
    drive(&mut app, ScriptState::LossPending);
    let changes: Vec<_> = messages::<XrStateChanged>(&app)
        .iter()
        .map(|m| (m.from, m.to))
        .collect();
    assert_eq!(
        changes,
        [
            (XrAppState::Idle, XrAppState::LossPending),
            (XrAppState::LossPending, XrAppState::NoSession)
        ]
    );
    assert_eq!(messages::<XrRuntimeLost>(&app).len(), 1);
    let st = app.world().resource::<XrState>();
    assert_eq!(st.retry_in, Some(5));
    assert_eq!(st.sessions_created, 1);
}

/// Catches input outliving its session: a stick held through the headset going away must be
/// released at once, not for the whole retry window, and the pose table must empty too.
#[test]
fn a_lost_session_releases_every_held_input_and_pose() {
    let mut app = sim_app(30);
    drive(&mut app, ScriptState::Focused);
    drive_input(
        app.world_mut(),
        "move",
        "left",
        XrActionValue::Vec2(Vec2::new(0.0, 1.0)),
    )
    .expect("inject");
    app.update();
    assert_eq!(
        injected(&app, "move"),
        Some(Vec3::new(0.0, 1.0, 0.0)),
        "held while focused"
    );
    drive(&mut app, ScriptState::LossPending);
    app.update();
    assert_eq!(
        injected(&app, "move"),
        None,
        "the player must stop moving when the headset is lost"
    );
    assert!(
        app.world().resource::<ActionPoses>().poses.is_empty(),
        "the pose table is emptied, not left full of (false, identity) entries"
    );
}

/// Catches disabling XR leaving a held stick driving the game forever (no session means no sync
/// to release it), and a disabled XR coming back by itself.
#[test]
fn disabling_xr_releases_held_input_and_stays_disabled_until_enabled() {
    let mut app = sim_app(2);
    drive(&mut app, ScriptState::Focused);
    drive_input(
        app.world_mut(),
        "move",
        "left",
        XrActionValue::Vec2(Vec2::new(1.0, 0.0)),
    )
    .expect("inject");
    app.update();
    assert_eq!(injected(&app, "move"), Some(Vec3::new(1.0, 0.0, 0.0)));
    assert_eq!(
        set_option(app.world_mut(), "enabled", &OptionValue::Bool(false)),
        Ok(AppliedAt::Now)
    );
    for _ in 0..10 {
        app.update();
    }
    assert_eq!(injected(&app, "move"), None, "nothing is left holding the stick");
    assert_eq!(app_state(&app), XrAppState::Off);
    assert!(
        !app.world().contains_resource::<XrRuntime>(),
        "no retry brings it back"
    );
    assert_eq!(app.world().resource::<XrState>().retry_in, None);
    assert_eq!(app.world().resource::<XrState>().sessions_created, 1);
    assert_eq!(option_value(app.world(), "enabled"), Ok(OptionValue::Bool(false)));
    // Turning it on creates exactly one fresh session; turning it on again is a no-op.
    assert_eq!(
        set_option(app.world_mut(), "enabled", &OptionValue::Bool(true)),
        Ok(AppliedAt::Now)
    );
    assert_eq!(app_state(&app), XrAppState::Idle);
    assert_eq!(app.world().resource::<XrState>().sessions_created, 2);
    assert_eq!(
        set_option(app.world_mut(), "enabled", &OptionValue::Bool(true)),
        Ok(AppliedAt::Now)
    );
    assert_eq!(app.world().resource::<XrState>().sessions_created, 2);
    assert_eq!(
        set_option(app.world_mut(), "enabled", &OptionValue::Number(1.0)).map_err(|e| e.code()),
        Err("xr_invalid_value")
    );
}

/// Catches the retry counter: re-creation lands on exactly the `retry_frames`-th fixed step after
/// the loss, whatever the setting.
#[test]
fn the_retry_counter_follows_retry_frames() {
    for frames in [1_u32, 2, 3, 7] {
        let mut app = sim_app(frames);
        drive(&mut app, ScriptState::Focused);
        drive(&mut app, ScriptState::LossPending);
        let mut created_at = None;
        for step in 1..=12_u32 {
            app.update();
            if app.world().contains_resource::<XrRuntime>() {
                created_at = Some(step);
                break;
            }
        }
        assert_eq!(created_at, Some(frames), "retry_frames {frames}");
        assert_eq!(app.world().resource::<XrState>().retry_in, None);
        assert_eq!(app_state(&app), XrAppState::Idle);
    }
}

/// Catches a failed re-creation giving up or retrying every step: it records the error, waits a
/// full `retry_frames` again, and recovers as soon as a session can be made.
#[test]
fn a_failed_retry_waits_again_and_recovers_when_a_backend_returns() {
    let mut app = sim_app(3);
    drive(&mut app, ScriptState::Focused);
    drive(&mut app, ScriptState::LossPending);
    // The simulator is replaced by a backend this build cannot open.
    app.world_mut().resource_mut::<XrConfig>().backend = XrBackendKind::OpenXr;
    for _ in 0..3 {
        app.update();
    }
    let st = app.world().resource::<XrState>();
    assert_eq!(
        st.last_error.as_ref().map(|e| e.0.as_str()),
        Some("xr_no_loader"),
        "the retry failed"
    );
    assert_eq!(st.retry_in, Some(3), "a full wait starts again");
    assert_eq!(st.app, XrAppState::NoSession);
    assert_eq!(st.sessions_created, 1);
    app.world_mut().resource_mut::<XrConfig>().backend = XrBackendKind::Simulator;
    for step in 1..=3 {
        app.update();
        assert_eq!(
            app.world().contains_resource::<XrRuntime>(),
            step == 3,
            "the next attempt is three steps later"
        );
    }
    assert_eq!(app_state(&app), XrAppState::Idle);
    assert_eq!(app.world().resource::<XrState>().sessions_created, 2);
}

/// Catches the retry path recording `XrError::NoLoader`'s install text. Startup and `enabled`
/// already say the OpenXR binding is not built; a failed re-create must say the same thing.
#[test]
fn a_failed_retry_says_the_binding_is_not_built() {
    let mut app = sim_app(3);
    drive(&mut app, ScriptState::Focused);
    drive(&mut app, ScriptState::LossPending);
    app.world_mut().resource_mut::<XrConfig>().backend = XrBackendKind::OpenXr;
    for _ in 0..3 {
        app.update();
    }
    let (code, message) = app
        .world()
        .resource::<XrState>()
        .last_error
        .clone()
        .expect("the retry failed");
    assert_eq!(code, "xr_no_loader");
    assert!(
        message.contains("OpenXR binding is not built"),
        "retry must not blame the install: {message:?}"
    );
    assert!(
        !message.contains("not installed"),
        "retry must not blame the install: {message:?}"
    );
}

/// Catches a non-finite `xr.drive input` being held and published. A NaN stick or pose must be
/// refused, and the next sync must not publish it.
#[test]
fn a_non_finite_driven_input_is_refused_and_does_not_poison_published_state() {
    let mut app = sim_app(5);
    drive(&mut app, ScriptState::Focused);
    let err = drive_input(
        app.world_mut(),
        "move",
        "left",
        XrActionValue::Vec2(Vec2::new(f32::NAN, 1.0)),
    )
    .expect_err("non-finite stick");
    assert_eq!(err.code(), "xr_invalid_value");
    let err = drive_input(
        app.world_mut(),
        "grip_pose",
        "right",
        XrActionValue::Pose(Pose {
            position: Vec3::new(1.0, f32::INFINITY, 0.0),
            orientation: Quat::IDENTITY,
        }),
    )
    .expect_err("non-finite pose");
    assert_eq!(err.code(), "xr_invalid_value");
    app.update();
    assert_eq!(injected(&app, "move"), None, "a refused stick is not held");
    assert!(
        app.world()
            .resource::<ActionPoses>()
            .get("grip_pose", "right")
            .is_none(),
        "a refused pose is not published"
    );
    assert_eq!(
        app.world().resource::<XrActionStates>().get("move", "left"),
        Some((false, XrActionValue::Vec2(Vec2::ZERO)))
    );
}

/// Catches the simulator drives answering while no session exists: each says "not running", and
/// recentering refuses with the session state it found.
#[test]
fn drives_and_recenter_refuse_while_the_session_is_gone() {
    let mut app = sim_app(50);
    drive(&mut app, ScriptState::LossPending);
    let w = app.world_mut();
    for result in [
        drive_session_state(w, ScriptState::Focused).map(|_| ()),
        drive_pose(w, Some(Pose::IDENTITY), [None, None]),
        drive_input(w, "select", "left", XrActionValue::Float(1.0)),
    ] {
        assert_eq!(result.map_err(|e| e.code()), Err("xr_not_running"));
    }
    assert_eq!(recenter(w).map_err(|e| e.code()), Err("xr_not_running"));
    assert!(eye_matrices(w).is_none() && last_views(w).is_none());
}

/// Catches exit restarting the session: after the runtime says exit, the runtime is gone, one
/// exit message is sent, and no retry ever brings XR back.
#[test]
fn exiting_is_final() {
    let mut app = sim_app(2);
    drive(&mut app, ScriptState::Exiting);
    assert_eq!(app_state(&app), XrAppState::Exiting);
    let exits: Vec<_> = messages::<AppExit>(&app);
    assert_eq!(exits, vec![AppExit::Success]);
    for _ in 0..20 {
        app.update();
    }
    assert_eq!(app_state(&app), XrAppState::Exiting);
    assert!(!app.world().contains_resource::<XrRuntime>());
    let st = app.world().resource::<XrState>();
    assert_eq!((st.retry_in, st.sessions_created), (None, 1));
}

// ---------------------------------------------------------------------------------------------
// Frames and focus
// ---------------------------------------------------------------------------------------------

/// Catches frames running before the session is up, and the focus messages: no frame, no stale
/// count while Idle; frame indices are consecutive once running; focus changes are reported once
/// each way.
#[test]
fn idle_runs_no_frames_and_focus_changes_are_reported_once() {
    let mut app = sim_app(5);
    for _ in 0..5 {
        app.update();
    }
    let st = app.world().resource::<XrState>();
    assert_eq!(
        (st.stats.frames, st.stats.stale_frames, st.stats.dropped_frames),
        (0, 0, 0)
    );
    assert!(app.world().resource::<XrSession>().extracted().is_none());
    drive(&mut app, ScriptState::Focused);
    let focus: Vec<_> = messages::<XrFocusChanged>(&app)
        .iter()
        .map(|m| m.focused)
        .collect();
    assert_eq!(focus, [true]);
    let mut indices = Vec::new();
    for _ in 0..4 {
        app.update();
        indices.push(
            app.world()
                .resource::<XrSession>()
                .extracted()
                .map(|f| f.frame_index),
        );
    }
    assert_eq!(indices, [Some(0), Some(1), Some(2), Some(3)]);
    let st = app.world().resource::<XrState>();
    assert_eq!((st.stats.frames, st.stats.stale_frames), (4, 0));
    let mailbox_view = app
        .world()
        .resource::<XrSession>()
        .mailbox()
        .peek()
        .expect("published");
    assert_eq!(
        mailbox_view.frame_index, 3,
        "the shared mailbox holds the newest frame"
    );
    drive(&mut app, ScriptState::Visible);
    let focus: Vec<_> = messages::<XrFocusChanged>(&app)
        .iter()
        .map(|m| m.focused)
        .collect();
    assert_eq!(focus, [false]);
}

/// Catches non-focused states reading as focused: input is dead while merely visible or
/// synchronized, and a turn queued then fires once on focus.
#[test]
fn input_actions_only_count_while_focused() {
    let mut app = sim_app(5);
    let (rig, _, _) = spawn_rig(&mut app, Vec3::ZERO);
    drive(&mut app, ScriptState::Visible);
    drive_input(
        app.world_mut(),
        "turn",
        "right",
        XrActionValue::Vec2(Vec2::new(1.0, 0.0)),
    )
    .expect("turn");
    for _ in 0..10 {
        app.update();
    }
    assert_eq!(
        app.world().resource::<XrSession>().snaps(),
        0,
        "visible is not focused"
    );
    assert_eq!(
        app.world().get::<Transform>(rig).expect("rig").rotation,
        Quat::IDENTITY
    );
    drive(&mut app, ScriptState::Focused);
    app.update();
    assert_eq!(app.world().resource::<XrSession>().snaps(), 1);
    let yaw = yaw_of(app.world().get::<Transform>(rig).expect("rig").rotation);
    assert!((yaw + 30f32.to_radians()).abs() < 1e-5, "{yaw}");
}

/// Catches the dominant-hand swap not reaching the live action states or the turn lookup: a
/// left-handed player turns with the left stick, and the right stick's `turn` no longer exists.
#[test]
fn a_left_handed_config_turns_with_the_left_stick() {
    let mut app = app_with(
        XrConfig {
            comfort: regia_xr::ComfortSettings {
                dominant_hand: DominantHand::Left,
                ..Default::default()
            },
            ..sim_config(5)
        },
        None,
    );
    spawn_rig(&mut app, Vec3::ZERO);
    drive(&mut app, ScriptState::Focused);
    let states = app.world().resource::<XrActionStates>();
    assert!(states.get("turn", "left").is_some() && states.get("turn", "right").is_none());
    assert!(states.get("move", "right").is_some() && states.get("move", "left").is_none());
    assert_eq!(
        drive_input(app.world_mut(), "turn", "right", XrActionValue::Vec2(Vec2::X)).map_err(|e| e.code()),
        Err("xr_action_unknown")
    );
    drive_input(
        app.world_mut(),
        "turn",
        "left",
        XrActionValue::Vec2(Vec2::new(-0.9, 0.0)),
    )
    .expect("turn");
    app.update();
    assert_eq!(app.world().resource::<XrSession>().snaps(), 1);
}

// ---------------------------------------------------------------------------------------------
// Rig, recenter, seated
// ---------------------------------------------------------------------------------------------

/// Catches the vignette being fed a bogus first sample, and its cap: no rig motion yet on the first
/// step, a steady 3 m/s walk converges on `vignette_strength` without passing it, and standing
/// still decays it to zero.
#[test]
fn walking_the_rig_drives_the_vignette_up_to_its_cap_and_back() {
    let mut app = sim_app(5);
    let (rig, _, _) = spawn_rig(&mut app, Vec3::new(2.0, 0.0, 3.0));
    drive(&mut app, ScriptState::Focused);
    app.update();
    assert_eq!(
        app.world().resource::<XrSession>().vignette(),
        0.0,
        "spawning is not motion"
    );
    let mut last = 0.0;
    for _ in 0..45 {
        app.world_mut()
            .get_mut::<Transform>(rig)
            .expect("rig")
            .translation
            .x += 0.05;
        app.update();
        let now = app.world().resource::<XrSession>().vignette();
        assert!(now >= last && now <= 0.5 + 1e-6, "{now} after {last}");
        last = now;
    }
    assert!((last - 0.5).abs() < 1e-3, "settled at the strength: {last}");
    for _ in 0..200 {
        app.update();
    }
    assert_eq!(app.world().resource::<XrSession>().vignette(), 0.0);
}

/// Catches the entity transforms ignoring the recenter offset: after recentering, the head entity
/// sits over the rig origin at its tracked height, facing the rig's forward, the rig itself does not
/// move, and a controller follows the same offset.
#[test]
fn recentering_moves_the_head_and_controllers_but_not_the_rig() {
    let mut app = sim_app(5);
    let (rig, head, hand) = spawn_rig(&mut app, Vec3::new(2.0, 0.0, 3.0));
    drive(&mut app, ScriptState::Focused);
    let head_pose = Pose::new(Vec3::new(0.5, 1.6, -0.2), Quat::from_rotation_y(1.0));
    let hand_pose = Pose::new(Vec3::new(0.7, 1.2, -0.2), Quat::IDENTITY);
    drive_pose(app.world_mut(), Some(head_pose), [None, Some(hand_pose)]).expect("pose");
    app.update();
    let offset = recenter(app.world_mut()).expect("recenter");
    let origin: Vec<_> = messages::<XrOriginChanged>(&app)
        .iter()
        .map(|m| (m.space, m.offset))
        .collect();
    assert_eq!(origin, [(TrackingSpace::Stage, offset)]);
    app.update();
    let head_t = app.world().get::<Transform>(head).expect("head");
    assert!(
        head_t.translation.x.abs() < 1e-5 && head_t.translation.z.abs() < 1e-5,
        "{:?}",
        head_t.translation
    );
    assert!((head_t.translation.y - 1.6).abs() < 1e-5);
    assert!(yaw_of(head_t.rotation).abs() < 1e-5, "facing the rig's forward");
    let rig_t = app.world().get::<Transform>(rig).expect("rig");
    assert_eq!(rig_t.translation, Vec3::new(2.0, 0.0, 3.0));
    let expected = offset.compose(&hand_pose);
    let hand_t = app.world().get::<Transform>(hand).expect("hand");
    assert!((hand_t.translation - expected.position).length() < 1e-5);
    assert!(hand_t.rotation.angle_between(expected.orientation) < 1e-5);
}

/// Catches `recenter` allowed in states without a headset, and in a world without the plugin.
#[test]
fn recenter_needs_the_plugin_and_a_live_or_starting_session() {
    let mut bare = App::new();
    assert!(matches!(
        recenter(bare.world_mut()),
        Err(XrError::Unsupported("xr plugin"))
    ));
    let mut off = app_with(
        XrConfig {
            backend: XrBackendKind::Off,
            ..XrConfig::default()
        },
        None,
    );
    assert_eq!(
        recenter(off.world_mut()).map_err(|e| e.code()),
        Err("xr_not_running")
    );
    let mut app = sim_app(5);
    assert!(recenter(app.world_mut()).is_ok(), "idle: the session exists");
    drive(&mut app, ScriptState::Focused);
    assert!(recenter(app.world_mut()).is_ok());
}

/// Catches seated mode losing its eye-height lift: seated plus the `local` space lifts the rig
/// offset by the eye height, `eye_height_m` re-lifts at once, recentering keeps the lift, asking
/// for `stage` while seated keeps `Local` and the lift (spec 58 § 2.8), and standing on a stage
/// space needs none.
#[test]
fn seated_mode_lifts_by_the_eye_height_until_the_stage_space_is_used() {
    let mut app = app_with(
        XrConfig {
            tracking_space: TrackingSpace::Local,
            comfort: regia_xr::ComfortSettings {
                seated: true,
                ..Default::default()
            },
            ..sim_config(5)
        },
        None,
    );
    let (_, head, _) = spawn_rig(&mut app, Vec3::ZERO);
    let offset_y = |app: &App| app.world().resource::<XrSession>().offset.position.y;
    assert_eq!(
        app.world().resource::<XrState>().actual_tracking_space,
        TrackingSpace::Local
    );
    assert_eq!(offset_y(&app), 1.65);
    drive(&mut app, ScriptState::Focused);
    drive_pose(
        app.world_mut(),
        Some(Pose::new(Vec3::new(0.0, 0.1, 0.0), Quat::from_rotation_y(0.5))),
        [None, None],
    )
    .expect("pose");
    app.update();
    assert!((app.world().get::<Transform>(head).expect("head").translation.y - 1.75).abs() < 1e-5);
    assert_eq!(
        set_option(app.world_mut(), "eye_height_m", &OptionValue::Number(1.25)),
        Ok(AppliedAt::Now)
    );
    assert_eq!(offset_y(&app), 1.25);
    let offset = recenter(app.world_mut()).expect("recenter");
    assert_eq!(offset.position.y, 1.25, "recentering keeps the floor lift");
    assert!(yaw_of(offset.orientation).abs() > 0.4, "and fixes the heading");
    // Seated play stays `Local` plus the lift whatever space is asked for (spec 58 § 2.8): the
    // request is recorded, the runtime space and the lift do not move.
    set_option(
        app.world_mut(),
        "tracking_space",
        &OptionValue::Text("stage".into()),
    )
    .expect("stage");
    assert_eq!(
        app.world().resource::<XrState>().tracking_space,
        TrackingSpace::Stage
    );
    assert_eq!(
        app.world().resource::<XrState>().actual_tracking_space,
        TrackingSpace::Local
    );
    assert_eq!(offset_y(&app), 1.25, "seated keeps the eye-height lift");
    let last = messages::<XrOriginChanged>(&app)
        .last()
        .copied()
        .expect("origin message");
    assert_eq!(last.space, TrackingSpace::Local);

    // Standing play on the runtime's stage floor needs no emulation.
    let mut standing = app_with(
        XrConfig {
            tracking_space: TrackingSpace::Local,
            ..sim_config(5)
        },
        None,
    );
    set_option(
        standing.world_mut(),
        "tracking_space",
        &OptionValue::Text("stage".into()),
    )
    .expect("stage");
    assert_eq!(offset_y(&standing), 0.0, "the runtime floor needs no emulation");
    let last = messages::<XrOriginChanged>(&standing)
        .last()
        .copied()
        .expect("origin message");
    assert_eq!(last.space, TrackingSpace::Stage);
}

// ---------------------------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------------------------

/// Catches an option key that cannot be read, or a stray key accepted.
#[test]
fn every_option_key_reads_and_an_unknown_one_is_refused() {
    let mut app = sim_app(5);
    assert_eq!(OPTION_KEYS.len(), 14);
    for key in OPTION_KEYS {
        assert!(option_value(app.world(), key).is_ok(), "{key}");
    }
    assert_eq!(
        option_value(app.world(), "warp").map_err(|e| e.code()),
        Err("xr_unknown_option")
    );
    assert_eq!(
        set_option(app.world_mut(), "warp", &OptionValue::Bool(true)).map_err(|e| e.code()),
        Err("xr_unknown_option")
    );
    assert_eq!(option_value(app.world(), "enabled"), Ok(OptionValue::Bool(true)));
    assert_eq!(
        option_value(app.world(), "stereo_mode"),
        Ok(OptionValue::Text("two_pass".into()))
    );
    assert_eq!(
        option_value(app.world(), "runtime_json"),
        Ok(OptionValue::Text(String::new()))
    );
}

/// Catches a range edge or an apply-time label moving, for every numeric option: edges pass,
/// just outside fails with the old value kept, and the label is the documented one.
#[test]
fn numeric_options_have_inclusive_ranges_and_documented_apply_times() {
    let mut app = sim_app(5);
    let cases: [(&str, f64, f64, AppliedAt); 5] = [
        ("render_scale", 0.5, 2.0, AppliedAt::NextFrame),
        ("snap_turn_deg", 0.0, 180.0, AppliedAt::Now),
        ("vignette_strength", 0.0, 1.0, AppliedAt::Now),
        ("eye_height_m", 0.5, 2.5, AppliedAt::Now),
        ("refresh_hz", 30.0, 240.0, AppliedAt::NextSession),
    ];
    for (key, lo, hi, when) in cases {
        let w = app.world_mut();
        assert_eq!(
            set_option(w, key, &OptionValue::Number(lo)),
            Ok(when),
            "{key} low edge"
        );
        assert_eq!(
            set_option(w, key, &OptionValue::Number(hi)),
            Ok(when),
            "{key} high edge"
        );
        assert_eq!(option_value(w, key), Ok(OptionValue::Number(hi)));
        for bad in [lo - 0.01, hi + 0.01, f64::NAN, f64::INFINITY] {
            let e = set_option(w, key, &OptionValue::Number(bad)).expect_err(key);
            assert_eq!(e.code(), "xr_invalid_value", "{key} {bad}");
            assert_eq!(
                option_value(w, key),
                Ok(OptionValue::Number(hi)),
                "{key}: a refusal keeps the old value"
            );
        }
        for wrong in [OptionValue::Bool(true), OptionValue::Text("1".into())] {
            assert!(set_option(w, key, &wrong).is_err(), "{key} {wrong:?}");
        }
    }
    let config = app.world().resource::<XrConfig>();
    assert_eq!(config.render_scale, 2.0);
    assert_eq!(config.comfort.snap_turn_deg, 180.0);
    assert_eq!(config.comfort.vignette_strength, 1.0);
    assert_eq!(config.comfort.eye_height_m, 2.5);
    assert_eq!(config.preferred_refresh_hz, Some(240.0));
    assert_eq!(app.world().resource::<XrState>().render_scale, 2.0);
    for (when, name) in [
        (AppliedAt::Now, "now"),
        (AppliedAt::NextFrame, "next_frame"),
        (AppliedAt::NextSession, "next_session"),
    ] {
        assert_eq!(when.name(), name);
    }
}

/// Catches the named options accepting a wrong name or type, and applying the wrong effect.
#[test]
fn named_and_boolean_options() {
    let mut app = sim_app(5);
    let w = app.world_mut();
    let text = |s: &str| OptionValue::Text(s.into());
    assert_eq!(
        set_option(w, "foveation", &text("high")),
        Ok(AppliedAt::NextFrame)
    );
    assert_eq!(w.resource::<XrState>().foveation, regia_xr::Foveation::High);
    assert_eq!(w.resource::<XrConfig>().foveation, regia_xr::Foveation::High);
    for bad in [text("max"), text("High"), OptionValue::Number(1.0)] {
        assert_eq!(
            set_option(w, "foveation", &bad).map_err(|e| e.code()),
            Err("xr_invalid_value")
        );
    }
    assert_eq!(set_option(w, "mirror", &text("both")), Ok(AppliedAt::Now));
    assert_eq!(w.resource::<XrState>().mirror, regia_xr::Mirror::Both);
    assert!(set_option(w, "mirror", &text("center")).is_err());
    assert_eq!(
        set_option(w, "tracking_space", &text("local_floor")),
        Ok(AppliedAt::Now)
    );
    assert_eq!(w.resource::<XrState>().tracking_space, TrackingSpace::LocalFloor);
    assert_eq!(
        w.resource::<XrState>().actual_tracking_space,
        TrackingSpace::LocalFloor
    );
    assert!(set_option(w, "tracking_space", &text("Stage")).is_err());
    assert_eq!(
        set_option(w, "stereo_mode", &text("multiview")),
        Ok(AppliedAt::NextFrame)
    );
    assert_eq!(w.resource::<XrRenderProfile>().stereo_mode, StereoMode::Multiview);
    assert!(set_option(w, "stereo_mode", &text("quad")).is_err());
    for key in ["hidden_area", "depth_layer"] {
        assert_eq!(
            set_option(w, key, &OptionValue::Bool(false)),
            Ok(AppliedAt::NextFrame)
        );
        assert_eq!(option_value(w, key), Ok(OptionValue::Bool(false)));
        assert!(set_option(w, key, &OptionValue::Number(0.0)).is_err(), "{key}");
        assert_eq!(option_value(w, key), Ok(OptionValue::Bool(false)));
    }
    let profile = w.resource::<XrRenderProfile>();
    assert!(!profile.hidden_area && !profile.depth_layer);
}

/// Catches `runtime_json` mishandling: stored as given, cleared by an empty string, wrong type
/// refused, applies at the next session.
#[test]
fn the_runtime_json_override_is_stored_and_clearable() {
    let mut app = sim_app(5);
    let w = app.world_mut();
    assert_eq!(
        set_option(
            w,
            "runtime_json",
            &OptionValue::Text("/opt/xr/runtime.json".into())
        ),
        Ok(AppliedAt::NextSession)
    );
    assert_eq!(
        w.resource::<XrSession>().runtime_json(),
        Some("/opt/xr/runtime.json")
    );
    assert_eq!(
        option_value(w, "runtime_json"),
        Ok(OptionValue::Text("/opt/xr/runtime.json".into()))
    );
    assert!(set_option(w, "runtime_json", &OptionValue::Bool(true)).is_err());
    assert_eq!(
        w.resource::<XrSession>().runtime_json(),
        Some("/opt/xr/runtime.json")
    );
    assert_eq!(
        set_option(w, "runtime_json", &OptionValue::Text(String::new())),
        Ok(AppliedAt::NextSession)
    );
    assert_eq!(w.resource::<XrSession>().runtime_json(), None);
}

// ---------------------------------------------------------------------------------------------
// Scripts
// ---------------------------------------------------------------------------------------------

/// Catches a script's controller going untracked not being honoured, and held inputs: the right
/// controller disappears at its keyframe, its pose action goes inactive, while a scripted input
/// stays held after the script ends.
#[test]
fn a_script_untracks_a_controller_and_its_inputs_outlive_the_script() {
    let mut a = key(0, 0.0);
    a.state = Some(ScriptState::Focused);
    a.right = Some(ScriptPose {
        position: (0.2, 1.2, -0.3),
        orientation: (0.0, 0.0, 0.0, 1.0),
    });
    a.inputs = vec![("select".into(), "right".into(), ScriptValue::Float(0.5))];
    let b = key(3, 0.3);
    let mut app = app_with(sim_config(5), Some(script(vec![a, b])));
    let mut trace = Vec::new();
    for _ in 0..8 {
        app.update();
        let s = app.world().resource::<XrSession>();
        let pose = app
            .world()
            .resource::<ActionPoses>()
            .get("grip_pose", "right")
            .map(|p| p.position);
        trace.push((
            s.held_controllers()[1].is_some(),
            pose.is_some(),
            s.held_head().position.x,
        ));
    }
    // Ticks 0..2 keep the controller (the next keyframe lacks it, so it is held); tick 3 drops it.
    assert_eq!(
        trace.iter().map(|t| (t.0, t.1)).collect::<Vec<_>>(),
        [
            (true, true),
            (true, true),
            (true, true),
            (false, false),
            (false, false),
            (false, false),
            (false, false),
            (false, false)
        ]
    );
    assert!(
        (trace[3].2 - 0.3).abs() < 1e-6,
        "the head reaches the last keyframe"
    );
    assert_eq!(trace[7].2, trace[3].2, "and holds it after the end");
    assert_eq!(
        app.world().resource::<XrActionStates>().get("select", "right"),
        Some((true, XrActionValue::Float(0.5))),
        "an input is held until replaced, not until the script ends"
    );
    assert_eq!(injected(&app, "select_right"), Some(Vec3::new(0.5, 0.0, 0.0)));
    assert_eq!(app.world().resource::<XrSession>().tick(), 8);
}

/// Catches a bad scripted input aborting the rest of its keyframe: a wrong-typed value and an
/// unknown action are recorded (the last one wins), and the valid input after them still applies.
#[test]
fn a_bad_scripted_input_is_recorded_and_the_rest_of_the_keyframe_still_applies() {
    let mut a = key(0, 0.0);
    a.state = Some(ScriptState::Focused);
    a.inputs = vec![
        ("move".into(), "left".into(), ScriptValue::Bool(true)),
        ("warp".into(), String::new(), ScriptValue::Bool(true)),
        ("select".into(), "right".into(), ScriptValue::Float(0.25)),
    ];
    let mut app = app_with(sim_config(5), Some(script(vec![a])));
    app.update();
    let states = app.world().resource::<XrActionStates>();
    assert_eq!(
        states.get("select", "right"),
        Some((true, XrActionValue::Float(0.25)))
    );
    assert_eq!(
        states.get("move", "left"),
        Some((false, XrActionValue::Vec2(Vec2::ZERO))),
        "a wrong-typed value is refused, the action stays released"
    );
    assert_eq!(
        app_state(&app),
        XrAppState::Focused,
        "the state change in the same keyframe still ran"
    );
    let err = app
        .world()
        .resource::<XrState>()
        .last_error
        .clone()
        .expect("recorded");
    assert_eq!(
        err.0, "xr_action_unknown",
        "the later failure is the one reported"
    );
}

/// Catches a script that cannot finish a session round trip: loss mid-script, recreation after the
/// retry, the script's later keyframes reaching the new session, and no scripted input leaking
/// from the old session into the new one.
#[test]
fn a_script_survives_a_session_loss_and_drives_the_new_session() {
    let mut first = key(0, 0.0);
    first.state = Some(ScriptState::Focused);
    first.inputs = vec![("menu".into(), "left".into(), ScriptValue::Bool(true))];
    let mut lost = key(3, 0.3);
    lost.state = Some(ScriptState::LossPending);
    let mut back = key(8, 0.8);
    back.state = Some(ScriptState::Focused);
    let mut app = app_with(sim_config(2), Some(script(vec![first, lost, back])));
    let mut trace = Vec::new();
    for _ in 0..10 {
        app.update();
        trace.push(app_state(&app));
    }
    use XrAppState::{Focused, Idle, NoSession};
    assert_eq!(
        trace,
        [
            Focused, Focused, Focused, NoSession, NoSession, Idle, Idle, Idle, Focused, Focused
        ]
    );
    assert_eq!(app.world().resource::<XrState>().sessions_created, 2);
    assert_eq!(
        app.world().resource::<XrSession>().tick(),
        10,
        "fixed steps never paused for the outage"
    );
    assert_eq!(
        app.world().resource::<XrActionStates>().get("menu", "left"),
        Some((false, XrActionValue::Bool(false))),
        "the new session starts with fresh action state"
    );
    let head = app.world().resource::<XrSession>().held_head();
    assert!(
        (head.position.x - 0.8).abs() < 1e-6,
        "the new session got the later keyframes: {head:?}"
    );
    assert_eq!(
        app.world().resource::<XrState>().stats.frames,
        5,
        "3 frames before the loss, 2 after refocus"
    );
}

/// Catches the recorder and the player disagreeing: a recorded run, written out and replayed in a
/// fresh app, reproduces the head and controller poses bit for bit at every tick.
#[test]
fn a_recorded_session_replays_exactly() {
    let mut app = sim_app(5);
    app.world_mut()
        .resource_mut::<XrSession>()
        .start_recording(100.0, 0.063, 90.0);
    let mut seen = Vec::new();
    for i in 0..12_u32 {
        let t = i as f32;
        let head = Pose::new(
            Vec3::new(0.1 * t, 1.6 + 0.01 * t, -0.05 * t),
            Quat::from_rotation_y(0.07 * t),
        );
        let hand = (i % 4 != 3).then(|| {
            Pose::new(
                Vec3::new(0.3, 1.2 + 0.02 * t, -0.3),
                Quat::from_rotation_x(0.1 * t),
            )
        });
        drive_pose(app.world_mut(), Some(head), [None, hand]).expect("pose");
        app.update();
        let s = app.world().resource::<XrSession>();
        seen.push((s.held_head(), s.held_controllers()));
    }
    let recorded = app
        .world_mut()
        .resource_mut::<XrSession>()
        .stop_recording()
        .expect("a recording");
    assert!(
        app.world_mut()
            .resource_mut::<XrSession>()
            .stop_recording()
            .is_none(),
        "stopping twice"
    );
    let ticks: Vec<u64> = recorded.ticks.iter().map(|t| t.tick).collect();
    assert_eq!(ticks, (0..12).collect::<Vec<_>>(), "one keyframe per fixed step");
    recorded.validate().expect("valid");
    let text = recorded.to_ron().expect("ser");
    let replay_script = XrSimScript::from_ron(&text).expect("reload");
    let mut replay = app_with(sim_config(5), Some(replay_script));
    for (i, (head, controllers)) in seen.iter().enumerate() {
        replay.update();
        let s = replay.world().resource::<XrSession>();
        assert_eq!(
            s.held_head().position.to_array(),
            head.position.to_array(),
            "head tick {i}"
        );
        assert_eq!(
            s.held_head().orientation.to_array(),
            head.orientation.to_array(),
            "head tick {i}"
        );
        for (hand, (replayed, recorded)) in s.held_controllers().iter().zip(controllers).enumerate() {
            assert_eq!(
                replayed.map(|p| p.position.to_array()),
                recorded.map(|p| p.position.to_array()),
                "controller {hand} tick {i}"
            );
        }
    }
}
