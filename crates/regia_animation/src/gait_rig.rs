// SPDX-License-Identifier: MPL-2.0
//! [`GaitRig`]: the engine walks a legged rig with the procedural gait (ADR-8707).
//!
//! Without this component the stepper in [`crate::gait`] only returns foot targets, and each
//! host schedules it, converts axes, types hip offsets and solves the legs itself. A rig with a
//! `GaitRig` is walked by the engine instead:
//!
//! 1. [`step_gait_rigs`] (`PostUpdate`, before `apply_animation`) resolves the legs on first use
//!    and after a skeleton or name change, measures the neutral foot offsets from the bind pose
//!    ([`GaitParams::hip_offsets_from_skeleton`]), steps the gait with the frame time and keeps
//!    the result in the rig's [`GaitState`] (inserted by the engine; the agent anchor). Ground
//!    comes from the scene's [`GroundProbes`] resource (animation-08's probe seam), adapted to
//!    the stepper's [`GaitGround`]; with no probe the ground is the flat plane at the rig's floor.
//! 2. `apply_animation`, right after the ragdoll blend and the pose-mode foot placement and
//!    before eye gaze, lifts the body joint to the stance height and solves every leg to its
//!    foot target: two-bone legs (three joints) with the analytic two-bone solve, longer chains
//!    with FABRIK under the leg's joint limits.
//!
//! Modes ([`GaitRigMode`]):
//!
//! * `Driven`: gameplay moves the rig's `Transform` (simulation, physics mover, navigation). Its
//!   XZ and yaw drive the body; the velocity comes from a [`GaitVelocity`] component, or else from
//!   the transform's change over the frame.
//! * `Integrating`: the stepper moves the rig from a [`GaitControls`] component and writes the
//!   rig's translation and yaw (toys, previews, tests).
//!
//! Conventions: the rig faces −Z in object space (Regia's character convention), +Y up, and its
//! scale is uniform (`scale.x` is used). The floor in the bind pose is the lowest leg tip: a
//! tip higher than that stays that much above the ground. A rig without `GaitRig` is untouched.
//!
//! IK level of detail ([`GaitRig::lod`], [`crate::ik_lod`]): at reduced detail the ground is
//! probed every few frames and the stepper reads the last hits in between (the plane through the
//! nearest cached hit); at off detail a `Driven` rig is neither stepped nor posed (the clip pose)
//! and starts from rest when it comes back, while an `Integrating` rig keeps stepping at the
//! reduced cadence (it moves its own transform) and only its leg solve stops. The level is in
//! [`GaitRigStatus::lod`].
//!
//! Presentation only: nothing here is read by a simulation checksum. Deterministic for identical
//! inputs on one target; no allocation per frame once the legs are resolved.

use regia_core::math::{Mat4, Quat, Vec3};
use regia_ecs::prelude::*;
use regia_scene::Transform;

use crate::chain_ik::{ChainIkScratch, ChainIkSettings, ChainSolver, JointChain, JointLimit, solve_chain};
use crate::components::SkinnedMesh;
use crate::foot_ik::GroundProbes;
use crate::gait::{
    GaitBody, GaitError, GaitGround, GaitMode, GaitParams, GaitState, GroundHit, MAX_LEGS, step_gait,
    step_gait_driven,
};
use crate::gait_asset::{GaitAsset, GaitAssets};
use crate::ik::{IkChain, refresh_global, solve_two_bone_with_globals};
use crate::ik_lod::{IkLod, IkLodSettings, LodCadence, camera_distance};
use crate::pose::Pose;
use crate::skeleton::Skeleton;
use regia_asset::Handle;

/// Longest gait substep in seconds. A longer frame is split into equal substeps.
pub const GAIT_RIG_MAX_SUBSTEP: f32 = 1.0 / 30.0;

/// Most substeps per frame; a longer frame is clamped to `GAIT_RIG_MAX_SUBSTEP * 8` seconds.
pub const GAIT_RIG_MAX_SUBSTEPS: u32 = 8;

/// Most times one frame halves its substep after the stepper refused a substep as longer than
/// one stance (`gait_step_too_large`), so a short stance or settle period walks the same at
/// 30, 60 and 144 Hz. Four halvings reach `GAIT_RIG_MAX_SUBSTEP / 16` (1/480 s at 30 Hz). A
/// finest slice the stepper still refuses is retried once with the ground speed capped just
/// under one stance per slice (ADR-10053): the rig walks at that cap instead of freezing, and
/// the cap is not a `gait_rig_step` error. Bounds a frame at `GAIT_RIG_MAX_SUBSTEPS * 32`
/// stepper calls.
pub const GAIT_RIG_MAX_SPLITS: u32 = 4;

/// One leg of a [`GaitRig`], named the way the skeleton names its joints.
#[derive(Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Default)]
pub struct GaitLeg {
    /// Chain root joint (the hip or coxa).
    pub root: String,
    /// Chain tip joint (the foot). Three joints from root to tip use the two-bone solve; any
    /// other count uses FABRIK.
    pub tip: String,
    /// FABRIK joint limits, one per chain joint root first, or empty for a free chain. Ignored by
    /// two-bone legs. Not reflected.
    #[reflect(ignore)]
    pub limits: Vec<JointLimit>,
}

impl GaitLeg {
    /// A free leg from `root` down to `tip`.
    #[must_use]
    pub fn new(root: impl Into<String>, tip: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            tip: tip.into(),
            limits: Vec::new(),
        }
    }
}

/// Where a [`GaitRig`]'s body motion comes from.
#[derive(Reflect, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[reflect(Default)]
pub enum GaitRigMode {
    /// The rig's `Transform` is moved by gameplay; the gait follows it ([`step_gait_driven`]).
    #[default]
    Driven,
    /// The gait moves the rig from [`GaitControls`] ([`step_gait`]) and writes its `Transform`.
    Integrating,
}

/// Controls of an integrating [`GaitRig`], each clamped to `[-1, 1]`.
#[derive(Component, Reflect, Clone, Copy, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct GaitControls {
    /// +1 walks forward (the rig's −Z) at `forward_speed`, −1 backward at `reverse_speed`.
    pub forward: f32,
    /// +1 walks to the rig's right (+X) at `strafe_speed`.
    pub strafe: f32,
    /// +1 turns left (counterclockwise seen from above, +yaw) at `max_yaw_rate`.
    pub steer: f32,
}

/// World velocity of a driven [`GaitRig`] in meters per second, written by the host. Without it
/// the velocity is the rig translation's change over the frame.
#[derive(Component, Reflect, Clone, Copy, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct GaitVelocity {
    /// World velocity, meters per second.
    pub linear: Vec3,
}

/// The engine-side walker: steps the gait and solves the legs of the rig it is on.
///
/// `params.patterns` must have one leg per entry of `legs`, in pattern order (pairs from the
/// front, left before right). With `auto_hips` (the default) the stepper's `hip_offsets` are
/// measured from the bind pose and scaled by the rig's scale; `params.hip_offsets` is then
/// unused, except for legs whose `.gait.ron` gives a hip offset ([`GaitRig::from_asset`]).
/// Lengths in `params` are world meters.
#[derive(Component, Reflect, Clone, Debug)]
#[reflect(Component)]
pub struct GaitRig {
    /// Gait settings (speeds, step length, clearance, ground band, patterns).
    pub params: GaitParams,
    /// Legs in pattern order.
    pub legs: Vec<GaitLeg>,
    /// Joint lifted to the stance height (the pelvis or thorax), or `None` to leave the body.
    pub body: Option<String>,
    /// Driven by the rig's transform, or integrating from [`GaitControls`].
    pub mode: GaitRigMode,
    /// Measure the neutral foot offsets from the bind pose (default `true`).
    pub auto_hips: bool,
    /// FABRIK iteration budget for legs that are not two-bone. Default 32.
    pub ik_iterations: u32,
    /// FABRIK stop distance, skeleton units. Default 1e-4.
    pub ik_tolerance: f32,
    /// With an unbounded `ground_band`: meters above the expected floor the probe starts.
    /// Default 1.
    pub probe_height: f32,
    /// With an unbounded `ground_band`: meters below the expected floor the probe reaches.
    /// Default 2.
    pub probe_length: f32,
    /// The `.gait.ron` settings this rig follows (gap G7, [`crate::gait_asset`]), or `None` for
    /// settings set in code. While the handle resolves in the app's [`GaitAssets`], every new
    /// generation (a load or a hot reload) replaces `params`, `legs` and `body` on the next frame;
    /// legs whose hip offset the file gives keep it instead of the bind-pose measure. Not
    /// reflected.
    #[reflect(ignore)]
    pub asset: Option<Handle<GaitAsset>>,
    #[reflect(ignore)]
    asset_generation: u64,
    #[reflect(ignore)]
    hip_overrides: u16,
    /// IK level of detail by distance to the [`AnimLodCamera`](crate::AnimLodCamera), or forced by
    /// the host ([`IkLodSettings::force`]). Without a camera the rig stays at full detail.
    pub lod: IkLodSettings,
    #[reflect(ignore)]
    resolved: Vec<ResolvedLeg>,
    #[reflect(ignore)]
    resolved_key: usize,
    #[reflect(ignore)]
    resolved_body: Option<String>,
    #[reflect(ignore)]
    body_joint: Option<u32>,
    #[reflect(ignore)]
    tips: [[f32; 2]; MAX_LEGS],
    #[reflect(ignore)]
    floor: f32,
    #[reflect(ignore)]
    resolved_count: u8,
    #[reflect(ignore)]
    issue: GaitRigIssue,
    #[reflect(ignore)]
    count_ok: bool,
    #[reflect(ignore)]
    flat_floor: f32,
    #[reflect(ignore)]
    last_translation: Option<Vec3>,
    #[reflect(ignore)]
    pending_state: Option<GaitState>,
    #[reflect(ignore)]
    globals: Vec<Mat4>,
    #[reflect(ignore)]
    scratch: ChainIkScratch,
    #[reflect(ignore)]
    cadence: LodCadence,
    #[reflect(ignore)]
    current_lod: IkLod,
    /// A `Driven` rig was off, or [`GaitRig::reset`] ran: the next step starts from rest.
    #[reflect(ignore)]
    restart: bool,
    #[reflect(ignore)]
    hits: HitCache,
}

impl GaitRig {
    /// A driven rig with automatic hip offsets and default solver and probe settings.
    #[must_use]
    pub fn new(params: GaitParams, legs: Vec<GaitLeg>) -> Self {
        Self {
            params,
            legs,
            body: None,
            mode: GaitRigMode::Driven,
            auto_hips: true,
            ik_iterations: 32,
            ik_tolerance: 1.0e-4,
            probe_height: 1.0,
            probe_length: 2.0,
            asset: None,
            asset_generation: 0,
            hip_overrides: 0,
            lod: IkLodSettings::default(),
            resolved: Vec::new(),
            resolved_key: 0,
            resolved_body: None,
            body_joint: None,
            tips: [[0.0; 2]; MAX_LEGS],
            floor: 0.0,
            resolved_count: 0,
            issue: GaitRigIssue::None,
            count_ok: true,
            flat_floor: 0.0,
            last_translation: None,
            pending_state: None,
            globals: Vec::new(),
            scratch: ChainIkScratch::default(),
            cadence: LodCadence::default(),
            current_lod: IkLod::Full,
            restart: false,
            hits: HitCache::default(),
        }
    }

    /// A driven rig on the `.gait.ron` settings `asset` (gap G7), stored in [`GaitAssets`] under
    /// `handle`: its params, legs and body now, and every reload of the handle from the next
    /// frame on. Legs whose hip offset the file gives keep it; the others are measured from the
    /// bind pose.
    #[must_use]
    pub fn from_asset(handle: Handle<GaitAsset>, asset: &GaitAsset) -> Self {
        let mut rig = Self::new(asset.params, asset.legs());
        rig.body.clone_from(&asset.file.body);
        rig.hip_overrides = asset.hip_override_mask();
        rig.asset = Some(handle);
        rig
    }

    /// Take `asset`'s settings if `generation` is new. Allocates only when the leg list or the
    /// body name changed.
    fn apply_asset(&mut self, asset: &GaitAsset, generation: u64) {
        if self.asset_generation == generation {
            return;
        }
        let _span = tracing::debug_span!("gait_rig_apply_asset").entered();
        self.asset_generation = generation;
        self.params = asset.params;
        let same_legs = self.legs.len() == asset.file.legs.len()
            && self
                .legs
                .iter()
                .zip(&asset.file.legs)
                .all(|(have, want)| have.root == want.root && have.tip == want.tip);
        if !same_legs {
            self.legs = asset.legs();
        }
        if self.body != asset.file.body {
            self.body.clone_from(&asset.file.body);
        }
        self.hip_overrides = asset.hip_override_mask();
    }

    /// Builder: the body joint.
    #[must_use]
    pub fn with_body(mut self, body: impl Into<String>) -> Self {
        self.body = Some(body.into());
        self
    }

    /// Builder: the mode.
    #[must_use]
    pub fn with_mode(mut self, mode: GaitRigMode) -> Self {
        self.mode = mode;
        self
    }

    /// Forget the gait state: the next frame plants every foot at neutral again. The engine's
    /// [`GaitState`] component is rebuilt on that frame (it is replaced, not stepped).
    pub fn reset(&mut self) {
        self.pending_state = None;
        self.last_translation = None;
        self.resolved_key = 0;
        self.restart = true;
    }

    /// `ik_tolerance`, `probe_height` and `probe_length` are finite. A rig whose solver or
    /// probe settings are not is refused (`gait_rig_step`): an infinite tolerance would read
    /// as "already solved" and an infinite probe start would never hit.
    fn settings_finite(&self) -> bool {
        self.ik_tolerance.is_finite() && self.probe_height.is_finite() && self.probe_length.is_finite()
    }

    /// The IK level of detail chosen on the latest frame (also in [`GaitRigStatus::lod`]).
    #[must_use]
    pub fn current_lod(&self) -> IkLod {
        self.current_lod
    }
}

/// What went wrong on a [`GaitRig`]; reflected in [`GaitRigStatus::issue`]. See [`Self::code`].
#[derive(Reflect, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[reflect(Default)]
pub enum GaitRigIssue {
    /// Nothing.
    #[default]
    None,
    /// `gait_rig_leg_unresolved`: a leg's root or tip joint is not on the skeleton, or the tip
    /// is not under the root. That leg is not solved; the others are.
    LegUnresolved,
    /// `gait_rig_body_unresolved`: the body joint is not on the skeleton. The body is not lifted.
    BodyUnresolved,
    /// `gait_rig_leg_count`: the number of legs is not the pattern set's leg count (or is above
    /// 16). Nothing is stepped or solved.
    LegCount,
    /// `gait_rig_step`: the stepper refused this frame's step (invalid settings, nonfinite
    /// input or ground, or a stance shorter than the finest substep). The state and pose are
    /// unchanged. Also a rig whose `ik_tolerance`, `probe_height` or `probe_length` is not
    /// finite: it is not stepped (a rig without a state yet is planted at rest on its floor
    /// plane, without a probe cast) and its FABRIK legs are not solved.
    Step,
}

impl GaitRigIssue {
    /// The agent error code, or `None` for [`Self::None`].
    #[must_use]
    pub fn code(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::LegUnresolved => Some("gait_rig_leg_unresolved"),
            Self::BodyUnresolved => Some("gait_rig_body_unresolved"),
            Self::LegCount => Some("gait_rig_leg_count"),
            Self::Step => Some("gait_rig_step"),
        }
    }
}

/// A [`GaitRig`]'s health, inserted by the engine. Reflected so `scene.get_entity` reads it.
#[derive(Component, Reflect, Clone, Copy, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct GaitRigStatus {
    /// A [`GroundProbes`] resource is on the world (otherwise the ground is the rig's floor
    /// plane).
    pub probe_installed: bool,
    /// Legs whose joints resolved.
    pub legs_resolved: u8,
    /// The latest issue; [`GaitRigIssue::None`] on a healthy frame.
    pub issue: GaitRigIssue,
    /// Warnings so far: one per resolve that left a leg or the body unresolved or the leg count
    /// wrong, and one per refused step. Saturates.
    pub warnings: u32,
    /// Frames whose step the stepper refused. Saturates.
    pub step_errors: u32,
    /// The IK level of detail this frame ([`GaitRig::lod`]).
    pub lod: IkLod,
}

#[derive(Clone, Debug)]
enum LegSolver {
    TwoBone(IkChain),
    Chain(JointChain),
}

#[derive(Clone, Debug)]
struct ResolvedLeg {
    root: String,
    tip: String,
    limits: Vec<JointLimit>,
    solver: Option<LegSolver>,
    /// Bind tip height over the bind floor, skeleton units.
    lift: f32,
}

impl GaitParams {
    /// The stepper's `hip_offsets` for `legs` on `skeleton`: each leg's neutral foothold is its
    /// tip joint's bind-pose position on XZ (the foot under its hip), in skeleton units. Multiply
    /// by the rig's scale for world meters.
    ///
    /// The skeleton faces −Z (+X right, +Y up). `mode` picks the stepper's frame:
    ///
    /// * [`GaitMode::Driven`] (`[right, forward]`, forward −Z): object `(x, z)` is `[x, -z]`.
    ///   The stepper's yaw is the rig's yaw.
    /// * [`GaitMode::Integrating`] (`[+X, +Z]`, forward +Z): object `(x, z)` is `[-x, -z]`, for
    ///   a stepper yaw of the rig's yaw plus pi, so the stepper's forward is the rig's −Z.
    ///   [`GaitRig`] keeps that relation: the rig's rotation is the stepper yaw minus pi.
    ///
    /// # Errors
    ///
    /// [`GaitRigIssue::LegCount`] for more than [`MAX_LEGS`] legs;
    /// [`GaitRigIssue::LegUnresolved`] when a tip joint is missing or not under its root.
    pub fn hip_offsets_from_skeleton(
        skeleton: &Skeleton,
        legs: &[GaitLeg],
        mode: GaitMode,
    ) -> Result<[[f32; 2]; MAX_LEGS], GaitRigIssue> {
        if legs.len() > MAX_LEGS {
            return Err(GaitRigIssue::LegCount);
        }
        let globals = skeleton.globals(&skeleton.bind_pose_pose(), None);
        let mut out = [[0.0; 2]; MAX_LEGS];
        for (slot, leg) in out.iter_mut().zip(legs) {
            let chain = JointChain::between(skeleton, &leg.root, &leg.tip)
                .map_err(|_| GaitRigIssue::LegUnresolved)?;
            let tip = chain.joints.last().copied().unwrap_or(0) as usize;
            let p = globals
                .get(tip)
                .map(|m| m.w_axis.truncate())
                .ok_or(GaitRigIssue::LegUnresolved)?;
            *slot = convert_offset([p.x, p.z], mode);
        }
        Ok(out)
    }
}

/// Object XZ `[x, z]` into the stepper's `[a, b]` offset for `mode` (see
/// [`GaitParams::hip_offsets_from_skeleton`]). Negations are exact.
fn convert_offset(p: [f32; 2], mode: GaitMode) -> [f32; 2] {
    match mode {
        GaitMode::Driven => [p[0], -p[1]],
        GaitMode::Integrating => [-p[0], -p[1]],
    }
}

/// Resolve legs and body against `skeleton`. Allocates only when something changed.
fn resolve(rig: &mut GaitRig, skeleton: &Skeleton) -> bool {
    let key = std::ptr::from_ref(skeleton) as usize;
    let same = rig.resolved_key == key
        && rig.resolved.len() == rig.legs.len()
        && rig.resolved_body == rig.body
        && rig
            .resolved
            .iter()
            .zip(&rig.legs)
            .all(|(r, l)| r.root == l.root && r.tip == l.tip && r.limits == l.limits);
    if same {
        return false;
    }
    let _span = tracing::debug_span!("gait_rig_resolve").entered();
    rig.resolved_key = key;
    rig.resolved_body = rig.body.clone();
    rig.resolved.clear();
    rig.tips = [[0.0; 2]; MAX_LEGS];
    let bind = skeleton.bind_pose_pose();
    let globals = skeleton.globals(&bind, None);
    let position = |joint: u32| globals.get(joint as usize).map(|m| m.w_axis.truncate());
    let mut lowest = f32::INFINITY;
    let mut heights = [0.0_f32; MAX_LEGS];
    let mut count = 0_u8;
    let mut issue = GaitRigIssue::None;
    for (i, leg) in rig.legs.iter().enumerate() {
        let chain = JointChain::between(skeleton, &leg.root, &leg.tip).ok();
        let tip = chain
            .as_ref()
            .and_then(|c| c.joints.last().copied())
            .and_then(position);
        let solver = match (chain, tip) {
            (Some(chain), Some(tip)) => {
                if i < MAX_LEGS {
                    rig.tips[i] = [tip.x, tip.z];
                    heights[i] = tip.y;
                }
                lowest = lowest.min(tip.y);
                count = count.saturating_add(1);
                Some(if chain.joints.len() == 3 {
                    LegSolver::TwoBone(IkChain {
                        root: chain.joints[0],
                        mid: chain.joints[1],
                        tip: chain.joints[2],
                    })
                } else {
                    LegSolver::Chain(JointChain {
                        joints: chain.joints,
                        limits: leg.limits.clone(),
                    })
                })
            }
            _ => {
                issue = GaitRigIssue::LegUnresolved;
                None
            }
        };
        rig.resolved.push(ResolvedLeg {
            root: leg.root.clone(),
            tip: leg.tip.clone(),
            limits: leg.limits.clone(),
            solver,
            lift: 0.0,
        });
    }
    rig.floor = if lowest.is_finite() { lowest } else { 0.0 };
    for (i, leg) in rig.resolved.iter_mut().enumerate().take(MAX_LEGS) {
        if leg.solver.is_some() {
            leg.lift = heights[i] - rig.floor;
        }
    }
    rig.body_joint = rig.body.as_deref().and_then(|name| skeleton.joint_index(name));
    if rig.body.is_some() && rig.body_joint.is_none() && issue == GaitRigIssue::None {
        issue = GaitRigIssue::BodyUnresolved;
    }
    rig.resolved_count = count;
    rig.issue = issue;
    if let Some(code) = issue.code() {
        tracing::warn!(
            code,
            legs = rig.legs.len(),
            resolved = count,
            "gait rig did not fully resolve"
        );
    }
    true
}

/// The scene probe as the stepper's ground: a downward cast through the band around the
/// expected floor. A zero or nonfinite normal reads as +Y.
#[derive(Clone, Copy)]
struct ProbeGround<'a> {
    probes: &'a GroundProbes,
    ignore: Entity,
    height: f32,
    length: f32,
}

impl GaitGround for ProbeGround<'_> {
    fn sample(&self, x: f32, z: f32, expected_y: f32, band: f32) -> Option<GroundHit> {
        let (up, reach) = if band.is_finite() {
            (band, 2.0 * band)
        } else {
            (self.height, self.height + self.length)
        };
        let from = Vec3::new(x, expected_y + up, z);
        let hit = self.probes.probe(from, Vec3::NEG_Y, reach, self.ignore)?;
        let n = hit.normal;
        let normal = if n.is_finite() && n.length_squared() > 0.0 {
            [n.x, n.y, n.z]
        } else {
            [0.0, 1.0, 0.0]
        };
        Some(GroundHit {
            y: hit.point.y,
            normal,
        })
    }
}

/// Ground of a scene without a probe: the flat plane at `y`.
#[derive(Clone, Copy)]
struct FlatGround(f32);

impl GaitGround for FlatGround {
    fn sample(&self, _x: f32, _z: f32, _expected_y: f32, _band: f32) -> Option<GroundHit> {
        Some(GroundHit {
            y: self.0,
            normal: [0.0, 1.0, 0.0],
        })
    }
}

/// Either ground, without boxing.
#[derive(Clone, Copy)]
enum RigGround<'a> {
    Probe(ProbeGround<'a>),
    Flat(FlatGround),
}

impl GaitGround for RigGround<'_> {
    fn sample(&self, x: f32, z: f32, expected_y: f32, band: f32) -> Option<GroundHit> {
        match self {
            Self::Probe(g) => g.sample(x, z, expected_y, band),
            Self::Flat(g) => g.sample(x, z, expected_y, band),
        }
    }
}

/// Probe hits kept for the frames that do not probe (reduced detail). Fixed size, no allocation.
const HIT_CACHE: usize = 16;

/// One cached probe: where it was asked and what it found.
#[derive(Clone, Copy, Debug, PartialEq)]
struct CachedHit {
    x: f32,
    z: f32,
    hit: GroundHit,
}

/// The hits of the latest probing frame, oldest overwritten first.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct HitCache {
    hits: [Option<CachedHit>; HIT_CACHE],
    next: usize,
}

/// A hit the stepper's ground check accepts: finite query point, finite height and a finite,
/// non-zero normal. Anything else is not cached (the stepper refuses it as `gait_floor` on the
/// probing frame only).
fn hit_usable(x: f32, z: f32, hit: &GroundHit) -> bool {
    let [nx, ny, nz] = hit.normal;
    x.is_finite()
        && z.is_finite()
        && hit.y.is_finite()
        && hit.normal.iter().all(|n| n.is_finite())
        && nx * nx + ny * ny + nz * nz > 0.0
}

impl HitCache {
    fn record(&mut self, x: f32, z: f32, hit: GroundHit) {
        if !hit_usable(x, z, &hit) {
            return;
        }
        self.hits[self.next % HIT_CACHE] = Some(CachedHit { x, z, hit });
        self.next = (self.next + 1) % HIT_CACHE;
    }

    fn is_empty(&self) -> bool {
        self.hits.iter().all(Option::is_none)
    }

    /// The plane through the cached hit nearest `(x, z)`, evaluated at `(x, z)`. Ties keep the
    /// earlier slot, so the answer does not depend on anything but the cache. Only usable hits
    /// are stored, and a non-finite answer is a miss (`None`).
    fn sample(&self, x: f32, z: f32) -> Option<GroundHit> {
        let mut best: Option<(f32, CachedHit)> = None;
        for cached in self.hits.iter().flatten() {
            let (dx, dz) = (x - cached.x, z - cached.z);
            let d = dx * dx + dz * dz;
            if best.is_none_or(|(b, _)| d < b) {
                best = Some((d, *cached));
            }
        }
        let (_, cached) = best?;
        let [nx, ny, nz] = cached.hit.normal;
        let rise = if ny.abs() > 1.0e-4 {
            -(nx * (x - cached.x) + nz * (z - cached.z)) / ny
        } else {
            0.0
        };
        let y = cached.hit.y + if rise.is_finite() { rise } else { 0.0 };
        let hit = GroundHit {
            y,
            normal: cached.hit.normal,
        };
        // A miss rather than a replayed non-finite hit (the plane can overflow far away).
        hit_usable(x, z, &hit).then_some(hit)
    }
}

/// The stepper's ground at a level of detail: live, live and recorded (a probing frame at
/// reduced detail), or the recorded hits (the frames between).
#[derive(Clone, Copy)]
enum LodGround<'a> {
    Live(RigGround<'a>),
    Record(RigGround<'a>, &'a std::cell::RefCell<HitCache>),
    Cached(&'a HitCache),
}

impl GaitGround for LodGround<'_> {
    fn sample(&self, x: f32, z: f32, expected_y: f32, band: f32) -> Option<GroundHit> {
        match self {
            Self::Live(g) => g.sample(x, z, expected_y, band),
            Self::Record(g, cache) => {
                let hit = g.sample(x, z, expected_y, band)?;
                cache.borrow_mut().record(x, z, hit);
                Some(hit)
            }
            Self::Cached(cache) => cache.sample(x, z),
        }
    }
}

/// The rig's yaw about +Y in the object convention (forward −Z).
fn rig_yaw(rotation: Quat) -> f32 {
    let forward = rotation * Vec3::NEG_Z;
    crate::num::det_atan2(-forward.x, -forward.z)
}

fn wrap_pi(angle: f32) -> f32 {
    let value = angle % core::f32::consts::TAU;
    if value <= -core::f32::consts::PI {
        value + core::f32::consts::TAU
    } else if value > core::f32::consts::PI {
        value - core::f32::consts::TAU
    } else {
        value
    }
}

/// Number of equal substeps for a frame of `dt` seconds, and the (clamped) frame length.
fn substeps(dt: f32) -> (u32, f32) {
    let dt = dt.min(GAIT_RIG_MAX_SUBSTEP * GAIT_RIG_MAX_SUBSTEPS as f32);
    let n = (dt / GAIT_RIG_MAX_SUBSTEP)
        .ceil()
        .clamp(1.0, GAIT_RIG_MAX_SUBSTEPS as f32) as u32;
    (n, dt)
}

/// Run one frame of `dt` seconds as stepper slices. The frame is clamped and split as
/// [`substeps`] says; a slice the stepper refuses as longer than one stance
/// ([`GaitError::StepTooLarge`]) is retried at half the length, at most
/// [`GAIT_RIG_MAX_SPLITS`] times per frame (the shorter slice is kept for the rest of the
/// frame). `step(state, end, h)` advances `state` by `h` seconds; `end` is the fraction of the
/// frame done after the slice (exactly 1 on the last). The stepper is atomic, so a refused
/// slice leaves `state` as it was. Without a refusal the slices are the equal substeps of
/// [`substeps`], bit for bit. The last argument of `step` is whether the slice is already the
/// finest (`GAIT_RIG_MAX_SPLITS` halvings): there the caller may cap its speed instead of
/// refusing (ADR-10053).
fn run_slices<S>(
    state: &mut S,
    dt: f32,
    mut step: impl FnMut(&mut S, f32, f32, bool) -> Result<(), GaitError>,
) -> Result<(), GaitError> {
    let (n, dt) = substeps(dt);
    let base = dt / n as f32;
    // Progress in units of the finest slice, so slice ends are exact.
    let total = u64::from(n) << GAIT_RIG_MAX_SPLITS;
    let mut level = 0_u32;
    let mut done = 0_u64;
    while done < total {
        let end = done + (1_u64 << (GAIT_RIG_MAX_SPLITS - level));
        let h = base / (1_u32 << level) as f32;
        let fraction = if end == total {
            1.0
        } else {
            (end >> (GAIT_RIG_MAX_SPLITS - level)) as f32 / (u64::from(n) << level) as f32
        };
        match step(state, fraction, h, level == GAIT_RIG_MAX_SPLITS) {
            Ok(()) => done = end,
            Err(GaitError::StepTooLarge) if level < GAIT_RIG_MAX_SPLITS => level += 1,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// What [`step_gait_rigs`] reads from each rig.
type StepRig = (
    Entity,
    &'static mut GaitRig,
    &'static SkinnedMesh,
    Option<&'static mut Transform>,
    Option<&'static mut GaitState>,
    Option<&'static GaitControls>,
    Option<&'static GaitVelocity>,
    Option<&'static mut GaitRigStatus>,
);

/// Step every [`GaitRig`] by the frame time. Runs in `PostUpdate` before `apply_animation`.
pub fn step_gait_rigs(
    mut commands: Commands,
    time: Res<regia_app::Time>,
    probes: Option<Res<GroundProbes>>,
    gaits: Option<Res<GaitAssets>>,
    lod_camera: Option<Res<crate::AnimLodCamera>>,
    mut rigs: Query<StepRig, Without<regia_scene::layers::LayerParked>>,
) {
    if rigs.is_empty() {
        return;
    }
    let _span = tracing::debug_span!("gait_rig_step").entered();
    let dt = time.delta_secs();
    let camera = lod_camera.as_ref().map(|c| c.position);
    for (entity, mut rig, mesh, transform, state, controls, velocity, status) in &mut rigs {
        let rig = &mut *rig;
        if let (Some(handle), Some(gaits)) = (rig.asset.as_ref(), gaits.as_deref())
            && let Some(asset) = gaits.get(handle)
        {
            let generation = gaits.generation(handle);
            rig.apply_asset(asset, generation);
        }
        let resolved = resolve(rig, &mesh.skeleton);
        let at = transform.as_deref().map_or(Vec3::ZERO, |t| t.translation);
        let lod = rig.lod.select(camera_distance(camera, at));
        rig.current_lod = lod;
        let count_ok = rig.legs.len() <= MAX_LEGS && rig.legs.len() == rig.params.legs();
        let mut report = status.as_deref().copied().unwrap_or_default();
        report.probe_installed = probes.is_some();
        report.legs_resolved = rig.resolved_count;
        report.issue = if count_ok {
            rig.issue
        } else {
            GaitRigIssue::LegCount
        };
        // One warning per resolve that left something unresolved, and one when the leg count
        // goes wrong; not one per frame.
        let count_went_wrong = !count_ok && rig.count_ok;
        if (resolved && rig.issue != GaitRigIssue::None) || count_went_wrong {
            report.warnings = report.warnings.saturating_add(1);
        }
        if count_went_wrong {
            tracing::warn!(
                code = "gait_rig_leg_count",
                legs = rig.legs.len(),
                pattern_legs = rig.params.legs(),
                "gait rig leg count does not match its patterns"
            );
        }
        rig.count_ok = count_ok;
        report.lod = lod;
        // Off: a driven rig is neither stepped nor posed, and starts from rest when it is back.
        let driven_off = lod == IkLod::Off && rig.mode == GaitRigMode::Driven;
        if driven_off {
            rig.restart = true;
            rig.last_translation = None;
        }
        if count_ok && !driven_off {
            // Non-finite solver or probe settings: refused. A rig without a state yet still
            // gets one, at rest on its floor plane with no probe cast, so the agent anchor
            // exists; it is not stepped after that until the settings are finite.
            let settings_ok = rig.settings_finite();
            let has_state = state.is_some() || rig.pending_state.is_some();
            let step = if !settings_ok && has_state {
                Err(GaitError::Settings)
            } else {
                step_rig(
                    rig,
                    RigStep {
                        entity,
                        transform,
                        state: state.as_deref(),
                        controls: controls.copied(),
                        velocity: velocity.map(|v| v.linear),
                        probes: if settings_ok { probes.as_deref() } else { None },
                        dt,
                        lod,
                        phase: entity.index_u32(),
                    },
                )
            };
            if !settings_ok {
                tracing::debug!(
                    code = "gait_rig_step",
                    "gait rig solver or probe settings are not finite"
                );
                report.step_errors = report.step_errors.saturating_add(1);
                report.warnings = report.warnings.saturating_add(1);
                report.issue = GaitRigIssue::Step;
            }
            match step {
                Ok(next) => {
                    if let Some(mut slot) = state {
                        if *slot != next {
                            *slot = next;
                        }
                        rig.pending_state = None;
                    } else {
                        rig.pending_state = Some(next);
                        commands.entity(entity).insert(next);
                    }
                }
                Err(_) if !settings_ok => {}
                Err(error) => {
                    tracing::debug!(code = %error, "gait rig step refused");
                    report.step_errors = report.step_errors.saturating_add(1);
                    report.warnings = report.warnings.saturating_add(1);
                    report.issue = GaitRigIssue::Step;
                }
            }
        }
        if let Some(mut slot) = status {
            if *slot != report {
                *slot = report;
            }
        } else {
            commands.entity(entity).insert(report);
        }
    }
}

/// Fraction of one stance per slice the speed cap allows: rounding in the stepper's
/// `dt / step_period` must stay at or under 1.
const SPEED_CAP_MARGIN: f32 = 1.0 - 1.0 / 1024.0;

/// Ground speed at which a slice of `h` seconds is just under one stance (ADR-10053).
fn speed_cap(params: &GaitParams, h: f32) -> f32 {
    params.step_length / h * SPEED_CAP_MARGIN
}

/// `velocity` with its horizontal (XZ) length at most `cap`; Y is kept.
fn cap_ground_velocity(velocity: [f32; 3], cap: f32) -> [f32; 3] {
    let speed = velocity[0].hypot(velocity[2]);
    if speed > cap && speed.is_finite() {
        let k = cap / speed;
        [velocity[0] * k, velocity[1], velocity[2] * k]
    } else {
        velocity
    }
}

/// An integrating slice at the speed cap: the state's ground velocity and the commanded
/// velocity both at most [`speed_cap`], so the stepper's acceleration (a move toward the
/// command) stays under it too. Atomic like the stepper.
fn step_capped(
    params: &GaitParams,
    state: &mut GaitState,
    input: [f32; 3],
    h: f32,
    ground: impl GaitGround,
) -> Result<(), GaitError> {
    let cap = speed_cap(params, h);
    let [forward, strafe, steer] = input.map(|v| v.clamp(-1.0, 1.0));
    let along = forward
        * if forward >= 0.0 {
            params.forward_speed
        } else {
            params.reverse_speed
        };
    let command = along.hypot(strafe * params.strafe_speed);
    let k = if command > cap && command.is_finite() {
        cap / command
    } else {
        1.0
    };
    let mut next = *state;
    next.velocity = cap_ground_velocity(next.velocity, cap);
    step_gait(params, &mut next, [forward * k, strafe * k, steer], h, ground)?;
    *state = next;
    Ok(())
}

struct RigStep<'a> {
    entity: Entity,
    transform: Option<Mut<'a, Transform>>,
    state: Option<&'a GaitState>,
    controls: Option<GaitControls>,
    velocity: Option<Vec3>,
    probes: Option<&'a GroundProbes>,
    dt: f32,
    lod: IkLod,
    phase: u32,
}

/// The settings the stepper sees: measured hips in the mode's frame, scaled to the rig.
fn effective_params(rig: &GaitRig, mode: GaitMode, scale: f32) -> GaitParams {
    let mut params = rig.params;
    if rig.auto_hips {
        for (i, leg) in rig.resolved.iter().enumerate().take(MAX_LEGS) {
            if leg.solver.is_some() && rig.hip_overrides & (1 << i) == 0 {
                let [a, b] = convert_offset(rig.tips[i], mode);
                params.hip_offsets[i] = [a * scale, b * scale];
            }
        }
    }
    params
}

fn step_rig(rig: &mut GaitRig, input: RigStep<'_>) -> Result<GaitState, GaitError> {
    let RigStep {
        entity,
        mut transform,
        state,
        controls,
        velocity,
        probes,
        dt,
        lod,
        phase,
    } = input;
    let current = transform.as_deref().copied().unwrap_or_default();
    let scale = current.scale.x;
    if !scale.is_finite() || scale <= 0.0 {
        return Err(GaitError::Input);
    }
    let gait_mode = match rig.mode {
        GaitRigMode::Driven => GaitMode::Driven,
        GaitRigMode::Integrating => GaitMode::Integrating,
    };
    let params = effective_params(rig, gait_mode, scale);
    let floor = current.translation.y + scale * rig.floor;
    let ground = match probes {
        Some(probes) => RigGround::Probe(ProbeGround {
            probes,
            ignore: entity,
            height: rig.probe_height,
            length: rig.probe_length,
        }),
        None => RigGround::Flat(FlatGround(match rig.mode {
            GaitRigMode::Driven => floor,
            GaitRigMode::Integrating => rig.flat_floor,
        })),
    };
    // Level of detail: an integrating rig that is off still steps (it moves its own transform),
    // at the reduced cadence. Only a real probe is cached; the flat floor is exact anyway.
    let cadence_lod = if lod == IkLod::Off { IkLod::Reduced } else { lod };
    let probing = rig
        .cadence
        .probe_now(cadence_lod, &rig.lod, phase, !rig.hits.is_empty());
    let recorder = std::cell::RefCell::new(HitCache::default());
    let cached = rig.hits;
    let ground = match (ground, cadence_lod, probing) {
        (RigGround::Probe(_), IkLod::Reduced, true) => LodGround::Record(ground, &recorder),
        (RigGround::Probe(_), IkLod::Reduced, false) => LodGround::Cached(&cached),
        _ => LodGround::Live(ground),
    };
    let mut run = || -> Result<GaitState, GaitError> {
        // The engine's component wins; the pending copy bridges the frame its insert is
        // deferred. A driven rig back from off starts from rest (its state is from where it left).
        // `restart` is cleared only once a step commits (below), so a refused frame keeps it.
        let existing = if rig.restart {
            None
        } else {
            state.copied().or(rig.pending_state)
        };
        let existing = existing.filter(|s| s.leg_count() == params.legs() && s.mode == gait_mode);
        let yaw = rig_yaw(current.rotation);
        match rig.mode {
            GaitRigMode::Driven => {
                // The derived velocity covers the move over the time the gait advances: a frame
                // clamped to eight substeps moves the body as far in less time.
                let (_, clamped) = substeps(dt);
                // `last_translation` is written only when the step commits (below).
                let linear = velocity.unwrap_or_else(|| match rig.last_translation {
                    Some(last) if clamped > 0.0 => (current.translation - last) / clamped,
                    _ => Vec3::ZERO,
                });
                let body = GaitBody {
                    position: [
                        current.translation.x,
                        floor + params.clearance,
                        current.translation.z,
                    ],
                    yaw,
                    velocity: [linear.x, linear.y, linear.z],
                };
                let Some(mut next) = existing else {
                    return GaitState::at_rest_driven(&params, body, ground);
                };
                let from = [next.position[0], next.position[2]];
                let (dx, dz) = (body.position[0] - from[0], body.position[2] - from[1]);
                if dx * dx + dz * dz > params.step_length * params.step_length {
                    // A frame that moved the body more than one step (a teleport, or a long frame
                    // at a high time scale) is the stepper's replant: one call with the final
                    // body. Sliced, each piece would stay under a step and the feet would trail.
                    step_gait_driven(&params, &mut next, body, clamped, ground)?;
                    return Ok(next);
                }
                let turn = wrap_pi(yaw - next.yaw);
                let start_yaw = next.yaw;
                run_slices(&mut next, dt, |next, f, h, finest| {
                    let mut sub = body;
                    if f < 1.0 {
                        sub.position[0] = from[0] + (body.position[0] - from[0]) * f;
                        sub.position[2] = from[1] + (body.position[2] - from[1]) * f;
                        sub.yaw = start_yaw + turn * f;
                    }
                    match step_gait_driven(&params, next, sub, h, ground) {
                        // The finest slice: the feet keep the cadence of the speed cap.
                        Err(GaitError::StepTooLarge) if finest => {
                            sub.velocity = cap_ground_velocity(sub.velocity, speed_cap(&params, h));
                            step_gait_driven(&params, next, sub, h, ground)
                        }
                        other => other,
                    }
                })?;
                Ok(next)
            }
            GaitRigMode::Integrating => {
                let Some(mut next) = existing else {
                    rig.flat_floor = floor;
                    let ground = match ground {
                        LodGround::Live(RigGround::Flat(_)) => {
                            LodGround::Live(RigGround::Flat(FlatGround(floor)))
                        }
                        other => other,
                    };
                    let position = [
                        current.translation.x,
                        floor + params.clearance,
                        current.translation.z,
                    ];
                    let state =
                        GaitState::at_rest(&params, position, wrap_pi(yaw + core::f32::consts::PI), ground)?;
                    write_rig_transform(&mut transform, &state, &params, scale, rig.floor);
                    return Ok(state);
                };
                let c = controls.unwrap_or_default();
                let input = [c.forward, -c.strafe, c.steer];
                run_slices(&mut next, dt, |next, _, h, finest| {
                    match step_gait(&params, next, input, h, ground) {
                        Err(GaitError::StepTooLarge) if finest => {
                            step_capped(&params, next, input, h, ground)
                        }
                        other => other,
                    }
                })?;
                write_rig_transform(&mut transform, &next, &params, scale, rig.floor);
                Ok(next)
            }
        }
    };
    let result = run();
    if result.is_ok() {
        rig.restart = false;
        if rig.mode == GaitRigMode::Driven {
            rig.last_translation = Some(current.translation);
        }
    }
    if let LodGround::Record(..) = ground {
        let recorded = recorder.into_inner();
        // A frame whose probes all missed keeps the older hits.
        if !recorded.is_empty() {
            rig.hits = recorded;
        }
    } else if cadence_lod == IkLod::Full {
        rig.hits = HitCache::default();
    }
    result
}

/// Integrating mode: the rig stands on the stance floor, facing the stepper's forward.
fn write_rig_transform(
    transform: &mut Option<Mut<'_, Transform>>,
    state: &GaitState,
    params: &GaitParams,
    scale: f32,
    floor: f32,
) {
    let Some(transform) = transform.as_mut() else {
        return;
    };
    let translation = Vec3::new(
        state.position[0],
        state.position[1] - params.clearance - scale * floor,
        state.position[2],
    );
    let rotation = crate::num::det_rotation_y(state.yaw - core::f32::consts::PI);
    if transform.translation != translation || transform.rotation != rotation {
        transform.translation = translation;
        transform.rotation = rotation;
    }
}

/// Pose one rig: lift the body joint to the stance floor and solve every resolved leg to its
/// foot target. Called by `apply_animation` on its working pose, after the ragdoll blend and
/// pose-mode foot placement, before eye gaze.
pub(crate) fn pose_gait_rig(
    rig: &mut GaitRig,
    state: Option<&GaitState>,
    skeleton: &Skeleton,
    pose: &mut Pose,
    transform: &Transform,
) {
    let Some(state) = state.or(rig.pending_state.as_ref()).copied() else {
        return;
    };
    // Off: the clip pose (ADR-8728).
    if rig.current_lod == IkLod::Off
        || !rig.count_ok
        || std::ptr::from_ref(skeleton) as usize != rig.resolved_key
    {
        return;
    }
    let _span = tracing::debug_span!("gait_rig_pose").entered();
    let scale = transform.scale.x;
    if !scale.is_finite() || scale <= 0.0 || pose.len() != skeleton.len() {
        return;
    }
    let to_object =
        Mat4::from_scale_rotation_translation(Vec3::splat(scale), transform.rotation, transform.translation)
            .inverse();
    // Body: the stance floor minus the rig's bind floor, as a world-vertical offset.
    if let Some(body) = rig.body_joint.map(|b| b as usize).filter(|&b| b < pose.len()) {
        let stance_floor = state.position[1] - rig.params.clearance;
        let rig_floor = transform.translation.y + scale * rig.floor;
        let lift = stance_floor - rig_floor;
        if lift.is_finite() && lift != 0.0 {
            let delta = to_object.transform_vector3(Vec3::new(0.0, lift, 0.0));
            let local = match skeleton.parent_of(body) {
                Some(parent) => {
                    // Only the parent's chain is read.
                    refresh_global(skeleton, pose, parent, &mut rig.globals);
                    rig.globals
                        .get(parent)
                        .map(|m| m.inverse().transform_vector3(delta))
                        .unwrap_or(delta)
                }
                None => delta,
            };
            if local.is_finite() {
                pose.translations[body] += local;
            }
        }
    }
    let settings = ChainIkSettings {
        iterations: rig.ik_iterations,
        tolerance: rig.ik_tolerance,
        weight: 1.0,
    };
    let legs = state.leg_count().min(rig.resolved.len());
    for i in 0..legs {
        let leg = &rig.resolved[i];
        let Some(solver) = leg.solver.as_ref() else {
            continue;
        };
        let foot = Vec3::from_array(state.feet[i].position);
        let target = to_object.transform_point3(foot) + Vec3::Y * leg.lift;
        if !target.is_finite() {
            continue;
        }
        match solver {
            LegSolver::TwoBone(chain) => {
                // Forward kinematics along this leg's chain only; a full rebuild per leg cost
                // six skeleton passes per hexapod frame.
                solve_two_bone_with_globals(skeleton, pose, chain, target, None, 1.0, &mut rig.globals);
            }
            // A non-finite tolerance is refused (`gait_rig_step`), not read as "solved".
            LegSolver::Chain(_) if !rig.ik_tolerance.is_finite() => {}
            LegSolver::Chain(chain) => {
                solve_chain(
                    skeleton,
                    pose,
                    chain,
                    target,
                    None,
                    ChainSolver::Fabrik,
                    settings,
                    &mut rig.scratch,
                );
            }
        }
    }
}

/// Register the types and the step system.
pub(crate) fn register(app: &mut regia_app::App) {
    use regia_app::{PostUpdate, PostUpdateSet};
    app.register_type::<GaitRig>()
        .register_type::<GaitLeg>()
        .register_type::<GaitRigMode>()
        .register_type::<GaitControls>()
        .register_type::<GaitVelocity>()
        .register_type::<GaitRigStatus>()
        .register_type::<GaitRigIssue>()
        .register_type::<Option<String>>()
        .add_systems(
            PostUpdate,
            step_gait_rigs
                .before(crate::systems::apply_animation)
                .in_set(PostUpdateSet::AnimationApply),
        );
}

// Read-only views of the resolved legs for debug draw (the locomotion gizmos, ADR-8729).
mod inspect;
pub use inspect::GaitLegChain;

#[cfg(test)]
mod hit_cache_tests {
    use super::*;

    fn slices(dt: f32, refuse_above: f32) -> (Result<(), GaitError>, Vec<(f32, f32)>, u32) {
        let mut seen = Vec::new();
        let mut refused = 0;
        let result = run_slices(&mut (), dt, |_, f, h, _| {
            if h > refuse_above {
                refused += 1;
                return Err(GaitError::StepTooLarge);
            }
            seen.push((f, h));
            Ok(())
        });
        (result, seen, refused)
    }

    /// Without a refusal the slices are `substeps`' equal pieces, bit for bit (the pre-ADR-8800
    /// schedule), with the interpolation fractions `k / n`.
    #[test]
    fn run_slices_without_refusal_is_the_equal_substep_schedule() {
        for dt in [1.0 / 144.0_f32, 1.0 / 60.0, 1.0 / 30.0, 0.1, 0.25, 1.0] {
            let (n, clamped) = substeps(dt);
            let (result, seen, refused) = slices(dt, f32::INFINITY);
            assert_eq!(result, Ok(()));
            assert_eq!(refused, 0);
            let want: Vec<(f32, f32)> = (1..=n)
                .map(|k| {
                    let f = if k == n { 1.0 } else { k as f32 / n as f32 };
                    (f, clamped / n as f32)
                })
                .collect();
            let bits = |v: &[(f32, f32)]| {
                v.iter()
                    .map(|(f, h)| (f.to_bits(), h.to_bits()))
                    .collect::<Vec<_>>()
            };
            assert_eq!(bits(&seen), bits(&want), "dt {dt}");
        }
    }

    /// A refused 1/30 s slice is retried as two 1/60 s slices ending at fractions 0.5 and 1.
    #[test]
    fn run_slices_halves_a_refused_slice() {
        let (result, seen, refused) = slices(1.0 / 30.0, 0.02);
        assert_eq!(result, Ok(()));
        assert_eq!(refused, 1);
        assert_eq!(seen, vec![(0.5, 1.0 / 60.0), (1.0, 1.0 / 60.0)]);
    }

    /// A stepper that refuses every length gives up after `GAIT_RIG_MAX_SPLITS` halvings.
    #[test]
    fn run_slices_stops_after_the_split_budget() {
        let (result, seen, refused) = slices(1.0 / 30.0, 0.0);
        assert_eq!(result, Err(GaitError::StepTooLarge));
        assert!(seen.is_empty());
        assert_eq!(refused, GAIT_RIG_MAX_SPLITS + 1);
    }

    /// Only a slice at `GAIT_RIG_MAX_SPLITS` halvings is flagged finest (ADR-10053), and a step
    /// that accepts there (the rig's speed cap) finishes the frame at that slice length.
    #[test]
    fn run_slices_flags_only_the_finest_slice() {
        let finest = 1.0 / 30.0 / (1_u32 << GAIT_RIG_MAX_SPLITS) as f32;
        let mut calls = Vec::new();
        let result = run_slices(&mut (), 1.0 / 30.0, |_, _, h, is_finest| {
            calls.push((h, is_finest));
            if is_finest || h <= finest * 1.5 {
                assert!(is_finest, "a slice of {h} s is the finest but was not flagged");
                return Ok(());
            }
            Err(GaitError::StepTooLarge)
        });
        assert_eq!(result, Ok(()));
        let flagged = calls.iter().filter(|(_, f)| *f).count();
        assert_eq!(flagged, 1 << GAIT_RIG_MAX_SPLITS, "{calls:?}");
        assert_eq!(
            calls.len(),
            GAIT_RIG_MAX_SPLITS as usize + (1 << GAIT_RIG_MAX_SPLITS)
        );
        assert!(calls.iter().all(|&(h, f)| f == (h <= finest * 1.5)));
    }

    /// Reduced detail records the probe hit before `sample_ground` rejects a non-finite
    /// height, then replays it on the frames that do not probe. `sample_ground` turns that
    /// replay into `GaitError::Floor` for the rest of the interval. A cache miss (or a
    /// finite substitute) is the safe answer.
    #[test]
    fn review_proof_reduced_cache_does_not_replay_a_non_finite_probe() {
        for y in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut cache = HitCache::default();
            cache.record(
                0.0,
                0.0,
                GroundHit {
                    y,
                    normal: [0.0, 1.0, 0.0],
                },
            );
            let hit = cache.sample(0.2, 0.0);
            assert!(
                hit.is_none_or(|hit| hit.y.is_finite() && hit.normal.iter().all(|n| n.is_finite())),
                "non-finite probe y={y} was replayed: {hit:?}"
            );
        }
    }
}
