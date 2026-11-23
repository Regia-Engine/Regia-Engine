// SPDX-License-Identifier: MPL-2.0
//! Debug snapshots of a running graph, for the editor's PIE debugging and the agent protocol
//! (`docs/specs/17-animation.md` § 5: "live debugging during PIE: highlight active state, show
//! weights").
//!
//! [`graph_debug`] walks a graph definition and its [`GraphInstance`] in parallel and reports,
//! per node, whether it contributes to the pose this frame, its accumulated weight, and
//! node-specific state (clip time, blend-space sample weights, current state and transition
//! progress). Nodes are keyed by [`NodePath`]s — the same paths the editor assigns when it
//! compiles its graph model — so an editor can map the snapshot back onto its canvas.

use regia_core::math::{Mat4, Quat, Vec3};
use serde::Serialize;

use crate::eval::GraphInstance;
use crate::graph::{AnimGraph, AnimNode};
use crate::params::{ParamValue, ParameterSet};
use crate::pose::Pose;
use crate::skeleton::Skeleton;
use crate::state_machine::TransitionBlend;

/// A node's position in the graph tree: `root`, then one segment per edge (`a`, `b`, `base`,
/// `additive`, `layer<i>`, `inner`, `state:<name>`), joined by `/`.
pub type NodePath = String;

/// The path of child `segment` under `parent`.
#[must_use]
pub fn child_path(parent: &str, segment: &str) -> NodePath {
    format!("{parent}/{segment}")
}

/// The path of the graph root.
pub const ROOT_PATH: &str = "root";

/// What a node is doing this frame.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NodeDebugState {
    /// A clip's playback.
    Clip {
        /// Clip name.
        clip: String,
        /// Clip-local time.
        time: f32,
        /// Non-looping clip reached its end.
        finished: bool,
    },
    /// A blend space's sample weights at the current parameter value(s).
    BlendSpace {
        /// The parameter input (`[x, 0]` for 1D).
        input: [f32; 2],
        /// `(clip, weight)` of the samples with weight > 0.
        weights: Vec<(String, f32)>,
    },
    /// A state machine.
    StateMachine {
        /// Current (target) state.
        current: String,
        /// Transition in progress: `(from, to, progress 0..=1)`.
        transition: Option<(String, String, f32)>,
        /// Blend mode of the transition in progress (`None` when none runs).
        blend: Option<TransitionBlend>,
        /// Largest joint rotation offset (radians) an inertialized transition still adds on
        /// top of the current state; 0 once it has settled (also 0 for cross-fades).
        inertial_offset: f32,
    },
    /// A two-input blend / additive node's alpha.
    Blend {
        /// Weight of the second input (`b` / additive).
        alpha: f32,
    },
    /// A layer stack: per-layer weights.
    Layers {
        /// Weight parameter value per layer.
        weights: Vec<f32>,
    },
    /// IK / look-at / root-motion wrappers.
    Wrapper,
    /// A `StrideWarp` node (format v4, ADR-8540).
    StrideWarp {
        /// `Rate` (clock) or `Pose` (stride IK).
        mode: crate::motion_matching::warp::StrideMode,
        /// The smoothed ratio (1 = no warp).
        ratio: f32,
        /// The inner graph's clip speed on the latest frame, m/s.
        clip_speed: f32,
        /// The ground-speed parameter on the latest frame, m/s.
        ground_speed: f32,
        /// How far `Pose` mode lowered the pelvis on the latest frame, meters.
        pelvis_drop: f32,
    },
    /// An `OrientationWarp` node (format v4, ADR-8540).
    OrientationWarp {
        /// The lower-body turn applied on the latest frame, radians.
        delta: f32,
        /// The inner graph's root-motion heading on the latest frame, radians.
        clip_heading: f32,
    },
}

/// One node of a [`GraphDebug`] snapshot.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct NodeDebug {
    /// The node's path.
    pub path: NodePath,
    /// Whether the node contributes to this frame's pose.
    pub active: bool,
    /// Accumulated weight from the root (1 at the root; 0 for inactive nodes).
    pub weight: f32,
    /// Node-specific state.
    pub state: NodeDebugState,
}

/// A snapshot of a running graph plus its parameters.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct GraphDebug {
    /// Nodes in depth-first order (root first).
    pub nodes: Vec<NodeDebug>,
    /// Parameter values, sorted by name.
    pub parameters: Vec<(String, ParamValue)>,
    /// One row per IK node of the graph, depth-first, with what its solve reached on the
    /// latest evaluation. Filled only while the rig's IK debug flag is on
    /// ([`crate::Animator::set_ik_debug`], `anim.ik_report`); empty otherwise.
    pub ik: Vec<IkNodeDebug>,
}

/// Which graph IK node an [`IkNodeDebug`] row reports.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IkNodeKind {
    /// `TwoBoneIK`: residual in meters (solved tip to target), one analytic pass.
    TwoBoneIk,
    /// `LookAt`: residual in radians (the joint's −Z axis to the target direction).
    LookAt,
    /// `ChainIK`: residual in meters (unblended solved tip to target), FABRIK or CCD passes.
    ChainIk,
    /// `BodyAim`: residual in radians (aim axis to the target direction), solver passes.
    BodyAim,
}

/// What one IK node's solve reached on the latest evaluation (`anim.ik_report`).
///
/// Recorded only while the rig's IK debug flag is on; with it off the evaluator measures
/// nothing (no extra forward kinematics, no buffer writes).
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct IkNodeDebug {
    /// The node's path, as in [`GraphDebug::nodes`].
    pub path: NodePath,
    /// Node kind; it fixes the unit of `residual`.
    pub kind: IkNodeKind,
    /// The node solved on the latest evaluation. False when its weight was 0, its target did
    /// not resolve, its chain was invalid, or the node sits in an inactive branch; the other
    /// fields are then 0 / false.
    pub solved: bool,
    /// Distance (meters: `two_bone_ik`, `chain_ik`) or angle (radians: `look_at`,
    /// `body_aim`) left between the solved pose and the target. `two_bone_ik` and `look_at`
    /// measure the final (weighted) pose; `chain_ik` measures the unblended solve, as its
    /// solver reports it.
    pub residual: f32,
    /// Solver iterations: 1 for the analytic `two_bone_ik` and `look_at`, the passes used
    /// for `chain_ik` and `body_aim`.
    pub iterations: u32,
    /// The target was out of the node's reach: outside a two-bone chain's reachable annulus,
    /// past a chain's total length from its root, or a look-at yaw or pitch limit held the
    /// joint (exact at weight 1); for `body_aim`, the joint limits left more than 1 mrad.
    pub clamped: bool,
    /// The node's IK weight `0..=1` (its weight parameter, 1 without one).
    pub weight: f32,
}

/// One capture of an IK node's solve, keyed by the node's address (crate internal).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct IkRecord {
    /// Address of the node definition inside the evaluated graph.
    pub node: usize,
    pub kind: IkNodeKind,
    pub residual: f32,
    pub iterations: u32,
    pub clamped: bool,
    pub weight: f32,
}

/// The IK debug flag and the latest evaluation's captures of one rig (kept in
/// [`crate::eval::EvalReuse`]). Off by default: the evaluator then only reads `on`.
#[derive(Clone, Debug, Default)]
pub(crate) struct IkDebugLog {
    /// Capture residuals during evaluation.
    pub on: bool,
    /// Captures of the latest evaluation; capacity stays between evaluations.
    pub records: Vec<IkRecord>,
    /// Residual measurements made since the rig was created (saturating). Stays 0 while
    /// the flag is off: the proof that the off path measures nothing.
    pub captures: u64,
}

impl IkDebugLog {
    /// Start an evaluation: forget the previous captures.
    pub fn begin(&mut self) {
        self.records.clear();
    }

    /// Store one capture (a node evaluated twice keeps its later capture).
    pub fn record(&mut self, record: IkRecord) {
        if let Some(slot) = self.records.iter_mut().find(|r| r.node == record.node) {
            *slot = record;
        } else {
            self.records.push(record);
        }
        self.captures = self.captures.saturating_add(1);
    }
}

/// Object-space matrix of `joint` under `pose` without a heap allocation (chains deeper than
/// 64 joints fall back to a full forward-kinematics pass). A joint outside the pose is the
/// identity.
fn joint_object_matrix(skeleton: &Skeleton, pose: &Pose, joint: u32) -> Mat4 {
    let n = skeleton.len().min(pose.len());
    let joint = joint as usize;
    if joint >= n {
        return Mat4::IDENTITY;
    }
    let mut chain = [0usize; 64];
    let mut depth = 0usize;
    let mut cursor = joint;
    loop {
        if depth == chain.len() {
            return skeleton
                .globals(pose, None)
                .get(joint)
                .copied()
                .unwrap_or(Mat4::IDENTITY);
        }
        chain[depth] = cursor;
        depth += 1;
        match skeleton.parents.get(cursor).copied().flatten() {
            Some(parent) if (parent as usize) < cursor => cursor = parent as usize,
            _ => break,
        }
    }
    let mut matrix = Mat4::IDENTITY;
    for &index in chain[..depth].iter().rev() {
        matrix *= Mat4::from_scale_rotation_translation(
            pose.scales[index],
            pose.rotations[index],
            pose.translations[index],
        );
    }
    matrix
}

/// Capture of a solved `TwoBoneIK` node: tip-to-target distance on the final pose, and
/// whether the target lay outside the reachable annulus (the solver's clamp, with its
/// margin).
pub(crate) fn two_bone_record(
    skeleton: &Skeleton,
    pose: &Pose,
    chain: &crate::ik::IkChain,
    target: Vec3,
    weight: f32,
    node: usize,
) -> IkRecord {
    let a = joint_object_matrix(skeleton, pose, chain.root).w_axis.truncate();
    let b = joint_object_matrix(skeleton, pose, chain.mid).w_axis.truncate();
    let c = joint_object_matrix(skeleton, pose, chain.tip).w_axis.truncate();
    let (ab, bc) = ((b - a).length(), (c - b).length());
    let margin = 1e-4f32.min(0.25 * ab.min(bc));
    let max_reach = ab + bc - margin;
    let min_reach = ((ab - bc).abs() + margin).min(max_reach);
    let dist = (target - a).length();
    IkRecord {
        node,
        kind: IkNodeKind::TwoBoneIk,
        residual: (c - target).length(),
        iterations: 1,
        clamped: dist > max_reach + 1e-6 || dist < min_reach - 1e-6,
        weight,
    }
}

/// Capture of a solved `LookAt` node: angle from the joint's −Z axis to the target, and
/// whether its yaw or pitch offset from the bind rotation sits on a limit.
pub(crate) fn look_at_record(
    skeleton: &Skeleton,
    pose: &Pose,
    joint: u32,
    bind_rotation: Quat,
    target: Vec3,
    settings: crate::ik::LookAtSettings,
    node: usize,
) -> IkRecord {
    let matrix = joint_object_matrix(skeleton, pose, joint);
    let (_, rotation, position) = matrix.to_scale_rotation_translation();
    let aim = (rotation * Vec3::NEG_Z).normalize_or_zero();
    let to = (target - position).normalize_or_zero();
    let residual = crate::num::det_atan2(aim.cross(to).length(), aim.dot(to)).abs();
    let limit = |l: f32| if l.is_nan() { f32::INFINITY } else { l.abs() };
    let local = pose
        .rotations
        .get(joint as usize)
        .copied()
        .unwrap_or(Quat::IDENTITY);
    let (yaw, pitch, _) = regia_core::math::det::quat_to_euler_yxz(bind_rotation.inverse() * local);
    let on = |value: f32, max: f32| max.is_finite() && value.abs() >= max - 1e-4;
    IkRecord {
        node,
        kind: IkNodeKind::LookAt,
        residual,
        iterations: 1,
        clamped: on(yaw, limit(settings.max_yaw)) || on(pitch, limit(settings.max_pitch)),
        weight: settings.weight,
    }
}

impl GraphDebug {
    /// The node at `path`.
    #[must_use]
    pub fn node(&self, path: &str) -> Option<&NodeDebug> {
        self.nodes.iter().find(|n| n.path == path)
    }

    /// Current state of the first state machine reached from the root (depth-first).
    #[must_use]
    pub fn first_state(&self) -> Option<&str> {
        self.nodes.iter().find_map(|n| match &n.state {
            NodeDebugState::StateMachine { current, .. } => Some(current.as_str()),
            _ => None,
        })
    }
}

/// Builds the snapshot of `graph` running as `instance` with `params`.
#[must_use]
pub fn graph_debug(graph: &AnimGraph, instance: &GraphInstance, params: &ParameterSet) -> GraphDebug {
    let mut out = GraphDebug {
        nodes: Vec::new(),
        parameters: params.entries(),
        ik: Vec::new(),
    };
    walk(
        &graph.root,
        Some(instance),
        params,
        ROOT_PATH.to_string(),
        1.0,
        &mut out.nodes,
    );
    out
}

/// [`graph_debug`] plus the [`GraphDebug::ik`] rows of the IK captures `reuse` holds (empty
/// while the rig's IK debug flag is off). `graph` must be the graph `reuse` was evaluated
/// with: captures are matched to nodes by address.
#[must_use]
pub fn graph_debug_with_ik(
    graph: &AnimGraph,
    instance: &GraphInstance,
    params: &ParameterSet,
    reuse: &crate::eval::EvalReuse,
) -> GraphDebug {
    let mut out = graph_debug(graph, instance, params);
    let log = reuse.ik_debug_log();
    if !log.on {
        return out;
    }
    let mut found = Vec::new();
    ik_nodes(&graph.root, ROOT_PATH.to_string(), &mut found);
    out.ik = found
        .into_iter()
        .map(|(path, node, kind)| {
            let active = out.node(&path).is_some_and(|n| n.active);
            match log.records.iter().find(|r| r.node == node).filter(|_| active) {
                Some(r) => IkNodeDebug {
                    path,
                    kind: r.kind,
                    solved: true,
                    residual: r.residual,
                    iterations: r.iterations,
                    clamped: r.clamped,
                    weight: r.weight,
                },
                None => IkNodeDebug {
                    path,
                    kind,
                    solved: false,
                    residual: 0.0,
                    iterations: 0,
                    clamped: false,
                    weight: 0.0,
                },
            }
        })
        .collect();
    out
}

/// Every IK node under `node` as `(path, node address, kind)`, depth-first, with the path
/// segments [`walk`] uses.
fn ik_nodes(node: &AnimNode, path: NodePath, out: &mut Vec<(NodePath, usize, IkNodeKind)>) {
    fn address<T>(def: &T) -> usize {
        std::ptr::from_ref(def) as usize
    }
    match node {
        AnimNode::Clip(_) | AnimNode::BlendSpace1D(_) | AnimNode::BlendSpace2D(_) => {}
        AnimNode::StateMachine(def) => {
            for (name, state) in &def.states {
                ik_nodes(state, child_path(&path, &format!("state:{name}")), out);
            }
        }
        AnimNode::Blend(def) => {
            ik_nodes(&def.a, child_path(&path, "a"), out);
            ik_nodes(&def.b, child_path(&path, "b"), out);
        }
        AnimNode::Additive(def) => {
            ik_nodes(&def.base, child_path(&path, "base"), out);
            ik_nodes(&def.additive, child_path(&path, "additive"), out);
        }
        AnimNode::Layer(def) => {
            ik_nodes(&def.base, child_path(&path, "base"), out);
            for (index, layer) in def.layers.iter().enumerate() {
                ik_nodes(&layer.graph, child_path(&path, &format!("layer{index}")), out);
            }
        }
        AnimNode::TwoBoneIK(def) => {
            out.push((path.clone(), address(def), IkNodeKind::TwoBoneIk));
            ik_nodes(&def.inner, child_path(&path, "inner"), out);
        }
        AnimNode::LookAt(def) => {
            out.push((path.clone(), address(def), IkNodeKind::LookAt));
            ik_nodes(&def.inner, child_path(&path, "inner"), out);
        }
        AnimNode::ChainIK(def) => {
            out.push((path.clone(), address(def), IkNodeKind::ChainIk));
            ik_nodes(&def.inner, child_path(&path, "inner"), out);
        }
        AnimNode::BodyAim(def) => {
            out.push((path.clone(), address(def), IkNodeKind::BodyAim));
            ik_nodes(&def.inner, child_path(&path, "inner"), out);
        }
        AnimNode::RootMotion(def) => ik_nodes(&def.inner, child_path(&path, "inner"), out),
        AnimNode::Slot(def) => ik_nodes(&def.source, child_path(&path, "source"), out),
        AnimNode::StrideWarp(def) => ik_nodes(&def.inner, child_path(&path, "inner"), out),
        AnimNode::OrientationWarp(def) => ik_nodes(&def.inner, child_path(&path, "inner"), out),
    }
}

fn walk(
    node: &AnimNode,
    instance: Option<&GraphInstance>,
    params: &ParameterSet,
    path: NodePath,
    weight: f32,
    out: &mut Vec<NodeDebug>,
) {
    let active = weight > 1e-6;
    let push = |out: &mut Vec<NodeDebug>, state: NodeDebugState| {
        out.push(NodeDebug {
            path: path.clone(),
            active,
            weight,
            state,
        });
    };
    match node {
        AnimNode::Clip(def) => {
            let (time, finished) = match instance {
                Some(GraphInstance::Clip(st)) => (st.time, st.finished),
                _ => (0.0, false),
            };
            push(
                out,
                NodeDebugState::Clip {
                    clip: def.clip.clone(),
                    time,
                    finished,
                },
            );
        }
        AnimNode::BlendSpace1D(def) => {
            let x = params.float(&def.space.param);
            let weights = def
                .space
                .weights(x)
                .into_iter()
                .filter_map(|(i, w)| def.space.samples.get(i).map(|s| (s.clip.clone(), w)))
                .collect();
            push(
                out,
                NodeDebugState::BlendSpace {
                    input: [x, 0.0],
                    weights,
                },
            );
        }
        AnimNode::BlendSpace2D(def) => {
            let input = [params.float(&def.space.param_x), params.float(&def.space.param_y)];
            let weights = def
                .space
                .weights(input)
                .into_iter()
                .filter_map(|(i, w)| def.space.samples.get(i).map(|s| (s.clip.clone(), w)))
                .collect();
            push(out, NodeDebugState::BlendSpace { input, weights });
        }
        AnimNode::StateMachine(def) => {
            let machine = match instance {
                Some(GraphInstance::StateMachine(sm)) => Some(sm.as_ref()),
                _ => None,
            };
            let current = machine.map_or_else(|| def.state_index(&def.initial).unwrap_or(0), |sm| sm.current);
            let transition = machine.and_then(|sm| sm.transition.as_ref());
            let (from, alpha) = match transition {
                // Inertialized: only the target is evaluated, at full weight.
                Some(t) if t.blend.is_inertial() => {
                    let progress = def
                        .transitions
                        .get(t.def)
                        .map_or(1.0, |d| (t.elapsed / d.duration.max(1e-6)).clamp(0.0, 1.0));
                    (Some((t.from, progress)), 1.0)
                }
                Some(t) => {
                    let def_t = def.transitions.get(t.def);
                    let progress = def_t.map_or(1.0, |d| (t.elapsed / d.duration.max(1e-6)).clamp(0.0, 1.0));
                    let alpha = def_t.map_or(1.0, |d| d.curve.alpha(progress));
                    (Some((t.from, progress)), alpha)
                }
                None => (None, 1.0),
            };
            let name = |i: usize| def.states.get(i).map(|s| s.0.clone()).unwrap_or_default();
            push(
                out,
                NodeDebugState::StateMachine {
                    current: name(current),
                    transition: from.map(|(f, progress)| (name(f), name(current), progress)),
                    blend: transition.map(|t| t.blend),
                    inertial_offset: machine.map_or(0.0, |sm| sm.inertia.max_rotation_offset()),
                },
            );
            for (index, (state_name, state_node)) in def.states.iter().enumerate() {
                let state_weight = if index == current {
                    weight * alpha
                } else if alpha < 1.0 && from.is_some_and(|(f, _)| f == index) {
                    weight * (1.0 - alpha)
                } else {
                    0.0
                };
                let child = machine.and_then(|sm| sm.states.get(index));
                walk(
                    state_node,
                    child,
                    params,
                    child_path(&path, &format!("state:{state_name}")),
                    state_weight,
                    out,
                );
            }
        }
        AnimNode::Blend(def) => {
            let alpha = crate::num::unit_weight(params.float(&def.alpha_param));
            push(out, NodeDebugState::Blend { alpha });
            let (a, b) = match instance {
                Some(GraphInstance::Blend { a, b }) => (Some(a.as_ref()), Some(b.as_ref())),
                _ => (None, None),
            };
            walk(
                &def.a,
                a,
                params,
                child_path(&path, "a"),
                weight * (1.0 - alpha),
                out,
            );
            walk(&def.b, b, params, child_path(&path, "b"), weight * alpha, out);
        }
        AnimNode::Additive(def) => {
            let alpha = crate::num::unit_weight(params.float(&def.weight_param));
            push(out, NodeDebugState::Blend { alpha });
            let (base, additive) = match instance {
                Some(GraphInstance::Additive { base, additive }) => {
                    (Some(base.as_ref()), Some(additive.as_ref()))
                }
                _ => (None, None),
            };
            walk(&def.base, base, params, child_path(&path, "base"), weight, out);
            walk(
                &def.additive,
                additive,
                params,
                child_path(&path, "additive"),
                weight * alpha,
                out,
            );
        }
        AnimNode::Layer(def) => {
            // The weight actually used: an inertialized layer weight lags its parameter.
            let eased = match instance {
                Some(GraphInstance::Layer { weights, .. }) => Some(weights),
                _ => None,
            };
            let weights: Vec<f32> = def
                .layers
                .iter()
                .enumerate()
                .map(|(i, l)| match eased.and_then(|w| w.get(i)) {
                    Some(state) if l.weight_inertialize > 0.0 => state.output(),
                    _ => crate::num::unit_weight(params.float(&l.weight_param)),
                })
                .collect();
            push(
                out,
                NodeDebugState::Layers {
                    weights: weights.clone(),
                },
            );
            let (base, layers) = match instance {
                Some(GraphInstance::Layer { base, layers, .. }) => (Some(base.as_ref()), Some(layers)),
                _ => (None, None),
            };
            walk(&def.base, base, params, child_path(&path, "base"), weight, out);
            for (index, layer) in def.layers.iter().enumerate() {
                let child = layers.and_then(|l| l.get(index));
                walk(
                    &layer.graph,
                    child,
                    params,
                    child_path(&path, &format!("layer{index}")),
                    weight * weights[index],
                    out,
                );
            }
        }
        AnimNode::TwoBoneIK(def) => wrapper(&def.inner, "inner", instance, params, path, weight, out),
        AnimNode::LookAt(def) => wrapper(&def.inner, "inner", instance, params, path, weight, out),
        AnimNode::RootMotion(def) => wrapper(&def.inner, "inner", instance, params, path, weight, out),
        AnimNode::Slot(def) => wrapper(&def.source, "source", instance, params, path, weight, out),
        AnimNode::ChainIK(def) => wrapper(&def.inner, "inner", instance, params, path, weight, out),
        AnimNode::BodyAim(def) => wrapper(&def.inner, "inner", instance, params, path, weight, out),
        AnimNode::StrideWarp(def) => {
            let (state, child) = warp_parts(instance);
            push(
                out,
                NodeDebugState::StrideWarp {
                    mode: def.settings.mode,
                    ratio: state.map_or(1.0, |s| s.ratio()),
                    clip_speed: state.map_or(0.0, |s| s.clip_speed()),
                    ground_speed: state.map_or(0.0, |s| s.ground_speed()),
                    pelvis_drop: state.map_or(0.0, |s| s.pelvis_drop()),
                },
            );
            walk(&def.inner, child, params, child_path(&path, "inner"), weight, out);
        }
        AnimNode::OrientationWarp(def) => {
            let (state, child) = warp_parts(instance);
            push(
                out,
                NodeDebugState::OrientationWarp {
                    delta: state.map_or(0.0, |s| s.orientation_delta()),
                    clip_heading: state.map_or(0.0, |s| s.clip_heading()),
                },
            );
            walk(&def.inner, child, params, child_path(&path, "inner"), weight, out);
        }
    }
}

/// The state and inner instance of a warp node's instance.
fn warp_parts(
    instance: Option<&GraphInstance>,
) -> (Option<&crate::warp_nodes::WarpNodeState>, Option<&GraphInstance>) {
    match instance {
        Some(GraphInstance::Warp { inner, state }) => (Some(state.as_ref()), Some(inner.as_ref())),
        _ => (None, None),
    }
}

fn wrapper(
    inner: &AnimNode,
    segment: &str,
    instance: Option<&GraphInstance>,
    params: &ParameterSet,
    path: NodePath,
    weight: f32,
    out: &mut Vec<NodeDebug>,
) {
    out.push(NodeDebug {
        path: path.clone(),
        active: weight > 1e-6,
        weight,
        state: NodeDebugState::Wrapper,
    });
    let child = match instance {
        Some(GraphInstance::Wrap(inner)) => Some(inner.as_ref()),
        _ => None,
    };
    walk(inner, child, params, child_path(&path, segment), weight, out);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blendspace::{BlendSample1D, BlendSpace1D};
    use crate::eval::{ActiveTransition, StateMachineInstance};
    use crate::graph::{BlendSpace1DNode, ClipNode};
    use crate::params::ParamDecl;
    use crate::state_machine::{CmpOp, Condition, StateMachineDef, TransitionCurve, TransitionDef};

    fn graph() -> AnimGraph {
        AnimGraph {
            parameters: vec![ParamDecl("Speed".into(), ParamValue::Float(0.0))],
            root: AnimNode::StateMachine(StateMachineDef {
                initial: "Idle".into(),
                states: vec![
                    (
                        "Idle".into(),
                        AnimNode::Clip(ClipNode {
                            clip: "Idle".into(),
                            looping: true,
                            speed: 1.0,
                            sync_group: None,
                        }),
                    ),
                    (
                        "Move".into(),
                        AnimNode::BlendSpace1D(BlendSpace1DNode {
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
                    ),
                ],
                transitions: vec![TransitionDef {
                    from: Some("Idle".into()),
                    to: "Move".into(),
                    conditions: vec![Condition::Float {
                        param: "Speed".into(),
                        op: CmpOp::Gt,
                        value: 0.5,
                    }],
                    duration: 0.4,
                    curve: TransitionCurve::Linear,
                    interruptible: true,
                    blend: Default::default(),
                }],
            }),
        }
    }

    #[test]
    fn snapshot_reports_states_transitions_and_weights() {
        let graph = graph();
        let mut params = graph.default_parameters();
        params.set_float("Speed", 1.0);
        let mut instance = GraphInstance::from_node(&graph.root);
        // Mid-transition Idle → Move.
        if let GraphInstance::StateMachine(sm) = &mut instance {
            let sm: &mut StateMachineInstance = sm;
            sm.current = 1;
            sm.transition = Some(ActiveTransition {
                def: 0,
                from: 0,
                elapsed: 0.1,
                blend: Default::default(),
            });
        }
        let debug = graph_debug(&graph, &instance, &params);
        assert_eq!(debug.first_state(), Some("Move"));
        let root = debug.node("root").expect("root");
        match &root.state {
            NodeDebugState::StateMachine { transition, .. } => {
                let (from, to, progress) = transition.clone().expect("transition");
                assert_eq!((from.as_str(), to.as_str()), ("Idle", "Move"));
                assert!((progress - 0.25).abs() < 1e-5);
            }
            other => panic!("unexpected {other:?}"),
        }
        let idle = debug.node("root/state:Idle").expect("idle");
        let moving = debug.node("root/state:Move").expect("move");
        assert!(idle.active && moving.active, "both blend during the transition");
        assert!((idle.weight - 0.75).abs() < 1e-5 && (moving.weight - 0.25).abs() < 1e-5);
        match &moving.state {
            NodeDebugState::BlendSpace { weights, input } => {
                assert_eq!(input[0], 1.0);
                let total: f32 = weights.iter().map(|w| w.1).sum();
                assert!((total - 1.0).abs() < 1e-5);
                assert!(weights.iter().any(|w| w.0 == "Walk" && (w.1 - 0.75).abs() < 1e-5));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(debug.parameters.iter().any(|p| p.0 == "Speed"));
        // Settled: the inactive state reports weight 0.
        if let GraphInstance::StateMachine(sm) = &mut instance {
            sm.transition = None;
        }
        let settled = graph_debug(&graph, &instance, &params);
        assert!(!settled.node("root/state:Idle").expect("idle").active);
        let json = serde_json_like(&settled);
        assert!(json.contains("state_machine"), "{json}");
    }

    /// The snapshot serialises (the agent protocol sends it as JSON); RON stands in for JSON
    /// here to keep serde_json out of the runtime crate's dependencies.
    fn serde_json_like(debug: &GraphDebug) -> String {
        ron::to_string(debug).unwrap_or_default()
    }
}
