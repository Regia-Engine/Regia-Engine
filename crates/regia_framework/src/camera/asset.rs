// SPDX-License-Identifier: MPL-2.0
//! The authored camera asset (`.camera.ron`, format `camera` v2): data types, RON form,
//! validation and the version contract (`docs/specs/35-camera-framework.md` §§ 3-8,
//! `docs/specs/12-assets.md` § 4a).
//!
//! A [`CameraAsset`] describes camera behavior as data: *rigs* (ordered pipelines of [`Node`]s),
//! shared [`TransitionDef`]s between rigs, one [`DirectorDef`] that picks the active rig, asset
//! parameters, runtime variables and input slots. [`CameraDriver`](super::driver::CameraDriver)
//! evaluates it every frame.
//!
//! ```
//! use regia_framework::camera::asset::CameraAsset;
//!
//! let asset = CameraAsset::from_ron(r#"(
//!     format_version: 1,
//!     name: "demo",
//!     rigs: [(name: "main", root: [Fov(degrees: Literal(70.0))])],
//!     director: Fixed("main"),
//! )"#).expect("valid asset");
//! assert_eq!(asset.rigs.len(), 1);
//! assert!(asset.validate(&|_| false).iter().all(|issue| !issue.is_error()));
//! ```

use std::collections::{BTreeMap, BTreeSet};

use regia_core::version::{self, VersionStep};

use super::crane::{CraneBase, CraneLimits};
use super::rail::{RailDrive, RailOrient};
use serde::{Deserialize, Serialize};

/// Current `.camera.ron` format version (v2: the `Rail` and `Crane` node kinds, ADR-3231).
pub const CAMERA_FORMAT_VERSION: u32 = 2;

/// The cooked type path of a camera asset; the payload is the asset's canonical RON text.
pub const CAMERA_ASSET_TYPE_PATH: &str = "regia_framework::camera::asset::CameraAsset";

/// Camera asset and driver errors. Each variant maps to a stable lowercase protocol code via
/// [`CameraError::code`].
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CameraError {
    /// The file is a newer version than this build reads.
    #[error("camera asset version {found} is newer than supported version {expected}; upgrade the engine")]
    Version {
        /// Version in the file.
        found: u32,
        /// Version this build reads.
        expected: u32,
    },
    /// RON syntax or shape error (the text carries line and column).
    #[error("camera asset parse error: {0}")]
    Parse(String),
    /// A migration step failed.
    #[error("camera asset migration failed: {0}")]
    Migration(String),
    /// A parameter override has the wrong type or names no parameter.
    #[error("bad camera parameter `{name}`: {message}")]
    BadParam {
        /// The parameter name.
        name: String,
        /// What is wrong.
        message: String,
    },
    /// The asset has validation errors and cannot be loaded into a driver.
    #[error("camera asset `{0}` has validation errors")]
    Invalid(String),
    /// A rig name matches no rig of the asset.
    #[error("unknown camera rig `{0}`")]
    UnknownRig(String),
    /// A variable, parameter or input slot name matches nothing in the asset.
    #[error("unknown camera name `{0}`")]
    UnknownName(String),
    /// A value is not finite or out of range.
    #[error("bad camera value: {0}")]
    BadValue(String),
    /// A layer id matches no live layer.
    #[error("unknown camera layer id {0}")]
    UnknownLayer(u32),
    /// An additive layer already uses this `order`.
    #[error("additive camera layer order {0} is taken")]
    OrderTaken(i32),
}

impl CameraError {
    /// The stable lowercase protocol code of the error.
    pub fn code(&self) -> &'static str {
        match self {
            CameraError::Version { .. } | CameraError::Parse(_) | CameraError::Migration(_) => {
                "camera_asset_invalid"
            }
            CameraError::BadParam { .. } | CameraError::BadValue(_) => "camera_bad_value",
            CameraError::Invalid(_) => "camera_asset_invalid",
            CameraError::UnknownRig(_) => "camera_unknown_rig",
            CameraError::UnknownName(_) => "camera_unknown_name",
            CameraError::UnknownLayer(_) => "camera_unknown_layer_id",
            CameraError::OrderTaken(_) => "camera_order_taken",
        }
    }
}

// ---------------------------------------------------------------------------
// Values, parameters, variables
// ---------------------------------------------------------------------------

/// A node setting: a literal, an asset parameter, or a runtime variable (§ 4.1).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Value<T> {
    /// A constant.
    Literal(T),
    /// An asset parameter; a driver override wins over the asset default.
    Param(String),
    /// A runtime variable, re-read every frame.
    Var(String),
}

/// A parameter default (§ 5).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum ParamValue {
    /// A flag.
    Bool(bool),
    /// An integer.
    Int(i64),
    /// A number.
    Float(f32),
    /// Three numbers.
    Vec3([f32; 3]),
    /// A name.
    Name(String),
}

impl ParamValue {
    /// The type name used in messages.
    pub fn kind(&self) -> &'static str {
        match self {
            ParamValue::Bool(_) => "Bool",
            ParamValue::Int(_) => "Int",
            ParamValue::Float(_) => "Float",
            ParamValue::Vec3(_) => "Vec3",
            ParamValue::Name(_) => "Name",
        }
    }

    /// Whether `other` has the same type.
    pub fn same_kind(&self, other: &ParamValue) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }

    /// A numeric view: floats and ints.
    pub fn as_f32(&self) -> Option<f32> {
        match self {
            ParamValue::Float(v) => Some(*v),
            ParamValue::Int(v) => Some(*v as f32),
            _ => None,
        }
    }

    /// A flag view.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            ParamValue::Bool(v) => Some(*v),
            _ => None,
        }
    }

    /// Whether every number inside is finite.
    pub fn is_finite(&self) -> bool {
        match self {
            ParamValue::Float(v) => v.is_finite(),
            ParamValue::Vec3(v) => v.iter().all(|c| c.is_finite()),
            _ => true,
        }
    }
}

/// A runtime variable value (§ 5).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub enum VarValue {
    /// A flag.
    Bool(bool),
    /// A number.
    Float(f32),
    /// Three numbers.
    Vec3([f32; 3]),
    /// Yaw and pitch in radians.
    YawPitch(f32, f32),
}

impl VarValue {
    /// The type name used in messages.
    pub fn kind(&self) -> &'static str {
        match self {
            VarValue::Bool(_) => "Bool",
            VarValue::Float(_) => "Float",
            VarValue::Vec3(_) => "Vec3",
            VarValue::YawPitch(..) => "YawPitch",
        }
    }

    /// Whether every number inside is finite.
    pub fn is_finite(&self) -> bool {
        match self {
            VarValue::Bool(_) => true,
            VarValue::Float(v) => v.is_finite(),
            VarValue::Vec3(v) => v.iter().all(|c| c.is_finite()),
            VarValue::YawPitch(a, b) => a.is_finite() && b.is_finite(),
        }
    }
}

/// A declared runtime variable (§ 5).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VarDef {
    /// The variable name.
    pub name: String,
    /// The value before gameplay writes one.
    pub default: VarValue,
    /// Never blended between rigs: every rig sees the unblended value.
    #[serde(default)]
    pub local_only: bool,
}

// ---------------------------------------------------------------------------
// Input slots
// ---------------------------------------------------------------------------

/// How many axes an input slot carries.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotKind {
    /// One axis (yaw only; `x`).
    Axis1,
    /// Two axes (yaw, pitch).
    Axis2,
}

fn default_pitch_clamp() -> (f32, f32) {
    (-89.0, 89.0)
}

fn default_pull_weight() -> f32 {
    0.1
}

/// A named accumulator for player look input (§ 5).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct InputSlotDef {
    /// Slot name.
    pub name: String,
    /// One or two axes.
    pub kind: SlotKind,
    /// Degrees per second per unit of raw input, `(yaw, pitch)`.
    pub speed: (f32, f32),
    /// Accumulated pitch limits in degrees.
    #[serde(default = "default_pitch_clamp")]
    pub pitch_clamp: (f32, f32),
    /// Invert the vertical axis.
    #[serde(default)]
    pub invert_pitch: bool,
    /// Normalize the raw input to length at most 1 before integrating.
    #[serde(default)]
    pub normalize: bool,
    /// A variable (`YawPitch`) the accumulated angle mixes toward (snap-behind-the-player).
    #[serde(default)]
    pub pull_toward_var: Option<String>,
    /// Rate (1/s) the accumulated angle eases toward the variable (when `pull_toward_var` is set).
    #[serde(default = "default_pull_weight")]
    pub pull_weight: f32,
}

// ---------------------------------------------------------------------------
// Conditions
// ---------------------------------------------------------------------------

/// Comparison operator of [`Condition::VarCompare`].
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmp {
    /// Less than.
    Lt,
    /// Less than or equal.
    Le,
    /// Equal (exact).
    Eq,
    /// Greater than or equal.
    Ge,
    /// Greater than.
    Gt,
}

impl Cmp {
    /// Applies the operator.
    pub fn test(self, left: f32, right: f32) -> bool {
        match self {
            Cmp::Lt => left < right,
            Cmp::Le => left <= right,
            Cmp::Eq => left == right,
            Cmp::Ge => left >= right,
            Cmp::Gt => left > right,
        }
    }
}

/// A pure, allocation-free test (§ 4.4).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Condition {
    /// Seconds since the rig instance started reach this value.
    Elapsed(f32),
    /// A bool variable equals the flag.
    VarBool(String, bool),
    /// A float variable compares to the number.
    VarCompare(String, Cmp, f32),
    /// The rig being evaluated (in transitions: the incoming or outgoing rig) has this tag.
    HasTag(String),
    /// The rig being evaluated is this one.
    RigIs(String),
    /// Every condition holds (an empty list passes).
    All(Vec<Condition>),
    /// At least one holds (an empty list fails).
    Any(Vec<Condition>),
    /// The condition does not hold.
    Not(Box<Condition>),
}

// ---------------------------------------------------------------------------
// Nodes
// ---------------------------------------------------------------------------

/// What a node follows or frames: the driver's target, a named entity or a fixed point.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum TargetRef {
    /// The driver's `target` entity.
    DriveTarget,
    /// The entity whose `Name` component equals this string.
    Entity(String),
    /// A world position.
    Point([f32; 3]),
}

/// Space of a [`Node::Pivot`] offset.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PivotSpace {
    /// The offset is rotated by the target's rotation.
    #[default]
    Target,
    /// The offset is added as is.
    World,
}

/// Space of an offset or smoothing node.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CamSpace {
    /// Axes of the current view (depth along the view axis, side to the right).
    #[default]
    Camera,
    /// World axes.
    World,
}

/// Where a [`Node::Collision`] cast starts.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CollisionAnchor {
    /// The point set by the rig's `Pivot` node (the pose position when the rig has none).
    #[default]
    Pivot,
    /// The driver target's position.
    Target,
}

fn lit_zero() -> Value<f32> {
    Value::Literal(0.0)
}

fn lit_false() -> Value<bool> {
    Value::Literal(false)
}

fn lit_true() -> Value<bool> {
    Value::Literal(true)
}

fn default_speed_unit() -> f32 {
    1.0
}

fn default_close_speed() -> f32 {
    20.0
}

fn default_open_speed() -> f32 {
    4.0
}

/// One step of a rig's evaluation (§ 4.3). Reads and writes the working pose.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub enum Node {
    /// Sets `position` (and `aim`) to the target plus an offset.
    Pivot {
        /// The followed target.
        target: TargetRef,
        /// Offset in metres.
        #[serde(default)]
        offset: [f32; 3],
        /// Space of the offset.
        #[serde(default)]
        space: PivotSpace,
        /// Extra offset of the aim point from the pivot point, in the same space (zero = the
        /// aim is the pivot, § 4.3).
        #[serde(default)]
        aim_offset: [f32; 3],
        /// Also take the target's rotation as the pose rotation (a first-person eye).
        #[serde(default)]
        inherit_rotation: bool,
    },
    /// Sets yaw and pitch from an input slot's accumulated angles.
    LookInput {
        /// The input slot.
        slot: String,
        /// Add to the current rotation instead of replacing it.
        #[serde(default)]
        additive: bool,
    },
    /// Moves the position back along the view direction.
    Boom {
        /// Boom length in metres.
        length: Value<f32>,
        /// Sideways shift in camera space.
        #[serde(default = "lit_zero")]
        side_offset: Value<f32>,
        /// Upward shift in camera space.
        #[serde(default = "lit_zero")]
        up_offset: Value<f32>,
        /// Metres per second the effective length eases at (0 = instant).
        #[serde(default = "lit_zero")]
        length_speed: Value<f32>,
    },
    /// Sphere-casts from the anchor to the position and shortens the boom on a hit.
    Collision {
        /// Cast radius in metres.
        radius: f32,
        /// Physics layer names the cast tests.
        #[serde(default)]
        layers: Vec<String>,
        /// Metres per second the distance closes at.
        #[serde(default = "default_close_speed")]
        close_speed: f32,
        /// Metres per second the distance reopens at.
        #[serde(default = "default_open_speed")]
        open_speed: f32,
        /// When above 0, the distance reopens exponentially at this rate (1/s, the legacy rig's
        /// form) instead of linearly at `open_speed`.
        #[serde(default)]
        open_rate: f32,
        /// Where the cast starts.
        #[serde(default)]
        anchor: CollisionAnchor,
        /// Use the previous frame's cast result (one frame late).
        #[serde(default)]
        reuse_last_frame: bool,
    },
    /// Sets the vertical field of view.
    Fov {
        /// Degrees, clamped to 1..=170.
        degrees: Value<f32>,
    },
    /// Adds a fixed offset.
    Offset {
        /// Translation in metres.
        #[serde(default)]
        translation: [f32; 3],
        /// Rotation (pitch, yaw, roll) in degrees.
        #[serde(default)]
        rotation_deg: [f32; 3],
        /// Space of the translation.
        #[serde(default)]
        space: CamSpace,
    },
    /// Per-axis exponential smoothing of the position.
    SmoothPosition {
        /// Rate (1/s) along the view axis; 0 or absent = none.
        #[serde(default)]
        depth: f32,
        /// Rate to the side.
        #[serde(default)]
        side: f32,
        /// Rate vertically.
        #[serde(default)]
        height: f32,
        /// Space of the axes.
        #[serde(default)]
        space: CamSpace,
    },
    /// Exponential smoothing of the angles (shortest path for yaw).
    SmoothRotation {
        /// Pitch rate (1/s).
        #[serde(default)]
        pitch: f32,
        /// Yaw rate.
        #[serde(default)]
        yaw: f32,
        /// Roll rate.
        #[serde(default)]
        roll: f32,
    },
    /// Roll proportional to the yaw velocity.
    Lean {
        /// Radians of roll per radian/second of yaw velocity.
        gain: f32,
        /// Largest roll in radians.
        max: f32,
        /// Relaxation rate (1/s).
        speed: f32,
    },
    /// Keeps a target's direction inside a soft zone.
    Frame {
        /// What to frame.
        target: TargetRef,
        /// No correction inside this half-angle `(horizontal, vertical)` in degrees.
        #[serde(default)]
        dead_zone_deg: (f32, f32),
        /// Eased correction up to this half-angle; beyond it the correction is full.
        #[serde(default)]
        soft_zone_deg: (f32, f32),
        /// Easing rate (1/s) between the zones.
        #[serde(default = "default_speed_unit")]
        speed: f32,
    },
    /// Adds smooth-noise shake.
    Shake {
        /// Metres.
        amplitude: f32,
        /// Hertz.
        frequency: f32,
        /// Amplitude decay rate (1/s) since the rig started (0 = constant).
        #[serde(default)]
        decay: f32,
    },
    /// Free flight (the spectator camera): integrates a smoothed velocity into the position
    /// from a move slot (x = strafe, y = forward) and optional rise and sprint values.
    Fly {
        /// The `Axis2` input slot whose raw value is the move axes.
        slot: String,
        /// Cruising speed in metres per second.
        speed: f32,
        /// Speed multiplier while sprinting.
        #[serde(default = "default_speed_unit")]
        sprint_multiplier: f32,
        /// Velocity easing rate (1/s); 0 = instant.
        #[serde(default)]
        accel: f32,
        /// Vertical movement, positive up.
        #[serde(default = "lit_zero")]
        rise: Value<f32>,
        /// Sprint switch.
        #[serde(default = "lit_false")]
        sprint: Value<bool>,
    },
    /// Rides a spline entity at a distance in meters along it (`camera/rail.rs`, ADR-3231).
    Rail {
        /// The `Name` of the spline entity.
        spline: String,
        /// How the distance is produced.
        drive: RailDrive,
        /// How the camera is oriented.
        #[serde(default)]
        orient: RailOrient,
        /// Offset from the rail point in the rail frame: x right, y up, z forward.
        #[serde(default)]
        offset: [f32; 3],
    },
    /// An arm with a camera mount, based on the ground or on a rail (`camera/crane.rs`, ADR-3231).
    Crane {
        /// Where the base is.
        base: CraneBase,
        /// Arm yaw in degrees.
        #[serde(default = "lit_zero")]
        yaw: Value<f32>,
        /// Arm pitch in degrees, positive up.
        #[serde(default = "lit_zero")]
        pitch: Value<f32>,
        /// Arm length in meters.
        length: Value<f32>,
        /// Mount yaw relative to the arm in degrees.
        #[serde(default = "lit_zero")]
        mount_yaw: Value<f32>,
        /// Mount pitch relative to the arm in degrees.
        #[serde(default = "lit_zero")]
        mount_pitch: Value<f32>,
        /// Per-axis clamps.
        #[serde(default)]
        limits: CraneLimits,
    },
    /// Runs `nodes` when enabled.
    Group {
        /// Switch.
        #[serde(default = "lit_true")]
        enabled: Value<bool>,
        /// The nested nodes.
        nodes: Vec<Node>,
    },
    /// Runs `nodes` when `cond` holds.
    When {
        /// The condition.
        cond: Condition,
        /// The nested nodes.
        nodes: Vec<Node>,
    },
}

impl Node {
    /// The node kind as a lowercase name (used in traces and the structure check).
    pub fn kind(&self) -> &'static str {
        match self {
            Node::Pivot { .. } => "pivot",
            Node::LookInput { .. } => "look_input",
            Node::Boom { .. } => "boom",
            Node::Collision { .. } => "collision",
            Node::Fov { .. } => "fov",
            Node::Offset { .. } => "offset",
            Node::SmoothPosition { .. } => "smooth_position",
            Node::SmoothRotation { .. } => "smooth_rotation",
            Node::Lean { .. } => "lean",
            Node::Frame { .. } => "frame",
            Node::Fly { .. } => "fly",
            Node::Shake { .. } => "shake",
            Node::Rail { .. } => "rail",
            Node::Crane { .. } => "crane",
            Node::Group { .. } => "group",
            Node::When { .. } => "when",
        }
    }

    /// Nested nodes of a `Group` or `When`.
    pub fn children(&self) -> &[Node] {
        match self {
            Node::Group { nodes, .. } | Node::When { nodes, .. } => nodes,
            _ => &[],
        }
    }
}

// ---------------------------------------------------------------------------
// Rigs, transitions, directors
// ---------------------------------------------------------------------------

/// Which rigs a transition end matches.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum RigMatch {
    /// Any rig.
    Any,
    /// The rig with this name.
    Rig(String),
    /// Any rig carrying this tag.
    Tag(String),
}

/// The blend curve and duration of a transition.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub enum Blend {
    /// No interpolation.
    Cut,
    /// Linear over `t` seconds.
    Linear(f32),
    /// Smoothstep over `t` seconds.
    Smooth(f32),
    /// Quadratic ease-in over `t` seconds.
    EaseIn(f32),
    /// Quadratic ease-out over `t` seconds.
    EaseOut(f32),
}

impl Blend {
    /// Duration in seconds (0 for a cut or a non-positive or non-finite time).
    pub fn duration(self) -> f32 {
        match self {
            Blend::Cut => 0.0,
            Blend::Linear(t) | Blend::Smooth(t) | Blend::EaseIn(t) | Blend::EaseOut(t) => {
                if t.is_finite() && t > 0.0 { t } else { 0.0 }
            }
        }
    }

    /// Incoming weight after `elapsed` seconds: exactly 0 at the start and 1 at the end,
    /// monotonic in between. A cut is 1 at once.
    pub fn weight(self, elapsed: f32) -> f32 {
        let t = self.duration();
        if t <= 0.0 {
            return 1.0;
        }
        let x = (elapsed / t).clamp(0.0, 1.0);
        if x >= 1.0 {
            return 1.0;
        }
        match self {
            Blend::Cut | Blend::Linear(_) => x,
            Blend::Smooth(_) => x * x * (3.0 - 2.0 * x),
            Blend::EaseIn(_) => x * x,
            Blend::EaseOut(_) => x * (2.0 - x),
        }
    }
}

/// How the incoming rig's rotation initializes when it starts.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum StartFacing {
    /// Its own seed (the previous-frame pose of the rig).
    #[default]
    OwnSeed,
    /// The driver target's yaw and pitch.
    TargetFacing,
    /// The outgoing rig's final yaw and pitch.
    CarryOver,
    /// Look at the outgoing rig's last aim point (falls back to `OwnSeed`).
    ContinueAim,
}

fn default_true() -> bool {
    true
}

/// A blend (or cut) between two rigs (§ 6.2).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TransitionDef {
    /// Outgoing side.
    pub from: RigMatch,
    /// Incoming side.
    pub to: RigMatch,
    /// Curve and duration.
    pub blend: Blend,
    /// Rotation initialization of the incoming rig.
    #[serde(default)]
    pub start_facing: StartFacing,
    /// While blending out, the outgoing rig stops updating.
    #[serde(default)]
    pub hold_outgoing: bool,
    /// Merge node states of same-structure rigs instead of cross-fading poses.
    #[serde(default)]
    pub share_state: bool,
    /// A new primary rig during the blend starts a new blend from the blended pose.
    #[serde(default = "default_true")]
    pub interruptible: bool,
    /// Entry conditions (all must hold; empty passes).
    #[serde(default)]
    pub conditions: Vec<Condition>,
}

/// One selectable behavior (§ 2).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RigDef {
    /// Rig name.
    pub name: String,
    /// Tags transitions and conditions match.
    #[serde(default)]
    pub tags: Vec<String>,
    /// The ordered node list.
    pub root: Vec<Node>,
    /// Transitions that only apply to this rig.
    #[serde(default)]
    pub transitions: Vec<TransitionDef>,
    /// Seconds a layer of this rig blends out over when released without `instant`.
    #[serde(default)]
    pub blend_out: Option<f32>,
}

/// A director rule: the first whose condition holds wins.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RuleDef {
    /// The condition.
    pub when: Condition,
    /// The rig to run.
    pub rig: String,
}

/// A ranked sub-director.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RankedDef {
    /// Higher wins.
    pub rank: i32,
    /// The sub-director.
    pub director: DirectorDef,
}

/// The policy that chooses the active rigs each frame (§ 6.1).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub enum DirectorDef {
    /// Always this rig.
    Fixed(String),
    /// First matching rule, else `default` (an empty string = none, so a ranked sub-director
    /// can act only on demand).
    Rules {
        /// The rig when no rule matches; empty = no result.
        #[serde(default)]
        default: String,
        /// The rules in order.
        #[serde(default)]
        rules: Vec<RuleDef>,
    },
    /// The highest-ranked sub-director whose result is `Some`.
    Ranked(Vec<RankedDef>),
    /// A Rust function registered by name.
    Hook(String),
}

/// The authored camera asset (§ 4).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CameraAsset {
    /// Format version (currently 1).
    pub format_version: u32,
    /// Asset name.
    pub name: String,
    /// Public parameters with defaults.
    #[serde(default)]
    pub params: BTreeMap<String, ParamValue>,
    /// Declared runtime variables (a parameter of the same name also seeds a variable).
    #[serde(default)]
    pub variables: Vec<VarDef>,
    /// Input slots.
    #[serde(default)]
    pub inputs: Vec<InputSlotDef>,
    /// The rigs.
    pub rigs: Vec<RigDef>,
    /// Shared transitions.
    #[serde(default)]
    pub transitions: Vec<TransitionDef>,
    /// The director.
    pub director: DirectorDef,
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Severity of a [`CameraIssue`].
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// The asset cannot load.
    Error,
    /// The asset loads; the author should look.
    Warning,
}

impl Severity {
    /// Lowercase name for the protocol.
    pub fn name(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }
}

/// One validation finding with a stable code (§ 8).
#[derive(Clone, Debug, PartialEq)]
pub struct CameraIssue {
    /// Stable lowercase code (`camera_unknown_rig`, ...).
    pub code: &'static str,
    /// Error or warning.
    pub severity: Severity,
    /// The rig the issue is in, if any.
    pub rig: Option<String>,
    /// A path to the node (`2` or `3/0` inside groups), if any.
    pub node: Option<String>,
    /// Human-readable text.
    pub message: String,
}

impl CameraIssue {
    /// Whether the issue is an error.
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

/// The result of the last validation (`Stale` = edited since).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CameraBuildStatus {
    /// No issues.
    Ok,
    /// Warnings only.
    Warnings,
    /// At least one error.
    Errors,
    /// Edited since the last validation.
    Stale,
}

impl CameraBuildStatus {
    /// Lowercase name for the protocol.
    pub fn name(self) -> &'static str {
        match self {
            CameraBuildStatus::Ok => "ok",
            CameraBuildStatus::Warnings => "warnings",
            CameraBuildStatus::Errors => "errors",
            CameraBuildStatus::Stale => "stale",
        }
    }

    /// The status a list of issues amounts to.
    pub fn of(issues: &[CameraIssue]) -> Self {
        if issues.iter().any(CameraIssue::is_error) {
            CameraBuildStatus::Errors
        } else if issues.is_empty() {
            CameraBuildStatus::Ok
        } else {
            CameraBuildStatus::Warnings
        }
    }
}

fn camera_migrations() -> &'static [VersionStep<CameraAsset>] {
    // v1 -> v2 (ADR-3231) only adds the `Rail` and `Crane` node kinds (and the types they use):
    // every v1 file means the same in v2, so the step only bumps the version. A v2 file that
    // uses the new kinds is refused by an engine that reads v1 (its version check fails first).
    &[VersionStep {
        from: 1,
        to: 2,
        migrate: |_| Ok(()),
    }]
}

/// The `from` versions of the `.camera.ron` migration steps (for `cargo xtask check-formats`).
pub fn camera_migration_sources() -> Vec<u32> {
    camera_migrations().iter().map(|step| step.from).collect()
}

/// Migrates `.camera.ron` text to the current version: the document is parsed, run through the
/// migration chain (which rewrites the root `format_version`) and re-serialized. Non-UTF-8 input,
/// unknown fields and documents newer than [`CAMERA_FORMAT_VERSION`] are refused with the same
/// errors as [`CameraAsset::from_ron`]. Text already at the current version comes back unchanged,
/// comments and all.
///
/// This is the project-level migrator behind `regia upgrade-project`; in-asset loading migrates
/// through [`CameraAsset::migrate_to_latest`] instead.
pub fn migrate_camera(text: &str) -> Result<String, CameraError> {
    let found = version::peek_ron_version(text).map_err(CameraError::Parse)?;
    if found == CAMERA_FORMAT_VERSION {
        return Ok(text.to_string());
    }
    CameraAsset::from_ron(text)?.to_ron()
}

/// The serde shape fingerprint of `.camera.ron` (hashed by `cargo xtask check-formats`): the
/// field list of every struct reachable from [`CameraAsset`], then the spellings of its enums.
pub fn camera_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<CameraAsset>(),
        serde_field_names::<RigDef>(),
        serde_field_names::<TransitionDef>(),
        serde_field_names::<RuleDef>(),
        serde_field_names::<RankedDef>(),
        serde_field_names::<VarDef>(),
        serde_field_names::<InputSlotDef>(),
        serde_variant_names::<Node>(),
        serde_variant_names::<DirectorDef>(),
        serde_variant_names::<Value<f32>>(),
        serde_variant_names::<ParamValue>(),
        serde_variant_names::<VarValue>(),
        serde_variant_names::<SlotKind>(),
        serde_variant_names::<Cmp>(),
        serde_variant_names::<Condition>(),
        serde_variant_names::<TargetRef>(),
        serde_variant_names::<PivotSpace>(),
        serde_variant_names::<CamSpace>(),
        serde_variant_names::<CollisionAnchor>(),
        serde_variant_names::<RigMatch>(),
        serde_variant_names::<Blend>(),
        serde_variant_names::<StartFacing>(),
        serde_variant_names::<RailDrive>(),
        serde_variant_names::<RailOrient>(),
        serde_variant_names::<CraneBase>(),
        serde_field_names::<CraneLimits>(),
    ])
}

/// Collects issues while walking an asset.
struct Checker<'a> {
    asset: &'a CameraAsset,
    hooks: &'a dyn Fn(&str) -> bool,
    issues: Vec<CameraIssue>,
    used_params: BTreeSet<String>,
    rigs: BTreeSet<&'a str>,
    var_names: BTreeSet<&'a str>,
}

impl<'a> Checker<'a> {
    fn push(
        &mut self,
        code: &'static str,
        severity: Severity,
        rig: Option<&str>,
        node: Option<String>,
        message: String,
    ) {
        self.issues.push(CameraIssue {
            code,
            severity,
            rig: rig.map(str::to_owned),
            node,
            message,
        });
    }

    fn error(&mut self, code: &'static str, rig: Option<&str>, node: Option<String>, message: String) {
        self.push(code, Severity::Error, rig, node, message);
    }

    fn warn(&mut self, code: &'static str, rig: Option<&str>, node: Option<String>, message: String) {
        self.push(code, Severity::Warning, rig, node, message);
    }

    fn check_rig_name(&mut self, name: &str, rig: Option<&str>, what: &str) {
        if !self.rigs.contains(name) {
            self.error(
                "camera_unknown_rig",
                rig,
                None,
                format!("{what} names rig `{name}`, which does not exist"),
            );
        }
    }

    fn check_var(&mut self, name: &str, rig: Option<&str>, node: &Option<String>) {
        if !self.var_names.contains(name) {
            self.error(
                "camera_unknown_name",
                rig,
                node.clone(),
                format!("variable `{name}` is not declared"),
            );
        }
    }

    fn check_condition(&mut self, cond: &Condition, rig: Option<&str>, node: &Option<String>) {
        match cond {
            Condition::Elapsed(seconds) => {
                if !seconds.is_finite() || *seconds < 0.0 {
                    self.error(
                        "camera_bad_value",
                        rig,
                        node.clone(),
                        format!("Elapsed({seconds}) must be finite and not negative"),
                    );
                }
            }
            Condition::VarBool(name, _) => self.check_var(name, rig, node),
            Condition::VarCompare(name, _, value) => {
                self.check_var(name, rig, node);
                if !value.is_finite() {
                    self.error(
                        "camera_bad_value",
                        rig,
                        node.clone(),
                        format!("comparison against {value} is not finite"),
                    );
                }
            }
            Condition::RigIs(name) => self.check_rig_name(name, rig, "RigIs"),
            Condition::HasTag(_) => {}
            Condition::All(list) | Condition::Any(list) => {
                for inner in list {
                    self.check_condition(inner, rig, node);
                }
            }
            Condition::Not(inner) => self.check_condition(inner, rig, node),
        }
    }

    fn check_value_f32(
        &mut self,
        value: &Value<f32>,
        rig: &str,
        node: &Option<String>,
        what: &str,
        min: f32,
    ) {
        match value {
            Value::Literal(v) => {
                if !v.is_finite() || *v < min {
                    self.error(
                        "camera_bad_value",
                        Some(rig),
                        node.clone(),
                        format!("{what} = {v} must be finite and at least {min}"),
                    );
                }
            }
            Value::Param(name) => {
                self.used_params.insert(name.clone());
                match self.asset.params.get(name) {
                    Some(p) if p.as_f32().is_some() => {}
                    Some(p) => self.error(
                        "camera_bad_value",
                        Some(rig),
                        node.clone(),
                        format!(
                            "{what} uses parameter `{name}` of type {}, a number is needed",
                            p.kind()
                        ),
                    ),
                    None => self.error(
                        "camera_unknown_name",
                        Some(rig),
                        node.clone(),
                        format!("parameter `{name}` is not declared"),
                    ),
                }
            }
            Value::Var(name) => self.check_var(name, Some(rig), node),
        }
    }

    fn check_value_bool(&mut self, value: &Value<bool>, rig: &str, node: &Option<String>) {
        match value {
            Value::Literal(_) => {}
            Value::Param(name) => {
                self.used_params.insert(name.clone());
                match self.asset.params.get(name) {
                    Some(ParamValue::Bool(_)) => {}
                    Some(p) => self.error(
                        "camera_bad_value",
                        Some(rig),
                        node.clone(),
                        format!(
                            "`enabled` uses parameter `{name}` of type {}, a Bool is needed",
                            p.kind()
                        ),
                    ),
                    None => self.error(
                        "camera_unknown_name",
                        Some(rig),
                        node.clone(),
                        format!("parameter `{name}` is not declared"),
                    ),
                }
            }
            Value::Var(name) => self.check_var(name, Some(rig), node),
        }
    }

    fn check_finite(&mut self, values: &[f32], rig: &str, node: &Option<String>, what: &str) {
        if values.iter().any(|v| !v.is_finite()) {
            self.error(
                "camera_bad_value",
                Some(rig),
                node.clone(),
                format!("{what} must be finite"),
            );
        }
    }

    fn check_spline_name(&mut self, name: &str, rig: &str, node: &Option<String>) {
        if name.is_empty() {
            self.error(
                "camera_bad_value",
                Some(rig),
                node.clone(),
                "a rail or crane needs the name of a spline entity".to_owned(),
            );
        }
    }

    fn check_target(&mut self, target: &TargetRef, rig: &str, node: &Option<String>) {
        if let TargetRef::Point(p) = target {
            self.check_finite(p, rig, node, "point");
        }
    }

    fn check_nodes(&mut self, nodes: &'a [Node], rig: &str, prefix: &str) {
        for (index, node) in nodes.iter().enumerate() {
            let path = if prefix.is_empty() {
                index.to_string()
            } else {
                format!("{prefix}/{index}")
            };
            let at = Some(path.clone());
            match node {
                Node::Pivot {
                    target,
                    offset,
                    aim_offset,
                    ..
                } => {
                    self.check_target(target, rig, &at);
                    self.check_finite(offset, rig, &at, "offset");
                    self.check_finite(aim_offset, rig, &at, "aim_offset");
                }
                Node::LookInput { slot, .. } => {
                    if !self.asset.inputs.iter().any(|s| &s.name == slot) {
                        self.error(
                            "camera_unknown_slot",
                            Some(rig),
                            at.clone(),
                            format!("input slot `{slot}` is not declared"),
                        );
                    }
                }
                Node::Boom {
                    length,
                    side_offset,
                    up_offset,
                    length_speed,
                } => {
                    self.check_value_f32(length, rig, &at, "boom length", 0.0);
                    self.check_value_f32(side_offset, rig, &at, "side_offset", f32::MIN);
                    self.check_value_f32(up_offset, rig, &at, "up_offset", f32::MIN);
                    self.check_value_f32(length_speed, rig, &at, "length_speed", 0.0);
                }
                Node::Collision {
                    radius,
                    close_speed,
                    open_speed,
                    open_rate,
                    ..
                } => {
                    self.check_finite(
                        &[*radius, *close_speed, *open_speed, *open_rate],
                        rig,
                        &at,
                        "collision settings",
                    );
                    if *radius < 0.0 || *close_speed < 0.0 || *open_speed < 0.0 || *open_rate < 0.0 {
                        self.error(
                            "camera_bad_value",
                            Some(rig),
                            at.clone(),
                            "collision radius and speeds must not be negative".to_owned(),
                        );
                    }
                }
                Node::Fov { degrees } => self.check_value_f32(degrees, rig, &at, "fov degrees", f32::MIN),
                Node::Offset {
                    translation,
                    rotation_deg,
                    ..
                } => {
                    self.check_finite(translation, rig, &at, "translation");
                    self.check_finite(rotation_deg, rig, &at, "rotation_deg");
                }
                Node::SmoothPosition {
                    depth, side, height, ..
                } => {
                    self.check_finite(&[*depth, *side, *height], rig, &at, "smoothing rates");
                }
                Node::SmoothRotation { pitch, yaw, roll } => {
                    self.check_finite(&[*pitch, *yaw, *roll], rig, &at, "smoothing rates");
                }
                Node::Lean { gain, max, speed } => {
                    self.check_finite(&[*gain, *max, *speed], rig, &at, "lean settings");
                }
                Node::Frame {
                    target,
                    dead_zone_deg,
                    soft_zone_deg,
                    speed,
                } => {
                    self.check_target(target, rig, &at);
                    self.check_finite(
                        &[
                            dead_zone_deg.0,
                            dead_zone_deg.1,
                            soft_zone_deg.0,
                            soft_zone_deg.1,
                            *speed,
                        ],
                        rig,
                        &at,
                        "frame zones",
                    );
                    if soft_zone_deg.0 < dead_zone_deg.0 || soft_zone_deg.1 < dead_zone_deg.1 {
                        self.error(
                            "camera_bad_value",
                            Some(rig),
                            at.clone(),
                            "soft zone must be at least as wide as the dead zone".to_owned(),
                        );
                    }
                }
                Node::Shake {
                    amplitude,
                    frequency,
                    decay,
                } => self.check_finite(&[*amplitude, *frequency, *decay], rig, &at, "shake settings"),
                Node::Fly {
                    slot,
                    speed,
                    sprint_multiplier,
                    accel,
                    rise,
                    sprint,
                } => {
                    if !self.asset.inputs.iter().any(|s| &s.name == slot) {
                        self.error(
                            "camera_unknown_slot",
                            Some(rig),
                            at.clone(),
                            format!("input slot `{slot}` is not declared"),
                        );
                    }
                    self.check_finite(&[*speed, *sprint_multiplier, *accel], rig, &at, "fly settings");
                    self.check_value_f32(rise, rig, &at, "rise", f32::MIN);
                    self.check_value_bool(sprint, rig, &at);
                }
                Node::Rail {
                    spline,
                    drive,
                    orient,
                    offset,
                } => {
                    self.check_spline_name(spline, rig, &at);
                    self.check_finite(offset, rig, &at, "rail offset");
                    match drive {
                        RailDrive::Value(v) => self.check_value_f32(v, rig, &at, "rail position", f32::MIN),
                        RailDrive::Time { speed, start } => {
                            self.check_finite(&[*speed, *start], rig, &at, "rail time drive");
                        }
                        RailDrive::Follow {
                            target,
                            lead,
                            damping,
                        } => {
                            self.check_target(target, rig, &at);
                            self.check_finite(&[*lead, *damping], rig, &at, "rail follow settings");
                            if *damping < 0.0 {
                                self.error(
                                    "camera_bad_value",
                                    Some(rig),
                                    at.clone(),
                                    "rail damping must not be negative".to_owned(),
                                );
                            }
                        }
                    }
                    if let RailOrient::LookAt(target) = orient {
                        self.check_target(target, rig, &at);
                    }
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
                    match base {
                        CraneBase::Ground(p) => self.check_finite(p, rig, &at, "crane base"),
                        CraneBase::Rail { spline, position, .. } => {
                            self.check_spline_name(spline, rig, &at);
                            self.check_value_f32(position, rig, &at, "crane rail position", f32::MIN);
                        }
                    }
                    self.check_value_f32(yaw, rig, &at, "crane yaw", f32::MIN);
                    self.check_value_f32(pitch, rig, &at, "crane pitch", f32::MIN);
                    self.check_value_f32(length, rig, &at, "crane length", 0.0);
                    self.check_value_f32(mount_yaw, rig, &at, "crane mount yaw", f32::MIN);
                    self.check_value_f32(mount_pitch, rig, &at, "crane mount pitch", f32::MIN);
                    if !limits.is_valid() {
                        self.error(
                            "camera_bad_value",
                            Some(rig),
                            at.clone(),
                            "crane limits must be finite with min not above max".to_owned(),
                        );
                    }
                }
                Node::Group { enabled, nodes } => {
                    self.check_value_bool(enabled, rig, &at);
                    self.check_nodes(nodes, rig, &path);
                }
                Node::When { cond, nodes } => {
                    self.check_condition(cond, Some(rig), &at);
                    self.check_nodes(nodes, rig, &path);
                }
            }
        }
    }

    fn check_transition(&mut self, transition: &TransitionDef, rig: Option<&str>) {
        for end in [&transition.from, &transition.to] {
            if let RigMatch::Rig(name) = end {
                self.check_rig_name(name, rig, "a transition");
            }
        }
        let time = match transition.blend {
            Blend::Cut => 0.0,
            Blend::Linear(t) | Blend::Smooth(t) | Blend::EaseIn(t) | Blend::EaseOut(t) => t,
        };
        if !time.is_finite() || time < 0.0 {
            self.error(
                "camera_bad_value",
                rig,
                None,
                format!("blend time {time} must be finite and not negative"),
            );
        }
        for cond in &transition.conditions {
            self.check_condition(cond, rig, &None);
        }
    }

    fn check_director(&mut self, director: &DirectorDef) {
        match director {
            DirectorDef::Fixed(name) => self.check_rig_name(name, None, "the director"),
            DirectorDef::Rules { default, rules } => {
                if !default.is_empty() {
                    self.check_rig_name(default, None, "the director default");
                }
                for rule in rules {
                    self.check_rig_name(&rule.rig, None, "a director rule");
                    self.check_condition(&rule.when, None, &None);
                }
            }
            DirectorDef::Ranked(list) => {
                for ranked in list {
                    self.check_director(&ranked.director);
                }
            }
            DirectorDef::Hook(name) => {
                if !(self.hooks)(name) {
                    self.error(
                        "camera_unknown_hook",
                        None,
                        None,
                        format!("director hook `{name}` is not registered"),
                    );
                }
            }
        }
    }
}

/// Whether a director can produce a rig even when no rule matches.
fn director_has_default(director: &DirectorDef) -> bool {
    match director {
        DirectorDef::Fixed(_) | DirectorDef::Hook(_) => true,
        DirectorDef::Rules { default, .. } => !default.is_empty(),
        DirectorDef::Ranked(list) => list.iter().any(|r| director_has_default(&r.director)),
    }
}

/// The node kinds of a rig in evaluation order (the structure `share_state` compares).
pub fn structure(nodes: &[Node], out: &mut Vec<&'static str>) {
    for node in nodes {
        out.push(node.kind());
        structure(node.children(), out);
    }
}

impl CameraAsset {
    /// Parses a `.camera.ron` text: peeks the version, deserializes (unknown fields are errors
    /// that carry line and column), migrates to the latest version. It does not validate; call
    /// [`validate`](Self::validate).
    pub fn from_ron(text: &str) -> Result<Self, CameraError> {
        let found = version::peek_ron_version(text).map_err(CameraError::Parse)?;
        if found > CAMERA_FORMAT_VERSION {
            return Err(CameraError::Version {
                found,
                expected: CAMERA_FORMAT_VERSION,
            });
        }
        let mut asset: Self = ron::from_str(text).map_err(|e| CameraError::Parse(e.to_string()))?;
        asset.migrate_to_latest()?;
        Ok(asset)
    }

    /// Serializes to RON with a trailing newline. Re-saving an unchanged asset is byte-identical.
    pub fn to_ron(&self) -> Result<String, CameraError> {
        // Fixed "\n": RON's default newline is "\r\n" on Windows, which made saves differ per platform.
        let config = regia_core::ron_pretty().new_line(std::borrow::Cow::Borrowed("\n"));
        let mut text =
            ron::ser::to_string_pretty(self, config).map_err(|e| CameraError::Parse(e.to_string()))?;
        text.push('\n');
        Ok(text)
    }

    /// Rejects a newer version and runs the migration chain on an older one.
    pub fn migrate_to_latest(&mut self) -> Result<u32, CameraError> {
        if self.format_version > CAMERA_FORMAT_VERSION {
            return Err(CameraError::Version {
                found: self.format_version,
                expected: CAMERA_FORMAT_VERSION,
            });
        }
        if self.format_version < CAMERA_FORMAT_VERSION {
            self.format_version = version::migrate(
                self,
                self.format_version,
                camera_migrations(),
                CAMERA_FORMAT_VERSION,
            )
            .map_err(|e| CameraError::Migration(e.to_string()))?;
        }
        Ok(self.format_version)
    }

    /// The index of the rig called `name`.
    pub fn rig_index(&self, name: &str) -> Option<usize> {
        self.rigs.iter().position(|rig| rig.name == name)
    }

    /// The rig called `name`.
    pub fn rig(&self, name: &str) -> Option<&RigDef> {
        self.rigs.iter().find(|rig| rig.name == name)
    }

    /// Whether `name` is a declared variable or a parameter that seeds one.
    pub fn has_var(&self, name: &str) -> bool {
        self.variables.iter().any(|v| v.name == name)
            || matches!(
                self.params.get(name),
                Some(ParamValue::Bool(_) | ParamValue::Float(_) | ParamValue::Int(_) | ParamValue::Vec3(_))
            )
    }

    /// Validates the asset (§ 8). `hooks` says whether a director hook name is registered.
    /// Findings come in a stable order (structure first, then rigs in order).
    pub fn validate(&self, hooks: &dyn Fn(&str) -> bool) -> Vec<CameraIssue> {
        let mut rigs = BTreeSet::new();
        let mut checker_issues = Vec::new();
        let mut seen = BTreeSet::new();
        for rig in &self.rigs {
            if !seen.insert(rig.name.as_str()) {
                checker_issues.push(CameraIssue {
                    code: "camera_duplicate_rig",
                    severity: Severity::Error,
                    rig: Some(rig.name.clone()),
                    node: None,
                    message: format!("rig name `{}` is used twice", rig.name),
                });
            }
            rigs.insert(rig.name.as_str());
        }
        let mut var_names: BTreeSet<&str> = self.variables.iter().map(|v| v.name.as_str()).collect();
        for (name, value) in &self.params {
            if !matches!(value, ParamValue::Name(_)) {
                var_names.insert(name.as_str());
            }
        }
        let mut checker = Checker {
            asset: self,
            hooks,
            issues: checker_issues,
            used_params: BTreeSet::new(),
            rigs,
            var_names,
        };

        for (name, value) in &self.params {
            if !value.is_finite() {
                checker.error(
                    "camera_bad_value",
                    None,
                    None,
                    format!("parameter `{name}` is not finite"),
                );
            }
        }
        for var in &self.variables {
            if !var.default.is_finite() {
                checker.error(
                    "camera_bad_value",
                    None,
                    None,
                    format!("variable `{}` default is not finite", var.name),
                );
            }
        }
        for slot in &self.inputs {
            let values = [
                slot.speed.0,
                slot.speed.1,
                slot.pitch_clamp.0,
                slot.pitch_clamp.1,
                slot.pull_weight,
            ];
            if values.iter().any(|v| !v.is_finite()) || slot.pitch_clamp.0 > slot.pitch_clamp.1 {
                checker.error(
                    "camera_bad_value",
                    None,
                    None,
                    format!(
                        "input slot `{}` has non-finite settings or crossed clamps",
                        slot.name
                    ),
                );
            }
            if let Some(var) = &slot.pull_toward_var
                && !checker.var_names.contains(var.as_str())
            {
                checker.error(
                    "camera_unknown_name",
                    None,
                    None,
                    format!(
                        "input slot `{}` pulls toward undeclared variable `{var}`",
                        slot.name
                    ),
                );
            }
        }

        checker.check_director(&self.director);
        if !director_has_default(&self.director) {
            checker.error(
                "camera_no_default_rig",
                None,
                None,
                "the director has no default rig: nothing runs when no rule matches".to_owned(),
            );
        }
        for transition in &self.transitions {
            checker.check_transition(transition, None);
        }
        for rig in &self.rigs {
            if rig.root.is_empty() {
                checker.warn(
                    "camera_empty_rig",
                    Some(&rig.name),
                    None,
                    format!("rig `{}` has no nodes", rig.name),
                );
            }
            checker.check_nodes(&rig.root, &rig.name, "");
            for transition in &rig.transitions {
                checker.check_transition(transition, Some(&rig.name));
            }
            if let Some(t) = rig.blend_out
                && (!t.is_finite() || t < 0.0)
            {
                checker.error(
                    "camera_bad_value",
                    Some(&rig.name),
                    None,
                    format!("blend_out {t} must be finite and not negative"),
                );
            }
        }

        // share_state needs equal structure: warn per pair-matching transition once.
        for transition in &self.transitions {
            if !transition.share_state {
                continue;
            }
            let from: Vec<&RigDef> = self
                .rigs
                .iter()
                .filter(|r| rig_matches(&transition.from, r))
                .collect();
            let to: Vec<&RigDef> = self
                .rigs
                .iter()
                .filter(|r| rig_matches(&transition.to, r))
                .collect();
            let mut mismatch = false;
            for a in &from {
                for b in &to {
                    if a.name != b.name && !same_structure(a, b) {
                        mismatch = true;
                    }
                }
            }
            if mismatch {
                checker.warn(
                    "camera_share_structure",
                    None,
                    None,
                    "a share_state transition joins rigs with different node structure; it cross-fades poses instead"
                        .to_owned(),
                );
            }
        }

        for name in self.params.keys() {
            if !checker.used_params.contains(name) && !checker_uses_var(self, name) {
                checker.warn(
                    "camera_unused_param",
                    None,
                    None,
                    format!("parameter `{name}` is never used"),
                );
            }
        }
        checker.issues
    }
}

fn rig_matches(end: &RigMatch, rig: &RigDef) -> bool {
    match end {
        RigMatch::Any => true,
        RigMatch::Rig(name) => &rig.name == name,
        RigMatch::Tag(tag) => rig.tags.iter().any(|t| t == tag),
    }
}

/// Whether two rigs have the same node kinds in the same order.
pub fn same_structure(a: &RigDef, b: &RigDef) -> bool {
    let (mut left, mut right) = (Vec::new(), Vec::new());
    structure(&a.root, &mut left);
    structure(&b.root, &mut right);
    left == right
}

/// Whether a parameter is used as a variable seed (a `Var` or a condition with its name).
fn checker_uses_var(asset: &CameraAsset, name: &str) -> bool {
    fn cond_uses(cond: &Condition, name: &str) -> bool {
        match cond {
            Condition::VarBool(n, _) | Condition::VarCompare(n, _, _) => n == name,
            Condition::All(list) | Condition::Any(list) => list.iter().any(|c| cond_uses(c, name)),
            Condition::Not(inner) => cond_uses(inner, name),
            _ => false,
        }
    }
    fn value_uses<T>(value: &Value<T>, name: &str) -> bool {
        matches!(value, Value::Var(n) if n == name)
    }
    fn nodes_use(nodes: &[Node], name: &str) -> bool {
        nodes.iter().any(|node| {
            let own = match node {
                Node::Boom {
                    length,
                    side_offset,
                    up_offset,
                    length_speed,
                } => [length, side_offset, up_offset, length_speed]
                    .iter()
                    .any(|v| value_uses(v, name)),
                Node::Fov { degrees } => value_uses(degrees, name),
                Node::Fly { rise, sprint, .. } => value_uses(rise, name) || value_uses(sprint, name),
                Node::Rail { drive, .. } => matches!(drive, RailDrive::Value(v) if value_uses(v, name)),
                Node::Crane {
                    base,
                    yaw,
                    pitch,
                    length,
                    mount_yaw,
                    mount_pitch,
                    ..
                } => {
                    [yaw, pitch, length, mount_yaw, mount_pitch]
                        .iter()
                        .any(|v| value_uses(v, name))
                        || matches!(base, CraneBase::Rail { position, .. } if value_uses(position, name))
                }
                Node::Group { enabled, .. } => value_uses(enabled, name),
                Node::When { cond, .. } => cond_uses(cond, name),
                _ => false,
            };
            own || nodes_use(node.children(), name)
        })
    }
    fn director_uses(director: &DirectorDef, name: &str) -> bool {
        match director {
            DirectorDef::Rules { rules, .. } => rules.iter().any(|r| cond_uses(&r.when, name)),
            DirectorDef::Ranked(list) => list.iter().any(|r| director_uses(&r.director, name)),
            _ => false,
        }
    }
    let transition_uses = |t: &TransitionDef| t.conditions.iter().any(|c| cond_uses(c, name));
    director_uses(&asset.director, name)
        || asset.transitions.iter().any(transition_uses)
        || asset
            .rigs
            .iter()
            .any(|r| nodes_use(&r.root, name) || r.transitions.iter().any(transition_uses))
        || asset
            .inputs
            .iter()
            .any(|s| s.pull_toward_var.as_deref() == Some(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"(
    format_version: 1,
    name: "third_person",
    params: { "boom_length": Float(4.5), "shoulder": Float(0.4), "aiming": Bool(false) },
    inputs: [ (name: "look", kind: Axis2, speed: (120.0, 90.0), pitch_clamp: (-80.0, 30.0), invert_pitch: false) ],
    rigs: [
        (name: "explore", tags: ["ground"], root: [
            Pivot(target: DriveTarget, offset: (0.0, 1.5, 0.0)),
            LookInput(slot: "look"),
            Boom(length: Param("boom_length"), side_offset: Param("shoulder")),
            Collision(radius: 0.25, layers: ["static"], close_speed: 20.0, open_speed: 4.0),
            Fov(degrees: Literal(70.0)),
            SmoothPosition(depth: 8.0, side: 8.0, height: 6.0),
        ], transitions: []),
        (name: "aim", tags: ["ground", "aim"], root: [ Fov(degrees: Literal(50.0)) ], transitions: []),
    ],
    transitions: [
        (from: Any, to: Tag("aim"), blend: Smooth(0.25), start_facing: OwnSeed),
        (from: Any, to: Any, blend: Smooth(0.5), start_facing: OwnSeed),
    ],
    director: Rules(default: "explore", rules: [ (when: VarBool("aiming", true), rig: "aim") ]),
)"#;

    fn no_hooks(_: &str) -> bool {
        false
    }

    fn codes(asset: &CameraAsset) -> Vec<&'static str> {
        asset.validate(&no_hooks).iter().map(|i| i.code).collect()
    }

    fn minimal() -> CameraAsset {
        CameraAsset::from_ron(
            r#"(format_version: 1, name: "m", rigs: [(name: "a", root: [Fov(degrees: Literal(60.0))])], director: Fixed("a"))"#,
        )
        .expect("minimal parses")
    }

    #[test]
    fn example_round_trips_and_validates() {
        let asset = CameraAsset::from_ron(EXAMPLE).expect("example parses");
        assert_eq!(asset.rigs.len(), 2);
        assert_eq!(asset.rigs[0].root.len(), 6);
        let issues = asset.validate(&no_hooks);
        assert!(issues.is_empty(), "example is clean: {issues:?}");
        let text = asset.to_ron().expect("serializes");
        let again = CameraAsset::from_ron(&text).expect("re-parses");
        assert_eq!(asset, again);
        assert_eq!(text, again.to_ron().expect("serializes again"));
    }

    #[test]
    fn saved_text_uses_unix_newlines_on_every_platform() {
        let text = minimal().to_ron().expect("serializes");
        assert!(text.contains('\n') && !text.contains('\r'), "{text:?}");
    }

    #[test]
    fn unknown_field_reports_line_and_column() {
        let text =
            "(\n format_version: 1,\n name: \"x\",\n bogus: 3,\n rigs: [],\n director: Fixed(\"a\"),\n)";
        let Err(CameraError::Parse(message)) = CameraAsset::from_ron(text) else {
            panic!("an unknown field must fail");
        };
        assert!(message.contains("bogus"), "{message}");
        assert!(message.contains(':'), "line and column present: {message}");
    }

    #[test]
    fn newer_version_is_refused() {
        let text = r#"(format_version: 3, name: "x", rigs: [], director: Fixed("a"))"#;
        assert!(matches!(
            CameraAsset::from_ron(text),
            Err(CameraError::Version {
                found: 3,
                expected: 2
            })
        ));
    }

    #[test]
    fn migrate_camera_rewrites_v1_to_v2() {
        // A genuine v1 file (only pre-v2 node kinds) loads, migrates and re-saves as v2.
        let migrated = migrate_camera(EXAMPLE).expect("v1 migrates");
        assert!(migrated.contains("format_version: 2"), "{migrated}");
        let asset = CameraAsset::from_ron(&migrated).expect("migrated text parses");
        assert_eq!(asset.format_version, CAMERA_FORMAT_VERSION);
        assert_eq!(asset.rigs.len(), 2);
    }

    #[test]
    fn migrate_camera_leaves_current_text_unchanged() {
        let text = minimal().to_ron().expect("serializes");
        assert_eq!(migrate_camera(&text).expect("current passes through"), text);
    }

    #[test]
    fn migrate_camera_refuses_unknown_fields_and_newer_versions() {
        let bogus =
            "(\n format_version: 1,\n name: \"x\",\n bogus: 3,\n rigs: [],\n director: Fixed(\"a\"),\n)";
        assert!(matches!(migrate_camera(bogus), Err(CameraError::Parse(_))));
        let newer = r#"(format_version: 3, name: "x", rigs: [], director: Fixed("a"))"#;
        assert!(matches!(
            migrate_camera(newer),
            Err(CameraError::Version { found: 3, .. })
        ));
    }

    #[test]
    fn unknown_rig_in_director_and_transition() {
        let mut asset = minimal();
        asset.director = DirectorDef::Fixed("nope".into());
        asset.transitions.push(TransitionDef {
            from: RigMatch::Rig("ghost".into()),
            to: RigMatch::Any,
            blend: Blend::Cut,
            start_facing: StartFacing::OwnSeed,
            hold_outgoing: false,
            share_state: false,
            interruptible: true,
            conditions: vec![],
        });
        let found = codes(&asset);
        assert_eq!(
            found.iter().filter(|c| **c == "camera_unknown_rig").count(),
            2,
            "{found:?}"
        );
    }

    #[test]
    fn duplicate_rig_and_empty_rig() {
        let mut asset = minimal();
        let copy = asset.rigs[0].clone();
        asset.rigs.push(copy);
        asset.rigs.push(RigDef {
            name: "empty".into(),
            tags: vec![],
            root: vec![],
            transitions: vec![],
            blend_out: None,
        });
        let issues = asset.validate(&no_hooks);
        assert!(
            issues
                .iter()
                .any(|i| i.code == "camera_duplicate_rig" && i.is_error())
        );
        assert!(
            issues
                .iter()
                .any(|i| i.code == "camera_empty_rig" && !i.is_error())
        );
    }

    #[test]
    fn unknown_name_slot_and_hook() {
        let mut asset = minimal();
        asset.rigs[0].root = vec![
            Node::Boom {
                length: Value::Param("missing".into()),
                side_offset: lit_zero(),
                up_offset: lit_zero(),
                length_speed: lit_zero(),
            },
            Node::LookInput {
                slot: "nope".into(),
                additive: false,
            },
            Node::When {
                cond: Condition::VarBool("ghost".into(), true),
                nodes: vec![],
            },
        ];
        asset.director = DirectorDef::Hook("h".into());
        let found = codes(&asset);
        assert!(found.contains(&"camera_unknown_name"));
        assert!(found.contains(&"camera_unknown_slot"));
        assert!(found.contains(&"camera_unknown_hook"));
        assert!(
            asset
                .validate(&|name| name == "h")
                .iter()
                .all(|i| i.code != "camera_unknown_hook")
        );
    }

    #[test]
    fn bad_value_no_default_and_share_structure() {
        let mut asset = minimal();
        asset.rigs[0].root = vec![Node::Fov {
            degrees: Value::Literal(f32::NAN),
        }];
        asset.rigs.push(RigDef {
            name: "b".into(),
            tags: vec![],
            root: vec![],
            transitions: vec![],
            blend_out: None,
        });
        asset.director = DirectorDef::Rules {
            default: String::new(),
            rules: vec![],
        };
        asset.transitions.push(TransitionDef {
            from: RigMatch::Any,
            to: RigMatch::Any,
            blend: Blend::Smooth(0.3),
            start_facing: StartFacing::OwnSeed,
            hold_outgoing: false,
            share_state: true,
            interruptible: true,
            conditions: vec![],
        });
        let found = codes(&asset);
        for code in [
            "camera_bad_value",
            "camera_no_default_rig",
            "camera_share_structure",
        ] {
            assert!(found.contains(&code), "{code} in {found:?}");
        }
    }

    #[test]
    fn unused_param_is_a_warning() {
        let mut asset = minimal();
        asset.params.insert("spare".into(), ParamValue::Float(1.0));
        let issues = asset.validate(&no_hooks);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].code, "camera_unused_param");
        assert_eq!(CameraBuildStatus::of(&issues), CameraBuildStatus::Warnings);
    }

    #[test]
    fn blend_curves_hit_endpoints_and_are_monotonic() {
        for blend in [
            Blend::Linear(0.5),
            Blend::Smooth(0.5),
            Blend::EaseIn(0.5),
            Blend::EaseOut(0.5),
        ] {
            assert_eq!(blend.weight(0.0), 0.0);
            assert_eq!(blend.weight(0.5), 1.0);
            assert_eq!(blend.weight(9.0), 1.0);
            let mut last = 0.0;
            for step in 0..=100 {
                let w = blend.weight(step as f32 * 0.005);
                assert!(w >= last, "monotonic at step {step}");
                last = w;
            }
        }
        assert_eq!(Blend::Cut.weight(0.0), 1.0);
        assert_eq!(Blend::Smooth(0.0).weight(0.0), 1.0);
    }
}
