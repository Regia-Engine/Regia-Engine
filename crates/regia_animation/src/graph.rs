// SPDX-License-Identifier: MPL-2.0
//! The animation graph asset and its `.animgraph.ron` format (frozen; v4).
//!
//! Specification: `docs/specs/17-animation.md` § 3 and `docs/specs/12-assets.md` § 4a: the
//! format is frozen at M12, carries a `format_version` field, and only changes after that
//! with a registered migrator. The JSON schema and canonical example live in
//! `docs/formats/`.
//!
//! Versions: v1 is the M12 freeze. v2 (ADR-203) adds the optional per-transition `blend`
//! (`Crossfade` | `Inertialize` | `InertializeSpring`, default `Crossfade`) and the optional
//! per-layer `weight_inertialize` seconds (default 0 = off). Both default to v1 behaviour, so
//! the 1→2 migrator only bumps the version and every v1 file loads and plays unchanged. v3
//! (ADR-1870) adds the montage [`SlotNode`]; the 2→3 migrator only bumps the version. v4
//! (ADR-8475) adds the iterative-IK nodes [`ChainIKNode`] (FABRIK or CCD over any chain) and
//! [`BodyAimNode`] (a turn spread over a joint stack); a v3 graph has neither, so the 3→4
//! migrator only bumps the version. The locomotion warp nodes [`StrideWarpNode`] and
//! [`OrientationWarpNode`] (ADR-8540) join v4 in the same format batch: they are new variants
//! too, so no older file changes meaning.

use regia_core::math::Vec3;
use serde::{Deserialize, Serialize};

use crate::blendspace::{BlendSpace1D, BlendSpace2D};
use crate::chain_ik::{ChainSolver, JointLimit};
use crate::clip::RootMotionMode;
use crate::motion_matching::warp::{StrideMode, StrideWarpSettings};
use crate::params::{ParamDecl, ParamValue, ParameterSet};
use crate::pose::JointMask;
use crate::state_machine::{Condition, StateMachineDef};

/// The `.animgraph.ron` format version. Version 1 is the M12 freeze; version 2 adds
/// inertialized transitions and layer weights (ADR-203); version 3 adds the montage `Slot` node
/// (spec 34 M9, ADR-1870); version 4 adds the `ChainIK` and `BodyAim` nodes (ADR-8475) and the
/// `StrideWarp` and `OrientationWarp` nodes (ADR-8540).
pub const ANIMGRAPH_FORMAT_VERSION: u32 = 4;

/// The most solver iterations a `ChainIK` node may ask for (format v4). The loader refuses
/// more, so a typo cannot stall a frame.
pub const MAX_CHAIN_IK_ITERATIONS: u32 = 256;

/// The most passes a `BodyAim` node may ask for (format v4).
pub const MAX_BODY_AIM_PASSES: u32 = 32;

/// Where an IK node's target comes from: a vec3 parameter or a socket (joint + offset).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum IkTarget {
    /// A vec3 parameter name (object space).
    Param(String),
    /// A socket name; the socket's joint global transform positions the target.
    Socket(String),
}

/// Plays a single clip.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ClipNode {
    /// Clip name (resolved against the animator's clip library).
    pub clip: String,
    /// Loop playback.
    pub looping: bool,
    /// Playback speed multiplier.
    pub speed: f32,
    /// Clips sharing a sync group advance on a shared normalised clock so foot phases stay
    /// aligned while blending.
    pub sync_group: Option<String>,
}

/// 1D blend space node.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BlendSpace1DNode {
    /// The space definition.
    pub space: BlendSpace1D,
    /// When set, all samples advance on a shared normalised clock driven by the
    /// highest-weight sample, keeping foot phases aligned.
    pub sync_group: Option<String>,
}

/// 2D blend space node.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BlendSpace2DNode {
    /// The space definition.
    pub space: BlendSpace2D,
    /// Shared normalised clock for all samples (see [`BlendSpace1DNode::sync_group`]).
    pub sync_group: Option<String>,
}

/// Cross-fades between two sub-graphs.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BlendNode {
    /// Sub-graph at weight 0.
    pub a: Box<AnimNode>,
    /// Sub-graph at weight 1.
    pub b: Box<AnimNode>,
    /// Float parameter driving the blend alpha.
    pub alpha_param: String,
}

/// Applies an additive sub-graph on a base pose.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AdditiveNode {
    /// Base pose.
    pub base: Box<AnimNode>,
    /// Additive deltas.
    pub additive: Box<AnimNode>,
    /// Float parameter for additive weight.
    pub weight_param: String,
}

/// A layer's mask: joint names resolved against the rig's skeleton at runtime.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct JointMaskDef {
    /// Joint names.
    pub joints: Vec<String>,
    /// Include descendants of the named joints.
    pub include_children: bool,
}

impl JointMaskDef {
    /// Resolve names to joint ids. Unknown names are skipped with a `warn` at eval time by the
    /// caller; here they are simply absent from the mask.
    pub fn resolve(&self, names: &[String]) -> JointMask {
        let joints = self
            .joints
            .iter()
            .filter_map(|n| names.iter().position(|m| m == n).map(|i| i as u32))
            .collect();
        JointMask {
            joints,
            include_children: self.include_children,
        }
    }
}

/// How a layer combines with what is below it.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum LayerMode {
    /// Replace the masked joints' pose.
    #[default]
    Override,
    /// Add deltas on top of the base pose.
    Additive,
}

/// One animation layer.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LayerEntry {
    /// The layer's sub-graph.
    pub graph: Box<AnimNode>,
    /// Which joints the layer affects.
    pub mask: JointMaskDef,
    /// Combination mode.
    pub mode: LayerMode,
    /// Float parameter for layer weight.
    pub weight_param: String,
    /// Seconds over which a change of the weight parameter is inertialized (format v2): the
    /// weight actually used eases to the new value with the quintic inertial decay, carrying
    /// its current rate, so a weight snapped from 0 to 1 does not pop. `0` (the default)
    /// follows the parameter directly, as v1 did.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub weight_inertialize: f32,
}

fn is_zero(value: &f32) -> bool {
    *value == 0.0
}

/// Stacks masked layers over a base graph (upper-body wave over locomotion).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LayerNode {
    /// Base graph.
    pub base: Box<AnimNode>,
    /// Layers, applied bottom-up in list order.
    pub layers: Vec<LayerEntry>,
}

/// Analytic two-bone IK.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TwoBoneIKNode {
    /// Inner graph the IK is applied to.
    pub inner: Box<AnimNode>,
    /// Chain joint names: `[root, mid, tip]`.
    pub chain: [String; 3],
    /// Target (vec3 parameter or socket).
    pub target: IkTarget,
    /// Optional vec3 parameter bending the chain in a preferred direction.
    pub pole: Option<String>,
    /// 0..=1 blend of the IK result.
    pub weight_param: Option<String>,
}

/// Rotates a joint to aim its local −Z at a target, with yaw/pitch limits from the bind pose.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LookAtNode {
    /// Inner graph.
    pub inner: Box<AnimNode>,
    /// Joint name to rotate.
    pub joint: String,
    /// Target (vec3 parameter or socket).
    pub target: IkTarget,
    /// Maximum yaw from the bind rotation, radians.
    pub max_yaw: f32,
    /// Maximum pitch from the bind rotation, radians.
    pub max_pitch: f32,
    /// 0..=1 blend of the look-at result.
    pub weight_param: Option<String>,
}

/// Overrides the root-motion extraction mode of the inner graph and routes the delta to the
/// [`crate::components::RootMotionOutput`] consumed by character controllers.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RootMotionNode {
    /// Inner graph.
    pub inner: Box<AnimNode>,
    /// Extraction mode override.
    pub mode: RootMotionMode,
}

fn yes() -> bool {
    true
}

/// Plays the montages of one slot inside the graph (format v3, spec 34 M9): outputs `source`
/// while nothing plays in slot `name`, and `source` blended with the slot's montage pose (by the
/// instance weight, under the slot's mask) while something does. Nodes above it (IK, layers)
/// see the montage pose; the post-graph montage stage skips the slots a node consumed.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SlotNode {
    /// Slot name (a slot of the skeleton's table).
    pub name: String,
    /// The pose under the montage.
    pub source: Box<AnimNode>,
    /// Evaluate `source` even while a full-body montage covers it at weight 1 (state machines
    /// under it keep running). `false` skips it then (cost saving).
    #[serde(default = "yes")]
    pub eval_idle_source: bool,
}

/// A limit on one named joint of an IK node (format v4).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NamedJointLimit {
    /// The joint's name.
    pub joint: String,
    /// Its limit, relative to its bind local rotation.
    pub limit: JointLimit,
}

/// Joint limits of an IK node (format v4).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum ChainLimits {
    /// The same limit on every joint the node turns.
    Uniform(JointLimit),
    /// Limits by joint name; joints not listed are free. A name the node does not turn is
    /// ignored with a warning (`animgraph_limit_joint_not_in_chain`).
    PerJoint(Vec<NamedJointLimit>),
}

impl ChainLimits {
    /// The limit for `joint` (by name), `Free` when none applies.
    #[must_use]
    pub fn limit_of(&self, joint: &str) -> JointLimit {
        match self {
            ChainLimits::Uniform(limit) => *limit,
            ChainLimits::PerJoint(list) => list
                .iter()
                .find(|l| l.joint == joint)
                .map_or(JointLimit::Free, |l| l.limit),
        }
    }
}

fn default_chain_iterations() -> u32 {
    24
}

fn default_chain_tolerance() -> f32 {
    1e-3
}

/// Iterative IK over a joint chain of any length (format v4): tails, tentacles, spines, insect
/// legs, cables. Solves with [`crate::chain_ik::solve_chain`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ChainIKNode {
    /// Inner graph the IK is applied to.
    pub inner: Box<AnimNode>,
    /// First joint of the chain (it turns; its parent does not).
    pub root: String,
    /// Last joint of the chain, a descendant of `root` (it moves to the target; its own
    /// rotation is left alone).
    pub tip: String,
    /// The solver (default `Fabrik`).
    #[serde(default)]
    pub solver: ChainSolver,
    /// Target (vec3 parameter or socket).
    pub target: IkTarget,
    /// Optional vec3 parameter the chain's bend turns towards.
    #[serde(default)]
    pub pole: Option<String>,
    /// Joint limits; `None` leaves every joint free.
    #[serde(default)]
    pub limits: Option<ChainLimits>,
    /// Iteration cap (default 24, at most [`MAX_CHAIN_IK_ITERATIONS`]).
    #[serde(default = "default_chain_iterations")]
    pub iterations: u32,
    /// Stop when the tip is this close to the target, meters (default 0.001).
    #[serde(default = "default_chain_tolerance")]
    pub tolerance: f32,
    /// 0..=1 blend of the IK result.
    #[serde(default)]
    pub weight_param: Option<String>,
}

fn default_aim_axis() -> Vec3 {
    Vec3::NEG_Z
}

fn default_aim_passes() -> u32 {
    8
}

/// Turns a stack of joints (spine, chest, neck, head; a turret's yaw ring, pitch cradle and
/// barrel) so one joint's axis points at a target (format v4). Solves with
/// [`crate::aim_ik::solve_body_aim_with`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BodyAimNode {
    /// Inner graph.
    pub inner: Box<AnimNode>,
    /// The joints that turn, bottom first, each with its relative share of the turn (the last
    /// joint with a share above zero takes whatever is left).
    pub joints: Vec<(String, f32)>,
    /// The joint whose axis points at the target: one of `joints` or a descendant of the last.
    pub aim_joint: String,
    /// The aiming direction in `aim_joint`'s local frame (default −Z, the engine's forward).
    #[serde(default = "default_aim_axis")]
    pub axis: Vec3,
    /// Target (vec3 parameter or socket).
    pub target: IkTarget,
    /// Joint limits; `None` leaves every joint free.
    #[serde(default)]
    pub limits: Option<ChainLimits>,
    /// Solve passes (default 8, at most [`MAX_BODY_AIM_PASSES`]): each absorbs most of the
    /// parallax the previous one left when the aim joint sits away from the turning joints (a
    /// muzzle 1.2 m from its pivot keeps about an eighth of the miss per pass).
    #[serde(default = "default_aim_passes")]
    pub passes: u32,
    /// 0..=1 blend of the aim result.
    #[serde(default)]
    pub weight_param: Option<String>,
}

fn default_stride_length() -> f32 {
    0.5
}

/// Stride warping for graph locomotion (format v4, ADR-8540): when gameplay moves the character
/// faster or slower than the inner graph's clips, the node either plays the inner graph faster
/// or slower (`Rate`) or lengthens or shortens the stride with leg IK (`Pose`), so planted feet
/// do not skate. Evaluated with [`crate::motion_matching::warp`] (the motion-matching warp code).
///
/// The clip speed is the inner graph's horizontal root motion this frame (blend spaces blend
/// root motion like their poses); the ground speed is the `ground_speed_param` float, m/s. An
/// inner graph without root motion leaves the pose and clock alone (warned once,
/// `animgraph_warp_no_root_motion`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StrideWarpNode {
    /// Inner graph (usually a locomotion blend space or state machine).
    pub inner: Box<AnimNode>,
    /// Float parameter: the character's actual ground speed, m/s.
    pub ground_speed_param: String,
    /// Strength, speed threshold, ratio range, smoothing and mode (`Rate` scales the inner
    /// clock, `Pose` scales the stride with leg IK). Omitted: [`StrideWarpSettings::graph_default`]
    /// (full strength, `Pose`).
    #[serde(default = "StrideWarpSettings::graph_default")]
    pub settings: StrideWarpSettings,
    /// `Pose` mode: each leg's `[hip, knee, ankle]` joint names.
    #[serde(default)]
    pub legs: Vec<[String; 3]>,
    /// `Pose` mode: the pelvis (hips) joint the legs hang from; it is lowered, never raised,
    /// when a longer stride is out of reach.
    #[serde(default)]
    pub pelvis: String,
    /// `Pose` mode: the largest foot offset from the pelvis along the movement direction that
    /// is scaled, meters (default 0.5).
    #[serde(default = "default_stride_length")]
    pub stride_length: f32,
}

fn default_warp_strength() -> f32 {
    1.0
}

fn default_max_delta() -> f32 {
    std::f32::consts::FRAC_PI_2
}

/// Orientation warping for graph locomotion (format v4, ADR-8540): turns the lower body (the
/// `pelvis` joint) toward the desired movement heading when it differs from the heading of the
/// inner graph's root motion (a strafe angle the blend space does not cover), and turns the
/// spine back so the chest keeps facing. Evaluated with
/// [`crate::motion_matching::warp::apply_orientation_warp_with_globals`].
///
/// Headings are radians about object-space +Y, 0 = the character's forward (−Z), positive
/// counter-clockwise seen from above (toward −X); [`crate::warp_nodes::movement_heading`]
/// computes one from a direction. The turn is the wrapped difference clipped to `±max_delta`,
/// times `strength`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct OrientationWarpNode {
    /// Inner graph.
    pub inner: Box<AnimNode>,
    /// Float parameter: the desired movement heading, radians (see the type docs).
    pub heading_param: String,
    /// The joint that turns the lower body (the hips); the legs turn with it.
    pub pelvis: String,
    /// Spine joints from low to high with their share of the counter-turn; the shares sum to 1
    /// (an empty list turns the whole body).
    #[serde(default)]
    pub spine: Vec<(String, f32)>,
    /// How much of the turn is applied, `[0, 1]` (default 1).
    #[serde(default = "default_warp_strength")]
    pub strength: f32,
    /// Largest turn, radians, `[0, π]` (default π/2).
    #[serde(default = "default_max_delta")]
    pub max_delta: f32,
}

/// A node in the animation graph tree. Tier 1 node set of `docs/specs/17-animation.md` § 3
/// (foot placement is `foot_ik` planning feeding `TwoBoneIK`, not a node of its own).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum AnimNode {
    /// Plays a clip.
    Clip(ClipNode),
    /// 1D blend space.
    BlendSpace1D(BlendSpace1DNode),
    /// 2D blend space.
    BlendSpace2D(BlendSpace2DNode),
    /// State machine.
    StateMachine(StateMachineDef),
    /// Cross-fade of two graphs.
    Blend(BlendNode),
    /// Additive application.
    Additive(AdditiveNode),
    /// Masked layer stack.
    Layer(LayerNode),
    /// Two-bone IK post-process.
    TwoBoneIK(TwoBoneIKNode),
    /// Look-at post-process.
    LookAt(LookAtNode),
    /// Root motion extraction override.
    RootMotion(RootMotionNode),
    /// Montage slot (format v3).
    Slot(SlotNode),
    /// Iterative chain IK post-process (format v4).
    ChainIK(ChainIKNode),
    /// Full-body aim post-process (format v4).
    BodyAim(BodyAimNode),
    /// Stride warp post-process (format v4, ADR-8540).
    StrideWarp(StrideWarpNode),
    /// Orientation warp post-process (format v4, ADR-8540).
    OrientationWarp(OrientationWarpNode),
}

/// An animation graph: parameter declarations and a root node.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AnimGraph {
    /// Parameter declarations (every parameter read by a node must be declared).
    pub parameters: Vec<ParamDecl>,
    /// Root node.
    pub root: AnimNode,
}

impl AnimGraph {
    /// A default parameter set for this graph (declaration defaults).
    pub fn default_parameters(&self) -> ParameterSet {
        ParameterSet::from_decls(&self.parameters)
    }

    /// Deserialise from `.animgraph.ron` text. Checks `format_version`, runs registered
    /// migrations, triangulates 2D blend spaces and validates references.
    ///
    /// The `format_version` field is peeked before the full deserialise, so a *newer* version is
    /// rejected even when its body no longer matches the current document.
    ///
    /// # Errors
    ///
    /// Returns [`AnimGraphError`] for RON syntax errors, unknown format versions that no
    /// migration reaches, or failed validation.
    pub fn from_ron_str(text: &str) -> Result<Self, AnimGraphError> {
        let version = regia_core::version::peek_ron_version(text)
            .map_err(|e| AnimGraphError::Invalid(format!("cannot read format_version: {e}")))?;
        if version > ANIMGRAPH_FORMAT_VERSION {
            return Err(AnimGraphError::UnknownVersion(version));
        }
        let mut document: AnimGraphDocument = ron::from_str(text)?;
        let applied = migrate_document(&mut document)?;
        if applied > 0 {
            tracing::info!(target: "regia_animation", "anim graph migrated to version {ANIMGRAPH_FORMAT_VERSION}");
        }
        let graph = AnimGraph {
            parameters: document.parameters,
            root: document.root,
        };
        graph.validate()?;
        Ok(graph.triangulated())
    }

    /// Serialise to `.animgraph.ron` text at the current format version.
    ///
    /// # Errors
    ///
    /// Returns [`AnimGraphError::RonWrite`] on serialisation failure.
    pub fn to_ron_string(&self) -> Result<String, AnimGraphError> {
        let document = AnimGraphDocument {
            format_version: ANIMGRAPH_FORMAT_VERSION,
            parameters: self.parameters.clone(),
            root: self.root.clone(),
        };
        let pretty = regia_core::ron_pretty();
        Ok(ron::ser::to_string_pretty(&document, pretty)?)
    }

    /// Return a copy with all 2D blend spaces triangulated (ready for evaluation).
    pub fn triangulated(&self) -> Self {
        let mut graph = self.clone();
        triangulate_node(&mut graph.root);
        graph
    }

    /// Validate: every parameter read by a node is declared; state machine transitions
    /// reference existing states and have a finite, non-negative duration; blend spaces have at
    /// least one sample; every number (clip speeds, sample positions, look-at limits, layer
    /// easing times, float condition thresholds) is finite.
    ///
    /// # Errors
    ///
    /// Returns the first problem found as [`AnimGraphError::Invalid`].
    pub fn validate(&self) -> Result<(), AnimGraphError> {
        let declared: Vec<&str> = self.parameters.iter().map(|p| p.0.as_str()).collect();
        validate_node(&self.root, &declared)?;
        Ok(())
    }
}

fn triangulate_node(node: &mut AnimNode) {
    match node {
        AnimNode::BlendSpace2D(n) => n.space.triangulate(),
        AnimNode::StateMachine(sm) => {
            for (_, state) in &mut sm.states {
                triangulate_node(state);
            }
        }
        AnimNode::Blend(n) => {
            triangulate_node(&mut n.a);
            triangulate_node(&mut n.b);
        }
        AnimNode::Additive(n) => {
            triangulate_node(&mut n.base);
            triangulate_node(&mut n.additive);
        }
        AnimNode::Layer(n) => {
            triangulate_node(&mut n.base);
            for layer in &mut n.layers {
                triangulate_node(&mut layer.graph);
            }
        }
        AnimNode::TwoBoneIK(n) => triangulate_node(&mut n.inner),
        AnimNode::LookAt(n) => triangulate_node(&mut n.inner),
        AnimNode::RootMotion(n) => triangulate_node(&mut n.inner),
        AnimNode::Slot(n) => triangulate_node(&mut n.source),
        AnimNode::ChainIK(n) => triangulate_node(&mut n.inner),
        AnimNode::BodyAim(n) => triangulate_node(&mut n.inner),
        AnimNode::StrideWarp(n) => triangulate_node(&mut n.inner),
        AnimNode::OrientationWarp(n) => triangulate_node(&mut n.inner),
        AnimNode::Clip(_) | AnimNode::BlendSpace1D(_) => {}
    }
}

fn require_param(declared: &[&str], name: &str, what: &str) -> Result<(), AnimGraphError> {
    if declared.contains(&name) {
        Ok(())
    } else {
        Err(AnimGraphError::Invalid(format!(
            "{what} references undeclared parameter {name:?}"
        )))
    }
}

/// A number read from the file must be finite (RON accepts `NaN` and `inf`, which would poison
/// every pose the graph produces).
fn require_finite(value: f32, what: &str) -> Result<(), AnimGraphError> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(AnimGraphError::Invalid(format!(
            "{what} must be a finite number, got {value}"
        )))
    }
}

fn validate_node(node: &AnimNode, declared: &[&str]) -> Result<(), AnimGraphError> {
    match node {
        AnimNode::Clip(n) => require_finite(n.speed, &format!("clip {:?} speed", n.clip))?,
        AnimNode::BlendSpace1D(n) => {
            require_param(declared, &n.space.param, "blend space")?;
            if n.space.samples.is_empty() {
                return Err(AnimGraphError::Invalid("blend space has no samples".into()));
            }
            for sample in &n.space.samples {
                require_finite(sample.value, &format!("blend space sample {:?}", sample.clip))?;
            }
        }
        AnimNode::BlendSpace2D(n) => {
            require_param(declared, &n.space.param_x, "blend space")?;
            require_param(declared, &n.space.param_y, "blend space")?;
            if n.space.samples.is_empty() {
                return Err(AnimGraphError::Invalid("blend space has no samples".into()));
            }
            for sample in &n.space.samples {
                for coordinate in sample.point {
                    require_finite(coordinate, &format!("blend space sample {:?}", sample.clip))?;
                }
            }
        }
        AnimNode::StateMachine(sm) => {
            for (_, state) in &sm.states {
                validate_node(state, declared)?;
            }
            if sm.state_index(&sm.initial).is_none() {
                return Err(AnimGraphError::Invalid(format!(
                    "initial state {:?} does not exist",
                    sm.initial
                )));
            }
            for transition in &sm.transitions {
                require_finite(transition.duration, "transition duration")?;
                if transition.duration < 0.0 {
                    return Err(AnimGraphError::Invalid(format!(
                        "transition to {:?} has a negative duration {}",
                        transition.to, transition.duration
                    )));
                }
                if let Some(from) = &transition.from
                    && sm.state_index(from).is_none()
                {
                    return Err(AnimGraphError::Invalid(format!(
                        "transition source state {from:?} does not exist"
                    )));
                }
                if sm.state_index(&transition.to).is_none() {
                    return Err(AnimGraphError::Invalid(format!(
                        "transition target state {:?} does not exist",
                        transition.to
                    )));
                }
                for condition in &transition.conditions {
                    if let Condition::Float { param, .. }
                    | Condition::Int { param, .. }
                    | Condition::Bool { param, .. }
                    | Condition::Trigger { param } = condition
                    {
                        require_param(declared, param, "transition condition")?;
                    }
                    // A NaN threshold compares false forever, so the transition could never fire.
                    if let Condition::Float { param, value, .. } = condition {
                        require_finite(*value, &format!("transition condition on {param:?}"))?;
                    }
                }
            }
        }
        AnimNode::Blend(n) => {
            require_param(declared, &n.alpha_param, "blend node")?;
            validate_node(&n.a, declared)?;
            validate_node(&n.b, declared)?;
        }
        AnimNode::Additive(n) => {
            require_param(declared, &n.weight_param, "additive node")?;
            validate_node(&n.base, declared)?;
            validate_node(&n.additive, declared)?;
        }
        AnimNode::Layer(n) => {
            validate_node(&n.base, declared)?;
            for layer in &n.layers {
                require_param(declared, &layer.weight_param, "layer")?;
                require_finite(layer.weight_inertialize, "layer weight_inertialize")?;
                validate_node(&layer.graph, declared)?;
            }
        }
        AnimNode::TwoBoneIK(n) => {
            if let IkTarget::Param(p) = &n.target {
                require_param(declared, p, "two-bone IK target")?;
            }
            if let Some(pole) = &n.pole {
                require_param(declared, pole, "two-bone IK pole")?;
            }
            if let Some(w) = &n.weight_param {
                require_param(declared, w, "two-bone IK weight")?;
            }
            validate_node(&n.inner, declared)?;
        }
        AnimNode::LookAt(n) => {
            require_finite(n.max_yaw, "look-at max_yaw")?;
            require_finite(n.max_pitch, "look-at max_pitch")?;
            if let IkTarget::Param(p) = &n.target {
                require_param(declared, p, "look-at target")?;
            }
            if let Some(w) = &n.weight_param {
                require_param(declared, w, "look-at weight")?;
            }
            validate_node(&n.inner, declared)?;
        }
        AnimNode::RootMotion(n) => validate_node(&n.inner, declared)?,
        AnimNode::ChainIK(n) => {
            if n.root.is_empty() || n.tip.is_empty() {
                return Err(AnimGraphError::Invalid(
                    "a chain IK node needs a root and a tip joint".into(),
                ));
            }
            if n.iterations > MAX_CHAIN_IK_ITERATIONS {
                return Err(AnimGraphError::Invalid(format!(
                    "chain IK iterations {} exceed the cap of {MAX_CHAIN_IK_ITERATIONS}",
                    n.iterations
                )));
            }
            require_finite(n.tolerance, "chain IK tolerance")?;
            if n.tolerance < 0.0 {
                return Err(AnimGraphError::Invalid(format!(
                    "chain IK tolerance {} is negative",
                    n.tolerance
                )));
            }
            validate_limits(n.limits.as_ref(), "chain IK")?;
            if let IkTarget::Param(p) = &n.target {
                require_param(declared, p, "chain IK target")?;
            }
            if let Some(pole) = &n.pole {
                require_param(declared, pole, "chain IK pole")?;
            }
            if let Some(w) = &n.weight_param {
                require_param(declared, w, "chain IK weight")?;
            }
            validate_node(&n.inner, declared)?;
        }
        AnimNode::BodyAim(n) => {
            if n.joints.is_empty() {
                return Err(AnimGraphError::Invalid("a body aim node has no joints".into()));
            }
            if n.aim_joint.is_empty() {
                return Err(AnimGraphError::Invalid("a body aim node has no aim joint".into()));
            }
            for (joint, share) in &n.joints {
                if joint.is_empty() {
                    return Err(AnimGraphError::Invalid(
                        "a body aim node has a turning joint with an empty name".into(),
                    ));
                }
                require_finite(*share, &format!("body aim share of {joint:?}"))?;
                if *share < 0.0 {
                    return Err(AnimGraphError::Invalid(format!(
                        "body aim share of {joint:?} is negative ({share})"
                    )));
                }
            }
            for c in n.axis.to_array() {
                require_finite(c, "body aim axis")?;
            }
            if n.axis.length_squared() < 1e-12 {
                return Err(AnimGraphError::Invalid("body aim axis is zero".into()));
            }
            if n.passes == 0 || n.passes > MAX_BODY_AIM_PASSES {
                return Err(AnimGraphError::Invalid(format!(
                    "body aim passes {} must be 1..={MAX_BODY_AIM_PASSES}",
                    n.passes
                )));
            }
            validate_limits(n.limits.as_ref(), "body aim")?;
            if let IkTarget::Param(p) = &n.target {
                require_param(declared, p, "body aim target")?;
            }
            if let Some(w) = &n.weight_param {
                require_param(declared, w, "body aim weight")?;
            }
            validate_node(&n.inner, declared)?;
        }
        AnimNode::Slot(n) => {
            if n.name.is_empty() {
                return Err(AnimGraphError::Invalid("a slot node has no slot name".into()));
            }
            validate_node(&n.source, declared)?;
        }
        AnimNode::StrideWarp(n) => {
            require_param(declared, &n.ground_speed_param, "stride warp ground speed")?;
            n.settings
                .validate()
                .map_err(|e| AnimGraphError::Invalid(format!("stride warp settings: {e}")))?;
            require_finite(n.stride_length, "stride warp stride_length")?;
            if n.stride_length < 0.0 {
                return Err(AnimGraphError::Invalid(format!(
                    "stride warp stride_length {} is negative",
                    n.stride_length
                )));
            }
            if n.settings.mode == StrideMode::Pose && (n.legs.is_empty() || n.pelvis.is_empty()) {
                return Err(AnimGraphError::Invalid(
                    "a stride warp in Pose mode needs legs and a pelvis joint (or use Rate)".into(),
                ));
            }
            if n.legs.iter().flatten().any(String::is_empty) {
                return Err(AnimGraphError::Invalid(
                    "a stride warp leg has an empty joint name".into(),
                ));
            }
            validate_node(&n.inner, declared)?;
        }
        AnimNode::OrientationWarp(n) => {
            require_param(declared, &n.heading_param, "orientation warp heading")?;
            if n.pelvis.is_empty() {
                return Err(AnimGraphError::Invalid(
                    "an orientation warp has no pelvis joint".into(),
                ));
            }
            let settings = crate::motion_matching::warp::OrientationWarpSettings {
                strength: n.strength,
                max_delta: n.max_delta,
                spine_joints: vec![0; n.spine.len()],
                spine_weights: n.spine.iter().map(|(_, w)| *w).collect(),
            };
            settings
                .validate()
                .map_err(|e| AnimGraphError::Invalid(format!("orientation warp: {e}")))?;
            if n.spine.iter().any(|(joint, _)| joint.is_empty()) {
                return Err(AnimGraphError::Invalid(
                    "an orientation warp spine entry has no joint name".into(),
                ));
            }
            validate_node(&n.inner, declared)?;
        }
    }
    Ok(())
}

/// Every number of a limit must be finite (a NaN cone would turn every joint into NaN).
fn validate_limits(limits: Option<&ChainLimits>, what: &str) -> Result<(), AnimGraphError> {
    let check = |limit: &JointLimit| -> Result<(), AnimGraphError> {
        match *limit {
            JointLimit::Free => Ok(()),
            JointLimit::Cone {
                max_swing,
                min_twist,
                max_twist,
            } => {
                require_finite(max_swing, &format!("{what} cone max_swing"))?;
                require_finite(min_twist, &format!("{what} cone min_twist"))?;
                require_finite(max_twist, &format!("{what} cone max_twist"))
            }
            JointLimit::Hinge { axis, min, max } => {
                for c in axis.to_array() {
                    require_finite(c, &format!("{what} hinge axis"))?;
                }
                require_finite(min, &format!("{what} hinge min"))?;
                require_finite(max, &format!("{what} hinge max"))
            }
        }
    };
    match limits {
        None => Ok(()),
        Some(ChainLimits::Uniform(limit)) => check(limit),
        Some(ChainLimits::PerJoint(list)) => {
            for entry in list {
                if entry.joint.is_empty() {
                    return Err(AnimGraphError::Invalid(format!("{what} limit has no joint name")));
                }
                check(&entry.limit)?;
            }
            Ok(())
        }
    }
}

/// The direct children of a node, in declaration order.
fn children(node: &AnimNode) -> Vec<&AnimNode> {
    match node {
        AnimNode::Clip(_) | AnimNode::BlendSpace1D(_) | AnimNode::BlendSpace2D(_) => Vec::new(),
        AnimNode::StateMachine(sm) => sm.states.iter().map(|(_, n)| n).collect(),
        AnimNode::Blend(n) => vec![&*n.a, &*n.b],
        AnimNode::Additive(n) => vec![&*n.base, &*n.additive],
        AnimNode::Layer(n) => std::iter::once(&*n.base)
            .chain(n.layers.iter().map(|l| &*l.graph))
            .collect(),
        AnimNode::TwoBoneIK(n) => vec![&*n.inner],
        AnimNode::LookAt(n) => vec![&*n.inner],
        AnimNode::RootMotion(n) => vec![&*n.inner],
        AnimNode::Slot(n) => vec![&*n.source],
        AnimNode::ChainIK(n) => vec![&*n.inner],
        AnimNode::BodyAim(n) => vec![&*n.inner],
        AnimNode::StrideWarp(n) => vec![&*n.inner],
        AnimNode::OrientationWarp(n) => vec![&*n.inner],
    }
}

/// The direct children of a node, for editing.
fn children_mut(node: &mut AnimNode) -> Vec<&mut AnimNode> {
    match node {
        AnimNode::Clip(_) | AnimNode::BlendSpace1D(_) | AnimNode::BlendSpace2D(_) => Vec::new(),
        AnimNode::StateMachine(sm) => sm.states.iter_mut().map(|(_, n)| n).collect(),
        AnimNode::Blend(n) => vec![&mut *n.a, &mut *n.b],
        AnimNode::Additive(n) => vec![&mut *n.base, &mut *n.additive],
        AnimNode::Layer(n) => std::iter::once(&mut *n.base)
            .chain(n.layers.iter_mut().map(|l| &mut *l.graph))
            .collect(),
        AnimNode::TwoBoneIK(n) => vec![&mut *n.inner],
        AnimNode::LookAt(n) => vec![&mut *n.inner],
        AnimNode::RootMotion(n) => vec![&mut *n.inner],
        AnimNode::Slot(n) => vec![&mut *n.source],
        AnimNode::ChainIK(n) => vec![&mut *n.inner],
        AnimNode::BodyAim(n) => vec![&mut *n.inner],
        AnimNode::StrideWarp(n) => vec![&mut *n.inner],
        AnimNode::OrientationWarp(n) => vec![&mut *n.inner],
    }
}

/// Runs per rig per frame while a montage plays, so it walks the tree without allocating.
fn has_slot(node: &AnimNode) -> bool {
    match node {
        AnimNode::Slot(_) => true,
        AnimNode::Clip(_) | AnimNode::BlendSpace1D(_) | AnimNode::BlendSpace2D(_) => false,
        AnimNode::StateMachine(sm) => sm.states.iter().any(|(_, n)| has_slot(n)),
        AnimNode::Blend(n) => has_slot(&n.a) || has_slot(&n.b),
        AnimNode::Additive(n) => has_slot(&n.base) || has_slot(&n.additive),
        AnimNode::Layer(n) => has_slot(&n.base) || n.layers.iter().any(|l| has_slot(&l.graph)),
        AnimNode::TwoBoneIK(n) => has_slot(&n.inner),
        AnimNode::LookAt(n) => has_slot(&n.inner),
        AnimNode::RootMotion(n) => has_slot(&n.inner),
        AnimNode::ChainIK(n) => has_slot(&n.inner),
        AnimNode::BodyAim(n) => has_slot(&n.inner),
        AnimNode::StrideWarp(n) => has_slot(&n.inner),
        AnimNode::OrientationWarp(n) => has_slot(&n.inner),
    }
}

fn rename_slot_in(node: &mut AnimNode, from: &str, to: &str) -> usize {
    let mut renamed = 0;
    if let AnimNode::Slot(n) = node
        && n.name == from
    {
        n.name = to.to_string();
        renamed = 1;
    }
    for child in children_mut(node) {
        renamed += rename_slot_in(child, from, to);
    }
    renamed
}

fn collect_slots(node: &AnimNode, out: &mut Vec<String>) {
    if let AnimNode::Slot(n) = node
        && !out.contains(&n.name)
    {
        out.push(n.name.clone());
    }
    for child in children(node) {
        collect_slots(child, out);
    }
}

impl AnimGraph {
    /// Whether any node is a montage [`SlotNode`] (the evaluate system then advances montages
    /// before the graph, spec 34 M9).
    #[must_use]
    pub fn has_slot_nodes(&self) -> bool {
        has_slot(&self.root)
    }

    /// The slot names the graph's `Slot` nodes play, in first-use order.
    #[must_use]
    pub fn slot_names(&self) -> Vec<String> {
        let mut out = Vec::new();
        collect_slots(&self.root, &mut out);
        out
    }

    /// Load warnings for `Slot` nodes naming a slot the skeleton's table lacks
    /// (`animgraph_unknown_slot`; the slot still plays, full body in group `default`).
    #[must_use]
    pub fn check_slots(&self, slots: &crate::skeleton_slots::SkeletonSlots) -> Vec<String> {
        self.slot_names()
            .into_iter()
            .filter(|name| slots.get(name).is_none())
            .map(|name| {
                format!("animgraph_unknown_slot: a Slot node plays {name:?}, which the skeleton's slot table lacks")
            })
            .collect()
    }

    /// [`AnimGraph::check_slots`] against the slots a [`crate::MontagePlayer`] offers. A player
    /// with no slot list plays every slot full body, so nothing is unknown to it.
    #[must_use]
    pub fn check_slot_defs(&self, slots: &[crate::montage_player::SlotDef]) -> Vec<String> {
        if slots.is_empty() {
            return Vec::new();
        }
        self.slot_names()
            .into_iter()
            .filter(|name| !slots.iter().any(|s| s.name == *name))
            .map(|name| {
                format!("animgraph_unknown_slot: a Slot node plays {name:?}, which the rig's slots lack")
            })
            .collect()
    }

    /// Point every `Slot` node named `from` at `to` (a slot rename, spec 34 § 3). Returns how
    /// many nodes changed.
    pub fn rename_slot(&mut self, from: &str, to: &str) -> usize {
        rename_slot_in(&mut self.root, from, to)
    }
}

/// The on-disk document: everything plus the format version.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnimGraphDocument {
    /// Format version of this file.
    pub format_version: u32,
    /// Parameter declarations.
    pub parameters: Vec<ParamDecl>,
    /// Root node.
    pub root: AnimNode,
}

/// One registered migration step: transforms a document from version `from` to `to`.
#[derive(Clone, Copy)]
pub struct MigrationStep {
    /// Version this step reads.
    pub from: u32,
    /// Version this step writes.
    pub to: u32,
    /// Applies the transformation in place.
    pub apply: fn(&mut AnimGraphDocument) -> Result<(), String>,
}

/// The built-in migration steps (`regia upgrade-project` calls [`migrate_document`] on
/// project files).
///
/// - 1 → 2: v2 only adds fields whose defaults reproduce v1 (`blend: Crossfade`,
///   `weight_inertialize: 0`); serde fills them while parsing, so the step bumps the version.
/// - 2 → 3: v3 only adds the `Slot` node variant (ADR-1870); a v2 graph has none, so the step
///   bumps the version.
/// - 3 → 4: v4 only adds the `ChainIK` and `BodyAim` node variants (ADR-8475) and the
///   `StrideWarp` and `OrientationWarp` variants (ADR-8540); a v3 graph has none, so the step
///   bumps the version.
pub fn registered_migrations() -> &'static [MigrationStep] {
    &[
        MigrationStep {
            from: 1,
            to: 2,
            apply: migrate_v1_to_v2,
        },
        MigrationStep {
            from: 2,
            to: 3,
            apply: migrate_v2_to_v3,
        },
        MigrationStep {
            from: 3,
            to: 4,
            apply: migrate_v3_to_v4,
        },
    ]
}

fn migrate_v1_to_v2(_document: &mut AnimGraphDocument) -> Result<(), String> {
    Ok(())
}

fn migrate_v2_to_v3(_document: &mut AnimGraphDocument) -> Result<(), String> {
    Ok(())
}

fn migrate_v3_to_v4(_document: &mut AnimGraphDocument) -> Result<(), String> {
    Ok(())
}

/// The versions a built-in migration step leaves (`from` side), for `cargo xtask check-formats`
/// to verify a version bump ships with a migrator from the previous version.
pub fn animgraph_migration_sources() -> Vec<u32> {
    registered_migrations().iter().map(|s| s.from).collect()
}

/// The serde field-list fingerprint of the `.animgraph.ron` format (hashed by
/// `cargo xtask check-formats`).
///
/// The root document *and* every nested struct are listed, so a shape change anywhere in the
/// graph tree changes the hash.
pub fn animgraph_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<AnimGraphDocument>(),
        regia_core::schema::serde_field_names::<AnimGraph>(),
        regia_core::schema::serde_field_names::<ClipNode>(),
        regia_core::schema::serde_field_names::<BlendSpace1DNode>(),
        regia_core::schema::serde_field_names::<BlendSpace2DNode>(),
        regia_core::schema::serde_field_names::<BlendNode>(),
        regia_core::schema::serde_field_names::<AdditiveNode>(),
        regia_core::schema::serde_field_names::<JointMaskDef>(),
        regia_core::schema::serde_field_names::<LayerEntry>(),
        regia_core::schema::serde_field_names::<LayerNode>(),
        regia_core::schema::serde_field_names::<TwoBoneIKNode>(),
        regia_core::schema::serde_field_names::<LookAtNode>(),
        regia_core::schema::serde_field_names::<RootMotionNode>(),
        regia_core::schema::serde_field_names::<SlotNode>(),
        regia_core::schema::serde_field_names::<ChainIKNode>(),
        regia_core::schema::serde_field_names::<BodyAimNode>(),
        regia_core::schema::serde_field_names::<NamedJointLimit>(),
        regia_core::schema::serde_variant_names::<ChainLimits>(),
        regia_core::schema::serde_variant_names::<ChainSolver>(),
        regia_core::schema::serde_variant_names::<JointLimit>(),
        // ADR-8732: the payload shapes of the v4 IK enums (struct-variant field names), so a
        // renamed or added `JointLimit::Cone` / `Hinge` field moves the fingerprint.
        regia_core::schema::serde_variant_shapes::<JointLimit>(),
        regia_core::schema::serde_variant_shapes::<ChainLimits>(),
        regia_core::schema::serde_variant_shapes::<IkTarget>(),
        regia_core::schema::serde_field_names::<crate::blendspace::BlendSample1D>(),
        regia_core::schema::serde_field_names::<crate::blendspace::BlendSample2D>(),
        regia_core::schema::serde_field_names::<crate::blendspace::BlendSpace1D>(),
        regia_core::schema::serde_field_names::<crate::blendspace::BlendSpace2D>(),
        regia_core::schema::serde_field_names::<crate::state_machine::TransitionDef>(),
        regia_core::schema::serde_field_names::<crate::state_machine::StateMachineDef>(),
        regia_core::schema::serde_variant_names::<IkTarget>(),
        regia_core::schema::serde_variant_names::<LayerMode>(),
        regia_core::schema::serde_variant_names::<crate::state_machine::TransitionBlend>(),
        regia_core::schema::serde_variant_names::<AnimNode>(),
        regia_core::schema::serde_field_names::<StrideWarpNode>(),
        regia_core::schema::serde_field_names::<OrientationWarpNode>(),
        regia_core::schema::serde_field_names::<StrideWarpSettings>(),
        regia_core::schema::serde_variant_names::<StrideMode>(),
    ])
}

/// Migrate a document to [`ANIMGRAPH_FORMAT_VERSION`] using `registered_migrations()`.
///
/// # Errors
///
/// Returns [`AnimGraphError::UnknownVersion`] when the chain cannot reach the current version.
pub fn migrate_document(document: &mut AnimGraphDocument) -> Result<u32, AnimGraphError> {
    migrate_with(document, registered_migrations())
}

/// Migrate using a caller-supplied step table (used by tests and by the project upgrader).
///
/// # Errors
///
/// Returns [`AnimGraphError::UnknownVersion`] when no step chain reaches the current version.
pub fn migrate_with(
    document: &mut AnimGraphDocument,
    steps: &[MigrationStep],
) -> Result<u32, AnimGraphError> {
    let mut applied = 0;
    while document.format_version < ANIMGRAPH_FORMAT_VERSION {
        let step = steps
            .iter()
            .find(|s| s.from == document.format_version && s.to == s.from + 1);
        match step {
            Some(step) => {
                (step.apply)(document).map_err(|e| {
                    AnimGraphError::Invalid(format!("migration {}→{} failed: {e}", step.from, step.to))
                })?;
                document.format_version = step.to;
                applied += 1;
            }
            None => {
                return Err(AnimGraphError::UnknownVersion(document.format_version));
            }
        }
    }
    if document.format_version > ANIMGRAPH_FORMAT_VERSION {
        return Err(AnimGraphError::UnknownVersion(document.format_version));
    }
    Ok(applied)
}

/// Errors from graph loading and validation.
#[derive(Debug, thiserror::Error)]
pub enum AnimGraphError {
    /// RON parse error (with span).
    #[error("anim graph RON parse error: {0}")]
    RonParse(#[from] ron::error::SpannedError),
    /// RON serialisation error.
    #[error("anim graph RON write error: {0}")]
    RonWrite(#[from] ron::Error),
    /// The document's format version cannot be migrated to the current version.
    #[error(
        "anim graph format version {0} is unknown (current is {ANIMGRAPH_FORMAT_VERSION}); no migration reaches it"
    )]
    UnknownVersion(u32),
    /// The graph failed validation.
    #[error("invalid anim graph: {0}")]
    Invalid(String),
}

/// Convenience constructor for a trigger declaration.
pub fn trigger_decl(name: &str) -> ParamDecl {
    ParamDecl(name.into(), ParamValue::Trigger)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blendspace::BlendSample1D;
    use crate::state_machine::{TransitionBlend, TransitionCurve, TransitionDef};

    fn locomotion_graph_ron() -> String {
        let graph = AnimGraph {
            parameters: vec![
                ParamDecl("Speed".into(), ParamValue::Float(0.0)),
                trigger_decl("Jump"),
                ParamDecl("WaveWeight".into(), ParamValue::Float(0.0)),
            ],
            root: AnimNode::Layer(LayerNode {
                base: Box::new(AnimNode::StateMachine(StateMachineDef {
                    initial: "Grounded".into(),
                    states: vec![
                        (
                            "Grounded".into(),
                            AnimNode::BlendSpace1D(BlendSpace1DNode {
                                space: BlendSpace1D {
                                    param: "Speed".into(),
                                    samples: vec![
                                        BlendSample1D {
                                            value: 0.0,
                                            clip: "Idle".into(),
                                        },
                                        BlendSample1D {
                                            value: 1.6,
                                            clip: "Walk".into(),
                                        },
                                        BlendSample1D {
                                            value: 6.0,
                                            clip: "Run".into(),
                                        },
                                    ],
                                },
                                sync_group: Some("Locomotion".into()),
                            }),
                        ),
                        (
                            "Jump".into(),
                            AnimNode::Clip(ClipNode {
                                clip: "JumpLoop".into(),
                                looping: false,
                                speed: 1.0,
                                sync_group: None,
                            }),
                        ),
                    ],
                    transitions: vec![
                        TransitionDef {
                            from: None,
                            to: "Jump".into(),
                            conditions: vec![Condition::Trigger { param: "Jump".into() }],
                            duration: 0.15,
                            curve: TransitionCurve::Smooth,
                            interruptible: false,
                            blend: TransitionBlend::Inertialize,
                        },
                        TransitionDef {
                            from: Some("Jump".into()),
                            to: "Grounded".into(),
                            conditions: vec![Condition::ClipFinished],
                            duration: 0.2,
                            curve: TransitionCurve::Linear,
                            interruptible: false,
                            blend: TransitionBlend::Crossfade,
                        },
                    ],
                })),
                layers: vec![LayerEntry {
                    graph: Box::new(AnimNode::Clip(ClipNode {
                        clip: "Wave".into(),
                        looping: true,
                        speed: 1.0,
                        sync_group: None,
                    })),
                    mask: JointMaskDef {
                        joints: vec!["Spine".into(), "ArmL".into()],
                        include_children: true,
                    },
                    mode: LayerMode::Override,
                    weight_param: "WaveWeight".into(),
                    weight_inertialize: 0.15,
                }],
            }),
        };
        graph.to_ron_string().expect("serialize")
    }

    #[test]
    fn ron_round_trip() {
        let text = locomotion_graph_ron();
        let graph = AnimGraph::from_ron_str(&text).expect("parse");
        assert_eq!(graph.parameters.len(), 3);
        assert!(matches!(graph.root, AnimNode::Layer(_)));
        // Text written again parses to the same graph.
        let again = AnimGraph::from_ron_str(&graph.to_ron_string().expect("write")).expect("reparse");
        assert_eq!(again.parameters, graph.parameters);
    }

    #[test]
    fn rejects_undeclared_parameter() {
        // Parse the full text, then strip the WaveWeight declaration by re-serialising a
        // graph that lacks it.
        let full = locomotion_graph_ron();
        let mut graph = AnimGraph::from_ron_str(&full).expect("full graph parses");
        graph.parameters.retain(|p| p.0 != "WaveWeight");
        let text = graph.to_ron_string().expect("write");
        let err = AnimGraph::from_ron_str(&text).expect_err("graph must be rejected");
        assert!(err.to_string().contains("WaveWeight"), "{err}");
    }

    #[test]
    fn rejects_unknown_version() {
        let text = r#"(format_version: 99, parameters: [], root: Clip((clip: "X", looping: true, speed: 1.0, sync_group: None)))"#;
        let err = AnimGraph::from_ron_str(text).expect_err("graph must be rejected");
        assert!(matches!(err, AnimGraphError::UnknownVersion(99)));
    }

    #[test]
    fn v1_files_load_as_crossfade_and_round_trip_inertialize() {
        let v1 = r#"(format_version: 1, parameters: [("Go", Bool(false)), ("W", Float(0.0))],
            root: Layer((base: StateMachine((initial: "A",
                states: [("A", Clip((clip: "A", looping: true, speed: 1.0, sync_group: None))),
                         ("B", Clip((clip: "B", looping: true, speed: 1.0, sync_group: None)))],
                transitions: [(from: Some("A"), to: "B", conditions: [Bool(param: "Go", value: true)],
                    duration: 0.05, curve: Linear, interruptible: true)])),
                layers: [(graph: Clip((clip: "C", looping: true, speed: 1.0, sync_group: None)),
                    mask: (joints: [], include_children: true), mode: Override, weight_param: "W")]))
        )"#;
        let graph = AnimGraph::from_ron_str(v1).expect("v1 loads through the 1→2 migrator");
        let AnimNode::Layer(layer) = &graph.root else {
            panic!("layer root");
        };
        assert_eq!(layer.layers[0].weight_inertialize, 0.0);
        let AnimNode::StateMachine(sm) = layer.base.as_ref() else {
            panic!("state machine base");
        };
        assert_eq!(sm.transitions[0].blend, TransitionBlend::Crossfade);
        let written = graph.to_ron_string().expect("write");
        assert!(written.contains(&format!("format_version: {ANIMGRAPH_FORMAT_VERSION}")));
        assert!(!written.contains("blend"), "defaults are not written");
        assert!(!written.contains("weight_inertialize"));

        let text = locomotion_graph_ron();
        assert!(text.contains("blend: Inertialize"), "{text}");
        assert!(text.contains("weight_inertialize: 0.15"));
        let again = AnimGraph::from_ron_str(&text).expect("v2 parses");
        let AnimNode::Layer(layer) = &again.root else {
            panic!("layer root");
        };
        let AnimNode::StateMachine(sm) = layer.base.as_ref() else {
            panic!("state machine base");
        };
        assert_eq!(sm.transitions[0].blend, TransitionBlend::Inertialize);
        assert_eq!(layer.layers[0].weight_inertialize, 0.15);
    }

    #[test]
    fn migration_chain_runs_to_current() {
        // A document already at the current version needs no steps; a table with a step from
        // the current version is simply not consulted.
        let steps = [MigrationStep {
            from: ANIMGRAPH_FORMAT_VERSION,
            to: ANIMGRAPH_FORMAT_VERSION + 1,
            apply: |doc: &mut AnimGraphDocument| {
                doc.format_version += 1;
                Ok(())
            },
        }];
        let mut doc = AnimGraphDocument {
            format_version: ANIMGRAPH_FORMAT_VERSION,
            parameters: vec![],
            root: AnimNode::Clip(ClipNode {
                clip: "X".into(),
                looping: true,
                speed: 1.0,
                sync_group: None,
            }),
        };
        let applied = migrate_with(&mut doc, &steps).expect("migrate");
        assert_eq!(applied, 0);
        assert_eq!(doc.format_version, ANIMGRAPH_FORMAT_VERSION);
        // The built-in table takes a v1 document to the current version.
        doc.format_version = 1;
        assert_eq!(
            migrate_document(&mut doc).expect("1→current"),
            ANIMGRAPH_FORMAT_VERSION - 1
        );
        assert_eq!(doc.format_version, ANIMGRAPH_FORMAT_VERSION);
    }

    #[test]
    fn migration_unknown_version_when_chain_incomplete() {
        let steps = [MigrationStep {
            from: 0,
            to: 1,
            apply: |_doc: &mut AnimGraphDocument| Ok(()),
        }];
        let mut doc = AnimGraphDocument {
            format_version: 5,
            parameters: vec![],
            root: AnimNode::Clip(ClipNode {
                clip: "X".into(),
                looping: true,
                speed: 1.0,
                sync_group: None,
            }),
        };
        assert!(matches!(
            migrate_with(&mut doc, &steps),
            Err(AnimGraphError::UnknownVersion(5))
        ));
    }

    #[test]
    fn loads_minimal_clip_graph() {
        let text = r#"(format_version: 1, parameters: [("Speed", Float(0.0))], root: Clip((clip: "Run", looping: true, speed: 1.0, sync_group: Some("Locomotion"))))"#;
        let graph = AnimGraph::from_ron_str(text).expect("parse");
        match &graph.root {
            AnimNode::Clip(c) => {
                assert_eq!(c.clip, "Run");
                assert_eq!(c.sync_group.as_deref(), Some("Locomotion"));
            }
            other => panic!("unexpected root {other:?}"),
        }
    }

    #[test]
    fn triangulates_2d_blendspaces_on_load() {
        let text = r#"(
            format_version: 1,
            parameters: [("Sx", Float(0.0)), ("Sy", Float(0.0))],
            root: BlendSpace2D((
                space: (
                    param_x: "Sx",
                    param_y: "Sy",
                    samples: [
                        (point: (0.0, 0.0), clip: "A"),
                        (point: (1.0, 0.0), clip: "B"),
                        (point: (0.0, 1.0), clip: "C"),
                        (point: (1.0, 1.0), clip: "D"),
                    ],
                    triangles: [],
                ),
                sync_group: None,
            )),
        )"#;
        let graph = AnimGraph::from_ron_str(text).expect("parse");
        match &graph.root {
            AnimNode::BlendSpace2D(n) => assert_eq!(n.space.triangles.len(), 2),
            other => panic!("unexpected root {other:?}"),
        }
    }

    #[test]
    fn loads_published_format_example() {
        let text = include_str!("../../../docs/formats/locomotion.animgraph.ron");
        let graph = AnimGraph::from_ron_str(text).expect("published example parses");
        assert!(graph.parameters.iter().any(|p| p.0 == "Speed"));
        assert!(graph.parameters.iter().any(|p| p.0 == "Land"));
        assert!(matches!(graph.root, AnimNode::RootMotion(_)));
        let again = AnimGraph::from_ron_str(&graph.to_ron_string().expect("write")).expect("reparse");
        assert_eq!(again.parameters, graph.parameters);
    }

    #[test]
    fn rejects_non_finite_numbers_and_negative_durations() {
        let cases = [
            (
                r#"(format_version: 2, parameters: [], root: Clip((clip: "X", looping: true, speed: NaN, sync_group: None)))"#,
                "speed",
            ),
            (
                r#"(format_version: 2, parameters: [("S", Float(0.0))], root: BlendSpace1D((space: (param: "S",
                samples: [(value: inf, clip: "A")]), sync_group: None)))"#,
                "blend space sample",
            ),
            (
                r#"(format_version: 2, parameters: [("T", Vec3((0.0, 0.0, 0.0)))], root: LookAt((
                inner: Clip((clip: "X", looping: true, speed: 1.0, sync_group: None)), joint: "head",
                target: Param("T"), max_yaw: NaN, max_pitch: 0.5, weight_param: None)))"#,
                "max_yaw",
            ),
            (
                r#"(format_version: 2, parameters: [("Go", Bool(false))], root: StateMachine((initial: "A",
                states: [("A", Clip((clip: "A", looping: true, speed: 1.0, sync_group: None)))],
                transitions: [(from: None, to: "A", conditions: [Bool(param: "Go", value: true)],
                    duration: -0.2, curve: Linear, interruptible: false)])))"#,
                "negative duration",
            ),
        ];
        for (text, needle) in cases {
            let err = AnimGraph::from_ron_str(text).expect_err(needle);
            assert!(err.to_string().contains(needle), "{needle}: {err}");
        }
        // Negative speed (reverse playback) is valid.
        let reverse = r#"(format_version: 2, parameters: [], root: Clip((clip: "X", looping: true, speed: -1.0, sync_group: None)))"#;
        assert!(AnimGraph::from_ron_str(reverse).is_ok());
    }
}
