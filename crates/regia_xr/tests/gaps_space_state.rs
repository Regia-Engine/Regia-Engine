// SPDX-License-Identifier: MPL-2.0
//! Tracking spaces, the app-state table, the render profile defaults and `XrState` defaults
//! (spec 58 §§ 2.2, 2.5). Pure CPU.

use regia_core::glam::{Quat, UVec2, Vec3};
use regia_platform::xr::{Pose, SessionState};
use regia_xr::space::{floor_offset, recenter_offset, resolve_tracking_space, yaw_of};
use regia_xr::{
    Foveation, HandTrackingSupport, Mirror, PlayArea, StereoMode, TrackingSpace, XrAppState, XrBackendKind,
    XrRenderProfile, XrState, XrStats, app_state_for,
};

use TrackingSpace::{Local, LocalFloor, Stage};

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-5
}

/// Catches a fallback chain that skips a link or reorders it, over every subset the runtime could
/// support (8 subsets x 3 requests), written as an independent rule.
#[test]
fn tracking_space_resolution_over_every_supported_subset() {
    for mask in 0..8_u8 {
        let supported: Vec<TrackingSpace> = TrackingSpace::ALL
            .into_iter()
            .enumerate()
            .filter(|(i, _)| mask & (1 << i) != 0)
            .map(|(_, s)| s)
            .collect();
        let has = |s: TrackingSpace| supported.contains(&s);
        let stage = if has(Stage) {
            Stage
        } else if has(LocalFloor) {
            LocalFloor
        } else {
            Local
        };
        let local_floor = if has(LocalFloor) { LocalFloor } else { Local };
        assert_eq!(resolve_tracking_space(Stage, &supported), stage, "{supported:?}");
        assert_eq!(
            resolve_tracking_space(LocalFloor, &supported),
            local_floor,
            "{supported:?}"
        );
        assert_eq!(resolve_tracking_space(Local, &supported), Local, "{supported:?}");
    }
}

/// Catches `is_floor` and the names drifting apart from the fallback logic.
#[test]
fn tracking_space_names_and_floor_flags() {
    assert_eq!(TrackingSpace::ALL, [Local, Stage, LocalFloor]);
    let names: Vec<_> = TrackingSpace::ALL.iter().map(|s| s.name()).collect();
    assert_eq!(names, ["local", "stage", "local_floor"]);
    assert_eq!(TrackingSpace::from_name("Stage"), None, "names are lowercase");
    assert_eq!(TrackingSpace::from_name("localfloor"), None);
    assert_eq!(TrackingSpace::from_name(""), None);
    assert!(!Local.is_floor() && Stage.is_floor() && LocalFloor.is_floor());
    assert_eq!(TrackingSpace::default(), Stage);
}

/// Catches an emulated floor applied when the runtime really offers the floor space, or
/// forgotten when it does not.
#[test]
fn floor_offset_over_every_request_and_actual_pair() {
    for requested in TrackingSpace::ALL {
        for actual in TrackingSpace::ALL {
            let expected = if actual == Local && requested.is_floor() {
                Vec3::new(0.0, 1.8, 0.0)
            } else {
                Vec3::ZERO
            };
            assert_eq!(
                floor_offset(requested, actual, 1.8),
                expected,
                "{requested:?} -> {actual:?}"
            );
        }
    }
}

/// Catches a sign or axis error in `yaw_of`: +90 degrees about Y turns the view toward -X.
#[test]
fn yaw_of_hand_values_and_independence_from_pitch_and_roll() {
    use std::f32::consts::{FRAC_PI_2, PI};
    assert!(close(yaw_of(Quat::IDENTITY), 0.0));
    assert!(close(yaw_of(Quat::from_rotation_y(FRAC_PI_2)), FRAC_PI_2));
    assert!(close(yaw_of(Quat::from_rotation_y(-FRAC_PI_2)), -FRAC_PI_2));
    assert!((yaw_of(Quat::from_rotation_y(PI)).abs() - PI).abs() < 1e-4);
    // Pitch is applied in the head frame (yaw * pitch), roll about the view axis changes nothing.
    assert!(close(
        yaw_of(Quat::from_rotation_y(0.7) * Quat::from_rotation_x(0.5)),
        0.7
    ));
    assert!(close(
        yaw_of(Quat::from_rotation_y(0.4) * Quat::from_rotation_z(1.0)),
        0.4
    ));
    assert!(close(
        yaw_of(Quat::from_rotation_y(-1.2) * Quat::from_rotation_x(-0.9)),
        -1.2
    ));
}

/// Catches a wrong inverse in `recenter_offset`: hand-computed for a head one meter right,
/// looking toward -X.
#[test]
fn recenter_offset_hand_computed() {
    let head = Pose::new(
        Vec3::new(1.0, 1.7, 0.0),
        Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
    );
    let offset = recenter_offset(&head);
    assert!(
        (offset.position - Vec3::new(0.0, 0.0, -1.0)).length() < 1e-5,
        "{:?}",
        offset.position
    );
    assert!(close(yaw_of(offset.orientation), -std::f32::consts::FRAC_PI_2));
    let centered = offset.compose(&head);
    assert!((centered.position - Vec3::new(0.0, 1.7, 0.0)).length() < 1e-5);
    assert!(centered.orientation.angle_between(Quat::IDENTITY) < 1e-5);
}

/// Catches a recenter that tilts the rig or is not stable: the offset is pure yaw, and
/// recentering an already-centered head is the identity.
#[test]
fn recenter_offset_is_pure_yaw_and_idempotent() {
    let head = Pose::new(
        Vec3::new(-0.8, 1.55, 2.5),
        Quat::from_rotation_y(2.2) * Quat::from_rotation_x(0.4) * Quat::from_rotation_z(0.2),
    );
    let offset = recenter_offset(&head);
    assert!(
        (offset.orientation * Vec3::Y - Vec3::Y).length() < 1e-5,
        "no tilt"
    );
    assert!(close(offset.position.y, 0.0), "height untouched");
    let centered = offset.compose(&head);
    let again = recenter_offset(&centered);
    assert!(again.position.length() < 1e-4, "{:?}", again.position);
    assert!(again.orientation.angle_between(Quat::IDENTITY) < 1e-4);
    assert!(
        recenter_offset(&Pose::IDENTITY)
            .orientation
            .angle_between(Quat::IDENTITY)
            < 1e-6
    );
}

/// Catches `enabled` and `has_session` being swapped, or a state missing from the name table.
#[test]
fn app_state_priority_names_and_running_flags() {
    for session in SessionState::ALL {
        assert_eq!(app_state_for(false, false, session), XrAppState::Off);
        assert_eq!(
            app_state_for(false, true, session),
            XrAppState::Off,
            "disabled beats a session"
        );
    }
    let all = [
        (XrAppState::Off, "Off"),
        (XrAppState::NoSession, "NoSession"),
        (XrAppState::Idle, "Idle"),
        (XrAppState::Starting, "Starting"),
        (XrAppState::Running, "Running"),
        (XrAppState::Focused, "Focused"),
        (XrAppState::Stopping, "Stopping"),
        (XrAppState::LossPending, "LossPending"),
        (XrAppState::Exiting, "Exiting"),
    ];
    for (state, name) in all {
        assert_eq!(state.name(), name);
        assert_eq!(
            state.is_running(),
            matches!(state, XrAppState::Running | XrAppState::Focused),
            "{name}"
        );
    }
    assert_eq!(XrAppState::default(), XrAppState::Off);
}

/// Catches defaults drifting: the render profile every eye path starts from, and the state
/// resource `xr.get` reports before any session exists.
#[test]
fn render_profile_and_state_defaults() {
    let p = XrRenderProfile::default();
    assert_eq!(p.view_count, 2);
    assert_eq!(p.stereo_mode, StereoMode::TwoPass);
    assert!(!p.multiview);
    assert_eq!(
        (p.recommended_extent, p.extent),
        (UVec2::new(1440, 1584), UVec2::new(1440, 1584))
    );
    assert_eq!(p.per_eye_history, !cfg!(target_os = "android"));
    assert!(p.hidden_area && p.depth_layer);

    let s = XrState::default();
    assert_eq!(s.app, XrAppState::Off);
    assert_eq!(s.session, SessionState::Idle);
    assert_eq!(s.backend_kind, XrBackendKind::Auto);
    assert_eq!(s.backend, "none");
    assert_eq!((s.tracking_space, s.actual_tracking_space), (Stage, Local));
    assert_eq!((s.foveation, s.mirror), (Foveation::Off, Mirror::Left));
    assert_eq!(s.hand_tracking, HandTrackingSupport::Unsupported);
    assert_eq!(s.stats, XrStats::default());
    assert_eq!((s.retry_in, s.sessions_created, s.render_scale), (None, 0, 1.0));
    assert!(s.last_error.is_none() && s.play_area.is_none() && s.runtime.is_none());
}

/// Catches enum-name tables drifting from the agent protocol's spelling.
#[test]
fn config_enum_name_tables_round_trip_and_reject_other_spellings() {
    let backends: Vec<_> = XrBackendKind::ALL.iter().map(|b| b.name()).collect();
    assert_eq!(backends, ["auto", "openxr", "simulator", "off"]);
    for b in XrBackendKind::ALL {
        assert_eq!(XrBackendKind::from_name(b.name()), Some(b));
    }
    assert_eq!(XrBackendKind::from_name("OpenXr"), None);
    let foveation: Vec<_> = Foveation::ALL.iter().map(|f| f.name()).collect();
    assert_eq!(foveation, ["off", "low", "medium", "high"]);
    for f in Foveation::ALL {
        assert_eq!(Foveation::from_name(f.name()), Some(f));
    }
    assert_eq!(Foveation::from_name("max"), None);
    let mirrors: Vec<_> = Mirror::ALL.iter().map(|m| m.name()).collect();
    assert_eq!(mirrors, ["off", "left", "right", "both"]);
    for m in Mirror::ALL {
        assert_eq!(Mirror::from_name(m.name()), Some(m));
    }
    assert_eq!(Mirror::from_name("Left"), None);
    assert_eq!(HandTrackingSupport::Unsupported.name(), "unsupported");
    assert_eq!(HandTrackingSupport::Supported.name(), "supported");
}

/// Catches `PlayArea` losing a field in RON (it is reported to games through `xr.get`).
#[test]
fn play_area_ron_round_trip() {
    let area = PlayArea {
        width_m: 3.5,
        depth_m: 2.25,
    };
    let text = ron::to_string(&area).expect("ser");
    let back: PlayArea = ron::from_str(&text).expect("de");
    assert_eq!(back, area);
    assert_eq!(
        ron::from_str::<TrackingSpace>("LocalFloor").expect("de"),
        LocalFloor
    );
}
