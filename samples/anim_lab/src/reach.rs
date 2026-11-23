// SPDX-License-Identifier: MIT-0
//! The moving-target reach case (spec 17 § 6, ADR-7216): two-bone IK on an arm and look-at on a
//! head follow targets that move every frame, and a turret barrel follows a moving `AimTarget`.
//!
//! Needs no downloaded content: the rigs are built here and the graphs are the sample's checked-in
//! `assets/reach.animgraph.ron` and `assets/turret.animgraph.ron`, read through the graph loader.
//! Fixed 16 ms frames and target paths indexed by frame number, so every run is the same. The
//! `anim_lab` binary prints the report; `tests/reach_acceptance.rs` asserts its bounds.
//!
//! Every frame the targets are set as graph parameters, the app updates once, and the error is
//! read from that frame's [`JointGlobals`]: the solvers read the parameters of the frame they run
//! in, so a moving target is reached in the same frame.

use std::sync::Arc;
use std::time::Duration;

use regia_animation::components::spawn_skeleton;
use regia_animation::graph::AnimGraph;
use regia_animation::skeleton::LocalBind;
use regia_animation::{
    AnimationClip, AnimationPlugin, Animator, ClipLibrary, JointGlobals, Skeleton, SkinnedMesh,
};
use regia_app::{App, MinimalPlugins, TimeUpdateStrategy};
use regia_core::math::{Mat4, Vec3};
use regia_ecs::prelude::Entity;
use regia_scene::Transform;

/// The character graph: `TwoBoneIK(upper_arm, fore_arm, hand)` over `LookAt(head)` over `Idle`.
pub const REACH_GRAPH_RON: &str = include_str!("../assets/reach.animgraph.ron");
/// The turret graph: `LookAt(barrel)` on `AimTarget` over `Idle`.
pub const TURRET_GRAPH_RON: &str = include_str!("../assets/turret.animgraph.ron");

/// Frames each case runs (about 3.2 s of 16 ms frames; the paths cover a full cycle).
pub const FRAMES: u32 = 200;
/// The acceptance bound: 1 mm, as a hand-to-target distance (IK) or as the distance from the
/// target to the aimed joint's −Z ray (look-at, turret).
pub const MISS_BOUND: f32 = 1e-3;

/// One cycle of a target path, in frames.
const CYCLE: f32 = 180.0;

/// What the character case measured.
#[derive(Clone, Debug, PartialEq)]
pub struct ReachReport {
    /// Frames run with moving targets.
    pub frames: u32,
    /// Largest hand-to-target distance over the frames, meters.
    pub ik_worst: f32,
    /// The frame of [`ReachReport::ik_worst`].
    pub ik_worst_frame: u32,
    /// Largest distance from the look target to the head's −Z ray, meters.
    pub look_worst: f32,
    /// The frame of [`ReachReport::look_worst`].
    pub look_worst_frame: u32,
    /// Length of the hand target's path, meters.
    pub hand_travel: f32,
    /// Largest angle between the head's aim on the first frame and on any later frame, radians.
    pub look_sweep: f32,
    /// Control: the hand-to-target distance on one more frame with `IkWeight` 0, meters.
    pub ik_off_miss: f32,
    /// Control: the look-target-to-ray distance on that frame with `LookWeight` 0, meters.
    pub look_off_miss: f32,
}

/// What the turret case measured.
#[derive(Clone, Debug, PartialEq)]
pub struct TurretReport {
    /// Frames run with a moving target.
    pub frames: u32,
    /// Largest distance from `AimTarget` to the barrel's −Z ray, meters.
    pub worst: f32,
    /// The frame of [`TurretReport::worst`].
    pub worst_frame: u32,
    /// Largest angle between the barrel's aim on the first frame and on any later frame, radians.
    pub sweep: f32,
}

/// The character graph from [`REACH_GRAPH_RON`].
///
/// # Errors
///
/// The graph loader's error, as text.
pub fn reach_graph() -> Result<AnimGraph, String> {
    AnimGraph::from_ron_str(REACH_GRAPH_RON).map_err(|e| format!("reach.animgraph.ron: {e}"))
}

/// The turret graph from [`TURRET_GRAPH_RON`].
///
/// # Errors
///
/// The graph loader's error, as text.
pub fn turret_graph() -> Result<AnimGraph, String> {
    AnimGraph::from_ron_str(TURRET_GRAPH_RON).map_err(|e| format!("turret.animgraph.ron: {e}"))
}

fn bind(translation: Vec3) -> LocalBind {
    LocalBind {
        translation,
        ..LocalBind::default()
    }
}

/// The character rig: `root`, `spine` (1 m up), `head` (0.5 m above the spine, facing −Z) and a
/// right arm `upper_arm` (shoulder at 0.2 m right, 0.3 m up from the spine), `fore_arm` and `hand`
/// (0.3 m each along +X). Reach 0.6 m.
///
/// # Errors
///
/// The skeleton builder's error, as text.
pub fn character_skeleton() -> Result<Skeleton, String> {
    Skeleton::from_local_bind(
        ["root", "spine", "head", "upper_arm", "fore_arm", "hand"]
            .iter()
            .map(|n| (*n).to_owned())
            .collect(),
        vec![None, Some(0), Some(1), Some(1), Some(3), Some(4)],
        vec![
            LocalBind::default(),
            bind(Vec3::new(0.0, 1.0, 0.0)),
            bind(Vec3::new(0.0, 0.5, 0.0)),
            bind(Vec3::new(0.2, 0.3, 0.0)),
            bind(Vec3::new(0.3, 0.0, 0.0)),
            bind(Vec3::new(0.3, 0.0, 0.0)),
        ],
    )
    .map_err(|e| e.to_string())
}

/// The turret rig of the lab: `base`, `yaw` (0.2 m up) and `barrel` (0.5 m forward, facing −Z).
///
/// # Errors
///
/// The skeleton builder's error, as text.
pub fn turret_skeleton() -> Result<Skeleton, String> {
    Skeleton::from_local_bind(
        vec!["base".into(), "yaw".into(), "barrel".into()],
        vec![None, Some(0), Some(1)],
        vec![
            LocalBind::default(),
            bind(Vec3::new(0.0, 0.2, 0.0)),
            bind(Vec3::new(0.0, 0.0, -0.5)),
        ],
    )
    .map_err(|e| e.to_string())
}

/// The shoulder's bind position (the hand path's anchor).
const SHOULDER: Vec3 = Vec3::new(0.2, 1.3, 0.0);
/// The head's bind position (the look path's anchor).
const HEAD: Vec3 = Vec3::new(0.0, 1.5, 0.0);
/// The barrel's bind position (the aim path's anchor).
const BARREL: Vec3 = Vec3::new(0.0, 0.2, -0.5);

fn phase(frame: u32) -> f32 {
    std::f32::consts::TAU * frame as f32 / CYCLE
}

/// The hand target at `frame`: a loop 0.13 m around a point 0.34 m from the shoulder, so it stays
/// between 0.2 m and 0.47 m from the shoulder (inside the 0.6 m reach, never at the root).
pub fn hand_target(frame: u32) -> Vec3 {
    let a = phase(frame);
    SHOULDER + Vec3::new(0.25, -0.1, -0.2) + Vec3::new(0.13 * a.cos(), 0.13 * a.sin(), 0.06 * (2.0 * a).sin())
}

/// The elbow pole: below and behind the shoulder, fixed.
pub const POLE: Vec3 = Vec3::new(0.2, 0.8, 0.3);

/// The look target at `frame`: 2 m ahead of the head, sweeping ±1.5 m sideways and ±0.4 m up and
/// down (within the graph's 1.2 rad yaw and 1.0 rad pitch limits).
pub fn look_target(frame: u32) -> Vec3 {
    let a = phase(frame);
    HEAD + Vec3::new(1.5 * a.sin(), 0.4 * (2.0 * a).sin(), -2.0)
}

/// The turret target at `frame`: 3 m from the barrel, yawing ±1.1 rad and pitching about ±0.3 rad
/// (within the graph's 1.4 rad yaw and 0.8 rad pitch limits).
pub fn aim_target(frame: u32) -> Vec3 {
    let a = phase(frame);
    let yaw = 1.1 * a.sin();
    BARREL + 3.0 * Vec3::new(yaw.sin(), 0.3 * (2.0 * a).sin(), -yaw.cos())
}

fn new_app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AnimationPlugin));
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app
}

fn spawn(app: &mut App, skeleton: Skeleton, graph: AnimGraph) -> Entity {
    let mut library = ClipLibrary::new();
    library.insert(AnimationClip::empty("Idle", 1.0));
    let rig = app.world_mut().spawn(Transform::default()).id();
    let binding = spawn_skeleton(app.world_mut(), Arc::new(skeleton));
    // `Animator::graph` starts with no parameters set (an absent float reads 0); the fixture's
    // declared defaults (IK and look weights 1, the pole) are seeded explicitly.
    let defaults = graph.default_parameters();
    let mut animator = Animator::graph(graph, library);
    animator.parameters = defaults;
    app.world_mut().entity_mut(rig).insert((binding, animator));
    rig
}

fn set(
    app: &mut App,
    rig: Entity,
    name: &str,
    value: impl Into<regia_animation::params::ParamValue>,
) -> Result<(), String> {
    let mut animator = app.world_mut().get_mut::<Animator>(rig).ok_or("no animator")?;
    animator.set(name, value);
    Ok(())
}

fn global(app: &App, rig: Entity, name: &str) -> Result<Mat4, String> {
    let index = app
        .world()
        .get::<SkinnedMesh>(rig)
        .ok_or("rig has no skeleton")?
        .skeleton
        .joint_index(name)
        .ok_or_else(|| format!("missing joint {name}"))?;
    app.world()
        .get::<JointGlobals>(rig)
        .ok_or("rig has no joint globals")?
        .globals
        .get(index as usize)
        .copied()
        .ok_or_else(|| format!("joint {name} has no global"))
}

/// The aimed joint's −Z direction and the distance from `target` to that ray (the full distance
/// when the target is behind the joint).
fn aim_miss(joint: Mat4, target: Vec3) -> (Vec3, f32) {
    let aim = joint.transform_vector3(Vec3::NEG_Z).normalize_or_zero();
    let to_target = target - joint.w_axis.truncate();
    let along = to_target.dot(aim);
    if along <= 0.0 {
        return (aim, to_target.length());
    }
    (aim, (to_target - aim * along).length())
}

/// Runs the character case for `frames` frames of moving targets, then one control frame with
/// both weights at 0.
///
/// # Errors
///
/// A missing joint, animator or joint global, or a fixture that does not load, as text.
pub fn run_character(frames: u32) -> Result<ReachReport, String> {
    let mut app = new_app();
    let rig = spawn(&mut app, character_skeleton()?, reach_graph()?);
    set(&mut app, rig, "Pole", POLE)?;
    let mut report = ReachReport {
        frames,
        ik_worst: 0.0,
        ik_worst_frame: 0,
        look_worst: 0.0,
        look_worst_frame: 0,
        hand_travel: 0.0,
        look_sweep: 0.0,
        ik_off_miss: 0.0,
        look_off_miss: 0.0,
    };
    let mut first_aim = None;
    for frame in 0..frames {
        let (hand, look) = (hand_target(frame), look_target(frame));
        set(&mut app, rig, "HandTarget", hand)?;
        set(&mut app, rig, "LookTarget", look)?;
        app.update();
        let miss = (global(&app, rig, "hand")?.w_axis.truncate() - hand).length();
        if miss > report.ik_worst || !miss.is_finite() {
            report.ik_worst = miss;
            report.ik_worst_frame = frame;
        }
        let (aim, look_miss) = aim_miss(global(&app, rig, "head")?, look);
        if look_miss > report.look_worst || !look_miss.is_finite() {
            report.look_worst = look_miss;
            report.look_worst_frame = frame;
        }
        let first = *first_aim.get_or_insert(aim);
        report.look_sweep = report.look_sweep.max(first.angle_between(aim));
        if frame > 0 {
            report.hand_travel += (hand - hand_target(frame - 1)).length();
        }
    }
    let (hand, look) = (hand_target(frames), look_target(frames));
    set(&mut app, rig, "HandTarget", hand)?;
    set(&mut app, rig, "LookTarget", look)?;
    set(&mut app, rig, "IkWeight", 0.0_f32)?;
    set(&mut app, rig, "LookWeight", 0.0_f32)?;
    app.update();
    report.ik_off_miss = (global(&app, rig, "hand")?.w_axis.truncate() - hand).length();
    report.look_off_miss = aim_miss(global(&app, rig, "head")?, look).1;
    Ok(report)
}

/// Runs the turret case for `frames` frames of a moving `AimTarget`.
///
/// # Errors
///
/// A missing joint, animator or joint global, or a fixture that does not load, as text.
pub fn run_turret(frames: u32) -> Result<TurretReport, String> {
    let mut app = new_app();
    let rig = spawn(&mut app, turret_skeleton()?, turret_graph()?);
    let mut report = TurretReport {
        frames,
        worst: 0.0,
        worst_frame: 0,
        sweep: 0.0,
    };
    let mut first_aim = None;
    for frame in 0..frames {
        let target = aim_target(frame);
        set(&mut app, rig, "AimTarget", target)?;
        app.update();
        let (aim, miss) = aim_miss(global(&app, rig, "barrel")?, target);
        if miss > report.worst || !miss.is_finite() {
            report.worst = miss;
            report.worst_frame = frame;
        }
        let first = *first_aim.get_or_insert(aim);
        report.sweep = report.sweep.max(first.angle_between(aim));
    }
    Ok(report)
}

/// The lab's line for both cases (the binary prints it).
///
/// # Errors
///
/// As [`run_character`] and [`run_turret`], or a bound that does not hold.
pub fn run() -> Result<String, String> {
    let character = run_character(FRAMES)?;
    let turret = run_turret(FRAMES)?;
    if character.ik_worst > MISS_BOUND || character.look_worst > MISS_BOUND || turret.worst > MISS_BOUND {
        return Err(format!(
            "moving targets missed: IK {:.6} m, look-at {:.6} m, turret {:.6} m (bound {MISS_BOUND} m)",
            character.ik_worst, character.look_worst, turret.worst
        ));
    }
    Ok(format!(
        "anim_lab: moving targets over {FRAMES} frames: IK worst {:.2e} m, look-at worst {:.2e} m, turret worst {:.2e} m",
        character.ik_worst, character.look_worst, turret.worst
    ))
}
