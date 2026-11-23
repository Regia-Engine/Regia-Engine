// SPDX-License-Identifier: MPL-2.0
//! The anim graph editor's editable model: nodes with ids, canvas positions and explicit
//! input slots, compiled to (and decompiled from) the frozen `.animgraph.ron` tree.
//!
//! The asset is a tree ([`AnimNode`] owns its children); an editor needs node identity,
//! positions and half-built graphs. [`GraphModel`] keeps a flat node map where every node
//! lives in a *scope* — the root canvas or one state of a state machine — and parents point at
//! children through input slots named like the runtime's debug paths (`a`, `b`, `base`,
//! `additive`, `layer<i>`, `inner`, see [`regia_animation::debug`]). [`GraphModel::compile`]
//! walks from each scope's output and builds the asset tree; nodes nothing reaches are
//! reported, not saved.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use regia_animation::blendspace::{BlendSpace1D, BlendSpace2D};
use regia_animation::chain_ik::ChainSolver;
use regia_animation::clip::RootMotionMode;
use regia_animation::debug::{NodePath, ROOT_PATH, child_path};
use regia_animation::graph::{
    AdditiveNode, AnimGraph, AnimNode, BlendNode, BlendSpace1DNode, BlendSpace2DNode, BodyAimNode,
    ChainIKNode, ChainLimits, ClipNode, IkTarget, JointMaskDef, LayerEntry, LayerMode, LayerNode, LookAtNode,
    OrientationWarpNode, RootMotionNode, SlotNode, StrideWarpNode, TwoBoneIKNode,
};
use regia_animation::motion_matching::warp::StrideWarpSettings;
use regia_animation::params::{ParamDecl, ParamValue};
use regia_animation::state_machine::{StateMachineDef, TransitionDef};
use regia_core::math::Vec3;

/// A model node id (stable for the editing session; not saved in the asset).
pub type NodeId = u32;

/// Which canvas a node lives on.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Scope {
    /// The graph's top-level canvas.
    Root,
    /// One state of a state machine node.
    State {
        /// The state machine node.
        machine: NodeId,
        /// State name.
        state: String,
    },
}

/// One layer's settings (its sub-graph is the matching `layer<i>` input).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayerMeta {
    /// Joints the layer affects.
    pub mask: JointMaskDef,
    /// Override or additive.
    pub mode: LayerMode,
    /// Float parameter for the layer weight.
    pub weight_param: String,
    /// Seconds over which weight changes are inertialized (0 = off).
    #[serde(default)]
    pub weight_inertialize: f32,
}

/// One state of a state machine: its canvas position and output node.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StateModel {
    /// State name.
    pub name: String,
    /// Position on the machine's state canvas.
    pub pos: [f32; 2],
    /// The node whose pose the state outputs.
    pub output: Option<NodeId>,
}

/// A state machine's states and transitions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MachineModel {
    /// Initial state name.
    pub initial: String,
    /// States in declaration order.
    pub states: Vec<StateModel>,
    /// Transitions in priority order.
    pub transitions: Vec<TransitionDef>,
}

/// A two-bone IK node's settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TwoBoneIkModel {
    /// `[root, mid, tip]` joint names.
    pub chain: [String; 3],
    /// Target.
    pub target: IkTarget,
    /// Optional pole vec3 parameter.
    pub pole: Option<String>,
    /// Optional weight parameter.
    pub weight_param: Option<String>,
}

/// A look-at node's settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LookAtModel {
    /// Joint to aim.
    pub joint: String,
    /// Target.
    pub target: IkTarget,
    /// Yaw limit, radians.
    pub max_yaw: f32,
    /// Pitch limit, radians.
    pub max_pitch: f32,
    /// Optional weight parameter.
    pub weight_param: Option<String>,
}

/// A chain IK node's settings (`.animgraph.ron` v4; the `ChainIK` node without its input).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChainIkModel {
    /// First joint of the chain.
    pub root: String,
    /// Last joint of the chain (a descendant of `root`).
    pub tip: String,
    /// FABRIK or CCD.
    #[serde(default)]
    pub solver: ChainSolver,
    /// Target.
    pub target: IkTarget,
    /// Optional pole vec3 parameter.
    #[serde(default)]
    pub pole: Option<String>,
    /// Joint limits (uniform or per joint by name); `None` = free.
    #[serde(default)]
    pub limits: Option<ChainLimits>,
    /// Iteration cap.
    pub iterations: u32,
    /// Stop distance, meters.
    pub tolerance: f32,
    /// Optional weight parameter.
    #[serde(default)]
    pub weight_param: Option<String>,
}

/// A body aim node's settings (`.animgraph.ron` v4; the `BodyAim` node without its input).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BodyAimModel {
    /// Turning joints, bottom first, with their shares.
    pub joints: Vec<(String, f32)>,
    /// The joint whose axis points at the target.
    pub aim_joint: String,
    /// The aim axis in the aim joint's frame.
    pub axis: Vec3,
    /// Target.
    pub target: IkTarget,
    /// Joint limits; `None` = free.
    #[serde(default)]
    pub limits: Option<ChainLimits>,
    /// Solve passes.
    pub passes: u32,
    /// Optional weight parameter.
    #[serde(default)]
    pub weight_param: Option<String>,
}

fn default_stride_length() -> f32 {
    0.5
}

/// A stride warp node's settings (`.animgraph.ron` v4, ADR-8540; the `StrideWarp` node without
/// its input).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StrideWarpModel {
    /// Float parameter: the character's ground speed, m/s.
    pub ground_speed_param: String,
    /// Strength, speed threshold, ratio range, smoothing and mode (`Rate` or `Pose`).
    #[serde(default = "StrideWarpSettings::graph_default")]
    pub settings: StrideWarpSettings,
    /// `Pose` mode: `[hip, knee, ankle]` per leg.
    #[serde(default)]
    pub legs: Vec<[String; 3]>,
    /// `Pose` mode: the pelvis joint.
    #[serde(default)]
    pub pelvis: String,
    /// `Pose` mode: the largest scaled foot offset, meters.
    #[serde(default = "default_stride_length")]
    pub stride_length: f32,
}

fn one() -> f32 {
    1.0
}

fn quarter_turn() -> f32 {
    std::f32::consts::FRAC_PI_2
}

/// An orientation warp node's settings (`.animgraph.ron` v4, ADR-8540; the `OrientationWarp`
/// node without its input).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OrientationWarpModel {
    /// Float parameter: the desired movement heading, radians (0 = forward, counter-clockwise).
    pub heading_param: String,
    /// The joint that turns the lower body.
    pub pelvis: String,
    /// Spine joints, low to high, with their share of the counter-turn (summing to 1).
    #[serde(default)]
    pub spine: Vec<(String, f32)>,
    /// How much of the turn is applied, `[0, 1]`.
    #[serde(default = "one")]
    pub strength: f32,
    /// Largest turn, radians.
    #[serde(default = "quarter_turn")]
    pub max_delta: f32,
}

/// What a node is, without its children (those are [`ModelNode::inputs`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum NodeKind {
    /// Plays a clip.
    Clip(ClipNode),
    /// 1D blend space.
    BlendSpace1D(BlendSpace1DNode),
    /// 2D blend space.
    BlendSpace2D(BlendSpace2DNode),
    /// State machine (its states hold sub-canvases).
    StateMachine(MachineModel),
    /// Cross-fade `a → b` by a float parameter.
    Blend {
        /// Alpha parameter.
        alpha_param: String,
    },
    /// Additive `additive` over `base`.
    Additive {
        /// Weight parameter.
        weight_param: String,
    },
    /// Masked layers over `base`.
    Layer {
        /// Layer settings, one per `layer<i>` input.
        layers: Vec<LayerMeta>,
    },
    /// Two-bone IK over `inner`.
    TwoBoneIK(TwoBoneIkModel),
    /// Look-at over `inner`.
    LookAt(LookAtModel),
    /// Root-motion mode override over `inner`.
    RootMotion {
        /// Extraction mode.
        mode: RootMotionMode,
    },
    /// Montage slot over `source` (format v3, spec 34 M9).
    Slot {
        /// Slot name.
        name: String,
        /// Evaluate the source even while a full-body montage covers it.
        eval_idle_source: bool,
    },
    /// FABRIK or CCD chain IK over `inner` (format v4).
    ChainIK(ChainIkModel),
    /// Full-body aim over `inner` (format v4).
    BodyAim(BodyAimModel),
    /// Stride warp over `inner` (format v4, ADR-8540).
    StrideWarp(StrideWarpModel),
    /// Orientation warp over `inner` (format v4, ADR-8540).
    OrientationWarp(OrientationWarpModel),
}

impl NodeKind {
    /// The kind's short name (agent protocol and node titles).
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Clip(_) => "clip",
            Self::BlendSpace1D(_) => "blend_space_1d",
            Self::BlendSpace2D(_) => "blend_space_2d",
            Self::StateMachine(_) => "state_machine",
            Self::Blend { .. } => "blend",
            Self::Additive { .. } => "additive",
            Self::Layer { .. } => "layer",
            Self::TwoBoneIK(_) => "two_bone_ik",
            Self::LookAt(_) => "look_at",
            Self::RootMotion { .. } => "root_motion",
            Self::Slot { .. } => "slot",
            Self::ChainIK(_) => "chain_ik",
            Self::BodyAim(_) => "body_aim",
            Self::StrideWarp(_) => "stride_warp",
            Self::OrientationWarp(_) => "orientation_warp",
        }
    }

    /// Input slot names (equal to the runtime debug path segments).
    #[must_use]
    pub fn input_names(&self) -> Vec<String> {
        match self {
            Self::Clip(_) | Self::BlendSpace1D(_) | Self::BlendSpace2D(_) | Self::StateMachine(_) => {
                Vec::new()
            }
            Self::Blend { .. } => vec!["a".into(), "b".into()],
            Self::Additive { .. } => vec!["base".into(), "additive".into()],
            Self::Layer { layers } => std::iter::once("base".to_string())
                .chain((0..layers.len()).map(|i| format!("layer{i}")))
                .collect(),
            Self::TwoBoneIK(_)
            | Self::LookAt(_)
            | Self::RootMotion { .. }
            | Self::ChainIK(_)
            | Self::BodyAim(_)
            | Self::StrideWarp(_)
            | Self::OrientationWarp(_) => vec!["inner".into()],
            Self::Slot { .. } => vec!["source".into()],
        }
    }

    /// A default node of kind `label` (see [`NodeKind::label`]); `None` for unknown labels.
    #[must_use]
    pub fn default_of(label: &str) -> Option<Self> {
        Some(match label {
            "clip" => Self::Clip(ClipNode {
                clip: String::new(),
                looping: true,
                speed: 1.0,
                sync_group: None,
            }),
            "blend_space_1d" => Self::BlendSpace1D(BlendSpace1DNode {
                space: BlendSpace1D {
                    param: String::new(),
                    samples: Vec::new(),
                },
                sync_group: None,
            }),
            "blend_space_2d" => Self::BlendSpace2D(BlendSpace2DNode {
                space: BlendSpace2D {
                    param_x: String::new(),
                    param_y: String::new(),
                    samples: Vec::new(),
                    triangles: Vec::new(),
                },
                sync_group: None,
            }),
            "state_machine" => Self::StateMachine(MachineModel {
                initial: String::new(),
                states: Vec::new(),
                transitions: Vec::new(),
            }),
            "blend" => Self::Blend {
                alpha_param: String::new(),
            },
            "additive" => Self::Additive {
                weight_param: String::new(),
            },
            "layer" => Self::Layer { layers: Vec::new() },
            "two_bone_ik" => Self::TwoBoneIK(TwoBoneIkModel {
                chain: Default::default(),
                target: IkTarget::Param(String::new()),
                pole: None,
                weight_param: None,
            }),
            "look_at" => Self::LookAt(LookAtModel {
                joint: String::new(),
                target: IkTarget::Param(String::new()),
                max_yaw: 1.2,
                max_pitch: 0.8,
                weight_param: None,
            }),
            "root_motion" => Self::RootMotion {
                mode: RootMotionMode::XZ,
            },
            "slot" => Self::Slot {
                name: "default".into(),
                eval_idle_source: true,
            },
            "chain_ik" => Self::ChainIK(ChainIkModel {
                root: String::new(),
                tip: String::new(),
                solver: ChainSolver::Fabrik,
                target: IkTarget::Param(String::new()),
                pole: None,
                limits: None,
                iterations: 24,
                tolerance: 1e-3,
                weight_param: None,
            }),
            "body_aim" => Self::BodyAim(BodyAimModel {
                joints: Vec::new(),
                aim_joint: String::new(),
                axis: Vec3::NEG_Z,
                target: IkTarget::Param(String::new()),
                limits: None,
                passes: 8,
                weight_param: None,
            }),
            "stride_warp" => Self::StrideWarp(StrideWarpModel {
                ground_speed_param: String::new(),
                settings: StrideWarpSettings::graph_default(),
                legs: Vec::new(),
                pelvis: String::new(),
                stride_length: default_stride_length(),
            }),
            "orientation_warp" => Self::OrientationWarp(OrientationWarpModel {
                heading_param: String::new(),
                pelvis: String::new(),
                spine: Vec::new(),
                strength: 1.0,
                max_delta: quarter_turn(),
            }),
            _ => return None,
        })
    }

    /// Every kind label, in palette order.
    pub const LABELS: [&'static str; 15] = [
        "clip",
        "blend_space_1d",
        "blend_space_2d",
        "state_machine",
        "blend",
        "additive",
        "layer",
        "two_bone_ik",
        "look_at",
        "root_motion",
        "slot",
        "chain_ik",
        "body_aim",
        "stride_warp",
        "orientation_warp",
    ];
}

/// One node of the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelNode {
    /// What the node is.
    pub kind: NodeKind,
    /// The canvas it lives on.
    pub scope: Scope,
    /// Canvas position (top-left), points.
    pub pos: [f32; 2],
    /// Children, parallel to [`NodeKind::input_names`].
    pub inputs: Vec<Option<NodeId>>,
}

/// The editable graph.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GraphModel {
    /// Parameter declarations.
    pub parameters: Vec<ParamDecl>,
    /// Nodes by id.
    pub nodes: BTreeMap<NodeId, ModelNode>,
    /// The root canvas's output node.
    pub output: Option<NodeId>,
    /// Next id to hand out.
    pub next_id: NodeId,
}

impl Default for GraphModel {
    fn default() -> Self {
        Self {
            parameters: Vec::new(),
            nodes: BTreeMap::new(),
            output: None,
            next_id: 1,
        }
    }
}

/// The result of compiling a model.
#[derive(Clone, Debug)]
pub struct Compiled {
    /// The asset graph (not yet validated against parameter declarations).
    pub graph: AnimGraph,
    /// Each reachable node's runtime path.
    pub paths: BTreeMap<NodeId, NodePath>,
    /// Nodes no output reaches (they are not saved).
    pub unreachable: Vec<NodeId>,
}

impl GraphModel {
    /// The node `id`.
    ///
    /// # Errors
    ///
    /// The node does not exist.
    pub fn node(&self, id: NodeId) -> Result<&ModelNode, String> {
        self.nodes
            .get(&id)
            .ok_or_else(|| format!("node {id} does not exist"))
    }

    fn node_mut(&mut self, id: NodeId) -> Result<&mut ModelNode, String> {
        self.nodes
            .get_mut(&id)
            .ok_or_else(|| format!("node {id} does not exist"))
    }

    /// The state machine model of node `id`.
    ///
    /// # Errors
    ///
    /// Not a state machine.
    pub fn machine(&self, id: NodeId) -> Result<&MachineModel, String> {
        match &self.node(id)?.kind {
            NodeKind::StateMachine(m) => Ok(m),
            other => Err(format!("node {id} is a {}, not a state machine", other.label())),
        }
    }

    /// Mutable state machine model of node `id`.
    ///
    /// # Errors
    ///
    /// Not a state machine.
    pub fn machine_mut(&mut self, id: NodeId) -> Result<&mut MachineModel, String> {
        match &mut self.node_mut(id)?.kind {
            NodeKind::StateMachine(m) => Ok(m),
            other => Err(format!("node {id} is a {}, not a state machine", other.label())),
        }
    }

    fn check_scope(&self, scope: &Scope) -> Result<(), String> {
        match scope {
            Scope::Root => Ok(()),
            Scope::State { machine, state } => {
                if self.machine(*machine)?.states.iter().any(|s| &s.name == state) {
                    Ok(())
                } else {
                    Err(format!("state machine {machine} has no state {state:?}"))
                }
            }
        }
    }

    /// Adds a node; returns its id.
    ///
    /// # Errors
    ///
    /// The scope does not exist.
    pub fn add_node(&mut self, kind: NodeKind, scope: Scope, pos: [f32; 2]) -> Result<NodeId, String> {
        self.check_scope(&scope)?;
        let id = self.next_id;
        self.next_id += 1;
        let inputs = vec![None; kind.input_names().len()];
        self.nodes.insert(
            id,
            ModelNode {
                kind,
                scope,
                pos,
                inputs,
            },
        );
        Ok(id)
    }

    /// Removes a node (and, for a state machine, every node inside its states); clears every
    /// reference to the removed nodes.
    ///
    /// # Errors
    ///
    /// The node does not exist.
    pub fn remove_node(&mut self, id: NodeId) -> Result<(), String> {
        self.node(id)?;
        let mut doomed = vec![id];
        let mut i = 0;
        while i < doomed.len() {
            let parent = doomed[i];
            for (child, node) in &self.nodes {
                if matches!(&node.scope, Scope::State { machine, .. } if *machine == parent)
                    && !doomed.contains(child)
                {
                    doomed.push(*child);
                }
            }
            i += 1;
        }
        for d in &doomed {
            self.nodes.remove(d);
        }
        self.clear_references(&doomed);
        Ok(())
    }

    fn clear_references(&mut self, ids: &[NodeId]) {
        if self.output.is_some_and(|o| ids.contains(&o)) {
            self.output = None;
        }
        for node in self.nodes.values_mut() {
            for input in &mut node.inputs {
                if input.is_some_and(|c| ids.contains(&c)) {
                    *input = None;
                }
            }
            if let NodeKind::StateMachine(m) = &mut node.kind {
                for state in &mut m.states {
                    if state.output.is_some_and(|o| ids.contains(&o)) {
                        state.output = None;
                    }
                }
            }
        }
    }

    /// Detaches `id` from whatever consumes it (a parent input or a scope output).
    fn detach(&mut self, id: NodeId) {
        self.clear_references(&[id]);
    }

    /// Whether `ancestor` is reachable from `id` through inputs (cycle check).
    fn reaches(&self, from: NodeId, target: NodeId) -> bool {
        let mut stack = vec![from];
        let mut seen = Vec::new();
        while let Some(n) = stack.pop() {
            if n == target {
                return true;
            }
            if seen.contains(&n) {
                continue;
            }
            seen.push(n);
            if let Some(node) = self.nodes.get(&n) {
                stack.extend(node.inputs.iter().flatten().copied());
            }
        }
        false
    }

    /// Connects `child`'s output to `parent`'s input `slot` (an index or a name like `"b"`),
    /// replacing what was there and detaching `child` from its previous consumer.
    ///
    /// # Errors
    ///
    /// Missing nodes, different scopes, unknown slot, or a cycle.
    pub fn connect(&mut self, child: NodeId, parent: NodeId, slot: &str) -> Result<(), String> {
        let child_scope = self.node(child)?.scope.clone();
        let parent_node = self.node(parent)?;
        if parent_node.scope != child_scope {
            return Err(format!("nodes {child} and {parent} are on different canvases"));
        }
        let names = parent_node.kind.input_names();
        let index = slot
            .parse::<usize>()
            .ok()
            .filter(|i| *i < names.len())
            .or_else(|| names.iter().position(|n| n == slot))
            .ok_or_else(|| {
                format!(
                    "node {parent} ({}) has no input {slot:?}; inputs: {names:?}",
                    parent_node.kind.label()
                )
            })?;
        if child == parent || self.reaches(child, parent) {
            return Err(format!("connecting {child} into {parent} would make a cycle"));
        }
        self.detach(child);
        self.node_mut(parent)?.inputs[index] = Some(child);
        Ok(())
    }

    /// Clears `parent`'s input `slot`.
    ///
    /// # Errors
    ///
    /// Missing node or slot.
    pub fn disconnect(&mut self, parent: NodeId, slot: &str) -> Result<(), String> {
        let node = self.node_mut(parent)?;
        let names = node.kind.input_names();
        let index = slot
            .parse::<usize>()
            .ok()
            .filter(|i| *i < names.len())
            .or_else(|| names.iter().position(|n| n == slot))
            .ok_or_else(|| format!("node {parent} has no input {slot:?}"))?;
        node.inputs[index] = None;
        Ok(())
    }

    /// The output node of `scope`.
    #[must_use]
    pub fn scope_output(&self, scope: &Scope) -> Option<NodeId> {
        match scope {
            Scope::Root => self.output,
            Scope::State { machine, state } => self
                .machine(*machine)
                .ok()
                .and_then(|m| m.states.iter().find(|s| &s.name == state))
                .and_then(|s| s.output),
        }
    }

    /// Makes `node` (which must live on `scope`) the scope's output; `None` clears it.
    ///
    /// # Errors
    ///
    /// Missing node/scope, or the node lives elsewhere.
    pub fn set_output(&mut self, scope: &Scope, node: Option<NodeId>) -> Result<(), String> {
        self.check_scope(scope)?;
        if let Some(id) = node {
            if &self.node(id)?.scope != scope {
                return Err(format!("node {id} is not on that canvas"));
            }
            self.detach(id);
        }
        match scope {
            Scope::Root => self.output = node,
            Scope::State { machine, state } => {
                let machine = self.machine_mut(*machine)?;
                if let Some(s) = machine.states.iter_mut().find(|s| &s.name == state) {
                    s.output = node;
                }
            }
        }
        Ok(())
    }

    /// Moves a node on its canvas.
    ///
    /// # Errors
    ///
    /// Missing node.
    pub fn move_node(&mut self, id: NodeId, pos: [f32; 2]) -> Result<(), String> {
        self.node_mut(id)?.pos = pos;
        Ok(())
    }

    /// Replaces a node's kind data, keeping inputs (resized when the slot count changes, e.g. a
    /// layer added).
    ///
    /// # Errors
    ///
    /// Missing node, or changing a state machine into something else while it has states.
    pub fn set_kind(&mut self, id: NodeId, kind: NodeKind) -> Result<(), String> {
        let node = self.node_mut(id)?;
        if matches!(node.kind, NodeKind::StateMachine(_)) != matches!(kind, NodeKind::StateMachine(_)) {
            return Err("a node cannot change to or from a state machine; add a new node".into());
        }
        let slots = kind.input_names().len();
        node.inputs.resize(slots, None);
        node.kind = kind;
        Ok(())
    }

    /// Adds a state to a machine (the first becomes initial).
    ///
    /// # Errors
    ///
    /// Not a machine, or the name exists.
    pub fn add_state(&mut self, machine: NodeId, name: &str, pos: [f32; 2]) -> Result<(), String> {
        let m = self.machine_mut(machine)?;
        if name.is_empty() || m.states.iter().any(|s| s.name == name) {
            return Err(format!("state name {name:?} is empty or taken"));
        }
        m.states.push(StateModel {
            name: name.to_string(),
            pos,
            output: None,
        });
        if m.initial.is_empty() {
            m.initial = name.to_string();
        }
        Ok(())
    }

    /// Renames a state, updating transitions, the initial state and its nodes' scopes.
    ///
    /// # Errors
    ///
    /// Missing state or the new name is taken.
    pub fn rename_state(&mut self, machine: NodeId, from: &str, to: &str) -> Result<(), String> {
        let m = self.machine_mut(machine)?;
        if to.is_empty() || m.states.iter().any(|s| s.name == to) {
            return Err(format!("state name {to:?} is empty or taken"));
        }
        let state = m
            .states
            .iter_mut()
            .find(|s| s.name == from)
            .ok_or_else(|| format!("no state {from:?}"))?;
        state.name = to.to_string();
        if m.initial == from {
            m.initial = to.to_string();
        }
        for t in &mut m.transitions {
            if t.from.as_deref() == Some(from) {
                t.from = Some(to.to_string());
            }
            if t.to == from {
                t.to = to.to_string();
            }
        }
        for node in self.nodes.values_mut() {
            if let Scope::State { machine: mm, state } = &mut node.scope
                && *mm == machine
                && state == from
            {
                *state = to.to_string();
            }
        }
        Ok(())
    }

    /// Removes a state, its nodes and the transitions touching it.
    ///
    /// # Errors
    ///
    /// Missing state.
    pub fn remove_state(&mut self, machine: NodeId, name: &str) -> Result<(), String> {
        let m = self.machine_mut(machine)?;
        let index = m
            .states
            .iter()
            .position(|s| s.name == name)
            .ok_or_else(|| format!("no state {name:?}"))?;
        m.states.remove(index);
        m.transitions
            .retain(|t| t.to != name && t.from.as_deref() != Some(name));
        if m.initial == name {
            m.initial = m.states.first().map(|s| s.name.clone()).unwrap_or_default();
        }
        let doomed: Vec<NodeId> = self
            .nodes
            .iter()
            .filter(|(_, n)| {
                n.scope
                    == Scope::State {
                        machine,
                        state: name.to_string(),
                    }
            })
            .map(|(id, _)| *id)
            .collect();
        for id in doomed {
            if self.nodes.contains_key(&id) {
                self.remove_node(id)?;
            }
        }
        Ok(())
    }

    /// Adds a transition; returns its index.
    ///
    /// # Errors
    ///
    /// Not a machine, or unknown states.
    pub fn add_transition(&mut self, machine: NodeId, transition: TransitionDef) -> Result<usize, String> {
        let m = self.machine_mut(machine)?;
        check_transition_states(m, &transition)?;
        m.transitions.push(transition);
        Ok(m.transitions.len() - 1)
    }

    /// Replaces transition `index`.
    ///
    /// # Errors
    ///
    /// Not a machine, bad index or unknown states.
    pub fn set_transition(
        &mut self,
        machine: NodeId,
        index: usize,
        transition: TransitionDef,
    ) -> Result<(), String> {
        let m = self.machine_mut(machine)?;
        check_transition_states(m, &transition)?;
        let slot = m
            .transitions
            .get_mut(index)
            .ok_or_else(|| format!("machine {machine} has no transition {index}"))?;
        *slot = transition;
        Ok(())
    }

    /// Removes transition `index`.
    ///
    /// # Errors
    ///
    /// Not a machine or bad index.
    pub fn remove_transition(&mut self, machine: NodeId, index: usize) -> Result<TransitionDef, String> {
        let m = self.machine_mut(machine)?;
        if index >= m.transitions.len() {
            return Err(format!("machine {machine} has no transition {index}"));
        }
        Ok(m.transitions.remove(index))
    }

    /// Declares a parameter (or replaces the default of an existing one of the same name).
    pub fn set_param(&mut self, name: &str, default: ParamValue) {
        match self.parameters.iter_mut().find(|p| p.0 == name) {
            Some(p) => p.1 = default,
            None => self.parameters.push(ParamDecl(name.to_string(), default)),
        }
    }

    /// Removes a parameter declaration; returns whether it existed.
    pub fn remove_param(&mut self, name: &str) -> bool {
        let before = self.parameters.len();
        self.parameters.retain(|p| p.0 != name);
        before != self.parameters.len()
    }

    /// Nodes on `scope`, by id.
    pub fn nodes_in<'a>(&'a self, scope: &'a Scope) -> impl Iterator<Item = (NodeId, &'a ModelNode)> + 'a {
        self.nodes
            .iter()
            .filter(move |(_, n)| &n.scope == scope)
            .map(|(id, n)| (*id, n))
    }

    /// Compiles to the asset tree.
    ///
    /// # Errors
    ///
    /// No root output, an unconnected input or an empty state on a reachable node.
    pub fn compile(&self) -> Result<Compiled, String> {
        let root = self
            .output
            .ok_or_else(|| "the graph has no output node (connect a node to Output)".to_string())?;
        let mut paths = BTreeMap::new();
        let node = self.compile_node(root, ROOT_PATH.to_string(), &mut paths, 0)?;
        let unreachable = self
            .nodes
            .keys()
            .filter(|id| !paths.contains_key(*id))
            .copied()
            .collect();
        Ok(Compiled {
            graph: AnimGraph {
                parameters: self.parameters.clone(),
                root: node,
            },
            paths,
            unreachable,
        })
    }

    fn compile_node(
        &self,
        id: NodeId,
        path: NodePath,
        paths: &mut BTreeMap<NodeId, NodePath>,
        depth: usize,
    ) -> Result<AnimNode, String> {
        if depth > 256 || paths.contains_key(&id) {
            return Err(format!("node {id} is reached twice (graphs are trees)"));
        }
        let node = self.node(id)?;
        paths.insert(id, path.clone());
        let names = node.kind.input_names();
        let mut child = |i: usize| -> Result<Box<AnimNode>, String> {
            let name = &names[i];
            let c = node.inputs.get(i).copied().flatten().ok_or_else(|| {
                format!(
                    "node {id} ({}) input `{name}` is not connected",
                    node.kind.label()
                )
            })?;
            Ok(Box::new(self.compile_node(
                c,
                child_path(&path, name),
                paths,
                depth + 1,
            )?))
        };
        Ok(match &node.kind {
            NodeKind::Clip(c) => AnimNode::Clip(c.clone()),
            NodeKind::BlendSpace1D(b) => AnimNode::BlendSpace1D(b.clone()),
            NodeKind::BlendSpace2D(b) => AnimNode::BlendSpace2D(b.clone()),
            NodeKind::Blend { alpha_param } => AnimNode::Blend(BlendNode {
                a: child(0)?,
                b: child(1)?,
                alpha_param: alpha_param.clone(),
            }),
            NodeKind::Additive { weight_param } => AnimNode::Additive(AdditiveNode {
                base: child(0)?,
                additive: child(1)?,
                weight_param: weight_param.clone(),
            }),
            NodeKind::Layer { layers } => {
                let base = child(0)?;
                let mut entries = Vec::with_capacity(layers.len());
                for (i, meta) in layers.iter().enumerate() {
                    entries.push(LayerEntry {
                        graph: child(i + 1)?,
                        mask: meta.mask.clone(),
                        mode: meta.mode,
                        weight_param: meta.weight_param.clone(),
                        weight_inertialize: meta.weight_inertialize,
                    });
                }
                AnimNode::Layer(LayerNode {
                    base,
                    layers: entries,
                })
            }
            NodeKind::TwoBoneIK(ik) => AnimNode::TwoBoneIK(TwoBoneIKNode {
                inner: child(0)?,
                chain: ik.chain.clone(),
                target: ik.target.clone(),
                pole: ik.pole.clone(),
                weight_param: ik.weight_param.clone(),
            }),
            NodeKind::LookAt(l) => AnimNode::LookAt(LookAtNode {
                inner: child(0)?,
                joint: l.joint.clone(),
                target: l.target.clone(),
                max_yaw: l.max_yaw,
                max_pitch: l.max_pitch,
                weight_param: l.weight_param.clone(),
            }),
            NodeKind::RootMotion { mode } => AnimNode::RootMotion(RootMotionNode {
                inner: child(0)?,
                mode: *mode,
            }),
            NodeKind::ChainIK(c) => AnimNode::ChainIK(ChainIKNode {
                inner: child(0)?,
                root: c.root.clone(),
                tip: c.tip.clone(),
                solver: c.solver,
                target: c.target.clone(),
                pole: c.pole.clone(),
                limits: c.limits.clone(),
                iterations: c.iterations,
                tolerance: c.tolerance,
                weight_param: c.weight_param.clone(),
            }),
            NodeKind::BodyAim(a) => AnimNode::BodyAim(BodyAimNode {
                inner: child(0)?,
                joints: a.joints.clone(),
                aim_joint: a.aim_joint.clone(),
                axis: a.axis,
                target: a.target.clone(),
                limits: a.limits.clone(),
                passes: a.passes,
                weight_param: a.weight_param.clone(),
            }),
            NodeKind::StrideWarp(w) => AnimNode::StrideWarp(StrideWarpNode {
                inner: child(0)?,
                ground_speed_param: w.ground_speed_param.clone(),
                settings: w.settings,
                legs: w.legs.clone(),
                pelvis: w.pelvis.clone(),
                stride_length: w.stride_length,
            }),
            NodeKind::OrientationWarp(w) => AnimNode::OrientationWarp(OrientationWarpNode {
                inner: child(0)?,
                heading_param: w.heading_param.clone(),
                pelvis: w.pelvis.clone(),
                spine: w.spine.clone(),
                strength: w.strength,
                max_delta: w.max_delta,
            }),
            NodeKind::Slot {
                name,
                eval_idle_source,
            } => AnimNode::Slot(SlotNode {
                name: name.clone(),
                source: child(0)?,
                eval_idle_source: *eval_idle_source,
            }),
            NodeKind::StateMachine(m) => {
                if m.states.is_empty() {
                    return Err(format!("state machine {id} has no states"));
                }
                let mut states = Vec::with_capacity(m.states.len());
                for state in &m.states {
                    let out = state.output.ok_or_else(|| {
                        format!("state {:?} of machine {id} has no output node", state.name)
                    })?;
                    let sub = self.compile_node(
                        out,
                        child_path(&path, &format!("state:{}", state.name)),
                        paths,
                        depth + 1,
                    )?;
                    states.push((state.name.clone(), sub));
                }
                AnimNode::StateMachine(StateMachineDef {
                    initial: m.initial.clone(),
                    states,
                    transitions: m.transitions.clone(),
                })
            }
        })
    }

    /// Compiles and validates (parameter declarations, transitions) — what Save writes.
    ///
    /// # Errors
    ///
    /// Compile or validation failure, as text.
    pub fn compile_validated(&self) -> Result<Compiled, String> {
        let compiled = self.compile()?;
        compiled.graph.validate().map_err(|e| e.to_string())?;
        Ok(compiled)
    }

    /// Builds a model from an asset graph, laying nodes out right-to-left from each scope's
    /// output (`layout` positions, keyed by runtime path, win when present).
    #[must_use]
    pub fn from_graph(graph: &AnimGraph, layout: &BTreeMap<NodePath, [f32; 2]>) -> Self {
        let mut model = GraphModel {
            parameters: graph.parameters.clone(),
            ..GraphModel::default()
        };
        let mut rows = BTreeMap::new();
        let root = model.import(
            &graph.root,
            Scope::Root,
            ROOT_PATH.to_string(),
            0,
            &mut rows,
            layout,
        );
        model.output = Some(root);
        model
    }

    fn import(
        &mut self,
        node: &AnimNode,
        scope: Scope,
        path: NodePath,
        depth: usize,
        rows: &mut BTreeMap<Scope, f32>,
        layout: &BTreeMap<NodePath, [f32; 2]>,
    ) -> NodeId {
        let id = self.next_id;
        self.next_id += 1;
        // Placeholder until children are known (they need ids first).
        let (kind, children): (NodeKind, Vec<(&str, &AnimNode)>) = match node {
            AnimNode::Clip(c) => (NodeKind::Clip(c.clone()), vec![]),
            AnimNode::BlendSpace1D(b) => (NodeKind::BlendSpace1D(b.clone()), vec![]),
            AnimNode::BlendSpace2D(b) => (NodeKind::BlendSpace2D(b.clone()), vec![]),
            AnimNode::Blend(b) => (
                NodeKind::Blend {
                    alpha_param: b.alpha_param.clone(),
                },
                vec![("a", &*b.a), ("b", &*b.b)],
            ),
            AnimNode::Additive(a) => (
                NodeKind::Additive {
                    weight_param: a.weight_param.clone(),
                },
                vec![("base", &*a.base), ("additive", &*a.additive)],
            ),
            AnimNode::Layer(l) => {
                let layers = l
                    .layers
                    .iter()
                    .map(|e| LayerMeta {
                        mask: e.mask.clone(),
                        mode: e.mode,
                        weight_param: e.weight_param.clone(),
                        weight_inertialize: e.weight_inertialize,
                    })
                    .collect();
                let mut children: Vec<(&str, &AnimNode)> = vec![("base", &*l.base)];
                children.extend(l.layers.iter().map(|e| ("layer", &*e.graph)));
                (NodeKind::Layer { layers }, children)
            }
            AnimNode::TwoBoneIK(ik) => (
                NodeKind::TwoBoneIK(TwoBoneIkModel {
                    chain: ik.chain.clone(),
                    target: ik.target.clone(),
                    pole: ik.pole.clone(),
                    weight_param: ik.weight_param.clone(),
                }),
                vec![("inner", &*ik.inner)],
            ),
            AnimNode::LookAt(l) => (
                NodeKind::LookAt(LookAtModel {
                    joint: l.joint.clone(),
                    target: l.target.clone(),
                    max_yaw: l.max_yaw,
                    max_pitch: l.max_pitch,
                    weight_param: l.weight_param.clone(),
                }),
                vec![("inner", &*l.inner)],
            ),
            AnimNode::RootMotion(r) => (NodeKind::RootMotion { mode: r.mode }, vec![("inner", &*r.inner)]),
            AnimNode::ChainIK(c) => (
                NodeKind::ChainIK(ChainIkModel {
                    root: c.root.clone(),
                    tip: c.tip.clone(),
                    solver: c.solver,
                    target: c.target.clone(),
                    pole: c.pole.clone(),
                    limits: c.limits.clone(),
                    iterations: c.iterations,
                    tolerance: c.tolerance,
                    weight_param: c.weight_param.clone(),
                }),
                vec![("inner", &*c.inner)],
            ),
            AnimNode::BodyAim(a) => (
                NodeKind::BodyAim(BodyAimModel {
                    joints: a.joints.clone(),
                    aim_joint: a.aim_joint.clone(),
                    axis: a.axis,
                    target: a.target.clone(),
                    limits: a.limits.clone(),
                    passes: a.passes,
                    weight_param: a.weight_param.clone(),
                }),
                vec![("inner", &*a.inner)],
            ),
            AnimNode::StrideWarp(w) => (
                NodeKind::StrideWarp(StrideWarpModel {
                    ground_speed_param: w.ground_speed_param.clone(),
                    settings: w.settings,
                    legs: w.legs.clone(),
                    pelvis: w.pelvis.clone(),
                    stride_length: w.stride_length,
                }),
                vec![("inner", &*w.inner)],
            ),
            AnimNode::OrientationWarp(w) => (
                NodeKind::OrientationWarp(OrientationWarpModel {
                    heading_param: w.heading_param.clone(),
                    pelvis: w.pelvis.clone(),
                    spine: w.spine.clone(),
                    strength: w.strength,
                    max_delta: w.max_delta,
                }),
                vec![("inner", &*w.inner)],
            ),
            AnimNode::Slot(n) => (
                NodeKind::Slot {
                    name: n.name.clone(),
                    eval_idle_source: n.eval_idle_source,
                },
                vec![("source", &*n.source)],
            ),
            AnimNode::StateMachine(sm) => (
                NodeKind::StateMachine(MachineModel {
                    initial: sm.initial.clone(),
                    states: Vec::new(),
                    transitions: sm.transitions.clone(),
                }),
                vec![],
            ),
        };
        let slots = kind.input_names();
        let row = *rows.get(&scope).unwrap_or(&0.0);
        let default_pos = [-(depth as f32) * 460.0, row * 130.0];
        if children.is_empty() {
            rows.insert(scope.clone(), row + 1.0);
        }
        let pos = layout.get(&path).copied().unwrap_or(default_pos);
        self.nodes.insert(
            id,
            ModelNode {
                kind,
                scope: scope.clone(),
                pos,
                inputs: vec![None; slots.len()],
            },
        );
        for (i, (_, child)) in children.into_iter().enumerate() {
            let child_id = self.import(
                child,
                scope.clone(),
                child_path(&path, &slots[i]),
                depth + 1,
                rows,
                layout,
            );
            if let Some(n) = self.nodes.get_mut(&id) {
                n.inputs[i] = Some(child_id);
            }
        }
        if let AnimNode::StateMachine(sm) = node {
            let count = sm.states.len().max(1) as f32;
            for (index, (name, state_node)) in sm.states.iter().enumerate() {
                let state_path = child_path(&path, &format!("state:{name}"));
                let angle = index as f32 / count * std::f32::consts::TAU;
                let default_state_pos = [220.0 * angle.cos(), 160.0 * angle.sin()];
                let state_pos = layout
                    .get(&state_layout_key(&state_path))
                    .copied()
                    .unwrap_or(default_state_pos);
                let state_scope = Scope::State {
                    machine: id,
                    state: name.clone(),
                };
                let out = self.import(state_node, state_scope, state_path, 0, rows, layout);
                if let Some(ModelNode {
                    kind: NodeKind::StateMachine(m),
                    ..
                }) = self.nodes.get_mut(&id)
                {
                    m.states.push(StateModel {
                        name: name.clone(),
                        pos: state_pos,
                        output: Some(out),
                    });
                }
            }
        }
        id
    }

    /// Canvas positions keyed by runtime path (nodes and states), for the layout sidecar.
    #[must_use]
    pub fn layout(&self) -> BTreeMap<NodePath, [f32; 2]> {
        let mut out = BTreeMap::new();
        let Ok(compiled) = self.compile() else {
            return out;
        };
        for (id, path) in &compiled.paths {
            let Some(node) = self.nodes.get(id) else {
                continue;
            };
            out.insert(path.clone(), node.pos);
            if let NodeKind::StateMachine(m) = &node.kind {
                for state in &m.states {
                    out.insert(
                        state_layout_key(&child_path(path, &format!("state:{}", state.name))),
                        state.pos,
                    );
                }
            }
        }
        out
    }
}

/// The layout key of a state box (the state's path is also its output node's path).
#[must_use]
pub fn state_layout_key(state_path: &str) -> String {
    format!("{state_path}@state")
}

fn check_transition_states(m: &MachineModel, t: &TransitionDef) -> Result<(), String> {
    let has = |name: &str| m.states.iter().any(|s| s.name == name);
    if let Some(from) = &t.from
        && !has(from)
    {
        return Err(format!("no state {from:?}"));
    }
    if !has(&t.to) {
        return Err(format!("no state {:?}", t.to));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_animation::blendspace::BlendSample1D;
    use regia_animation::state_machine::{CmpOp, Condition, TransitionCurve};

    fn clip(name: &str) -> NodeKind {
        NodeKind::Clip(ClipNode {
            clip: name.into(),
            looping: true,
            speed: 1.0,
            sync_group: None,
        })
    }

    #[test]
    fn builds_compiles_and_round_trips_a_machine() {
        let mut m = GraphModel::default();
        m.set_param("Speed", ParamValue::Float(0.0));
        m.set_param("Wave", ParamValue::Float(0.0));
        let sm = m
            .add_node(
                NodeKind::default_of("state_machine").expect("kind"),
                Scope::Root,
                [0.0, 0.0],
            )
            .expect("add");
        m.add_state(sm, "Idle", [0.0, 0.0]).expect("state");
        m.add_state(sm, "Walk", [200.0, 0.0]).expect("state");
        let idle_scope = Scope::State {
            machine: sm,
            state: "Idle".into(),
        };
        let walk_scope = Scope::State {
            machine: sm,
            state: "Walk".into(),
        };
        let idle = m
            .add_node(clip("Idle"), idle_scope.clone(), [0.0, 0.0])
            .expect("add");
        m.set_output(&idle_scope, Some(idle)).expect("output");
        let bs = m
            .add_node(
                NodeKind::BlendSpace1D(BlendSpace1DNode {
                    space: BlendSpace1D {
                        param: "Speed".into(),
                        samples: vec![
                            BlendSample1D {
                                value: 0.0,
                                clip: "Walk".into(),
                            },
                            BlendSample1D {
                                value: 4.0,
                                clip: "Run".into(),
                            },
                        ],
                    },
                    sync_group: None,
                }),
                walk_scope.clone(),
                [0.0, 0.0],
            )
            .expect("add");
        m.set_output(&walk_scope, Some(bs)).expect("output");
        m.add_transition(
            sm,
            TransitionDef {
                from: Some("Idle".into()),
                to: "Walk".into(),
                conditions: vec![Condition::Float {
                    param: "Speed".into(),
                    op: CmpOp::Gt,
                    value: 0.5,
                }],
                duration: 0.2,
                curve: TransitionCurve::Smooth,
                interruptible: true,
                blend: Default::default(),
            },
        )
        .expect("transition");
        // Layer over the machine with one wave layer.
        let layer = m
            .add_node(
                NodeKind::Layer {
                    layers: vec![LayerMeta {
                        mask: JointMaskDef {
                            joints: vec!["Spine".into()],
                            include_children: true,
                        },
                        mode: LayerMode::Override,
                        weight_param: "Wave".into(),
                        weight_inertialize: 0.0,
                    }],
                },
                Scope::Root,
                [300.0, 0.0],
            )
            .expect("add");
        let wave = m.add_node(clip("Wave"), Scope::Root, [0.0, 200.0]).expect("add");
        m.connect(sm, layer, "base").expect("connect");
        m.connect(wave, layer, "layer0").expect("connect");
        assert!(m.compile().is_err(), "no output yet");
        m.set_output(&Scope::Root, Some(layer)).expect("output");

        let compiled = m.compile_validated().expect("valid");
        assert!(compiled.unreachable.is_empty());
        assert_eq!(compiled.paths[&idle], "root/base/state:Idle");
        assert_eq!(compiled.paths[&wave], "root/layer0");
        let text = compiled.graph.to_ron_string().expect("ron");
        let loaded = AnimGraph::from_ron_str(&text).expect("loads");
        assert_eq!(loaded.root, compiled.graph.root);

        // Decompile with the saved layout: same tree, same positions.
        let layout = m.layout();
        assert_eq!(layout["root/base/state:Walk@state"], [200.0, 0.0]);
        let back = GraphModel::from_graph(&loaded, &layout);
        let again = back.compile_validated().expect("valid");
        assert_eq!(again.graph.root, loaded.root);
        let layer_back = back.output.expect("output");
        assert_eq!(back.node(layer_back).expect("node").pos, [300.0, 0.0]);
    }

    #[test]
    fn edits_enforce_scopes_cycles_and_clean_up() {
        let mut m = GraphModel::default();
        m.set_param("A", ParamValue::Float(0.0));
        let blend = m
            .add_node(
                NodeKind::default_of("blend").expect("kind"),
                Scope::Root,
                [0.0, 0.0],
            )
            .expect("add");
        let other = m
            .add_node(
                NodeKind::default_of("blend").expect("kind"),
                Scope::Root,
                [0.0, 0.0],
            )
            .expect("add");
        let a = m.add_node(clip("A"), Scope::Root, [0.0, 0.0]).expect("add");
        m.connect(a, blend, "a").expect("connect");
        // Reconnecting moves the child (trees: one consumer).
        m.connect(a, other, "b").expect("connect");
        assert_eq!(m.node(blend).expect("node").inputs[0], None);
        assert_eq!(m.node(other).expect("node").inputs[1], Some(a));
        // Cycles are refused.
        m.connect(other, blend, "a").expect("connect");
        assert!(m.connect(blend, other, "a").is_err());
        assert!(m.connect(a, a, "a").is_err());
        assert!(m.connect(a, blend, "nope").is_err());
        // Scopes.
        let sm = m
            .add_node(
                NodeKind::default_of("state_machine").expect("kind"),
                Scope::Root,
                [0.0, 0.0],
            )
            .expect("add");
        assert!(
            m.add_node(
                clip("x"),
                Scope::State {
                    machine: sm,
                    state: "Missing".into()
                },
                [0.0, 0.0]
            )
            .is_err()
        );
        m.add_state(sm, "S", [0.0, 0.0]).expect("state");
        let scope = Scope::State {
            machine: sm,
            state: "S".into(),
        };
        let inner = m.add_node(clip("x"), scope.clone(), [0.0, 0.0]).expect("add");
        assert!(m.connect(inner, blend, "b").is_err(), "different canvases");
        m.set_output(&scope, Some(inner)).expect("output");
        // Rename follows into scopes and transitions.
        m.add_state(sm, "T", [0.0, 0.0]).expect("state");
        m.add_transition(
            sm,
            TransitionDef {
                from: Some("S".into()),
                to: "T".into(),
                conditions: vec![],
                duration: 0.1,
                curve: TransitionCurve::Linear,
                interruptible: false,
                blend: Default::default(),
            },
        )
        .expect("transition");
        m.rename_state(sm, "S", "Start").expect("rename");
        assert_eq!(
            m.node(inner).expect("node").scope,
            Scope::State {
                machine: sm,
                state: "Start".into()
            }
        );
        assert_eq!(
            m.machine(sm).expect("sm").transitions[0].from.as_deref(),
            Some("Start")
        );
        assert_eq!(m.machine(sm).expect("sm").initial, "Start");
        // Removing the state drops its nodes and transitions.
        m.remove_state(sm, "Start").expect("remove");
        assert!(m.node(inner).is_err());
        assert!(m.machine(sm).expect("sm").transitions.is_empty());
        // Removing a machine removes its sub-canvases.
        let t_node = m
            .add_node(
                clip("t"),
                Scope::State {
                    machine: sm,
                    state: "T".into(),
                },
                [0.0, 0.0],
            )
            .expect("add");
        m.remove_node(sm).expect("remove");
        assert!(m.node(t_node).is_err());
        // Unreachable nodes are reported.
        m.set_output(&Scope::Root, Some(blend)).expect("output");
        m.connect(a, other, "b").expect("connect");
        let leaf = m.add_node(clip("leaf"), Scope::Root, [0.0, 0.0]).expect("add");
        m.connect(leaf, other, "a").expect("connect");
        let c = m.add_node(clip("c"), Scope::Root, [0.0, 0.0]).expect("add");
        m.connect(c, blend, "b").expect("connect");
        let compiled = m.compile().expect("compiles");
        assert!(compiled.unreachable.is_empty());
        let stray = m.add_node(clip("stray"), Scope::Root, [0.0, 0.0]).expect("add");
        assert_eq!(m.compile().expect("compiles").unreachable, vec![stray]);
        // The undeclared alpha parameter fails validation.
        assert!(m.compile_validated().is_err());
        m.set_kind(
            blend,
            NodeKind::Blend {
                alpha_param: "A".into(),
            },
        )
        .expect("kind");
        m.set_kind(
            other,
            NodeKind::Blend {
                alpha_param: "A".into(),
            },
        )
        .expect("kind");
        m.compile_validated().expect("valid now");
        assert!(
            m.set_kind(blend, NodeKind::default_of("state_machine").expect("kind"))
                .is_err()
        );
        assert!(m.remove_param("A") && !m.remove_param("A"));
    }

    #[test]
    fn slot_node_compiles_and_decompiles() {
        let graph = AnimGraph {
            parameters: Vec::new(),
            root: AnimNode::Slot(SlotNode {
                name: "upper".into(),
                source: Box::new(AnimNode::Clip(ClipNode {
                    clip: "Idle".into(),
                    looping: true,
                    speed: 1.0,
                    sync_group: None,
                })),
                eval_idle_source: false,
            }),
        };
        let model = GraphModel::from_graph(&graph, &BTreeMap::new());
        let root = model.output.expect("output");
        assert_eq!(model.node(root).expect("node").kind.label(), "slot");
        assert_eq!(model.node(root).expect("node").kind.input_names(), ["source"]);
        let compiled = model.compile_validated().expect("valid");
        assert_eq!(compiled.graph.root, graph.root);
        assert!(
            compiled.paths.values().any(|p| p == "root/source"),
            "{:?}",
            compiled.paths
        );
    }

    #[test]
    fn every_kind_label_round_trips() {
        for label in NodeKind::LABELS {
            let kind = NodeKind::default_of(label).expect("known");
            assert_eq!(kind.label(), label);
        }
        assert!(NodeKind::default_of("teleport").is_none());
    }
}
