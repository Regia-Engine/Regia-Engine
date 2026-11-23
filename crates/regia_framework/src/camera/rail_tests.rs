// SPDX-License-Identifier: MPL-2.0
//! Tests of the `Rail` and `Crane` nodes (ADR-3231): evaluation against a mock world with a
//! spline, the asset round trip and the hostile-asset checks. Fixed steps only; no clock.

use std::collections::BTreeMap;
use std::sync::Arc;

use regia_core::math::{Quat, Vec3};
use regia_scene::spline::{Spline, SplineCurve};

use super::asset::{CameraAsset, Node, TargetRef, VarValue};
use super::nodes::{CameraPose, CameraWorld, EvalCtx, InputBank, RigScratch, VarTable, eval_rig};

const DT: f32 = 1.0 / 60.0;

/// A world with one named rail and one target.
struct RailWorld {
    curve: Option<Arc<SplineCurve>>,
    target: Option<Vec3>,
}

impl CameraWorld for RailWorld {
    fn target(&self, target: &TargetRef) -> Option<(Vec3, Quat)> {
        match target {
            TargetRef::Point(p) => Some((Vec3::from(*p), Quat::IDENTITY)),
            _ => self.target.map(|p| (p, Quat::IDENTITY)),
        }
    }

    fn cast_sphere(&self, _: Vec3, _: Vec3, _: f32, _: f32, _: &[String]) -> Option<f32> {
        None
    }

    fn rail(&self, name: &str) -> Option<Arc<SplineCurve>> {
        (name == "track").then(|| self.curve.clone()).flatten()
    }
}

fn bend() -> Spline {
    Spline::through([
        Vec3::new(0.0, 1.0, 0.0),
        Vec3::new(10.0, 1.0, 0.0),
        Vec3::new(14.0, 1.0, -8.0),
        Vec3::new(14.0, 1.0, -30.0),
    ])
}

fn world(spline: &Spline) -> RailWorld {
    RailWorld {
        curve: Some(Arc::new(SplineCurve::new(spline, None))),
        target: None,
    }
}

fn asset(root: &str, variables: &str) -> CameraAsset {
    let text = format!(
        r#"(format_version: 2, name: "rc", variables: [{variables}],
            rigs: [(name: "r", root: [{root}])], director: Fixed("r"))"#
    );
    CameraAsset::from_ron(&text).expect("test asset parses")
}

/// One evaluation; returns the pose.
fn step(
    asset: &CameraAsset,
    world: &RailWorld,
    vars: &VarTable,
    scratch: &mut RigScratch,
    elapsed: f32,
) -> CameraPose {
    let overrides = BTreeMap::new();
    let inputs = InputBank::default();
    let mut pose = CameraPose::default();
    let ctx = EvalCtx {
        dt: DT,
        elapsed,
        seed: 1,
        asset,
        rig: &asset.rigs[0],
        overrides: &overrides,
        vars,
        inputs: &inputs,
        world,
        prev: pose,
        aim_override: None,
    };
    eval_rig(&ctx, &mut pose, scratch);
    pose
}

const RAIL_VAR: &str = r#"(name: "rail_position", default: Float(0.0))"#;

#[test]
fn a_value_drive_puts_the_camera_at_exact_distances_with_the_frame_orientation() {
    let spline = bend();
    let w = world(&spline);
    let a = asset(
        r#"Rail(spline: "track", drive: Value(Var("rail_position")))"#,
        RAIL_VAR,
    );
    let mut vars = VarTable::from_asset(&a);
    let mut scratch = RigScratch::default();
    let curve = w.curve.clone().expect("curve");
    for d in [0.0, 3.5, 12.0, 20.25, curve.length()] {
        assert!(vars.set("rail_position", VarValue::Float(d)));
        let pose = step(&a, &w, &vars, &mut scratch, 0.0);
        let sample = curve.at_distance(d);
        assert!((pose.position - sample.position).length() < 1.0e-5, "at {d}");
        let forward = pose.forward();
        assert!(
            forward.dot(sample.tangent) > 0.999,
            "faces along the rail at {d}: {forward}"
        );
    }
    assert_eq!(scratch.skipped, 0);
    let report = &scratch.rails[0];
    assert!(report.at_end && !report.clamped);
}

#[test]
fn a_time_drive_moves_at_constant_speed_around_a_bend() {
    let spline = bend();
    let w = world(&spline);
    let a = asset(
        r#"Rail(spline: "track", drive: Time(speed: 4.0, start: 1.0))"#,
        "",
    );
    let vars = VarTable::from_asset(&a);
    let mut scratch = RigScratch::default();
    let mut previous = step(&a, &w, &vars, &mut scratch, 0.0).position;
    let mut worst = 0.0f32;
    let length = w.curve.as_ref().map_or(0.0, |c| c.length());
    for i in 1..300 {
        let elapsed = i as f32 * DT;
        if 1.0 + 4.0 * elapsed >= length {
            break;
        }
        let p = step(&a, &w, &vars, &mut scratch, elapsed).position;
        worst = worst.max((p.distance(previous) - 4.0 * DT).abs());
        previous = p;
    }
    // Chord vs arc over a 6.7 cm step: bounded by the sagitta, far below a millimeter here.
    assert!(worst < 1.0e-3, "step error {worst} m");
}

#[test]
fn an_open_rail_clamps_and_a_closed_rail_loops() {
    let open = bend();
    let w = world(&open);
    let a = asset(
        r#"Rail(spline: "track", drive: Value(Var("rail_position")))"#,
        RAIL_VAR,
    );
    let mut vars = VarTable::from_asset(&a);
    let mut scratch = RigScratch::default();
    let length = w.curve.as_ref().map_or(0.0, |c| c.length());
    assert!(vars.set("rail_position", VarValue::Float(length + 50.0)));
    let end = step(&a, &w, &vars, &mut scratch, 0.0).position;
    let report = scratch.rails[0].clone();
    assert!(report.clamped && report.at_end);
    assert!((end - Vec3::new(14.0, 1.0, -30.0)).length() < 1.0e-3);
    assert!(vars.set("rail_position", VarValue::Float(-5.0)));
    let start = step(&a, &w, &vars, &mut scratch, 0.0).position;
    assert!((start - Vec3::new(0.0, 1.0, 0.0)).length() < 1.0e-4);

    let ring = Spline::through([
        Vec3::new(0.0, 0.0, 0.0),
        Vec3::new(8.0, 0.0, 0.0),
        Vec3::new(8.0, 0.0, 8.0),
        Vec3::new(0.0, 0.0, 8.0),
    ])
    .closed();
    let w = world(&ring);
    let length = w.curve.as_ref().map_or(0.0, |c| c.length());
    assert!(vars.set("rail_position", VarValue::Float(length + 3.0)));
    let wrapped = step(&a, &w, &vars, &mut scratch, 0.0).position;
    assert!(vars.set("rail_position", VarValue::Float(3.0)));
    let direct = step(&a, &w, &vars, &mut scratch, 0.0).position;
    assert!((wrapped - direct).length() < 1.0e-3, "{wrapped} vs {direct}");
    assert!(scratch.rails[0].closed && !scratch.rails[0].clamped);
}

#[test]
fn follow_rides_the_closest_point_with_the_lead_and_never_leaves_the_rail() {
    let spline = bend();
    let mut w = world(&spline);
    let curve = w.curve.clone().expect("curve");
    let a = asset(
        r#"Rail(spline: "track", drive: Follow(target: DriveTarget, lead: 2.5, damping: 0.0))"#,
        "",
    );
    let vars = VarTable::from_asset(&a);
    let mut scratch = RigScratch::default();
    // The target stands off to the side of the rail.
    let on_rail = curve.at_distance(9.0);
    w.target = Some(on_rail.position + on_rail.right * 6.0 + Vec3::Y * 2.0);
    let pose = step(&a, &w, &vars, &mut scratch, 0.0);
    let expected = curve.at_distance(11.5).position;
    assert!(
        (pose.position - expected).length() < 0.05,
        "{} vs {expected}",
        pose.position
    );
    assert!((scratch.rails[0].goal_m.unwrap_or(-1.0) - 11.5).abs() < 0.05);
    // On the rail: the camera is a rail point, not the target.
    assert!(curve.closest(pose.position).distance_m < 1.0e-3);
}

#[test]
fn damped_follow_converges_to_the_end_without_passing_it() {
    let spline = bend();
    let mut w = world(&spline);
    let curve = w.curve.clone().expect("curve");
    let a = asset(
        r#"Rail(spline: "track", drive: Follow(target: DriveTarget, lead: 5.0, damping: 3.0))"#,
        "",
    );
    let vars = VarTable::from_asset(&a);
    let mut scratch = RigScratch::default();
    w.target = Some(curve.at_distance(1.0).position);
    step(&a, &w, &vars, &mut scratch, 0.0);
    // The target jumps past the end of the rail.
    w.target = Some(Vec3::new(14.0, 1.0, -60.0));
    let length = curve.length();
    let mut last = scratch.rails[0].distance_m;
    for i in 1..900 {
        step(&a, &w, &vars, &mut scratch, i as f32 * DT);
        let d = scratch.rails[0].distance_m;
        assert!(
            d >= last && d <= length,
            "step {i}: {d} after {last} (length {length})"
        );
        last = d;
    }
    assert!((length - last).abs() < 1.0e-2, "converged to {last} of {length}");
}

#[test]
fn a_zero_length_or_missing_rail_skips_the_node_and_leaves_the_pose() {
    let a = asset(r#"Rail(spline: "track", drive: Time(speed: 1.0))"#, "");
    let vars = VarTable::from_asset(&a);
    let mut scratch = RigScratch::default();
    let point = Spline::through([Vec3::new(3.0, 3.0, 3.0), Vec3::new(3.0, 3.0, 3.0)]);
    let w = world(&point);
    // The world hands out whatever curve it has; the node refuses one with no length.
    let pose = step(&a, &w, &vars, &mut scratch, 1.0);
    assert_eq!(scratch.skipped, 1);
    assert_eq!(pose.position, Vec3::ZERO);
    let none = RailWorld {
        curve: None,
        target: None,
    };
    step(&a, &none, &vars, &mut scratch, 1.0);
    assert_eq!(scratch.skipped, 2);
    assert!(scratch.rails.is_empty());
}

#[test]
fn a_skipped_crane_leaves_no_report() {
    // The crane evaluates (and pushes its report) before the pose is found non-finite; a
    // skipped node must roll the report back with the pose, or `camera.crane_state` would claim
    // the node ran with a NaN tip.
    let a = asset(
        r#"Crane(base: Ground((1.0, 0.0, 0.0)), yaw: Literal(0.0), pitch: Literal(0.0),
            length: Literal(NaN))"#,
        "",
    );
    let vars = VarTable::from_asset(&a);
    let mut scratch = RigScratch::default();
    let w = RailWorld {
        curve: None,
        target: None,
    };
    let pose = step(&a, &w, &vars, &mut scratch, 0.0);
    assert_eq!(scratch.skipped, 1);
    assert!(scratch.cranes.is_empty(), "the report is rolled back");
    assert_eq!(pose.position, Vec3::ZERO, "the pose is rolled back");
}

fn crane_asset(limits: &str) -> CameraAsset {
    asset(
        &format!(
            r#"Crane(base: Ground((1.0, 0.0, 0.0)), yaw: Var("crane_yaw"), pitch: Var("crane_pitch"),
                length: Var("crane_length"), mount_yaw: Var("crane_mount_yaw"),
                limits: ({limits}))"#
        ),
        r#"(name: "crane_yaw", default: Float(0.0)), (name: "crane_pitch", default: Float(0.0)),
           (name: "crane_length", default: Float(4.0)), (name: "crane_mount_yaw", default: Float(0.0))"#,
    )
}

#[test]
fn the_crane_places_the_tip_and_clamps_each_axis() {
    let a = crane_asset(
        "yaw_deg: Some((-45.0, 45.0)), pitch_deg: Some((-10.0, 30.0)), length: Some((2.0, 6.0)), mount_yaw_deg: Some((-5.0, 5.0))",
    );
    let w = RailWorld {
        curve: None,
        target: None,
    };
    let mut vars = VarTable::from_asset(&a);
    let mut scratch = RigScratch::default();
    let pose = step(&a, &w, &vars, &mut scratch, 0.0);
    assert!(
        (pose.position - Vec3::new(1.0, 0.0, -4.0)).length() < 1.0e-4,
        "{}",
        pose.position
    );
    for (name, value) in [
        ("crane_yaw", 100.0),
        ("crane_pitch", -50.0),
        ("crane_length", 9.0),
        ("crane_mount_yaw", 40.0),
    ] {
        assert!(vars.set(name, VarValue::Float(value)));
    }
    let pose = step(&a, &w, &vars, &mut scratch, 0.0);
    let report = &scratch.cranes[0];
    assert_eq!(report.axes.to_array(), [45.0, -10.0, 6.0, 5.0, 0.0]);
    assert_eq!(report.clamped, [true, true, true, true, false]);
    assert_eq!(report.requested.length, 9.0);
    assert!(pose.is_finite());
    assert!((pose.position - report.tip).length() < 1.0e-6);
}

#[test]
fn a_crane_on_a_rail_follows_the_track_and_can_turn_with_it() {
    let spline = bend();
    let w = world(&spline);
    let a = asset(
        r#"Crane(base: Rail(spline: "track", position: Literal(10.0), heading: true), length: Literal(2.0))"#,
        "",
    );
    let vars = VarTable::from_asset(&a);
    let mut scratch = RigScratch::default();
    let pose = step(&a, &w, &vars, &mut scratch, 0.0);
    let report = &scratch.cranes[0];
    let curve = w.curve.clone().expect("curve");
    let at = curve.at_distance(10.0);
    assert!((report.base - at.position).length() < 1.0e-5);
    // Yaw 0 on a rail with heading on: the arm points along the track.
    let along = (pose.position - at.position).normalize();
    let flat = Vec3::new(at.tangent.x, 0.0, at.tangent.z).normalize();
    assert!(along.dot(flat) > 0.999, "{along} vs {flat}");
}

#[test]
fn rail_and_crane_nodes_round_trip_byte_identical() {
    let text = r#"(format_version: 2, name: "rc",
        variables: [(name: "rail_position", default: Float(0.0)), (name: "crane_yaw", default: Float(0.0))],
        rigs: [
          (name: "dolly", root: [
            Rail(spline: "track", drive: Follow(target: DriveTarget, lead: 1.5, damping: 4.0),
                 orient: LookAt(Entity("actor")), offset: (0.0, 0.5, 0.0)),
          ]),
          (name: "timed", root: [Rail(spline: "track", drive: Time(speed: 2.0, start: 0.0))]),
          (name: "keyed", root: [Rail(spline: "track", drive: Value(Var("rail_position")), orient: Free)]),
          (name: "crane", root: [
            Crane(base: Rail(spline: "track", position: Var("rail_position"), heading: true),
                  yaw: Var("crane_yaw"), pitch: Literal(10.0), length: Literal(5.0),
                  mount_yaw: Literal(180.0), mount_pitch: Literal(-5.0),
                  limits: (yaw_deg: Some((-90.0, 90.0)), length: Some((1.0, 8.0)))),
          ]),
        ],
        director: Fixed("dolly"))"#;
    let parsed = CameraAsset::from_ron(text).expect("parses");
    assert!(parsed.validate(&|_| true).iter().all(|i| !i.is_error()));
    let first = parsed.to_ron().expect("writes");
    let again = CameraAsset::from_ron(&first).expect("re-parses");
    assert_eq!(again, parsed);
    assert_eq!(
        again.to_ron().expect("writes again"),
        first,
        "re-saving is byte-identical"
    );
    assert!(matches!(parsed.rigs[0].root[0], Node::Rail { .. }));
    assert!(matches!(parsed.rigs[3].root[0], Node::Crane { .. }));
    assert_eq!(parsed.rigs[3].root[0].kind(), "crane");
}

#[test]
fn hostile_values_are_refused_by_validation() {
    let bad = |root: &str| -> Vec<String> {
        let a = asset(root, "");
        a.validate(&|_| true)
            .into_iter()
            .filter(|i| i.is_error())
            .map(|i| i.code.to_owned())
            .collect()
    };
    for root in [
        r#"Rail(spline: "", drive: Time(speed: 1.0))"#,
        r#"Rail(spline: "t", drive: Time(speed: NaN))"#,
        r#"Rail(spline: "t", drive: Value(Literal(NaN)))"#,
        r#"Rail(spline: "t", drive: Follow(target: DriveTarget, lead: inf, damping: 1.0))"#,
        r#"Rail(spline: "t", drive: Follow(target: DriveTarget, damping: -1.0))"#,
        r#"Rail(spline: "t", drive: Time(speed: 1.0), offset: (NaN, 0.0, 0.0))"#,
        r#"Crane(base: Ground((NaN, 0.0, 0.0)), length: Literal(1.0))"#,
        r#"Crane(base: Ground((0.0, 0.0, 0.0)), length: Literal(-1.0))"#,
        r#"Crane(base: Ground((0.0, 0.0, 0.0)), length: Literal(1.0), limits: (yaw_deg: Some((5.0, -5.0))))"#,
        r#"Crane(base: Ground((0.0, 0.0, 0.0)), length: Literal(1.0), limits: (pitch_deg: Some((NaN, 5.0))))"#,
        r#"Crane(base: Rail(spline: "", position: Literal(0.0)), length: Literal(1.0))"#,
    ] {
        assert!(!bad(root).is_empty(), "accepted: {root}");
    }
}

#[test]
fn a_non_finite_follow_target_keeps_the_distance_already_stored() {
    let spline = bend();
    let mut w = world(&spline);
    let curve = w.curve.clone().expect("curve");
    let a = asset(
        r#"Rail(spline: "track", drive: Follow(target: DriveTarget, lead: 0.0, damping: 3.0))"#,
        "",
    );
    let vars = VarTable::from_asset(&a);
    let mut scratch = RigScratch::default();
    w.target = Some(curve.at_distance(9.0).position);
    step(&a, &w, &vars, &mut scratch, 0.0);
    let held = scratch.states[0].f[0];
    assert!(held.is_finite() && held > 1.0, "settled on the rail: {held}");
    w.target = Some(Vec3::NAN);
    step(&a, &w, &vars, &mut scratch, DT);
    assert_eq!(
        scratch.states[0].f[0].to_bits(),
        held.to_bits(),
        "a NaN target must not commit a new distance"
    );
    assert!(
        scratch.skipped >= 1,
        "the node is skipped, not applied at the origin"
    );
}

/// Spec 35 § 17 `orient`: `LookAt` turns toward its target and sets the aim, falling back to the
/// rail frame while the target cannot be resolved; `Free` keeps the rotation earlier nodes set
/// (lane spec-15-35, ADR-5778).
#[test]
fn rail_orient_look_at_and_free_behave_as_specified() {
    let spline = bend();
    let w = world(&spline);
    let curve = w.curve.clone().expect("curve");
    let d = 3.5;
    let sample = curve.at_distance(d);
    let target = Vec3::new(5.0, 1.0, 10.0);
    let look = asset(
        r#"Rail(spline: "track", drive: Value(Var("rail_position")), orient: LookAt(Point((5.0, 1.0, 10.0))))"#,
        RAIL_VAR,
    );
    let mut vars = VarTable::from_asset(&look);
    assert!(vars.set("rail_position", VarValue::Float(d)));
    let mut scratch = RigScratch::default();
    let pose = step(&look, &w, &vars, &mut scratch, 0.0);
    assert!((pose.position - sample.position).length() < 1.0e-5);
    let wanted = (target - pose.position).normalize();
    assert!(
        pose.forward().dot(wanted) > 0.9999,
        "looks at the target: {}",
        pose.forward()
    );
    assert_eq!(pose.aim, Some(target), "the target becomes the aim");
    // An unresolvable target (no drive target in this world) falls back to the rail frame.
    let lost = asset(
        r#"Rail(spline: "track", drive: Value(Var("rail_position")), orient: LookAt(DriveTarget))"#,
        RAIL_VAR,
    );
    let pose = step(&lost, &w, &vars, &mut RigScratch::default(), 0.0);
    assert!(
        pose.forward().dot(sample.tangent) > 0.999,
        "frame fallback: {}",
        pose.forward()
    );
    // Free keeps the 30 degree yaw an earlier node set, not the rail tangent.
    let free = asset(
        r#"Offset(translation: (0.0, 0.0, 0.0), rotation_deg: (0.0, 30.0, 0.0), space: World),
           Rail(spline: "track", drive: Value(Var("rail_position")), orient: Free)"#,
        RAIL_VAR,
    );
    let pose = step(&free, &w, &vars, &mut RigScratch::default(), 0.0);
    assert!((pose.position - sample.position).length() < 1.0e-5);
    assert!(
        (pose.yaw - 30.0f32.to_radians()).abs() < 1.0e-5,
        "yaw {}",
        pose.yaw
    );
}
