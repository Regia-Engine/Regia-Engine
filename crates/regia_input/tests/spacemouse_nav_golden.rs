// SPDX-License-Identifier: MPL-2.0
//! The `nav::step` golden fixture (`docs/specs/29-spacemouse.md` § 5): inputs and the deltas the
//! real function produces, checked in at `tests/data/spacemouse_nav_golden.json` so apps that
//! mirror the navigation (Tincal, Red-Rouge) can prove identical behaviour without building
//! Regia Engine.
//!
//! `REGIA_WRITE_NAV_GOLDEN=1` rewrites the file from the current `nav::step` (only when the
//! change in behaviour is intended; say why in the commit). Otherwise the test asserts the
//! checked-in file still matches.

#![cfg(all(feature = "devices", not(target_os = "android")))]

use std::path::PathBuf;

use regia_input::SpaceMouseConfig;
use regia_input::spacemouse::{
    NavContext, NavDelta, NavigationDefaults, SpaceMouseNav, SpaceMouseState, nav,
};
use serde::{Deserialize, Serialize};

/// One row: `delta == nav::step(&state, dt, context)`.
#[derive(Debug, Serialize, Deserialize)]
struct Case {
    name: String,
    state: SpaceMouseState,
    dt: f32,
    context: NavContext,
    delta: NavDelta,
}

/// The file.
#[derive(Debug, Serialize, Deserialize)]
struct Golden {
    format: u32,
    spec: String,
    comparison: String,
    cases: Vec<Case>,
}

fn data_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/spacemouse_nav_golden.json")
}

fn axes(a: [f32; 6]) -> SpaceMouseState {
    SpaceMouseState {
        translation: [a[0], a[1], a[2]],
        rotation: [a[3], a[4], a[5]],
        buttons: 0,
    }
}

fn unlocked(ctx: NavContext) -> NavContext {
    NavContext {
        horizon_lock: false,
        ..ctx
    }
}

/// The inputs, in a fixed order.
fn inputs() -> Vec<(&'static str, SpaceMouseState, f32, NavContext)> {
    let frame = 1.0 / 60.0;
    let half = 0.52; // shapes to exactly 0.25
    let limit = NavigationDefaults::PITCH_LIMIT_DEG.to_radians();
    let mixed = axes([0.7, -0.4, 0.6, 0.5, -0.8, 0.3]);
    let config = |sensitivity: f32, invert: [bool; 6]| SpaceMouseConfig {
        sensitivity,
        invert,
        ..SpaceMouseConfig::DEFAULT
    };
    let dominant = SpaceMouseNav {
        dominant: true,
        ..SpaceMouseNav::default()
    };
    vec![
        // Orbit
        (
            "orbit_idle",
            SpaceMouseState::default(),
            frame,
            NavContext::orbit(5.0),
        ),
        (
            "orbit_pan_right_full",
            axes([1.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
            frame,
            NavContext::orbit(10.0),
        ),
        (
            "orbit_pan_up_half",
            axes([0.0, half, 0.0, 0.0, 0.0, 0.0]),
            0.1,
            NavContext::orbit(4.0),
        ),
        (
            "orbit_zoom_in_push_away",
            axes([0.0, 0.0, -1.0, 0.0, 0.0, 0.0]),
            0.1,
            NavContext::orbit(10.0),
        ),
        (
            "orbit_zoom_out_pull",
            axes([0.0, 0.0, 0.6, 0.0, 0.0, 0.0]),
            frame,
            NavContext::orbit(10.0),
        ),
        (
            "orbit_yaw_ccw_full",
            axes([0.0, 0.0, 0.0, 0.0, 1.0, 0.0]),
            0.1,
            NavContext::orbit(10.0),
        ),
        (
            "orbit_pitch_half",
            axes([0.0, 0.0, 0.0, half, 0.0, 0.0]),
            0.1,
            NavContext::orbit(10.0),
        ),
        (
            "orbit_mixed",
            mixed,
            frame,
            NavContext::orbit(7.5).with_pitch(0.3),
        ),
        // Horizon lock
        (
            "orbit_roll_ignored_horizon_lock_on",
            axes([0.0, 0.0, 0.0, 0.0, 0.0, 1.0]),
            0.1,
            NavContext::orbit(10.0),
        ),
        (
            "orbit_roll_horizon_lock_off",
            axes([0.0, 0.0, 0.0, 0.0, 0.0, 1.0]),
            0.1,
            unlocked(NavContext::orbit(10.0)),
        ),
        (
            "orbit_mixed_horizon_lock_off",
            mixed,
            frame,
            unlocked(NavContext::orbit(7.5).with_pitch(0.3)),
        ),
        // Pitch clamp (horizon lock on)
        (
            "orbit_pitch_clamp_up_near_limit",
            axes([0.0, 0.0, 0.0, 1.0, 0.0, 0.0]),
            0.1,
            NavContext::orbit(5.0).with_pitch(limit - 0.01),
        ),
        (
            "orbit_pitch_clamp_up_at_limit",
            axes([0.0, 0.0, 0.0, 1.0, 0.0, 0.0]),
            0.1,
            NavContext::orbit(5.0).with_pitch(limit),
        ),
        (
            "orbit_pitch_back_inside_from_limit",
            axes([0.0, 0.0, 0.0, -1.0, 0.0, 0.0]),
            0.1,
            NavContext::orbit(5.0).with_pitch(limit),
        ),
        (
            "orbit_pitch_clamp_down_near_limit",
            axes([0.0, 0.0, 0.0, -1.0, 0.0, 0.0]),
            0.1,
            NavContext::orbit(5.0).with_pitch(-limit + 0.02),
        ),
        (
            "orbit_pitch_no_clamp_horizon_lock_off",
            axes([0.0, 0.0, 0.0, 1.0, 0.0, 0.0]),
            0.1,
            unlocked(NavContext::orbit(5.0).with_pitch(limit)),
        ),
        // Dominant axis
        (
            "orbit_dominant_on",
            mixed,
            frame,
            NavContext::orbit(7.5).with_toggles(&dominant),
        ),
        (
            "orbit_dominant_on_translation_wins",
            axes([0.9, 0.2, -0.3, 0.1, 0.5, 0.0]),
            frame,
            NavContext::orbit(3.0).with_toggles(&dominant),
        ),
        // User overrides
        (
            "orbit_invert_x_and_ry",
            mixed,
            frame,
            NavContext::orbit(7.5).with_config(&config(1.0, [true, false, false, false, true, false])),
        ),
        (
            "orbit_invert_all",
            mixed,
            frame,
            NavContext::orbit(7.5).with_config(&config(1.0, [true; 6])),
        ),
        (
            "orbit_sensitivity_min_0_1",
            mixed,
            frame,
            NavContext::orbit(7.5).with_config(&config(0.1, [false; 6])),
        ),
        (
            "orbit_sensitivity_max_4_0",
            mixed,
            frame,
            NavContext::orbit(7.5).with_config(&config(4.0, [false; 6])),
        ),
        (
            "orbit_sensitivity_above_max_clamped",
            mixed,
            frame,
            NavContext {
                sensitivity: 10.0,
                ..NavContext::orbit(7.5)
            },
        ),
        // dt
        ("orbit_dt_above_clamp_0_25", mixed, 0.25, NavContext::orbit(7.5)),
        ("orbit_dt_144_fps", mixed, 1.0 / 144.0, NavContext::orbit(7.5)),
        // Dead-zone edges
        (
            "dead_zone_just_below_0_0399",
            axes([0.0399, -0.0399, 0.0399, 0.0399, -0.0399, 0.0399]),
            0.1,
            NavContext::orbit(100.0),
        ),
        (
            "dead_zone_at_edge_0_04",
            axes([0.04, 0.0, 0.0, 0.0, -0.04, 0.0]),
            0.1,
            NavContext::orbit(100.0),
        ),
        (
            "dead_zone_just_above_0_0401",
            axes([0.0401, -0.0401, 0.0401, 0.0401, -0.0401, 0.0]),
            0.1,
            NavContext::orbit(100.0),
        ),
        (
            "dead_zone_above_0_05",
            axes([0.05, 0.0, 0.0, 0.0, 0.05, 0.0]),
            0.1,
            NavContext::orbit(100.0),
        ),
        // Fly
        (
            "fly_translate_all",
            axes([1.0, half, -1.0, 0.0, 0.0, 0.0]),
            0.1,
            NavContext::fly(20.0),
        ),
        (
            "fly_turn_horizon_lock_on",
            axes([0.0, 0.0, 0.0, 0.6, -1.0, 1.0]),
            frame,
            NavContext::fly(20.0).with_pitch(0.2),
        ),
        (
            "fly_roll_horizon_lock_off",
            axes([0.0, 0.0, 0.0, 0.6, -1.0, 1.0]),
            frame,
            unlocked(NavContext::fly(20.0).with_pitch(0.2)),
        ),
        (
            "fly_pitch_clamp",
            axes([0.0, 0.0, 0.0, 1.0, 0.0, 0.0]),
            0.1,
            NavContext::fly(20.0).with_pitch(limit - 0.05),
        ),
        (
            "fly_mixed_sensitivity_2",
            mixed,
            frame,
            NavContext::fly(3.0).with_config(&config(2.0, [false; 6])),
        ),
        // Turntable (the Orbit delta, applied to the object)
        (
            "turntable_mixed",
            mixed,
            frame,
            NavContext::turntable(7.5).with_pitch(0.3),
        ),
        (
            "turntable_pitch_clamp",
            axes([0.0, 0.0, 0.0, 1.0, 0.5, 0.0]),
            0.1,
            NavContext::turntable(2.0).with_pitch(limit - 0.01),
        ),
        // View2D
        (
            "view2d_pan_and_zoom_in",
            axes([1.0, half, -1.0, 0.0, 0.0, 0.0]),
            0.1,
            NavContext::view2d(200.0),
        ),
        (
            "view2d_rotation_unused",
            axes([0.0, 0.0, 0.0, 1.0, 1.0, 1.0]),
            0.1,
            NavContext::view2d(200.0),
        ),
        ("view2d_mixed", mixed, frame, NavContext::view2d(1080.0)),
        // Timeline
        (
            "timeline_scrub_forward_full",
            axes([0.0, 0.0, 0.0, 0.0, 1.0, 0.0]),
            0.1,
            NavContext::timeline(30.0),
        ),
        (
            "timeline_scrub_back_half_zoom_out",
            axes([0.0, 0.0, 0.6, 0.0, -half, 0.0]),
            0.1,
            NavContext::timeline(30.0),
        ),
        (
            "timeline_unit_span_fraction",
            mixed,
            frame,
            NavContext::timeline(1.0),
        ),
        // Step (threshold on the shaped |rz|: 0.5 ⇔ raw about 0.7188)
        (
            "step_below_threshold_0_7",
            axes([0.0, 0.0, 0.0, 0.0, 0.0, -0.7]),
            frame,
            NavContext::list(0.0),
        ),
        (
            "step_crossing_right_0_72",
            axes([0.0, 0.0, 0.0, 0.0, 0.0, -0.72]),
            frame,
            NavContext::list(0.0),
        ),
        (
            "step_crossing_left_full",
            axes([0.0, 0.0, 0.0, 0.0, 0.0, 1.0]),
            frame,
            NavContext::list(0.0),
        ),
        (
            "step_held_after_crossing",
            axes([0.0, 0.0, 0.0, 0.0, 0.0, -1.0]),
            frame,
            NavContext::list(f32::MIN_POSITIVE),
        ),
        (
            "step_held_between_repeats",
            axes([0.0, 0.0, 0.0, 0.0, 0.0, -1.0]),
            frame,
            NavContext::list(0.2),
        ),
        (
            "step_repeat_at_0_35",
            axes([0.0, 0.0, 0.0, 0.0, 0.0, -1.0]),
            frame,
            NavContext::list(0.34),
        ),
        (
            "step_second_repeat_at_0_70",
            axes([0.0, 0.0, 0.0, 0.0, 0.0, 1.0]),
            frame,
            NavContext::list(0.69),
        ),
        (
            "step_released",
            SpaceMouseState::default(),
            frame,
            NavContext::list(0.5),
        ),
        (
            "step_ignores_other_axes",
            axes([1.0, 1.0, 1.0, 1.0, 1.0, 0.0]),
            frame,
            NavContext::list(0.0),
        ),
    ]
}

fn computed() -> Golden {
    let cases = inputs()
        .into_iter()
        .map(|(name, state, dt, context)| Case {
            name: name.to_string(),
            state,
            dt,
            context,
            delta: nav::step(&state, dt, context),
        })
        .collect();
    Golden {
        format: 1,
        spec: "docs/specs/29-spacemouse.md".to_string(),
        comparison: "delta == nav::step(state, dt, context); f32 fields within 1e-6 relative (1e-12 absolute), step exact".to_string(),
        cases,
    }
}

fn same(a: f32, b: f32) -> bool {
    (a - b).abs() <= 1e-6 * a.abs().max(b.abs()) + 1e-12
}

fn same_delta(a: &NavDelta, b: &NavDelta) -> bool {
    let fields = |d: &NavDelta| {
        [
            d.pan.x,
            d.pan.y,
            d.pan.z,
            d.orbit.x,
            d.orbit.y,
            d.roll,
            d.zoom,
            d.translate.x,
            d.translate.y,
            d.translate.z,
            d.scrub,
            d.step_held,
        ]
    };
    a.step == b.step && fields(a).iter().zip(fields(b)).all(|(x, y)| same(*x, y))
}

#[test]
fn nav_step_matches_the_golden_fixture() {
    let fresh = computed();
    assert!(fresh.cases.len() >= 30, "about 30 cases");
    let mut names: Vec<&str> = fresh.cases.iter().map(|c| c.name.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), fresh.cases.len(), "case names are unique");

    let path = data_path();
    if regia_core::brand::env_var("REGIA_WRITE_NAV_GOLDEN").is_ok_and(|v| v == "1") {
        let mut text = serde_json::to_string_pretty(&fresh).expect("serialise the fixture");
        text.push('\n');
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).expect("create tests/data");
        }
        std::fs::write(&path, text).expect("write the fixture");
        return;
    }

    let text = std::fs::read_to_string(&path)
        .expect("tests/data/spacemouse_nav_golden.json (REGIA_WRITE_NAV_GOLDEN=1 writes it)");
    let golden: Golden = serde_json::from_str(&text).expect("parse the fixture");
    assert_eq!(
        golden.cases.len(),
        fresh.cases.len(),
        "case count changed: rewrite the fixture deliberately"
    );
    for (stored, now) in golden.cases.iter().zip(&fresh.cases) {
        assert_eq!(stored.name, now.name, "case order changed");
        // The stored inputs, through the real function, give the stored delta.
        let replayed = nav::step(&stored.state, stored.dt, stored.context);
        assert!(
            same_delta(&replayed, &stored.delta),
            "{}: nav::step now gives {replayed:?}, fixture says {:?}",
            stored.name,
            stored.delta
        );
        assert!(
            same_delta(&now.delta, &stored.delta),
            "{}: inputs drifted",
            stored.name
        );
    }
}
