// SPDX-License-Identifier: MPL-2.0
//! The no-loader contract (spec 58 § 2.2 rule 5, § 2.3 step 1; `book/agents/xr.md`).
//!
//! The real OpenXR binding (loader, instance, swapchains) is not built, so `Auto` and `OpenXr`
//! must start in app state `Off` with `last_error.code` `xr_no_loader`, keep the desktop game
//! running, and never report a hardware session: no runtime resource, no runtime or headset name,
//! no session created, no frames, no state change. The simulator stays the explicit fallback and
//! never names a runtime or headset either. No GPU, loader or headset: fixed steps only.

use regia_app::prelude::*;
use regia_platform::xr::{XrError, XrRuntime};
use regia_xr::sim::ScriptState;
use regia_xr::{
    OptionValue, XrActionStates, XrAppState, XrBackendKind, XrConfig, XrPlugin, XrSession, XrState,
    XrStateChanged, drive_input, drive_pose, drive_session_state, option_value, recenter, set_option,
};

/// The two backends that need the (unbuilt) OpenXR loader.
const LOADER_KINDS: [XrBackendKind; 2] = [XrBackendKind::Auto, XrBackendKind::OpenXr];

fn app_with(config: XrConfig) -> App {
    let mut app = App::new();
    enable_fixed_step(&mut app, 60.0);
    app.add_plugins(XrPlugin::new(config));
    app
}

fn loader_app(kind: XrBackendKind) -> App {
    app_with(XrConfig {
        backend: kind,
        retry_frames: 3,
        ..XrConfig::default()
    })
}

fn state(app: &App) -> &XrState {
    app.world().resource::<XrState>()
}

fn exits(app: &mut App) -> Vec<AppExit> {
    let messages = app.world_mut().resource_mut::<Messages<AppExit>>();
    let mut cursor = messages.get_cursor();
    cursor.read(&messages).cloned().collect()
}

fn state_changes(app: &mut App) -> usize {
    let messages = app.world_mut().resource_mut::<Messages<XrStateChanged>>();
    let mut cursor = messages.get_cursor();
    cursor.read(&messages).count()
}

/// Everything `xr.get` reads (it maps `XrState` field by field and adds the runtime block only
/// when `XrRuntime` exists) says "no session, no hardware".
fn assert_no_hardware_session(app: &App, kind: XrBackendKind, when: &str) {
    let st = state(app);
    assert_eq!(st.app, XrAppState::Off, "{kind:?} {when}: app state");
    assert_eq!(st.backend_kind, kind, "{kind:?} {when}: configured backend kept");
    assert_eq!(st.backend, "none", "{kind:?} {when}: no backend opened");
    assert_eq!(st.runtime, None, "{kind:?} {when}: no runtime name");
    assert_eq!(st.system, None, "{kind:?} {when}: no headset name");
    assert_eq!(st.sessions_created, 0, "{kind:?} {when}: no session created");
    assert_eq!(
        st.retry_in, None,
        "{kind:?} {when}: a missing loader is not retried"
    );
    assert_eq!(st.refresh_hz, 0.0, "{kind:?} {when}: no display rate");
    assert!(
        st.extensions_enabled.is_empty(),
        "{kind:?} {when}: no instance extensions"
    );
    assert_eq!(st.play_area, None, "{kind:?} {when}: no stage bounds");
    assert_eq!(st.stats, Default::default(), "{kind:?} {when}: no frames");
    assert_eq!(
        st.last_error.as_ref().map(|e| e.0.as_str()),
        Some("xr_no_loader"),
        "{kind:?} {when}: reason"
    );
    assert!(
        !app.world().contains_resource::<XrRuntime>(),
        "{kind:?} {when}: no runtime resource, so xr.get has no state/frames block"
    );
    assert_eq!(
        option_value(app.world(), "enabled").ok(),
        Some(OptionValue::Bool(false)),
        "{kind:?} {when}: xr.set_option reports enabled false"
    );
}

#[test]
fn auto_and_openxr_start_off_with_xr_no_loader_and_no_hardware_session() {
    for kind in LOADER_KINDS {
        let mut app = loader_app(kind);
        assert_no_hardware_session(&app, kind, "after build");
        app.update();
        assert_no_hardware_session(&app, kind, "after one frame");
        assert!(
            exits(&mut app).is_empty(),
            "{kind:?}: not required, the game keeps running"
        );
        assert_eq!(
            state_changes(&mut app),
            0,
            "{kind:?}: no app state change was announced"
        );
    }
}

#[test]
fn the_reason_message_says_the_binding_is_not_built_rather_than_blaming_the_install() {
    // A player with a working OpenXR runtime installed must not be told their install is broken:
    // this build has no OpenXR binding (spec 58 § 3.4 gate), so no loader was looked for.
    for kind in LOADER_KINDS {
        let app = loader_app(kind);
        let (code, message) = state(&app).last_error.clone().expect("reason recorded");
        assert_eq!(code, "xr_no_loader");
        assert!(
            message.contains("OpenXR binding is not built"),
            "{kind:?}: message names the missing binding: {message:?}"
        );
        assert!(
            message.contains("runs without XR"),
            "{kind:?}: message names the fallback: {message:?}"
        );
    }
}

#[test]
fn a_missing_loader_stays_off_across_fixed_steps_and_frames() {
    for kind in LOADER_KINDS {
        let mut app = loader_app(kind);
        let ticks_before = app.world().resource::<XrSession>().tick();
        // Far past `retry_frames`: a missing loader never turns into a scheduled retry.
        for _ in 0..40 {
            app.update();
        }
        assert_eq!(
            app.world().resource::<XrSession>().tick(),
            ticks_before + 40,
            "{kind:?}: the fixed-step systems ran"
        );
        assert_no_hardware_session(&app, kind, "after 40 frames");
        assert_eq!(
            state_changes(&mut app),
            0,
            "{kind:?}: no state change in 40 frames"
        );
        let actions = app.world().resource::<XrActionStates>();
        assert!(
            actions.iter().all(|(_, active, _)| !active),
            "{kind:?}: every action is inactive at its default"
        );
        assert!(exits(&mut app).is_empty(), "{kind:?}: the game keeps running");
    }
}

#[test]
fn enabling_at_runtime_fails_with_the_same_reason_and_creates_nothing() {
    for kind in LOADER_KINDS {
        let mut app = loader_app(kind);
        app.update();
        let err = set_option(app.world_mut(), "enabled", &OptionValue::Bool(true))
            .expect_err("no loader: enabling cannot succeed");
        assert_eq!(err.code(), "xr_no_loader", "{kind:?}");
        assert_no_hardware_session(&app, kind, "after enabled=true");
        let (_, message) = state(&app).last_error.clone().expect("reason recorded");
        assert!(
            message.contains("OpenXR binding is not built"),
            "{kind:?}: the runtime path records the same truthful message: {message:?}"
        );
        // Off and back on: still no session, the configured kind is restored.
        set_option(app.world_mut(), "enabled", &OptionValue::Bool(false)).expect("disable");
        assert_eq!(state(&app).app, XrAppState::Off);
        let err =
            set_option(app.world_mut(), "enabled", &OptionValue::Bool(true)).expect_err("still no loader");
        assert_eq!(err.code(), "xr_no_loader", "{kind:?}");
        assert_eq!(
            app.world().resource::<XrConfig>().backend,
            kind,
            "{kind:?}: kind restored"
        );
        assert_no_hardware_session(&app, kind, "after off and on");
        app.update();
        assert_no_hardware_session(&app, kind, "a frame after off and on");
        assert_eq!(state_changes(&mut app), 0, "{kind:?}: nothing was announced");
    }
}

#[test]
fn simulator_drives_and_recenter_refuse_without_a_session() {
    for kind in LOADER_KINDS {
        let mut app = loader_app(kind);
        app.update();
        let w = app.world_mut();
        let e = drive_session_state(w, ScriptState::Focused).expect_err("no session to drive");
        assert_eq!(e.code(), "xr_not_running", "{kind:?}: session_state");
        let e = drive_pose(w, None, [None, None]).expect_err("no session to drive");
        assert_eq!(e.code(), "xr_not_running", "{kind:?}: pose");
        let e = drive_input(w, "select", "right", regia_xr::XrActionValue::Bool(true))
            .expect_err("no session to drive");
        assert_eq!(e.code(), "xr_not_running", "{kind:?}: input");
        let e = recenter(w).expect_err("no session to recenter");
        assert_eq!(e.code(), "xr_not_running", "{kind:?}: recenter");
        assert_no_hardware_session(&app, kind, "after refused drives");
    }
}

#[test]
fn required_true_without_a_loader_exits_with_an_error_not_a_session() {
    for kind in LOADER_KINDS {
        let mut app = app_with(XrConfig {
            backend: kind,
            required: true,
            ..XrConfig::default()
        });
        let exit = exits(&mut app);
        assert_eq!(exit.len(), 1, "{kind:?}: one exit request");
        assert!(
            matches!(exit[0], AppExit::Error(_)),
            "{kind:?}: an error exit, not success: {:?}",
            exit[0]
        );
        assert_eq!(state(&app).app, XrAppState::Off);
        assert_eq!(state(&app).sessions_created, 0);
        assert!(!app.world().contains_resource::<XrRuntime>());
    }
}

#[test]
fn the_simulator_is_the_explicit_fallback_and_never_names_hardware() {
    let mut app = app_with(XrConfig {
        backend: XrBackendKind::Simulator,
        ..XrConfig::default()
    });
    {
        let st = state(&app);
        assert_eq!(st.app, XrAppState::Idle, "the simulator opens a session");
        assert_eq!(st.backend, "null", "the scripted backend, by name");
        assert_eq!(st.backend_kind, XrBackendKind::Simulator);
        assert_eq!(st.sessions_created, 1);
        assert_eq!(st.last_error, None);
    }
    drive_session_state(app.world_mut(), ScriptState::Focused).expect("focus");
    app.update();
    let st = state(&app);
    assert_eq!(st.app, XrAppState::Focused);
    assert_eq!(st.runtime, None, "the simulator is not an OpenXR runtime");
    assert_eq!(st.system, None, "the simulator is not a headset");
    let rt = app.world().resource::<XrRuntime>();
    assert_eq!(
        rt.backend().name(),
        "null",
        "xr.get.backend says null, never openxr"
    );
}

#[test]
fn backend_off_is_off_without_a_reason() {
    let mut app = app_with(XrConfig {
        backend: XrBackendKind::Off,
        ..XrConfig::default()
    });
    app.update();
    let st = state(&app);
    assert_eq!(st.app, XrAppState::Off);
    assert_eq!(st.backend, "none");
    assert_eq!(st.sessions_created, 0);
    assert_eq!(st.last_error, None, "switched off is not an error");
    assert!(!app.world().contains_resource::<XrRuntime>());
}

#[test]
fn the_no_loader_code_is_stable_protocol_text() {
    let code = XrError::NoLoader.code();
    assert_eq!(code, "xr_no_loader");
    assert!(
        code.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
        "agent error codes are lowercase ascii, digits and `_`"
    );
}
