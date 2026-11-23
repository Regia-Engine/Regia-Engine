// SPDX-License-Identifier: MPL-2.0
//! `GaitRig` (ADR-8707): the engine steps the gait and solves the legs. These tests fail to
//! compile on the base, which has no component.
//!
//! The rig is a code-built hexapod (pairs from the front, left before right; −Z forward):
//! a root at the floor, a body joint 0.5 m up, and six legs whose tips rest on the floor.
//! This binary installs a counting global allocator for the allocation test.

#![allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::time::Duration;

use regia_animation::clip::AnimationClip;
use regia_animation::components::spawn_skeleton;
use regia_animation::foot_ik::{GroundHit, GroundProbe, GroundProbes};
use regia_animation::gait::{
    GaitMode, GaitParams, GaitPatternSet, GaitPosture, GaitState, MAX_LEGS, TRIPOD_LEGS, leg_array,
};
use regia_animation::gait_rig::{
    GaitControls, GaitLeg, GaitRig, GaitRigIssue, GaitRigMode, GaitRigStatus, GaitVelocity,
};
use regia_animation::skeleton::LocalBind;
use regia_animation::{AnimationPlugin, Animator, JointGlobals, Skeleton};
use regia_app::prelude::*;
use regia_core::math::{Mat4, Quat, Vec3};
use regia_ecs::ecs::schedule::{Schedules, SingleThreadedExecutor};
use regia_scene::Transform;

const DT: f32 = 1.0 / 60.0;
const BODY_HEIGHT: f32 = 0.5;
/// Leg pairs along Z (object space, front first: −Z is forward).
const PAIR_Z: [f32; 3] = [-0.5, 0.0, 0.5];

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: forwards to the system allocator and counts calls.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
        // SAFETY: same contract as the caller.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
        // SAFETY: same contract as the caller.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocations() -> usize {
    ALLOCATIONS.with(Cell::get)
}

/// Leg segments as local translations from the hip (unit side, the sign of X applied later).
fn segments(four: bool) -> &'static [[f32; 3]] {
    if four {
        // hip -> a -> b -> c -> foot: four segments, five joints (FABRIK).
        &[
            [0.15, 0.1, 0.0],
            [0.15, 0.05, 0.0],
            [0.1, -0.3, 0.0],
            [0.0, -0.35, 0.0],
        ]
    } else {
        // hip -> knee -> foot: two segments, three joints (two-bone).
        &[[0.2, 0.15, 0.0], [0.15, -0.65, 0.0]]
    }
}

fn leg_names(leg: usize, four: bool) -> Vec<String> {
    let count = segments(four).len() + 1;
    (0..count)
        .map(|j| match j {
            0 => format!("hip{leg}"),
            j if j + 1 == count => format!("foot{leg}"),
            j => format!("seg{leg}_{j}"),
        })
        .collect()
}

/// A hexapod: `root` at the floor, `body` 0.5 m up, six legs whose tips are on the floor.
fn hexapod(four: bool) -> Arc<Skeleton> {
    let mut names = vec!["root".to_string(), "body".to_string()];
    let mut parents = vec![None, Some(0)];
    let mut binds = vec![
        LocalBind::default(),
        LocalBind {
            translation: Vec3::new(0.0, BODY_HEIGHT, 0.0),
            ..LocalBind::default()
        },
    ];
    for leg in 0..TRIPOD_LEGS {
        let side = if leg % 2 == 0 { -1.0 } else { 1.0 };
        let z = PAIR_Z[leg / 2];
        let mut parent = 1_u32;
        for (j, name) in leg_names(leg, four).into_iter().enumerate() {
            let translation = if j == 0 {
                Vec3::new(side * 0.25, 0.0, z)
            } else {
                let [x, y, z] = segments(four)[j - 1];
                Vec3::new(side * x, y, z)
            };
            names.push(name);
            parents.push(Some(parent));
            binds.push(LocalBind {
                translation,
                ..LocalBind::default()
            });
            parent = (names.len() - 1) as u32;
        }
    }
    Arc::new(Skeleton::from_local_bind(names, parents, binds).expect("hexapod"))
}

fn legs() -> Vec<GaitLeg> {
    (0..TRIPOD_LEGS)
        .map(|i| GaitLeg::new(format!("hip{i}"), format!("foot{i}")))
        .collect()
}

fn params() -> GaitParams {
    GaitParams {
        forward_speed: 0.6,
        reverse_speed: 0.3,
        strafe_speed: 0.3,
        acceleration: 2.0,
        step_length: 0.3,
        clearance: BODY_HEIGHT,
        swing_lift: 0.1,
        max_slope: 1.5,
        idle_speed: 0.02,
        max_yaw_rate: 0.8,
        hip_offsets: leg_array([[0.0; 2]; TRIPOD_LEGS], [0.0; 2]),
        settle_distance: 0.05,
        settle_period: 0.3,
        turn_step_distance: 0.1,
        turn_step_angle: 0.35,
        ground_band: 0.6,
        foothold_candidates: 0,
        foothold_spacing: 0.2,
        max_reach: [f32::INFINITY; MAX_LEGS],
        hang_drop: 0.2,
        patterns: GaitPatternSet::tripod(),
        posture: GaitPosture::UPRIGHT,
    }
}

/// A plane through `origin` rising `tan` per meter along `uphill` (unit, on XZ).
#[derive(Clone, Copy)]
struct Slope {
    origin: Vec3,
    uphill: Vec3,
    tan: f32,
}

impl Slope {
    fn new(uphill: Vec3, degrees: f32) -> Self {
        Self {
            origin: Vec3::ZERO,
            uphill: uphill.normalize(),
            tan: degrees.to_radians().tan(),
        }
    }

    fn height(&self, x: f32, z: f32) -> f32 {
        self.origin.y + self.tan * (Vec3::new(x, 0.0, z) - self.origin).dot(self.uphill)
    }

    fn normal(&self) -> Vec3 {
        (Vec3::Y - self.uphill * self.tan).normalize()
    }
}

impl GroundProbe for Slope {
    fn probe(&self, from: Vec3, down: Vec3, max_distance: f32, _ignore: Entity) -> Option<GroundHit> {
        let down = down.normalize_or_zero();
        let normal = self.normal();
        let denom = down.dot(normal);
        if denom.abs() < 1.0e-8 {
            return None;
        }
        let t = (self.origin - from).dot(normal) / denom;
        if !t.is_finite() || t < 0.0 || t > max_distance {
            return None;
        }
        Some(GroundHit {
            point: from + down * t,
            normal,
        })
    }
}

fn pin_schedules(app: &mut App) {
    let mut schedules = app.world_mut().resource_mut::<Schedules>();
    for (_, schedule) in schedules.iter_mut() {
        schedule.set_executor(SingleThreadedExecutor::new());
    }
}

fn app(probe: Option<Slope>) -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AnimationPlugin));
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(DT)));
    if let Some(probe) = probe {
        app.insert_resource(GroundProbes::new(probe));
    }
    app
}

fn spawn(app: &mut App, skeleton: Arc<Skeleton>, transform: Transform, rig: Option<GaitRig>) -> Entity {
    let animator = Animator::single(AnimationClip::empty("still", 1.0), true);
    let binding = spawn_skeleton(app.world_mut(), skeleton);
    let mut entity = app.world_mut().spawn((transform, binding, animator));
    if let Some(rig) = rig {
        entity.insert(rig);
    }
    entity.id()
}

fn world_joint(app: &App, rig: Entity, joint: u32) -> Vec3 {
    let globals = app.world().get::<JointGlobals>(rig).expect("JointGlobals");
    let local = globals.globals[joint as usize].w_axis.truncate();
    let t = app.world().get::<Transform>(rig).expect("transform");
    t.translation + t.rotation * (local * t.scale)
}

/// Worst distance from each leg tip to its gait foot target this frame, over `legs`.
fn worst_tip_error(app: &App, rig: Entity, skeleton: &Skeleton, legs: &[usize]) -> f32 {
    let state = app.world().get::<GaitState>(rig).expect("GaitState");
    legs.iter()
        .map(|&i| {
            let tip = skeleton.joint_index(&format!("foot{i}")).expect("foot");
            let target = Vec3::from_array(state.feet[i].position);
            world_joint(app, rig, tip).distance(target)
        })
        .fold(0.0, f32::max)
}

/// A driven hexapod walking straight ahead (−Z rotated by `yaw`) at `speed` up a slope, for
/// `frames` frames. Returns the worst tip error after the first frame (the state exists from
/// then on), and how far the feet moved in total.
fn walk_on_slope(four: bool, frames: usize) -> (f32, f32, GaitRigStatus) {
    let skeleton = hexapod(four);
    let yaw = 0.3_f32;
    let rotation = Quat::from_rotation_y(yaw);
    let forward = rotation * Vec3::NEG_Z;
    let slope = Slope::new(forward, 10.0);
    let mut app = app(Some(slope));
    let start = Transform {
        translation: Vec3::new(0.0, slope.height(0.0, 0.0), 0.0),
        rotation,
        scale: Vec3::ONE,
    };
    let rig = spawn(
        &mut app,
        skeleton.clone(),
        start,
        Some(GaitRig::new(params(), legs()).with_body("body")),
    );
    let speed = 0.4;
    let all: Vec<usize> = (0..TRIPOD_LEGS).collect();
    let mut worst = 0.0_f32;
    let mut travel = 0.0_f32;
    let mut last: Option<[Vec3; TRIPOD_LEGS]> = None;
    for frame in 0..frames {
        let distance = speed * DT * frame as f32;
        let p = forward * distance;
        let mut t = app.world_mut().get_mut::<Transform>(rig).expect("transform");
        t.translation = Vec3::new(p.x, slope.height(p.x, p.z), p.z);
        app.update();
        if frame == 0 {
            continue;
        }
        worst = worst.max(worst_tip_error(&app, rig, &skeleton, &all));
        let state = app.world().get::<GaitState>(rig).expect("state");
        let feet: [Vec3; TRIPOD_LEGS] = std::array::from_fn(|i| Vec3::from_array(state.feet[i].position));
        if let Some(prev) = last {
            travel += feet.iter().zip(prev).map(|(a, b)| a.distance(b)).sum::<f32>();
        }
        last = Some(feet);
        // Planted feet stand on the slope.
        for foot in state.feet() {
            if foot.planted {
                let ground = slope.height(foot.position[0], foot.position[2]);
                assert!(
                    (foot.position[1] - ground).abs() < 1.0e-3,
                    "frame {frame}: planted foot {:?} off the slope ({ground})",
                    foot.position
                );
            }
        }
    }
    let status = *app.world().get::<GaitRigStatus>(rig).expect("status");
    (worst, travel, status)
}

#[test]
fn gait_rig_feet_reach_their_targets_within_1mm() {
    let (worst, travel, status) = walk_on_slope(false, 600);
    println!("two-bone legs: worst tip error {worst:.6} m, feet travel {travel:.3} m");
    assert!(worst < 1.0e-3, "worst tip error {worst} m");
    // 600 frames at 0.4 m/s walk 4 m: every foot travels with the body.
    assert!(travel > 6.0 * 3.0, "feet travelled {travel} m");
    assert_eq!(status.issue, GaitRigIssue::None);
    assert_eq!(status.legs_resolved, 6);
    assert_eq!(status.warnings, 0);
    assert_eq!(status.step_errors, 0);
    assert!(status.probe_installed);
}

#[test]
fn four_segment_legs_use_fabrik_and_reach_within_1mm() {
    let (worst, travel, status) = walk_on_slope(true, 600);
    println!("four-segment legs: worst tip error {worst:.6} m, feet travel {travel:.3} m");
    assert!(worst < 1.0e-3, "worst tip error {worst} m");
    assert!(travel > 6.0 * 3.0, "feet travelled {travel} m");
    assert_eq!(status.issue, GaitRigIssue::None);
    assert_eq!(status.legs_resolved, 6);
    assert_eq!(status.step_errors, 0);
}

/// Both conventions: the measured offsets match the bind tips by hand, and a rig planted at
/// rest in either mode puts every neutral foot exactly under its bind-pose tip in the world.
#[test]
fn hip_offsets_from_skeleton_match_the_bind_pose() {
    let skeleton = hexapod(false);
    let legs = legs();
    let driven = GaitParams::hip_offsets_from_skeleton(&skeleton, &legs, GaitMode::Driven).expect("driven");
    let integrating =
        GaitParams::hip_offsets_from_skeleton(&skeleton, &legs, GaitMode::Integrating).expect("integrating");
    for i in 0..TRIPOD_LEGS {
        let side = if i % 2 == 0 { -1.0 } else { 1.0 };
        let (x, z) = (side * 0.6, PAIR_Z[i / 2]);
        // Driven: [right, forward] with forward = -Z.
        assert!(
            (driven[i][0] - x).abs() < 1.0e-6 && (driven[i][1] + z).abs() < 1.0e-6,
            "driven {i}: {:?}",
            driven[i]
        );
        // Integrating: [+X, +Z] of a stepper turned by pi.
        assert!(
            (integrating[i][0] + x).abs() < 1.0e-6 && (integrating[i][1] + z).abs() < 1.0e-6,
            "integrating {i}: {:?}",
            integrating[i]
        );
    }
    // Front legs are forward (+ in the driven frame), left legs have negative right.
    assert!(driven[0][1] > 0.0 && driven[0][0] < 0.0);
    // Unused entries stay zero; a missing tip is refused.
    assert!(driven[TRIPOD_LEGS..].iter().all(|o| *o == [0.0; 2]));
    let mut bad = legs.clone();
    bad[3].tip = "nope".into();
    assert_eq!(
        GaitParams::hip_offsets_from_skeleton(&skeleton, &bad, GaitMode::Driven),
        Err(GaitRigIssue::LegUnresolved)
    );

    let bind_globals = skeleton.globals(&skeleton.bind_pose_pose(), None);
    for mode in [GaitRigMode::Driven, GaitRigMode::Integrating] {
        let rotation = Quat::from_rotation_y(0.7);
        let start = Transform {
            translation: Vec3::new(2.0, 0.0, -3.0),
            rotation,
            scale: Vec3::ONE,
        };
        let mut app = app(None);
        let rig = spawn(
            &mut app,
            skeleton.clone(),
            start,
            Some(GaitRig::new(params(), legs.clone()).with_mode(mode)),
        );
        app.update();
        app.update();
        let state = *app.world().get::<GaitState>(rig).expect("state");
        for i in 0..TRIPOD_LEGS {
            let tip = skeleton.joint_index(&format!("foot{i}")).expect("foot") as usize;
            let bind = start.translation + rotation * bind_globals[tip].w_axis.truncate();
            let foot = Vec3::from_array(state.feet[i].position);
            assert!(
                (foot.x - bind.x).abs() < 1.0e-5 && (foot.z - bind.z).abs() < 1.0e-5 && foot.y.abs() < 1.0e-6,
                "{mode:?} leg {i}: neutral {foot:?} vs bind tip {bind:?}"
            );
        }
        // The integrating rig keeps its heading: it still faces -Z rotated by 0.7.
        let t = app.world().get::<Transform>(rig).expect("transform");
        let facing = t.rotation * Vec3::NEG_Z;
        assert!(
            facing.distance(rotation * Vec3::NEG_Z) < 1.0e-5,
            "{mode:?}: facing {facing:?}"
        );
    }
}

/// An integrating rig walks forward on its own, along its −Z.
#[test]
fn integrating_rig_walks_forward_from_controls() {
    let skeleton = hexapod(false);
    let mut app = app(None);
    let rotation = Quat::from_rotation_y(-0.5);
    let start = Transform {
        translation: Vec3::ZERO,
        rotation,
        scale: Vec3::ONE,
    };
    let rig = spawn(
        &mut app,
        skeleton.clone(),
        start,
        Some(
            GaitRig::new(params(), legs())
                .with_body("body")
                .with_mode(GaitRigMode::Integrating),
        ),
    );
    app.world_mut().entity_mut(rig).insert(GaitControls {
        forward: 1.0,
        ..GaitControls::default()
    });
    let all: Vec<usize> = (0..TRIPOD_LEGS).collect();
    let mut worst = 0.0_f32;
    for frame in 0..300 {
        app.update();
        if frame > 0 {
            worst = worst.max(worst_tip_error(&app, rig, &skeleton, &all));
        }
    }
    let t = *app.world().get::<Transform>(rig).expect("transform");
    let moved = t.translation.dot(rotation * Vec3::NEG_Z);
    assert!(moved > 1.5, "moved {moved} m forward");
    assert!(
        t.translation.dot(rotation * Vec3::X).abs() < 1.0e-3,
        "drifted sideways: {:?}",
        t.translation
    );
    assert!(worst < 1.0e-3, "worst tip error {worst} m");
}

fn bits(m: &[Mat4]) -> Vec<u32> {
    m.iter()
        .flat_map(|m| m.to_cols_array())
        .map(f32::to_bits)
        .collect()
}

/// A rig without `GaitRig` gets the same apply output as before, with a walker in the scene or
/// not, and that output is the bind pose's forward kinematics.
#[test]
fn a_rig_without_gait_rig_is_bit_identical() {
    let skeleton = hexapod(false);
    let mut with = app(None);
    let mut without = app(None);
    let still = Transform {
        translation: Vec3::new(1.0, 0.0, 2.0),
        rotation: Quat::from_rotation_y(0.2),
        scale: Vec3::ONE,
    };
    let plain_with = spawn(&mut with, skeleton.clone(), still, None);
    let walker = spawn(
        &mut with,
        skeleton.clone(),
        Transform::default(),
        Some(GaitRig::new(params(), legs()).with_body("body")),
    );
    let plain_without = spawn(&mut without, skeleton.clone(), still, None);
    let expected = bits(&skeleton.globals(&skeleton.bind_pose_pose(), None));
    for frame in 0..120 {
        let z = -0.4 * DT * frame as f32;
        with.world_mut()
            .get_mut::<Transform>(walker)
            .expect("walker")
            .translation
            .z = z;
        with.update();
        without.update();
        let a = with.world().get::<JointGlobals>(plain_with).expect("globals");
        let b = without
            .world()
            .get::<JointGlobals>(plain_without)
            .expect("globals");
        assert_eq!(bits(&a.globals), bits(&b.globals), "frame {frame}");
        assert_eq!(bits(&a.globals), expected, "frame {frame}");
    }
    assert!(with.world().get::<GaitState>(plain_with).is_none());
    assert!(with.world().get::<GaitRigStatus>(plain_with).is_none());
    assert!(with.world().get::<GaitState>(walker).is_some());
}

/// A leg whose tip is not on the skeleton is reported once and skipped; the other legs walk.
/// A wrong leg count is reported and nothing is stepped. Neither panics.
#[test]
fn missing_leg_joint_reports_and_skips() {
    let skeleton = hexapod(false);
    let mut legs = legs();
    legs[2].tip = "no_such_foot".into();
    let mut app = app(Some(Slope::new(Vec3::NEG_Z, 0.0)));
    let rig = spawn(
        &mut app,
        skeleton.clone(),
        Transform::default(),
        Some(GaitRig::new(params(), legs).with_body("body")),
    );
    let solved = [0, 1, 3, 4, 5];
    let hip2 = skeleton.joint_index("hip2").expect("hip2") as usize;
    let mut worst = 0.0_f32;
    for frame in 0..300 {
        app.world_mut()
            .get_mut::<Transform>(rig)
            .expect("t")
            .translation
            .z = -0.4 * DT * frame as f32;
        app.update();
        if frame > 0 {
            worst = worst.max(worst_tip_error(&app, rig, &skeleton, &solved));
            // The skipped leg keeps the clip's (bind) rotation at its hip.
            let joints = app
                .world()
                .get::<regia_animation::SkinnedMesh>(rig)
                .expect("mesh")
                .joints[hip2];
            let hip = app.world().get::<Transform>(joints).expect("hip");
            assert_eq!(hip.rotation, Quat::IDENTITY, "frame {frame}");
        }
    }
    let status = *app.world().get::<GaitRigStatus>(rig).expect("status");
    assert_eq!(status.issue, GaitRigIssue::LegUnresolved);
    assert_eq!(status.issue.code(), Some("gait_rig_leg_unresolved"));
    assert_eq!(status.legs_resolved, 5);
    assert_eq!(status.warnings, 1, "one warning per resolve, not per frame");
    assert_eq!(status.step_errors, 0);
    assert!(worst < 1.0e-3, "worst tip error {worst} m");

    // A missing body joint: legs still solve, the body is not lifted.
    let mut app2 = app_with_rig(GaitRig::new(params(), self::legs()).with_body("no_body"));
    for _ in 0..10 {
        app2.0.update();
    }
    let status = *app2.0.world().get::<GaitRigStatus>(app2.1).expect("status");
    assert_eq!(status.issue.code(), Some("gait_rig_body_unresolved"));
    assert_eq!(status.legs_resolved, 6);

    // Five legs for a six-leg pattern: reported, never stepped.
    let mut five = self::legs();
    five.pop();
    let mut app3 = app_with_rig(GaitRig::new(params(), five));
    for _ in 0..10 {
        app3.0.update();
    }
    let status = *app3.0.world().get::<GaitRigStatus>(app3.1).expect("status");
    assert_eq!(status.issue.code(), Some("gait_rig_leg_count"));
    assert_eq!(status.warnings, 1);
    assert!(app3.0.world().get::<GaitState>(app3.1).is_none());
}

fn app_with_rig(rig: GaitRig) -> (App, Entity) {
    let mut app = app(None);
    let entity = spawn(&mut app, hexapod(false), Transform::default(), Some(rig));
    (app, entity)
}

/// Frames of a walking `GaitRig` allocate exactly as many times as the same rig without one.
#[test]
fn gait_rig_allocates_nothing_after_warmup() {
    fn frame_counts(walker: bool, four: bool) -> Vec<usize> {
        let mut app = app(Some(Slope::new(Vec3::NEG_Z, 6.0)));
        pin_schedules(&mut app);
        let rig = spawn(
            &mut app,
            hexapod(four),
            Transform::default(),
            walker.then(|| GaitRig::new(params(), legs()).with_body("body")),
        );
        let mut frame = 0;
        let mut step = |app: &mut App| {
            let z = -0.4 * DT * frame as f32;
            app.world_mut()
                .get_mut::<Transform>(rig)
                .expect("t")
                .translation
                .z = z;
            frame += 1;
            app.update();
        };
        for _ in 0..60 {
            step(&mut app);
        }
        pin_schedules(&mut app);
        for _ in 0..120 {
            step(&mut app);
        }
        (0..60)
            .map(|_| {
                let before = allocations();
                step(&mut app);
                allocations() - before
            })
            .collect()
    }
    for four in [false, true] {
        let walking = frame_counts(true, four);
        let plain = frame_counts(false, four);
        println!("gait_rig alloc (four segments {four}): walker {walking:?}, plain {plain:?}");
        assert_eq!(
            walking.iter().sum::<usize>(),
            plain.iter().sum::<usize>(),
            "a walking GaitRig allocated (four segments {four}): {walking:?} vs {plain:?}"
        );
    }
}

/// Timing evidence only (never an assertion): mean wall time of an app frame with eight walking
/// hexapods against the same scene without `GaitRig`. Run with
/// `cargo test --release -p regia_animation --test gait_rig gait_rig_frame_cost_evidence -- --ignored --nocapture`.
#[test]
#[ignore = "timing evidence; prints numbers, asserts nothing"]
fn gait_rig_frame_cost_evidence() {
    fn mean_frame_us(walkers: bool, four: bool) -> f64 {
        let mut app = app(Some(Slope::new(Vec3::NEG_Z, 6.0)));
        let rigs: Vec<Entity> = (0..8)
            .map(|k| {
                let start = Transform {
                    translation: Vec3::new(3.0 * k as f32, 0.0, 0.0),
                    ..Transform::default()
                };
                spawn(
                    &mut app,
                    hexapod(four),
                    start,
                    walkers.then(|| GaitRig::new(params(), legs()).with_body("body")),
                )
            })
            .collect();
        let frames = 600;
        let mut total = Duration::ZERO;
        for frame in 0..(frames + 60) {
            for (k, &rig) in rigs.iter().enumerate() {
                let mut t = app.world_mut().get_mut::<Transform>(rig).expect("t");
                t.translation = Vec3::new(3.0 * k as f32, 0.0, -0.4 * DT * frame as f32);
            }
            let start = std::time::Instant::now();
            app.update();
            if frame >= 60 {
                total += start.elapsed();
            }
        }
        total.as_secs_f64() * 1.0e6 / f64::from(frames)
    }
    for four in [false, true] {
        let plain = mean_frame_us(false, four);
        let walking = mean_frame_us(true, four);
        println!(
            "gait_rig frame cost (8 hexapods, four segments {four}): plain {plain:.1} us, walking {walking:.1} us, \
             gait share {:.1} us/frame ({:.2} us per rig)",
            walking - plain,
            (walking - plain) / 8.0
        );
    }
}

// --- review proofs (anim-gait-rig-review). Each asserts the contract at head 5ac8991ad. ---

fn app_dt(probe: Option<Slope>, dt: f32) -> App {
    let mut app = app(probe);
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(dt)));
    app
}

/// Largest XZ distance from a gait foot to that leg's bind-pose tip under the rig's transform.
fn max_foot_offset_from_neutral(app: &App, rig: Entity, skeleton: &Skeleton) -> f32 {
    let state = app.world().get::<GaitState>(rig).expect("state");
    let transform = *app.world().get::<Transform>(rig).expect("transform");
    let bind = skeleton.globals(&skeleton.bind_pose_pose(), None);
    (0..TRIPOD_LEGS)
        .map(|i| {
            let tip = skeleton.joint_index(&format!("foot{i}")).expect("foot") as usize;
            let neutral = transform.translation + transform.rotation * bind[tip].w_axis.truncate();
            let foot = Vec3::from_array(state.feet[i].position);
            Vec3::new(foot.x - neutral.x, 0.0, foot.z - neutral.z).length()
        })
        .fold(0.0_f32, f32::max)
}

fn ground_speed(state: &GaitState) -> f32 {
    state.velocity[0].hypot(state.velocity[2])
}

/// Integrating fast gait. A 30 Hz frame is one substep of 1/30 s, which is longer than this
/// stance (`step_length / forward_speed`), so the stepper's `gait_step_too_large` fires and the
/// rig freezes. 60 Hz and 144 Hz stay under one stance per frame and walk.
fn fast_params() -> GaitParams {
    let mut params = params();
    params.forward_speed = 12.0;
    params.acceleration = 40.0;
    params.step_length = 0.25;
    params
}

fn integrating_travel(dt: f32, seconds: f32, gait: GaitParams, controls: GaitControls) -> (f32, u32, f32) {
    let frames = (seconds / dt).round() as u32;
    let mut app = app_dt(None, dt);
    let rig = spawn(
        &mut app,
        hexapod(false),
        Transform::default(),
        Some(
            GaitRig::new(gait, legs())
                .with_body("body")
                .with_mode(GaitRigMode::Integrating),
        ),
    );
    app.world_mut().entity_mut(rig).insert(controls);
    for _ in 0..frames {
        app.update();
    }
    let transform = *app.world().get::<Transform>(rig).expect("transform");
    let status = *app.world().get::<GaitRigStatus>(rig).expect("status");
    let state = *app.world().get::<GaitState>(rig).expect("state");
    let facing = transform.rotation * Vec3::NEG_Z;
    (
        -transform.translation.z,
        status.step_errors,
        ground_speed(&state) + facing.x.abs(),
    )
}

/// Same settings and the same 2 s of `forward = 1` at 30, 60 and 144 Hz. The rig must accept
/// every frame and finish within 5% of the 60 Hz distance. `forward_speed` 12 m/s and
/// `step_length` 0.25 m is one stance every 0.0208 s: under 1/60 and 1/144, over 1/30.
#[test]
fn fast_gait_is_frame_rate_independent_at_30_60_144() {
    let seconds = 2.0;
    let rates = [30.0_f32, 60.0, 144.0];
    let mut distances = [0.0_f32; 3];
    let mut errors = [0_u32; 3];
    let mut speeds = [0.0_f32; 3];
    for (i, hz) in rates.iter().enumerate() {
        let dt = 1.0 / hz;
        let (moved, errs, speed) = integrating_travel(
            dt,
            seconds,
            fast_params(),
            GaitControls {
                forward: 1.0,
                ..GaitControls::default()
            },
        );
        println!("fast gait {hz} Hz: moved {moved:.3} m, speed term {speed:.3}, step_errors {errs}");
        distances[i] = moved;
        errors[i] = errs;
        speeds[i] = speed;
    }
    for (hz, (errs, speed)) in rates.iter().zip(errors.iter().zip(speeds)) {
        assert_eq!(*errs, 0, "{hz} Hz refused {errs} frames");
        assert!(
            speed > 11.0,
            "{hz} Hz ground speed stayed {speed} m/s (commanded 12)"
        );
    }
    let at_60 = distances[1];
    let scale = at_60.max(1.0);
    for (moved, hz) in [(distances[0], 30.0_f32), (distances[2], 144.0)] {
        assert!(
            (moved - at_60).abs() / scale < 0.05,
            "{hz} Hz moved {moved:.3} m, 60 Hz moved {at_60:.3} m"
        );
    }
}

/// Idle turn with `settle_period` 0.02 s (a legal positive value). The swing clock refuses a
/// step longer than one settle period. At 30 Hz the frame is 1/30 s, so the first swing is
/// refused and the turn sticks. At 60 and 144 Hz the same turn completes.
#[test]
fn short_settle_period_turns_at_30_60_144() {
    let mut gait = params();
    gait.settle_period = 0.02;
    gait.turn_step_angle = 0.05;
    gait.max_yaw_rate = 1.0;
    let controls = GaitControls {
        steer: 1.0,
        ..GaitControls::default()
    };
    let rates = [30.0_f32, 60.0, 144.0];
    let mut turned = [0.0_f32; 3];
    let mut errors = [0_u32; 3];
    for (i, hz) in rates.iter().enumerate() {
        let dt = 1.0 / hz;
        let frames = (2.0 / dt).round() as u32;
        let mut app = app_dt(None, dt);
        let rig = spawn(
            &mut app,
            hexapod(false),
            Transform::default(),
            Some(
                GaitRig::new(gait, legs())
                    .with_body("body")
                    .with_mode(GaitRigMode::Integrating),
            ),
        );
        app.world_mut().entity_mut(rig).insert(controls);
        for _ in 0..frames {
            app.update();
        }
        let transform = *app.world().get::<Transform>(rig).expect("transform");
        let status = *app.world().get::<GaitRigStatus>(rig).expect("status");
        turned[i] = (transform.rotation * Vec3::NEG_Z).distance(Vec3::NEG_Z);
        errors[i] = status.step_errors;
        let yaw = turned[i];
        let errs = errors[i];
        println!("settle {hz} Hz: facing moved {yaw:.3}, step_errors {errs}");
    }
    for (hz, (errs, yaw)) in rates.iter().zip(errors.iter().zip(turned)) {
        assert_eq!(*errs, 0, "{hz} Hz refused {errs} frames");
        assert!(
            yaw > 0.8,
            "{hz} Hz only turned {yaw} (2 s at 1 rad/s is about 2 rad)"
        );
    }
}

/// A driven frame whose gameplay delta exceeds eight substeps of 1/30 s. `Time` feeds the gait
/// that delta: a wall gap is clamped to 250 ms and then multiplied by `time_scale`, so a 250 ms
/// stall at scale 4 is a 1 s gameplay frame. The body has already moved 1.5 m. That is more
/// than one `step_length` (0.4 m), which the stepper replants, but the rig slices the move into
/// eight pieces under `step_length` and only advances phase for the clamped 8/30 s.
#[test]
fn long_driven_frame_keeps_feet_within_one_step_of_neutral() {
    let mut gait = params();
    gait.step_length = 0.4;
    let skeleton = hexapod(false);
    let mut app = app_dt(None, DT);
    let rig = spawn(
        &mut app,
        skeleton.clone(),
        Transform::default(),
        Some(GaitRig::new(gait, legs()).with_body("body")),
    );
    app.update();
    let moved = 1.5_f32;
    app.world_mut()
        .get_mut::<Transform>(rig)
        .expect("transform")
        .translation
        .z = -moved;
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(1.0)));
    app.update();
    let status = *app.world().get::<GaitRigStatus>(rig).expect("status");
    let offset = max_foot_offset_from_neutral(&app, rig, &skeleton);
    let issue = status.issue;
    let errors = status.step_errors;
    let warnings = status.warnings;
    let step_length = gait.step_length;
    println!(
        "long frame: foot offset {offset:.3} m, issue {issue:?} step_errors {errors} warnings {warnings}"
    );
    assert_eq!(errors, 0, "a 1.5 m move is a replant, not a refused step");
    assert_eq!(issue, GaitRigIssue::None);
    assert!(
        offset <= step_length + 0.05,
        "feet left {offset:.3} m from neutral after a {moved} m frame (step_length {step_length})"
    );
}

/// `GaitRig::reset` replants every foot at neutral on the next frame.
#[test]
fn reset_replants_the_feet_at_neutral() {
    let skeleton = hexapod(false);
    let mut app = app_dt(None, DT);
    let rig = spawn(
        &mut app,
        skeleton.clone(),
        Transform::default(),
        Some(GaitRig::new(params(), legs()).with_body("body")),
    );
    for frame in 0..90 {
        app.world_mut()
            .get_mut::<Transform>(rig)
            .expect("transform")
            .translation
            .z = -0.4 * DT * frame as f32;
        app.update();
    }
    let before = max_foot_offset_from_neutral(&app, rig, &skeleton);
    assert!(
        before > 0.02,
        "warmup left every foot within {before} m of neutral; the reset check would be vacuous"
    );
    app.world_mut().get_mut::<GaitRig>(rig).expect("rig").reset();
    app.update();
    let after = max_foot_offset_from_neutral(&app, rig, &skeleton);
    println!("reset: offset before {before:.3} m, after {after:.4} m");
    assert!(
        after < 1.0e-3,
        "reset left feet {after:.3} m off their bind-pose neutral"
    );
}

/// `ik_tolerance` of +inf (what `scene.set_property` stores for a JSON number such as 1e39)
/// is not a stepper input. FABRIK treats the chain as already solved and the status stays
/// healthy while the feet miss their targets.
#[test]
fn infinite_ik_tolerance_is_reported_or_still_reaches() {
    let skeleton = hexapod(true);
    let mut gait_rig = GaitRig::new(params(), legs()).with_body("body");
    gait_rig.ik_tolerance = f32::INFINITY;
    let mut app = app_dt(None, DT);
    let rig = spawn(&mut app, skeleton.clone(), Transform::default(), Some(gait_rig));
    let all: Vec<usize> = (0..TRIPOD_LEGS).collect();
    let mut worst = 0.0_f32;
    for frame in 0..60 {
        app.world_mut()
            .get_mut::<Transform>(rig)
            .expect("transform")
            .translation
            .z = -0.4 * DT * frame as f32;
        app.update();
        if frame > 0 {
            worst = worst.max(worst_tip_error(&app, rig, &skeleton, &all));
        }
    }
    let issue = app.world().get::<GaitRigStatus>(rig).expect("status").issue;
    println!("inf tolerance: worst tip {worst:.3} m, issue {issue:?}");
    assert!(
        worst < 1.0e-3 || issue == GaitRigIssue::Step,
        "worst tip {worst} m with issue {issue:?} (non-finite tolerance was neither solved nor reported)"
    );
}

/// An unbounded `ground_band` (infinity is legal) with a non-finite probe start. The cast
/// origin is not finite, the probe misses, and the feet hang with no issue.
#[test]
fn infinite_probe_height_is_reported_or_still_stands() {
    let mut gait = params();
    gait.ground_band = f32::INFINITY;
    let mut gait_rig = GaitRig::new(gait, legs()).with_body("body");
    gait_rig.probe_height = f32::INFINITY;
    gait_rig.probe_length = 2.0;
    let skeleton = hexapod(false);
    let mut app = app_dt(Some(Slope::new(Vec3::NEG_Z, 0.0)), DT);
    let rig = spawn(
        &mut app,
        skeleton,
        Transform::default(),
        Some(gait_rig.with_body("body")),
    );
    for _ in 0..5 {
        app.update();
    }
    let status = *app.world().get::<GaitRigStatus>(rig).expect("status");
    let state = *app.world().get::<GaitState>(rig).expect("state");
    let planted = state.feet().iter().filter(|foot| foot.planted).count();
    let worst_y = state
        .feet()
        .iter()
        .map(|foot| foot.position[1].abs())
        .fold(0.0_f32, f32::max);
    let issue = status.issue;
    println!("inf probe: planted {planted}/6, worst |y| {worst_y:.3}, issue {issue:?}");
    assert!(
        issue == GaitRigIssue::Step || (planted == TRIPOD_LEGS && worst_y < 1.0e-3),
        "planted {planted}/6, worst |y| {worst_y:.3}, issue {issue:?} (non-finite probe was not reported)"
    );
}

// --- verification proofs (anim-locomotion-review-fixes-verification). These assert the
// contract the fix lane should have kept. They fail on c38e14767. ---

/// `forward_speed` 200 m/s, `step_length` 0.25 m, `acceleration` 40. At 30 Hz the finest
/// slice is 1/480 s, so one legal stance is 120 m/s. The speed reaches that cap in about
/// 3 s (`progress == 1` is still legal). The next frame's first finest slice accelerates
/// past the cap, `run_slices` rolls the whole frame back, and the rig stops while holding
/// a walkable speed. The last second of a 4 s run must still move, with no refused frames.
#[test]
fn command_above_the_finest_slice_keeps_walking() {
    let mut gait = fast_params();
    gait.forward_speed = 200.0;
    let controls = GaitControls {
        forward: 1.0,
        ..GaitControls::default()
    };
    let dt = 1.0 / 30.0;
    let (at_3, errs_3, speed_3) = integrating_travel(dt, 3.0, gait, controls);
    let (at_4, errs_4, speed_4) = integrating_travel(dt, 4.0, gait, controls);
    let last = at_4 - at_3;
    println!(
        "cap walk 30 Hz: 3 s {at_3:.3} m (errors {errs_3}, speed {speed_3:.3}), \
         4 s {at_4:.3} m (errors {errs_4}, speed {speed_4:.3}), last second {last:.3} m"
    );
    assert_eq!(
        errs_4, 0,
        "30 Hz refused {errs_4} frames after the speed reached the finest-slice cap"
    );
    assert!(
        last > 30.0,
        "30 Hz last second moved {last:.3} m (the frame that passed the cap rolled back and the walk froze)"
    );
}

/// A non-finite velocity refuses the rest plant (`gait_input`). The restart must survive so
/// the next finite frame replants at neutral instead of stepping the old walk. Probe hits
/// cannot inject this refusal: GroundProbes filters non-finite hits to misses (ADR-8721).
#[test]
fn reset_survives_a_refused_frame() {
    let skeleton = hexapod(false);
    let mut app = app_dt(None, DT);
    let rig = spawn(
        &mut app,
        skeleton.clone(),
        Transform::default(),
        Some(GaitRig::new(params(), legs()).with_body("body")),
    );
    for frame in 0..90 {
        app.world_mut()
            .get_mut::<Transform>(rig)
            .expect("transform")
            .translation
            .z = -0.4 * DT * frame as f32;
        app.update();
    }
    let before = max_foot_offset_from_neutral(&app, rig, &skeleton);
    assert!(
        before > 0.02,
        "warmup left every foot within {before} m of neutral; the reset check would be vacuous"
    );
    let prior_state = *app.world().get::<GaitState>(rig).expect("state");
    app.world_mut().get_mut::<GaitRig>(rig).expect("rig").reset();
    app.world_mut().entity_mut(rig).insert(GaitVelocity {
        linear: Vec3::splat(f32::NAN),
    });
    app.update();
    let refused = app.world().get::<GaitRigStatus>(rig).expect("status").issue;
    assert_eq!(
        *app.world().get::<GaitState>(rig).expect("state"),
        prior_state,
        "a refused reset must preserve the prior gait state"
    );
    app.world_mut().entity_mut(rig).remove::<GaitVelocity>();
    app.update();
    let after = max_foot_offset_from_neutral(&app, rig, &skeleton);
    println!(
        "refused reset: offset before {before:.3} m, after the finite frame {after:.4} m, issue on the bad frame {refused:?}"
    );
    assert_eq!(
        refused,
        GaitRigIssue::Step,
        "the non-finite velocity was not reported"
    );
    assert!(
        after < 1.0e-3,
        "reset left feet {after:.3} m off their bind-pose neutral after the refused frame"
    );
}
