// SPDX-License-Identifier: MPL-2.0
//! Rig node evaluators and the camera pose (`docs/specs/35-camera-framework.md` §§ 4.2-4.4, 5).
//!
//! Everything here is a pure function of its inputs: no wall clock, no world access except
//! through the read-only [`CameraWorld`] trait, so the same inputs and `dt` give the same pose.

use std::collections::BTreeMap;

use std::sync::Arc;

use regia_core::math::{
    Quat, Vec3, det_atan2, det_exp, det_nlerp, det_quat_from_yaw_pitch_roll, det_yaw_pitch_roll,
};
use regia_scene::spline::SplineCurve;

use super::asset::{
    CamSpace, CameraAsset, CollisionAnchor, Condition, InputSlotDef, Node, ParamValue, PivotSpace, RigDef,
    SlotKind, TargetRef, Value, VarValue,
};
use super::crane::{self, CraneAxes, CraneBase, CraneReport};
use super::rail::{self, MIN_RAIL_LENGTH_M, RailDrive, RailOrient, RailReport};

/// Skin kept between the probe sphere and geometry (metres), as in the legacy rig.
pub const PROBE_SKIN: f32 = 0.05;
/// 0.1 m. The collision node does not raise a hit to this distance or to the probe
/// radius: the boom is the hit minus [`PROBE_SKIN`], clamped so the probe centre stays
/// on the near side of the hit, and that distance may be 0.
pub const PROBE_MIN_DISTANCE: f32 = 0.1;
/// A `close_speed` at or above this snaps (the legacy rig retracts instantly).
pub const SNAP_SPEED: f32 = 1.0e8;

/// The share of the remaining distance covered in one step by exponential smoothing at rate
/// `k` (1/s): `1 − exp(−k·dt)`, clamped into `[0, 1]`. Bad input yields 0. The exponential is the
/// pinned [`det_exp`] series, so the stored factor is bit-identical across platforms (spec 35
/// § 12.13).
pub fn smoothing_factor(k: f32, dt: f32) -> f32 {
    let t = 1.0 - det_exp(-k * dt);
    if t.is_finite() { t.clamp(0.0, 1.0) } else { 0.0 }
}

/// Wraps an angle difference into `(-π, π]`.
pub fn wrap_pi(angle: f32) -> f32 {
    if !angle.is_finite() {
        return 0.0;
    }
    let wrapped = angle.rem_euclid(std::f32::consts::TAU);
    if wrapped > std::f32::consts::PI {
        wrapped - std::f32::consts::TAU
    } else {
        wrapped
    }
}

/// The camera pose a rig produces (§ 4.2). Angles are radians, yaw about +Y (0 faces −Z,
/// positive turns toward −X), positive pitch looks up; rotation is `Ry(yaw)·Rx(pitch)·Rz(roll)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraPose {
    /// World position in metres.
    pub position: Vec3,
    /// Yaw in radians.
    pub yaw: f32,
    /// Pitch in radians.
    pub pitch: f32,
    /// Roll in radians.
    pub roll: f32,
    /// Vertical field of view in degrees.
    pub fov_deg: f32,
    /// The point the rig aims at, if it has one.
    pub aim: Option<Vec3>,
    /// The pose is a cut: consumers should not interpolate from the previous frame.
    pub cut: bool,
}

/// The vertical field of view a pose starts with (degrees).
pub const DEFAULT_FOV_DEG: f32 = 60.0;

impl Default for CameraPose {
    fn default() -> Self {
        Self {
            position: Vec3::ZERO,
            yaw: 0.0,
            pitch: 0.0,
            roll: 0.0,
            fov_deg: DEFAULT_FOV_DEG,
            aim: None,
            cut: false,
        }
    }
}

impl CameraPose {
    /// The rotation `Ry(yaw)·Rx(pitch)·Rz(roll)`, from the pinned [`det_quat_from_yaw_pitch_roll`]
    /// euler construction (spec 35 § 12.13).
    pub fn rotation(&self) -> Quat {
        det_quat_from_yaw_pitch_roll(self.yaw, self.pitch, self.roll)
    }

    /// Sets yaw, pitch and roll from a rotation, with the pinned [`det_yaw_pitch_roll`] extraction
    /// (the same bits on every platform).
    pub fn set_rotation(&mut self, rotation: Quat) {
        let (yaw, pitch, roll) = det_yaw_pitch_roll(rotation);
        self.yaw = yaw;
        self.pitch = pitch;
        self.roll = roll;
    }

    /// The view direction.
    pub fn forward(&self) -> Vec3 {
        self.rotation() * Vec3::NEG_Z
    }

    /// The view's right axis.
    pub fn right(&self) -> Vec3 {
        self.rotation() * Vec3::X
    }

    /// The view's up axis.
    pub fn up(&self) -> Vec3 {
        self.rotation() * Vec3::Y
    }

    /// Whether every component is finite.
    pub fn is_finite(&self) -> bool {
        self.position.is_finite()
            && self.yaw.is_finite()
            && self.pitch.is_finite()
            && self.roll.is_finite()
            && self.fov_deg.is_finite()
            && self.aim.is_none_or(|a| a.is_finite())
    }

    /// Turns the pose to look from its position toward `point`; roll is kept. A point on top
    /// of the position leaves the rotation alone.
    pub fn look_at(&mut self, point: Vec3) {
        let direction = point - self.position;
        let length = direction.length();
        if length < 1.0e-5 || !length.is_finite() {
            return;
        }
        let f = direction / length;
        self.pitch = regia_core::math::det_asin(f.y.clamp(-1.0, 1.0));
        if f.x.abs() > 1.0e-4 || f.z.abs() > 1.0e-4 {
            self.yaw = det_atan2(-f.x, -f.z);
        }
    }
}

/// Weighted combination of poses (§ 6.2): position and fov by weighted average, rotation by
/// weighted quaternion nlerp along the shorter arc, roll folded into the rotation, aim by
/// weighted average over the poses that have one, `cut` true when a component with weight ≥ 0.5
/// is a cut. `weights` need not sum to 1; they are normalized. Empty input gives the default.
pub fn combine(poses: &[(CameraPose, f32)]) -> CameraPose {
    let total: f32 = poses.iter().map(|(_, w)| w.max(0.0)).sum();
    if poses.is_empty() || total <= 0.0 {
        return poses.first().map(|(p, _)| *p).unwrap_or_default();
    }
    if poses.len() == 1 {
        return poses[0].0;
    }
    let mut position = Vec3::ZERO;
    let mut fov = 0.0;
    let mut aim = Vec3::ZERO;
    let mut aim_weight = 0.0;
    let mut rotation = Quat::from_xyzw(0.0, 0.0, 0.0, 0.0);
    let mut cut = false;
    let reference = poses[0].0.rotation();
    for (pose, weight) in poses {
        let w = weight.max(0.0) / total;
        position += pose.position * w;
        fov += pose.fov_deg * w;
        if let Some(a) = pose.aim {
            aim += a * w;
            aim_weight += w;
        }
        let mut q = pose.rotation();
        if q.dot(reference) < 0.0 {
            q = -q;
        }
        rotation = Quat::from_xyzw(
            rotation.x + q.x * w,
            rotation.y + q.y * w,
            rotation.z + q.z * w,
            rotation.w + q.w * w,
        );
        cut |= pose.cut && w >= 0.5;
    }
    let mut out = CameraPose {
        position,
        fov_deg: fov,
        aim: (aim_weight > 0.0).then(|| aim / aim_weight),
        cut,
        ..CameraPose::default()
    };
    if rotation.length_squared() > 1.0e-12 {
        out.set_rotation(rotation.normalize());
    } else {
        out.yaw = det_yaw_pitch_roll(reference).0;
    }
    out
}

/// Per-node runtime state (the damping the nodes keep). Plain data so `share_state` can merge it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct NodeState {
    /// The state has been initialized.
    pub init: bool,
    /// A position state (smoothed position).
    pub v: Vec3,
    /// Scalar states (boom length, collision distance, last yaw, lean, ...).
    pub f: [f32; 3],
    /// The last collision cast result (`NaN` = no hit), for `reuse_last_frame`.
    pub last_hit: f32,
    /// A warning has been logged for this node.
    pub warned: bool,
}

impl NodeState {
    /// The state merged toward `other` by `t` (0 = self, 1 = other).
    pub fn lerp(self, other: NodeState, t: f32) -> NodeState {
        let mix = |a: f32, b: f32| a + (b - a) * t;
        NodeState {
            init: self.init || other.init,
            v: self.v.lerp(other.v, t),
            f: [
                mix(self.f[0], other.f[0]),
                mix(self.f[1], other.f[1]),
                mix(self.f[2], other.f[2]),
            ],
            last_hit: self.last_hit,
            warned: self.warned,
        }
    }
}

/// How many state slots a node list uses (every node, nested ones included, takes one).
pub fn count_nodes(nodes: &[Node]) -> usize {
    nodes.iter().map(|n| 1 + count_nodes(n.children())).sum()
}

/// Read-only world access for nodes. The driver's implementation reads ECS queries and the
/// physics world; tests use a mock.
pub trait CameraWorld {
    /// The position and rotation of a target, or `None` when it cannot be resolved.
    fn target(&self, target: &TargetRef) -> Option<(Vec3, Quat)>;

    /// Casts a sphere of `radius` from `from` along unit `dir` for at most `max` metres against
    /// `layers` (empty = all); returns the distance the sphere centre travels before touching.
    fn cast_sphere(&self, from: Vec3, dir: Vec3, max: f32, radius: f32, layers: &[String]) -> Option<f32>;

    /// The world-space curve of the spline entity named `name`, or `None` when there is no such
    /// entity (the default: a world without rails). A rail camera or crane base reads it.
    fn rail(&self, _name: &str) -> Option<Arc<SplineCurve>> {
        None
    }
}

/// A world with nothing in it: no entities and no physics; fixed points still resolve.
pub struct EmptyWorld;

impl CameraWorld for EmptyWorld {
    fn target(&self, target: &TargetRef) -> Option<(Vec3, Quat)> {
        match target {
            TargetRef::Point(p) => Some((Vec3::from(*p), Quat::IDENTITY)),
            _ => None,
        }
    }

    fn cast_sphere(&self, _: Vec3, _: Vec3, _: f32, _: f32, _: &[String]) -> Option<f32> {
        None
    }
}

// ---------------------------------------------------------------------------
// Variables and input slots
// ---------------------------------------------------------------------------

/// The runtime variable table of one driver (or one rig's blended view of it).
#[derive(Clone, Debug, Default)]
pub struct VarTable {
    /// Variable names, in declaration order.
    pub names: Vec<String>,
    /// Current values, parallel to `names`.
    pub values: Vec<VarValue>,
    /// Written since the last [`VarTable::end_frame`], parallel to `names`.
    pub written: Vec<bool>,
    /// Never blended between rigs, parallel to `names`.
    pub local_only: Vec<bool>,
}

impl VarTable {
    /// The table of an asset: declared variables, then parameters that seed one.
    pub fn from_asset(asset: &CameraAsset) -> Self {
        let mut table = VarTable::default();
        for var in &asset.variables {
            table.push(&var.name, var.default, var.local_only);
        }
        for (name, value) in &asset.params {
            if table.index(name).is_some() {
                continue;
            }
            let seed = match value {
                ParamValue::Bool(v) => VarValue::Bool(*v),
                ParamValue::Float(v) => VarValue::Float(*v),
                ParamValue::Int(v) => VarValue::Float(*v as f32),
                ParamValue::Vec3(v) => VarValue::Vec3(*v),
                ParamValue::Name(_) => continue,
            };
            table.push(name, seed, false);
        }
        table
    }

    fn push(&mut self, name: &str, value: VarValue, local_only: bool) {
        self.names.push(name.to_owned());
        self.values.push(value);
        self.written.push(false);
        self.local_only.push(local_only);
    }

    /// The index of a variable.
    pub fn index(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|n| n == name)
    }

    /// A variable's value.
    pub fn get(&self, name: &str) -> Option<VarValue> {
        self.index(name).map(|i| self.values[i])
    }

    /// A boolean view: `Bool` directly, `Float` as non-zero.
    pub fn get_bool(&self, name: &str) -> Option<bool> {
        match self.get(name)? {
            VarValue::Bool(v) => Some(v),
            VarValue::Float(v) => Some(v != 0.0),
            _ => None,
        }
    }

    /// A numeric view: `Float` directly, `Bool` as 0 or 1.
    pub fn get_f32(&self, name: &str) -> Option<f32> {
        match self.get(name)? {
            VarValue::Float(v) => Some(v),
            VarValue::Bool(v) => Some(f32::from(v)),
            _ => None,
        }
    }

    /// Writes a variable; the type must match the variable's. Returns false on a type mismatch,
    /// a non-finite value or an unknown name.
    pub fn set(&mut self, name: &str, value: VarValue) -> bool {
        let Some(i) = self.index(name) else {
            return false;
        };
        if !value.is_finite() || std::mem::discriminant(&self.values[i]) != std::mem::discriminant(&value) {
            return false;
        }
        self.values[i] = value;
        self.written[i] = true;
        true
    }

    /// Whether the variable was written since the last [`end_frame`](Self::end_frame).
    pub fn written_this_frame(&self, name: &str) -> bool {
        self.index(name).is_some_and(|i| self.written[i])
    }

    /// Clears the written flags (the driver calls it at the end of each frame).
    pub fn end_frame(&mut self) {
        self.written.iter_mut().for_each(|w| *w = false);
    }

    /// Fills `self` (same layout as `global`) with `global` blended by `weight` against `frozen`
    /// (the values when the rig became active): floats and vec3 linearly, bools by threshold 0.5
    /// on the weight, yaw/pitch by shortest path; `local_only` variables are never blended.
    pub fn blend_view(&mut self, global: &VarTable, frozen: &[VarValue], weight: f32) {
        self.names.clone_from(&global.names);
        self.local_only.clone_from(&global.local_only);
        self.written.clone_from(&global.written);
        self.values.clear();
        for (i, current) in global.values.iter().enumerate() {
            let old = frozen.get(i).copied().unwrap_or(*current);
            let w = weight.clamp(0.0, 1.0);
            let value = if global.local_only[i] || w >= 1.0 {
                *current
            } else {
                match (old, *current) {
                    (VarValue::Float(a), VarValue::Float(b)) => VarValue::Float(a + (b - a) * w),
                    (VarValue::Vec3(a), VarValue::Vec3(b)) => {
                        let (a, b) = (Vec3::from(a), Vec3::from(b));
                        VarValue::Vec3(a.lerp(b, w).to_array())
                    }
                    (VarValue::Bool(a), VarValue::Bool(b)) => VarValue::Bool(if w >= 0.5 { b } else { a }),
                    (VarValue::YawPitch(ay, ap), VarValue::YawPitch(by, bp)) => {
                        VarValue::YawPitch(ay + wrap_pi(by - ay) * w, ap + (bp - ap) * w)
                    }
                    (_, b) => b,
                }
            };
            self.values.push(value);
        }
    }
}

/// The state of one input slot.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SlotState {
    /// The raw per-frame value gameplay wrote.
    pub raw: [f32; 2],
    /// Accumulated yaw in radians.
    pub yaw: f32,
    /// Accumulated pitch in radians.
    pub pitch: f32,
}

/// All input slots of one driver.
#[derive(Clone, Debug, Default)]
pub struct InputBank {
    /// Slot definitions (copied from the asset).
    pub defs: Vec<InputSlotDef>,
    /// Slot states, parallel to `defs`.
    pub states: Vec<SlotState>,
}

impl InputBank {
    /// The bank of an asset.
    pub fn from_asset(asset: &CameraAsset) -> Self {
        Self {
            defs: asset.inputs.clone(),
            states: vec![SlotState::default(); asset.inputs.len()],
        }
    }

    /// The index of a slot.
    pub fn index(&self, name: &str) -> Option<usize> {
        self.defs.iter().position(|d| d.name == name)
    }

    /// Writes a slot's raw value. The value is held, like a stick position: every integration
    /// applies it until the next write (or [`reset`](Self::reset)), so an input source that only
    /// reports changes works. `Axis1` slots use `value[0]` only.
    /// Returns false for an unknown slot or a non-finite value.
    pub fn set_raw(&mut self, name: &str, value: [f32; 2]) -> bool {
        let Some(i) = self.index(name) else {
            return false;
        };
        if !value.iter().all(|v| v.is_finite()) {
            return false;
        }
        self.states[i].raw = value;
        true
    }

    /// A slot's raw value as last written.
    pub fn raw(&self, name: &str) -> Option<[f32; 2]> {
        self.index(name).map(|i| self.states[i].raw)
    }

    /// Sets a slot's accumulated angles directly (radians): a replay, a respawn, a test.
    pub fn set_angles(&mut self, name: &str, yaw: f32, pitch: f32) -> bool {
        let Some(i) = self.index(name) else {
            return false;
        };
        if !yaw.is_finite() || !pitch.is_finite() {
            return false;
        }
        self.states[i].yaw = wrap_pi(yaw);
        self.states[i].pitch = pitch;
        true
    }

    /// Resets a slot's accumulated angles and raw value.
    pub fn reset(&mut self, name: &str) -> bool {
        let Some(i) = self.index(name) else {
            return false;
        };
        self.states[i] = SlotState::default();
        true
    }

    /// A slot's accumulated `(yaw, pitch)` in radians.
    pub fn angles(&self, name: &str) -> Option<(f32, f32)> {
        self.index(name)
            .map(|i| (self.states[i].yaw, self.states[i].pitch))
    }

    /// Integrates every slot by `dt` (§ 5): `value · speed · dt` per axis, pitch clamped (yaw
    /// wrapped), optional normalization and inversion, then the pull toward a variable.
    pub fn integrate(&mut self, dt: f32, vars: &VarTable) {
        for (def, state) in self.defs.iter().zip(self.states.iter_mut()) {
            let mut raw = state.raw;
            if def.kind == SlotKind::Axis1 {
                raw[1] = 0.0;
            }
            if def.normalize {
                let length = (raw[0] * raw[0] + raw[1] * raw[1]).sqrt();
                if length > 1.0 {
                    raw[0] /= length;
                    raw[1] /= length;
                }
            }
            let pitch_sign = if def.invert_pitch { -1.0 } else { 1.0 };
            // A positive horizontal value looks right, which turns the yaw toward +X (negative yaw).
            state.yaw = wrap_pi(state.yaw - raw[0] * def.speed.0.to_radians() * dt);
            state.pitch += raw[1] * pitch_sign * def.speed.1.to_radians() * dt;
            if let Some(name) = &def.pull_toward_var
                && let Some(VarValue::YawPitch(yaw, pitch)) = vars.get(name)
            {
                let f = smoothing_factor(def.pull_weight, dt);
                state.yaw = wrap_pi(state.yaw + wrap_pi(yaw - state.yaw) * f);
                state.pitch += (pitch - state.pitch) * f;
            }
            let (low, high) = (def.pitch_clamp.0.to_radians(), def.pitch_clamp.1.to_radians());
            state.pitch = state.pitch.clamp(low.min(high), high.max(low));
        }
    }
}

// ---------------------------------------------------------------------------
// Conditions
// ---------------------------------------------------------------------------

/// What a condition can see.
pub struct CondCtx<'a> {
    /// Seconds since the rig instance started.
    pub elapsed: f32,
    /// The variable view.
    pub vars: &'a VarTable,
    /// The rig being evaluated (in transitions: the outgoing or incoming rig).
    pub rig: &'a RigDef,
}

/// Evaluates a condition (pure, allocation-free).
pub fn eval_condition(cond: &Condition, ctx: &CondCtx<'_>) -> bool {
    match cond {
        Condition::Elapsed(seconds) => ctx.elapsed >= *seconds,
        Condition::VarBool(name, expected) => ctx.vars.get_bool(name) == Some(*expected),
        Condition::VarCompare(name, op, value) => ctx.vars.get_f32(name).is_some_and(|v| op.test(v, *value)),
        Condition::HasTag(tag) => ctx.rig.tags.iter().any(|t| t == tag),
        Condition::RigIs(name) => &ctx.rig.name == name,
        Condition::All(list) => list.iter().all(|c| eval_condition(c, ctx)),
        Condition::Any(list) => list.iter().any(|c| eval_condition(c, ctx)),
        Condition::Not(inner) => !eval_condition(inner, ctx),
    }
}

// ---------------------------------------------------------------------------
// Rig evaluation
// ---------------------------------------------------------------------------

/// One entry of the node-by-node pose trace the inspector shows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TraceEntry {
    /// The node kind name.
    pub kind: &'static str,
    /// The node's index in evaluation order (nested nodes included).
    pub index: usize,
    /// The pose after the node ran (or the unchanged pose when it was skipped).
    pub pose: CameraPose,
    /// The node was skipped (invalid value, missing input).
    pub skipped: bool,
}

/// Runtime scratch of one rig instance: node states, counters and the trace. Buffers are reused
/// from frame to frame.
#[derive(Clone, Debug, Default)]
pub struct RigScratch {
    /// Node states in evaluation order.
    pub states: Vec<NodeState>,
    /// Nodes skipped so far (lifetime of the instance).
    pub skipped: u32,
    /// The trace of the last evaluation.
    pub trace: Vec<TraceEntry>,
    /// The pivot point the last evaluation set, if any.
    pub pivot: Option<Vec3>,
    /// The collision probe of the last evaluation, if a `Collision` node ran.
    pub probe: Option<Probe>,
    /// What the `Rail` nodes reported in the last evaluation.
    pub rails: Vec<RailReport>,
    /// What the `Crane` nodes reported in the last evaluation.
    pub cranes: Vec<CraneReport>,
}

/// A collision probe, for the debug overlay and the inspector.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Probe {
    /// Where the cast started.
    pub from: Vec3,
    /// Where the camera would be without a hit.
    pub wanted: Vec3,
    /// Where the camera is after the node.
    pub placed: Vec3,
    /// Sphere radius.
    pub radius: f32,
    /// The cast hit something that shortened the distance.
    pub blocked: bool,
}

/// Everything a rig evaluation reads.
pub struct EvalCtx<'a> {
    /// Step in seconds.
    pub dt: f32,
    /// Seconds since the rig instance started.
    pub elapsed: f32,
    /// The driver seed.
    pub seed: u64,
    /// The asset.
    pub asset: &'a CameraAsset,
    /// The rig.
    pub rig: &'a RigDef,
    /// Driver parameter overrides.
    pub overrides: &'a BTreeMap<String, ParamValue>,
    /// This rig's variable view.
    pub vars: &'a VarTable,
    /// The input slots.
    pub inputs: &'a InputBank,
    /// World access.
    pub world: &'a dyn CameraWorld,
    /// The rig's previous-frame final pose (the seed on its first frame).
    pub prev: CameraPose,
    /// An active `LookAt` action's aim point, if any.
    pub aim_override: Option<Vec3>,
}

/// Mutable bookkeeping inside one evaluation.
struct Cursor {
    index: usize,
    pivot: Option<Vec3>,
    look_yaw: f32,
}

/// Evaluates a rig's nodes in listed order on `pose` (§ 4.2). `pose` must be seeded by the
/// caller. A node whose result is not finite is undone and counted in `scratch.skipped`.
#[tracing::instrument(skip_all, target = "regia_framework", name = "camera_rig", fields(name = %ctx.rig.name))]
pub fn eval_rig(ctx: &EvalCtx<'_>, pose: &mut CameraPose, scratch: &mut RigScratch) {
    let total = count_nodes(&ctx.rig.root);
    if scratch.states.len() != total {
        scratch.states.clear();
        scratch.states.resize(total, NodeState::default());
    }
    scratch.trace.clear();
    scratch.pivot = None;
    scratch.probe = None;
    scratch.rails.clear();
    scratch.cranes.clear();
    pose.aim = ctx.aim_override.or(pose.aim);
    let mut cursor = Cursor {
        index: 0,
        pivot: None,
        look_yaw: pose.yaw,
    };
    eval_list(ctx, &ctx.rig.root, pose, scratch, &mut cursor);
    scratch.pivot = cursor.pivot;
}

fn eval_list(
    ctx: &EvalCtx<'_>,
    nodes: &[Node],
    pose: &mut CameraPose,
    scratch: &mut RigScratch,
    cur: &mut Cursor,
) {
    for node in nodes {
        let index = cur.index;
        cur.index += 1;
        let before = *pose;
        let before_pivot = cur.pivot;
        // A node that is skipped (evaluator refused, or a non-finite pose) must leave no trace:
        // truncate the reports and follow state its evaluation pushed before the pose rollback.
        let rails_before = scratch.rails.len();
        let cranes_before = scratch.cranes.len();
        let ok = eval_node(ctx, node, index, pose, scratch, cur);
        let finite = pose.is_finite();
        let skipped = !ok || !finite;
        if skipped {
            *pose = before;
            cur.pivot = before_pivot;
            scratch.rails.truncate(rails_before);
            scratch.cranes.truncate(cranes_before);
            scratch.skipped = scratch.skipped.saturating_add(1);
        }
        scratch.trace.push(TraceEntry {
            kind: node.kind(),
            index,
            pose: *pose,
            skipped,
        });
        // Nested nodes: the group/when evaluators below walk their children themselves; here we
        // must skip their state slots when the container did not run them.
        if matches!(node, Node::Group { .. } | Node::When { .. }) {
            let run = !skipped && container_enabled(ctx, node);
            if run {
                eval_list(ctx, node.children(), pose, scratch, cur);
            } else {
                cur.index += count_nodes(node.children());
            }
        }
    }
}

fn container_enabled(ctx: &EvalCtx<'_>, node: &Node) -> bool {
    match node {
        Node::Group { enabled, .. } => resolve_bool(ctx, enabled).unwrap_or(false),
        Node::When { cond, .. } => eval_condition(
            cond,
            &CondCtx {
                elapsed: ctx.elapsed,
                vars: ctx.vars,
                rig: ctx.rig,
            },
        ),
        _ => false,
    }
}

fn resolve_f32(ctx: &EvalCtx<'_>, value: &Value<f32>) -> Option<f32> {
    let v = match value {
        Value::Literal(v) => *v,
        Value::Param(name) => ctx
            .overrides
            .get(name)
            .or_else(|| ctx.asset.params.get(name))
            .and_then(ParamValue::as_f32)?,
        Value::Var(name) => ctx.vars.get_f32(name)?,
    };
    v.is_finite().then_some(v)
}

fn resolve_bool(ctx: &EvalCtx<'_>, value: &Value<bool>) -> Option<bool> {
    match value {
        Value::Literal(v) => Some(*v),
        Value::Param(name) => ctx
            .overrides
            .get(name)
            .or_else(|| ctx.asset.params.get(name))
            .and_then(ParamValue::as_bool),
        Value::Var(name) => ctx.vars.get_bool(name),
    }
}

fn vec3(v: [f32; 3]) -> Vec3 {
    Vec3::from(v)
}

/// Evaluates one node. Returns false when the node was skipped for an invalid value.
fn eval_node(
    ctx: &EvalCtx<'_>,
    node: &Node,
    index: usize,
    pose: &mut CameraPose,
    scratch: &mut RigScratch,
    cur: &mut Cursor,
) -> bool {
    let dt = ctx.dt;
    let state = &mut scratch.states[index];
    match node {
        Node::Pivot {
            target,
            offset,
            space,
            aim_offset,
            inherit_rotation,
        } => {
            let Some((position, rotation)) = ctx.world.target(target) else {
                if !state.warned {
                    state.warned = true;
                    tracing::warn!(target: "regia_framework", rig = %ctx.rig.name, "camera pivot target is missing");
                }
                return false;
            };
            let (offset, aim_extra) = match space {
                PivotSpace::Target => (rotation * vec3(*offset), rotation * vec3(*aim_offset)),
                PivotSpace::World => (vec3(*offset), vec3(*aim_offset)),
            };
            let point = position + offset;
            pose.position = point;
            if *inherit_rotation {
                pose.set_rotation(rotation);
                cur.look_yaw = pose.yaw;
            }
            if ctx.aim_override.is_none() {
                pose.aim = Some(point + aim_extra);
            }
            cur.pivot = Some(point);
            true
        }
        Node::LookInput { slot, additive } => {
            let Some((yaw, pitch)) = ctx.inputs.angles(slot) else {
                return false;
            };
            if *additive {
                // The look composes onto the current rotation (a first-person eye on a turning
                // target), which equals adding the angles for a level base. The euler construction
                // is the pinned one, so the composed pose is bit-identical across platforms.
                let look = det_quat_from_yaw_pitch_roll(yaw, pitch, 0.0);
                pose.set_rotation(pose.rotation() * look);
            } else {
                pose.yaw = yaw;
                pose.pitch = pitch;
            }
            cur.look_yaw = yaw;
            true
        }
        Node::Boom {
            length,
            side_offset,
            up_offset,
            length_speed,
        } => {
            let (Some(length), Some(side), Some(up), Some(speed)) = (
                resolve_f32(ctx, length),
                resolve_f32(ctx, side_offset),
                resolve_f32(ctx, up_offset),
                resolve_f32(ctx, length_speed),
            ) else {
                return false;
            };
            if length < 0.0 || speed < 0.0 {
                return false;
            }
            let effective = if !state.init || speed <= 0.0 {
                length
            } else {
                let step = speed * dt;
                let current = state.f[0];
                current + (length - current).clamp(-step, step)
            };
            state.init = true;
            state.f[0] = effective;
            let rotation = pose.rotation();
            pose.position += rotation * (Vec3::Z * effective + Vec3::X * side + Vec3::Y * up);
            if let Some(aim) = pose.aim {
                pose.look_at(aim);
            }
            true
        }
        Node::Collision {
            radius,
            layers,
            close_speed,
            open_speed,
            open_rate,
            anchor,
            reuse_last_frame,
        } => {
            if !radius.is_finite() || *radius < 0.0 {
                return false;
            }
            let anchor_point = match anchor {
                CollisionAnchor::Pivot => cur.pivot.or(pose.aim).unwrap_or(pose.position),
                CollisionAnchor::Target => ctx
                    .world
                    .target(&TargetRef::DriveTarget)
                    .map_or_else(|| cur.pivot.unwrap_or(pose.position), |(p, _)| p),
            };
            let span = pose.position - anchor_point;
            let full = span.length();
            if full < 1.0e-5 || !full.is_finite() {
                return true;
            }
            let dir = span / full;
            let _span = tracing::trace_span!(target: "regia_framework", "camera_collision").entered();
            let cast = ctx.world.cast_sphere(anchor_point, dir, full, *radius, layers);
            let hit = if *reuse_last_frame {
                let previous = if state.init { state.last_hit } else { f32::NAN };
                state.last_hit = cast.unwrap_or(f32::NAN);
                (!previous.is_nan()).then_some(previous)
            } else {
                cast
            };
            let allowed = match hit {
                Some(toi) => {
                    // `toi` is how far the sphere centre travels before contact. Subtract the skin
                    // and clamp to `[0, full]` so the centre stays on the near side of the hit.
                    // The radius and `PROBE_MIN_DISTANCE` do not raise this distance.
                    (toi - PROBE_SKIN).clamp(0.0, full)
                }
                None => full,
            };
            let effective = if !state.init {
                allowed
            } else {
                let current = state.f[1];
                if allowed < current {
                    if *close_speed >= SNAP_SPEED {
                        allowed
                    } else {
                        (current - close_speed * dt).max(allowed)
                    }
                } else if allowed > current {
                    if *open_rate > 0.0 {
                        current + (allowed - current) * smoothing_factor(*open_rate, dt)
                    } else {
                        (current + open_speed * dt).min(allowed)
                    }
                } else {
                    current
                }
            };
            state.init = true;
            state.f[1] = effective;
            pose.position = anchor_point + dir * effective;
            scratch.probe = Some(Probe {
                from: anchor_point,
                wanted: anchor_point + dir * full,
                placed: pose.position,
                radius: *radius,
                blocked: allowed < full - 1.0e-4,
            });
            if let Some(aim) = pose.aim {
                pose.look_at(aim);
            }
            true
        }
        Node::Fov { degrees } => {
            let Some(degrees) = resolve_f32(ctx, degrees) else {
                return false;
            };
            pose.fov_deg = degrees.clamp(1.0, 170.0);
            true
        }
        Node::Offset {
            translation,
            rotation_deg,
            space,
        } => {
            let t = vec3(*translation);
            match space {
                CamSpace::Camera => pose.position += pose.rotation() * t,
                CamSpace::World => pose.position += t,
            }
            pose.pitch += rotation_deg[0].to_radians();
            pose.yaw = wrap_pi(pose.yaw + rotation_deg[1].to_radians());
            pose.roll += rotation_deg[2].to_radians();
            true
        }
        Node::SmoothPosition {
            depth,
            side,
            height,
            space,
        } => {
            if !state.init {
                state.init = true;
                state.v = ctx.prev.position;
            }
            let wanted = pose.position;
            let delta = wanted - state.v;
            let axes = match space {
                CamSpace::Camera => {
                    let rotation = pose.rotation();
                    [rotation * Vec3::NEG_Z, rotation * Vec3::X, rotation * Vec3::Y]
                }
                CamSpace::World => [Vec3::Z, Vec3::X, Vec3::Y],
            };
            let rates = [*depth, *side, *height];
            let mut moved = Vec3::ZERO;
            for (axis, rate) in axes.iter().zip(rates) {
                let share = if rate > 0.0 {
                    smoothing_factor(rate, dt)
                } else {
                    1.0
                };
                // World depth is +Z (behind); the sign does not matter for a share of the delta.
                moved += *axis * (delta.dot(*axis) * share);
            }
            state.v += moved;
            pose.position = state.v;
            true
        }
        Node::SmoothRotation { pitch, yaw, roll } => {
            let wanted = *pose;
            let from = ctx.prev;
            if *pitch > 0.0 && pitch == yaw && yaw == roll {
                // Equal rates on all axes: blend the quaternions along the shorter arc with the
                // pinned nlerp (add, multiply, one sqrt) instead of `slerp`'s libm `acos`/`sin`.
                let q = det_nlerp(from.rotation(), wanted.rotation(), smoothing_factor(*pitch, dt));
                pose.set_rotation(q);
            } else {
                let share = |rate: f32| {
                    if rate > 0.0 {
                        smoothing_factor(rate, dt)
                    } else {
                        1.0
                    }
                };
                pose.yaw = wrap_pi(from.yaw + wrap_pi(wanted.yaw - from.yaw) * share(*yaw));
                pose.pitch = from.pitch + (wanted.pitch - from.pitch) * share(*pitch);
                pose.roll = from.roll + (wanted.roll - from.roll) * share(*roll);
            }
            true
        }
        Node::Lean { gain, max, speed } => {
            if !state.init {
                // A fresh slot has no previous yaw: seed it so the first frame reads no spin.
                state.f[0] = cur.look_yaw;
                state.init = true;
            }
            let rate = wrap_pi(cur.look_yaw - state.f[0]) / dt.max(1.0e-6);
            let limit = max.abs();
            let wanted = (rate * gain).max(-limit).min(limit);
            state.f[0] = cur.look_yaw;
            state.f[1] += (wanted - state.f[1]) * smoothing_factor(*speed, dt);
            pose.roll += state.f[1];
            true
        }
        Node::Frame {
            target,
            dead_zone_deg,
            soft_zone_deg,
            speed,
        } => {
            let Some((point, _)) = ctx.world.target(target) else {
                return false;
            };
            let local = pose.rotation().inverse() * (point - pose.position);
            if local.length_squared() < 1.0e-8 {
                return true;
            }
            let horizontal = det_atan2(local.x, -local.z);
            let vertical = det_atan2(local.y, (local.x * local.x + local.z * local.z).sqrt());
            let ease = smoothing_factor(*speed, dt);
            let correct = |angle: f32, dead: f32, soft: f32| {
                let magnitude = angle.abs();
                let dead = dead.to_radians();
                if magnitude <= dead {
                    return 0.0;
                }
                let eased = (magnitude - dead) * ease;
                let beyond = (magnitude - soft.to_radians()).max(0.0);
                angle.signum() * eased.max(beyond)
            };
            let ch = correct(horizontal, dead_zone_deg.0, soft_zone_deg.0);
            let cv = correct(vertical, dead_zone_deg.1, soft_zone_deg.1);
            // A target to the right (positive horizontal angle) turns the yaw toward +X.
            pose.yaw = wrap_pi(pose.yaw - ch);
            pose.pitch += cv;
            true
        }
        Node::Shake {
            amplitude,
            frequency,
            decay,
        } => {
            let envelope = det_exp(-decay.max(0.0) * ctx.elapsed);
            let phase = ctx.elapsed * frequency;
            let base = (ctx.seed % 9973) as f32 + index as f32 * 13.0 + 1.0;
            let n = |salt: f32| super::smooth_noise(base + salt, phase);
            let scale = amplitude * envelope;
            pose.position += Vec3::new(n(0.0), n(31.7), n(77.3)) * scale;
            pose.roll += n(101.9) * scale * 0.5;
            true
        }
        Node::Fly {
            slot,
            speed,
            sprint_multiplier,
            accel,
            rise,
            sprint,
        } => {
            let (Some(rise), Some(sprint)) = (resolve_f32(ctx, rise), resolve_bool(ctx, sprint)) else {
                return false;
            };
            let Some(axes) = ctx.inputs.raw(slot) else {
                return false;
            };
            let rotation = pose.rotation();
            let (forward, right) = (rotation * Vec3::NEG_Z, rotation * Vec3::X);
            let cruise = speed * if sprint { *sprint_multiplier } else { 1.0 };
            let wanted = (forward * axes[1] + right * axes[0] + Vec3::Y * rise) * cruise;
            let factor = if *accel <= 0.0 {
                1.0
            } else {
                smoothing_factor(*accel, dt)
            };
            state.v = state.v.lerp(wanted, factor);
            pose.position += state.v * dt;
            true
        }
        Node::Rail {
            spline,
            drive,
            orient,
            offset,
        } => {
            let Some(curve) = rail_curve(ctx, spline, state) else {
                return false;
            };
            let (length, closed) = (curve.length(), curve.is_closed());
            let mut goal_m = None;
            let raw = match drive {
                RailDrive::Value(value) => {
                    let Some(v) = resolve_f32(ctx, value) else {
                        return false;
                    };
                    v
                }
                RailDrive::Time { speed, start } => start + speed * ctx.elapsed,
                RailDrive::Follow {
                    target,
                    lead,
                    damping,
                } => {
                    let Some((point, _)) = ctx.world.target(target) else {
                        return false;
                    };
                    let goal = rail::follow_goal(&curve, point, *lead);
                    // `normalize_distance` maps a non-finite goal to 0. Skip before the state
                    // write so a NaN target does not snap the stored distance to the rail start.
                    if !goal.is_finite() {
                        return false;
                    }
                    goal_m = Some(goal);
                    if state.init {
                        rail::damp_distance(state.f[0], goal, *damping, dt, length, closed)
                    } else {
                        goal
                    }
                }
            };
            let (distance, clamped) = rail::normalize_distance(length, closed, raw);
            state.init = true;
            state.f[0] = distance;
            let sample = curve.at_distance(distance);
            let o = vec3(*offset);
            pose.position = sample.position + sample.right * o.x + sample.up * o.y + sample.tangent * o.z;
            let frame = || {
                Quat::from_mat3(&regia_core::math::Mat3::from_cols(
                    sample.right,
                    sample.up,
                    -sample.tangent,
                ))
            };
            match orient {
                RailOrient::Frame => pose.set_rotation(frame()),
                RailOrient::LookAt(target) => match ctx.world.target(target) {
                    Some((point, _)) => {
                        pose.look_at(point);
                        if ctx.aim_override.is_none() {
                            pose.aim = Some(point);
                        }
                    }
                    None => pose.set_rotation(frame()),
                },
                RailOrient::Free => {}
            }
            scratch.rails.push(RailReport {
                node: index,
                spline: spline.clone(),
                distance_m: distance,
                length_m: length,
                closed,
                clamped,
                at_end: !closed && distance >= length,
                point: sample.position,
                tangent: sample.tangent,
                goal_m,
            });
            true
        }
        Node::Crane {
            base,
            yaw,
            pitch,
            length,
            mount_yaw,
            mount_pitch,
            limits,
        } => {
            let (Some(yaw), Some(pitch), Some(length), Some(mount_yaw), Some(mount_pitch)) = (
                resolve_f32(ctx, yaw),
                resolve_f32(ctx, pitch),
                resolve_f32(ctx, length),
                resolve_f32(ctx, mount_yaw),
                resolve_f32(ctx, mount_pitch),
            ) else {
                return false;
            };
            let (origin, heading_deg) = match base {
                CraneBase::Ground(p) => (vec3(*p), 0.0),
                CraneBase::Rail {
                    spline,
                    position,
                    heading,
                } => {
                    let Some(curve) = rail_curve(ctx, spline, state) else {
                        return false;
                    };
                    let Some(raw) = resolve_f32(ctx, position) else {
                        return false;
                    };
                    let (distance, _) = rail::normalize_distance(curve.length(), curve.is_closed(), raw);
                    let sample = curve.at_distance(distance);
                    let t = sample.tangent;
                    let heading_deg = if *heading && (t.x.abs() > 1.0e-4 || t.z.abs() > 1.0e-4) {
                        (-t.x).atan2(-t.z).to_degrees()
                    } else {
                        0.0
                    };
                    (sample.position, heading_deg)
                }
            };
            let requested = CraneAxes {
                yaw_deg: yaw,
                pitch_deg: pitch,
                length,
                mount_yaw_deg: mount_yaw,
                mount_pitch_deg: mount_pitch,
            };
            let (axes, clamped) = requested.clamped(limits);
            let (tip, rotation) = crane::solve(origin, heading_deg, axes);
            pose.position = tip;
            pose.set_rotation(rotation);
            scratch.cranes.push(CraneReport {
                node: index,
                base: origin,
                heading_deg,
                requested,
                axes,
                clamped,
                tip,
            });
            true
        }
        Node::Group { .. } | Node::When { .. } => true,
    }
}

/// The curve of a named rail, or `None` (warning once) when the entity is missing or too short
/// to be a rail.
fn rail_curve(ctx: &EvalCtx<'_>, name: &str, state: &mut NodeState) -> Option<Arc<SplineCurve>> {
    let curve = ctx.world.rail(name).filter(|c| c.length() >= MIN_RAIL_LENGTH_M);
    if curve.is_none() && !state.warned {
        state.warned = true;
        tracing::warn!(target: "regia_framework", rig = %ctx.rig.name, spline = name, "camera rail is missing or has no length");
    }
    curve
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::asset::{Blend, Cmp, DirectorDef, InputSlotDef};

    /// A mock world: one target and an optional wall plane perpendicular to the cast.
    pub struct MockWorld {
        pub target: Option<(Vec3, Quat)>,
        pub wall_distance: Option<f32>,
    }

    impl CameraWorld for MockWorld {
        fn target(&self, target: &TargetRef) -> Option<(Vec3, Quat)> {
            match target {
                TargetRef::Point(p) => Some((Vec3::from(*p), Quat::IDENTITY)),
                _ => self.target,
            }
        }

        fn cast_sphere(&self, _: Vec3, _: Vec3, max: f32, _: f32, _: &[String]) -> Option<f32> {
            self.wall_distance.filter(|d| *d < max)
        }
    }

    pub fn asset_with(root: Vec<Node>) -> CameraAsset {
        CameraAsset {
            format_version: 1,
            name: "t".into(),
            params: BTreeMap::new(),
            variables: vec![],
            inputs: vec![InputSlotDef {
                name: "look".into(),
                kind: SlotKind::Axis2,
                speed: (100.0, 100.0),
                pitch_clamp: (-80.0, 30.0),
                invert_pitch: false,
                normalize: false,
                pull_toward_var: None,
                pull_weight: 0.1,
            }],
            rigs: vec![RigDef {
                name: "r".into(),
                tags: vec!["t".into()],
                root,
                transitions: vec![],
                blend_out: None,
            }],
            transitions: vec![],
            director: DirectorDef::Fixed("r".into()),
        }
    }

    pub fn run(
        asset: &CameraAsset,
        world: &dyn CameraWorld,
        inputs: &InputBank,
        pose: &mut CameraPose,
        scratch: &mut RigScratch,
        dt: f32,
        elapsed: f32,
    ) {
        let overrides = BTreeMap::new();
        let vars = VarTable::from_asset(asset);
        let ctx = EvalCtx {
            dt,
            elapsed,
            seed: 7,
            asset,
            rig: &asset.rigs[0],
            overrides: &overrides,
            vars: &vars,
            inputs,
            world,
            prev: *pose,
            aim_override: None,
        };
        eval_rig(&ctx, pose, scratch);
    }

    fn boom_node(length: f32) -> Node {
        Node::Boom {
            length: Value::Literal(length),
            side_offset: Value::Literal(0.0),
            up_offset: Value::Literal(0.0),
            length_speed: Value::Literal(0.0),
        }
    }

    fn pivot_origin() -> Node {
        Node::Pivot {
            target: TargetRef::Point([0.0, 0.0, 0.0]),
            offset: [0.0, 1.0, 0.0],
            space: PivotSpace::World,
            aim_offset: [0.0; 3],
            inherit_rotation: false,
        }
    }

    const WORLD: MockWorld = MockWorld {
        target: None,
        wall_distance: None,
    };

    #[test]
    fn pose_conventions() {
        let mut pose = CameraPose::default();
        assert!((pose.forward() - Vec3::NEG_Z).length() < 1e-6);
        pose.yaw = std::f32::consts::FRAC_PI_2;
        assert!(
            (pose.forward() - Vec3::NEG_X).length() < 1e-5,
            "positive yaw turns toward -X"
        );
        pose.yaw = 0.0;
        pose.pitch = 0.5;
        assert!(pose.forward().y > 0.0, "positive pitch looks up");
        let mut look = CameraPose {
            position: Vec3::new(0.0, 0.0, 5.0),
            ..CameraPose::default()
        };
        look.look_at(Vec3::ZERO);
        assert!(look.yaw.abs() < 1e-6 && look.pitch.abs() < 1e-6);
    }

    #[test]
    fn pivot_and_boom_place_the_camera_behind_the_pivot() {
        let asset = asset_with(vec![pivot_origin(), boom_node(4.0)]);
        let inputs = InputBank::from_asset(&asset);
        let mut pose = CameraPose::default();
        let mut scratch = RigScratch::default();
        run(&asset, &WORLD, &inputs, &mut pose, &mut scratch, 1.0 / 60.0, 0.0);
        assert!(
            (pose.position - Vec3::new(0.0, 1.0, 4.0)).length() < 1e-5,
            "{:?}",
            pose.position
        );
        assert_eq!(scratch.trace.len(), 2);
        assert_eq!(scratch.skipped, 0);
    }

    #[test]
    fn boom_side_and_up_offsets_shift_in_camera_space_and_keep_aiming() {
        let asset = asset_with(vec![
            pivot_origin(),
            Node::Boom {
                length: Value::Literal(4.0),
                side_offset: Value::Literal(1.0),
                up_offset: Value::Literal(0.5),
                length_speed: Value::Literal(0.0),
            },
        ]);
        let inputs = InputBank::from_asset(&asset);
        let mut pose = CameraPose::default();
        let mut scratch = RigScratch::default();
        run(&asset, &WORLD, &inputs, &mut pose, &mut scratch, 1.0 / 60.0, 0.0);
        assert!((pose.position - Vec3::new(1.0, 1.5, 4.0)).length() < 1e-5);
        let to_aim = (pose.aim.expect("aim") - pose.position).normalize();
        assert!((pose.forward() - to_aim).length() < 1e-5);
    }

    #[test]
    fn invalid_values_skip_the_node_and_count() {
        let asset = asset_with(vec![pivot_origin(), boom_node(-1.0)]);
        let inputs = InputBank::from_asset(&asset);
        let mut pose = CameraPose::default();
        let mut scratch = RigScratch::default();
        run(&asset, &WORLD, &inputs, &mut pose, &mut scratch, 1.0 / 60.0, 0.0);
        assert_eq!(scratch.skipped, 1);
        assert!(scratch.trace[1].skipped);
        assert!(pose.is_finite());
    }

    #[test]
    fn fov_clamps_to_one_and_one_seventy() {
        for (input, expected) in [(0.0, 1.0), (500.0, 170.0), (70.0, 70.0)] {
            let asset = asset_with(vec![Node::Fov {
                degrees: Value::Literal(input),
            }]);
            let inputs = InputBank::from_asset(&asset);
            let mut pose = CameraPose::default();
            let mut scratch = RigScratch::default();
            run(&asset, &WORLD, &inputs, &mut pose, &mut scratch, 1.0 / 60.0, 0.0);
            assert_eq!(pose.fov_deg, expected);
        }
    }

    #[test]
    fn damping_is_frame_rate_independent() {
        let converge = |dt: f32| {
            let asset = asset_with(vec![
                Node::Offset {
                    translation: [10.0, 0.0, 0.0],
                    rotation_deg: [0.0; 3],
                    space: CamSpace::World,
                },
                Node::SmoothPosition {
                    depth: 3.0,
                    side: 3.0,
                    height: 3.0,
                    space: CamSpace::World,
                },
            ]);
            let inputs = InputBank::from_asset(&asset);
            let mut pose = CameraPose::default();
            let mut scratch = RigScratch::default();
            let steps = (2.0 / dt).round() as usize;
            for step in 0..steps {
                // The offset adds to the seed, so restart from the origin each frame and let the
                // smoothing node carry the history.
                let mut next = CameraPose::default();
                let overrides = BTreeMap::new();
                let vars = VarTable::from_asset(&asset);
                let ctx = EvalCtx {
                    dt,
                    elapsed: step as f32 * dt,
                    seed: 1,
                    asset: &asset,
                    rig: &asset.rigs[0],
                    overrides: &overrides,
                    vars: &vars,
                    inputs: &inputs,
                    world: &WORLD,
                    prev: pose,
                    aim_override: None,
                };
                eval_rig(&ctx, &mut next, &mut scratch);
                pose = next;
            }
            pose.position.x
        };
        let (a, b) = (converge(1.0 / 60.0), converge(1.0 / 30.0));
        assert!((a - b).abs() < 1.0e-3, "{a} vs {b}");
        assert!((a - 10.0 * (1.0 - (-6.0f32).exp())).abs() < 1.0e-2);
    }

    #[test]
    fn smooth_rotation_takes_the_short_way_across_pi() {
        let asset = asset_with(vec![Node::SmoothRotation {
            pitch: 0.0,
            yaw: 5.0,
            roll: 0.0,
        }]);
        let inputs = InputBank::from_asset(&asset);
        let mut pose = CameraPose {
            yaw: 3.0,
            ..CameraPose::default()
        };
        let from = pose;
        let mut scratch = RigScratch::default();
        // Wanted yaw -3.0 is 0.283 rad away through ±π, not 6.0 through zero.
        let overrides = BTreeMap::new();
        let vars = VarTable::from_asset(&asset);
        let ctx = EvalCtx {
            dt: 0.1,
            elapsed: 0.0,
            seed: 1,
            asset: &asset,
            rig: &asset.rigs[0],
            overrides: &overrides,
            vars: &vars,
            inputs: &inputs,
            world: &WORLD,
            prev: from,
            aim_override: None,
        };
        pose.yaw = -3.0;
        eval_rig(&ctx, &mut pose, &mut scratch);
        let moved = wrap_pi(pose.yaw - 3.0).abs();
        assert!(moved > 0.0 && moved < 0.283, "moved {moved}");
    }

    #[test]
    fn frame_dead_zone_leaves_the_pose_and_soft_zone_eases() {
        let node = |dead: f32| Node::Frame {
            target: TargetRef::Point([1.0, 0.0, -10.0]),
            dead_zone_deg: (dead, dead),
            soft_zone_deg: (20.0, 20.0),
            speed: 4.0,
        };
        // The target is ~5.7 degrees to the right of straight ahead.
        let inputs_of = |asset: &CameraAsset| InputBank::from_asset(asset);
        let asset = asset_with(vec![node(10.0)]);
        let mut pose = CameraPose::default();
        let mut scratch = RigScratch::default();
        run(
            &asset,
            &WORLD,
            &inputs_of(&asset),
            &mut pose,
            &mut scratch,
            1.0 / 60.0,
            0.0,
        );
        assert_eq!(pose.yaw, 0.0, "inside the dead zone nothing moves");
        let asset = asset_with(vec![node(2.0)]);
        let mut pose = CameraPose::default();
        run(
            &asset,
            &WORLD,
            &inputs_of(&asset),
            &mut pose,
            &mut scratch,
            1.0 / 60.0,
            0.0,
        );
        assert!(
            pose.yaw < 0.0 && pose.yaw > -0.1,
            "eased toward the target: {}",
            pose.yaw
        );
    }

    #[test]
    fn collision_never_places_the_probe_past_a_near_hit() {
        let asset = asset_with(vec![
            pivot_origin(),
            boom_node(6.0),
            Node::Collision {
                radius: 0.4,
                layers: vec![],
                close_speed: 1000.0,
                open_speed: 2.0,
                open_rate: 0.0,
                anchor: CollisionAnchor::Pivot,
                reuse_last_frame: false,
            },
        ]);
        let inputs = InputBank::from_asset(&asset);
        let mut scratch = RigScratch::default();
        let wall = MockWorld {
            target: None,
            wall_distance: Some(0.1),
        };
        let mut pose = CameraPose::default();
        run(&asset, &wall, &inputs, &mut pose, &mut scratch, 0.1, 0.0);
        assert!(
            (pose.position.z - (0.1 - PROBE_SKIN)).abs() < 1e-4,
            "hit 0.1 m minus skin, not raised to the radius or {PROBE_MIN_DISTANCE}: {}",
            pose.position.z
        );
        assert!(pose.position.z < PROBE_MIN_DISTANCE);
        assert!(pose.position.z < 0.4);
        // A hit inside the skin lands at the anchor on the first evaluation.
        let mut scratch = RigScratch::default();
        let wall = MockWorld {
            target: None,
            wall_distance: Some(0.02),
        };
        let asset = asset_with(vec![
            pivot_origin(),
            boom_node(6.0),
            Node::Collision {
                radius: 0.25,
                layers: vec![],
                close_speed: 1000.0,
                open_speed: 2.0,
                open_rate: 0.0,
                anchor: CollisionAnchor::Pivot,
                reuse_last_frame: false,
            },
        ]);
        let inputs = InputBank::from_asset(&asset);
        let mut pose = CameraPose::default();
        run(&asset, &wall, &inputs, &mut pose, &mut scratch, 0.1, 0.0);
        assert!(
            pose.position.z.abs() < 1e-4,
            "a 0.02 m hit is 0 on the first evaluation: {}",
            pose.position.z
        );
    }

    #[test]
    fn collision_shortens_to_the_hit_and_eases_back() {
        let asset = asset_with(vec![
            pivot_origin(),
            boom_node(6.0),
            Node::Collision {
                radius: 0.25,
                layers: vec![],
                close_speed: 10.0,
                open_speed: 2.0,
                open_rate: 0.0,
                anchor: CollisionAnchor::Pivot,
                reuse_last_frame: false,
            },
        ]);
        let inputs = InputBank::from_asset(&asset);
        let mut scratch = RigScratch::default();
        let free = MockWorld {
            target: None,
            wall_distance: None,
        };
        let mut pose = CameraPose::default();
        run(&asset, &free, &inputs, &mut pose, &mut scratch, 0.1, 0.0);
        assert!((pose.position.z - 6.0).abs() < 1e-4, "unobstructed: full boom");
        // A wall at 2.0: closes at 10 m/s, 1 m this step.
        let wall = MockWorld {
            target: None,
            wall_distance: Some(2.0),
        };
        let mut pose = CameraPose::default();
        run(&asset, &wall, &inputs, &mut pose, &mut scratch, 0.1, 0.1);
        assert!(
            (pose.position.z - 5.0).abs() < 1e-4,
            "closing at close_speed: {}",
            pose.position.z
        );
        for _ in 0..30 {
            let mut pose = CameraPose::default();
            run(&asset, &wall, &inputs, &mut pose, &mut scratch, 0.1, 0.2);
        }
        let mut pose = CameraPose::default();
        run(&asset, &wall, &inputs, &mut pose, &mut scratch, 0.1, 0.3);
        assert!(
            (pose.position.z - (2.0 - PROBE_SKIN)).abs() < 1e-4,
            "settles at hit minus skin"
        );
        // Reopen at 2 m/s.
        let mut pose = CameraPose::default();
        run(&asset, &free, &inputs, &mut pose, &mut scratch, 0.1, 0.4);
        assert!(
            (pose.position.z - (2.0 - PROBE_SKIN + 0.2)).abs() < 1e-4,
            "{}",
            pose.position.z
        );
    }

    #[test]
    fn collision_without_a_world_is_a_no_op_and_reuse_is_one_frame_late() {
        let asset = asset_with(vec![
            pivot_origin(),
            boom_node(6.0),
            Node::Collision {
                radius: 0.25,
                layers: vec![],
                close_speed: SNAP_SPEED,
                open_speed: 4.0,
                open_rate: 0.0,
                anchor: CollisionAnchor::Pivot,
                reuse_last_frame: true,
            },
        ]);
        let inputs = InputBank::from_asset(&asset);
        let mut scratch = RigScratch::default();
        let mut pose = CameraPose::default();
        run(&asset, &EmptyWorld, &inputs, &mut pose, &mut scratch, 0.1, 0.0);
        assert!((pose.position.z - 6.0).abs() < 1e-4);
        let wall = MockWorld {
            target: None,
            wall_distance: Some(2.0),
        };
        let mut pose = CameraPose::default();
        run(&asset, &wall, &inputs, &mut pose, &mut scratch, 0.1, 0.1);
        assert!(
            (pose.position.z - 6.0).abs() < 1e-4,
            "the wall is seen one frame late"
        );
        let mut pose = CameraPose::default();
        run(&asset, &wall, &inputs, &mut pose, &mut scratch, 0.1, 0.2);
        assert!((pose.position.z - 1.95).abs() < 1e-4, "{}", pose.position.z);
    }

    #[test]
    fn group_and_when_run_nested_nodes_only_when_enabled() {
        let inner = vec![Node::Fov {
            degrees: Value::Literal(30.0),
        }];
        let asset = asset_with(vec![
            Node::Group {
                enabled: Value::Literal(false),
                nodes: inner.clone(),
            },
            Node::When {
                cond: Condition::Not(Box::new(Condition::Elapsed(1.0))),
                nodes: inner,
            },
            Node::Fov {
                degrees: Value::Literal(80.0),
            },
        ]);
        let inputs = InputBank::from_asset(&asset);
        let mut pose = CameraPose::default();
        let mut scratch = RigScratch::default();
        run(&asset, &WORLD, &inputs, &mut pose, &mut scratch, 0.1, 0.0);
        // Group disabled: its Fov did not run; When ran its Fov (30) then the last Fov set 80.
        assert_eq!(pose.fov_deg, 80.0);
        assert_eq!(scratch.states.len(), 5);
        let kinds: Vec<&str> = scratch.trace.iter().map(|t| t.kind).collect();
        assert_eq!(kinds, ["group", "when", "fov", "fov"]);
    }

    #[test]
    fn lean_first_frame_reads_no_spin_from_a_fresh_slot() {
        let asset = asset_with(vec![Node::Lean {
            gain: 0.12,
            max: 0.18,
            speed: 8.0,
        }]);
        let inputs = InputBank::from_asset(&asset);
        let mut scratch = RigScratch::default();
        let mut pose = CameraPose {
            yaw: std::f32::consts::FRAC_PI_2,
            ..CameraPose::default()
        };
        run(&asset, &WORLD, &inputs, &mut pose, &mut scratch, 1.0 / 60.0, 0.0);
        assert!(pose.roll.abs() < 1e-6, "no fake spin on frame one: {}", pose.roll);
    }

    #[test]
    fn lean_rolls_into_a_turn_and_relaxes() {
        let asset = asset_with(vec![Node::Lean {
            gain: 0.1,
            max: 0.3,
            speed: 8.0,
        }]);
        let inputs = InputBank::from_asset(&asset);
        let mut scratch = RigScratch::default();
        let mut cursor_pose = CameraPose::default();
        let mut turning = inputs.clone();
        turning.defs[0].pitch_clamp = (-80.0, 30.0);
        let mut last = 0.0f32;
        for step in 0..30 {
            turning.states[0].yaw = step as f32 * 0.05;
            // The Lean node reads the look yaw the rig started with when no LookInput ran.
            let mut pose = CameraPose {
                yaw: turning.states[0].yaw,
                ..CameraPose::default()
            };
            run(&asset, &WORLD, &turning, &mut pose, &mut scratch, 1.0 / 60.0, 0.0);
            last = pose.roll;
            cursor_pose = pose;
        }
        assert!(last > 0.0 && last <= 0.3, "lean {last}");
        assert!(cursor_pose.is_finite());
    }

    #[test]
    fn shake_is_deterministic_and_decays() {
        let asset = asset_with(vec![Node::Shake {
            amplitude: 0.2,
            frequency: 6.0,
            decay: 2.0,
        }]);
        let inputs = InputBank::from_asset(&asset);
        let sample = |elapsed: f32| {
            let mut pose = CameraPose::default();
            let mut scratch = RigScratch::default();
            run(
                &asset,
                &WORLD,
                &inputs,
                &mut pose,
                &mut scratch,
                1.0 / 60.0,
                elapsed,
            );
            pose.position
        };
        assert_eq!(sample(0.37), sample(0.37));
        assert!(sample(0.0).length() > 0.0 || sample(0.3).length() > 0.0);
        assert!(sample(10.0).length() < 1.0e-6, "decayed to nothing");
    }

    #[test]
    fn conditions_evaluate() {
        let asset = asset_with(vec![]);
        let mut vars = VarTable::default();
        vars.push("aim", VarValue::Bool(true), false);
        vars.push("speed", VarValue::Float(5.0), false);
        let ctx = CondCtx {
            elapsed: 2.0,
            vars: &vars,
            rig: &asset.rigs[0],
        };
        let e = |c: Condition| eval_condition(&c, &ctx);
        assert!(e(Condition::Elapsed(1.5)) && !e(Condition::Elapsed(2.5)));
        assert!(e(Condition::VarBool("aim".into(), true)) && !e(Condition::VarBool("aim".into(), false)));
        assert!(
            e(Condition::VarCompare("speed".into(), Cmp::Ge, 5.0))
                && !e(Condition::VarCompare("speed".into(), Cmp::Lt, 5.0))
        );
        assert!(e(Condition::HasTag("t".into())) && !e(Condition::HasTag("x".into())));
        assert!(e(Condition::RigIs("r".into())));
        assert!(e(Condition::All(vec![])), "an empty list passes");
        assert!(!e(Condition::Any(vec![])));
        assert!(e(Condition::Not(Box::new(Condition::RigIs("z".into())))));
        assert!(!e(Condition::VarBool("missing".into(), true)));
        let _ = Blend::Cut;
    }

    #[test]
    fn input_slot_integrates_clamps_inverts_normalizes_resets_and_pulls() {
        let mut asset = asset_with(vec![]);
        asset.inputs[0].normalize = true;
        asset.variables.push(crate::camera::asset::VarDef {
            name: "behind".into(),
            default: VarValue::YawPitch(1.0, 0.1),
            local_only: false,
        });
        let vars = VarTable::from_asset(&asset);
        let mut bank = InputBank::from_asset(&asset);
        assert!(bank.set_raw("look", [3.0, 4.0]));
        assert!(!bank.set_raw("nope", [0.0, 0.0]));
        assert!(!bank.set_raw("look", [f32::NAN, 0.0]));
        bank.integrate(1.0, &vars);
        let (yaw, pitch) = bank.angles("look").expect("slot");
        // Normalized to length 1: (0.6, 0.8) at 100 deg/s for 1 s.
        assert!((yaw + 0.6 * 100.0f32.to_radians()).abs() < 1e-5);
        assert!(
            (pitch - 30.0f32.to_radians()).abs() < 1e-5,
            "pitch clamped to 30 deg: {pitch}"
        );
        assert!(bank.reset("look"));
        assert_eq!(bank.angles("look"), Some((0.0, 0.0)));
        asset.inputs[0].invert_pitch = true;
        let mut inverted = InputBank::from_asset(&asset);
        inverted.set_raw("look", [0.0, 0.5]);
        inverted.integrate(1.0, &vars);
        assert!(inverted.angles("look").map(|a| a.1).unwrap_or(0.0) < 0.0);
        asset.inputs[0].pull_toward_var = Some("behind".into());
        asset.inputs[0].pull_weight = 4.0;
        let mut pulled = InputBank::from_asset(&asset);
        for _ in 0..200 {
            pulled.integrate(0.05, &vars);
        }
        let (yaw, pitch) = pulled.angles("look").expect("slot");
        assert!((yaw - 1.0).abs() < 1e-3 && (pitch - 0.1).abs() < 1e-3);
    }

    #[test]
    fn variable_views_blend_by_weight_and_keep_local_only() {
        let mut asset = asset_with(vec![]);
        asset.variables = vec![
            crate::camera::asset::VarDef {
                name: "f".into(),
                default: VarValue::Float(0.0),
                local_only: false,
            },
            crate::camera::asset::VarDef {
                name: "b".into(),
                default: VarValue::Bool(false),
                local_only: false,
            },
            crate::camera::asset::VarDef {
                name: "l".into(),
                default: VarValue::Float(0.0),
                local_only: true,
            },
        ];
        let mut global = VarTable::from_asset(&asset);
        let frozen = global.values.clone();
        assert!(global.set("f", VarValue::Float(10.0)));
        assert!(global.set("b", VarValue::Bool(true)));
        assert!(global.set("l", VarValue::Float(10.0)));
        assert!(!global.set("f", VarValue::Bool(true)), "type check");
        assert!(global.written_this_frame("f"));
        let mut view = VarTable::default();
        view.blend_view(&global, &frozen, 0.25);
        assert_eq!(view.get("f"), Some(VarValue::Float(2.5)));
        assert_eq!(view.get("b"), Some(VarValue::Bool(false)));
        assert_eq!(
            view.get("l"),
            Some(VarValue::Float(10.0)),
            "local_only is not blended"
        );
        view.blend_view(&global, &frozen, 0.5);
        assert_eq!(view.get("b"), Some(VarValue::Bool(true)));
        global.end_frame();
        assert!(!global.written_this_frame("f"));
    }

    #[test]
    fn combine_weights_and_cut_flag() {
        let a = CameraPose {
            fov_deg: 40.0,
            ..CameraPose::default()
        };
        let b = CameraPose {
            position: Vec3::new(2.0, 0.0, 0.0),
            fov_deg: 80.0,
            yaw: 1.0,
            cut: true,
            ..CameraPose::default()
        };
        let mid = combine(&[(a, 0.5), (b, 0.5)]);
        assert!((mid.position.x - 1.0).abs() < 1e-6 && (mid.fov_deg - 60.0).abs() < 1e-5);
        assert!(mid.yaw > 0.4 && mid.yaw < 0.6);
        assert!(mid.cut, "weight >= 0.5 cut");
        let low = combine(&[(a, 0.7), (b, 0.3)]);
        assert!(!low.cut);
        assert_eq!(combine(&[]), CameraPose::default());
    }

    /// Pin: the damping share and a two-second damped move, bit for bit. The series behind
    /// `det_exp` must not drift: camera state feeds the next frame (spec 35 § 12.13).
    #[test]
    fn smoothing_factor_and_a_damped_move_are_pinned() {
        assert_eq!(smoothing_factor(6.0, 1.0 / 60.0).to_bits(), 0x3dc2_e498);
        assert_eq!(smoothing_factor(0.0, 1.0 / 60.0), 0.0);
        assert_eq!(smoothing_factor(-3.0, 1.0 / 60.0), 0.0);
        assert_eq!(smoothing_factor(f32::NAN, 0.1), 0.0);
        let converge = |dt: f32| {
            let asset = asset_with(vec![
                Node::Offset {
                    translation: [10.0, 0.0, 0.0],
                    rotation_deg: [0.0; 3],
                    space: CamSpace::World,
                },
                Node::SmoothPosition {
                    depth: 3.0,
                    side: 3.0,
                    height: 3.0,
                    space: CamSpace::World,
                },
            ]);
            let inputs = InputBank::from_asset(&asset);
            let mut pose = CameraPose::default();
            let mut scratch = RigScratch::default();
            let steps = (2.0 / dt).round() as usize;
            for step in 0..steps {
                let mut next = CameraPose::default();
                let overrides = BTreeMap::new();
                let vars = VarTable::from_asset(&asset);
                let ctx = EvalCtx {
                    dt,
                    elapsed: step as f32 * dt,
                    seed: 1,
                    asset: &asset,
                    rig: &asset.rigs[0],
                    overrides: &overrides,
                    vars: &vars,
                    inputs: &inputs,
                    world: &WORLD,
                    prev: pose,
                    aim_override: None,
                };
                eval_rig(&ctx, &mut next, &mut scratch);
                pose = next;
            }
            pose.position
        };
        let end = converge(1.0 / 60.0);
        assert_eq!(end.x.to_bits(), 0x411f_9a7a);
        assert_eq!(end.y.to_bits(), 0);
        assert_eq!(end.z.to_bits(), 0);
    }

    /// Pin: blending two poses through `combine` at 0.25 and 0.5, bit for bit. The quaternion
    /// nlerp and the deterministic euler extraction must not drift (spec 35 § 12.13).
    #[test]
    fn combine_pins_yaw_pitch_roll_and_quaternion_bits() {
        let a = CameraPose {
            yaw: 0.3,
            pitch: -0.2,
            roll: 0.1,
            fov_deg: 40.0,
            ..CameraPose::default()
        };
        let b = CameraPose {
            yaw: -1.1,
            pitch: 0.4,
            roll: -0.3,
            position: Vec3::new(2.0, 1.0, -0.5),
            fov_deg: 80.0,
            cut: true,
            ..CameraPose::default()
        };
        let quarter = combine(&[(a, 0.75), (b, 0.25)]);
        let half = combine(&[(a, 0.5), (b, 0.5)]);
        let q = half.rotation();
        // One array, so a failure shows every value at once. The nlerp normalizes through glam, whose SIMD
        // dot groups the lanes differently on SSE2 and NEON (ADR-5176), so three values are one per
        // instruction set: quarter pitch, half roll and the rotation's z (the M5 run, ADR-5550).
        let bits = [
            quarter.yaw,
            quarter.pitch,
            quarter.roll,
            half.yaw,
            half.pitch,
            half.roll,
            q.x,
            q.y,
            q.z,
            q.w,
        ]
        .map(f32::to_bits);
        #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
        assert_eq!(
            bits,
            [
                0xbc3f_c9a8,
                0xbc76_32da,
                0x3db7_f2da,
                0xbeb5_70a2,
                0x3e2e_118c,
                0x3c7c_aead,
                0x3da8_5bbc,
                0xbe34_7eb8,
                0x3cb8_8819,
                0x3f7b_0b95
            ],
            "quarter yaw/pitch/roll, half yaw/pitch/roll, half rotation xyzw: {bits:#010x?}"
        );
        #[cfg(target_arch = "aarch64")]
        assert_eq!(
            bits,
            [
                0xbc3f_c9a8,
                0xbc76_32d9,
                0x3db7_f2da,
                0xbeb5_70a2,
                0x3e2e_118c,
                0x3c7c_aeb1,
                0x3da8_5bbc,
                0xbe34_7eb8,
                0x3cb8_881a,
                0x3f7b_0b95
            ],
            "quarter yaw/pitch/roll, half yaw/pitch/roll, half rotation xyzw: {bits:#010x?}"
        );
    }

    /// Pin: the rig shake offset at three elapsed times, bit for bit (fixed seed, decaying
    /// envelope from the pinned `det_exp`, cosine fade from the pinned `det_cos`).
    #[test]
    fn shake_offsets_are_pinned_across_platforms() {
        let asset = asset_with(vec![Node::Shake {
            amplitude: 0.02,
            frequency: 7.0,
            decay: 1.5,
        }]);
        let inputs = InputBank::from_asset(&asset);
        let sample = |elapsed: f32| {
            let mut pose = CameraPose::default();
            let mut scratch = RigScratch::default();
            run(
                &asset,
                &WORLD,
                &inputs,
                &mut pose,
                &mut scratch,
                1.0 / 60.0,
                elapsed,
            );
            pose
        };
        let at0 = sample(0.0);
        assert_eq!(at0.position.x.to_bits(), 0xbc25_6440);
        assert_eq!(at0.position.y.to_bits(), 0x3c08_f647);
        assert_eq!(at0.position.z.to_bits(), 0xbc32_eb1e);
        let at04 = sample(0.4);
        assert_eq!(at04.position.x.to_bits(), 0xba43_9b53);
        assert_eq!(at04.position.y.to_bits(), 0x3c0a_df11);
        assert_eq!(at04.position.z.to_bits(), 0xbc06_e6a0);
        let at10 = sample(1.0);
        assert_eq!(at10.position.x.to_bits(), 0xb99d_5762);
        assert_eq!(at10.position.y.to_bits(), 0x3b69_6bec);
        assert_eq!(at10.position.z.to_bits(), 0x3b61_070a);
        // Sanity: unchanged behavior shape (decays to nothing far out).
        assert!(sample(10.0).position.length() < 1.0e-6);
    }

    // ---- Spec 35 conformance additions (lane spec-15-35, ADR-5778) ----

    /// § 4.4: `VarCompare` with each of the five operators, on both sides of the value.
    #[test]
    fn var_compare_covers_every_operator() {
        let asset = asset_with(vec![]);
        let mut vars = VarTable::default();
        vars.push("speed", VarValue::Float(5.0), false);
        let ctx = CondCtx {
            elapsed: 0.0,
            vars: &vars,
            rig: &asset.rigs[0],
        };
        let e = |op: Cmp, v: f32| eval_condition(&Condition::VarCompare("speed".into(), op, v), &ctx);
        assert!(e(Cmp::Lt, 6.0) && !e(Cmp::Lt, 5.0));
        assert!(e(Cmp::Le, 5.0) && !e(Cmp::Le, 4.0));
        assert!(e(Cmp::Eq, 5.0) && !e(Cmp::Eq, 5.5));
        assert!(e(Cmp::Ge, 5.0) && !e(Cmp::Ge, 6.0));
        assert!(e(Cmp::Gt, 4.0) && !e(Cmp::Gt, 5.0));
        // An unknown variable never compares true.
        assert!(!eval_condition(
            &Condition::VarCompare("missing".into(), Cmp::Le, 100.0),
            &ctx
        ));
    }

    /// § 5: vec3 variables blend linearly and yaw/pitch variables by the shortest path.
    #[test]
    fn vec3_and_yaw_pitch_variables_blend_by_weight_the_short_way() {
        let mut asset = asset_with(vec![]);
        asset.variables = vec![
            crate::camera::asset::VarDef {
                name: "v".into(),
                default: VarValue::Vec3([0.0, 0.0, 0.0]),
                local_only: false,
            },
            crate::camera::asset::VarDef {
                name: "yp".into(),
                default: VarValue::YawPitch(3.0, 0.0),
                local_only: false,
            },
        ];
        let mut global = VarTable::from_asset(&asset);
        let frozen = global.values.clone();
        assert!(global.set("v", VarValue::Vec3([4.0, 8.0, -2.0])));
        assert!(global.set("yp", VarValue::YawPitch(-3.0, 0.4)));
        let mut view = VarTable::default();
        view.blend_view(&global, &frozen, 0.25);
        assert_eq!(view.get("v"), Some(VarValue::Vec3([1.0, 2.0, -0.5])));
        view.blend_view(&global, &frozen, 0.5);
        let Some(VarValue::YawPitch(yaw, pitch)) = view.get("yp") else {
            panic!("yaw/pitch variable");
        };
        // From 3.0 to -3.0 the short way is +0.283 rad through +pi, not -6.0 rad through zero.
        assert!((yaw - (3.0 + wrap_pi(-6.0) * 0.5)).abs() < 1e-5, "yaw {yaw}");
        assert!(yaw > 3.0, "went through pi: {yaw}");
        assert!((pitch - 0.2).abs() < 1e-6);
    }

    /// § 5: an `Axis1` slot reads only the horizontal value, and the accumulated yaw wraps.
    #[test]
    fn an_axis1_slot_ignores_the_vertical_value_and_its_yaw_wraps() {
        let mut asset = asset_with(vec![]);
        asset.inputs[0].kind = SlotKind::Axis1;
        asset.inputs[0].speed = (90.0, 90.0);
        let vars = VarTable::from_asset(&asset);
        let mut bank = InputBank::from_asset(&asset);
        assert!(bank.set_raw("look", [1.0, 1.0]));
        let quarter = std::f32::consts::FRAC_PI_2;
        let expected = [-quarter, -2.0 * quarter, quarter, 0.0];
        for want in expected {
            bank.integrate(1.0, &vars);
            let (yaw, pitch) = bank.angles("look").expect("slot");
            assert_eq!(pitch, 0.0, "Axis1 never moves pitch");
            assert!(yaw.abs() <= std::f32::consts::PI + 1e-6, "wrapped: {yaw}");
            assert!(wrap_pi(yaw - want).abs() < 1e-4, "yaw {yaw} want {want}");
        }
    }

    /// § 4.3 `Boom.length_speed`: 0 is instant; otherwise the length eases toward the target
    /// at that many meters per second and stops on it.
    #[test]
    fn boom_length_speed_eases_toward_the_target_length() {
        let mut asset = asset_with(vec![
            pivot_origin(),
            Node::Boom {
                length: Value::Var("len".into()),
                side_offset: Value::Literal(0.0),
                up_offset: Value::Literal(0.0),
                length_speed: Value::Literal(2.0),
            },
        ]);
        asset.variables.push(crate::camera::asset::VarDef {
            name: "len".into(),
            default: VarValue::Float(4.0),
            local_only: false,
        });
        let inputs = InputBank::from_asset(&asset);
        let mut scratch = RigScratch::default();
        let mut pose = CameraPose::default();
        run(&asset, &WORLD, &inputs, &mut pose, &mut scratch, 0.5, 0.0);
        assert!((pose.position.z - 4.0).abs() < 1e-5, "first frame is the length");
        asset.variables[0].default = VarValue::Float(8.0);
        let mut pose = CameraPose::default();
        run(&asset, &WORLD, &inputs, &mut pose, &mut scratch, 0.5, 0.5);
        assert!(
            (pose.position.z - 5.0).abs() < 1e-5,
            "2 m/s for 0.5 s: {}",
            pose.position.z
        );
        for step in 0..10 {
            let mut next = CameraPose::default();
            run(
                &asset,
                &WORLD,
                &inputs,
                &mut next,
                &mut scratch,
                0.5,
                1.0 + step as f32,
            );
            pose = next;
        }
        assert!((pose.position.z - 8.0).abs() < 1e-5, "stops on the target");
        // Speed 0: a change is instant.
        let mut instant = asset.clone();
        if let Node::Boom { length_speed, .. } = &mut instant.rigs[0].root[1] {
            *length_speed = Value::Literal(0.0);
        }
        instant.variables[0].default = VarValue::Float(4.0);
        let mut scratch = RigScratch::default();
        let mut pose = CameraPose::default();
        run(&instant, &WORLD, &inputs, &mut pose, &mut scratch, 0.5, 0.0);
        instant.variables[0].default = VarValue::Float(8.0);
        let mut pose = CameraPose::default();
        run(&instant, &WORLD, &inputs, &mut pose, &mut scratch, 0.5, 0.5);
        assert!((pose.position.z - 8.0).abs() < 1e-5);
    }

    /// § 4.3 `Offset`: `Camera` space follows the view, `World` space does not, and
    /// `rotation_deg` adds (pitch, yaw, roll) degrees.
    #[test]
    fn offset_in_camera_space_follows_the_view_and_rotation_adds_degrees() {
        let facing_neg_x = CameraPose {
            yaw: std::f32::consts::FRAC_PI_2,
            ..CameraPose::default()
        };
        let offset = |space: CamSpace| {
            asset_with(vec![Node::Offset {
                translation: [0.0, 0.0, -2.0],
                rotation_deg: [10.0, 20.0, 5.0],
                space,
            }])
        };
        let camera = offset(CamSpace::Camera);
        let inputs = InputBank::from_asset(&camera);
        let mut pose = facing_neg_x;
        let mut scratch = RigScratch::default();
        run(&camera, &WORLD, &inputs, &mut pose, &mut scratch, 1.0 / 60.0, 0.0);
        assert!(
            (pose.position - Vec3::new(-2.0, 0.0, 0.0)).length() < 1e-5,
            "two meters forward of a camera facing -X: {:?}",
            pose.position
        );
        assert!((pose.pitch - 10.0f32.to_radians()).abs() < 1e-6);
        assert!((pose.yaw - (std::f32::consts::FRAC_PI_2 + 20.0f32.to_radians())).abs() < 1e-6);
        assert!((pose.roll - 5.0f32.to_radians()).abs() < 1e-6);
        let world = offset(CamSpace::World);
        let mut pose = facing_neg_x;
        let mut scratch = RigScratch::default();
        run(&world, &WORLD, &inputs, &mut pose, &mut scratch, 1.0 / 60.0, 0.0);
        assert!((pose.position - Vec3::new(0.0, 0.0, -2.0)).length() < 1e-6);
    }

    /// § 4.3 `SmoothPosition` in `Camera` space: each rate applies along its view axis (depth
    /// along the view, side along camera right), not along the world axes.
    #[test]
    fn smooth_position_in_camera_space_smooths_each_view_axis_by_its_own_rate() {
        let dt = 1.0 / 60.0;
        let asset = |space: CamSpace| {
            asset_with(vec![
                Node::Offset {
                    translation: [10.0, 0.0, 4.0],
                    rotation_deg: [0.0; 3],
                    space: CamSpace::World,
                },
                Node::SmoothPosition {
                    depth: 0.0,
                    side: 5.0,
                    height: 0.0,
                    space,
                },
            ])
        };
        let facing_neg_x = CameraPose {
            yaw: std::f32::consts::FRAC_PI_2,
            ..CameraPose::default()
        };
        let share = smoothing_factor(5.0, dt);
        // Facing -X: the view (depth) axis is world X, camera right is world -Z.
        let cam = asset(CamSpace::Camera);
        let inputs = InputBank::from_asset(&cam);
        let mut pose = facing_neg_x;
        let mut scratch = RigScratch::default();
        run(&cam, &WORLD, &inputs, &mut pose, &mut scratch, dt, 0.0);
        assert!(
            (pose.position.x - 10.0).abs() < 1e-4,
            "depth unsmoothed: {:?}",
            pose.position
        );
        assert!(
            (pose.position.z - 4.0 * share).abs() < 1e-4,
            "side smoothed: {:?}",
            pose.position
        );
        // World space with the same rates: depth is world Z, side is world X.
        let world = asset(CamSpace::World);
        let mut pose = facing_neg_x;
        let mut scratch = RigScratch::default();
        run(&world, &WORLD, &inputs, &mut pose, &mut scratch, dt, 0.0);
        assert!(
            (pose.position.x - 10.0 * share).abs() < 1e-4,
            "{:?}",
            pose.position
        );
        assert!((pose.position.z - 4.0).abs() < 1e-4, "{:?}", pose.position);
    }

    /// § 4.3 `Frame`: a target beyond the soft zone is brought back at least to the soft-zone
    /// edge in one step, horizontally and vertically.
    #[test]
    fn frame_beyond_the_soft_zone_corrects_to_its_edge_on_both_axes() {
        let node = |target: [f32; 3]| Node::Frame {
            target: TargetRef::Point(target),
            dead_zone_deg: (2.0, 2.0),
            soft_zone_deg: (10.0, 10.0),
            // A very slow ease: the soft-zone edge, not the ease, decides this step.
            speed: 0.001,
        };
        let edge = (45.0f32 - 10.0).to_radians();
        // 45 degrees to the right.
        let asset = asset_with(vec![node([10.0, 0.0, -10.0])]);
        let inputs = InputBank::from_asset(&asset);
        let mut pose = CameraPose::default();
        let mut scratch = RigScratch::default();
        run(&asset, &WORLD, &inputs, &mut pose, &mut scratch, 1.0 / 60.0, 0.0);
        assert!((pose.yaw + edge).abs() < 1e-3, "yaw {} want {}", pose.yaw, -edge);
        assert!(pose.pitch.abs() < 1e-6);
        // 45 degrees up.
        let asset = asset_with(vec![node([0.0, 10.0, -10.0])]);
        let mut pose = CameraPose::default();
        let mut scratch = RigScratch::default();
        run(&asset, &WORLD, &inputs, &mut pose, &mut scratch, 1.0 / 60.0, 0.0);
        assert!(
            (pose.pitch - edge).abs() < 1e-3,
            "pitch {} want {edge}",
            pose.pitch
        );
        assert!(pose.yaw.abs() < 1e-6);
    }

    /// § 4.3 `Collision.anchor: Target` casts from the drive target; with `Pivot` and no pivot,
    /// aim or offset the anchor is the pose itself and nothing is cast.
    #[test]
    fn collision_anchor_target_casts_from_the_drive_target() {
        let nodes = |anchor: CollisionAnchor| {
            asset_with(vec![
                Node::Offset {
                    translation: [0.0, 2.0, 6.0],
                    rotation_deg: [0.0; 3],
                    space: CamSpace::World,
                },
                Node::Collision {
                    radius: 0.25,
                    layers: vec![],
                    close_speed: SNAP_SPEED,
                    open_speed: 4.0,
                    open_rate: 0.0,
                    anchor,
                    reuse_last_frame: false,
                },
            ])
        };
        let wall = MockWorld {
            target: Some((Vec3::ZERO, Quat::IDENTITY)),
            wall_distance: Some(3.0),
        };
        let asset = nodes(CollisionAnchor::Target);
        let inputs = InputBank::from_asset(&asset);
        let mut pose = CameraPose::default();
        let mut scratch = RigScratch::default();
        run(&asset, &wall, &inputs, &mut pose, &mut scratch, 0.1, 0.0);
        let dir = Vec3::new(0.0, 2.0, 6.0).normalize();
        assert!(
            (pose.position - dir * (3.0 - PROBE_SKIN)).length() < 1e-4,
            "{:?}",
            pose.position
        );
        let probe = scratch.probe.expect("probe");
        assert_eq!(probe.from, Vec3::ZERO);
        assert!(probe.blocked);
        let asset = nodes(CollisionAnchor::Pivot);
        let mut pose = CameraPose::default();
        let mut scratch = RigScratch::default();
        run(&asset, &wall, &inputs, &mut pose, &mut scratch, 0.1, 0.0);
        assert!((pose.position - Vec3::new(0.0, 2.0, 6.0)).length() < 1e-6);
        assert!(scratch.probe.is_none());
    }

    /// § 4.3 `Group.enabled` reads a parameter or a variable; an unresolvable value disables it.
    #[test]
    fn group_enabled_reads_parameters_and_variables() {
        let group = |enabled: Value<bool>| {
            asset_with(vec![Node::Group {
                enabled,
                nodes: vec![Node::Fov {
                    degrees: Value::Literal(30.0),
                }],
            }])
        };
        let fov = |asset: &CameraAsset| {
            let inputs = InputBank::from_asset(asset);
            let mut pose = CameraPose::default();
            let mut scratch = RigScratch::default();
            run(asset, &WORLD, &inputs, &mut pose, &mut scratch, 0.1, 0.0);
            pose.fov_deg
        };
        let default_fov = CameraPose::default().fov_deg;
        let mut by_param = group(Value::Param("on".into()));
        by_param.params.insert("on".into(), ParamValue::Bool(true));
        assert_eq!(fov(&by_param), 30.0);
        by_param.params.insert("on".into(), ParamValue::Bool(false));
        assert_eq!(fov(&by_param), default_fov);
        let mut by_var = group(Value::Var("v".into()));
        by_var.variables.push(crate::camera::asset::VarDef {
            name: "v".into(),
            default: VarValue::Bool(true),
            local_only: false,
        });
        assert_eq!(fov(&by_var), 30.0);
        let missing = group(Value::Var("nope".into()));
        assert_eq!(fov(&missing), default_fov);
    }

    /// § 4.3 `Pivot` with a missing target keeps the pose and warns once (the node's `warned`
    /// flag guards the log); as built it also counts as skipped every frame (ADR-5778).
    #[test]
    fn a_pivot_with_a_missing_target_keeps_the_pose_and_warns_once() {
        let asset = asset_with(vec![Node::Pivot {
            target: TargetRef::DriveTarget,
            offset: [0.0, 1.0, 0.0],
            space: PivotSpace::Target,
            aim_offset: [0.0; 3],
            inherit_rotation: false,
        }]);
        let inputs = InputBank::from_asset(&asset);
        let mut scratch = RigScratch::default();
        let start = CameraPose {
            position: Vec3::new(1.0, 2.0, 3.0),
            yaw: 0.3,
            ..CameraPose::default()
        };
        for frame in 1..=3u32 {
            let mut pose = start;
            run(&asset, &WORLD, &inputs, &mut pose, &mut scratch, 0.1, 0.0);
            assert_eq!(pose, start, "the pose is unchanged");
            assert!(scratch.states[0].warned, "the warning latch is set");
            assert_eq!(scratch.skipped, frame);
            assert!(scratch.pivot.is_none());
        }
    }
}
