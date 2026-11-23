// SPDX-License-Identifier: MPL-2.0
//! `config/xr.ron` and `*.xrsim.ron` (spec 58 § 4): round trips of non-default values, bad input of
//! every kind, validation boundaries, the script player's ordering and end-of-script behaviour,
//! the recorder, the session-state path finder, and the schema fingerprints.

use regia_core::glam::{Quat, Vec3};
use regia_platform::xr::{Fov, Pose, SessionCore, SessionState, Transition, XrError};
use regia_xr::formats::{
    xr_actions_migration_sources, xr_actions_schema_fingerprint, xr_config_migration_sources,
    xr_config_schema_fingerprint, xrsim_migration_sources, xrsim_schema_fingerprint,
};
use regia_xr::sim::path_to_state;
use regia_xr::space::yaw_of;
use regia_xr::{
    ComfortSettings, DominantHand, Foveation, Mirror, ScriptPose, ScriptState, ScriptValue, SimPlayer,
    SimRecorder, SimTick, TrackingSpace, XR_CONFIG_FORMAT_VERSION, XR_CONFIG_PATH, XrActionValue,
    XrBackendKind, XrConfig, XrSimScript,
};

fn code_and_key(e: &XrError) -> (&'static str, String) {
    match e {
        XrError::InvalidValue { key, .. } => (e.code(), key.clone()),
        other => (other.code(), String::new()),
    }
}

fn busy_config() -> XrConfig {
    XrConfig {
        format_version: 1,
        backend: XrBackendKind::Simulator,
        required: true,
        tracking_space: TrackingSpace::LocalFloor,
        render_scale: 1.25,
        foveation: Foveation::High,
        mirror: Mirror::Both,
        comfort: ComfortSettings {
            snap_turn_deg: 45.0,
            snap_threshold: 0.75,
            snap_rearm: 0.25,
            snap_cooldown_frames: 3,
            smooth_turn_deg_per_s: 120.0,
            vignette_strength: 0.75,
            vignette_inner: 0.5,
            vignette_outer: 0.875,
            vignette_ease_frames: 12,
            eye_height_m: 1.5,
            dominant_hand: DominantHand::Left,
            seated: true,
        },
        preferred_refresh_hz: Some(72.0),
        retry_frames: 30,
        near_m: 0.0625,
    }
}

/// Catches a field the writer or reader drops: every setting differs from the default.
#[test]
fn a_config_with_every_field_changed_round_trips_through_text_and_a_file() {
    let config = busy_config();
    let text = config.to_ron().expect("ser");
    assert!(text.contains("format_version: 1"), "{text}");
    assert_eq!(XrConfig::from_ron(&text).expect("reload"), config);
    let default = XrConfig::default();
    assert_ne!(config.comfort, default.comfort);
    // Through the project path the engine reads it from.
    assert_eq!(XR_CONFIG_PATH, "config/xr.ron");
    let dir = regia_test_utils::unique_temp_dir("xr-config");
    let path = dir.join(XR_CONFIG_PATH);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&path, &text).expect("write");
    let read = std::fs::read_to_string(&path).expect("read");
    assert_eq!(XrConfig::from_ron(&read).expect("from file"), config);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Catches a partial file losing its defaults: a nested comfort block keeps its other defaults,
/// and a missing `format_version` reads as the current one.
#[test]
fn partial_files_fill_the_rest_from_defaults() {
    let config = XrConfig::from_ron("(comfort: (snap_turn_deg: 0.0, seated: true))").expect("loads");
    assert_eq!(config.format_version, XR_CONFIG_FORMAT_VERSION);
    assert_eq!(config.comfort.snap_turn_deg, 0.0);
    assert!(config.comfort.seated);
    assert_eq!(
        config.comfort.snap_threshold,
        ComfortSettings::default().snap_threshold
    );
    assert_eq!(config.retry_frames, 120);
    assert_eq!(config.near_m, 0.05);
}

/// Catches a reader that panics or accepts: each kind of bad text is an `InvalidValue` on the
/// file, not a default config.
#[test]
fn malformed_config_text_is_an_error_on_the_file() {
    for text in [
        "",
        "garbage",
        "(render_scale: )",
        "(render_scale: \"big\")",
        "(backend: Hyperdrive)",
        "(retry_frames: -1)",
        "(retry_frames: 1.5)",
        "[1, 2, 3]",
        "(format_version: -1)",
        "(format_version: 1) trailing",
        "(comfort: 5)",
        "(preferred_refresh_hz: 90.0)",
    ] {
        let e = XrConfig::from_ron(text).expect_err(text);
        assert_eq!(
            code_and_key(&e),
            ("xr_invalid_value", "xr.ron".to_string()),
            "{text:?}: {e}"
        );
    }
    let e = XrConfig::from_ron("(comfort: (warp: 1))").expect_err("unknown nested field");
    assert!(e.to_string().contains("warp"), "{e}");
}

/// Catches validation boundaries moving by one: inclusive edges pass, the next value fails, and
/// the error names the field.
#[test]
fn config_range_edges() {
    for text in [
        "(render_scale: 0.5)",
        "(render_scale: 2.0)",
        "(retry_frames: 1)",
        "(retry_frames: 7200)",
        "(preferred_refresh_hz: Some(30.0))",
        "(preferred_refresh_hz: Some(240.0))",
        "(near_m: 0.0001)",
        "(format_version: 1)",
    ] {
        XrConfig::from_ron(text).unwrap_or_else(|e| panic!("{text}: {e}"));
    }
    for (text, key) in [
        ("(render_scale: 0.49)", "render_scale"),
        ("(render_scale: 2.01)", "render_scale"),
        ("(render_scale: NaN)", "render_scale"),
        ("(retry_frames: 7201)", "retry_frames"),
        ("(preferred_refresh_hz: Some(29.9))", "preferred_refresh_hz"),
        ("(preferred_refresh_hz: Some(240.1))", "preferred_refresh_hz"),
        ("(near_m: -0.05)", "near_m"),
        ("(near_m: inf)", "near_m"),
        ("(comfort: (snap_rearm: 0.9))", "snap_threshold"),
        ("(comfort: (eye_height_m: 0.2))", "eye_height_m"),
    ] {
        let Err(e) = XrConfig::from_ron(text) else {
            panic!("{text} should be refused");
        };
        assert_eq!(
            code_and_key(&e),
            ("xr_invalid_value", key.to_string()),
            "{text}: {e}"
        );
    }
}

/// Catches the newest-version check using `>=` or running after the field parse: any newer version
/// is refused by version, even with fields this build cannot parse.
#[test]
fn a_newer_version_is_refused_before_anything_else() {
    for version in [2_u32, 7, u32::MAX] {
        let text = format!("(format_version: {version}, brand_new: [1, 2], render_scale: \"?\")");
        assert_eq!(
            XrConfig::from_ron(&text),
            Err(XrError::FormatTooNew {
                what: "xr.ron",
                found: version,
                supported: 1
            })
        );
    }
}

/// Catches the schema fingerprints dropping a field or variant (they gate `check-formats`).
#[test]
fn schema_fingerprints_name_every_field_and_variant() {
    let config = xr_config_schema_fingerprint();
    for needle in [
        "format_version",
        "backend",
        "required",
        "tracking_space",
        "render_scale",
        "foveation",
        "mirror",
        "comfort",
        "preferred_refresh_hz",
        "retry_frames",
        "near_m",
        "snap_turn_deg",
        "snap_cooldown_frames",
        "vignette_ease_frames",
        "eye_height_m",
        "dominant_hand",
        "seated",
        "Simulator",
        "LocalFloor",
        "High",
        "Both",
    ] {
        assert!(
            config.contains(needle),
            "config fingerprint lacks {needle}: {config}"
        );
    }
    let actions = xr_actions_schema_fingerprint();
    for needle in [
        "sets", "bindings", "priority", "actions", "subs", "label", "optional", "profile", "Haptic", "Pose",
    ] {
        assert!(
            actions.contains(needle),
            "actions fingerprint lacks {needle}: {actions}"
        );
    }
    let sim = xrsim_schema_fingerprint();
    for needle in [
        "fov_deg",
        "ipd_m",
        "refresh_hz",
        "ticks",
        "tick",
        "head",
        "left",
        "right",
        "inputs",
        "state",
        "position",
        "orientation",
        "Bool",
        "Float",
        "Vec2",
        "LossPending",
        "Exiting",
    ] {
        assert!(sim.contains(needle), "sim fingerprint lacks {needle}: {sim}");
    }
    assert_ne!(config, actions);
    assert_ne!(actions, sim);
    assert_eq!(config, xr_config_schema_fingerprint(), "deterministic");
    assert!(xr_config_migration_sources().is_empty());
    assert!(xr_actions_migration_sources().is_empty());
    assert!(xrsim_migration_sources().is_empty());
}

// ---------------------------------------------------------------------------------------------
// The simulator script
// ---------------------------------------------------------------------------------------------

fn tick_at(tick: u64, x: f32) -> SimTick {
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

fn script_of(ticks: Vec<SimTick>) -> XrSimScript {
    XrSimScript {
        ticks,
        ..XrSimScript::new()
    }
}

/// Catches the defaults of an omitted header drifting, and the `NullConfig` they imply.
#[test]
fn script_defaults_and_the_null_config_they_imply() {
    let s = XrSimScript::from_ron("(format_version: 1, ticks: [])").expect("loads");
    assert_eq!((s.fov_deg, s.ipd_m, s.refresh_hz), (100.0, 0.063, 90.0));
    assert!(s.ticks.is_empty());
    let c = s.null_config();
    assert_eq!(c.ipd_m, 0.063);
    assert_eq!(c.display_period_ns, 11_111_111, "90 Hz");
    let half = 50f32.to_radians();
    assert_eq!(c.fov, Fov::symmetric(half, half));
    assert_eq!(c.start_time_ns, 0);
    let s60 = XrSimScript {
        refresh_hz: 60.0,
        fov_deg: 90.0,
        ..XrSimScript::new()
    };
    assert_eq!(s60.null_config().display_period_ns, 16_666_667);
    assert_eq!(
        s60.null_config().fov,
        Fov::symmetric(45f32.to_radians(), 45f32.to_radians())
    );
    assert_eq!(XrSimScript::default(), XrSimScript::new());
}

/// Catches validation edges moving, for the numeric header fields and tick ordering.
#[test]
fn script_validation_edges() {
    let ok = |s: XrSimScript| s.validate().is_ok();
    let base = XrSimScript::new();
    assert!(ok(XrSimScript {
        fov_deg: 10.0,
        ..base.clone()
    }));
    assert!(ok(XrSimScript {
        fov_deg: 170.0,
        ..base.clone()
    }));
    assert!(!ok(XrSimScript {
        fov_deg: 9.99,
        ..base.clone()
    }));
    assert!(!ok(XrSimScript {
        fov_deg: 170.01,
        ..base.clone()
    }));
    assert!(!ok(XrSimScript {
        fov_deg: f32::NAN,
        ..base.clone()
    }));
    assert!(ok(XrSimScript {
        refresh_hz: 30.0,
        ..base.clone()
    }));
    assert!(ok(XrSimScript {
        refresh_hz: 240.0,
        ..base.clone()
    }));
    assert!(!ok(XrSimScript {
        refresh_hz: 29.0,
        ..base.clone()
    }));
    assert!(!ok(XrSimScript {
        refresh_hz: 241.0,
        ..base.clone()
    }));
    assert!(ok(XrSimScript {
        ipd_m: 0.02,
        ..base.clone()
    }));
    assert!(ok(XrSimScript {
        ipd_m: 0.1,
        ..base.clone()
    }));
    assert!(!ok(XrSimScript {
        ipd_m: 0.019,
        ..base.clone()
    }));
    assert!(!ok(XrSimScript {
        ipd_m: f32::INFINITY,
        ..base.clone()
    }));
    assert!(!ok(XrSimScript {
        format_version: 0,
        ..base.clone()
    }));
    // Ticks: strictly increasing, not necessarily starting at zero, an equal tick is refused.
    assert!(ok(script_of(vec![
        tick_at(3, 0.0),
        tick_at(4, 0.0),
        tick_at(900, 0.0)
    ])));
    let e = script_of(vec![tick_at(5, 0.0), tick_at(5, 1.0)])
        .validate()
        .expect_err("equal");
    assert_eq!(code_and_key(&e), ("xr_invalid_value", "tick 5".to_string()));
    assert!(e.to_string().contains("strictly"), "{e}");
    assert!(!ok(script_of(vec![tick_at(5, 0.0), tick_at(4, 0.0)])));
    // Non-finite poses in any pose slot.
    let mut bad = tick_at(1, 0.0);
    bad.right = Some(ScriptPose {
        position: (0.0, f32::NAN, 0.0),
        orientation: (0.0, 0.0, 0.0, 1.0),
    });
    let e = script_of(vec![bad]).validate().expect_err("nan");
    assert!(e.to_string().contains("tick 1"), "{e}");
    let mut bad = tick_at(1, 0.0);
    bad.left = Some(ScriptPose {
        position: (0.0, 0.0, 0.0),
        orientation: (0.0, f32::INFINITY, 0.0, 1.0),
    });
    assert!(!ok(script_of(vec![bad])));
}

/// Catches a script input of NaN or infinity loading and then being held. Poses are already
/// refused; a non-finite `Float` or `Vec2` must be too.
#[test]
fn non_finite_script_inputs_are_refused() {
    for value in [
        ScriptValue::Float(f32::NAN),
        ScriptValue::Float(f32::INFINITY),
        ScriptValue::Vec2(f32::NEG_INFINITY, 0.0),
        ScriptValue::Vec2(0.0, f32::NAN),
    ] {
        let mut bad = tick_at(1, 0.0);
        bad.inputs = vec![("move".into(), "left".into(), value)];
        let e = script_of(vec![bad]).validate().expect_err("non-finite input");
        assert_eq!(code_and_key(&e), ("xr_invalid_value", "tick 1".to_string()));
        assert!(e.to_string().contains("finite"), "{e}");
    }
    let text = r#"(format_version: 1, ticks: [(tick: 0, head: (position: (0.0, 1.0, 0.0)), inputs: [("move", "left", Float(NaN))])])"#;
    let e = XrSimScript::from_ron(text).expect_err("ron NaN");
    assert_eq!(e.code(), "xr_invalid_value", "{e}");
}

/// Catches a reader that accepts text it cannot honour: bad syntax, unknown names, wrong arity,
/// a missing required field, and a missing version, all as errors on the file.
#[test]
fn malformed_script_text_is_an_error() {
    for text in [
        "",
        "not ron",
        "(ticks: [])",
        "(format_version: 1)",
        "(format_version: 1, ticks: [(tick: 0)])",
        "(format_version: 1, ticks: [(tick: 0, head: (position: (0.0, 1.0)))])",
        "(format_version: 1, ticks: [(tick: 0, head: (position: (0.0, 1.0, 0.0), orientation: (0.0, 0.0, 1.0)))])",
        "(format_version: 1, ticks: [(tick: 0, head: (position: (0.0, 1.0, 0.0)), state: Some(Dancing))])",
        "(format_version: 1, ticks: [(tick: 0, head: (position: (0.0, 1.0, 0.0)), inputs: [(\"a\", \"left\", Int(3))])])",
        "(format_version: 1, ticks: [(tick: -1, head: (position: (0.0, 1.0, 0.0)))])",
    ] {
        let e = XrSimScript::from_ron(text).expect_err(text);
        assert_eq!(
            code_and_key(&e),
            ("xr_invalid_value", "xrsim.ron".to_string()),
            "{text:?}: {e}"
        );
    }
    let newer = XrSimScript::from_ron("(format_version: 3, anything: true)").expect_err("newer");
    assert_eq!(
        newer,
        XrError::FormatTooNew {
            what: "xrsim.ron",
            found: 3,
            supported: 1
        }
    );
}

/// Catches `to_pose` handling of quaternions: unit ones bit for bit, others normalised, bad ones
/// replaced by the identity orientation.
#[test]
fn script_poses_normalise_or_fall_back_to_identity() {
    let unit = ScriptPose {
        position: (1.0, 2.0, 3.0),
        orientation: Quat::from_rotation_y(0.3).to_array().into(),
    };
    let pose = unit.to_pose();
    assert_eq!(pose.position, Vec3::new(1.0, 2.0, 3.0));
    assert_eq!(
        pose.orientation.to_array(),
        Quat::from_rotation_y(0.3).to_array(),
        "bit for bit"
    );
    let scaled = ScriptPose {
        position: (0.0, 0.0, 0.0),
        orientation: (0.0, 2.0, 0.0, 2.0),
    };
    let q = scaled.to_pose().orientation;
    assert!((q.length() - 1.0).abs() < 1e-6);
    assert!(
        (yaw_of(q) - std::f32::consts::FRAC_PI_2).abs() < 1e-5,
        "(0,2,0,2) is a 90 degree yaw"
    );
    for degenerate in [(0.0, 0.0, 0.0, 0.0), (f32::NAN, 0.0, 0.0, 1.0)] {
        let p = ScriptPose {
            position: (0.5, 0.0, 0.0),
            orientation: degenerate,
        }
        .to_pose();
        assert_eq!(p.orientation, Quat::IDENTITY, "{degenerate:?}");
        assert_eq!(p.position.x, 0.5);
    }
    // The omitted orientation is the identity.
    let s =
        XrSimScript::from_ron("(format_version: 1, ticks: [(tick: 0, head: (position: (1.0, 2.0, 3.0)))])")
            .expect("loads");
    assert_eq!(s.ticks[0].head.to_pose().orientation, Quat::IDENTITY);
}

/// Catches an empty script panicking, and `last_tick` being wrong.
#[test]
fn an_empty_script_has_no_frames_and_no_last_tick() {
    let player = SimPlayer::new(XrSimScript::new());
    assert!(player.frame(0).is_none() && player.frame(u64::MAX).is_none());
    assert_eq!(player.last_tick(), None);
    let player = SimPlayer::new(script_of(vec![tick_at(2, 0.0), tick_at(40, 1.0)]));
    assert_eq!(player.last_tick(), Some(40));
    assert_eq!(player.script().ticks.len(), 2);
}

/// Catches the interval maths: unevenly spaced keyframes interpolate by tick distance, the
/// position at a keyframe tick is exact, the first keyframe holds before the script starts, and
/// the last holds after it ends.
#[test]
fn interpolation_follows_tick_distance_and_holds_at_both_ends() {
    let mut a = tick_at(4, 0.0);
    a.head.orientation = (0.0, 0.0, 0.0, 1.0);
    let mut b = tick_at(20, 8.0);
    let q = Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
    b.head.orientation = (q.x, q.y, q.z, q.w);
    let player = SimPlayer::new(script_of(vec![a, b]));
    let x_at = |t| player.frame(t).expect("frame").head.position.x;
    assert_eq!(x_at(0), 0.0, "before the first keyframe: held");
    assert_eq!(x_at(3), 0.0);
    assert_eq!(x_at(4), 0.0);
    assert!((x_at(8) - 2.0).abs() < 1e-6, "a quarter of the way: {}", x_at(8));
    assert!((x_at(12) - 4.0).abs() < 1e-6);
    assert!((x_at(19) - 7.5).abs() < 1e-6);
    assert_eq!(x_at(20), 8.0, "exact at the keyframe");
    assert_eq!(x_at(21), 8.0, "held after the end");
    assert_eq!(x_at(u64::MAX), 8.0);
    // The orientation is a slerp: half way is 45 degrees of yaw, a quarter is 22.5.
    let yaw = |t| yaw_of(player.frame(t).expect("frame").head.orientation);
    assert!((yaw(12) - 45f32.to_radians()).abs() < 1e-4);
    assert!((yaw(8) - 22.5f32.to_radians()).abs() < 1e-4);
    assert!(
        (yaw(0)).abs() < 1e-6,
        "before the start the first orientation holds"
    );
}

/// Catches controllers interpolating across a tracking gap: a controller present only on one side
/// holds the earlier keyframe's value, one that appears pops in at its keyframe, and one that
/// vanishes stays until its keyframe.
#[test]
fn controllers_do_not_interpolate_across_a_tracking_gap() {
    let pose_at = |x: f32| ScriptPose {
        position: (x, 1.0, 0.0),
        orientation: (0.0, 0.0, 0.0, 1.0),
    };
    let mut a = tick_at(0, 0.0);
    a.left = Some(pose_at(1.0));
    let mut b = tick_at(10, 0.0);
    b.right = Some(pose_at(5.0));
    let mut c = tick_at(20, 0.0);
    c.right = Some(pose_at(9.0));
    let player = SimPlayer::new(script_of(vec![a, b, c]));
    let left = |t| player.frame(t).expect("frame").controllers[0].map(|p| p.position.x);
    let right = |t| player.frame(t).expect("frame").controllers[1].map(|p| p.position.x);
    assert_eq!(left(0), Some(1.0));
    assert_eq!(left(5), Some(1.0), "held, not faded toward the missing keyframe");
    assert_eq!(left(9), Some(1.0));
    assert_eq!(left(10), None, "gone at the keyframe that lacks it");
    assert_eq!(left(15), None);
    assert_eq!(right(5), None, "not tracked before its first keyframe");
    assert_eq!(right(10), Some(5.0));
    assert_eq!(right(15), Some(7.0), "interpolated between two tracked keyframes");
    assert_eq!(right(99), Some(9.0));
}

/// Catches inputs and states replaying: they fire on their keyframe's tick only, in declared
/// order, with every value type converted, and never again once the script has ended.
#[test]
fn inputs_and_states_fire_only_at_their_tick() {
    let mut k = tick_at(3, 0.0);
    k.inputs = vec![
        ("select".into(), "right".into(), ScriptValue::Float(0.5)),
        ("primary".into(), "left".into(), ScriptValue::Bool(true)),
        ("move".into(), "left".into(), ScriptValue::Vec2(0.25, -1.0)),
    ];
    k.state = Some(ScriptState::Visible);
    let player = SimPlayer::new(script_of(vec![tick_at(0, 0.0), k]));
    let at = player.frame(3).expect("frame");
    assert_eq!(at.state, Some(ScriptState::Visible));
    let values: Vec<_> = at
        .inputs
        .iter()
        .map(|(n, s, v)| (n.as_str(), s.as_str(), *v))
        .collect();
    assert_eq!(
        values,
        [
            ("select", "right", XrActionValue::Float(0.5)),
            ("primary", "left", XrActionValue::Bool(true)),
            (
                "move",
                "left",
                XrActionValue::Vec2(regia_core::glam::Vec2::new(0.25, -1.0))
            ),
        ]
    );
    for t in [0, 1, 2, 4, 5, 1000] {
        let f = player.frame(t).expect("frame");
        assert!(f.inputs.is_empty() && f.state.is_none(), "tick {t}");
    }
}

/// Catches the recorder accepting out-of-order ticks (which would write a script the reader
/// refuses), losing controllers or states, or mangling the header.
#[test]
fn the_recorder_keeps_strictly_increasing_ticks_and_what_it_was_given() {
    let mut rec = SimRecorder::new(95.0, 0.07, 120.0);
    assert!(rec.is_empty());
    let head = |x: f32| Pose::new(Vec3::new(x, 1.7, 0.0), Quat::IDENTITY);
    let hand = Pose::new(Vec3::new(0.2, 1.0, -0.4), Quat::from_rotation_x(0.2));
    rec.record(5, &head(1.0), [None, Some(hand)], Some(ScriptState::Focused));
    rec.record(5, &head(9.0), [None, None], None);
    rec.record(3, &head(8.0), [None, None], None);
    rec.record(6, &head(2.0), [Some(hand), None], None);
    assert_eq!(rec.len(), 2, "the repeat and the step back were ignored");
    let script = rec.script();
    assert_eq!(
        (script.fov_deg, script.ipd_m, script.refresh_hz),
        (95.0, 0.07, 120.0)
    );
    assert_eq!(script.ticks[0].tick, 5);
    assert_eq!(script.ticks[0].head.position.0, 1.0);
    assert_eq!(script.ticks[0].state, Some(ScriptState::Focused));
    assert!(script.ticks[0].left.is_none() && script.ticks[0].right.is_some());
    assert!(script.ticks[1].left.is_some() && script.ticks[1].right.is_none());
    script.validate().expect("what it records is a valid script");
    let again = XrSimScript::from_ron(&script.to_ron().expect("ser")).expect("reload");
    assert_eq!(&again, script);
}

/// Catches the path finder returning a sequence the session core refuses, or one that ends in
/// the wrong state, for every ordered pair of states.
#[test]
fn every_found_path_replays_on_a_session_core_and_reaches_its_target() {
    let reach = |target: SessionState| {
        let mut core = SessionCore::new();
        for t in path_to_state(SessionState::Idle, target)? {
            core.transition(t).expect("valid from idle");
        }
        Some(core)
    };
    for from in SessionState::ALL {
        for to in SessionState::ALL {
            let Some(path) = path_to_state(from, to) else {
                continue;
            };
            if from == to {
                assert!(path.is_empty(), "{from:?}");
            }
            // Rebuild a core in `from` (reachable from idle), then walk the path.
            let Some(mut core) = reach(from) else {
                panic!("{from:?} is not reachable from idle, yet a path from it exists");
            };
            assert_eq!(core.state(), from);
            for t in &path {
                core.transition(*t)
                    .unwrap_or_else(|e| panic!("{from:?} -> {to:?}: {t:?}: {e}"));
            }
            assert_eq!(core.state(), to, "{from:?} -> {to:?} via {path:?}");
        }
    }
}

/// Catches terminal states being escapable, and the shortest-path property on known edges.
#[test]
fn exiting_is_terminal_and_short_paths_are_short() {
    for to in SessionState::ALL {
        let path = path_to_state(SessionState::Exiting, to);
        assert_eq!(path.is_some(), to == SessionState::Exiting, "{to:?}");
    }
    assert_eq!(
        path_to_state(SessionState::Idle, SessionState::Idle),
        Some(Vec::new())
    );
    assert_eq!(
        path_to_state(SessionState::Idle, SessionState::Ready),
        Some(vec![Transition::RuntimeReady])
    );
    assert_eq!(
        path_to_state(SessionState::Idle, SessionState::Running),
        Some(vec![Transition::RuntimeReady, Transition::Begin])
    );
    assert_eq!(
        path_to_state(SessionState::Focused, SessionState::LossPending),
        Some(vec![Transition::RuntimeLost])
    );
    assert_eq!(
        path_to_state(SessionState::LossPending, SessionState::Idle),
        Some(vec![Transition::End]),
        "a lost session ends back to idle"
    );
    let focus = path_to_state(SessionState::Idle, SessionState::Focused).expect("path");
    assert_eq!(
        focus.len(),
        5,
        "ready, begin, synchronized, visible, focused: {focus:?}"
    );
    assert_eq!(ScriptState::ALL.len(), 7);
    assert_eq!(
        ScriptState::from_name("idle"),
        None,
        "idle is not a scriptable target"
    );
    assert_eq!(ScriptState::from_name("Focused"), None, "names are lowercase");
    assert_eq!(
        ScriptState::from_name("loss_pending"),
        Some(ScriptState::LossPending)
    );
}
