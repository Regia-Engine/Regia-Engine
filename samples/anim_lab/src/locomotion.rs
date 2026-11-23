// SPDX-License-Identifier: MIT-0
//! The locomotion scene (spec 17 § 6 "walker gait", gaps G11, F10, W3): procedural legs over
//! rough ground, with no clip authored for the ground and no downloaded content.
//!
//! Two characters share one course, each driven by a scripted mover (the "simulation body" of
//! the user guide's "Legged units: sim body, presentation feet"):
//!
//! * a **hexapod** with a [`GaitRig`] (driven mode, alternating tripod, body posture on) walks
//!   along world −Z on the left half of the course (x < 0): flat ground, three 8 cm steps up, a
//!   landing, a 10° slope up and a flat top. It then stops, turns in place and stops again;
//! * a **biped** (the built-in demo humanoid playing its in-place walk clip) with a
//!   [`FootPlacement`] that has heel and toe probes walks laps on the right half (x > 0): up ten
//!   stairs (0.18 m rise, 0.28 m run), then it is moved back to the bottom (a respawn, which
//!   clears the planter's pins) and walks up again. The walk clip goes straight, so the script
//!   never turns the biped while it walks. (Walking down is not in the acceptance yet: see
//!   ADR-8807.)
//!
//! The course is one [`GroundProbe`] ([`Course`]), installed as the scene's `GroundProbes`. The
//! script is a function of the frame number ([`hexapod_pose`], [`biped_pose`]) applied by the
//! [`drive_scripted_walkers`] system before the animation systems run, and every frame advances
//! the clock by exactly [`DT`], so every run gives the same numbers on one target. The `anim_lab`
//! binary prints the report; `tests/locomotion_acceptance.rs` asserts it and
//! `tests/locomotion_agent.rs` drives the same scene through the agent protocol.
//!
//! Presentation only: none of this is read by a simulation checksum.

use std::sync::Arc;
use std::time::Duration;

use regia_animation::components::spawn_skeleton;
use regia_animation::demo_rig::{
    DemoGait, DemoRig, RigNaming, demo_humanoid, demo_in_place, demo_locomotion,
};
use regia_animation::foot_ik::{
    FootLegDesc, FootPlacement, FootPlacementStatus, FootPlantReport, FootShapeSettings, GroundHit,
    GroundProbe, GroundProbes,
};
use regia_animation::gait::{
    GaitParams, GaitPatternSet, GaitPosture, GaitState, MAX_LEGS, TRIPOD_LEGS, leg_array,
};
use regia_animation::gait_rig::{GaitLeg, GaitRig, GaitRigStatus, GaitVelocity};
use regia_animation::skeleton::LocalBind;
use regia_animation::{AnimationClip, AnimationPlugin, Animator, JointGlobals, Skeleton};
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_scene::Transform;
use regia_scene::components::Name;

/// Fixed frame time, seconds (60 Hz).
pub const DT: f32 = 1.0 / 60.0;

/// Frames the scene's script runs: the hexapod's whole course, turn and settle (about 23 s).
pub const FRAMES: u32 = HEX_TURN_END + HEX_REST_FRAMES;

/// Frames measured after spawn before the acceptance reads a frame (the rigs resolve their legs
/// and the foot planter finds its first contacts).
pub const WARM_UP_FRAMES: u32 = 30;

/// World X of the hexapod's lane (the course left of x = 0 is the hexapod's).
pub const HEXAPOD_X: f32 = -2.0;
/// World X of the biped's uphill lane.
pub const BIPED_UP_X: f32 = 2.0;
/// Frames after a respawn that the biped's measurements skip (the planter finds its contacts
/// again, as after spawn).
pub const BIPED_RESPAWN_SETTLE: u32 = WARM_UP_FRAMES;

/// Rise of one of the hexapod's steps, meters.
pub const HEX_STEP_RISE: f32 = 0.08;
/// Run of one of the hexapod's steps, meters.
pub const HEX_STEP_RUN: f32 = 0.4;
/// Number of the hexapod's steps.
pub const HEX_STEPS: f32 = 3.0;
/// Where (s = −z) the hexapod's first step starts.
pub const HEX_STEPS_START: f32 = 1.0;
/// Where the hexapod's slope starts and ends (s = −z).
pub const HEX_SLOPE: [f32; 2] = [3.0, 6.0];
/// The hexapod's slope, degrees.
pub const HEX_SLOPE_DEGREES: f32 = 10.0;

/// Rise of one stair, meters.
pub const STAIR_RISE: f32 = 0.18;
/// Run of one stair, meters.
pub const STAIR_RUN: f32 = 0.28;
/// Number of stairs (risers at s = 0, 0.28, … 2.52; the landing follows).
pub const STAIRS: f32 = 10.0;

/// The hexapod's walking speed, meters per second.
pub const HEX_SPEED: f32 = 0.4;
/// The hexapod's yaw rate while it turns in place, radians per second.
pub const HEX_TURN_RATE: f32 = 0.5;
/// Where the hexapod starts and stops walking (s = −z).
pub const HEX_WALK: [f32; 2] = [-0.5, 6.6];

/// Frame the hexapod starts walking.
pub const HEX_WALK_START: u32 = WARM_UP_FRAMES;
/// Frame the hexapod stops (it reached `HEX_WALK[1]`).
pub const HEX_WALK_END: u32 = HEX_WALK_START + ((HEX_WALK[1] - HEX_WALK[0]) / HEX_SPEED / DT) as u32;
/// Frames the hexapod stands after the walk and after the turn.
pub const HEX_REST_FRAMES: u32 = 120;
/// Frames the hexapod turns in place (one radian).
pub const HEX_TURN_FRAMES: u32 = 120;
/// Frame the hexapod starts turning in place.
pub const HEX_TURN_START: u32 = HEX_WALK_END + HEX_REST_FRAMES;
/// Frame the hexapod stops turning.
pub const HEX_TURN_END: u32 = HEX_TURN_START + HEX_TURN_FRAMES;

/// The hexapod's body joint height over the floor in its bind pose, meters.
const HEX_BODY_HEIGHT: f32 = 0.5;
/// The hexapod's leg pairs along Z, object space (front first: −Z is forward).
const HEX_PAIR_Z: [f32; 3] = [-0.5, 0.0, 0.5];

/// Where (s = −z) the biped's laps start and end on the straight lanes.
const BIPED_LANE: [f32; 2] = [-1.5, 4.0];

/// The ground of the whole course.
///
/// Left of x = 0 (the hexapod's half): flat to s = 1 (s = −z), three steps up
/// ([`HEX_STEP_RISE`] by [`HEX_STEP_RUN`]), a landing, a [`HEX_SLOPE_DEGREES`] slope from
/// s = 3 to s = 6 and a flat top. From x = 0 on (the biped's half): flat to s = 0, then ten
/// stairs ([`STAIR_RISE`] by [`STAIR_RUN`]) and a landing that goes on forever.
#[derive(Clone, Copy, Debug, Default)]
pub struct Course;

impl Course {
    /// Ground height under `(x, z)`.
    #[must_use]
    pub fn height(x: f32, z: f32) -> f32 {
        let s = -z;
        if x < 0.0 {
            let step_top = HEX_STEP_RISE * HEX_STEPS;
            if s < HEX_STEPS_START {
                0.0
            } else if s < HEX_SLOPE[0] {
                (HEX_STEP_RISE * (((s - HEX_STEPS_START) / HEX_STEP_RUN).floor() + 1.0)).min(step_top)
            } else {
                step_top + slope_tan() * (s.min(HEX_SLOPE[1]) - HEX_SLOPE[0])
            }
        } else if s < 0.0 {
            0.0
        } else {
            STAIR_RISE * ((s / STAIR_RUN).floor() + 1.0).min(STAIRS)
        }
    }

    /// Ground normal at `(x, z)` (straight up everywhere but on the slope; risers are never hit
    /// by the vertical probes the engine casts).
    #[must_use]
    pub fn normal(x: f32, z: f32) -> Vec3 {
        let s = -z;
        if x < 0.0 && (HEX_SLOPE[0]..HEX_SLOPE[1]).contains(&s) {
            // The ground rises toward −Z, so the normal leans toward +Z (downhill).
            Vec3::new(0.0, 1.0, slope_tan()).normalize()
        } else {
            Vec3::Y
        }
    }
}

fn slope_tan() -> f32 {
    HEX_SLOPE_DEGREES.to_radians().tan()
}

impl GroundProbe for Course {
    fn probe(&self, from: Vec3, down: Vec3, max_distance: f32, _ignore: Entity) -> Option<GroundHit> {
        // The engine's foot and gait probes are vertical; anything else misses.
        let y = Course::height(from.x, from.z);
        let depth = from.y - y;
        (down == Vec3::NEG_Y && (0.0..=max_distance).contains(&depth)).then(|| GroundHit {
            point: Vec3::new(from.x, y, from.z),
            normal: Course::normal(from.x, from.z),
        })
    }
}

/// Which character a [`ScriptedWalker`] is.
#[derive(Reflect, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[reflect(Default)]
pub enum WalkerKind {
    /// The six-legged [`GaitRig`] walker.
    #[default]
    Hexapod,
    /// The two-legged [`FootPlacement`] walker.
    Biped,
}

/// A character moved by the scene's script ([`drive_scripted_walkers`]). Agents find the two
/// walkers with `scene.query_entities { "component": "ScriptedWalker" }`.
#[derive(Component, Reflect, Clone, Copy, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct ScriptedWalker {
    /// Which character this is.
    pub kind: WalkerKind,
    /// The script frame of this walker's current pose (agents read it with `scene.get_entity`
    /// and step relative to it).
    pub frame: u32,
}

/// The script's clock: the frame the next [`drive_scripted_walkers`] run poses. Agent predicates
/// (`play.expect`, `play.wait_until`) read only a fixed list of resources, so agents read the
/// same frame from each walker's [`ScriptedWalker::frame`] instead.
#[derive(Resource, Reflect, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[reflect(Resource, Default)]
pub struct LocomotionScript {
    /// Frames posed so far.
    pub frame: u32,
}

/// One scripted pose: where the mover puts a character this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScriptPose {
    /// World translation of the rig root (the floor point under the character).
    pub translation: Vec3,
    /// Heading about +Y, radians (0 faces −Z).
    pub yaw: f32,
    /// World velocity, meters per second (what the mover would report).
    pub velocity: Vec3,
}

/// The hexapod's mover at `frame`: stand, walk the course along −Z at [`HEX_SPEED`], stand,
/// turn in place at [`HEX_TURN_RATE`], stand. The root follows the ground under it; the
/// [`GaitRig`] lifts the body to its stance height itself.
#[must_use]
pub fn hexapod_pose(frame: u32) -> ScriptPose {
    let walked = frame.clamp(HEX_WALK_START, HEX_WALK_END) - HEX_WALK_START;
    let s = (HEX_WALK[0] + HEX_SPEED * DT * walked as f32).min(HEX_WALK[1]);
    let turned = frame.clamp(HEX_TURN_START, HEX_TURN_END) - HEX_TURN_START;
    let yaw = HEX_TURN_RATE * DT * turned as f32;
    let walking = (HEX_WALK_START..HEX_WALK_END).contains(&frame);
    let z = -s;
    ScriptPose {
        translation: Vec3::new(HEXAPOD_X, Course::height(HEXAPOD_X, z), z),
        yaw,
        velocity: if walking {
            Vec3::new(0.0, 0.0, -HEX_SPEED)
        } else {
            Vec3::ZERO
        },
    }
}

/// The biped's walk speed: the demo walk clip's authored speed, meters per second.
#[must_use]
pub fn biped_speed() -> f32 {
    walk_gait().map_or(1.3, |g| g.speed)
}

/// The biped's mover at `frame`: laps up the stairs at [`biped_speed`], along −Z on
/// x = [`BIPED_UP_X`] from s = −1.5 to s = 4 (the landing), then a respawn back to s = −1.5 (5.5 m,
/// farther than the planter's 2 m teleport distance, so the pins clear). The root rides the line
/// through the tread middles (where a capsule would sit), as in the stairs acceptance of `foot_ik`.
#[must_use]
pub fn biped_pose(frame: u32) -> ScriptPose {
    let speed = biped_speed();
    let straight = BIPED_LANE[1] - BIPED_LANE[0];
    let s = BIPED_LANE[0] + (speed * DT * frame as f32) % straight;
    ScriptPose {
        translation: Vec3::new(BIPED_UP_X, biped_root_height(s), -s),
        yaw: 0.0,
        velocity: Vec3::NEG_Z * speed,
    }
}

/// Whether the script moves the biped back to the bottom on `frame` (a respawn).
#[must_use]
pub fn biped_respawns(frame: u32) -> bool {
    frame > 0 && biped_pose(frame).translation.z > biped_pose(frame - 1).translation.z
}

/// Whether `frame` is a respawn or one of the [`BIPED_RESPAWN_SETTLE`] frames after one.
#[must_use]
pub fn biped_settling(frame: u32) -> bool {
    (frame.saturating_sub(BIPED_RESPAWN_SETTLE)..=frame).any(biped_respawns)
}

/// The biped's root height at `s` (s = −z): the line through the stair tread middles, flat
/// below the first stair and on the landing.
#[must_use]
pub fn biped_root_height(s: f32) -> f32 {
    (STAIR_RISE * (s / STAIR_RUN + 0.5)).clamp(0.0, STAIR_RISE * STAIRS)
}

/// Moves every [`ScriptedWalker`] to this frame's scripted pose and advances the script.
/// Runs in `Update`, before the animation systems of `PostUpdate`.
pub fn drive_scripted_walkers(
    mut script: ResMut<LocomotionScript>,
    mut walkers: Query<(&mut ScriptedWalker, &mut Transform, Option<&mut GaitVelocity>)>,
) {
    let frame = script.frame;
    for (mut walker, mut transform, velocity) in &mut walkers {
        walker.frame = frame;
        let pose = match walker.kind {
            WalkerKind::Hexapod => hexapod_pose(frame),
            WalkerKind::Biped => biped_pose(frame),
        };
        transform.translation = pose.translation;
        transform.rotation = Quat::from_rotation_y(pose.yaw);
        if let Some(mut velocity) = velocity {
            velocity.linear = pose.velocity;
        }
    }
    script.frame = frame.saturating_add(1);
}

/// The scene's two characters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocomotionScene {
    /// The [`GaitRig`] hexapod.
    pub hexapod: Entity,
    /// The [`FootPlacement`] biped.
    pub biped: Entity,
}

/// The hexapod's settings: an alternating tripod at 0.3 m steps, a real leg reach (so a foot
/// that fell behind would take a forced step and be counted), a foothold search around risers,
/// and a body that follows the ground's height and tilt.
#[must_use]
pub fn hexapod_params() -> GaitParams {
    GaitParams {
        forward_speed: 0.6,
        reverse_speed: 0.3,
        strafe_speed: 0.3,
        acceleration: 2.0,
        step_length: 0.3,
        clearance: HEX_BODY_HEIGHT,
        swing_lift: 0.12,
        max_slope: 0.8,
        idle_speed: 0.02,
        max_yaw_rate: 0.8,
        // Measured from the bind pose by the rig (`auto_hips`).
        hip_offsets: leg_array([[0.0; 2]; TRIPOD_LEGS], [0.0; 2]),
        settle_distance: 0.03,
        settle_period: 0.35,
        turn_step_distance: 0.08,
        turn_step_angle: 0.35,
        ground_band: 0.3,
        foothold_candidates: 16,
        foothold_spacing: 0.12,
        max_reach: [0.85; MAX_LEGS],
        hang_drop: 0.2,
        patterns: GaitPatternSet::tripod(),
        posture: GaitPosture {
            swing_weight: 0.5,
            height_tau: 0.08,
            height_slew: 0.6,
            attitude_weight: 1.0,
            max_pitch: 25f32.to_radians(),
            max_roll: 20f32.to_radians(),
            attitude_tau: 0.1,
        },
    }
}

fn bind_at(translation: Vec3) -> LocalBind {
    LocalBind {
        translation,
        ..LocalBind::default()
    }
}

/// The hexapod rig: `root` at the floor, `body` 0.5 m up, and six two-bone legs `hip{i}` →
/// `knee{i}` → `foot{i}` (pairs from the front, left before right) whose tips rest on the floor.
///
/// # Errors
///
/// The skeleton builder's error, as text.
pub fn hexapod_skeleton() -> Result<Skeleton, String> {
    let mut names = vec!["root".to_owned(), "body".to_owned()];
    let mut parents = vec![None, Some(0)];
    let mut binds = vec![
        LocalBind::default(),
        bind_at(Vec3::new(0.0, HEX_BODY_HEIGHT, 0.0)),
    ];
    for leg in 0..TRIPOD_LEGS {
        let side = if leg % 2 == 0 { -1.0 } else { 1.0 };
        let z = HEX_PAIR_Z[leg / 2];
        let hip = names.len() as u32;
        names.extend([format!("hip{leg}"), format!("knee{leg}"), format!("foot{leg}")]);
        parents.extend([Some(1), Some(hip), Some(hip + 1)]);
        binds.extend([
            bind_at(Vec3::new(side * 0.25, 0.0, z)),
            bind_at(Vec3::new(side * 0.2, 0.15, 0.0)),
            bind_at(Vec3::new(side * 0.15, -0.65, 0.0)),
        ]);
    }
    Skeleton::from_local_bind(names, parents, binds).map_err(|e| format!("hexapod skeleton: {e}"))
}

/// The hexapod's [`GaitRig`]: the legs in pattern order and `body` lifted to the stance height.
#[must_use]
pub fn hexapod_rig() -> GaitRig {
    let legs = (0..TRIPOD_LEGS)
        .map(|i| GaitLeg::new(format!("hip{i}"), format!("foot{i}")))
        .collect();
    GaitRig::new(hexapod_params(), legs).with_body("body")
}

fn walk_gait() -> Option<DemoGait> {
    DemoGait::standard().into_iter().find(|g| g.name == "walk")
}

/// The biped rig: the built-in demo humanoid with game-style joint names.
///
/// # Errors
///
/// When the demo rig cannot be built.
pub fn biped_skeleton() -> Result<Skeleton, String> {
    demo_humanoid(&DemoRig {
        naming: RigNaming::Game,
        ..DemoRig::default()
    })
    .ok_or_else(|| "demo humanoid".to_owned())
}

/// The biped's walk clip, played in place (the script moves the rig).
///
/// # Errors
///
/// When the demo walk cannot be authored on `skeleton`.
pub fn biped_walk_clip(skeleton: &Skeleton) -> Result<AnimationClip, String> {
    let gait = walk_gait().ok_or_else(|| "demo walk gait".to_owned())?;
    let clip = demo_locomotion(skeleton, std::slice::from_ref(&gait))
        .pop()
        .ok_or_else(|| "demo walk clip".to_owned())?;
    Ok(demo_in_place(&clip))
}

/// The biped's [`FootPlacement`]: both legs by name with their toe joints, and heel and toe
/// probes for the stairs.
#[must_use]
pub fn biped_placement() -> FootPlacement {
    let mut placement = FootPlacement::default();
    placement.legs = ["l", "r"]
        .map(|side| FootLegDesc {
            hip: format!("thigh_{side}"),
            knee: format!("calf_{side}"),
            ankle: format!("foot_{side}"),
            toe: Some(format!("ball_{side}")),
            pole_offset: Vec3::ZERO,
        })
        .to_vec();
    placement.foot_shape = Some(FootShapeSettings::default());
    placement
}

/// Adds the course, the script and both characters to `app` (which must have the animation
/// plugin). Registers [`ScriptedWalker`] and [`LocomotionScript`] for reflection.
///
/// # Errors
///
/// When a rig or the walk clip cannot be built.
pub fn add_scene(app: &mut App) -> Result<LocomotionScene, String> {
    app.register_type::<ScriptedWalker>()
        .register_type::<WalkerKind>()
        .register_type::<LocomotionScript>()
        .insert_resource(LocomotionScript::default())
        .insert_resource(GroundProbes::new(Course))
        .add_systems(Update, drive_scripted_walkers);

    let hexapod_skeleton = Arc::new(hexapod_skeleton()?);
    let start = hexapod_pose(0);
    let binding = spawn_skeleton(app.world_mut(), hexapod_skeleton);
    let hexapod = app
        .world_mut()
        .spawn((
            Name::new("hexapod"),
            Transform {
                translation: start.translation,
                rotation: Quat::from_rotation_y(start.yaw),
                scale: Vec3::ONE,
            },
            binding,
            Animator::single(AnimationClip::empty("still", 1.0), true),
            hexapod_rig(),
            GaitVelocity::default(),
            ScriptedWalker {
                kind: WalkerKind::Hexapod,
                frame: 0,
            },
        ))
        .id();

    let biped_skeleton = Arc::new(biped_skeleton()?);
    let clip = biped_walk_clip(&biped_skeleton)?;
    let start = biped_pose(0);
    let binding = spawn_skeleton(app.world_mut(), biped_skeleton);
    let biped = app
        .world_mut()
        .spawn((
            Name::new("biped"),
            Transform {
                translation: start.translation,
                rotation: Quat::from_rotation_y(start.yaw),
                scale: Vec3::ONE,
            },
            binding,
            Animator::single(clip, true),
            biped_placement(),
            ScriptedWalker {
                kind: WalkerKind::Biped,
                frame: 0,
            },
        ))
        .id();
    Ok(LocomotionScene { hexapod, biped })
}

/// A headless app with the scene: minimal plugins, the animation plugin and a clock that
/// advances exactly [`DT`] per update.
///
/// # Errors
///
/// As [`add_scene`].
pub fn headless_app() -> Result<(App, LocomotionScene), String> {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AnimationPlugin));
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(DT)));
    let scene = add_scene(&mut app)?;
    Ok((app, scene))
}

/// What the hexapod did, measured every frame after [`WARM_UP_FRAMES`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HexapodReport {
    /// Largest horizontal movement in one frame of a foot that was planted (and not swinging)
    /// on both frames, meters. A planted foot only follows the ground height, so this is 0.
    pub max_slide: f32,
    /// Largest movement of any foot target in one frame, meters (a pop shows up here).
    pub max_foot_jump: f32,
    /// The frame of [`HexapodReport::max_foot_jump`].
    pub max_foot_jump_frame: u32,
    /// Largest distance from a leg tip to its foot target after the leg solve, meters.
    pub max_tip_error: f32,
    /// Forced (out of pattern) steps over the run: a foot that fell out of reach.
    pub forced_steps: u32,
    /// Refused landings over the run (each moved to a foothold nearby).
    pub refused: u32,
    /// Replants (teleports of the body) over the run.
    pub replants: u32,
    /// Fewest feet planted on any measured frame.
    pub min_planted: usize,
    /// Largest height error of a planted foot over the ground under it, meters.
    pub max_planted_height_error: f32,
    /// Largest body pitch over the run, radians (positive raises the nose; the slope tilts it).
    pub max_pitch: f32,
    /// Body height over the root's ground at the end, meters.
    pub final_body_height: f32,
    /// Ground height under the root at the end, meters (how far the hexapod climbed).
    pub climbed: f32,
    /// Whether the feet were settled at the end.
    pub settled: bool,
    /// The rig's status at the end.
    pub status: Option<GaitRigStatus>,
}

/// What the biped did, measured every frame after [`WARM_UP_FRAMES`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BipedReport {
    /// Largest `FootPlantReport::max_slide` over the run, meters: how far a planted foot's
    /// world target moved since its stance began.
    pub max_slide: f32,
    /// The frame of [`BipedReport::max_slide`].
    pub max_slide_frame: u32,
    /// Largest movement of an ankle joint in one frame, meters (a pop shows up here), not
    /// counting the frames the script respawns the biped on the other lane.
    pub max_foot_jump: f32,
    /// The frame of [`BipedReport::max_foot_jump`].
    pub max_foot_jump_frame: u32,
    /// Respawns measured (back to the bottom; each clears the planter's pins).
    pub respawns: u32,
    /// Largest heel or toe gap of a planted foot (over the tread), meters.
    pub max_gap: f32,
    /// The frame of [`BipedReport::max_gap`].
    pub max_gap_frame: u32,
    /// Smallest heel or toe gap of a planted foot, meters (negative: under the tread).
    pub min_gap: f32,
    /// Leg-frames measured with the foot planted.
    pub planted_frames: u32,
    /// Leg-frames planted on a tread above the first stair (the feet really were on the stairs).
    pub planted_on_stairs: u32,
    /// Most negative pelvis offset over the run, meters.
    pub min_pelvis_offset: f32,
    /// Whether the foot placement found the probe and resolved both legs at the end.
    pub status: Option<FootPlacementStatus>,
}

/// What [`run`] measured.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LocomotionReport {
    /// Frames run.
    pub frames: u32,
    /// The hexapod.
    pub hexapod: HexapodReport,
    /// The biped.
    pub biped: BipedReport,
}

fn world_joint(app: &App, rig: Entity, joint: u32) -> Option<Vec3> {
    let globals = app.world().get::<JointGlobals>(rig)?;
    let local = globals.globals.get(joint as usize)?.w_axis.truncate();
    let t = app.world().get::<Transform>(rig)?;
    Some(t.translation + t.rotation * (local * t.scale))
}

fn joint(skeleton: &Skeleton, name: &str) -> Result<u32, String> {
    skeleton.joint_index(name).ok_or_else(|| format!("joint {name}"))
}

/// Runs the scene headless for `frames` frames and measures both characters.
///
/// # Errors
///
/// When the scene cannot be built or a measured component is missing.
pub fn run(frames: u32) -> Result<LocomotionReport, String> {
    let (mut app, scene) = headless_app()?;
    let hex_skeleton = hexapod_skeleton()?;
    let hex_tips: Vec<u32> = (0..TRIPOD_LEGS)
        .map(|i| joint(&hex_skeleton, &format!("foot{i}")))
        .collect::<Result<_, _>>()?;
    let biped_skeleton = biped_skeleton()?;
    let ankles = [
        joint(&biped_skeleton, "foot_l")?,
        joint(&biped_skeleton, "foot_r")?,
    ];

    let mut report = LocomotionReport {
        frames,
        hexapod: HexapodReport {
            min_planted: TRIPOD_LEGS,
            ..HexapodReport::default()
        },
        ..LocomotionReport::default()
    };
    let mut last_gait: Option<GaitState> = None;
    let mut last_ankles: Option<[Vec3; 2]> = None;
    for frame in 0..frames {
        app.update();
        let gait = *app
            .world()
            .get::<GaitState>(scene.hexapod)
            .ok_or_else(|| format!("frame {frame}: no GaitState"))?;
        let ankle_now = [
            world_joint(&app, scene.biped, ankles[0]).ok_or("biped ankle")?,
            world_joint(&app, scene.biped, ankles[1]).ok_or("biped ankle")?,
        ];
        if frame >= WARM_UP_FRAMES {
            measure_hexapod(
                &app,
                scene.hexapod,
                &hex_tips,
                &gait,
                last_gait.as_ref(),
                frame,
                &mut report.hexapod,
            )?;
            let plant = app
                .world()
                .get::<FootPlantReport>(scene.biped)
                .ok_or_else(|| format!("frame {frame}: no FootPlantReport"))?;
            measure_biped(plant, &ankle_now, last_ankles.as_ref(), frame, &mut report.biped);
        }
        last_gait = Some(gait);
        last_ankles = Some(ankle_now);
    }
    let hex = &mut report.hexapod;
    if let Some(gait) = last_gait {
        hex.forced_steps = gait.forced_steps;
        hex.refused = gait.refused;
        hex.replants = gait.replants;
        hex.settled = gait.settled;
        let root = app
            .world()
            .get::<Transform>(scene.hexapod)
            .map(|t| t.translation)
            .ok_or("hexapod transform")?;
        hex.climbed = Course::height(root.x, root.z);
        hex.final_body_height = gait.position[1] - hex.climbed;
    }
    hex.status = app.world().get::<GaitRigStatus>(scene.hexapod).copied();
    report.biped.status = app.world().get::<FootPlacementStatus>(scene.biped).copied();
    Ok(report)
}

fn measure_hexapod(
    app: &App,
    rig: Entity,
    tips: &[u32],
    now: &GaitState,
    before: Option<&GaitState>,
    frame: u32,
    out: &mut HexapodReport,
) -> Result<(), String> {
    let mut planted = 0;
    for (i, foot) in now.feet().iter().enumerate() {
        let p = Vec3::from_array(foot.position);
        if foot.planted {
            planted += 1;
            let ground = Course::height(p.x, p.z);
            out.max_planted_height_error = out.max_planted_height_error.max((p.y - ground).abs());
        }
        if let Some(before) = before {
            let b = before.feet[i];
            let d = p - Vec3::from_array(b.position);
            if d.length() > out.max_foot_jump {
                out.max_foot_jump = d.length();
                out.max_foot_jump_frame = frame;
            }
            if b.planted && foot.planted && !before.is_swinging(i) && !now.is_swinging(i) {
                out.max_slide = out.max_slide.max(Vec3::new(d.x, 0.0, d.z).length());
            }
        }
        let tip = world_joint(app, rig, tips[i]).ok_or("hexapod tip")?;
        out.max_tip_error = out.max_tip_error.max(tip.distance(p));
    }
    out.min_planted = out.min_planted.min(planted);
    if now.pitch.is_finite() {
        out.max_pitch = out.max_pitch.max(now.pitch);
    }
    Ok(())
}

fn measure_biped(
    plant: &FootPlantReport,
    ankles: &[Vec3; 2],
    before: Option<&[Vec3; 2]>,
    frame: u32,
    out: &mut BipedReport,
) {
    if biped_respawns(frame) {
        out.respawns += 1;
    }
    if biped_settling(frame) {
        return;
    }
    if plant.max_slide > out.max_slide {
        out.max_slide = plant.max_slide;
        out.max_slide_frame = frame;
    }
    out.min_pelvis_offset = out.min_pelvis_offset.min(plant.pelvis_offset);
    for (leg, ankle) in ankles.iter().enumerate() {
        if let Some(before) = before {
            let jump = ankle.distance(before[leg]);
            if jump > out.max_foot_jump {
                out.max_foot_jump = jump;
                out.max_foot_jump_frame = frame;
            }
        }
        if plant.planted.get(leg).copied().unwrap_or(0.0) < 0.5 {
            continue;
        }
        out.planted_frames += 1;
        let heel = plant.heel_gap.get(leg).copied().unwrap_or(0.0);
        let toe = plant.toe_gap.get(leg).copied().unwrap_or(0.0);
        if heel.max(toe) > out.max_gap {
            out.max_gap = heel.max(toe);
            out.max_gap_frame = frame;
        }
        out.min_gap = out.min_gap.min(heel).min(toe);
        if Course::height(ankle.x, ankle.z) >= 2.0 * STAIR_RISE - 1.0e-4 {
            out.planted_on_stairs += 1;
        }
    }
}

/// Runs [`FRAMES`] frames and formats the report as the `anim_lab` binary prints it.
///
/// # Errors
///
/// As [`run`].
pub fn summary() -> Result<String, String> {
    let r = run(FRAMES)?;
    let (h, b) = (&r.hexapod, &r.biped);
    Ok(format!(
        "anim_lab: locomotion {} frames: hexapod climbed {:.3} m, max slide {:.6} m, max foot jump {:.4} m, \
         max tip error {:.6} m, forced steps {}, refused {}; biped max slide {:.4} m, max foot jump {:.4} m, \
         heel/toe gap [{:.4}, {:.4}] m over {} planted leg-frames ({} on the stairs)",
        r.frames,
        h.climbed,
        h.max_slide,
        h.max_foot_jump,
        h.max_tip_error,
        h.forced_steps,
        h.refused,
        b.max_slide,
        b.max_foot_jump,
        b.min_gap,
        b.max_gap,
        b.planted_frames,
        b.planted_on_stairs,
    ))
}
