// SPDX-License-Identifier: MPL-2.0
//! [`FootPlacement`]: the engine plants feet, instead of the host calling [`FootPlanter`] by hand.
//!
//! A rig with this component is probed, planned and applied every frame. [`FootPlantReport`] is
//! inserted by the engine (hosts do not write it). [`FootPlacementStatus`] says whether a
//! [`GroundProbes`] resource is installed and whether the leg names resolved.
//!
//! Two output modes:
//!
//! * [`FootPlacementOutput::Pose`] runs inside `apply_animation`, after the ragdoll blend and
//!   before eye gaze, and poses the skeleton with [`apply_foot_plant`](super::apply_foot_plant).
//! * [`FootPlacementOutput::GraphParams`] runs in `Update` before `evaluate_animators`, because a
//!   graph's `TwoBoneIK` nodes read their targets while the graph evaluates. The written target
//!   drops the bind tip height, so the tip joint meets the contact. Pose output keeps that
//!   height: the sole sits one bind height below the ankle. The rig entity is moved by the
//!   pelvis offset; the host places the rig without last frame's pelvis first (the third-person
//!   sample's `drive_rig` does). The foot tilt and toe bend, which a `TwoBoneIK` node cannot
//!   express, are applied after evaluation inside `apply_animation`, in the pose-output slot, on
//!   the copy the renderer reads (ADR-10130); flat feet with no toe bend skip that copy.
//!
//! `regia_animation` does not depend on physics. The host installs a [`GroundProbe`] in
//! [`GroundProbes`]. With no probe the effect fades out and the feet stay on the clip
//! (`foot_probe_missing`). A leg name that is not on the skeleton skips planting
//! (`foot_legs_unresolved`).
//!
//! [`FootPlacement::foot_shape`] turns on heel and toe probes (module [`shape`](super::shape)):
//! pitch from the heel and toe heights, the stair rule and, when the leg names a toe joint that
//! resolves, the toe bend. `None` (the default) plans every leg from the ankle probe alone.
//!
//! [`FootPlacement::lod`] is the IK level of detail ([`crate::ik_lod`]): full detail probes and
//! plans every frame, reduced detail probes every few frames and reuses the last hits, off fades
//! the effect out on the cached hits and then leaves the clip pose. The level is reported in
//! [`FootPlacementStatus::lod`]. A settled frame allocates nothing.
//!
//! Presentation only: nothing here is read by a simulation checksum.

use regia_core::math::{Mat4, Quat, Vec3};
use regia_ecs::prelude::*;
use regia_scene::Transform;

use super::plant::{
    ApplyBuffers, FootPlant, FootPlantInput, FootPlantLeg, FootPlantReport, FootPlantSettings, FootPlanter,
    apply_foot_plant_into, apply_foot_plant_with_toes_into, apply_foot_turns_into, plan_turns_feet,
};
use super::shape::{FootShape, FootShapeSettings, FootSole, foot_shape_from_bind, probe_foot_sole};
use super::{FootGround, FootIkLeg};
use crate::components::{Animator, JointGlobals, SkinnedMesh};
use crate::ik::{IkChain, refresh_global};
use crate::ik_lod::{IkLod, IkLodSettings, LodCadence, camera_distance};
use crate::montage_player::MontagePlayer;
use crate::params::ParamValue;
use crate::pose::Pose;
use crate::skeleton::Skeleton;

/// A ground hit in world space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GroundHit {
    /// The point on the ground, meters.
    pub point: Vec3,
    /// The ground normal. A zero or downward normal tilts nothing.
    pub normal: Vec3,
}

/// One ground query. Implementations live outside this crate (a physics raycast, an analytic
/// plane in tests). `regia_animation` only stores the trait object.
pub trait GroundProbe: Send + Sync {
    /// Cast from `from` along `down` for at most `max_distance` meters.
    ///
    /// `ignore` is the character (or the rig, when the component has no character): the probe
    /// must not hit that body. `down` need not be a unit vector. `None` when nothing is hit.
    fn probe(&self, from: Vec3, down: Vec3, max_distance: f32, ignore: Entity) -> Option<GroundHit>;

    /// [`GroundProbe::probe`] plus the velocity of the ground at the hit, meters per second, world
    /// space (a moving platform carries a planted foot by it). The default reports still ground.
    fn probe_moving(
        &self,
        from: Vec3,
        down: Vec3,
        max_distance: f32,
        ignore: Entity,
    ) -> Option<(GroundHit, Vec3)> {
        self.probe(from, down, max_distance, ignore)
            .map(|hit| (hit, Vec3::ZERO))
    }
}

/// The scene's ground probe. Absent means feet are not moved (`foot_probe_missing`).
#[derive(Resource)]
pub struct GroundProbes {
    probe: Box<dyn GroundProbe + Send + Sync>,
}

impl GroundProbes {
    /// Install `probe` as the scene probe.
    #[must_use]
    pub fn new(probe: impl GroundProbe + 'static) -> Self {
        Self {
            probe: Box::new(probe),
        }
    }

    /// Forward one query to the installed probe, keeping only a hit that can be ground.
    ///
    /// A hit at the ray origin (a cast that starts inside solid geometry reports time of impact
    /// 0 there), a hit behind it, a back face (its normal points along `down`: the far side of
    /// the shape the cast started in) and a non-finite hit read as no hit, so a foot is never
    /// planted on the probe origin `probe_height` above it.
    #[must_use]
    pub fn probe(&self, from: Vec3, down: Vec3, max_distance: f32, ignore: Entity) -> Option<GroundHit> {
        self.probe
            .probe(from, down, max_distance, ignore)
            .filter(|hit| is_ground_hit(from, down, hit))
    }

    /// [`GroundProbes::probe`] with the ground's velocity at the hit
    /// ([`GroundProbe::probe_moving`]; a non-finite velocity reads as still ground).
    #[must_use]
    pub fn probe_moving(
        &self,
        from: Vec3,
        down: Vec3,
        max_distance: f32,
        ignore: Entity,
    ) -> Option<(GroundHit, Vec3)> {
        self.probe
            .probe_moving(from, down, max_distance, ignore)
            .filter(|(hit, _)| is_ground_hit(from, down, hit))
            .map(|(hit, velocity)| {
                (
                    hit,
                    if velocity.is_finite() {
                        velocity
                    } else {
                        Vec3::ZERO
                    },
                )
            })
    }
}

/// Shortest travel, meters, for a probe hit to count as ground (a hit at the origin is a cast
/// that started inside the geometry).
const MIN_GROUND_TRAVEL: f32 = 1.0e-4;

/// Whether `hit`, from a cast at `from` along `down`, is a ground surface the foot can stand on.
fn is_ground_hit(from: Vec3, down: Vec3, hit: &GroundHit) -> bool {
    let dir = down.normalize_or_zero();
    if dir == Vec3::ZERO || !hit.point.is_finite() || !hit.normal.is_finite() || !from.is_finite() {
        return false;
    }
    let travel = (hit.point - from).dot(dir);
    travel > MIN_GROUND_TRAVEL && hit.normal.dot(dir) <= 0.0
}

/// One leg, named the way the skeleton names its joints.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Default)]
pub struct FootLegDesc {
    /// Hip (chain root) joint name.
    pub hip: String,
    /// Knee (chain mid) joint name.
    pub knee: String,
    /// Ankle (chain tip) joint name.
    pub ankle: String,
    /// Optional toe joint name. With [`FootPlacement::foot_shape`] set, it places the toe tip
    /// and bends the toe; a name that does not resolve uses `front_length` and never bends.
    pub toe: Option<String>,
    /// Added to the knee position when writing a graph pole, skeleton units. Zero uses the
    /// pose-mode pole (object-space −Z).
    pub pole_offset: Vec3,
}

/// Where the plan is written.
#[derive(Reflect, Clone, Debug, PartialEq, Default)]
#[reflect(Default)]
pub enum FootPlacementOutput {
    /// Pose the skeleton with [`apply_foot_plant`](super::apply_foot_plant) inside `apply_animation`.
    #[default]
    Pose,
    /// Parameter names a graph's `TwoBoneIK` nodes read. `{i}` is the leg index, expanded once
    /// when the names resolve (`"Foot{i}Target"` → `"Foot0Target"`). A pattern with no `{i}`
    /// has the index appended. The target is the planter's object-space target in skeleton
    /// units, lowered by the bind tip height so the tip joint meets the contact. Pose output
    /// keeps that height.
    GraphParams {
        /// Vec3 target parameter pattern.
        target: String,
        /// Float weight parameter pattern. `0` leaves the clip pose (a missing float is also 0
        /// inside `TwoBoneIK`).
        weight: String,
        /// Vec3 pole parameter pattern.
        pole: String,
    },
}

/// One character's feet. The engine probes, plans and applies; the host does not call
/// [`FootPlanter::plan`].
///
/// `grounded`: `None` means always active. `Some(false)` (in the air) fades the effect out at
/// [`FootPlantSettings::blend_out_per_s`]. When a character movement component is present, the
/// framework copies its grounded flag onto this field. This crate does not depend on that
/// component. A host sets `Some(false)` itself when no character state exists.
///
/// The weight curve or parameter ([`FootPlacement::weight_param`], default `FootPlacementWeight`)
/// is the goal. A montage curve of that name wins over the parameter. Absent means goal 1. A
/// goal at or below 0 fades the effect out at the planter's blend rates; any positive goal fades
/// it back in. After the fade the clip pose is unchanged.
#[derive(Component, Reflect, Clone, Debug)]
#[reflect(Component, Default)]
pub struct FootPlacement {
    /// Legs, hip then knee then ankle. Empty, or a missing required name: `legs_resolved` is
    /// false and the feet are not moved (`foot_legs_unresolved`).
    pub legs: Vec<FootLegDesc>,
    /// Planter tuning. Lengths are meters.
    pub settings: FootPlantSettings,
    /// Meters above the foot (or the pin) the probe starts. Default 0.6.
    pub probe_height: f32,
    /// Meters the probe may travel. Default 1.2.
    pub probe_length: f32,
    /// Pose the skeleton, or write graph parameters.
    pub output: FootPlacementOutput,
    /// Float parameter or montage curve that fades the effect. Default `FootPlacementWeight`.
    pub weight_param: String,
    /// `None` = always active. `Some(false)` fades out (airborne, prone, dead).
    pub grounded: Option<bool>,
    /// Character entity whose body the probe ignores, and whose grounded flag the framework
    /// copies. `None` ignores the rig entity itself.
    pub character: Option<Entity>,
    /// A rig translation jump farther than this, meters, clears the pins and the smoothed
    /// offsets. Default 2. A normal step is far smaller. Separate from the planter's
    /// `lock_max_drift`, which only re-pins one foot.
    pub teleport_distance: f32,
    /// Heel and toe probes per leg (stairs, curbs, ramps, toe bend). `None` (the default)
    /// plans each leg from its ankle probe alone.
    pub foot_shape: Option<FootShapeSettings>,
    /// IK level of detail by distance to the [`AnimLodCamera`](crate::AnimLodCamera), or forced by
    /// the host ([`IkLodSettings::force`]). Without a camera the rig stays at full detail.
    pub lod: IkLodSettings,
    /// Planter state. Not reflected.
    #[reflect(ignore)]
    planter: FootPlanter,
    /// Rig translation last frame, for the teleport check.
    #[reflect(ignore)]
    last_translation: Option<Vec3>,
    /// Resolved chains. Rebuilt when the names or the skeleton change.
    #[reflect(ignore)]
    resolved: Vec<ResolvedLeg>,
    /// Skeleton the chains were resolved against.
    #[reflect(ignore)]
    resolved_skeleton: usize,
    /// Parameter patterns the chains were resolved with. Compared as stored text so a settled
    /// frame does not build the expanded names.
    #[reflect(ignore)]
    pattern_target: String,
    /// Weight-parameter pattern captured with `pattern_target`.
    #[reflect(ignore)]
    pattern_weight: String,
    /// Pole-parameter pattern captured with `pattern_target`.
    #[reflect(ignore)]
    pattern_pole: String,
    /// `false` after a failed resolve.
    #[reflect(ignore)]
    legs_ok: bool,
    /// Skeleton-unit IK chains parallel to `resolved`.
    #[reflect(ignore)]
    ik_legs: Vec<FootIkLeg>,
    /// Toe joints parallel to `resolved` (for `apply_foot_plant_with_toes_into`).
    #[reflect(ignore)]
    toe_joints: Vec<Option<u32>>,
    /// Foot shape settings the shapes were resolved with.
    #[reflect(ignore)]
    resolved_shape: Option<FootShapeSettings>,
    /// Joint matrices for the heel-and-toe pose apply, reused across frames.
    #[reflect(ignore)]
    apply_buffers: ApplyBuffers,
    /// Object matrices of the working pose, reused across frames.
    #[reflect(ignore)]
    globals: Vec<Mat4>,
    /// Clip joint positions (skeleton units) captured before graph IK, for the next frame.
    #[reflect(ignore)]
    animated: Vec<(u32, Vec3)>,
    /// Knee positions in skeleton units, parallel to the legs, this frame.
    #[reflect(ignore)]
    mids: Vec<Vec3>,
    /// Leg inputs of a rig with more than eight legs, reused across frames.
    #[reflect(ignore)]
    spill: LegSpill,
    /// Scaled plan passed to `apply_foot_plant_into`, reused.
    #[reflect(ignore)]
    applied: FootPlant,
    /// Report waiting for its first insert (the query cannot insert mid-iteration).
    #[reflect(ignore)]
    pending_report: Option<FootPlantReport>,
    /// Status waiting for its first insert.
    #[reflect(ignore)]
    pending_status: Option<FootPlacementStatus>,
    /// This frame's plan, reused across frames.
    #[reflect(ignore)]
    plan: FootPlant,
    /// The last probe results per leg, reused at reduced and off detail.
    #[reflect(ignore)]
    hits: Vec<LegHit>,
    /// Reduced-detail probe cadence.
    #[reflect(ignore)]
    cadence: LodCadence,
    /// The level chosen on the latest frame.
    #[reflect(ignore)]
    current_lod: IkLod,
}

/// Planter inputs of the legs past the eight-slot stack, kept across frames.
#[derive(Clone, Debug, Default)]
struct LegSpill {
    legs: Vec<FootPlantLeg>,
    soles: Vec<FootSole>,
}

/// One leg's probe results, kept for the frames that do not probe.
#[derive(Clone, Copy, Debug)]
struct LegHit {
    ground: FootGround,
    normal: Vec3,
    sole: FootSole,
    /// The ground's velocity at the hit, meters per second (a moving platform carries the pin).
    velocity: Vec3,
}

impl Default for LegHit {
    fn default() -> Self {
        Self {
            ground: FootGround::None,
            normal: Vec3::Y,
            sole: FootSole::default(),
            velocity: Vec3::ZERO,
        }
    }
}

/// A resolved leg. Names are kept so a rename resolves again without allocating on the
/// unchanged path (the compare borrows the component's strings).
#[derive(Clone, Debug)]
struct ResolvedLeg {
    hip: String,
    knee: String,
    ankle: String,
    toe_name: Option<String>,
    chain: IkChain,
    /// Heel, ball and toe in the ankle joint's space (with [`FootPlacement::foot_shape`]).
    shape: Option<FootShape>,
    /// Thigh plus shin, skeleton units.
    length: f32,
    /// Bind ankle height over the sole, skeleton units.
    ankle_height: f32,
    pole_offset: Vec3,
    target_param: String,
    weight_param: String,
    pole_param: String,
}

impl Default for FootPlacement {
    fn default() -> Self {
        Self {
            legs: Vec::new(),
            settings: FootPlantSettings::default(),
            probe_height: 0.6,
            probe_length: 1.2,
            output: FootPlacementOutput::Pose,
            weight_param: "FootPlacementWeight".to_string(),
            grounded: None,
            character: None,
            teleport_distance: 2.0,
            foot_shape: None,
            lod: IkLodSettings::default(),
            planter: FootPlanter::default(),
            last_translation: None,
            resolved: Vec::new(),
            resolved_skeleton: 0,
            pattern_target: String::new(),
            pattern_weight: String::new(),
            pattern_pole: String::new(),
            legs_ok: false,
            ik_legs: Vec::new(),
            toe_joints: Vec::new(),
            resolved_shape: None,
            apply_buffers: ApplyBuffers::default(),
            globals: Vec::new(),
            animated: Vec::new(),
            mids: Vec::new(),
            spill: LegSpill::default(),
            applied: FootPlant::default(),
            pending_report: None,
            pending_status: None,
            plan: FootPlant::default(),
            hits: Vec::new(),
            cadence: LodCadence::default(),
            current_lod: IkLod::Full,
        }
    }
}

impl FootPlacement {
    /// `true` when the output poses the skeleton inside `apply_animation`.
    #[must_use]
    pub fn is_pose(&self) -> bool {
        matches!(self.output, FootPlacementOutput::Pose)
    }

    /// `true` when the output writes graph parameters before evaluation.
    #[must_use]
    pub fn is_graph(&self) -> bool {
        matches!(self.output, FootPlacementOutput::GraphParams { .. })
    }

    /// Forget pins and smoothed offsets (a teleport or a respawn). The next plan fades in
    /// from weight 0.
    pub fn reset(&mut self) {
        self.planter.reset();
        self.last_translation = None;
    }

    /// The IK level of detail chosen on the latest frame (also in [`FootPlacementStatus::lod`]).
    #[must_use]
    pub fn current_lod(&self) -> IkLod {
        self.current_lod
    }
}

/// Whether a probe is installed and the leg names resolved. Reflected so `scene.get_entity`
/// can read it. `probe_installed` false is `foot_probe_missing`; `legs_resolved` false is
/// `foot_legs_unresolved`.
#[derive(Component, Reflect, Clone, Copy, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct FootPlacementStatus {
    /// A [`GroundProbes`] resource is on the world. Raycasts that miss still count as installed.
    pub probe_installed: bool,
    /// Every required joint name (hip, knee, ankle) resolved on this skeleton.
    pub legs_resolved: bool,
    /// The IK level of detail this frame ([`FootPlacement::lod`]).
    pub lod: IkLod,
}

/// Inserted for one frame when the character respawns (the framework does this when `Died`
/// is removed). The foot system clears the pins and removes the marker the same frame.
#[derive(Component, Reflect, Debug, Default)]
#[reflect(Component, Default)]
pub struct FootPlacementRespawn;

/// What one advance decided (the plan itself is in `FootPlacement::plan`). `planned` is false
/// when the joints are not available yet (the first frames of a graph, before a pose exists) or
/// the rig is at [`IkLod::Off`] with the effect faded out: the feet are not moved that frame.
pub(crate) struct FootAdvance {
    /// See [`FootPlacementStatus::probe_installed`].
    pub probe_installed: bool,
    /// See [`FootPlacementStatus::legs_resolved`].
    pub legs_resolved: bool,
    /// `false` when this frame did not call the planter.
    pub planned: bool,
    /// The level of detail this frame.
    pub lod: IkLod,
}

impl FootAdvance {
    fn status(&self) -> FootPlacementStatus {
        FootPlacementStatus {
            probe_installed: self.probe_installed,
            legs_resolved: self.legs_resolved,
            lod: self.lod,
        }
    }
}

/// Inputs of one pose-mode plant. Bundled so the call stays under the argument lint.
pub(crate) struct PosePlant<'a> {
    /// The rig skeleton.
    pub skeleton: &'a Skeleton,
    /// The working pose (a copy of the evaluated pose).
    pub pose: &'a mut Pose,
    /// The rig entity's transform. Not moved: the pelvis offset goes into the pose.
    pub transform: &'a Transform,
    /// Parameters and the evaluation delta.
    pub animator: &'a Animator,
    /// Playing montages, when the rig has a player.
    pub montage: Option<&'a MontagePlayer>,
    /// The scene probe. `None` fades the effect out (`foot_probe_missing`).
    pub probe: Option<&'a GroundProbes>,
    /// Body the probe must not hit.
    pub ignore: Entity,
    /// Clear the pins before planning (a respawn marker).
    pub respawn: bool,
    /// Filled in place. The caller inserts it on the first frame.
    pub report: &'a mut FootPlantReport,
    /// The [`AnimLodCamera`](crate::AnimLodCamera) position, if any.
    pub camera: Option<Vec3>,
    /// Probe stagger at reduced detail (the rig entity's index).
    pub phase: u32,
}

/// Borrowed inputs of one advance.
struct AdvanceIn<'a> {
    skeleton: &'a Skeleton,
    pose: Option<&'a Pose>,
    globals: Option<&'a [Mat4]>,
    transform: &'a Transform,
    dt: f32,
    goal: f32,
    probe: Option<&'a GroundProbes>,
    ignore: Entity,
    respawn: bool,
    camera: Option<Vec3>,
    phase: u32,
}

/// What the graph-output pass reads and writes on each rig.
pub type FootPlacementGraphRig = (
    Entity,
    &'static mut FootPlacement,
    &'static mut Transform,
    &'static mut Animator,
    &'static SkinnedMesh,
    Option<&'static MontagePlayer>,
    Option<&'static JointGlobals>,
    Option<&'static mut FootPlantReport>,
    Option<&'static mut FootPlacementStatus>,
    Has<FootPlacementRespawn>,
);

/// Graph output, before `evaluate_animators`. Exclusive (`&mut World`): the physics probe's
/// published pointer is not read in parallel with a system that mutates the physics world. The
/// queries are system state, so a frame does not rebuild them (or allocate).
pub fn prepare_foot_placement(
    world: &mut World,
    scan: &mut QueryState<&FootPlacement, Without<regia_scene::layers::LayerParked>>,
    query: &mut QueryState<FootPlacementGraphRig, Without<regia_scene::layers::LayerParked>>,
) {
    let _span = tracing::debug_span!("foot_placement").entered();
    let mut any_graph = false;
    {
        for placement in scan.iter(world) {
            if placement.is_graph() {
                any_graph = true;
                break;
            }
        }
    }
    // Pose-only worlds plant inside `apply_animation`. Skipping the rest keeps that path from
    // taking the probe resource (and allocating) on every frame.
    if !any_graph {
        return;
    }
    let probes = world.remove_resource::<GroundProbes>();
    let camera = world.get_resource::<crate::AnimLodCamera>().map(|c| c.position);
    let mut insert_report = Vec::new();
    let mut insert_status = Vec::new();
    let mut clear_respawn = Vec::new();
    {
        for (
            entity,
            mut placement,
            mut transform,
            mut animator,
            mesh,
            montage,
            globals,
            mut report,
            mut status,
            respawn,
        ) in query.iter_mut(world)
        {
            if !placement.is_graph() {
                continue;
            }
            let goal = weight_goal(&placement, &animator, montage);
            let ignore = placement.character.unwrap_or(entity);
            let advance = advance(
                &mut placement,
                AdvanceIn {
                    skeleton: &mesh.skeleton,
                    pose: None,
                    globals: globals.map(|g| g.globals.as_slice()),
                    transform: &transform,
                    dt: animator.last_eval_delta,
                    goal,
                    probe: probes.as_ref(),
                    ignore,
                    respawn,
                    camera,
                    phase: entity.index_u32(),
                },
            );
            if advance.planned {
                write_graph_params(&placement, &mut animator, &placement.plan, transform.scale);
                let pelvis = transform.rotation * Vec3::Y * placement.plan.pelvis_offset;
                transform.translation += pelvis;
            }
            let flags = advance.status();
            if let Some(slot) = report.as_deref_mut() {
                placement.planter.report_into(&placement.plan, slot);
            } else {
                let mut slot = FootPlantReport::default();
                placement.planter.report_into(&placement.plan, &mut slot);
                placement.pending_report = Some(slot);
                insert_report.push(entity);
            }
            if let Some(slot) = status.as_deref_mut() {
                *slot = flags;
            } else {
                placement.pending_status = Some(flags);
                insert_status.push(entity);
            }
            if respawn {
                clear_respawn.push(entity);
            }
        }
    }
    if let Some(probes) = probes {
        world.insert_resource(probes);
    }
    for entity in insert_report {
        let report = world
            .get_mut::<FootPlacement>(entity)
            .and_then(|mut placement| placement.pending_report.take());
        if let Some(report) = report {
            world.entity_mut(entity).insert(report);
        }
    }
    for entity in insert_status {
        let flags = world
            .get_mut::<FootPlacement>(entity)
            .and_then(|mut placement| placement.pending_status.take());
        if let Some(flags) = flags {
            world.entity_mut(entity).insert(flags);
        }
    }
    for entity in clear_respawn {
        world.entity_mut(entity).remove::<FootPlacementRespawn>();
    }
}

/// Pose output, on the working pose inside `apply_animation` (after ragdoll, before gaze).
pub(crate) fn plant_pose(placement: &mut FootPlacement, job: PosePlant<'_>) -> FootPlacementStatus {
    let _span = tracing::debug_span!("foot_placement").entered();
    let goal = weight_goal(placement, job.animator, job.montage);
    let dt = job.animator.last_eval_delta;
    let scale = job.transform.scale;
    // The shared pose borrow has to end before `apply_plan_to_pose` takes it mutably.
    let advance = {
        let pose = &*job.pose;
        advance(
            placement,
            AdvanceIn {
                skeleton: job.skeleton,
                pose: Some(pose),
                globals: None,
                transform: job.transform,
                dt,
                goal,
                probe: job.probe,
                ignore: job.ignore,
                respawn: job.respawn,
                camera: job.camera,
                phase: job.phase,
            },
        )
    };
    // The plan is moved out for the apply (which borrows the component mutably) and back: no
    // allocation, the buffer keeps its capacity.
    let plan = std::mem::take(&mut placement.plan);
    if advance.planned {
        apply_plan_to_pose(placement, job.skeleton, job.pose, &plan, scale);
    }
    placement.planter.report_into(&plan, job.report);
    placement.plan = plan;
    advance.status()
}

/// Whether graph output has foot tilt or toe bend to apply after evaluation this frame
/// ([`turn_graph_feet`]); `false` for pose output and for flat feet, so a level walk keeps the
/// animator's pose without a copy.
pub(crate) fn graph_turns_feet(placement: &FootPlacement) -> bool {
    placement.is_graph() && placement.legs_ok && plan_turns_feet(&placement.plan)
}

/// Graph output (ADR-10130): turn each foot by its tilt and bend each toe on the evaluated pose,
/// after the graph's `TwoBoneIK` nodes moved the legs to the targets written before evaluation.
/// The graph cannot express the turns, so without this graph output kept the clip's foot
/// orientation on slopes and stairs.
pub(crate) fn turn_graph_feet(placement: &mut FootPlacement, skeleton: &Skeleton, pose: &mut Pose) {
    if !graph_turns_feet(placement) {
        return;
    }
    let _span = tracing::debug_span!("foot_placement_turns").entered();
    let FootPlacement {
        plan,
        ik_legs,
        toe_joints,
        apply_buffers,
        ..
    } = placement;
    apply_foot_turns_into(
        skeleton,
        pose,
        plan,
        ik_legs,
        toe_joints,
        &mut apply_buffers.scratch,
    );
}

/// Copy the pre-IK joint positions recorded during this evaluation, for next frame's graph plan.
pub(crate) fn store_animated(placement: &mut FootPlacement, tips: &[(u32, Vec3)]) {
    if placement.is_graph() {
        placement.animated.clear();
        placement.animated.extend_from_slice(tips);
    }
}

/// Goal weight from a montage curve, else the float parameter, else 1.
fn weight_goal(placement: &FootPlacement, animator: &Animator, montage: Option<&MontagePlayer>) -> f32 {
    if let Some(player) = montage
        && let Some(value) = player.named_curve(&placement.weight_param)
        && value.is_finite()
    {
        return value;
    }
    match animator.parameters.get(&placement.weight_param) {
        Some(ParamValue::Float(value)) if value.is_finite() => value,
        _ => 1.0,
    }
}

fn advance(placement: &mut FootPlacement, input: AdvanceIn<'_>) -> FootAdvance {
    let probe_installed = input.probe.is_some();
    let legs_resolved = resolve(placement, input.skeleton);
    let current = input.transform.translation;
    let limit = if placement.teleport_distance.is_finite() && placement.teleport_distance >= 0.0 {
        placement.teleport_distance
    } else {
        2.0
    };
    let jumped = placement
        .last_translation
        .is_some_and(|last| last.is_finite() && current.is_finite() && last.distance(current) > limit);
    if input.respawn || jumped {
        placement.planter.reset();
    }
    if current.is_finite() {
        placement.last_translation = Some(current);
    }
    placement.planter.settings = placement.settings;
    let lod = placement.lod.select(camera_distance(input.camera, current));
    placement.current_lod = lod;
    let idle = FootAdvance {
        probe_installed,
        legs_resolved,
        planned: false,
        lod,
    };

    // Off with the effect faded out: no probe, no plan, the clip pose.
    if !legs_resolved
        || !joints_ready(placement, input.skeleton, input.pose, input.globals)
        || (lod == IkLod::Off && placement.planter.weight() <= 0.0)
    {
        placement.plan.legs.clear();
        placement.plan.weight = 0.0;
        placement.plan.pelvis_offset = 0.0;
        return idle;
    }

    let scale = span_scale(input.transform.scale);
    if let Some(pose) = input.pose {
        // Forward kinematics along each leg's chain (hip, knee and ankle are on it); the other
        // joints are not read.
        for leg in &placement.resolved {
            refresh_global(
                input.skeleton,
                pose,
                leg.chain.tip as usize,
                &mut placement.globals,
            );
        }
    }
    let n = placement.resolved.len();
    let cached = placement.hits.len() == n;
    let probing = placement
        .cadence
        .probe_now(lod, &placement.lod, input.phase, cached);
    if !cached {
        placement.hits.resize(n, LegHit::default());
    }
    // Leg inputs are built into a stack so a character (two or four legs) does not allocate.
    // The planter borrows the slice for the call. More than eight legs spill into buffers kept
    // on the component (cleared and refilled, grown once), so a settled frame of a many-legged
    // rig does not allocate either.
    let mut stack = [FootPlantLeg::new(Vec3::ZERO, Vec3::ZERO, 0.0, 0.0); 8];
    let mut sole_stack = [FootSole::default(); 8];
    let on_stack = n <= stack.len();
    let mut mids = std::mem::take(&mut placement.mids);
    mids.clear();
    let mut spill = std::mem::take(&mut placement.spill);
    spill.legs.clear();
    spill.soles.clear();
    for i in 0..n {
        let mid = placement.resolved[i].chain.mid;
        mids.push(joint_local(placement, mid, input.globals));
        let hit = leg_hit(
            placement,
            i,
            input.transform,
            input.globals,
            probing,
            input.probe,
            input.ignore,
        );
        placement.hits[i] = hit;
        let (leg, sole) = leg_input(placement, i, input.transform, scale, input.globals, hit);
        if on_stack {
            (stack[i], sole_stack[i]) = (leg, sole);
        } else {
            spill.legs.push(leg);
            spill.soles.push(sole);
        }
    }
    placement.mids = mids;
    let inputs: &[FootPlantLeg] = if on_stack { &stack[..n] } else { &spill.legs };
    // Without a foot shape the planter sees no soles and plans exactly as `plan` does.
    let soles: &[FootSole] = if placement.foot_shape.is_none() {
        &[]
    } else if on_stack {
        &sole_stack[..n]
    } else {
        &spill.soles
    };

    // Moving ground: a platform under a planted foot carries its pin by the ground's motion
    // this frame (the probe ran under the pin, so the planter's sample alone cannot see it). A
    // frame that does not probe carries by the leg's last measured ground velocity.
    let dt = if input.dt.is_finite() {
        input.dt.max(0.0)
    } else {
        0.0
    };
    for i in 0..n {
        let velocity = placement.hits[i].velocity;
        placement.planter.carry(i, velocity * dt);
    }

    let active =
        placement.grounded.unwrap_or(true) && input.goal > 0.0 && probe_installed && lod != IkLod::Off;
    placement.planter.plan_with_soles_into(
        &FootPlantInput {
            rig_translation: current,
            rig_rotation: input.transform.rotation,
            legs: inputs,
            active,
            dt: input.dt,
        },
        soles,
        &mut placement.plan,
    );
    placement.spill = spill;
    FootAdvance {
        planned: true,
        ..idle
    }
}

fn joints_ready(
    placement: &FootPlacement,
    skeleton: &Skeleton,
    pose: Option<&Pose>,
    globals: Option<&[Mat4]>,
) -> bool {
    if placement.resolved.is_empty() {
        return false;
    }
    if pose.is_some_and(|pose| pose.len() == skeleton.len()) {
        return true;
    }
    placement.resolved.iter().all(|leg| {
        placement
            .animated
            .iter()
            .any(|(joint, _)| *joint == leg.chain.tip)
            || globals.is_some_and(|globals| (leg.chain.tip as usize) < globals.len())
    })
}

/// The ground under leg `index`: probed this frame when `probing` (with no probe resource, the
/// clip's flat floor), else the leg's last hits.
fn leg_hit(
    placement: &FootPlacement,
    index: usize,
    transform: &Transform,
    globals: Option<&[Mat4]>,
    probing: bool,
    probe: Option<&GroundProbes>,
    ignore: Entity,
) -> LegHit {
    if !probing {
        return placement.hits.get(index).copied().unwrap_or_default();
    }
    let resolved = &placement.resolved[index];
    let foot = to_world(transform, joint_local(placement, resolved.chain.tip, globals));
    let probe_at = placement.planter.pin(index).unwrap_or(foot);
    let height = if placement.probe_height.is_finite() {
        placement.probe_height.abs()
    } else {
        0.6
    };
    let length = if placement.probe_length.is_finite() {
        placement.probe_length.abs()
    } else {
        1.2
    };
    let (ground, normal, velocity) = match probe
        .and_then(|probe| probe.probe_moving(probe_at + Vec3::Y * height, Vec3::NEG_Y, length, ignore))
    {
        Some((hit, velocity)) => (FootGround::Contact(hit.point), hit.normal, velocity),
        None => (FootGround::None, Vec3::Y, Vec3::ZERO),
    };
    // Heel and toe: probed at the same anchor (the pin while the stance holds it) plus the clip
    // foot's sole offsets. A bisection finds a step edge between them.
    let sole = match (placement.foot_shape, resolved.shape, probe) {
        (Some(settings), Some(shape), Some(probe)) => {
            match joint_matrix(placement, resolved.chain.tip, globals) {
                Some(matrix) => {
                    let world = Mat4::from_scale_rotation_translation(
                        transform.scale,
                        transform.rotation,
                        transform.translation,
                    ) * matrix;
                    probe_foot_sole(
                        world,
                        probe_at,
                        &shape,
                        placement.settings.step_edge,
                        settings.edge_steps,
                        |p| {
                            probe
                                .probe(p + Vec3::Y * height, Vec3::NEG_Y, length, ignore)
                                .map(|hit| hit.point)
                        },
                    )
                }
                None => FootSole::default(),
            }
        }
        _ => FootSole::default(),
    };
    LegHit {
        ground,
        normal,
        sole,
        velocity,
    }
}

/// The planter's input for leg `index` with the ground `hit`.
fn leg_input(
    placement: &FootPlacement,
    index: usize,
    transform: &Transform,
    scale: f32,
    globals: Option<&[Mat4]>,
    hit: LegHit,
) -> (FootPlantLeg, FootSole) {
    let resolved = &placement.resolved[index];
    let hip = to_world(transform, joint_local(placement, resolved.chain.root, globals));
    let foot = to_world(transform, joint_local(placement, resolved.chain.tip, globals));
    (
        FootPlantLeg {
            hip,
            foot,
            length: resolved.length * scale,
            ankle_height: resolved.ankle_height * scale,
            ground: hit.ground,
            normal: hit.normal,
            contact: None,
        },
        hit.sole,
    )
}

/// The object matrix of `joint`: this frame's pose in pose mode, else last frame's joint globals
/// with the pre-IK position recorded for the graph. `None` when neither exists.
fn joint_matrix(placement: &FootPlacement, joint: u32, globals: Option<&[Mat4]>) -> Option<Mat4> {
    if placement.is_pose() && (joint as usize) < placement.globals.len() {
        return Some(placement.globals[joint as usize]);
    }
    let mut matrix = *globals?.get(joint as usize)?;
    if let Some((_, position)) = placement.animated.iter().find(|(id, _)| *id == joint) {
        matrix.w_axis = position.extend(1.0);
    }
    Some(matrix)
}

fn joint_local(placement: &FootPlacement, joint: u32, globals: Option<&[Mat4]>) -> Vec3 {
    // Pose mode fills `globals` for this frame before the legs are built. A graph frame leaves
    // that buffer alone and reads the pre-IK snapshot, then last frame's joint globals.
    if placement.is_pose() && !placement.globals.is_empty() && (joint as usize) < placement.globals.len() {
        return placement.globals[joint as usize].w_axis.truncate();
    }
    if let Some((_, position)) = placement.animated.iter().find(|(id, _)| *id == joint) {
        return *position;
    }
    globals
        .and_then(|globals| globals.get(joint as usize))
        .map_or(Vec3::ZERO, |matrix| matrix.w_axis.truncate())
}

fn write_graph_params(placement: &FootPlacement, animator: &mut Animator, plan: &FootPlant, scale: Vec3) {
    let inv = 1.0 / span_scale(scale);
    let weight = if plan.weight > 0.0 { 1.0 } else { 0.0 };
    for (i, leg) in plan.legs.iter().enumerate() {
        let Some(resolved) = placement.resolved.get(i) else {
            continue;
        };
        // The planter lifts the ankle by its bind height so a pose-mode sole meets the ground.
        // A graph node moves the tip joint itself. Drop that lift, already scaled by the
        // placement weight inside the lerped target, or the tip sits a bind height above the
        // contact. At weight 0 or 1 the written tilt is the tilt the planter divided by.
        let mut target = (leg.target - Vec3::Y * plan.pelvis_offset) * inv;
        target.y -= resolved.ankle_height.max(0.0) / graph_tilt_cos(leg.tilt) * plan.weight;
        animator.set(&resolved.target_param, target);
        animator.set(&resolved.weight_param, weight);
        let mid = placement.mids.get(i).copied().unwrap_or(Vec3::ZERO);
        animator.set(&resolved.pole_param, mid + resolved.pole_offset);
    }
}

fn apply_plan_to_pose(
    placement: &mut FootPlacement,
    skeleton: &Skeleton,
    pose: &mut Pose,
    plan: &FootPlant,
    scale: Vec3,
) {
    let inv = 1.0 / span_scale(scale);
    placement.applied.pelvis_offset = plan.pelvis_offset * inv;
    placement.applied.weight = plan.weight;
    placement.applied.legs.clear();
    placement
        .applied
        .legs
        .extend(plan.legs.iter().copied().map(|mut leg| {
            leg.target *= inv;
            leg
        }));
    let pole = placement
        .resolved
        .iter()
        .find(|leg| leg.pole_offset != Vec3::ZERO)
        .map(|leg| leg.pole_offset)
        .unwrap_or(Vec3::NEG_Z);
    if placement.foot_shape.is_some() {
        apply_foot_plant_with_toes_into(
            skeleton,
            pose,
            &placement.applied,
            &placement.ik_legs,
            &placement.toe_joints,
            Some(pole),
            &mut placement.apply_buffers,
        );
    } else {
        apply_foot_plant_into(
            skeleton,
            pose,
            &placement.applied,
            &placement.ik_legs,
            Some(pole),
            &mut placement.apply_buffers.scratch,
        );
    }
}

fn resolve(placement: &mut FootPlacement, skeleton: &Skeleton) -> bool {
    let key = std::ptr::from_ref(skeleton) as usize;
    let target_pattern = graph_pattern(&placement.output, GraphSlot::Target);
    let weight_pattern = graph_pattern(&placement.output, GraphSlot::Weight);
    let pole_pattern = graph_pattern(&placement.output, GraphSlot::Pole);
    let same = placement.resolved_skeleton == key
        && placement.resolved_shape == placement.foot_shape
        && placement.resolved.len() == placement.legs.len()
        && placement.pattern_target == target_pattern
        && placement.pattern_weight == weight_pattern
        && placement.pattern_pole == pole_pattern
        && placement
            .resolved
            .iter()
            .zip(&placement.legs)
            .all(|(resolved, leg)| {
                resolved.hip == leg.hip
                    && resolved.knee == leg.knee
                    && resolved.ankle == leg.ankle
                    && resolved.toe_name.as_deref() == leg.toe.as_deref()
                    && resolved.pole_offset == leg.pole_offset
            });
    if same && !placement.resolved.is_empty() {
        return placement.legs_ok;
    }
    placement.resolved.clear();
    placement.ik_legs.clear();
    placement.toe_joints.clear();
    placement.resolved_shape = placement.foot_shape;
    placement.resolved_skeleton = key;
    placement.pattern_target = target_pattern.to_string();
    placement.pattern_weight = weight_pattern.to_string();
    placement.pattern_pole = pole_pattern.to_string();
    placement.legs_ok = false;
    if placement.legs.is_empty() {
        return false;
    }
    let target_pattern = placement.pattern_target.clone();
    let weight_pattern = placement.pattern_weight.clone();
    let pole_pattern = placement.pattern_pole.clone();
    let bind = skeleton.bind_pose_pose();
    let count = placement.legs.len();
    for index in 0..count {
        // Copy the names out first: the push below mutably borrows the component.
        let hip_name = placement.legs[index].hip.clone();
        let knee_name = placement.legs[index].knee.clone();
        let ankle_name = placement.legs[index].ankle.clone();
        let toe_name = placement.legs[index].toe.clone();
        let pole_offset = placement.legs[index].pole_offset;
        let Some(hip) = skeleton.joint_index(&hip_name) else {
            return false;
        };
        let Some(knee) = skeleton.joint_index(&knee_name) else {
            return false;
        };
        let Some(ankle) = skeleton.joint_index(&ankle_name) else {
            return false;
        };
        let hip_p = crate::ik::joint_object_point(skeleton, &bind, hip);
        let knee_p = crate::ik::joint_object_point(skeleton, &bind, knee);
        let ankle_p = crate::ik::joint_object_point(skeleton, &bind, ankle);
        let chain = IkChain {
            root: hip,
            mid: knee,
            tip: ankle,
        };
        // A toe name that does not resolve is not an error: the shape uses `front_length`.
        let toe = toe_name.as_deref().and_then(|name| skeleton.joint_index(name));
        let shape = placement
            .foot_shape
            .and_then(|settings| foot_shape_from_bind(skeleton, ankle, toe, &settings));
        placement.resolved.push(ResolvedLeg {
            hip: hip_name,
            knee: knee_name,
            ankle: ankle_name,
            toe_name,
            chain,
            shape,
            length: (knee_p - hip_p).length() + (ankle_p - knee_p).length(),
            ankle_height: ankle_p.y,
            pole_offset,
            target_param: expand_index(&target_pattern, index),
            weight_param: expand_index(&weight_pattern, index),
            pole_param: expand_index(&pole_pattern, index),
        });
        placement.ik_legs.push(FootIkLeg::new(chain));
        placement.toe_joints.push(toe);
    }
    placement.legs_ok = placement.resolved.len() == placement.legs.len();
    placement.legs_ok
}

enum GraphSlot {
    Target,
    Weight,
    Pole,
}

fn graph_pattern(output: &FootPlacementOutput, slot: GraphSlot) -> &str {
    match output {
        FootPlacementOutput::GraphParams { target, weight, pole } => match slot {
            GraphSlot::Target => target.as_str(),
            GraphSlot::Weight => weight.as_str(),
            GraphSlot::Pole => pole.as_str(),
        },
        FootPlacementOutput::Pose => "",
    }
}

fn expand_index(pattern: &str, index: usize) -> String {
    let rendered = index.to_string();
    if pattern.contains("{i}") {
        pattern.replace("{i}", &rendered)
    } else if pattern.is_empty() {
        String::new()
    } else {
        let mut name = String::with_capacity(pattern.len() + rendered.len());
        name.push_str(pattern);
        name.push_str(&rendered);
        name
    }
}

/// Cosine of the foot tilt, clamped the way the planter lifts the ankle.
///
/// Identity is 1. A tilt past 60 degrees stays at 0.5, matching `cos_angle(tilt).max(0.5)`
/// in the planter (`cos_angle` is private there).
fn graph_tilt_cos(tilt: Quat) -> f32 {
    (2.0 * tilt.w * tilt.w - 1.0).clamp(0.5, 1.0)
}

fn span_scale(scale: Vec3) -> f32 {
    let s = scale.x.abs();
    if s.is_finite() && s > 1.0e-8 { s } else { 1.0 }
}

fn to_world(transform: &Transform, local: Vec3) -> Vec3 {
    transform.translation + transform.rotation * (local * transform.scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns one fixed hit whatever the ray.
    struct Fixed(GroundHit);

    impl GroundProbe for Fixed {
        fn probe(&self, _from: Vec3, _down: Vec3, _max_distance: f32, _ignore: Entity) -> Option<GroundHit> {
            Some(self.0)
        }
    }

    fn cast(hit: GroundHit) -> Option<GroundHit> {
        GroundProbes::new(Fixed(hit)).probe(Vec3::new(0.0, 0.6, 0.0), Vec3::NEG_Y, 1.2, Entity::PLACEHOLDER)
    }

    /// The scene probe keeps ground under the ray and drops a hit at (or above) the origin, a
    /// back face and a non-finite hit (ADR-8721).
    #[test]
    fn only_ground_under_the_ray_counts() {
        let ground = GroundHit {
            point: Vec3::ZERO,
            normal: Vec3::Y,
        };
        assert_eq!(cast(ground), Some(ground));
        let sloped = GroundHit {
            normal: Vec3::new(0.3, 0.9, 0.0),
            ..ground
        };
        assert_eq!(cast(sloped), Some(sloped));
        // A zero normal tilts nothing, but the point is still ground.
        let flat = GroundHit {
            normal: Vec3::ZERO,
            ..ground
        };
        assert_eq!(cast(flat), Some(flat));
        // A cast that starts inside the geometry: the hit is the ray origin.
        let origin = GroundHit {
            point: Vec3::new(0.0, 0.6, 0.0),
            normal: Vec3::Y,
        };
        assert_eq!(cast(origin), None);
        // Above the origin (behind the ray).
        let above = GroundHit {
            point: Vec3::new(0.0, 0.9, 0.0),
            normal: Vec3::Y,
        };
        assert_eq!(cast(above), None);
        // The far face of the shape the ray started in: its normal points along the ray.
        let back = GroundHit {
            point: Vec3::new(0.0, -0.4, 0.0),
            normal: Vec3::NEG_Y,
        };
        assert_eq!(cast(back), None);
        let nan = GroundHit {
            point: Vec3::new(f32::NAN, 0.0, 0.0),
            normal: Vec3::Y,
        };
        assert_eq!(cast(nan), None);
    }
}

// Read-only views of the resolved legs and pins for debug draw (the locomotion gizmos, ADR-8729).
mod inspect;
pub use inspect::FootLegJoints;
