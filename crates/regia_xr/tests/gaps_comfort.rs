// SPDX-License-Identifier: MPL-2.0
//! Comfort limits (spec 58 § 2.8) the in-crate tests leave open: snap-turn thresholds, cooldown
//! lengths and clamps, smooth turning, `rig_turn` / `rig_move` arithmetic, vignette arithmetic,
//! and every `validate` boundary. Pure CPU, hand-computed expectations.

use regia_core::glam::{Quat, Vec3};
use regia_platform::xr::{Hand, Pose, XrError};
use regia_xr::comfort::{VIGNETTE_FULL_ANGULAR_RPS, VIGNETTE_FULL_LINEAR_MPS};
use regia_xr::space::yaw_of;
use regia_xr::{ComfortSettings, DominantHand, SnapTurn, Vignette, rig_move, rig_turn};

const DT: f32 = 1.0 / 60.0;

fn settings() -> ComfortSettings {
    ComfortSettings::default()
}

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-5
}

/// Catches a drifting default: the numbers are ADR-1585's starting points.
#[test]
fn defaults_are_the_documented_starting_points() {
    let s = settings();
    assert_eq!(s.snap_turn_deg, 30.0);
    assert_eq!(s.snap_threshold, 0.6);
    assert_eq!(s.snap_rearm, 0.3);
    assert_eq!(s.snap_cooldown_frames, 8);
    assert_eq!(s.smooth_turn_deg_per_s, 90.0);
    assert_eq!(s.vignette_strength, 0.5);
    assert_eq!((s.vignette_inner, s.vignette_outer), (0.6, 1.0));
    assert_eq!(s.vignette_ease_frames, 6);
    assert_eq!(s.eye_height_m, 1.65);
    assert_eq!(s.dominant_hand, DominantHand::Right);
    assert!(!s.seated);
    assert_eq!(VIGNETTE_FULL_LINEAR_MPS, 3.0);
    assert_eq!(VIGNETTE_FULL_ANGULAR_RPS, 1.5);
}

/// Catches `>` instead of `>=` at the threshold, or a sign flip for the left side.
#[test]
fn the_snap_threshold_is_inclusive_and_the_sign_follows_the_stick() {
    let s = settings();
    let mut snap = SnapTurn::default();
    assert_eq!(snap.step(&s, 0.59, DT), 0.0, "just under the threshold");
    assert_eq!(snap.snaps(), 0);
    let at = snap.step(&s, 0.6, DT);
    assert!(
        close(at, -30f32.to_radians()),
        "exactly at the threshold fires: {at}"
    );
    let mut snap = SnapTurn::default();
    let left = snap.step(&s, -0.6, DT);
    assert!(
        close(left, 30f32.to_radians()),
        "left stick turns left (+yaw): {left}"
    );
}

/// Catches a snap angle hard-coded to 30 degrees instead of read from the settings.
#[test]
fn the_snap_angle_comes_from_the_settings() {
    for deg in [15.0_f32, 45.0, 90.0, 180.0] {
        let s = ComfortSettings {
            snap_turn_deg: deg,
            ..settings()
        };
        let mut snap = SnapTurn::default();
        let yaw = snap.step(&s, 1.0, DT);
        assert!(close(yaw, -deg.to_radians()), "{deg}: {yaw}");
    }
}

/// Catches a missing clamp: an over-range or non-finite stick must neither change the angle nor
/// fire a turn.
#[test]
fn out_of_range_and_non_finite_sticks_are_sanitised() {
    let s = settings();
    let mut snap = SnapTurn::default();
    let big = snap.step(&s, 5.0, DT);
    assert!(
        close(big, -30f32.to_radians()),
        "5.0 clamps to full deflection: {big}"
    );
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let mut snap = SnapTurn::default();
        assert_eq!(snap.step(&s, bad, DT), 0.0, "{bad}");
        assert_eq!(snap.snaps(), 0);
        // And the trigger is still armed afterwards.
        assert_ne!(snap.step(&s, 1.0, DT), 0.0, "{bad} did not disarm the snap");
    }
}

/// Catches a cooldown that ignores `snap_cooldown_frames` (the in-crate test only pins 8).
#[test]
fn the_cooldown_length_follows_the_setting() {
    // Cooldown c, set on the snap at call 0; call k leaves c-k; a held stick fires at call c.
    for c in [0_u32, 1, 3, 10] {
        let s = ComfortSettings {
            snap_cooldown_frames: c,
            ..settings()
        };
        let mut snap = SnapTurn::default();
        assert_ne!(snap.step(&s, 1.0, 0.0), 0.0);
        assert_eq!(snap.step(&s, 0.0, 0.0), 0.0, "release re-arms (call 1)");
        let mut fired_at = None;
        for call in 2..40_u32 {
            if snap.step(&s, 1.0, 0.0) != 0.0 {
                fired_at = Some(call);
                break;
            }
        }
        // A zero or one step cooldown is spent by the release; otherwise the call index is `c`.
        assert_eq!(fired_at, Some(c.max(2)), "cooldown {c}");
    }
}

/// Catches `<=` instead of `<` on the re-arm level: a stick resting exactly on the re-arm value
/// has not yet released.
#[test]
fn the_rearm_level_is_exclusive() {
    let s = settings();
    let mut snap = SnapTurn::default();
    assert_ne!(snap.step(&s, 1.0, DT), 0.0);
    for _ in 0..30 {
        snap.step(&s, 0.3, DT);
    }
    assert_eq!(snap.step(&s, 1.0, DT), 0.0, "0.3 is not below the re-arm level");
    snap.step(&s, 0.29, DT);
    assert_ne!(snap.step(&s, 1.0, DT), 0.0, "0.29 re-arms");
    assert_eq!(snap.snaps(), 2);
}

/// Catches smooth turning that fires snaps, counts them, or ignores the step length.
#[test]
fn smooth_turning_is_rate_times_deflection_times_step_and_never_counts_snaps() {
    let s = ComfortSettings {
        snap_turn_deg: 0.0,
        ..settings()
    };
    let mut snap = SnapTurn::default();
    let one = snap.step(&s, 1.0, DT);
    assert!(close(one, -1.5f32.to_radians()), "90 deg/s * 1 * (1/60 s): {one}");
    let left_half = snap.step(&s, -0.5, 0.1);
    assert!(close(left_half, 4.5f32.to_radians()), "{left_half}");
    assert!(close(snap.step(&s, 3.0, DT), one), "over-range clamps to 1");
    assert_eq!(snap.step(&s, f32::NAN, DT), 0.0);
    assert_eq!(snap.step(&s, 1.0, 0.0), 0.0, "a zero step turns nothing");
    for _ in 0..100 {
        snap.step(&s, 1.0, DT);
    }
    assert_eq!(snap.snaps(), 0);
}

/// Catches a wrong pivot or sign in `rig_turn`: hand-computed on a rig at the origin and a head
/// one meter to the +X side, turned 90 degrees.
#[test]
fn rig_turn_pivots_about_the_head_with_a_hand_computed_result() {
    let rig = Pose::IDENTITY;
    let head = Pose::new(Vec3::new(1.0, 0.0, 0.0), Quat::IDENTITY);
    let out = rig_turn(&rig, &head, std::f32::consts::FRAC_PI_2);
    // R_y(90) maps (-1,0,0) to (0,0,1); the rig lands at head + that.
    assert!(
        (out.position - Vec3::new(1.0, 0.0, 1.0)).length() < 1e-5,
        "{:?}",
        out.position
    );
    assert!(close(yaw_of(out.orientation), std::f32::consts::FRAC_PI_2));
    // And a rotated rig: the head's world position is invariant.
    let rig = Pose::new(Vec3::ZERO, Quat::from_rotation_y(std::f32::consts::FRAC_PI_2));
    let head = Pose::new(Vec3::new(0.0, 1.7, -1.0), Quat::IDENTITY);
    let out = rig_turn(&rig, &head, std::f32::consts::FRAC_PI_2);
    assert!(
        (out.position - Vec3::new(-1.0, 0.0, -1.0)).length() < 1e-5,
        "{:?}",
        out.position
    );
    let world_head = out.compose(&head).position;
    assert!(
        (world_head - Vec3::new(-1.0, 1.7, 0.0)).length() < 1e-5,
        "{world_head:?}"
    );
}

/// Catches rotation drift: a zero yaw must leave the rig where it is, and an un-normalised rig
/// orientation must come out a unit quaternion.
#[test]
fn rig_turn_zero_is_identity_and_the_orientation_stays_unit() {
    let rig = Pose {
        position: Vec3::new(3.0, 0.5, -2.0),
        orientation: Quat::from_xyzw(0.0, 2.0 * 0.3_f32.sin() / 2.0, 0.0, 2.0 * 0.3_f32.cos() / 2.0) * 2.0,
    };
    let head = Pose::new(Vec3::new(0.2, 1.6, 0.4), Quat::IDENTITY);
    let same = rig_turn(&rig, &head, 0.0);
    assert!((same.position - rig.position).length() < 1e-5);
    let turned = rig_turn(&rig, &head, 0.9);
    assert!(
        (turned.orientation.length() - 1.0).abs() < 1e-6,
        "{}",
        turned.orientation.length()
    );
    // A head at the rig origin turns the rig in place.
    let at_origin = rig_turn(&rig, &Pose::IDENTITY, 0.9);
    assert!((at_origin.position - rig.position).length() < 1e-5);
}

/// Catches `rig_move` touching the orientation, or applying the delta in rig space instead of
/// world space.
#[test]
fn rig_move_is_a_world_space_translation() {
    let rig = Pose::new(Vec3::new(1.0, 2.0, 3.0), Quat::from_rotation_y(1.0));
    let moved = rig_move(&rig, Vec3::new(0.5, -1.0, 2.0));
    assert_eq!(moved.position, Vec3::new(1.5, 1.0, 5.0));
    assert_eq!(moved.orientation, rig.orientation);
    assert_eq!(rig_move(&rig, Vec3::ZERO), rig);
}

/// Catches the vignette's target scaling: one step from rest closes `1/ease` of the target,
/// linear and angular speeds add, and the sum is capped.
#[test]
fn the_vignette_target_adds_linear_and_angular_drive_and_caps_at_one() {
    let s = settings();
    let step = |lin: f32, ang: f32| Vignette::default().step(&s, lin, ang);
    assert_eq!(step(3.0, 0.0), 0.5_f32 / 6.0, "full linear drive");
    assert_eq!(step(0.0, 1.5), 0.5_f32 / 6.0, "full angular drive");
    assert_eq!(step(1.5, 0.75), 0.5_f32 / 6.0, "halves add to full");
    assert_eq!(step(30.0, 30.0), 0.5_f32 / 6.0, "capped at vignette_strength");
    assert_eq!(
        step(1.5, 0.0),
        0.25_f32 / 6.0,
        "half linear speed, half the target"
    );
    assert_eq!(step(-3.0, -1.5), step(3.0, 1.5), "signs do not matter");
    assert_eq!(
        step(f32::NAN, f32::INFINITY),
        0.0,
        "non-finite speeds read as still"
    );
}

/// Catches a zero ease dividing by zero, and a strength of zero not disabling the effect.
#[test]
fn the_vignette_ease_floor_and_the_zero_strength_switch() {
    for ease in [0_u32, 1] {
        let s = ComfortSettings {
            vignette_ease_frames: ease,
            ..settings()
        };
        let mut v = Vignette::default();
        assert_eq!(
            v.step(&s, 3.0, 0.0),
            0.5,
            "ease {ease} reaches the target in one step"
        );
    }
    let off = ComfortSettings {
        vignette_strength: 0.0,
        ..settings()
    };
    let mut v = Vignette::default();
    for _ in 0..20 {
        assert_eq!(v.step(&off, 10.0, 10.0), 0.0);
    }
    // Six steps at the full target close 1 - (5/6)^6 of the gap.
    let s = settings();
    let mut v = Vignette::default();
    for _ in 0..6 {
        v.step(&s, 3.0, 0.0);
    }
    let expected = 0.5 * (1.0 - (5.0_f32 / 6.0).powi(6));
    assert!(
        (v.strength() - expected).abs() < 1e-5,
        "{} vs {expected}",
        v.strength()
    );
}

/// Catches a decay that stalls above zero or overshoots below the new target.
#[test]
fn the_vignette_eases_down_without_undershooting() {
    let s = settings();
    let mut v = Vignette::default();
    for _ in 0..60 {
        v.step(&s, 3.0, 0.0);
    }
    // Lower the drive to a quarter: the strength settles at the new, lower target from above.
    let mut last = v.strength();
    for _ in 0..80 {
        let now = v.step(&s, 0.75, 0.0);
        assert!(now <= last + 1e-7 && now >= 0.125 - 1e-6, "{now} after {last}");
        last = now;
    }
    assert!((last - 0.125).abs() < 1e-4, "{last}");
}

/// Catches a range check that moved by one step: the inclusive edge passes, just past it fails,
/// and the error names the right field.
#[test]
fn validate_boundaries_and_field_names() {
    let base = settings();
    let ok = |s: ComfortSettings| s.validate().is_ok();
    let key_of = |s: ComfortSettings| match s.validate() {
        Err(XrError::InvalidValue { key, .. }) => key,
        other => panic!("expected InvalidValue, got {other:?}"),
    };
    assert!(ok(ComfortSettings {
        snap_turn_deg: 0.0,
        ..base.clone()
    }));
    assert!(ok(ComfortSettings {
        snap_turn_deg: 180.0,
        ..base.clone()
    }));
    assert_eq!(
        key_of(ComfortSettings {
            snap_turn_deg: 180.01,
            ..base.clone()
        }),
        "snap_turn_deg"
    );
    assert_eq!(
        key_of(ComfortSettings {
            snap_turn_deg: -0.01,
            ..base.clone()
        }),
        "snap_turn_deg"
    );
    assert_eq!(
        key_of(ComfortSettings {
            snap_turn_deg: f32::NAN,
            ..base.clone()
        }),
        "snap_turn_deg"
    );

    assert!(ok(ComfortSettings {
        snap_threshold: 1.0,
        ..base.clone()
    }));
    assert_eq!(
        key_of(ComfortSettings {
            snap_threshold: 1.01,
            ..base.clone()
        }),
        "snap_threshold"
    );
    assert_eq!(
        key_of(ComfortSettings {
            snap_rearm: 0.0,
            ..base.clone()
        }),
        "snap_threshold"
    );
    assert_eq!(
        key_of(ComfortSettings {
            snap_rearm: 0.6,
            ..base.clone()
        }),
        "snap_threshold",
        "rearm must be strictly below the threshold"
    );
    assert_eq!(
        key_of(ComfortSettings {
            snap_rearm: f32::NAN,
            ..base.clone()
        }),
        "snap_threshold"
    );

    assert!(ok(ComfortSettings {
        smooth_turn_deg_per_s: 720.0,
        ..base.clone()
    }));
    assert_eq!(
        key_of(ComfortSettings {
            smooth_turn_deg_per_s: 721.0,
            ..base.clone()
        }),
        "smooth_turn_deg_per_s"
    );

    assert!(ok(ComfortSettings {
        vignette_strength: 0.0,
        ..base.clone()
    }));
    assert!(ok(ComfortSettings {
        vignette_strength: 1.0,
        ..base.clone()
    }));
    assert_eq!(
        key_of(ComfortSettings {
            vignette_strength: 1.01,
            ..base.clone()
        }),
        "vignette_strength"
    );
    assert_eq!(
        key_of(ComfortSettings {
            vignette_strength: -0.1,
            ..base.clone()
        }),
        "vignette_strength"
    );

    assert!(ok(ComfortSettings {
        vignette_inner: 0.0,
        ..base.clone()
    }));
    assert_eq!(
        key_of(ComfortSettings {
            vignette_inner: -0.1,
            ..base.clone()
        }),
        "vignette_inner"
    );
    assert_eq!(
        key_of(ComfortSettings {
            vignette_inner: 1.0,
            vignette_outer: 1.0,
            ..base.clone()
        }),
        "vignette_inner",
        "inner must be strictly below outer"
    );

    assert!(ok(ComfortSettings {
        eye_height_m: 0.5,
        ..base.clone()
    }));
    assert!(ok(ComfortSettings {
        eye_height_m: 2.5,
        ..base.clone()
    }));
    assert_eq!(
        key_of(ComfortSettings {
            eye_height_m: 0.49,
            ..base.clone()
        }),
        "eye_height_m"
    );
    assert_eq!(
        key_of(ComfortSettings {
            eye_height_m: 2.51,
            ..base.clone()
        }),
        "eye_height_m"
    );
    assert_eq!(
        key_of(ComfortSettings {
            eye_height_m: f32::INFINITY,
            ..base
        }),
        "eye_height_m"
    );
}

/// Catches a half-written swap table: both hands, both dominant settings.
#[test]
fn dominant_hand_map_is_a_full_table() {
    use DominantHand::{Left, Right};
    assert_eq!(Right.map(Hand::Left), Hand::Left);
    assert_eq!(Right.map(Hand::Right), Hand::Right);
    assert_eq!(Left.map(Hand::Left), Hand::Right);
    assert_eq!(Left.map(Hand::Right), Hand::Left);
    assert_eq!(DominantHand::default(), Right);
}
